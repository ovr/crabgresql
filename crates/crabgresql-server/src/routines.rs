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
/// symbol, or one whose signature mentions a user type (see [`pg_type_of`]).
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

/// A resolved [`TypeRef`] as a `PgType`. `cstring` is refused because no
/// callable routine's signature can mention it — only the I/O symbols that
/// bootstrap a type do.
fn pg_type_of(r: &TypeRef) -> Option<PgType> {
    match r {
        TypeRef::Builtin(t) => Some(*t),
        // TODO: resolve a user type to `PgType::User(oid)` so a PL/pgSQL
        // routine declared over one can run. `GlobalCatalog::routines` already
        // resolves it, so the binder offers the overload and a call like
        // `f('happy'::mood)` binds, then fails out of here with "function with
        // OID N does not exist".
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
/// rows, so a `RAISE NOTICE` from row 3 lands between row 3 and row 4 — and
/// drains it *before* an ErrorResponse too, which is what puts a statement's
/// notices ahead of the error that ended it, as PostgreSQL does.
///
/// That last property is why a DDL path with notices to raise should push them
/// here rather than returning them: a `Result`'s `Ok` half cannot carry
/// diagnostics past a failure, and `PgError` has nowhere to put them.
///
/// Buffers already-converted [`Notice`]s. Converting on the way *in* rather than
/// on the way out is what lets [`Self::push`] accept one directly: a `Notice`
/// routed through [`RuntimeNotice`] would lose its `location`, and some
/// producers (`CatalogNotice`) do carry a caret.
#[derive(Default)]
pub struct SessionNotices(Mutex<Vec<Notice>>);

impl SessionNotices {
    /// Take everything buffered so far, leaving the buffer empty.
    pub fn drain(&self) -> Vec<Notice> {
        match self.0.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            // A poisoned buffer means a panic already unwound through a notice
            // emit; dropping the diagnostics is better than propagating it.
            Err(_) => Vec::new(),
        }
    }

    /// Buffer a notice raised outside the executor — a DDL diagnostic that must
    /// still reach the client if the statement goes on to fail.
    pub fn push(&self, notice: Notice) {
        if let Ok(mut buf) = self.0.lock() {
            buf.push(notice);
        }
    }
}

impl NoticeSink for SessionNotices {
    fn emit(&self, notice: RuntimeNotice) {
        self.push(into_notice(notice));
    }
}

fn into_notice(notice: RuntimeNotice) -> Notice {
    Notice {
        severity: match notice.severity {
            Severity::Warning => NoticeSeverity::Warning,
            // DEBUG and LOG never reach here: they rank below the default
            // client_min_messages, so the interpreter drops them.
            // TODO: carry INFO through as its own severity — PostgreSQL sends
            // `INFO:  ...` for `RAISE INFO`, this collapses it to `NOTICE:`.
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
