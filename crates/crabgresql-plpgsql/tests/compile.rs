//! Compiling routine bodies: what the parser accepts, what it rejects, and —
//! most importantly — that variable references come out of the body as `$n`
//! placeholders without disturbing the surrounding SQL text.

use crabgresql_plpgsql::ast::{LoopDirection, RaiseLevel, SqlFragment, Stmt, VarId};
use crabgresql_plpgsql::{CompileError, Routine, compile};

/// Frame slots run: arguments, then `FOUND` (which PostgreSQL makes a real
/// variable of the routine's outermost scope), then declarations in order.
fn args(names: &[&str]) -> Vec<Option<String>> {
    names.iter().map(|n| Some((*n).to_string())).collect()
}

fn ok(body: &str, arg_names: &[&str]) -> Routine {
    match compile(body, &args(arg_names)) {
        Ok(routine) => routine,
        Err(e) => panic!("expected {body:?} to compile, got: {e} (line {})", e.line),
    }
}

fn err(body: &str, arg_names: &[&str]) -> CompileError {
    match compile(body, &args(arg_names)) {
        Ok(_) => panic!("expected {body:?} to fail to compile"),
        Err(e) => e,
    }
}

/// The whole point of the compiler: SQL comes out as written, with variables —
/// and only variables — swapped for placeholders.
#[test]
fn variables_become_placeholders_and_the_rest_is_verbatim() {
    let routine = ok("DECLARE x int; BEGIN x := a + b * 2; END", &["a", "b"]);
    let [Stmt::Assign { target, value, .. }] = &routine.block.stmts[..] else {
        panic!("expected one assignment");
    };
    assert_eq!(*target, VarId(3), "x follows two arguments and FOUND");
    assert_eq!(value.text, "$1 + $2 * 2");
    assert_eq!(value.params, vec![VarId(0), VarId(1)]);
}

/// A variable used twice reuses one placeholder, so the frame is read once and
/// a volatile expression behind it cannot run twice.
#[test]
fn a_repeated_variable_reuses_one_placeholder() {
    let routine = ok("DECLARE r int; BEGIN r := a * a; END", &["a"]);
    let [Stmt::Assign { value, .. }] = &routine.block.stmts[..] else {
        panic!("expected one assignment");
    };
    assert_eq!(value.text, "$1 * $1");
    assert_eq!(value.params, vec![VarId(0)]);
}

/// Text inside string literals, quoted identifiers and qualified names is not a
/// variable reference, however much it looks like one.
#[test]
fn placeholders_do_not_leak_into_literals_or_qualified_names() {
    let routine = ok(
        "DECLARE r text; BEGIN r := 'a is ' || a || t.a || \"A\"; END",
        &["a"],
    );
    let [Stmt::Assign { value, .. }] = &routine.block.stmts[..] else {
        panic!("expected one assignment");
    };
    // The literal keeps its text, `t.a` stays a qualified column, and `"A"` is
    // a different identifier from the parameter `a` because it was quoted.
    assert_eq!(value.text, "'a is ' || $1 || t.a || \"A\"");
    assert_eq!(value.params, vec![VarId(0)]);

    // ...but `"a"` and `a` are the same name, quoting or not.
    let routine = ok("DECLARE r int; BEGIN r := \"a\"; END", &["a"]);
    let [Stmt::Assign { value, .. }] = &routine.block.stmts[..] else {
        panic!("expected one assignment");
    };
    assert_eq!(value.text, "$1");
}

/// Arguments are reachable positionally as well as by name, and an out-of-range
/// `$n` is left alone for the binder to report.
#[test]
fn dollar_n_refers_to_arguments() {
    let routine = ok("DECLARE r int; BEGIN r := $1 + $2; END", &["a", "b"]);
    let [Stmt::Assign { value, .. }] = &routine.block.stmts[..] else {
        panic!("expected one assignment");
    };
    assert_eq!(value.text, "$1 + $2");
    assert_eq!(value.params, vec![VarId(0), VarId(1)]);

    // `a` and `$1` are the same slot, so they share one placeholder.
    let routine = ok("DECLARE r int; BEGIN r := a + $1; END", &["a"]);
    let [Stmt::Assign { value, .. }] = &routine.block.stmts[..] else {
        panic!("expected one assignment");
    };
    assert_eq!(value.text, "$1 + $1");
    assert_eq!(value.params, vec![VarId(0)]);
}

#[test]
fn declarations_carry_their_type_text_and_modifiers() {
    let routine = ok(
        "DECLARE \
           a numeric(10, 2) := 1.5; \
           b CONSTANT text DEFAULT 'x'; \
           c int NOT NULL := 0; \
           d myenum; \
         BEGIN NULL; END",
        &[],
    );
    let decls = &routine.block.decls;
    assert_eq!(decls.len(), 4);

    assert_eq!(decls[0].type_text, "numeric(10, 2)");
    assert_eq!(decls[0].init.as_ref().map(|f| f.text.as_str()), Some("1.5"));
    assert!(decls[1].constant);
    assert_eq!(decls[1].type_text, "text");
    assert!(decls[2].not_null);
    // An unresolvable type name compiles fine — types are resolved per call,
    // so a body can name a type that does not exist yet.
    assert_eq!(decls[3].type_text, "myenum");
    assert!(decls[3].init.is_none());

    assert_eq!(routine.nvars, 5, "four declarations plus FOUND");
}

/// NOT NULL without an initializer is a definition-time error in PostgreSQL,
/// because there is no value that could satisfy it.
#[test]
fn not_null_requires_a_default() {
    let e = err("DECLARE x int NOT NULL; BEGIN NULL; END", &[]);
    assert!(
        e.message.contains("must have a default value"),
        "{}",
        e.message
    );
}

/// An initializer sees the *outer* scope, so `x int := x` is not self-reference.
#[test]
fn an_initializer_resolves_against_the_enclosing_scope() {
    let routine = ok("DECLARE x int := x; BEGIN NULL; END", &["x"]);
    let init = routine.block.decls[0].init.as_ref().expect("initializer");
    assert_eq!(init.text, "$1");
    assert_eq!(
        init.params,
        vec![VarId(0)],
        "the argument, not the new slot"
    );
}

#[test]
fn nested_blocks_shadow_and_then_restore() {
    let routine = ok(
        "DECLARE x int := 1; y int; z int; \
         BEGIN \
           DECLARE x int := 2; BEGIN y := x; END; \
           z := x; \
         END",
        &[],
    );
    let [Stmt::Block(inner), Stmt::Assign { value: outer, .. }] = &routine.block.stmts[..] else {
        panic!("expected a nested block then an assignment");
    };
    let [
        Stmt::Assign {
            value: shadowed, ..
        },
    ] = &inner.stmts[..]
    else {
        panic!("expected one assignment inside");
    };
    assert_eq!(shadowed.params, vec![VarId(4)], "the inner x");
    assert_eq!(outer.params, vec![VarId(1)], "the outer x again");
}

#[test]
fn if_elsif_else_arms_are_kept_in_order() {
    let routine = ok(
        "DECLARE r int; \
         BEGIN \
           IF a > 0 THEN r := 1; \
           ELSIF a < 0 THEN r := -1; \
           ELSE r := 0; \
           END IF; \
         END",
        &["a"],
    );
    let [
        Stmt::If {
            arms, else_body, ..
        },
    ] = &routine.block.stmts[..]
    else {
        panic!("expected one IF");
    };
    assert_eq!(arms.len(), 2);
    assert_eq!(arms[0].0.text, "$1 > 0");
    assert_eq!(arms[1].0.text, "$1 < 0");
    assert_eq!(else_body.as_ref().map(Vec::len), Some(1));
}

/// The SQL tokenizer takes `1..10` as the numbers `1.` and `.10`, so the range
/// separator has to be found in the source text. Every spelling must split in
/// the same place.
#[test]
fn for_range_bounds_split_on_the_dot_dot_however_it_is_written() {
    for (body, lower, upper) in [
        ("BEGIN FOR i IN 1..10 LOOP NULL; END LOOP; END", "1", "10"),
        ("BEGIN FOR i IN 1 .. 10 LOOP NULL; END LOOP; END", "1", "10"),
        ("BEGIN FOR i IN lo..hi LOOP NULL; END LOOP; END", "lo", "hi"),
        ("BEGIN FOR i IN 1..hi LOOP NULL; END LOOP; END", "1", "hi"),
        ("BEGIN FOR i IN lo..10 LOOP NULL; END LOOP; END", "lo", "10"),
        (
            "BEGIN FOR i IN (a+1)..(b) LOOP NULL; END LOOP; END",
            "(a+1)",
            "(b)",
        ),
        (
            "BEGIN FOR i IN 1.5..2.5 LOOP NULL; END LOOP; END",
            "1.5",
            "2.5",
        ),
    ] {
        let routine = ok(body, &[]);
        let [
            Stmt::ForRange {
                lower: lo,
                upper: hi,
                ..
            },
        ] = &routine.block.stmts[..]
        else {
            panic!("expected one FOR in {body:?}");
        };
        assert_eq!(lo.text, lower, "lower bound of {body:?}");
        assert_eq!(hi.text, upper, "upper bound of {body:?}");
    }
}

/// A `..` inside parentheses or a string is not the range separator.
#[test]
fn for_range_separator_ignores_nested_and_quoted_dots() {
    let routine = ok(
        "BEGIN FOR i IN f('a..b')..g(1) LOOP NULL; END LOOP; END",
        &[],
    );
    let [Stmt::ForRange { lower, upper, .. }] = &routine.block.stmts[..] else {
        panic!("expected one FOR");
    };
    assert_eq!(lower.text, "f('a..b')");
    assert_eq!(upper.text, "g(1)");
}

#[test]
fn for_loop_variable_is_scoped_to_the_loop_and_shadows() {
    let routine = ok(
        "DECLARE i int := 99; r int; s int; \
         BEGIN FOR i IN 1..3 LOOP r := i; END LOOP; s := i; END",
        &[],
    );
    let [
        Stmt::ForRange { var, body, .. },
        Stmt::Assign { value: after, .. },
    ] = &routine.block.stmts[..]
    else {
        panic!("expected a FOR then an assignment");
    };
    let [Stmt::Assign { value: inside, .. }] = &body[..] else {
        panic!("expected one assignment in the body");
    };
    assert_eq!(inside.params, vec![*var], "the loop variable");
    assert_eq!(after.params, vec![VarId(1)], "the declared i again");
}

#[test]
fn reverse_and_by_are_recorded() {
    let routine = ok(
        "BEGIN FOR i IN REVERSE 10..1 BY 2 LOOP NULL; END LOOP; END",
        &[],
    );
    let [
        Stmt::ForRange {
            direction, step, ..
        },
    ] = &routine.block.stmts[..]
    else {
        panic!("expected one FOR");
    };
    assert_eq!(*direction, LoopDirection::Reverse);
    assert_eq!(step.as_ref().map(|f| f.text.as_str()), Some("2"));
}

/// Labels are resolved at compile time, so a typo is a definition-time error
/// rather than a surprise at run time.
#[test]
fn exit_and_continue_labels_are_checked_when_compiled() {
    let routine = ok(
        "BEGIN <<outer>> LOOP LOOP EXIT outer; END LOOP; END LOOP; END",
        &[],
    );
    assert!(matches!(routine.block.stmts[..], [Stmt::Loop { .. }]));

    let e = err("BEGIN LOOP EXIT nope; END LOOP; END", &[]);
    assert!(
        e.message.contains("there is no label \"nope\""),
        "{}",
        e.message
    );

    let e = err("BEGIN EXIT; END", &[]);
    assert!(
        e.message
            .contains("EXIT cannot be used outside a loop, unless it has a label"),
        "{}",
        e.message
    );

    // EXIT may leave a labeled block; CONTINUE may not.
    ok("BEGIN <<b>> BEGIN EXIT b; END; END", &[]);
    let e = err("BEGIN <<b>> BEGIN CONTINUE b; END; END", &[]);
    assert!(
        e.message.contains("there is no label \"b\""),
        "{}",
        e.message
    );
}

#[test]
fn end_label_must_match_the_opening_one() {
    ok("BEGIN <<lbl>> LOOP EXIT lbl; END LOOP lbl; END", &[]);
    let e = err("BEGIN <<lbl>> LOOP EXIT lbl; END LOOP other; END", &[]);
    assert!(
        e.message.contains("differs from block's label"),
        "{}",
        e.message
    );
}

#[test]
fn select_into_lifts_its_targets_out_of_the_query() {
    let routine = ok(
        "DECLARE n int; m int; BEGIN SELECT count(*), max(id) INTO n, m FROM t WHERE x = a; END",
        &["a"],
    );
    let [
        Stmt::SelectInto {
            query,
            targets,
            strict,
            ..
        },
    ] = &routine.block.stmts[..]
    else {
        panic!("expected one SELECT INTO, got {:?}", routine.block.stmts);
    };
    assert_eq!(query.text, "SELECT count(*), max(id) FROM t WHERE x = $1");
    assert_eq!(targets, &[VarId(2), VarId(3)]);
    assert!(!strict);
    assert_eq!(query.params, vec![VarId(0)]);
}

#[test]
fn select_into_strict_is_recorded() {
    let routine = ok(
        "DECLARE n int; BEGIN SELECT id INTO STRICT n FROM t; END",
        &[],
    );
    let [Stmt::SelectInto { strict, .. }] = &routine.block.stmts[..] else {
        panic!("expected one SELECT INTO");
    };
    assert!(strict);
}

/// `INSERT INTO t` must not have its target table read as a variable list —
/// the two INTOs are spelled the same and mean opposite things.
#[test]
fn insert_into_is_not_a_select_into() {
    let routine = ok("BEGIN INSERT INTO t (x) VALUES (a); END", &["a"]);
    let [Stmt::Sql { query, .. }] = &routine.block.stmts[..] else {
        panic!(
            "expected a plain SQL statement, got {:?}",
            routine.block.stmts
        );
    };
    assert_eq!(query.text, "INSERT INTO t (x) VALUES ($1)");

    // ...but an INSERT ... RETURNING ... INTO still lifts its targets.
    let routine = ok(
        "DECLARE n int; BEGIN INSERT INTO t (x) VALUES (1) RETURNING id INTO n; END",
        &[],
    );
    let [Stmt::SelectInto { query, targets, .. }] = &routine.block.stmts[..] else {
        panic!("expected an INTO statement");
    };
    assert_eq!(query.text, "INSERT INTO t (x) VALUES (1) RETURNING id");
    assert_eq!(targets, &[VarId(1)]);
}

#[test]
fn raise_forms_are_parsed() {
    let routine = ok(
        "BEGIN RAISE NOTICE 'hello %, %', a, 2; \
               RAISE EXCEPTION 'bad' USING ERRCODE = 'XX999', HINT = 'fix it'; \
               RAISE division_by_zero; \
         END",
        &["a"],
    );
    let [
        Stmt::Raise(notice),
        Stmt::Raise(exception),
        Stmt::Raise(condition),
    ] = &routine.block.stmts[..]
    else {
        panic!("expected three RAISEs, got {:?}", routine.block.stmts);
    };

    assert_eq!(notice.level, RaiseLevel::Notice);
    assert_eq!(notice.format.as_deref(), Some("hello %, %"));
    assert_eq!(notice.args.len(), 2);
    assert_eq!(notice.args[0].text, "$1");

    assert_eq!(exception.level, RaiseLevel::Exception);
    assert_eq!(
        exception.using.errcode.as_ref().map(|f| f.text.as_str()),
        Some("'XX999'")
    );
    assert_eq!(
        exception.using.hint.as_ref().map(|f| f.text.as_str()),
        Some("'fix it'")
    );

    // No level given: EXCEPTION, and a bare word is a condition name.
    assert_eq!(condition.level, RaiseLevel::Exception);
    assert_eq!(condition.condition.as_deref(), Some("division_by_zero"));
}

#[test]
fn raise_rejects_a_repeated_or_unknown_using_option() {
    let e = err("BEGIN RAISE 'x' USING HINT = 'a', HINT = 'b'; END", &[]);
    assert!(
        e.message.contains("already specified: HINT"),
        "{}",
        e.message
    );

    let e = err("BEGIN RAISE 'x' USING NONSENSE = 'a'; END", &[]);
    assert!(
        e.message
            .contains("unrecognized RAISE statement option \"nonsense\""),
        "{}",
        e.message
    );

    // A bare re-raise needs an exception handler to re-raise from.
    let e = err("BEGIN RAISE; END", &[]);
    assert!(
        e.message
            .contains("RAISE without parameters cannot be used outside an exception handler"),
        "{}",
        e.message
    );
}

/// Constructs PostgreSQL has and this rung does not are named rather than
/// reported as a syntax error — the body is valid PL/pgSQL, just not yet run.
#[test]
fn unsupported_constructs_say_so() {
    for (body, needle) in [
        (
            "BEGIN NULL; EXCEPTION WHEN others THEN NULL; END",
            "EXCEPTION handlers",
        ),
        ("BEGIN RETURN NEXT 1; END", "RETURN NEXT and RETURN QUERY"),
        (
            "BEGIN RETURN QUERY SELECT 1; END",
            "RETURN NEXT and RETURN QUERY",
        ),
        ("BEGIN EXECUTE 'SELECT 1'; END", "EXECUTE in PL/pgSQL"),
        (
            "BEGIN FOREACH x IN ARRAY a LOOP NULL; END LOOP; END",
            "FOREACH in PL/pgSQL",
        ),
        (
            "BEGIN GET DIAGNOSTICS n = ROW_COUNT; END",
            "GET in PL/pgSQL",
        ),
        (
            "BEGIN FOR r IN SELECT * FROM t LOOP NULL; END LOOP; END",
            "only integer ranges",
        ),
    ] {
        let e = err(body, &[]);
        assert_eq!(e.code, "0A000", "{body:?} should be feature-not-supported");
        assert!(e.message.contains(needle), "{body:?}: {}", e.message);
    }
}

#[test]
fn syntax_errors_report_a_body_relative_line() {
    // Body line 1 is the (empty) remainder of the `$$` line, as PostgreSQL
    // numbers them, so the unterminated statement runs into `END` on line 4.
    let e = err("\nBEGIN\n  NULL\nEND", &[]);
    assert_eq!(e.code, "42601");
    assert_eq!(e.line, 4, "{}", e.message);

    // Assigning to a name nothing declared is caught where it is written.
    let e = err("\nBEGIN\n  x := 1;\nEND", &[]);
    assert_eq!(e.code, "42601");
    assert_eq!(e.message, "\"x\" is not a known variable");
    assert_eq!(e.line, 3);
}

/// A statement's recorded line is what the CONTEXT traceback reports, so it has
/// to survive compilation.
#[test]
fn statements_remember_their_line() {
    let routine = ok("\nBEGIN\n  NULL;\n  RAISE NOTICE 'x';\nEND", &[]);
    assert_eq!(routine.block.stmts[0].line(), 3);
    assert_eq!(routine.block.stmts[1].line(), 4);
}

/// Comments and original spacing survive into the lifted SQL: the text is
/// sliced from the source, never re-rendered from tokens.
#[test]
fn comments_inside_a_fragment_survive() {
    let routine = ok("DECLARE r int; BEGIN r := 1 /* keep me */ + 2; END", &[]);
    let [Stmt::Assign { value, .. }] = &routine.block.stmts[..] else {
        panic!("expected one assignment");
    };
    assert_eq!(value.text, "1 /* keep me */ + 2");
}

#[test]
fn trailing_input_after_the_outer_end_is_rejected() {
    let e = err("BEGIN NULL; END; SELECT 1;", &[]);
    assert!(
        e.message.contains("end of function definition"),
        "{}",
        e.message
    );
}

#[test]
fn a_fragment_records_its_own_line() {
    let routine = ok("\nBEGIN\n\n  PERFORM f(1);\nEND", &[]);
    let [Stmt::Perform { query, .. }] = &routine.block.stmts[..] else {
        panic!("expected one PERFORM");
    };
    assert_eq!(
        *query,
        SqlFragment {
            text: "f(1)".to_string(),
            params: vec![],
            line: 4,
        }
    );
}
