//! The PL/pgSQL interpreter.
//!
//! Executing a routine is a tree walk over its [`Block`], with one twist: every
//! expression and embedded statement is SQL, so evaluating one means re-entering
//! the whole pipeline — parse, bind, plan, execute — against the caller's own
//! transaction and catalog snapshot. That re-entrancy is why this crate sits
//! above the executor rather than inside it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crabgresql_binder::{DeletePlan, InsertPlan, LogicalPlan, UpdatePlan};
use crabgresql_executor::{ExecContext, ExecError, Execution, RuntimeNotice, Severity, execute};
use crabgresql_parser::ast;
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::{TableEngine, Tuple, TypeCatalog};
use crabgresql_txn::TxnContext;
use crabgresql_types::{PgType, Value};

use crate::ast::{
    Block, Decl, LoopDirection, Raise, RaiseLevel, Routine, SqlFragment, Stmt, VarId,
};
use crate::condition;
use crate::frame::{Flow, Frame};

/// How the interpreter finds a routine's definition. Implemented by whoever
/// owns the function catalog — this crate deliberately does not, so it can be
/// tested against a stub and stays independent of how routines are stored.
pub trait RoutineSource: Send + Sync {
    fn routine(&self, oid: u32) -> Option<RoutineDef>;
}

/// Everything the interpreter needs to call a routine, as the catalog holds it.
#[derive(Clone, Debug)]
pub struct RoutineDef {
    pub name: String,
    /// Declared parameter names, in order; `None` for an unnamed one, which is
    /// still reachable as `$n`.
    pub arg_names: Vec<Option<String>>,
    /// Declared input-argument types, for the `CONTEXT:` signature and for
    /// coercing arguments into the frame.
    pub arg_types: Vec<PgType>,
    /// The declared return type, or `None` for a procedure or `DO` block.
    pub ret: Option<PgType>,
    /// `STRICT`: a NULL argument short-circuits to NULL without entering the
    /// body. Checked by the caller, before arguments are even evaluated.
    pub strict: bool,
    /// The body text, as written.
    pub src: String,
}

impl RoutineDef {
    /// PostgreSQL's `f(integer,text)` — note no space after the comma, which is
    /// how it renders a `CONTEXT:` traceback line.
    fn signature(&self) -> String {
        let args: Vec<String> = self
            .arg_types
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        format!("{}({})", self.name, args.join(","))
    }
}

/// Compiled routine bodies, keyed by catalog OID.
///
/// No invalidation, because none is needed: OIDs are never reused, and a
/// routine's body and signature are fixed once created — there is no
/// `CREATE OR REPLACE FUNCTION` yet. Adding one means adding a generation
/// stamp here.
#[derive(Default)]
pub struct RoutineCache {
    entries: Mutex<HashMap<u32, Arc<Routine>>>,
}

impl RoutineCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&self, oid: u32) -> Option<Arc<Routine>> {
        self.entries
            .lock()
            .ok()
            .and_then(|e| e.get(&oid).map(Arc::clone))
    }

    fn put(&self, oid: u32, routine: Arc<Routine>) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(oid, routine);
        }
    }
}

/// How deep routine calls may nest.
///
/// PostgreSQL's `max_stack_depth` is a *byte* budget, checked by probing the
/// actual stack pointer; there is no equivalent here, so this is a frame count
/// chosen to stay well inside the smallest stack we can rely on. A level costs
/// an interpreter frame plus a whole bind/plan/execute/eval recursion, which
/// measured at roughly 40 KB in a debug build — a 2 MB thread stack overflows
/// somewhere between 40 and 50 levels. This is set at half that, because the
/// failure mode it guards against is a process abort, not an error.
///
/// That makes it far shallower than PostgreSQL's effective depth. Raising it
/// means either giving the runtime's worker threads a bigger stack or probing
/// the stack pointer the way PostgreSQL does.
const MAX_CALL_DEPTH: u32 = 24;

/// A PL/pgSQL interpreter bound to one statement's catalog snapshot.
///
/// Built per top-level statement, because the catalogs it binds against are: a
/// session's temp relations shadow the shared engine, and a body must resolve
/// names exactly as its caller did. Only the compiled-body cache is shared
/// across statements — it holds nothing but rewritten text.
pub struct Interpreter {
    engine: Arc<dyn TableEngine>,
    type_catalog: Arc<dyn TypeCatalog>,
    source: Arc<dyn RoutineSource>,
    cache: Arc<RoutineCache>,
}

impl Interpreter {
    pub fn new(
        engine: Arc<dyn TableEngine>,
        type_catalog: Arc<dyn TypeCatalog>,
        source: Arc<dyn RoutineSource>,
        cache: Arc<RoutineCache>,
    ) -> Self {
        Self {
            engine,
            type_catalog,
            source,
            cache,
        }
    }

    /// Call a routine and produce its return value. A procedure or `DO` block
    /// yields `Value::Null`, which its caller discards.
    pub fn call(
        &self,
        oid: u32,
        args: Vec<Value>,
        ctx: &ExecContext,
        txn: &TxnContext,
    ) -> Result<Value, ExecError> {
        let def = self.source.routine(oid).ok_or_else(|| {
            // The routine was dropped between binding and execution; there are
            // no catalog locks to prevent that.
            ExecError::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!("function with OID {oid} does not exist"),
            )
        })?;
        let routine = self.compiled(oid, &def)?;
        let inner = self.deeper(ctx)?;
        self.run(&def, &routine, args, &inner, txn)
    }

    /// Run a `DO $$ ... $$` block, whose body has no catalog entry to look up
    /// and is therefore compiled fresh each time.
    pub fn run_inline_block(
        &self,
        body: &str,
        ctx: &ExecContext,
        txn: &TxnContext,
    ) -> Result<(), ExecError> {
        let routine =
            crate::compile_inline_block(body).map_err(|e| compile_error(INLINE_BLOCK_NAME, e))?;
        let def = RoutineDef {
            // PostgreSQL names an anonymous block `inline_code_block` in
            // tracebacks, with no argument list.
            name: INLINE_BLOCK_NAME.to_string(),
            arg_names: Vec::new(),
            arg_types: Vec::new(),
            ret: None,
            strict: false,
            src: body.to_string(),
        };
        let inner = self.deeper(ctx)?;
        self.run(&def, &Arc::new(routine), Vec::new(), &inner, txn)?;
        Ok(())
    }

    /// The compiled body for `oid`, compiling and caching it on first call.
    fn compiled(&self, oid: u32, def: &RoutineDef) -> Result<Arc<Routine>, ExecError> {
        if let Some(routine) = self.cache.get(oid) {
            return Ok(routine);
        }
        let routine = Arc::new(
            crate::compile(&def.src, &def.arg_names).map_err(|e| compile_error(&def.name, e))?,
        );
        self.cache.put(oid, Arc::clone(&routine));
        Ok(routine)
    }

    /// A context one call deeper, refusing to go past [`MAX_CALL_DEPTH`].
    ///
    /// The depth lives on the context rather than in a thread-local because a
    /// suspended portal carries a cloned context across `Execute` round-trips,
    /// and tokio may resume it on a different worker thread.
    fn deeper(&self, ctx: &ExecContext) -> Result<ExecContext, ExecError> {
        let mut inner = ctx.clone();
        inner.call_depth = ctx.call_depth + 1;
        if inner.call_depth > MAX_CALL_DEPTH {
            return Err(ExecError::new(
                sqlstate::STATEMENT_TOO_COMPLEX,
                "stack depth limit exceeded",
            )
            .with_hint(Some(
                "Increase the configuration parameter max_stack_depth, after ensuring \
                         the platform's stack depth limit is adequate."
                    .into(),
            )));
        }
        Ok(inner)
    }

    fn run(
        &self,
        def: &RoutineDef,
        routine: &Routine,
        args: Vec<Value>,
        ctx: &ExecContext,
        txn: &TxnContext,
    ) -> Result<Value, ExecError> {
        let mut frame = Frame::new(routine.nvars);
        for (i, (value, ty)) in args.into_iter().zip(def.arg_types.iter()).enumerate() {
            frame.init_slot(VarId(i), value, Some(*ty));
        }
        frame.init_slot(routine.found, Value::Bool(false), Some(PgType::Bool));
        frame.track_found(routine.found);

        // One `CONTEXT:` frame per invocation, naming the statement this
        // invocation was executing. A nested call has already pushed its own
        // frames by the time the error arrives here, so they stack
        // innermost-first exactly as PostgreSQL renders them.
        let flow = match self.block(&routine.block, &mut frame, ctx, txn, def) {
            Ok(flow) => flow,
            Err(e) => {
                return Err(match frame.current_statement() {
                    Some((line, label)) => e.push_context(frame_line(def, line, label)),
                    None => e,
                });
            }
        };
        match flow {
            Flow::Return(value) => match def.ret {
                Some(ty) => crabgresql_executor::coerce_value_assign(value, ty, ctx),
                None => Ok(Value::Null),
            },
            // Falling off the end of a function without RETURN is an error;
            // for a procedure or DO block it is the normal way to finish.
            _ if def.ret.is_some() => Err(ExecError::new(
                "2F005",
                "control reached end of function without RETURN",
            )
            // No statement to name: the function ran out, it did not fail at
            // a particular line.
            .push_context(format!("PL/pgSQL function {}", routine_label(def)))),
            _ => Ok(Value::Null),
        }
    }

    // -----------------------------------------------------------------------
    // Statements
    // -----------------------------------------------------------------------

    fn block(
        &self,
        block: &Block,
        frame: &mut Frame,
        ctx: &ExecContext,
        txn: &TxnContext,
        def: &RoutineDef,
    ) -> Result<Flow, ExecError> {
        for decl in &block.decls {
            self.init_decl(decl, frame, ctx, txn, def)?;
        }
        for stmt in &block.stmts {
            match self.statement(stmt, frame, ctx, txn, def)? {
                Flow::Normal => {}
                // A labeled block can be left with `EXIT <label>`.
                Flow::Exit(Some(label)) if block.label.as_deref() == Some(label.as_str()) => {
                    return Ok(Flow::Normal);
                }
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    fn init_decl(
        &self,
        decl: &Decl,
        frame: &mut Frame,
        ctx: &ExecContext,
        txn: &TxnContext,
        def: &RoutineDef,
    ) -> Result<(), ExecError> {
        let ty = self.resolve_type(&decl.type_text)?;
        let value = match &decl.init {
            Some(init) => self.scalar(init, ty, frame, ctx, txn, def, "assignment")?,
            None => Value::Null,
        };
        if decl.not_null && matches!(value, Value::Null) {
            return Err(ExecError::new(
                "23502",
                format!(
                    "null value cannot be assigned to variable \"{}\" declared NOT NULL",
                    decl.name
                ),
            ));
        }
        frame.init_slot(decl.var, value, Some(ty));
        frame.set_flags(decl.var, decl.constant, decl.not_null, &decl.name);
        Ok(())
    }

    fn statement(
        &self,
        stmt: &Stmt,
        frame: &mut Frame,
        ctx: &ExecContext,
        txn: &TxnContext,
        def: &RoutineDef,
    ) -> Result<Flow, ExecError> {
        // Record the statement rather than pushing a `CONTEXT:` frame here.
        // PostgreSQL emits one frame per routine *invocation*, naming the
        // statement that invocation was on; pushing per dispatch would stack a
        // line for every enclosing IF/LOOP/block as the error unwound, where
        // PostgreSQL prints one. `run` pushes the single frame at the
        // invocation boundary, using whatever this leaves behind.
        frame.enter_statement(stmt.line(), stmt.context_label());
        self.statement_inner(stmt, frame, ctx, txn, def)
    }

    fn statement_inner(
        &self,
        stmt: &Stmt,
        frame: &mut Frame,
        ctx: &ExecContext,
        txn: &TxnContext,
        def: &RoutineDef,
    ) -> Result<Flow, ExecError> {
        match stmt {
            Stmt::Null { .. } => Ok(Flow::Normal),
            Stmt::Block(block) => self.block(block, frame, ctx, txn, def),

            Stmt::Assign { target, value, .. } => {
                let ty = frame.type_of(*target).unwrap_or(PgType::Text);
                let v = self.scalar(value, ty, frame, ctx, txn, def, "assignment")?;
                frame.assign(*target, v)?;
                Ok(Flow::Normal)
            }

            Stmt::If {
                arms, else_body, ..
            } => {
                for (cond, body) in arms {
                    if self.condition(cond, frame, ctx, txn, def)? {
                        return self.statements(body, frame, ctx, txn, def);
                    }
                }
                match else_body {
                    Some(body) => self.statements(body, frame, ctx, txn, def),
                    None => Ok(Flow::Normal),
                }
            }

            Stmt::Loop { label, body, .. } => loop {
                match self.statements(body, frame, ctx, txn, def)? {
                    Flow::Normal => {}
                    flow => {
                        if let Some(flow) = loop_flow(flow, label.as_deref()) {
                            break Ok(flow);
                        }
                    }
                }
            },

            Stmt::While {
                label, cond, body, ..
            } => {
                while self.condition(cond, frame, ctx, txn, def)? {
                    match self.statements(body, frame, ctx, txn, def)? {
                        Flow::Normal => {}
                        flow => {
                            if let Some(flow) = loop_flow(flow, label.as_deref()) {
                                return Ok(flow);
                            }
                        }
                    }
                }
                Ok(Flow::Normal)
            }

            Stmt::ForRange {
                label,
                var,
                direction,
                lower,
                upper,
                step,
                body,
                ..
            } => self.for_range(
                label.as_deref(),
                *var,
                *direction,
                lower,
                upper,
                step.as_ref(),
                body,
                frame,
                ctx,
                txn,
                def,
            ),

            Stmt::Exit { label, when, .. } => {
                if self.guard(when.as_ref(), frame, ctx, txn, def)? {
                    Ok(Flow::Exit(label.clone()))
                } else {
                    Ok(Flow::Normal)
                }
            }
            Stmt::Continue { label, when, .. } => {
                if self.guard(when.as_ref(), frame, ctx, txn, def)? {
                    Ok(Flow::Continue(label.clone()))
                } else {
                    Ok(Flow::Normal)
                }
            }

            Stmt::Return { value, .. } => match (value, def.ret) {
                (Some(expr), Some(ty)) => Ok(Flow::Return(
                    self.scalar(expr, ty, frame, ctx, txn, def, "RETURN")?,
                )),
                (Some(_), None) => Err(ExecError::new(
                    sqlstate::SYNTAX_ERROR,
                    "RETURN cannot have a parameter in a procedure",
                )),
                (None, _) => Ok(Flow::Return(Value::Null)),
            },

            Stmt::Raise(raise) => {
                self.raise(raise, frame, ctx, txn, def)?;
                Ok(Flow::Normal)
            }

            Stmt::Perform { query, .. } => {
                let rows =
                    self.run_query(&format!("SELECT {}", query.text), query, frame, ctx, txn)?;
                frame.set_found(!rows.is_empty());
                Ok(Flow::Normal)
            }

            Stmt::SelectInto {
                query,
                targets,
                strict,
                ..
            } => self.select_into(query, targets, *strict, frame, ctx, txn),

            Stmt::Sql { query, .. } => {
                let rows = self.run_statement(&query.text, query, frame, ctx, txn)?;
                match rows {
                    // A statement that produces rows has nowhere to put them.
                    // PostgreSQL refuses it and points at PERFORM rather than
                    // silently discarding a result the author meant to use.
                    Rows::Set(_) => Err(ExecError::new(
                        sqlstate::SYNTAX_ERROR,
                        "query has no destination for result data",
                    )
                    .with_hint(Some(
                        "If you want to discard the results of a SELECT, use PERFORM instead."
                            .into(),
                    ))),
                    Rows::Count(n) => {
                        frame.set_found(n > 0);
                        Ok(Flow::Normal)
                    }
                }
            }
        }
    }

    fn statements(
        &self,
        stmts: &[Stmt],
        frame: &mut Frame,
        ctx: &ExecContext,
        txn: &TxnContext,
        def: &RoutineDef,
    ) -> Result<Flow, ExecError> {
        for stmt in stmts {
            match self.statement(stmt, frame, ctx, txn, def)? {
                Flow::Normal => {}
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    #[allow(clippy::too_many_arguments)]
    fn for_range(
        &self,
        label: Option<&str>,
        var: VarId,
        direction: LoopDirection,
        lower: &SqlFragment,
        upper: &SqlFragment,
        step: Option<&SqlFragment>,
        body: &[Stmt],
        frame: &mut Frame,
        ctx: &ExecContext,
        txn: &TxnContext,
        def: &RoutineDef,
    ) -> Result<Flow, ExecError> {
        let bound =
            |frag: &SqlFragment, which: &str, frame: &mut Frame| -> Result<i64, ExecError> {
                let value = self.scalar(frag, PgType::Int8, frame, ctx, txn, def, "FOR")?;
                match value {
                    Value::Int8(n) => Ok(n),
                    Value::Null => Err(ExecError::new(
                        sqlstate::NULL_VALUE_NOT_ALLOWED,
                        format!("{which} bound of FOR loop cannot be null"),
                    )),
                    other => Err(ExecError::new(
                        sqlstate::DATATYPE_MISMATCH,
                        format!(
                            "{which} bound of FOR loop must be integer, not {}",
                            other.pg_type().map_or("unknown", |t| t.name())
                        ),
                    )),
                }
            };
        let lo = bound(lower, "lower", frame)?;
        let hi = bound(upper, "upper", frame)?;
        let step = match step {
            Some(step) => {
                let n = bound(step, "BY", frame)?;
                if n <= 0 {
                    return Err(ExecError::new(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "BY value of FOR loop must be greater than zero",
                    ));
                }
                n
            }
            None => 1,
        };

        frame.init_slot(var, Value::Null, Some(PgType::Int4));
        let mut current = match direction {
            LoopDirection::Forward => lo,
            LoopDirection::Reverse => lo,
        };
        loop {
            let done = match direction {
                LoopDirection::Forward => current > hi,
                LoopDirection::Reverse => current < hi,
            };
            if done {
                return Ok(Flow::Normal);
            }
            // The loop variable is `integer`; a range wider than int4 is the
            // caller's problem, reported as an ordinary out-of-range cast.
            let value = crabgresql_executor::coerce_value(Value::Int8(current), PgType::Int4, ctx)?;
            frame.init_slot(var, value, Some(PgType::Int4));

            match self.statements(body, frame, ctx, txn, def)? {
                Flow::Normal => {}
                flow => {
                    if let Some(flow) = loop_flow(flow, label) {
                        return Ok(flow);
                    }
                }
            }

            current = match direction {
                LoopDirection::Forward => current.saturating_add(step),
                LoopDirection::Reverse => current.saturating_sub(step),
            };
        }
    }

    fn select_into(
        &self,
        query: &SqlFragment,
        targets: &[VarId],
        strict: bool,
        frame: &mut Frame,
        ctx: &ExecContext,
        txn: &TxnContext,
    ) -> Result<Flow, ExecError> {
        let rows = match self.run_statement(&query.text, query, frame, ctx, txn)? {
            Rows::Set(rows) => rows,
            Rows::Count(_) => Vec::new(),
        };
        if strict {
            if rows.is_empty() {
                return Err(ExecError::new("P0002", "query returned no rows"));
            }
            if rows.len() > 1 {
                return Err(
                    ExecError::new("P0003", "query returned more than one row").with_hint(Some(
                        "Make sure the query returns a single row, or use LIMIT 1.".into(),
                    )),
                );
            }
        }
        match rows.first() {
            Some(row) => {
                if row.len() != targets.len() {
                    return Err(ExecError::new(
                        sqlstate::SYNTAX_ERROR,
                        format!(
                            "number of source and target fields in assignment do not match; \
                             {} source, {} target",
                            row.len(),
                            targets.len()
                        ),
                    ));
                }
                for (target, value) in targets.iter().zip(row.iter()) {
                    let ty = frame.type_of(*target).unwrap_or(PgType::Text);
                    let value = crabgresql_executor::coerce_value_assign(value.clone(), ty, ctx)?;
                    frame.assign(*target, value)?;
                }
                frame.set_found(true);
            }
            // No row: PostgreSQL nulls every target and leaves FOUND false.
            None => {
                for target in targets {
                    frame.assign(*target, Value::Null)?;
                }
                frame.set_found(false);
            }
        }
        Ok(Flow::Normal)
    }

    // -----------------------------------------------------------------------
    // RAISE
    // -----------------------------------------------------------------------

    fn raise(
        &self,
        raise: &Raise,
        frame: &mut Frame,
        ctx: &ExecContext,
        txn: &TxnContext,
        def: &RoutineDef,
    ) -> Result<(), ExecError> {
        let text = |frag: &SqlFragment, frame: &mut Frame| -> Result<Option<String>, ExecError> {
            let value = self.scalar(frag, PgType::Text, frame, ctx, txn, def, "RAISE")?;
            Ok(match value {
                Value::Null => None,
                other => Some(other.encode_text_with(&ctx.fmt).unwrap_or_default()),
            })
        };

        // Precedence follows PostgreSQL: a condition name supplies both code and
        // message, an explicit ERRCODE overrides the code, and USING MESSAGE
        // overrides the message.
        let mut code = raise.level.default_sqlstate().to_string();
        let mut message = None;
        if let Some(name) = &raise.condition {
            let (sqlstate, default_message) = condition::lookup(name).ok_or_else(|| {
                ExecError::new(
                    sqlstate::SYNTAX_ERROR,
                    format!("unrecognized exception condition \"{name}\""),
                )
            })?;
            code = sqlstate.to_string();
            message = Some(default_message.to_string());
        }
        if let Some(format) = &raise.format {
            let mut args = Vec::with_capacity(raise.args.len());
            for arg in &raise.args {
                args.push(text(arg, frame)?);
            }
            message = Some(format_message(format, &args)?);
        }

        if let Some(frag) = &raise.using.errcode {
            let value = text(frag, frame)?.unwrap_or_default();
            code = normalize_errcode(&value)?;
        }
        if let Some(frag) = &raise.using.message {
            message = text(frag, frame)?;
        }
        let detail = match &raise.using.detail {
            Some(frag) => text(frag, frame)?,
            None => None,
        };
        let hint = match &raise.using.hint {
            Some(frag) => text(frag, frame)?,
            None => None,
        };
        // `RAISE ... USING DETAIL = ...` with no format string, condition name or
        // `USING MESSAGE`: PostgreSQL uses the SQLSTATE itself as the message
        // text (`ERROR: P0001`, `NOTICE: 00000`). It is the *resolved* code, so
        // `RAISE SQLSTATE '22012' USING DETAIL = ...` reports `22012`, not the
        // level's default.
        let message = message.unwrap_or_else(|| code.to_string());

        if raise.level == RaiseLevel::Exception {
            return Err(ExecError::new(code, message)
                .with_detail(detail)
                .with_hint(hint));
        }

        let severity = match raise.level {
            RaiseLevel::Debug => Severity::Debug,
            RaiseLevel::Log => Severity::Log,
            RaiseLevel::Info => Severity::Info,
            RaiseLevel::Notice => Severity::Notice,
            RaiseLevel::Warning => Severity::Warning,
            RaiseLevel::Exception => Severity::Warning,
        };
        // DEBUG and LOG go to the server log, not the client, under
        // PostgreSQL's default client_min_messages.
        if matches!(severity, Severity::Debug | Severity::Log) {
            return Ok(());
        }
        if let Some(sink) = &ctx.notices {
            sink.emit(RuntimeNotice {
                severity,
                code,
                message,
                detail,
                hint,
                // PostgreSQL prints no CONTEXT for a message below ERROR
                // under the default `client_min_messages`.
                context: Vec::new(),
            });
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Running SQL
    // -----------------------------------------------------------------------

    /// Evaluate a fragment as a single scalar and coerce it to `ty`.
    #[allow(clippy::too_many_arguments)]
    fn scalar(
        &self,
        frag: &SqlFragment,
        ty: PgType,
        frame: &mut Frame,
        ctx: &ExecContext,
        txn: &TxnContext,
        _def: &RoutineDef,
        _what: &str,
    ) -> Result<Value, ExecError> {
        let rows = self.run_query(&format!("SELECT {}", frag.text), frag, frame, ctx, txn)?;
        if rows.len() > 1 {
            return Err(ExecError::new(
                sqlstate::CARDINALITY_VIOLATION,
                "query returned more than one row",
            ));
        }
        let value = rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or(Value::Null);
        crabgresql_executor::coerce_value_assign(value, ty, ctx)
    }

    /// Evaluate a fragment as a boolean condition. A NULL condition is false,
    /// as in SQL.
    fn condition(
        &self,
        frag: &SqlFragment,
        frame: &mut Frame,
        ctx: &ExecContext,
        txn: &TxnContext,
        def: &RoutineDef,
    ) -> Result<bool, ExecError> {
        Ok(matches!(
            self.scalar(frag, PgType::Bool, frame, ctx, txn, def, "IF")?,
            Value::Bool(true)
        ))
    }

    /// An `EXIT ... WHEN cond` guard; absent means unconditional.
    fn guard(
        &self,
        when: Option<&SqlFragment>,
        frame: &mut Frame,
        ctx: &ExecContext,
        txn: &TxnContext,
        def: &RoutineDef,
    ) -> Result<bool, ExecError> {
        match when {
            Some(cond) => self.condition(cond, frame, ctx, txn, def),
            None => Ok(true),
        }
    }

    /// Bind, plan and run `sql`, which must produce a result set.
    fn run_query(
        &self,
        sql: &str,
        frag: &SqlFragment,
        frame: &mut Frame,
        ctx: &ExecContext,
        txn: &TxnContext,
    ) -> Result<Vec<Tuple>, ExecError> {
        match self.run_statement(sql, frag, frame, ctx, txn)? {
            Rows::Set(rows) => Ok(rows),
            Rows::Count(_) => Ok(Vec::new()),
        }
    }

    /// Bind, plan and run one statement of a routine body against the caller's
    /// transaction, with the frame's values substituted for its placeholders.
    fn run_statement(
        &self,
        sql: &str,
        frag: &SqlFragment,
        frame: &mut Frame,
        ctx: &ExecContext,
        txn: &TxnContext,
    ) -> Result<Rows, ExecError> {
        let mut statements = crabgresql_parser::parse(sql)
            .map_err(|e| ExecError::new(e.sqlstate, e.message).with_hint(e.hint))?;
        if statements.len() != 1 {
            return Err(ExecError::new(
                sqlstate::SYNTAX_ERROR,
                "a PL/pgSQL statement must contain exactly one SQL statement",
            ));
        }
        let statement = statements.remove(0);

        let param_types: Vec<Option<PgType>> =
            frag.params.iter().map(|v| frame.type_of(*v)).collect();
        let params = crabgresql_binder::param_ctx_capped(param_types);
        let mut logical = self.bind(&statement, &params)?;

        let values: Vec<Value> = frag
            .params
            .iter()
            .map(|v| frame.get(*v))
            .collect::<Result<_, _>>()?;
        crabgresql_binder::substitute_params(&mut logical, &values);

        let is_write = matches!(
            logical,
            LogicalPlan::Insert(InsertPlan { .. })
                | LogicalPlan::Update(UpdatePlan { .. })
                | LogicalPlan::Delete(DeletePlan { .. })
        );
        if is_write && ctx.read_only {
            let verb = match logical {
                LogicalPlan::Insert(InsertPlan { .. }) => "INSERT",
                LogicalPlan::Update(UpdatePlan { .. }) => "UPDATE",
                _ => "DELETE",
            };
            return Err(ExecError::new(
                sqlstate::READ_ONLY_SQL_TRANSACTION,
                format!("cannot execute {verb} in a read-only transaction"),
            ));
        }

        // Each statement in a body gets a fresh command id, so statement k+1
        // sees the rows statement k wrote. The counter is shared with the
        // session, which reads it back when the top-level statement finishes.
        let txn = match &ctx.command_counter {
            Some(counter) => txn.with_cid(crabgresql_txn::CommandId(
                counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1,
            )),
            None => txn.clone(),
        };

        match execute(crabgresql_planner::plan(logical), ctx, &txn)? {
            Execution::Rows { node, .. } | Execution::ReturningRows { node, .. } => {
                Ok(Rows::Set(drain(node)?))
            }
            Execution::Inserted(n) | Execution::Updated(n) | Execution::Deleted(n) => {
                Ok(Rows::Count(n))
            }
        }
    }

    fn bind(
        &self,
        statement: &ast::Statement,
        params: &crabgresql_binder::ParamCtx,
    ) -> Result<LogicalPlan, ExecError> {
        let plan = match statement {
            ast::Statement::Query(query) => crabgresql_binder::bind_query_with_params(
                &self.engine,
                &self.type_catalog,
                query,
                params,
            ),
            ast::Statement::Insert(insert) => crabgresql_binder::bind_insert_with_params(
                &self.engine,
                &self.type_catalog,
                insert,
                params,
            ),
            ast::Statement::Update(update) => crabgresql_binder::bind_update_with_params(
                &self.engine,
                &self.type_catalog,
                update,
                params,
            ),
            ast::Statement::Delete(delete) => crabgresql_binder::bind_delete_with_params(
                &self.engine,
                &self.type_catalog,
                delete,
                params,
            ),
            other => {
                return Err(ExecError::new(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    format!(
                        "{} inside a PL/pgSQL routine is not supported yet",
                        first_word(&other.to_string())
                    ),
                ));
            }
        };
        plan.map_err(|e| {
            ExecError::new(e.code, e.message)
                .with_detail(e.detail)
                .with_hint(e.hint)
        })
    }

    /// Resolve a declared type name against the catalog. Done per call rather
    /// than at compile time, so a body may name a type created after it.
    fn resolve_type(&self, type_text: &str) -> Result<PgType, ExecError> {
        let data_type = crabgresql_parser::parse_data_type(type_text).map_err(|e| {
            ExecError::new(
                sqlstate::UNDEFINED_OBJECT,
                format!("type \"{type_text}\" does not exist: {e}"),
            )
        })?;
        crabgresql_binder::resolve_data_type(&self.type_catalog, &data_type)
            .map_err(|e| ExecError::new(e.code, e.message).with_detail(e.detail))
    }
}

/// What running one statement produced.
enum Rows {
    Set(Vec<Tuple>),
    Count(u64),
}

fn drain(mut node: Box<dyn crabgresql_executor::ExecNode>) -> Result<Vec<Tuple>, ExecError> {
    let mut rows = Vec::new();
    while let Some(row) = node.next()? {
        rows.push(row);
    }
    Ok(rows)
}

/// How a flow signal interacts with the loop it reaches.
///
/// Returns `Some(flow)` if the loop should stop and propagate `flow`, or `None`
/// if the loop should carry on to its next iteration.
fn loop_flow(flow: Flow, label: Option<&str>) -> Option<Flow> {
    match flow {
        Flow::Normal => None,
        Flow::Return(v) => Some(Flow::Return(v)),
        Flow::Exit(None) => Some(Flow::Normal),
        Flow::Exit(Some(l)) if Some(l.as_str()) == label => Some(Flow::Normal),
        Flow::Exit(other) => Some(Flow::Exit(other)),
        Flow::Continue(None) => None,
        Flow::Continue(Some(l)) if Some(l.as_str()) == label => None,
        Flow::Continue(other) => Some(Flow::Continue(other)),
    }
}

/// One line of a `CONTEXT:` traceback, e.g.
/// `PL/pgSQL function f(integer) line 3 at RAISE`. An anonymous block renders
/// as `inline_code_block` with no argument list, as PostgreSQL does.
fn frame_line(def: &RoutineDef, line: u32, label: &str) -> String {
    format!(
        "PL/pgSQL function {} line {line} at {label}",
        routine_label(def)
    )
}

/// How a routine names itself in a traceback: its signature, or the literal
/// `inline_code_block` (with no argument list) for an anonymous `DO`.
fn routine_label(def: &RoutineDef) -> String {
    if def.name == INLINE_BLOCK_NAME {
        INLINE_BLOCK_NAME.to_string()
    } else {
        def.signature()
    }
}

/// How PostgreSQL names a `DO $$ ... $$` block in a traceback.
const INLINE_BLOCK_NAME: &str = "inline_code_block";

/// Expand a `RAISE` format string: `%` takes the next argument, `%%` is a
/// literal percent, and a NULL argument renders as `<NULL>`.
fn format_message(format: &str, args: &[Option<String>]) -> Result<String, ExecError> {
    let mut out = String::with_capacity(format.len());
    let mut next = 0usize;
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('%') => {
                chars.next();
                out.push('%');
            }
            _ => {
                let Some(arg) = args.get(next) else {
                    return Err(ExecError::new(
                        sqlstate::SYNTAX_ERROR,
                        "too few parameters specified for RAISE",
                    ));
                };
                next += 1;
                out.push_str(arg.as_deref().unwrap_or("<NULL>"));
            }
        }
    }
    if next < args.len() {
        return Err(ExecError::new(
            sqlstate::SYNTAX_ERROR,
            "too many parameters specified for RAISE",
        ));
    }
    Ok(out)
}

/// `USING ERRCODE` takes either a 5-character SQLSTATE or a condition name.
fn normalize_errcode(value: &str) -> Result<String, ExecError> {
    if value.len() == 5 && value.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok(value.to_string());
    }
    match condition::lookup(value) {
        Some((code, _)) => Ok(code.to_string()),
        None => Err(ExecError::new(
            sqlstate::SYNTAX_ERROR,
            format!("unrecognized exception condition \"{value}\""),
        )),
    }
}

fn first_word(s: &str) -> &str {
    s.split_whitespace().next().unwrap_or("statement")
}

/// A compile-time failure surfaced at run time, as PostgreSQL does for a body
/// it only validates when the routine is first called.
///
/// PostgreSQL names the routine in quotes here, and — unlike a runtime
/// traceback frame — bare, with no argument list.
fn compile_error(name: &str, e: crate::CompileError) -> ExecError {
    ExecError::new(e.code, e.message).push_context(format!(
        "compilation of PL/pgSQL function \"{name}\" near line {}",
        e.line
    ))
}
