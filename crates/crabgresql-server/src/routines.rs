//! Wiring between the server's function catalog and the PL/pgSQL interpreter.
//!
//! The interpreter deliberately does not know how routines are stored, and the
//! executor deliberately does not know PL/pgSQL exists. This module is the one
//! place that knows both: it adapts [`GlobalCatalog`] to the interpreter's
//! [`RoutineSource`], and adapts the interpreter to the executor's
//! [`RoutineOps`].

use std::sync::{Arc, Mutex};

use crabgresql_executor::{
    ExecContext, ExecError, NoticeSink, RoutineOps, RuntimeNotice, Severity,
};
use crabgresql_plpgsql::{Interpreter, RoutineCache, RoutineDef, RoutineSource};
use crabgresql_storage_api::{TableEngine, TypeCatalog};
use crabgresql_txn::TxnContext;
use crabgresql_types::{PgType, Value};

use crate::global_catalog::{FuncInfo, GlobalCatalog, RoutineKind, TypeRef};
use crate::query::{Notice, NoticeSeverity};

/// Adapts [`GlobalCatalog`] to the interpreter's view of a routine.
pub struct CatalogRoutines(Arc<GlobalCatalog>);

impl RoutineSource for CatalogRoutines {
    fn routine(&self, oid: u32) -> Option<RoutineDef> {
        let info = self.0.functions().into_iter().find(|f| f.oid == oid)?;
        routine_def(&info)
    }
}

/// Translate a catalog entry into the interpreter's [`RoutineDef`], or `None`
/// if it is not a routine the interpreter can run — a `LANGUAGE internal` I/O
/// symbol, or one whose signature mentions a type that does not resolve.
fn routine_def(info: &FuncInfo) -> Option<RoutineDef> {
    Some(RoutineDef {
        name: info.name.clone(),
        arg_names: info
            .all_args
            .iter()
            .filter(|a| a.mode.is_input())
            .map(|a| a.name.clone())
            .collect(),
        arg_types: info
            .args
            .iter()
            .map(pg_type_of)
            .collect::<Option<Vec<_>>>()?,
        // A procedure declares no return type; the interpreter uses `None` to
        // mean "falling off the end is fine".
        ret: match info.kind {
            RoutineKind::Function => Some(pg_type_of(&info.ret)?),
            RoutineKind::Procedure => None,
        },
        strict: info.strict,
        src: info.src.clone(),
    })
}

/// A resolved [`TypeRef`] as a `PgType`. A `cstring` or unresolved user type
/// cannot appear in a callable routine's signature.
fn pg_type_of(r: &TypeRef) -> Option<PgType> {
    match r {
        TypeRef::Builtin(t) => Some(*t),
        // A user type resolves through the catalog's own OID assignment; the
        // binder has already refused any overload it could not resolve, so a
        // signature reaching here mentions only resolvable types.
        TypeRef::User(_) | TypeRef::Cstring => None,
    }
}

/// The executor's handle on the interpreter.
pub struct RoutineDispatch(Interpreter);

impl RoutineDispatch {
    /// Build the dispatcher for one statement.
    ///
    /// Per statement, not per session: the catalogs a body binds against are a
    /// session's temp-first overlay plus that statement's `pg_catalog`
    /// snapshot, and a body must resolve names exactly as its caller did. Only
    /// the compiled-body cache is shared, and it holds nothing but text.
    pub fn new(
        engine: Arc<dyn TableEngine>,
        type_catalog: Arc<dyn TypeCatalog>,
        catalog: Arc<GlobalCatalog>,
        cache: Arc<RoutineCache>,
    ) -> Self {
        Self(Interpreter::new(
            engine,
            type_catalog,
            Arc::new(CatalogRoutines(catalog)),
            cache,
        ))
    }
}

impl RoutineOps for RoutineDispatch {
    fn call(
        &self,
        oid: u32,
        args: Vec<Value>,
        ctx: &ExecContext,
        txn: &TxnContext,
    ) -> Result<Value, ExecError> {
        self.0.call(oid, args, ctx, txn)
    }

    fn run_inline_block(
        &self,
        body: &str,
        ctx: &ExecContext,
        txn: &TxnContext,
    ) -> Result<(), ExecError> {
        self.0.run_inline_block(body, ctx, txn)
    }
}

/// A session's buffer of diagnostics raised during execution.
///
/// Shared behind an `Arc` because the execution context inside a suspended
/// portal still has to reach it. The connection layer drains it as it writes
/// rows, so a `RAISE NOTICE` from row 3 lands between row 3 and row 4.
#[derive(Default)]
pub struct SessionNotices(Mutex<Vec<RuntimeNotice>>);

impl SessionNotices {
    /// Take everything buffered so far, leaving the buffer empty.
    pub fn drain(&self) -> Vec<Notice> {
        let taken = match self.0.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            // A poisoned buffer means a panic already unwound through a notice
            // emit; dropping the diagnostics is better than propagating it.
            Err(_) => Vec::new(),
        };
        taken.into_iter().map(into_notice).collect()
    }
}

impl NoticeSink for SessionNotices {
    fn emit(&self, notice: RuntimeNotice) {
        if let Ok(mut buf) = self.0.lock() {
            buf.push(notice);
        }
    }
}

fn into_notice(notice: RuntimeNotice) -> Notice {
    Notice {
        severity: match notice.severity {
            Severity::Warning => NoticeSeverity::Warning,
            // DEBUG and LOG never reach here — the interpreter routes them to
            // the server log — and INFO renders as a NOTICE on the wire.
            _ => NoticeSeverity::Notice,
        },
        code: notice.code.into(),
        message: notice.message,
        detail: notice.detail,
        hint: notice.hint,
        location: None,
        context: notice.context,
    }
}
