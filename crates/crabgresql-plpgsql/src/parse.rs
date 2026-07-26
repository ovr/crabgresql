//! Compile a PL/pgSQL routine body into a [`Routine`].
//!
//! Two things happen at once here, and it is worth being explicit about why.
//!
//! **Parsing** is ordinary recursive descent over the token stream; PL/pgSQL's
//! grammar is small and its keywords are all unreserved.
//!
//! **Lifting** is the interesting half. Every SQL construct in the body — the
//! right-hand side of an assignment, an `IF` condition, an embedded `INSERT` —
//! is copied out of the source verbatim, with each reference to a routine
//! variable replaced by a `$n` placeholder and recorded against its frame slot.
//! Doing that once, at compile time, means a call site pays only for binding
//! text it could not have bound earlier anyway (the catalog can change between
//! definition and call), and it lets the existing bind-parameter machinery
//! carry variable values in without a second substitution mechanism.

use std::collections::HashMap;

use crabgresql_parser::tokenizer::{Token, Word};
use crabgresql_pg_wire::sqlstate;

use crate::ast::{
    Block, CompileError, Decl, LoopDirection, Raise, RaiseLevel, RaiseUsing, Routine, SqlFragment,
    Stmt, VarId,
};
use crate::lexer::Lexer;

/// Compile the body of a `CREATE FUNCTION ... LANGUAGE plpgsql`.
///
/// `arg_names` are the routine's declared parameter names in order; an unnamed
/// parameter is still reachable as `$n`. The body is not bound against a
/// catalog — see the crate docs for why — so this reports syntax and structural
/// errors only, exactly as PostgreSQL's PL/pgSQL validator does.
pub fn compile(body: &str, arg_names: &[Option<String>]) -> Result<Routine, CompileError> {
    Compiler::new(body, arg_names)?.finish()
}

/// Compile a `DO $$ ... $$` block, which takes no arguments.
pub fn compile_inline_block(body: &str) -> Result<Routine, CompileError> {
    compile(body, &[])
}

/// One lexical scope's variable names.
type Scope = HashMap<String, VarId>;

struct Compiler<'a> {
    lex: Lexer<'a>,
    /// Innermost scope last. Name lookup walks it back to front, so an inner
    /// declaration shadows an outer one.
    scopes: Vec<Scope>,
    /// Slots allocated so far; also the frame size. Slots are never reused
    /// across sibling blocks, so entering a block never has to resize a frame.
    nvars: usize,
    /// How many leading slots are routine arguments — the range `$1..=nargs`
    /// refers to.
    nargs: usize,
    arg_names: Vec<Option<String>>,
    /// The slot holding `FOUND`.
    found: VarId,
    /// Loop labels currently in scope, innermost last, for `EXIT`/`CONTINUE`.
    loop_labels: Vec<Option<String>>,
    /// Block labels currently in scope, for `EXIT <label>` out of a block.
    block_labels: Vec<Option<String>>,
}

impl<'a> Compiler<'a> {
    fn new(body: &'a str, arg_names: &[Option<String>]) -> Result<Self, CompileError> {
        let mut args = Scope::new();
        for (i, name) in arg_names.iter().enumerate() {
            if let Some(name) = name {
                args.insert(name.clone(), VarId(i));
            }
        }
        // `FOUND` is a real variable in PostgreSQL, not a magic expression:
        // it lives in the routine's outermost scope, starts false, and is
        // updated by every statement that can report whether it matched a row.
        // Giving it a frame slot means a reference to it lifts into a `$n` like
        // any other variable, with no special case in the fragment rewriter.
        let found = VarId(arg_names.len());
        args.insert("found".to_string(), found);

        Ok(Self {
            lex: Lexer::new(body)?,
            scopes: vec![args],
            nvars: arg_names.len() + 1,
            nargs: arg_names.len(),
            arg_names: arg_names.to_vec(),
            found,
            loop_labels: Vec::new(),
            block_labels: Vec::new(),
        })
    }

    fn finish(mut self) -> Result<Routine, CompileError> {
        let block = self.block()?;
        // PostgreSQL allows an optional trailing semicolon after the outermost
        // END, and nothing else.
        self.lex.eat(&Token::SemiColon);
        if !self.lex.at_eof() {
            return Err(self.lex.unexpected("end of function definition"));
        }
        Ok(Routine {
            arg_names: self.arg_names,
            found: self.found,
            block,
            nvars: self.nvars,
        })
    }

    // -----------------------------------------------------------------------
    // Scopes and variables
    // -----------------------------------------------------------------------

    fn push_scope(&mut self) {
        self.scopes.push(Scope::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    /// Allocate a frame slot for `name` in the innermost scope. `name` is
    /// already normalized (an unquoted identifier folded to lowercase, a quoted
    /// one left alone), so lookups are exact — `"a"` finds the variable `a`,
    /// but `"A"` does not.
    fn declare(&mut self, name: &str) -> VarId {
        let var = VarId(self.nvars);
        self.nvars += 1;
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), var);
        }
        var
    }

    /// Resolve a normalized name to a frame slot, innermost scope first.
    fn lookup(&self, name: &str) -> Option<VarId> {
        self.scopes.iter().rev().find_map(|s| s.get(name)).copied()
    }

    // -----------------------------------------------------------------------
    // Blocks
    // -----------------------------------------------------------------------

    /// `[<<label>>] [DECLARE decls] BEGIN stmts END [label]`
    fn block(&mut self) -> Result<Block, CompileError> {
        let label = self.opt_label()?;
        self.push_scope();
        let result = self.block_body(label.clone());
        self.pop_scope();
        result
    }

    fn block_body(&mut self, label: Option<String>) -> Result<Block, CompileError> {
        let mut decls = Vec::new();
        if self.lex.eat_word("declare") {
            while !self.lex.at_word("begin") {
                if self.lex.at_eof() {
                    return Err(self.lex.unexpected("BEGIN"));
                }
                decls.push(self.declaration()?);
            }
        }
        self.lex.expect_word("begin")?;

        self.block_labels.push(label.clone());
        let stmts = self.statements_until_block_end();
        self.block_labels.pop();
        let stmts = stmts?;

        if self.lex.at_word("exception") {
            return Err(CompileError::unsupported(
                "EXCEPTION handlers in PL/pgSQL are not supported yet",
                self.lex.line(),
            ));
        }
        self.lex.expect_word("end")?;
        self.end_label(label.as_deref())?;

        Ok(Block {
            label,
            decls,
            stmts,
            exception: None,
        })
    }

    /// `<<label>>` before a block or loop.
    fn opt_label(&mut self) -> Result<Option<String>, CompileError> {
        // `<<` lexes as a single shift-left token.
        if !self.lex.eat(&Token::ShiftLeft) {
            return Ok(None);
        }
        let label = self.lex.expect_ident()?;
        if !self.lex.eat(&Token::ShiftRight) {
            return Err(self.lex.unexpected(">>"));
        }
        Ok(Some(label))
    }

    /// The optional label repeated after `END` / `END LOOP`, which must match
    /// the opening one.
    fn end_label(&mut self, opened: Option<&str>) -> Result<(), CompileError> {
        if !matches!(self.lex.peek(), Token::Word(_)) {
            return Ok(());
        }
        let line = self.lex.line();
        let name = self.lex.expect_ident()?;
        match opened {
            Some(opened) if opened == name => Ok(()),
            Some(opened) => Err(CompileError::syntax(
                format!("end label \"{name}\" differs from block's label \"{opened}\""),
                line,
            )),
            None => Err(CompileError::syntax(
                format!("end label \"{name}\" specified for unlabeled block"),
                line,
            )),
        }
    }

    /// `name [CONSTANT] type [NOT NULL] [ { DEFAULT | := } expr ] ;`
    fn declaration(&mut self) -> Result<Decl, CompileError> {
        let line = self.lex.line();
        let name = self.lex.expect_ident()?;
        let constant = self.lex.eat_word("constant");

        // The type runs to `NOT NULL`, an initializer, or the semicolon. It is
        // kept as text and resolved against the catalog at run time, so a body
        // can name a type that does not exist yet.
        let type_start = self.lex.mark();
        let type_end = self.scan_to(&[
            Stop::Token(Token::SemiColon),
            Stop::Token(Token::Assignment),
            // PostgreSQL accepts `=` as a synonym for `:=` in a declaration, so
            // it ends the type just as `:=` does. Without it the initializer is
            // swallowed into the type text and `DECLARE x int = 5` fails at run
            // time with `type "int = 5" does not exist`.
            Stop::Token(Token::Eq),
            Stop::Word("default"),
            Stop::Word("not"),
        ])?;
        let type_text = self.lex.slice(type_start, type_end).trim().to_string();
        if type_text.is_empty() {
            return Err(CompileError::syntax(
                format!("variable \"{name}\" has no type"),
                line,
            ));
        }

        let not_null = self.lex.eat_words(&["not", "null"]);
        let init = if self.lex.eat(&Token::Assignment)
            || self.lex.eat(&Token::Eq)
            || self.lex.eat_word("default")
        {
            Some(self.fragment(&[Stop::Token(Token::SemiColon)])?)
        } else {
            None
        };
        self.lex.expect(&Token::SemiColon)?;

        if not_null && init.is_none() {
            return Err(CompileError::syntax(
                format!(
                    "variable \"{name}\" must have a default value, since it's declared NOT NULL"
                ),
                line,
            ));
        }

        // Declared last, so `x int := x` refers to the *outer* x, as in PG.
        let var = self.declare(&name);
        Ok(Decl {
            name,
            var,
            type_text,
            constant,
            not_null,
            init,
        })
    }

    // -----------------------------------------------------------------------
    // Statements
    // -----------------------------------------------------------------------

    /// Statements up to (not including) the `END` / `EXCEPTION` that closes a
    /// block. PostgreSQL requires at least one, `NULL;` being the way to write
    /// none.
    fn statements_until_block_end(&mut self) -> Result<Vec<Stmt>, CompileError> {
        let mut stmts = Vec::new();
        loop {
            if self.lex.at_word("end") || self.lex.at_word("exception") {
                break;
            }
            if self.lex.at_eof() {
                return Err(self.lex.unexpected("END"));
            }
            stmts.push(self.statement()?);
        }
        Ok(stmts)
    }

    /// Statements up to one of `stop`, which is left unconsumed.
    fn statements_until(&mut self, stop: &[&str]) -> Result<Vec<Stmt>, CompileError> {
        let mut stmts = Vec::new();
        while !stop.iter().any(|k| self.lex.at_word(k)) {
            if self.lex.at_eof() {
                return Err(self.lex.unexpected("END"));
            }
            stmts.push(self.statement()?);
        }
        Ok(stmts)
    }

    fn statement(&mut self) -> Result<Stmt, CompileError> {
        let line = self.lex.line();

        // A label introduces either a labeled loop or a labeled nested block.
        if matches!(self.lex.peek(), Token::ShiftLeft) {
            let label = self.opt_label()?;
            return match self.lex.peek_word() {
                Some(w) if w.eq_ignore_ascii_case("loop") => self.loop_stmt(label, line),
                Some(w) if w.eq_ignore_ascii_case("while") => self.while_stmt(label, line),
                Some(w) if w.eq_ignore_ascii_case("for") => self.for_stmt(label, line),
                _ => {
                    self.push_scope();
                    let block = self.block_body(label);
                    self.pop_scope();
                    let block = block?;
                    self.lex.expect(&Token::SemiColon)?;
                    Ok(Stmt::Block(Box::new(block)))
                }
            };
        }

        match self.lex.peek_word().unwrap_or("").to_lowercase().as_str() {
            "begin" | "declare" => {
                let block = self.block()?;
                self.lex.expect(&Token::SemiColon)?;
                Ok(Stmt::Block(Box::new(block)))
            }
            "if" => self.if_stmt(line),
            "loop" => self.loop_stmt(None, line),
            "while" => self.while_stmt(None, line),
            "for" => self.for_stmt(None, line),
            "exit" => self.exit_or_continue(true, line),
            "continue" => self.exit_or_continue(false, line),
            "return" => self.return_stmt(line),
            "raise" => self.raise_stmt(line),
            "perform" => {
                self.lex.next();
                let query = self.fragment(&[Stop::Token(Token::SemiColon)])?;
                self.lex.expect(&Token::SemiColon)?;
                Ok(Stmt::Perform { query, line })
            }
            // `NULL;` as a statement — but `NULL` also starts nothing else, so
            // only treat it as the no-op statement when a semicolon follows.
            "null" if matches!(self.lex.nth(1), Token::SemiColon) => {
                self.lex.next();
                self.lex.next();
                Ok(Stmt::Null { line })
            }
            // Constructs PostgreSQL has and this rung does not. Naming them
            // beats "syntax error": the body is valid PL/pgSQL, just not yet
            // supported.
            kw @ ("case" | "execute" | "foreach" | "open" | "fetch" | "close" | "assert"
            | "get") => Err(CompileError::unsupported(
                format!("{} in PL/pgSQL is not supported yet", kw.to_uppercase()),
                line,
            )),
            _ => self.assignment_or_sql(line),
        }
    }

    /// Either `var := expr;` or a bare SQL statement (which may carry an
    /// `INTO` clause).
    fn assignment_or_sql(&mut self, line: u32) -> Result<Stmt, CompileError> {
        if let Token::Word(w) = self.lex.peek().clone()
            && matches!(self.lex.nth(1), Token::Assignment | Token::Eq)
        {
            let name = ident_value(&w);
            let target = self.lookup(&name);
            // `:=` is unambiguously an assignment, so an unknown name is an
            // error rather than a statement to hand to the SQL binder. `=`
            // is not: `UPDATE`'s and a comparison's `=` look the same here, so
            // it only assigns when the name really is a variable.
            match target {
                Some(target) => {
                    self.lex.next();
                    self.lex.next();
                    let value = self.fragment(&[Stop::Token(Token::SemiColon)])?;
                    self.lex.expect(&Token::SemiColon)?;
                    return Ok(Stmt::Assign {
                        target,
                        value,
                        line,
                    });
                }
                None if matches!(self.lex.nth(1), Token::Assignment) => {
                    return Err(CompileError::syntax(
                        format!("\"{name}\" is not a known variable"),
                        line,
                    ));
                }
                None => {}
            }
        }
        self.sql_stmt(line)
    }

    /// `IF cond THEN ... [ELSIF cond THEN ...] [ELSE ...] END IF;`
    fn if_stmt(&mut self, line: u32) -> Result<Stmt, CompileError> {
        self.lex.expect_word("if")?;
        let mut arms = Vec::new();
        let mut else_body = None;
        loop {
            let cond = self.fragment(&[Stop::Word("then")])?;
            self.lex.expect_word("then")?;
            let body = self.statements_until(&["elsif", "elseif", "else", "end"])?;
            arms.push((cond, body));
            if self.lex.eat_word("elsif") || self.lex.eat_word("elseif") {
                continue;
            }
            if self.lex.eat_word("else") {
                else_body = Some(self.statements_until(&["end"])?);
            }
            break;
        }
        self.lex.expect_word("end")?;
        self.lex.expect_word("if")?;
        self.lex.expect(&Token::SemiColon)?;
        Ok(Stmt::If {
            arms,
            else_body,
            line,
        })
    }

    fn loop_stmt(&mut self, label: Option<String>, line: u32) -> Result<Stmt, CompileError> {
        self.lex.expect_word("loop")?;
        let body = self.loop_body(label.clone())?;
        Ok(Stmt::Loop { label, body, line })
    }

    fn while_stmt(&mut self, label: Option<String>, line: u32) -> Result<Stmt, CompileError> {
        self.lex.expect_word("while")?;
        let cond = self.fragment(&[Stop::Word("loop")])?;
        self.lex.expect_word("loop")?;
        let body = self.loop_body(label.clone())?;
        Ok(Stmt::While {
            label,
            cond,
            body,
            line,
        })
    }

    /// `FOR v IN [REVERSE] lo .. hi [BY step] LOOP ... END LOOP;`
    ///
    /// The loop variable is implicitly declared `integer` and scoped to the
    /// loop, shadowing any outer variable of the same name.
    fn for_stmt(&mut self, label: Option<String>, line: u32) -> Result<Stmt, CompileError> {
        self.lex.expect_word("for")?;
        let name = self.lex.expect_ident()?;
        if matches!(self.lex.peek(), Token::Comma) {
            return Err(CompileError::unsupported(
                "FOR over a query with multiple target variables is not supported yet",
                line,
            ));
        }
        self.lex.expect_word("in")?;
        let direction = if self.lex.eat_word("reverse") {
            LoopDirection::Reverse
        } else {
            LoopDirection::Forward
        };

        // Find where the bounds end, then split them on `..` in the source —
        // the tokenizer swallows `..` into adjacent numbers, so there is no
        // token to match (see `Lexer::find_range_separator`).
        let bounds_start = self.lex.mark();
        let bounds_end = self.scan_to(&[Stop::Word("by"), Stop::Word("loop")])?;
        let Some((sep_start, sep_end)) = self.lex.find_range_separator(bounds_start, bounds_end)
        else {
            return Err(CompileError::unsupported(
                "FOR over a query is not supported yet; only integer ranges (lo .. hi) are",
                line,
            ));
        };
        let (Some(lo_start), Some(hi_end)) = (
            self.lex.byte_start(bounds_start),
            self.lex.byte_end(bounds_end),
        ) else {
            return Err(self.lex.unexpected("a range expression"));
        };
        let lower = self.fragment_from_text(self.lex.slice_bytes(lo_start, sep_start), line)?;
        let upper = self.fragment_from_text(self.lex.slice_bytes(sep_end, hi_end), line)?;

        let step = if self.lex.eat_word("by") {
            Some(self.fragment(&[Stop::Word("loop")])?)
        } else {
            None
        };
        self.lex.expect_word("loop")?;

        // The loop variable's scope covers only the body, and it is declared
        // after the bounds so `FOR i IN i..10` reads the outer `i`.
        self.push_scope();
        let var = self.declare(&name);
        let body = self.loop_body(label.clone());
        self.pop_scope();

        Ok(Stmt::ForRange {
            label,
            var,
            direction,
            lower,
            upper,
            step,
            body: body?,
            line,
        })
    }

    /// A loop's `... END LOOP [label];`, with the label in scope for EXIT.
    fn loop_body(&mut self, label: Option<String>) -> Result<Vec<Stmt>, CompileError> {
        self.loop_labels.push(label.clone());
        let body = self.statements_until(&["end"]);
        self.loop_labels.pop();
        let body = body?;
        self.lex.expect_word("end")?;
        self.lex.expect_word("loop")?;
        self.end_label(label.as_deref())?;
        self.lex.expect(&Token::SemiColon)?;
        Ok(body)
    }

    /// `EXIT|CONTINUE [label] [WHEN cond];`
    fn exit_or_continue(&mut self, is_exit: bool, line: u32) -> Result<Stmt, CompileError> {
        self.lex.next();
        let label = if matches!(self.lex.peek(), Token::Word(_)) && !self.lex.at_word("when") {
            Some(self.lex.expect_ident()?)
        } else {
            None
        };
        let when = if self.lex.eat_word("when") {
            Some(self.fragment(&[Stop::Token(Token::SemiColon)])?)
        } else {
            None
        };
        self.lex.expect(&Token::SemiColon)?;

        // PostgreSQL resolves the label at compile time; an unknown one, or a
        // bare EXIT outside any loop, is a definition-time error.
        let verb = if is_exit { "EXIT" } else { "CONTINUE" };
        match &label {
            Some(label) => {
                // CONTINUE may only name a loop; EXIT may also name a block.
                let visible = self.loop_labels.iter().flatten().any(|l| l == label)
                    || (is_exit && self.block_labels.iter().flatten().any(|l| l == label));
                if !visible {
                    return Err(CompileError::syntax(
                        format!("there is no label \"{label}\" surrounding this statement"),
                        line,
                    ));
                }
            }
            None if self.loop_labels.is_empty() => {
                return Err(CompileError::syntax(
                    format!("{verb} cannot be used outside a loop, unless it has a label"),
                    line,
                ));
            }
            None => {}
        }

        if is_exit {
            Ok(Stmt::Exit { label, when, line })
        } else {
            Ok(Stmt::Continue { label, when, line })
        }
    }

    fn return_stmt(&mut self, line: u32) -> Result<Stmt, CompileError> {
        self.lex.expect_word("return")?;
        if self.lex.eat_word("next") || self.lex.eat_word("query") {
            return Err(CompileError::unsupported(
                "RETURN NEXT and RETURN QUERY are not supported yet",
                line,
            ));
        }
        let value = if matches!(self.lex.peek(), Token::SemiColon) {
            None
        } else {
            Some(self.fragment(&[Stop::Token(Token::SemiColon)])?)
        };
        self.lex.expect(&Token::SemiColon)?;
        Ok(Stmt::Return { value, line })
    }

    /// `RAISE [level] [ 'format' [, arg ...] | condition_name ] [USING opt = expr, ...];`
    fn raise_stmt(&mut self, line: u32) -> Result<Stmt, CompileError> {
        self.lex.expect_word("raise")?;

        // The level is optional and defaults to EXCEPTION. It is only a level
        // if something follows it — `RAISE notice;` with a condition named
        // `notice` is not a thing anyone writes, and PG reads it as the level.
        let level = match self.lex.peek_word().unwrap_or("").to_lowercase().as_str() {
            "debug" => Some(RaiseLevel::Debug),
            "log" => Some(RaiseLevel::Log),
            "info" => Some(RaiseLevel::Info),
            "notice" => Some(RaiseLevel::Notice),
            "warning" => Some(RaiseLevel::Warning),
            "exception" => Some(RaiseLevel::Exception),
            _ => None,
        };
        if level.is_some() {
            self.lex.next();
        }
        let level = level.unwrap_or(RaiseLevel::Exception);

        if matches!(self.lex.peek(), Token::SemiColon) {
            // A bare `RAISE;` re-raises the exception being handled, which
            // requires an EXCEPTION block — and there are none yet.
            return Err(CompileError::syntax(
                "RAISE without parameters cannot be used outside an exception handler",
                line,
            ));
        }

        let mut format = None;
        let mut condition = None;
        let mut args = Vec::new();
        if matches!(
            self.lex.peek(),
            Token::SingleQuotedString(_) | Token::EscapedStringLiteral(_)
        ) {
            format = Some(self.lex.expect_string()?);
            while self.lex.eat(&Token::Comma) {
                args.push(self.fragment(&[
                    Stop::Token(Token::Comma),
                    Stop::Token(Token::SemiColon),
                    Stop::Word("using"),
                ])?);
            }
        } else if !self.lex.at_word("using") {
            condition = Some(self.lex.expect_ident()?);
        }

        let using = self.raise_using()?;
        self.lex.expect(&Token::SemiColon)?;

        // The placeholder count is fixed by the format string, so a mismatch is
        // knowable now — and PostgreSQL reports it when the routine is defined
        // rather than waiting for the RAISE to be reached.
        if let Some(format) = &format {
            let placeholders = count_placeholders(format);
            if placeholders > args.len() {
                return Err(CompileError::new(
                    sqlstate::SYNTAX_ERROR,
                    "too few parameters specified for RAISE",
                    line,
                ));
            }
            if placeholders < args.len() {
                return Err(CompileError::new(
                    sqlstate::SYNTAX_ERROR,
                    "too many parameters specified for RAISE",
                    line,
                ));
            }
        }

        Ok(Stmt::Raise(Box::new(Raise {
            level,
            format,
            args,
            condition,
            using,
            line,
        })))
    }

    fn raise_using(&mut self) -> Result<RaiseUsing, CompileError> {
        let mut using = RaiseUsing::default();
        if !self.lex.eat_word("using") {
            return Ok(using);
        }
        loop {
            let line = self.lex.line();
            let option = self.lex.expect_ident()?;
            if !self.lex.eat(&Token::Eq) && !self.lex.eat(&Token::Assignment) {
                return Err(self.lex.unexpected("="));
            }
            let value =
                self.fragment(&[Stop::Token(Token::Comma), Stop::Token(Token::SemiColon)])?;
            let slot = match option.as_str() {
                "message" => &mut using.message,
                "detail" => &mut using.detail,
                "hint" => &mut using.hint,
                "errcode" => &mut using.errcode,
                // PostgreSQL also accepts COLUMN/CONSTRAINT/DATATYPE/TABLE/
                // SCHEMA, which only populate error fields this engine does not
                // send. Accepting and ignoring them beats rejecting a body that
                // would work.
                "column" | "constraint" | "datatype" | "table" | "schema" => {
                    if !self.lex.eat(&Token::Comma) {
                        break;
                    }
                    continue;
                }
                other => {
                    return Err(CompileError::syntax(
                        format!("unrecognized RAISE statement option \"{other}\""),
                        line,
                    ));
                }
            };
            if slot.is_some() {
                return Err(CompileError::syntax(
                    format!("RAISE option already specified: {}", option.to_uppercase()),
                    line,
                ));
            }
            *slot = Some(value);
            if !self.lex.eat(&Token::Comma) {
                break;
            }
        }
        Ok(using)
    }

    /// A bare SQL statement, with its `INTO [STRICT] targets` clause — if any —
    /// lifted out of the text and turned into a list of frame slots.
    fn sql_stmt(&mut self, line: u32) -> Result<Stmt, CompileError> {
        let start = self.lex.mark();
        let end = self.scan_to(&[Stop::Token(Token::SemiColon)])?;
        if start == end {
            return Err(self.lex.unexpected("a statement"));
        }

        let Some(into) = self.find_into(start, end) else {
            let query = self.fragment_range(start, end, line)?;
            self.lex.expect(&Token::SemiColon)?;
            return Ok(Stmt::Sql { query, line });
        };

        // Re-read the INTO clause with the cursor parked on it, so target
        // names resolve through the ordinary path, then splice the text either
        // side of it back together.
        let resume = self.lex.mark();
        self.lex.reset(into + 1);
        let strict = self.lex.eat_word("strict");
        let mut targets = Vec::new();
        loop {
            let target_line = self.lex.line();
            let name = self.lex.expect_ident()?;
            match self.lookup(&name) {
                Some(var) => targets.push(var),
                None => {
                    return Err(CompileError::syntax(
                        format!("\"{name}\" is not a known variable"),
                        target_line,
                    ));
                }
            }
            if !self.lex.eat(&Token::Comma) {
                break;
            }
        }
        let after_targets = self.lex.mark();
        self.lex.reset(resume);

        let mut text = self.lex.slice(start, into).trim_end().to_string();
        let tail = self.lex.slice(after_targets, end);
        if !tail.trim().is_empty() {
            text.push(' ');
            text.push_str(tail.trim_start());
        }
        // The spliced text is re-lifted as one fragment; placeholders are
        // numbered over the result, not over the original token range.
        let query = self.fragment_from_text(&text, line)?;
        self.lex.expect(&Token::SemiColon)?;
        Ok(Stmt::SelectInto {
            query,
            targets,
            strict,
            line,
        })
    }

    /// The index of the `INTO` keyword introducing a PL/pgSQL target list in
    /// tokens `[start, end)`, if there is one.
    ///
    /// `INTO` must be at parenthesis depth zero and must not be the `INTO` of
    /// `INSERT INTO` — PL/pgSQL makes the same distinction, and getting it
    /// wrong would silently turn an INSERT's target table into a variable list.
    fn find_into(&self, start: usize, end: usize) -> Option<usize> {
        let mut depth = 0i32;
        for i in start..end {
            match self.lex.token(i)? {
                Token::LParen => depth += 1,
                Token::RParen => depth -= 1,
                _ if depth == 0 && self.lex.word_at(i, "into") => {
                    let is_insert_into = i > start && self.lex.word_at(i - 1, "insert");
                    if !is_insert_into {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        None
    }

    // -----------------------------------------------------------------------
    // Fragment lifting
    // -----------------------------------------------------------------------

    /// Lift the tokens from the cursor up to (not including) the first `stop`
    /// at parenthesis depth zero, leaving the cursor on that stop.
    fn fragment(&mut self, stop: &[Stop]) -> Result<SqlFragment, CompileError> {
        let line = self.lex.line();
        let start = self.lex.mark();
        let end = self.scan_to(stop)?;
        if start == end {
            return Err(self.lex.unexpected("an expression"));
        }
        self.fragment_range(start, end, line)
    }

    /// Lift tokens `[start, end)`, rewriting variable references to `$n`.
    fn fragment_range(
        &mut self,
        start: usize,
        end: usize,
        line: u32,
    ) -> Result<SqlFragment, CompileError> {
        let text = self.lex.slice(start, end);
        self.fragment_from_text(text, line)
    }

    /// Lift a piece of SQL given as text — used where the text had to be
    /// spliced first (a `SELECT ... INTO`, a `FOR` loop's range bounds).
    fn fragment_from_text(&mut self, text: &str, line: u32) -> Result<SqlFragment, CompileError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(CompileError::syntax("missing expression", line));
        }
        let inner = Lexer::new(text)?;
        let name_spans = name_positions(&inner, text);
        let mut params: Vec<VarId> = Vec::new();
        let mut out = String::with_capacity(text.len());
        let mut copied = 0usize;

        for i in 0..inner.len() {
            let (Some(tok_start), Some(tok_end)) = (inner.byte_start(i), inner.byte_end(i + 1))
            else {
                continue;
            };
            if name_spans
                .iter()
                .any(|(s, e)| tok_start >= *s && tok_end <= *e)
            {
                continue;
            }
            if let Some(var) = self.fragment_var(&inner, i) {
                out.push_str(inner.slice_bytes(copied, tok_start));
                // A variable referenced twice reuses one placeholder, so the
                // frame is read once per evaluation and a volatile expression
                // behind it cannot run twice.
                let index = match params.iter().position(|p| *p == var) {
                    Some(index) => index,
                    None => {
                        params.push(var);
                        params.len() - 1
                    }
                };
                out.push_str(&format!("${}", index + 1));
                copied = tok_end;
            }
        }
        out.push_str(inner.slice_bytes(copied, text.len()));

        Ok(SqlFragment {
            text: out,
            params,
            line,
        })
    }

    /// Whether token `i` of `inner` is a reference to a routine variable, and
    /// which slot it names.
    ///
    /// A bare word is a variable if a declaration of that name is in scope.
    /// PostgreSQL's `plpgsql.variable_conflict` GUC chooses what happens when
    /// the name is also a column of a table in the query; this rung always
    /// takes the variable, which is PostgreSQL's historical behavior and what
    /// almost all real code assumes.
    fn fragment_var(&self, inner: &Lexer<'_>, i: usize) -> Option<VarId> {
        match inner.token(i)? {
            // `$n` refers to argument n, as PostgreSQL makes arguments
            // reachable both by name and positionally.
            Token::Placeholder(p) => {
                let n: usize = p.strip_prefix('$')?.parse().ok()?;
                (n >= 1 && n <= self.nargs).then_some(VarId(n - 1))
            }
            Token::Word(w) => {
                // `a.b` is a qualified column reference, never a variable —
                // neither the qualifier nor the field.
                let follows_dot = i > 0 && matches!(inner.token(i - 1), Some(Token::Period));
                let precedes_dot = matches!(inner.token(i + 1), Some(Token::Period));
                if follows_dot || precedes_dot {
                    return None;
                }
                self.lookup(&ident_value(w))
            }
            _ => None,
        }
    }

    /// Advance past tokens until one of `stop` is reached at parenthesis depth
    /// zero, and return the index it stopped at. The stop token is not
    /// consumed. `CASE ... END` nests, so an `END` inside one does not close an
    /// enclosing block.
    fn scan_to(&mut self, stop: &[Stop]) -> Result<usize, CompileError> {
        let mut depth = 0i32;
        let mut case_depth = 0i32;
        loop {
            if self.lex.at_eof() {
                return Err(self.lex.unexpected(&describe(stop)));
            }
            if depth == 0 && case_depth == 0 && stop.iter().any(|s| self.at_stop(s)) {
                return Ok(self.lex.mark());
            }
            match self.lex.peek() {
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket => depth -= 1,
                _ if self.lex.at_word("case") => case_depth += 1,
                _ if case_depth > 0 && self.lex.at_word("end") => case_depth -= 1,
                _ => {}
            }
            self.lex.next();
        }
    }

    fn at_stop(&self, stop: &Stop) -> bool {
        match stop {
            Stop::Token(t) => self.lex.peek() == t,
            Stop::Word(k) => self.lex.at_word(k),
        }
    }
}

/// What ends a lifted fragment: a punctuation token, or a keyword given as
/// lowercase text (PL/pgSQL's keywords are not the SQL parser's).
enum Stop {
    Token(Token),
    Word(&'static str),
}

fn describe(stop: &[Stop]) -> String {
    let names: Vec<String> = stop
        .iter()
        .map(|s| match s {
            Stop::Token(t) => t.to_string(),
            Stop::Word(k) => k.to_uppercase(),
        })
        .collect();
    names.join(" or ")
}

/// Byte ranges of identifiers that name a *column* rather than stand for an
/// expression, and so must never be rewritten to a placeholder.
///
/// The two that matter — an `INSERT`'s column list and an `UPDATE`'s `SET`
/// targets — are indistinguishable from an expression at the token level, and
/// both routinely share a name with a routine variable: `INSERT INTO t (n)
/// VALUES (n)` means "insert the variable n into the column n". Substituting
/// the first `n` would turn the statement into nonsense.
///
/// The statement is parsed to find them. Text that does not parse yields no
/// exclusions, which is the safe direction: the binder reports the syntax error
/// at call time either way.
fn name_positions(lex: &Lexer<'_>, text: &str) -> Vec<(usize, usize)> {
    use crabgresql_parser::ast::{AssignmentTarget, Spanned, Statement};

    let Ok(statements) = crabgresql_parser::parse(text) else {
        return Vec::new();
    };
    let mut spans = Vec::new();
    let mut push = |span: crabgresql_parser::Span| {
        if let (Some(start), Some(end)) = (
            lex.offset_of(span.start.line, span.start.column),
            lex.offset_of(span.end.line, span.end.column),
        ) {
            spans.push((start, end));
        }
    };
    for statement in &statements {
        match statement {
            Statement::Insert(insert) => {
                for column in &insert.columns {
                    push(column.span());
                }
            }
            Statement::Update(update) => {
                for assignment in &update.assignments {
                    match &assignment.target {
                        AssignmentTarget::ColumnName(name) => push(name.span()),
                        AssignmentTarget::Tuple(names) => {
                            for name in names {
                                push(name.span());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    spans
}

/// How many arguments a `RAISE` format string consumes: every `%` except the
/// `%%` that stands for a literal percent sign.
fn count_placeholders(format: &str) -> usize {
    let mut count = 0usize;
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
        } else {
            count += 1;
        }
    }
    count
}

/// An identifier's value, folded to lowercase unless it was quoted.
fn ident_value(w: &Word) -> String {
    match w.quote_style {
        Some(_) => w.value.clone(),
        None => w.value.to_lowercase(),
    }
}
