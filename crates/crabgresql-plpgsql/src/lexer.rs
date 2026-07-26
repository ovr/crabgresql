//! Token stream over a PL/pgSQL routine body.
//!
//! PL/pgSQL is a thin imperative shell around SQL: everything between the
//! keywords is SQL text that this crate never interprets, only hands to the
//! binder. So rather than write a lexer, we reuse the SQL tokenizer — which
//! already gets string literals, `E''` escapes, comments and tagged
//! dollar-quoting right — and add the one thing it cannot do: map a token back
//! to its byte range in the source, so an embedded SQL construct can be lifted
//! out **as written**.
//!
//! Slicing the original text is deliberate. Re-rendering tokens through
//! `Display` would round-trip most input but silently change a string literal's
//! escaping and an identifier's quoting, which is exactly the kind of
//! difference a routine body is likely to depend on.

use crabgresql_parser::dialect::PostgreSqlDialect;
use crabgresql_parser::tokenizer::{Token, TokenWithSpan, Tokenizer, Word};

use crate::ast::CompileError;

/// A routine body's tokens, with the source text they came from.
pub struct Lexer<'a> {
    src: &'a str,
    /// Significant tokens only — whitespace and comments are dropped, since
    /// text is recovered by slicing `src` between spans rather than by
    /// re-rendering the token stream.
    tokens: Vec<TokenWithSpan>,
    /// Byte ranges of tokens whose contents are opaque text: string literals
    /// and comments. Used when searching the source for a delimiter the
    /// tokenizer does not surface (see [`Lexer::find_range_separator`]).
    opaque: Vec<(usize, usize)>,
    /// Byte offset of the start of each line of `src`, 0-based by line index.
    line_starts: Vec<usize>,
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Result<Self, CompileError> {
        let raw = Tokenizer::new(&PostgreSqlDialect {}, src)
            .tokenize_with_location()
            .map_err(|e| CompileError::syntax(e.message, e.location.line.max(1) as u32))?;
        let mut line_starts = vec![0usize];
        line_starts.extend(src.match_indices('\n').map(|(i, _)| i + 1));

        let mut lexer = Self {
            src,
            tokens: raw,
            opaque: Vec::new(),
            line_starts,
            pos: 0,
        };
        // Record the opaque ranges before whitespace is dropped, while indices
        // still line up with the raw stream.
        let mut opaque = Vec::new();
        for i in 0..lexer.tokens.len() {
            let is_opaque = matches!(
                lexer.tokens[i].token,
                Token::SingleQuotedString(_)
                    | Token::DoubleQuotedString(_)
                    | Token::EscapedStringLiteral(_)
                    | Token::DollarQuotedString(_)
                    | Token::Whitespace(
                        crabgresql_parser::tokenizer::Whitespace::SingleLineComment { .. }
                    )
                    | Token::Whitespace(
                        crabgresql_parser::tokenizer::Whitespace::MultiLineComment(_)
                    )
            );
            if is_opaque && let (Some(s), Some(e)) = (lexer.byte_start(i), lexer.byte_end(i + 1)) {
                opaque.push((s, e));
            }
        }
        lexer.opaque = opaque;
        lexer
            .tokens
            .retain(|t| !matches!(t.token, Token::Whitespace(_)));
        Ok(lexer)
    }

    /// The token at absolute index `i`, independent of the cursor. Used when
    /// walking a token range that has already been scanned.
    pub fn token(&self, i: usize) -> Option<&Token> {
        self.tokens.get(i).map(|t| &t.token)
    }

    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// The token at the cursor, or `EOF`.
    pub fn peek(&self) -> &Token {
        self.nth(0)
    }

    /// The token `n` places ahead of the cursor, or `EOF`.
    pub fn nth(&self, n: usize) -> &Token {
        static EOF: Token = Token::EOF;
        self.tokens.get(self.pos + n).map_or(&EOF, |t| &t.token)
    }

    /// Advance past one token and return it.
    pub fn next(&mut self) -> Token {
        let token = self.nth(0).clone();
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        token
    }

    pub fn at_eof(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    /// The 1-based body line the cursor sits on. Used for `CONTEXT:` lines,
    /// which PostgreSQL numbers relative to the body, not the outer statement.
    pub fn line(&self) -> u32 {
        self.tokens
            .get(self.pos.min(self.tokens.len().saturating_sub(1)))
            .map_or(1, |t| t.span.start.line.max(1) as u32)
    }

    /// The cursor's position, for slicing and for backtracking.
    pub fn mark(&self) -> usize {
        self.pos
    }

    pub fn reset(&mut self, mark: usize) {
        self.pos = mark;
    }

    /// The bare word at the cursor, folded to lowercase, or `None` if the
    /// token is not an unquoted word.
    ///
    /// PL/pgSQL keywords are matched by text rather than through the SQL
    /// parser's `Keyword` enum: the two vocabularies barely overlap (`LOOP`,
    /// `EXIT`, `PERFORM`, `ELSIF` are not SQL keywords at all), and every
    /// PL/pgSQL keyword is unreserved, so a quoted identifier of the same
    /// spelling must not match.
    pub fn peek_word(&self) -> Option<&str> {
        match self.peek() {
            Token::Word(Word {
                value,
                quote_style: None,
                ..
            }) => Some(value.as_str()),
            _ => None,
        }
    }

    /// Whether the cursor is on the keyword `kw` (given lowercase).
    pub fn at_word(&self, kw: &str) -> bool {
        self.peek_word().is_some_and(|w| w.eq_ignore_ascii_case(kw))
    }

    /// Whether the word at absolute index `i` is the keyword `kw`.
    pub fn word_at(&self, i: usize, kw: &str) -> bool {
        matches!(
            self.token(i),
            Some(Token::Word(Word { value, quote_style: None, .. })) if value.eq_ignore_ascii_case(kw)
        )
    }

    /// Consume the cursor token if it is the keyword `kw`.
    pub fn eat_word(&mut self, kw: &str) -> bool {
        if self.at_word(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consume `kws` in order, or consume nothing.
    pub fn eat_words(&mut self, kws: &[&str]) -> bool {
        let mark = self.mark();
        for kw in kws {
            if !self.eat_word(kw) {
                self.reset(mark);
                return false;
            }
        }
        true
    }

    pub fn expect_word(&mut self, kw: &str) -> Result<(), CompileError> {
        if self.eat_word(kw) {
            Ok(())
        } else {
            Err(self.unexpected(&kw.to_uppercase()))
        }
    }

    /// Consume the cursor token if it is `token`.
    pub fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == token {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    pub fn expect(&mut self, token: &Token) -> Result<(), CompileError> {
        if self.eat(token) {
            Ok(())
        } else {
            Err(self.unexpected(&token.to_string()))
        }
    }

    /// Consume an identifier, folding an unquoted one to lowercase as SQL does.
    /// PL/pgSQL keywords are unreserved, so any word is a valid identifier here.
    pub fn expect_ident(&mut self) -> Result<String, CompileError> {
        match self.peek().clone() {
            Token::Word(w) => {
                self.pos += 1;
                Ok(match w.quote_style {
                    Some(_) => w.value,
                    None => w.value.to_lowercase(),
                })
            }
            _ => Err(self.unexpected("identifier")),
        }
    }

    /// Consume a string literal — the format string of a `RAISE`, or a
    /// condition name written as one.
    pub fn expect_string(&mut self) -> Result<String, CompileError> {
        match self.peek().clone() {
            Token::SingleQuotedString(s) | Token::EscapedStringLiteral(s) => {
                self.pos += 1;
                Ok(s)
            }
            Token::DollarQuotedString(d) => {
                self.pos += 1;
                Ok(d.value)
            }
            _ => Err(self.unexpected("string literal")),
        }
    }

    /// PostgreSQL's `syntax error at or near "<token>"`, at the cursor's line.
    pub fn unexpected(&self, expected: &str) -> CompileError {
        let found = match self.peek() {
            Token::EOF => "end of function definition".to_string(),
            other => format!("\"{other}\""),
        };
        CompileError::syntax(
            format!("syntax error at or near {found}, expected {expected}"),
            self.line(),
        )
    }

    /// The source text spanned by tokens `[from, to)`, verbatim — comments and
    /// original spacing included.
    pub fn slice(&self, from: usize, to: usize) -> &'a str {
        let (Some(start), Some(end)) = (self.byte_start(from), self.byte_end(to)) else {
            return "";
        };
        self.src.get(start..end.max(start)).unwrap_or("")
    }

    /// Byte offset of the first character of token `i`.
    pub fn byte_start(&self, i: usize) -> Option<usize> {
        let span = self.tokens.get(i)?.span;
        self.offset_of(span.start.line, span.start.column)
    }

    /// Locate the `..` separating a `FOR` loop's range bounds, somewhere in the
    /// source spanned by tokens `[from, to)`. Returns the byte range the `..`
    /// itself occupies.
    ///
    /// This has to work on the source rather than the token stream because the
    /// SQL tokenizer greedily takes `1..10` as the two numbers `1.` and `.10` —
    /// there is no `..` token to match on. Searching the text instead is exact
    /// for every spelling (`1..10`, `lo..hi`, `1 .. 10`, `(a+1)..(b)`), and
    /// only needs the token spans to skip string literals and comments and to
    /// know the parenthesis depth.
    pub fn find_range_separator(&self, from: usize, to: usize) -> Option<(usize, usize)> {
        let (start, end) = (self.byte_start(from)?, self.byte_end(to)?);
        let mut depth = 0i32;
        let mut at = start;
        while at + 2 <= end {
            // Skip wholesale over any string literal or comment.
            if let Some((_, o_end)) = self
                .opaque
                .iter()
                .find(|(o_start, o_end)| at >= *o_start && at < *o_end && *o_end <= end)
            {
                at = *o_end;
                continue;
            }
            match self.src.as_bytes().get(at) {
                Some(b'(' | b'[') => depth += 1,
                Some(b')' | b']') => depth -= 1,
                Some(b'.') if depth == 0 && self.src.as_bytes().get(at + 1) == Some(&b'.') => {
                    return Some((at, at + 2));
                }
                _ => {}
            }
            at += 1;
        }
        None
    }

    /// The source text between two byte offsets, for the halves of a range
    /// whose split point [`Lexer::find_range_separator`] found.
    pub fn slice_bytes(&self, from: usize, to: usize) -> &'a str {
        self.src.get(from..to.max(from)).unwrap_or("")
    }

    /// Byte offset just past the last character of token `i - 1`.
    pub fn byte_end(&self, i: usize) -> Option<usize> {
        let span = self.tokens.get(i.checked_sub(1)?)?.span;
        self.offset_of(span.end.line, span.end.column)
    }

    /// Convert a 1-based (line, column) to a byte offset. The tokenizer counts
    /// columns in characters, so the column is walked as a `char` count rather
    /// than added to the line's byte offset.
    fn offset_of(&self, line: u64, column: u64) -> Option<usize> {
        let line_start = *self.line_starts.get(line.checked_sub(1)? as usize)?;
        let rest = self.src.get(line_start..)?;
        let col = column.saturating_sub(1) as usize;
        Some(line_start + rest.char_indices().nth(col).map_or(rest.len(), |(i, _)| i))
    }
}
