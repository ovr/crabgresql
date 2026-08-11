//! The PL/pgSQL syntax tree.
//!
//! A routine body is compiled once, at `CREATE FUNCTION` time or on first call,
//! and the result is immutable — so every SQL construct embedded in the body is
//! already lifted out as a [`SqlFragment`] with the routine's variables
//! rewritten to `$n` placeholders. Executing a statement is then a matter of
//! binding the fragment's text with the frame's current values.

use crabgresql_parser::Span;

/// A frame slot: the index of a variable in the routine's flat variable table.
/// Slots are assigned at compile time in declaration order, arguments first, so
/// a frame is a `Vec<Value>` and lookup is an index rather than a name hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VarId(pub usize);

/// A piece of SQL lifted out of a routine body, with every reference to a
/// PL/pgSQL variable already rewritten to a `$n` placeholder.
///
/// The rewrite happens once per routine rather than once per call, and works by
/// slicing the original body text between token spans — not by re-rendering
/// tokens, which would lose a string literal's escaping and an identifier's
/// quoting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlFragment {
    /// The rewritten SQL, e.g. `$1 + f($2)`. For an expression fragment this is
    /// the expression alone; the interpreter prefixes `SELECT `.
    pub text: String,
    /// The frame slot behind each placeholder: `params[i]` feeds `$(i+1)`. A
    /// variable referenced twice reuses one placeholder, so it is read from the
    /// frame exactly once per evaluation.
    pub params: Vec<VarId>,
    /// 1-based line within the routine body, for the `CONTEXT:` traceback.
    pub line: u32,
}

/// One `DECLARE` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decl {
    pub name: String,
    pub var: VarId,
    /// The declared type, as written. Resolved against the catalog at run time
    /// (it may name a `CREATE TYPE`), not at compile time — compiling a body
    /// must not depend on a catalog, so a body can be stored before the types
    /// it mentions exist.
    pub type_text: String,
    /// `CONSTANT`: assignment after initialization is an error.
    pub constant: bool,
    /// `NOT NULL`: a null assignment is an error, and the declaration must have
    /// an initializer.
    pub not_null: bool,
    /// The `:=` / `DEFAULT` initializer. `None` initializes to NULL.
    pub init: Option<SqlFragment>,
}

/// A `BEGIN ... END` block, optionally preceded by `DECLARE` and a label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub label: Option<String>,
    pub decls: Vec<Decl>,
    pub stmts: Vec<Stmt>,
    /// `EXCEPTION WHEN ...` handlers. The field exists so that adding them is a
    /// local change rather than a reshaping of every block.
    ///
    /// TODO: support `EXCEPTION WHEN` handlers — they need a subtransaction
    /// per block entry, so until that exists the parser rejects the clause and
    /// this field stays `None`.
    pub exception: Option<()>,
}

/// Which end of a range a `FOR` loop counts from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopDirection {
    Forward,
    Reverse,
}

/// `RAISE` severity. `EXCEPTION` aborts; the rest are diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaiseLevel {
    Debug,
    Log,
    Info,
    Notice,
    Warning,
    Exception,
}

impl RaiseLevel {
    /// The SQLSTATE a `RAISE` at this level defaults to. PostgreSQL uses
    /// `P0001` (`raise_exception`) for EXCEPTION and successful-completion for
    /// every diagnostic level.
    pub fn default_sqlstate(self) -> &'static str {
        match self {
            RaiseLevel::Exception => "P0001",
            _ => "00000",
        }
    }

    /// How the level is spelled in a client's severity field.
    pub fn severity(self) -> &'static str {
        match self {
            RaiseLevel::Debug => "DEBUG",
            RaiseLevel::Log => "LOG",
            RaiseLevel::Info => "INFO",
            RaiseLevel::Notice => "NOTICE",
            RaiseLevel::Warning => "WARNING",
            RaiseLevel::Exception => "ERROR",
        }
    }
}

/// The optional `USING option = expr, ...` trailer on `RAISE`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RaiseUsing {
    pub message: Option<SqlFragment>,
    pub detail: Option<SqlFragment>,
    pub hint: Option<SqlFragment>,
    /// `ERRCODE`, as either a 5-character SQLSTATE or a condition name.
    pub errcode: Option<SqlFragment>,
}

/// A `RAISE` statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Raise {
    pub level: RaiseLevel,
    /// The format string, when written in the `RAISE level 'fmt', args` form.
    /// `None` for the bare `RAISE level USING MESSAGE = ...` form.
    pub format: Option<String>,
    /// Arguments consumed by the format string's `%` placeholders.
    pub args: Vec<SqlFragment>,
    /// A condition name written in place of a format string, e.g.
    /// `RAISE division_by_zero`, which supplies both SQLSTATE and message.
    pub condition: Option<String>,
    pub using: RaiseUsing,
    pub line: u32,
}

/// One PL/pgSQL statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stmt {
    /// A nested `BEGIN ... END`, with its own scope.
    Block(Box<Block>),
    /// `NULL;` — does nothing. Distinct from an empty statement list because
    /// PostgreSQL requires at least one statement in a block.
    Null {
        line: u32,
    },
    /// `var := expr;`
    Assign {
        target: VarId,
        value: SqlFragment,
        line: u32,
    },
    /// `IF cond THEN ... ELSIF cond THEN ... ELSE ... END IF;`
    If {
        /// One `(condition, body)` per `IF`/`ELSIF` arm, in order.
        arms: Vec<(SqlFragment, Vec<Stmt>)>,
        else_body: Option<Vec<Stmt>>,
        line: u32,
    },
    /// `[<<label>>] LOOP ... END LOOP;` — an unconditional loop, exited by
    /// `EXIT` or `RETURN`.
    Loop {
        label: Option<String>,
        body: Vec<Stmt>,
        line: u32,
    },
    /// `[<<label>>] WHILE cond LOOP ... END LOOP;`
    While {
        label: Option<String>,
        cond: SqlFragment,
        body: Vec<Stmt>,
        line: u32,
    },
    /// `[<<label>>] FOR v IN [REVERSE] lo..hi [BY step] LOOP ... END LOOP;`
    ///
    /// The loop variable is implicitly declared as `integer`, scoped to the
    /// loop, and shadows any outer variable of the same name.
    ForRange {
        label: Option<String>,
        var: VarId,
        direction: LoopDirection,
        lower: SqlFragment,
        upper: SqlFragment,
        step: Option<SqlFragment>,
        body: Vec<Stmt>,
        line: u32,
    },
    /// `EXIT [label] [WHEN cond];`
    Exit {
        label: Option<String>,
        when: Option<SqlFragment>,
        line: u32,
    },
    /// `CONTINUE [label] [WHEN cond];`
    Continue {
        label: Option<String>,
        when: Option<SqlFragment>,
        line: u32,
    },
    /// `RETURN [expr];` — the expression is absent in a procedure or `DO` block.
    Return {
        value: Option<SqlFragment>,
        line: u32,
    },
    Raise(Box<Raise>),
    /// `PERFORM query;` — run a query for its side effects, discarding rows.
    Perform {
        query: SqlFragment,
        line: u32,
    },
    /// `SELECT ... INTO [STRICT] targets FROM ...;`
    SelectInto {
        query: SqlFragment,
        targets: Vec<VarId>,
        /// `STRICT`: exactly one row must match, else P0002/P0003.
        strict: bool,
        line: u32,
    },
    /// A bare embedded SQL statement — `INSERT`/`UPDATE`/`DELETE`, or any other
    /// statement run for effect. Sets `FOUND` and `ROW_COUNT`.
    Sql {
        query: SqlFragment,
        line: u32,
    },
}

impl Stmt {
    /// The 1-based line within the routine body this statement starts on.
    pub fn line(&self) -> u32 {
        match self {
            Stmt::Block(b) => b.stmts.first().map_or(1, Stmt::line),
            Stmt::Null { line }
            | Stmt::Assign { line, .. }
            | Stmt::If { line, .. }
            | Stmt::Loop { line, .. }
            | Stmt::While { line, .. }
            | Stmt::ForRange { line, .. }
            | Stmt::Exit { line, .. }
            | Stmt::Continue { line, .. }
            | Stmt::Return { line, .. }
            | Stmt::Perform { line, .. }
            | Stmt::SelectInto { line, .. }
            | Stmt::Sql { line, .. } => *line,
            Stmt::Raise(r) => r.line,
        }
    }

    /// How PostgreSQL labels this statement in a `CONTEXT:` traceback line —
    /// the `<label>` of `PL/pgSQL function f() line N at <label>`.
    pub fn context_label(&self) -> &'static str {
        match self {
            Stmt::Block(_) => "SQL statement",
            Stmt::Null { .. } => "NULL",
            Stmt::Assign { .. } => "assignment",
            Stmt::If { .. } => "IF",
            Stmt::Loop { .. } | Stmt::While { .. } => "LOOP",
            Stmt::ForRange { .. } => "FOR with integer loop variable",
            Stmt::Exit { .. } => "EXIT",
            Stmt::Continue { .. } => "CONTINUE",
            Stmt::Return { .. } => "RETURN",
            Stmt::Raise(_) => "RAISE",
            Stmt::Perform { .. } => "PERFORM",
            Stmt::SelectInto { .. } => "SQL statement",
            Stmt::Sql { .. } => "SQL statement",
        }
    }
}

/// A compiled routine body: the parsed program plus the shape of its frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Routine {
    /// The routine's parameter names, in declaration order. Slots `0..len` of
    /// the frame hold the arguments, reachable by these names and as `$n`.
    pub arg_names: Vec<Option<String>>,
    /// The slot holding `FOUND`, which PostgreSQL makes a real variable of the
    /// routine's outermost scope rather than a magic expression.
    pub found: VarId,
    pub block: Block,
    /// Total frame size — every declaration in every nested block gets its own
    /// slot, so entering a block never reallocates.
    pub nvars: usize,
}

/// A PL/pgSQL compile-time error. Carries a body-relative line so the caller can
/// build PostgreSQL's `CONTEXT: compilation of PL/pgSQL function "f" near line N`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    pub code: &'static str,
    pub message: String,
    /// 1-based line within the routine body.
    pub line: u32,
    /// Optional `HINT:` line, for the diagnostics PG pairs with one.
    pub hint: Option<String>,
}

impl CompileError {
    pub fn new(code: &'static str, message: impl Into<String>, line: u32) -> Self {
        Self {
            code,
            message: message.into(),
            line,
            hint: None,
        }
    }

    pub fn syntax(message: impl Into<String>, line: u32) -> Self {
        Self::new(crabgresql_pg_wire::sqlstate::SYNTAX_ERROR, message, line)
    }

    pub fn unsupported(message: impl Into<String>, line: u32) -> Self {
        Self::new(
            crabgresql_pg_wire::sqlstate::FEATURE_NOT_SUPPORTED,
            message,
            line,
        )
    }

    pub fn with_hint(mut self, hint: Option<String>) -> Self {
        self.hint = hint;
        self
    }

    pub fn at_span(mut self, span: Span) -> Self {
        if span.start.line != 0 {
            self.line = span.start.line as u32;
        }
        self
    }
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CompileError {}
