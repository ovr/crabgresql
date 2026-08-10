//! Running routines end to end against a real engine: control flow, variable
//! semantics, `RAISE`, embedded DML, and the errors PostgreSQL reports.

use std::collections::HashMap;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};

use crabgresql_executor::{ExecContext, ExecError, NoticeSink, RuntimeNotice, Severity};
use crabgresql_plpgsql::{Interpreter, RoutineCache, RoutineDef, RoutineSource};
use crabgresql_storage_api::{Column, TableEngine, TableSchema, TypeCatalog, UserCast, UserType};
use crabgresql_txn::{CommandId, TransactionManager, TxnContext};
use crabgresql_types::{PgType, Value};

/// A catalog holding routines registered by the test itself.
#[derive(Default)]
struct Routines(Mutex<HashMap<u32, RoutineDef>>);

impl RoutineSource for Routines {
    fn routine(&self, oid: u32) -> Option<RoutineDef> {
        self.0.lock().ok()?.get(&oid).cloned()
    }
}

/// Collects the notices a body raises, so their order and content can be
/// asserted.
#[derive(Default)]
struct Notices(Mutex<Vec<RuntimeNotice>>);

impl NoticeSink for Notices {
    fn emit(&self, notice: RuntimeNotice) {
        if let Ok(mut n) = self.0.lock() {
            n.push(notice);
        }
    }
}

/// An empty type catalog — these tests use built-in types only, so nothing
/// resolves as a user type and no user cast exists.
struct NoTypes;

impl TypeCatalog for NoTypes {
    fn resolve_type(&self, _name: &str) -> Option<UserType> {
        None
    }

    fn find_cast(&self, _source: PgType, _target: PgType) -> Option<UserCast> {
        None
    }

    fn backing_rep(&self, ty: PgType) -> PgType {
        ty
    }
}

struct Harness {
    interp: Interpreter,
    routines: Arc<Routines>,
    notices: Arc<Notices>,
    txnmgr: Arc<TransactionManager>,
    engine: Arc<dyn TableEngine>,
    command_counter: Arc<AtomicU32>,
    next_oid: Mutex<u32>,
}

impl Harness {
    fn new() -> Self {
        let engine: Arc<dyn TableEngine> = crabgresql_pg_engine::ephemeral_engine();
        let routines = Arc::new(Routines::default());
        let notices = Arc::new(Notices::default());
        let type_catalog: Arc<dyn TypeCatalog> = Arc::new(NoTypes);
        Self {
            interp: Interpreter::new(
                Arc::clone(&engine),
                type_catalog,
                Arc::clone(&routines) as Arc<dyn RoutineSource>,
                Arc::new(RoutineCache::new()),
            ),
            routines,
            notices,
            txnmgr: Arc::new(TransactionManager::new()),
            engine,
            command_counter: Arc::new(AtomicU32::new(0)),
            next_oid: Mutex::new(16384),
        }
    }

    /// Register a routine and return its OID.
    fn define(&self, name: &str, args: &[(&str, PgType)], ret: Option<PgType>, body: &str) -> u32 {
        let mut next = self.next_oid.lock().expect("oid lock");
        let oid = *next;
        *next += 1;
        drop(next);
        self.routines.0.lock().expect("routines lock").insert(
            oid,
            RoutineDef {
                name: name.to_string(),
                arg_names: args.iter().map(|(n, _)| Some((*n).to_string())).collect(),
                arg_types: args.iter().map(|(_, t)| *t).collect(),
                ret,
                strict: false,
                src: body.to_string(),
            },
        );
        oid
    }

    /// The context the server builds for a statement: a notice sink, and the
    /// transaction's command counter, so each statement inside a body gets its
    /// own command id and sees what the statement before it wrote.
    fn ctx(&self) -> ExecContext {
        ExecContext {
            notices: Some(Arc::clone(&self.notices) as Arc<dyn NoticeSink>),
            command_counter: Some(Arc::clone(&self.command_counter)),
            ..ExecContext::default()
        }
    }

    /// A writing transaction, committed up front so its own writes are visible
    /// to the reads that follow within the same test.
    fn txn(&self) -> TxnContext {
        let xid = self.txnmgr.allocate_xid();
        self.txnmgr.commit(xid).expect("commit");
        self.txnmgr.context(xid, CommandId::FIRST)
    }

    fn call(&self, oid: u32, args: Vec<Value>) -> Result<Value, ExecError> {
        self.interp.call(oid, args, &self.ctx(), &self.txn())
    }

    fn run_block(&self, body: &str) -> Result<(), ExecError> {
        self.interp.run_inline_block(body, &self.ctx(), &self.txn())
    }

    fn taken_notices(&self) -> Vec<RuntimeNotice> {
        std::mem::take(&mut *self.notices.0.lock().expect("notices lock"))
    }
}

fn ok(result: Result<Value, ExecError>) -> Value {
    match result {
        Ok(value) => value,
        Err(e) => panic!("expected success, got {}: {}", e.code, e.message),
    }
}

fn err(result: Result<Value, ExecError>) -> ExecError {
    match result {
        Ok(value) => panic!("expected an error, got {value:?}"),
        Err(e) => e,
    }
}

#[test]
fn arguments_are_reachable_by_name_and_the_result_is_coerced() {
    let h = Harness::new();
    let oid = h.define(
        "add",
        &[("a", PgType::Int4), ("b", PgType::Int4)],
        Some(PgType::Int8),
        "BEGIN RETURN a + b; END",
    );
    assert_eq!(
        ok(h.call(oid, vec![Value::Int4(2), Value::Int4(3)])),
        // Declared bigint, so the int4 sum is widened on the way out.
        Value::Int8(5)
    );
}

#[test]
fn declarations_initialize_and_assignments_coerce_to_the_declared_type() {
    let h = Harness::new();
    let oid = h.define(
        "f",
        &[],
        Some(PgType::Text),
        "DECLARE n int := 7; s text; BEGIN s := n; RETURN s || '!'; END",
    );
    assert_eq!(ok(h.call(oid, vec![])), Value::Text("7!".into()));
}

#[test]
fn a_constant_cannot_be_assigned_and_not_null_rejects_null() {
    let h = Harness::new();
    let oid = h.define(
        "f",
        &[],
        Some(PgType::Int4),
        "DECLARE c CONSTANT int := 1; BEGIN c := 2; RETURN c; END",
    );
    let e = err(h.call(oid, vec![]));
    assert_eq!(e.message, "variable \"c\" is declared CONSTANT");

    let oid = h.define(
        "g",
        &[],
        Some(PgType::Int4),
        "DECLARE n int NOT NULL := 1; BEGIN n := NULL; RETURN n; END",
    );
    let e = err(h.call(oid, vec![]));
    assert_eq!(e.code, "23502");
}

#[test]
fn if_elsif_else_picks_the_first_true_arm() {
    let h = Harness::new();
    let oid = h.define(
        "sign",
        &[("n", PgType::Int4)],
        Some(PgType::Text),
        "BEGIN \
           IF n > 0 THEN RETURN 'positive'; \
           ELSIF n < 0 THEN RETURN 'negative'; \
           ELSE RETURN 'zero'; \
           END IF; \
         END",
    );
    for (input, want) in [(5, "positive"), (-5, "negative"), (0, "zero")] {
        assert_eq!(
            ok(h.call(oid, vec![Value::Int4(input)])),
            Value::Text(want.into()),
            "sign({input})"
        );
    }
}

/// A NULL condition is false, as in SQL — not an error, and not true.
#[test]
fn a_null_condition_is_false() {
    let h = Harness::new();
    let oid = h.define(
        "f",
        &[],
        Some(PgType::Text),
        "BEGIN IF NULL THEN RETURN 'yes'; ELSE RETURN 'no'; END IF; END",
    );
    assert_eq!(ok(h.call(oid, vec![])), Value::Text("no".into()));
}

#[test]
fn for_range_counts_forward_reverse_and_by_step() {
    let h = Harness::new();
    for (header, want) in [
        ("1..4", 10),         // 1+2+3+4
        ("REVERSE 4..1", 10), // same set, other direction
        ("1..10 BY 3", 22),   // 1+4+7+10
    ] {
        let oid = h.define(
            "sum",
            &[],
            Some(PgType::Int4),
            &format!("DECLARE t int := 0; BEGIN FOR i IN {header} LOOP t := t + i; END LOOP; RETURN t; END"),
        );
        assert_eq!(
            ok(h.call(oid, vec![])),
            Value::Int4(want),
            "FOR i IN {header}"
        );
    }
}

/// A range whose bounds never meet runs zero times rather than forever.
#[test]
fn an_empty_for_range_runs_no_iterations() {
    let h = Harness::new();
    let oid = h.define(
        "f",
        &[],
        Some(PgType::Int4),
        "DECLARE t int := 0; BEGIN FOR i IN 5..1 LOOP t := t + 1; END LOOP; RETURN t; END",
    );
    assert_eq!(ok(h.call(oid, vec![])), Value::Int4(0));
}

#[test]
fn while_and_loop_with_exit_and_continue() {
    let h = Harness::new();
    let oid = h.define(
        "f",
        &[],
        Some(PgType::Int4),
        "DECLARE i int := 0; t int := 0; \
         BEGIN \
           WHILE i < 10 LOOP \
             i := i + 1; \
             CONTINUE WHEN i % 2 = 0; \
             EXIT WHEN i > 7; \
             t := t + i; \
           END LOOP; \
           RETURN t; \
         END",
    );
    // Odd i below 8: 1 + 3 + 5 + 7.
    assert_eq!(ok(h.call(oid, vec![])), Value::Int4(16));
}

/// A labeled `EXIT` leaves the named loop, not just the innermost one.
#[test]
fn a_labeled_exit_leaves_the_outer_loop() {
    let h = Harness::new();
    let oid = h.define(
        "f",
        &[],
        Some(PgType::Int4),
        "DECLARE t int := 0; \
         BEGIN \
           <<outer>> FOR i IN 1..3 LOOP \
             FOR j IN 1..3 LOOP \
               t := t + 1; \
               EXIT outer WHEN t = 4; \
             END LOOP; \
           END LOOP; \
           RETURN t; \
         END",
    );
    assert_eq!(ok(h.call(oid, vec![])), Value::Int4(4));
}

/// Falling off the end of a function without RETURN is an error, and the
/// CONTEXT traceback names the routine.
#[test]
fn falling_off_the_end_without_return_is_an_error() {
    let h = Harness::new();
    let oid = h.define(
        "f",
        &[("n", PgType::Int4)],
        Some(PgType::Int4),
        "BEGIN NULL; END",
    );
    let e = err(h.call(oid, vec![Value::Int4(1)]));
    assert_eq!(e.code, "2F005");
    assert_eq!(e.message, "control reached end of function without RETURN");
    // No line: the function ran out, it did not fail at a particular statement.
    assert_eq!(
        e.context(),
        Some("PL/pgSQL function f(integer)".to_string())
    );
}

#[test]
fn raise_notice_formats_its_arguments_and_reaches_the_sink() {
    let h = Harness::new();
    let oid = h.define(
        "f",
        &[("n", PgType::Int4)],
        Some(PgType::Int4),
        "BEGIN RAISE NOTICE 'n is %, 100%% sure, % again', n, NULL; RETURN n; END",
    );
    assert_eq!(ok(h.call(oid, vec![Value::Int4(7)])), Value::Int4(7));

    let notices = h.taken_notices();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].severity, Severity::Notice);
    // `%%` is a literal percent; a NULL argument renders as `<NULL>`.
    assert_eq!(notices[0].message, "n is 7, 100% sure, <NULL> again");
    // No CONTEXT: PostgreSQL prints one only for messages at ERROR and above
    // under the default `client_min_messages`.
    assert!(notices[0].context.is_empty());
}

#[test]
fn raise_reports_a_wrong_argument_count() {
    let h = Harness::new();
    for (body, want) in [
        ("BEGIN RAISE NOTICE 'a % b %', 1; END", "too few parameters"),
        ("BEGIN RAISE NOTICE 'a %', 1, 2; END", "too many parameters"),
    ] {
        let oid = h.define("f", &[], None, body);
        let e = err(h.call(oid, vec![]));
        // PostgreSQL reports a RAISE placeholder/argument mismatch as a syntax
        // error, not as an invalid parameter value — the upstream `plpgsql`
        // regression test expects "too few parameters specified for RAISE".
        assert_eq!(e.code, "42601");
        assert!(e.message.contains(want), "{body}: {}", e.message);
    }
}

#[test]
fn raise_exception_carries_its_sqlstate_detail_and_hint() {
    let h = Harness::new();
    let oid = h.define(
        "f",
        &[],
        None,
        "BEGIN RAISE EXCEPTION 'boom' USING ERRCODE = 'XX999', DETAIL = 'why', HINT = 'how'; END",
    );
    let e = err(h.call(oid, vec![]));
    assert_eq!(e.code, "XX999");
    assert_eq!(e.message, "boom");
    assert_eq!(e.detail.as_deref(), Some("why"));
    assert_eq!(e.hint.as_deref(), Some("how"));

    // With no ERRCODE, a bare RAISE EXCEPTION is P0001.
    let oid = h.define("g", &[], None, "BEGIN RAISE EXCEPTION 'plain'; END");
    assert_eq!(err(h.call(oid, vec![])).code, "P0001");
}

/// With nothing to say, PostgreSQL says the SQLSTATE: `RAISE EXCEPTION USING
/// DETAIL = 'x'` reports `ERROR: P0001`, and `RAISE SQLSTATE '22012' USING
/// DETAIL = 'x'` reports `ERROR: 22012` — the *resolved* code, not the level's
/// default.
#[test]
fn raise_without_a_message_reports_the_sqlstate() {
    let h = Harness::new();

    let oid = h.define(
        "f",
        &[],
        None,
        "BEGIN RAISE EXCEPTION USING DETAIL = 'more'; END",
    );
    let e = err(h.call(oid, vec![]));
    assert_eq!(e.code, "P0001");
    assert_eq!(e.message, "P0001");
    assert_eq!(e.detail.as_deref(), Some("more"));

    let oid = h.define(
        "g",
        &[],
        None,
        "BEGIN RAISE EXCEPTION USING ERRCODE = '22012', DETAIL = 'more'; END",
    );
    let e = err(h.call(oid, vec![]));
    assert_eq!(e.code, "22012");
    assert_eq!(e.message, "22012");
}

/// PostgreSQL prints exactly one `CONTEXT:` frame per routine invocation,
/// naming the statement that invocation was on — not one per enclosing
/// statement. A `RAISE` nested inside `IF` inside `FOR` is one line.
#[test]
fn context_reports_one_frame_per_invocation() {
    let h = Harness::new();
    let oid = h.define(
        "f",
        &[],
        Some(PgType::Int4),
        "BEGIN\n\
         FOR i IN 1..3 LOOP\n\
         IF i = 2 THEN\n\
         RAISE EXCEPTION 'boom';\n\
         END IF;\n\
         END LOOP;\n\
         RETURN 0;\n\
         END",
    );
    let e = err(h.call(oid, vec![]));
    assert_eq!(e.message, "boom");
    assert_eq!(
        e.context,
        vec!["PL/pgSQL function f() line 4 at RAISE".to_string()],
        "expected a single frame naming the innermost statement"
    );
}

/// A condition name supplies both SQLSTATE and message, wherever it appears.
#[test]
fn raise_accepts_a_condition_name() {
    let h = Harness::new();
    let oid = h.define("f", &[], None, "BEGIN RAISE division_by_zero; END");
    let e = err(h.call(oid, vec![]));
    assert_eq!(e.code, "22012");

    let oid = h.define(
        "g",
        &[],
        None,
        "BEGIN RAISE EXCEPTION 'nope' USING ERRCODE = 'unique_violation'; END",
    );
    assert_eq!(err(h.call(oid, vec![])).code, "23505");

    // A name that is not a condition is an error, not an invented SQLSTATE.
    let oid = h.define("h", &[], None, "BEGIN RAISE no_such_condition; END");
    let e = err(h.call(oid, vec![]));
    assert!(
        e.message
            .contains("unrecognized exception condition \"no_such_condition\""),
        "{}",
        e.message
    );
}

/// DEBUG and LOG go to the server log rather than the client, under
/// PostgreSQL's default client_min_messages.
#[test]
fn debug_and_log_do_not_reach_the_client() {
    let h = Harness::new();
    let oid = h.define(
        "f",
        &[],
        None,
        "BEGIN RAISE DEBUG 'd'; RAISE LOG 'l'; RAISE INFO 'i'; RAISE WARNING 'w'; END",
    );
    ok(h.call(oid, vec![]));
    let severities: Vec<Severity> = h.taken_notices().iter().map(|n| n.severity).collect();
    assert_eq!(severities, vec![Severity::Info, Severity::Warning]);
}

/// An error carries a `CONTEXT:` frame naming the routine, its signature and
/// the statement it came from. Frames from nested calls stack innermost-first,
/// which needs the binder wiring to exercise end to end; one frame is what this
/// layer can show on its own.
#[test]
fn an_error_carries_a_context_frame_for_the_statement_that_raised_it() {
    let h = Harness::new();
    let oid = h.define(
        "inner",
        &[("a", PgType::Int4), ("b", PgType::Text)],
        None,
        "\nBEGIN\n  NULL;\n  RAISE EXCEPTION 'deep';\nEND",
    );
    let e = err(h.call(oid, vec![Value::Int4(1), Value::Text("x".into())]));
    // No space after the comma in the signature, and the line is relative to
    // the body — both are how PostgreSQL renders it.
    assert_eq!(
        e.context(),
        Some("PL/pgSQL function inner(integer,text) line 4 at RAISE".to_string())
    );
}

#[test]
fn embedded_dml_runs_and_select_into_reads_it_back() {
    let h = Harness::new();
    h.engine
        .create_table(TableSchema::in_namespace(
            "t",
            "public",
            vec![Column::new("n", PgType::Int4)],
        ))
        .expect("create table");

    let oid = h.define(
        "f",
        // Named `v`, not `n`: a parameter sharing a column's name is resolved
        // in the variable's favour (see below), which would turn `sum(n)` into
        // `sum(v)`.
        &[("v", PgType::Int4)],
        Some(PgType::Int8),
        "DECLARE total bigint; \
         BEGIN \
           INSERT INTO t (n) VALUES (v); \
           INSERT INTO t (n) VALUES (v * 2); \
           SELECT sum(n) INTO total FROM t; \
           RETURN total; \
         END",
    );
    assert_eq!(ok(h.call(oid, vec![Value::Int4(5)])), Value::Int8(15));
}

/// When a variable and a column share a name, the variable wins.
///
/// PostgreSQL makes this configurable and defaults to raising an ambiguity
/// error; deciding that needs the binder's view of which columns are in scope,
/// which compilation deliberately does not have. Variable-wins is PostgreSQL's
/// historical behavior and what almost all real code assumes — but it is a
/// documented divergence, so it gets a test that pins it down rather than
/// leaving it to be discovered.
#[test]
fn a_variable_shadows_a_column_of_the_same_name() -> anyhow::Result<()> {
    let h = Harness::new();
    let table = h
        .engine
        .create_table(TableSchema::in_namespace(
            "t",
            "public",
            vec![Column::new("n", PgType::Int4)],
        ))
        .expect("create table");
    let txn = h.txn();
    table.insert(vec![Value::Int4(1)], &txn)?;
    table.insert(vec![Value::Int4(2)], &txn)?;

    let oid = h.define(
        "f",
        &[("n", PgType::Int4)],
        Some(PgType::Int8),
        "DECLARE total bigint; BEGIN SELECT sum(n) INTO total FROM t; RETURN total; END",
    );
    // `sum(n)` sums the *parameter*, once per row — not the column.
    assert_eq!(ok(h.call(oid, vec![Value::Int4(10)])), Value::Int8(20));
    Ok(())
}

/// A statement inside a body sees what the statement before it wrote — which
/// only works if each gets its own command id.
#[test]
fn a_body_statement_sees_the_previous_statements_writes() {
    let h = Harness::new();
    h.engine
        .create_table(TableSchema::in_namespace(
            "t",
            "public",
            vec![Column::new("n", PgType::Int4)],
        ))
        .expect("create table");

    let oid = h.define(
        "f",
        &[],
        Some(PgType::Int8),
        "DECLARE c bigint; \
         BEGIN INSERT INTO t (n) VALUES (1); SELECT count(*) INTO c FROM t; RETURN c; END",
    );
    // Within one call the INSERT and the SELECT share a transaction, so the
    // SELECT can only see the new row if it runs under a later command id.
    assert_eq!(ok(h.call(oid, vec![])), Value::Int8(1));
}

#[test]
fn found_reflects_whether_the_last_statement_matched() {
    let h = Harness::new();
    h.engine
        .create_table(TableSchema::in_namespace(
            "t",
            "public",
            vec![Column::new("n", PgType::Int4)],
        ))
        .expect("create table");

    let oid = h.define(
        "f",
        &[],
        Some(PgType::Bool),
        "DECLARE x int; BEGIN SELECT n INTO x FROM t WHERE n = 99; RETURN FOUND; END",
    );
    assert_eq!(ok(h.call(oid, vec![])), Value::Bool(false));
}

#[test]
fn select_into_strict_reports_no_rows_and_too_many_rows() -> anyhow::Result<()> {
    let h = Harness::new();
    let table = h
        .engine
        .create_table(TableSchema::in_namespace(
            "t",
            "public",
            vec![Column::new("n", PgType::Int4)],
        ))
        .expect("create table");
    let txn = h.txn();
    table.insert(vec![Value::Int4(1)], &txn)?;
    table.insert(vec![Value::Int4(2)], &txn)?;

    let oid = h.define(
        "none",
        &[],
        Some(PgType::Int4),
        "DECLARE x int; BEGIN SELECT n INTO STRICT x FROM t WHERE n = 99; RETURN x; END",
    );
    let e = err(h.call(oid, vec![]));
    assert_eq!(e.code, "P0002");
    assert_eq!(e.message, "query returned no rows");

    let oid = h.define(
        "many",
        &[],
        Some(PgType::Int4),
        "DECLARE x int; BEGIN SELECT n INTO STRICT x FROM t; RETURN x; END",
    );
    let e = err(h.call(oid, vec![]));
    assert_eq!(e.code, "P0003");
    assert_eq!(e.message, "query returned more than one row");
    Ok(())
}

/// A bare SELECT has nowhere to put its rows; PostgreSQL says so and points at
/// PERFORM rather than discarding them.
#[test]
fn a_bare_select_has_no_destination() {
    let h = Harness::new();
    let oid = h.define("f", &[], None, "BEGIN SELECT 1; END");
    let e = err(h.call(oid, vec![]));
    assert_eq!(e.code, "42601");
    assert_eq!(e.message, "query has no destination for result data");
    assert_eq!(
        e.hint.as_deref(),
        Some("If you want to discard the results of a SELECT, use PERFORM instead.")
    );
}

#[test]
fn perform_discards_rows_and_sets_found() {
    let h = Harness::new();
    let oid = h.define(
        "f",
        &[],
        Some(PgType::Bool),
        "BEGIN PERFORM 1; RETURN FOUND; END",
    );
    assert_eq!(ok(h.call(oid, vec![])), Value::Bool(true));
}

#[test]
fn a_scalar_expression_returning_many_rows_is_a_cardinality_violation() -> anyhow::Result<()> {
    let h = Harness::new();
    let table = h
        .engine
        .create_table(TableSchema::in_namespace(
            "t",
            "public",
            vec![Column::new("n", PgType::Int4)],
        ))
        .expect("create table");
    let txn = h.txn();
    table.insert(vec![Value::Int4(1)], &txn)?;
    table.insert(vec![Value::Int4(2)], &txn)?;

    let oid = h.define(
        "f",
        &[],
        Some(PgType::Int4),
        "DECLARE x int; BEGIN x := (SELECT n FROM t); RETURN x; END",
    );
    assert_eq!(err(h.call(oid, vec![])).code, "21000");
    Ok(())
}

#[test]
fn a_read_only_context_refuses_dml_inside_a_body() {
    let h = Harness::new();
    h.engine
        .create_table(TableSchema::in_namespace(
            "t",
            "public",
            vec![Column::new("n", PgType::Int4)],
        ))
        .expect("create table");

    let oid = h.define("f", &[], None, "BEGIN INSERT INTO t (n) VALUES (1); END");
    let ctx = ExecContext {
        read_only: true,
        ..h.ctx()
    };
    let e = err(h.interp.call(oid, vec![], &ctx, &h.txn()));
    assert_eq!(e.code, "25006");
    assert_eq!(
        e.message,
        "cannot execute INSERT in a read-only transaction"
    );
}

/// Recursion is bounded, and reports PostgreSQL's error rather than blowing the
/// Rust stack.
#[test]
fn call_depth_is_bounded() {
    let h = Harness::new();
    let oid = h.define("f", &[], None, "BEGIN NULL; END");
    let mut ctx = h.ctx();
    ctx.call_depth = 200;
    let e = err(h.interp.call(oid, vec![], &ctx, &h.txn()));
    assert_eq!(e.code, "54001");
    assert_eq!(e.message, "stack depth limit exceeded");
}

#[test]
fn an_inline_block_runs_and_names_itself_in_a_traceback() {
    let h = Harness::new();
    h.run_block("BEGIN RAISE NOTICE 'from a DO block'; END")
        .expect("run block");
    let notices = h.taken_notices();
    assert_eq!(notices[0].message, "from a DO block");
    assert!(notices[0].context.is_empty());
}

/// An anonymous block names itself `inline_code_block` where a routine would
/// print its signature.
#[test]
fn an_inline_block_names_itself_in_an_error_traceback() {
    let h = Harness::new();
    let e = h
        .run_block("BEGIN RAISE EXCEPTION 'from a DO block'; END")
        .expect_err("expected the block to raise");
    assert_eq!(
        e.context(),
        Some("PL/pgSQL function inline_code_block line 1 at RAISE".to_string())
    );
}

/// A body that fails to compile reports the failure when it is first called,
/// not silently.
#[test]
fn a_body_that_does_not_compile_fails_at_call_time() {
    let h = Harness::new();
    let oid = h.define("f", &[], None, "BEGIN this is not plpgsql END");
    let e = err(h.call(oid, vec![]));
    assert_eq!(e.code, "42601");
    assert!(
        e.context()
            .is_some_and(|c| c.contains("compilation of PL/pgSQL function")),
        "{:?}",
        e.context()
    );
}

/// A declared type is resolved per call, so a body may name a type that did not
/// exist when it was written — but an unknown one still fails clearly.
#[test]
fn an_unknown_declared_type_is_reported_at_call_time() {
    let h = Harness::new();
    let oid = h.define("f", &[], None, "DECLARE x nosuchtype; BEGIN NULL; END");
    let e = err(h.call(oid, vec![]));
    assert!(
        e.message.contains("nosuchtype"),
        "{}: {}",
        e.code,
        e.message
    );
}
