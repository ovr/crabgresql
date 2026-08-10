// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! SQL Tokenizer
//!
//! The tokenizer (a.k.a. lexer) converts a string into a sequence of tokens.
//!
//! The tokens then form the input for the parser, which outputs an Abstract Syntax Tree (AST).

#[cfg(not(feature = "std"))]
use alloc::{
    borrow::ToOwned,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::num::NonZeroU8;
use core::str::Chars;
use core::{cmp, fmt};
use core::{iter::Peekable, str};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[cfg(feature = "visitor")]
use sqlparser_derive::{Visit, VisitMut};

use crate::ast::{DollarQuotedString, QuoteDelimitedString};
use crate::dialect::Dialect;
use crate::dialect::{GenericDialect, PostgreSqlDialect};
use crate::keywords::{Keyword, ALL_KEYWORDS, ALL_KEYWORDS_INDEX};

/// SQL Token enumeration
#[derive(Debug, Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "visitor", derive(Visit, VisitMut))]
pub enum Token {
    /// An end-of-file marker, not a real token
    EOF,
    /// A keyword (like SELECT) or an optionally quoted SQL identifier
    Word(Word),
    /// An unsigned numeric literal
    Number(String, bool),
    /// A character that could not be tokenized
    Char(char),
    /// Single quoted string: i.e: 'string'
    SingleQuotedString(String),
    /// Double quoted string: i.e: "string"
    DoubleQuotedString(String),
    /// Triple single quoted strings: Example '''abc'''
    /// [BigQuery](https://cloud.google.com/bigquery/docs/reference/standard-sql/lexical#quoted_literals)
    TripleSingleQuotedString(String),
    /// Triple double quoted strings: Example """abc"""
    /// [BigQuery](https://cloud.google.com/bigquery/docs/reference/standard-sql/lexical#quoted_literals)
    TripleDoubleQuotedString(String),
    /// Dollar quoted string: i.e: $$string$$ or $tag_name$string$tag_name$
    DollarQuotedString(DollarQuotedString),
    /// Byte string literal: i.e: b'string' or B'string' (note that some backends, such as
    /// PostgreSQL, may treat this syntax as a bit string literal instead, i.e: b'10010101')
    SingleQuotedByteStringLiteral(String),
    /// Byte string literal: i.e: b"string" or B"string"
    DoubleQuotedByteStringLiteral(String),
    /// Triple single quoted literal with byte string prefix. Example `B'''abc'''`
    /// [BigQuery](https://cloud.google.com/bigquery/docs/reference/standard-sql/lexical#quoted_literals)
    TripleSingleQuotedByteStringLiteral(String),
    /// Triple double quoted literal with byte string prefix. Example `B"""abc"""`
    /// [BigQuery](https://cloud.google.com/bigquery/docs/reference/standard-sql/lexical#quoted_literals)
    TripleDoubleQuotedByteStringLiteral(String),
    /// Single quoted literal with raw string prefix. Example `R'abc'`
    /// [BigQuery](https://cloud.google.com/bigquery/docs/reference/standard-sql/lexical#quoted_literals)
    SingleQuotedRawStringLiteral(String),
    /// Double quoted literal with raw string prefix. Example `R"abc"`
    /// [BigQuery](https://cloud.google.com/bigquery/docs/reference/standard-sql/lexical#quoted_literals)
    DoubleQuotedRawStringLiteral(String),
    /// Triple single quoted literal with raw string prefix. Example `R'''abc'''`
    /// [BigQuery](https://cloud.google.com/bigquery/docs/reference/standard-sql/lexical#quoted_literals)
    TripleSingleQuotedRawStringLiteral(String),
    /// Triple double quoted literal with raw string prefix. Example `R"""abc"""`
    /// [BigQuery](https://cloud.google.com/bigquery/docs/reference/standard-sql/lexical#quoted_literals)
    TripleDoubleQuotedRawStringLiteral(String),
    /// "National" string literal: i.e: N'string'
    NationalStringLiteral(String),
    /// Quote delimited literal. Examples `Q'{ab'c}'`, `Q'|ab'c|'`, `Q'|ab|c|'`
    /// [Oracle](https://docs.oracle.com/en/database/oracle/oracle-database/21/sqlrf/Literals.html#GUID-1824CBAA-6E16-4921-B2A6-112FB02248DA)
    QuoteDelimitedStringLiteral(QuoteDelimitedString),
    /// "Nationa" quote delimited literal. Examples `NQ'{ab'c}'`, `NQ'|ab'c|'`, `NQ'|ab|c|'`
    /// [Oracle](https://docs.oracle.com/en/database/oracle/oracle-database/21/sqlrf/Literals.html#GUID-1824CBAA-6E16-4921-B2A6-112FB02248DA)
    NationalQuoteDelimitedStringLiteral(QuoteDelimitedString),
    /// "escaped" string literal, which are an extension to the SQL standard: i.e: e'first \n second' or E 'first \n second'
    EscapedStringLiteral(String),
    /// Unicode string literal: i.e: U&'first \000A second'
    UnicodeStringLiteral(String),
    /// Hexadecimal string literal: i.e.: X'deadbeef'
    HexStringLiteral(String),
    /// Comma
    Comma,
    /// Whitespace (space, tab, etc)
    Whitespace(Whitespace),
    /// Double equals sign `==`
    DoubleEq,
    /// Equality operator `=`
    Eq,
    /// Not Equals operator `<>` (or `!=` in some dialects)
    Neq,
    /// Less Than operator `<`
    Lt,
    /// Greater Than operator `>`
    Gt,
    /// Less Than Or Equals operator `<=`
    LtEq,
    /// Greater Than Or Equals operator `>=`
    GtEq,
    /// Spaceship operator <=>
    Spaceship,
    /// Plus operator `+`
    Plus,
    /// Minus operator `-`
    Minus,
    /// Multiplication operator `*`
    Mul,
    /// Division operator `/`
    Div,
    /// Integer division operator `//` in DuckDB
    DuckIntDiv,
    /// Modulo Operator `%`
    Mod,
    /// String concatenation `||`
    StringConcat,
    /// Left parenthesis `(`
    LParen,
    /// Right parenthesis `)`
    RParen,
    /// Period (used for compound identifiers or projections into nested types)
    Period,
    /// Colon `:`
    Colon,
    /// DoubleColon `::` (used for casting in PostgreSQL)
    DoubleColon,
    /// Assignment `:=` (used for keyword argument in DuckDB macros and some functions, and for variable declarations in DuckDB and Snowflake)
    Assignment,
    /// SemiColon `;` used as separator for COPY and payload
    SemiColon,
    /// Backslash `\` used in terminating the COPY payload with `\.`
    Backslash,
    /// Left bracket `[`
    LBracket,
    /// Right bracket `]`
    RBracket,
    /// Ampersand `&`
    Ampersand,
    /// Pipe `|`
    Pipe,
    /// Caret `^`
    Caret,
    /// Left brace `{`
    LBrace,
    /// Right brace `}`
    RBrace,
    /// Right Arrow `=>`
    RArrow,
    /// Sharp `#` used for PostgreSQL Bitwise XOR operator, also PostgreSQL/Redshift geometrical unary/binary operator (Number of points in path or polygon/Intersection)
    Sharp,
    /// `##` PostgreSQL/Redshift geometrical binary operator (Point of closest proximity)
    DoubleSharp,
    /// Tilde `~` used for PostgreSQL Bitwise NOT operator or case sensitive match regular expression operator
    Tilde,
    /// `~*` , a case insensitive match regular expression operator in PostgreSQL
    TildeAsterisk,
    /// `!~` , a case sensitive not match regular expression operator in PostgreSQL
    ExclamationMarkTilde,
    /// `!~*` , a case insensitive not match regular expression operator in PostgreSQL
    ExclamationMarkTildeAsterisk,
    /// `~~`, a case sensitive match pattern operator in PostgreSQL
    DoubleTilde,
    /// `~~*`, a case insensitive match pattern operator in PostgreSQL
    DoubleTildeAsterisk,
    /// `!~~`, a case sensitive not match pattern operator in PostgreSQL
    ExclamationMarkDoubleTilde,
    /// `!~~*`, a case insensitive not match pattern operator in PostgreSQL
    ExclamationMarkDoubleTildeAsterisk,
    /// `<<`, a bitwise shift left operator in PostgreSQL
    ShiftLeft,
    /// `>>`, a bitwise shift right operator in PostgreSQL
    ShiftRight,
    /// `&&`, an overlap operator in PostgreSQL
    Overlap,
    /// Exclamation Mark `!` used for PostgreSQL factorial operator
    ExclamationMark,
    /// Double Exclamation Mark `!!` used for PostgreSQL prefix factorial operator
    DoubleExclamationMark,
    /// AtSign `@` used for PostgreSQL abs operator, also PostgreSQL/Redshift geometrical unary/binary operator (Center, Contained or on)
    AtSign,
    /// `^@`, a "starts with" string operator in PostgreSQL
    CaretAt,
    /// `|/`, a square root math operator in PostgreSQL
    PGSquareRoot,
    /// `||/`, a cube root math operator in PostgreSQL
    PGCubeRoot,
    /// `?` or `$` , a prepared statement arg placeholder
    Placeholder(String),
    /// `->`, used as a operator to extract json field in PostgreSQL
    Arrow,
    /// `->>`, used as a operator to extract json field as text in PostgreSQL
    LongArrow,
    /// `#>`, extracts JSON sub-object at the specified path
    HashArrow,
    /// `@-@` PostgreSQL/Redshift geometrical unary operator (Length or circumference)
    AtDashAt,
    /// `?-` PostgreSQL/Redshift geometrical unary/binary operator (Is horizontal?/Are horizontally aligned?)
    QuestionMarkDash,
    /// `&<` PostgreSQL/Redshift geometrical binary operator (Overlaps to left?)
    AmpersandLeftAngleBracket,
    /// `&>` PostgreSQL/Redshift geometrical binary operator (Overlaps to right?)`
    AmpersandRightAngleBracket,
    /// `&<|` PostgreSQL/Redshift geometrical binary operator (Does not extend above?)`
    AmpersandLeftAngleBracketVerticalBar,
    /// `|&>` PostgreSQL/Redshift geometrical binary operator (Does not extend below?)`
    VerticalBarAmpersandRightAngleBracket,
    /// `<->` PostgreSQL/Redshift geometrical binary operator (Distance between)
    TwoWayArrow,
    /// `<^` PostgreSQL/Redshift geometrical binary operator (Is below?)
    LeftAngleBracketCaret,
    /// `>^` PostgreSQL/Redshift geometrical binary operator (Is above?)
    RightAngleBracketCaret,
    /// `?#` PostgreSQL/Redshift geometrical binary operator (Intersects or overlaps)
    QuestionMarkSharp,
    /// `?-|` PostgreSQL/Redshift geometrical binary operator (Is perpendicular?)
    QuestionMarkDashVerticalBar,
    /// `?||` PostgreSQL/Redshift geometrical binary operator (Are parallel?)
    QuestionMarkDoubleVerticalBar,
    /// `~=` PostgreSQL/Redshift geometrical binary operator (Same as)
    TildeEqual,
    /// `<<| PostgreSQL/Redshift geometrical binary operator (Is strictly below?)
    ShiftLeftVerticalBar,
    /// `|>> PostgreSQL/Redshift geometrical binary operator (Is strictly above?)
    VerticalBarShiftRight,
    /// `#>>`, extracts JSON sub-object at the specified path as text
    HashLongArrow,
    /// jsonb @> jsonb -> boolean: Test whether left json contains the right json
    AtArrow,
    /// jsonb <@ jsonb -> boolean: Test whether right json contains the left json
    ArrowAt,
    /// jsonb #- text[] -> jsonb: Deletes the field or array element at the specified
    /// path, where path elements can be either field keys or array indexes.
    HashMinus,
    /// jsonb @? jsonpath -> boolean: Does JSON path return any item for the specified
    /// JSON value?
    AtQuestion,
    /// jsonb @@ jsonpath → boolean: Returns the result of a JSON path predicate check
    /// for the specified JSON value. Only the first item of the result is taken into
    /// account. If the result is not Boolean, then NULL is returned.
    AtAt,
    /// jsonb ? text -> boolean: Checks whether the string exists as a top-level key within the
    /// jsonb object
    Question,
    /// jsonb ?& text[] -> boolean: Check whether all members of the text array exist as top-level
    /// keys within the jsonb object
    QuestionAnd,
    /// jsonb ?| text[] -> boolean: Check whether any member of the text array exists as top-level
    /// keys within the jsonb object
    QuestionPipe,
    /// Custom binary operator
    /// This is used to represent any custom binary operator that is not part of the SQL standard.
    /// PostgreSQL allows defining custom binary operators using CREATE OPERATOR.
    CustomBinaryOperator(String),
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Token::EOF => f.write_str("EOF"),
            Token::Word(ref w) => write!(f, "{w}"),
            Token::Number(ref n, l) => write!(f, "{}{long}", n, long = if *l { "L" } else { "" }),
            Token::Char(ref c) => write!(f, "{c}"),
            Token::SingleQuotedString(ref s) => write!(f, "'{s}'"),
            Token::TripleSingleQuotedString(ref s) => write!(f, "'''{s}'''"),
            Token::DoubleQuotedString(ref s) => write!(f, "\"{s}\""),
            Token::TripleDoubleQuotedString(ref s) => write!(f, "\"\"\"{s}\"\"\""),
            Token::DollarQuotedString(ref s) => write!(f, "{s}"),
            Token::NationalStringLiteral(ref s) => write!(f, "N'{s}'"),
            Token::QuoteDelimitedStringLiteral(ref s) => s.fmt(f),
            Token::NationalQuoteDelimitedStringLiteral(ref s) => write!(f, "N{s}"),
            Token::EscapedStringLiteral(ref s) => write!(f, "E'{s}'"),
            Token::UnicodeStringLiteral(ref s) => write!(f, "U&'{s}'"),
            Token::HexStringLiteral(ref s) => write!(f, "X'{s}'"),
            Token::SingleQuotedByteStringLiteral(ref s) => write!(f, "B'{s}'"),
            Token::TripleSingleQuotedByteStringLiteral(ref s) => write!(f, "B'''{s}'''"),
            Token::DoubleQuotedByteStringLiteral(ref s) => write!(f, "B\"{s}\""),
            Token::TripleDoubleQuotedByteStringLiteral(ref s) => write!(f, "B\"\"\"{s}\"\"\""),
            Token::SingleQuotedRawStringLiteral(ref s) => write!(f, "R'{s}'"),
            Token::DoubleQuotedRawStringLiteral(ref s) => write!(f, "R\"{s}\""),
            Token::TripleSingleQuotedRawStringLiteral(ref s) => write!(f, "R'''{s}'''"),
            Token::TripleDoubleQuotedRawStringLiteral(ref s) => write!(f, "R\"\"\"{s}\"\"\""),
            Token::Comma => f.write_str(","),
            Token::Whitespace(ws) => write!(f, "{ws}"),
            Token::DoubleEq => f.write_str("=="),
            Token::Spaceship => f.write_str("<=>"),
            Token::Eq => f.write_str("="),
            Token::Neq => f.write_str("<>"),
            Token::Lt => f.write_str("<"),
            Token::Gt => f.write_str(">"),
            Token::LtEq => f.write_str("<="),
            Token::GtEq => f.write_str(">="),
            Token::Plus => f.write_str("+"),
            Token::Minus => f.write_str("-"),
            Token::Mul => f.write_str("*"),
            Token::Div => f.write_str("/"),
            Token::DuckIntDiv => f.write_str("//"),
            Token::StringConcat => f.write_str("||"),
            Token::Mod => f.write_str("%"),
            Token::LParen => f.write_str("("),
            Token::RParen => f.write_str(")"),
            Token::Period => f.write_str("."),
            Token::Colon => f.write_str(":"),
            Token::DoubleColon => f.write_str("::"),
            Token::Assignment => f.write_str(":="),
            Token::SemiColon => f.write_str(";"),
            Token::Backslash => f.write_str("\\"),
            Token::LBracket => f.write_str("["),
            Token::RBracket => f.write_str("]"),
            Token::Ampersand => f.write_str("&"),
            Token::Caret => f.write_str("^"),
            Token::Pipe => f.write_str("|"),
            Token::LBrace => f.write_str("{"),
            Token::RBrace => f.write_str("}"),
            Token::RArrow => f.write_str("=>"),
            Token::Sharp => f.write_str("#"),
            Token::DoubleSharp => f.write_str("##"),
            Token::ExclamationMark => f.write_str("!"),
            Token::DoubleExclamationMark => f.write_str("!!"),
            Token::Tilde => f.write_str("~"),
            Token::TildeAsterisk => f.write_str("~*"),
            Token::ExclamationMarkTilde => f.write_str("!~"),
            Token::ExclamationMarkTildeAsterisk => f.write_str("!~*"),
            Token::DoubleTilde => f.write_str("~~"),
            Token::DoubleTildeAsterisk => f.write_str("~~*"),
            Token::ExclamationMarkDoubleTilde => f.write_str("!~~"),
            Token::ExclamationMarkDoubleTildeAsterisk => f.write_str("!~~*"),
            Token::AtSign => f.write_str("@"),
            Token::CaretAt => f.write_str("^@"),
            Token::ShiftLeft => f.write_str("<<"),
            Token::ShiftRight => f.write_str(">>"),
            Token::Overlap => f.write_str("&&"),
            Token::PGSquareRoot => f.write_str("|/"),
            Token::PGCubeRoot => f.write_str("||/"),
            Token::AtDashAt => f.write_str("@-@"),
            Token::QuestionMarkDash => f.write_str("?-"),
            Token::AmpersandLeftAngleBracket => f.write_str("&<"),
            Token::AmpersandRightAngleBracket => f.write_str("&>"),
            Token::AmpersandLeftAngleBracketVerticalBar => f.write_str("&<|"),
            Token::VerticalBarAmpersandRightAngleBracket => f.write_str("|&>"),
            Token::TwoWayArrow => f.write_str("<->"),
            Token::LeftAngleBracketCaret => f.write_str("<^"),
            Token::RightAngleBracketCaret => f.write_str(">^"),
            Token::QuestionMarkSharp => f.write_str("?#"),
            Token::QuestionMarkDashVerticalBar => f.write_str("?-|"),
            Token::QuestionMarkDoubleVerticalBar => f.write_str("?||"),
            Token::TildeEqual => f.write_str("~="),
            Token::ShiftLeftVerticalBar => f.write_str("<<|"),
            Token::VerticalBarShiftRight => f.write_str("|>>"),
            Token::Placeholder(ref s) => write!(f, "{s}"),
            Token::Arrow => write!(f, "->"),
            Token::LongArrow => write!(f, "->>"),
            Token::HashArrow => write!(f, "#>"),
            Token::HashLongArrow => write!(f, "#>>"),
            Token::AtArrow => write!(f, "@>"),
            Token::ArrowAt => write!(f, "<@"),
            Token::HashMinus => write!(f, "#-"),
            Token::AtQuestion => write!(f, "@?"),
            Token::AtAt => write!(f, "@@"),
            Token::Question => write!(f, "?"),
            Token::QuestionAnd => write!(f, "?&"),
            Token::QuestionPipe => write!(f, "?|"),
            Token::CustomBinaryOperator(s) => f.write_str(s),
        }
    }
}

impl Token {
    /// Create a `Token::Word` from an unquoted `keyword`.
    ///
    /// The lookup is case-insensitive; unknown values become `Keyword::NoKeyword`.
    pub fn make_keyword(keyword: &str) -> Self {
        Token::make_word(keyword, None)
    }

    /// Create a `Token::Word` from `word` with an optional `quote_style`.
    ///
    /// When `quote_style` is `None`, the parser attempts a case-insensitive keyword
    /// lookup and sets the `Word::keyword` accordingly.
    pub fn make_word(word: &str, quote_style: Option<char>) -> Self {
        Token::Word(Word {
            keyword: keyword_lookup(word, quote_style),
            value: word.to_string(),
            quote_style,
        })
    }

    /// Like [`Self::make_word`] but takes ownership of the word `String`,
    /// avoiding an extra allocation when the caller already has an owned value.
    fn make_word_owned(word: String, quote_style: Option<char>) -> Self {
        Token::Word(Word {
            keyword: keyword_lookup(&word, quote_style),
            value: word,
            quote_style,
        })
    }
}

/// Case-insensitive keyword lookup using binary search over [`ALL_KEYWORDS`].
fn keyword_lookup(word: &str, quote_style: Option<char>) -> Keyword {
    if quote_style.is_some() {
        return Keyword::NoKeyword;
    }
    ALL_KEYWORDS
        .binary_search_by(|probe| {
            let probe = probe.as_bytes();
            let word = word.as_bytes();
            for (p, w) in probe.iter().zip(word.iter()) {
                let cmp = p.cmp(&w.to_ascii_uppercase());
                if cmp != core::cmp::Ordering::Equal {
                    return cmp;
                }
            }
            probe.len().cmp(&word.len())
        })
        .map_or(Keyword::NoKeyword, |x| ALL_KEYWORDS_INDEX[x])
}

/// A keyword (like SELECT) or an optionally quoted SQL identifier
#[derive(Debug, Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "visitor", derive(Visit, VisitMut))]
pub struct Word {
    /// The value of the token, without the enclosing quotes. A doubled quote
    /// inside a quoted identifier is collapsed to one, unless the tokenizer
    /// was built with `unescape` off.
    ///
    /// TODO: decode the unicode escapes of a `U&"…"` identifier — `U&` is
    /// recognized only before a single quote, so `U&"d\0061ta"` tokenizes as
    /// the word `U`, an `&`, and the quoted identifier `d\0061ta`.
    pub value: String,
    /// An identifier can be "quoted" (&lt;delimited identifier> in ANSI parlance).
    /// The standard and most implementations allow using double quotes for this,
    /// but some implementations support other quoting styles as well (e.g. \[MS SQL])
    pub quote_style: Option<char>,
    /// If the word was not quoted and it matched one of the known keywords,
    /// this will have one of the values from dialect::keywords, otherwise empty
    pub keyword: Keyword,
}

impl fmt::Display for Word {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.quote_style {
            Some(s) if s == '"' || s == '[' || s == '`' => {
                write!(f, "{}{}{}", s, self.value, Word::matching_end_quote(s))
            }
            None => f.write_str(&self.value),
            _ => panic!("Unexpected quote_style!"),
        }
    }
}

impl Word {
    fn matching_end_quote(ch: char) -> char {
        match ch {
            '"' => '"', // ANSI and most dialects
            '[' => ']', // MS SQL
            '`' => '`', // MySQL
            _ => panic!("unexpected quoting style!"),
        }
    }
}

/// Represents whitespace in the input: spaces, newlines, tabs and comments.
#[derive(Debug, Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "visitor", derive(Visit, VisitMut))]
pub enum Whitespace {
    /// A single space character.
    Space,
    /// A newline character.
    Newline,
    /// A tab character.
    Tab,
    /// A single-line comment (`-- comment`).
    /// The `comment` field contains the text, and `prefix` contains the comment prefix.
    SingleLineComment {
        /// The content of the comment (without the prefix).
        comment: String,
        /// The prefix used for the comment. Always `--`: PostgreSQL introduces
        /// single-line comments only that way, and the dialects that spell them
        /// `#` or `//` are not part of this fork.
        prefix: String,
    },

    /// A multi-line comment (without the `/* ... */` delimiters).
    MultiLineComment(String),
}

impl fmt::Display for Whitespace {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Whitespace::Space => f.write_str(" "),
            Whitespace::Newline => f.write_str("\n"),
            Whitespace::Tab => f.write_str("\t"),
            Whitespace::SingleLineComment { prefix, comment } => write!(f, "{prefix}{comment}"),
            Whitespace::MultiLineComment(s) => write!(f, "/*{s}*/"),
        }
    }
}

/// Location in input string
///
/// # Create an "empty" (unknown) `Location`
/// ```
/// # use sqlparser::tokenizer::Location;
/// let location = Location::empty();
/// ```
///
/// # Create a `Location` from a line and column
/// ```
/// # use sqlparser::tokenizer::Location;
/// let location = Location::new(1, 1);
/// ```
///
/// # Create a `Location` from a pair
/// ```
/// # use sqlparser::tokenizer::Location;
/// let location = Location::from((1, 1));
/// ```
#[derive(Eq, PartialEq, Hash, Clone, Copy, Ord, PartialOrd)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "visitor", derive(Visit, VisitMut))]
pub struct Location {
    /// Line number, starting from 1.
    ///
    /// Note: Line 0 is used for empty spans
    pub line: u64,
    /// Line column, starting from 1.
    ///
    /// Note: Column 0 is used for empty spans
    pub column: u64,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.line == 0 {
            return Ok(());
        }
        write!(f, " at Line: {}, Column: {}", self.line, self.column)
    }
}

impl fmt::Debug for Location {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Location({},{})", self.line, self.column)
    }
}

impl Location {
    /// Return an "empty" / unknown location
    pub fn empty() -> Self {
        Self { line: 0, column: 0 }
    }

    /// Create a new `Location` for a given line and column
    pub fn new(line: u64, column: u64) -> Self {
        Self { line, column }
    }

    /// Create a new location for a given line and column
    ///
    /// Alias for [`Self::new`]
    pub fn of(line: u64, column: u64) -> Self {
        Self::new(line, column)
    }

    /// Combine self and `end` into a new `Span`
    pub fn span_to(self, end: Self) -> Span {
        Span { start: self, end }
    }
}

impl From<(u64, u64)> for Location {
    fn from((line, column): (u64, u64)) -> Self {
        Self { line, column }
    }
}

/// A span represents a linear portion of the input string (start, end)
///
/// See [Spanned](crate::ast::Spanned) for more information.
#[derive(Eq, PartialEq, Hash, Clone, PartialOrd, Ord, Copy)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "visitor", derive(Visit, VisitMut))]
pub struct Span {
    /// Start `Location` (inclusive).
    pub start: Location,
    /// End `Location` (inclusive).
    pub end: Location,
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Span({:?}..{:?})", self.start, self.end)
    }
}

impl Span {
    // An empty span (0, 0) -> (0, 0)
    // We need a const instance for pattern matching
    const EMPTY: Span = Self::empty();

    /// Create a new span from a start and end [`Location`]
    pub fn new(start: Location, end: Location) -> Span {
        Span { start, end }
    }

    /// Returns an empty span `(0, 0) -> (0, 0)`
    ///
    /// Empty spans represent no knowledge of source location
    /// See [Spanned](crate::ast::Spanned) for more information.
    pub const fn empty() -> Span {
        Span {
            start: Location { line: 0, column: 0 },
            end: Location { line: 0, column: 0 },
        }
    }

    /// Returns the smallest Span that contains both `self` and `other`
    /// If either span is [Span::empty], the other span is returned
    ///
    /// # Examples
    /// ```
    /// # use sqlparser::tokenizer::{Span, Location};
    /// // line 1, column1 -> line 2, column 5
    /// let span1 = Span::new(Location::new(1, 1), Location::new(2, 5));
    /// // line 2, column 3 -> line 3, column 7
    /// let span2 = Span::new(Location::new(2, 3), Location::new(3, 7));
    /// // Union of the two is the min/max of the two spans
    /// // line 1, column 1 -> line 3, column 7
    /// let union = span1.union(&span2);
    /// assert_eq!(union, Span::new(Location::new(1, 1), Location::new(3, 7)));
    /// ```
    pub fn union(&self, other: &Span) -> Span {
        // If either span is empty, return the other
        // this prevents propagating (0, 0) through the tree
        match (self, other) {
            (&Span::EMPTY, _) => *other,
            (_, &Span::EMPTY) => *self,
            _ => Span {
                start: cmp::min(self.start, other.start),
                end: cmp::max(self.end, other.end),
            },
        }
    }

    /// Same as [Span::union] for `Option<Span>`
    ///
    /// If `other` is `None`, `self` is returned
    pub fn union_opt(&self, other: &Option<Span>) -> Span {
        match other {
            Some(other) => self.union(other),
            None => *self,
        }
    }

    /// Return the [Span::union] of all spans in the iterator
    ///
    /// If the iterator is empty, an empty span is returned
    ///
    /// # Example
    /// ```
    /// # use sqlparser::tokenizer::{Span, Location};
    /// let spans = vec![
    ///     Span::new(Location::new(1, 1), Location::new(2, 5)),
    ///     Span::new(Location::new(2, 3), Location::new(3, 7)),
    ///     Span::new(Location::new(3, 1), Location::new(4, 2)),
    /// ];
    /// // line 1, column 1 -> line 4, column 2
    /// assert_eq!(
    ///   Span::union_iter(spans),
    ///   Span::new(Location::new(1, 1), Location::new(4, 2))
    /// );
    pub fn union_iter<I: IntoIterator<Item = Span>>(iter: I) -> Span {
        iter.into_iter()
            .reduce(|acc, item| acc.union(&item))
            .unwrap_or(Span::empty())
    }
}

/// Backwards compatibility struct for [`TokenWithSpan`]
#[deprecated(since = "0.53.0", note = "please use `TokenWithSpan` instead")]
pub type TokenWithLocation = TokenWithSpan;

/// A [Token] with [Span] attached to it
///
/// This is used to track the location of a token in the input string
///
/// # Examples
/// ```
/// # use sqlparser::tokenizer::{Location, Span, Token, TokenWithSpan};
/// // commas @ line 1, column 10
/// let tok1 = TokenWithSpan::new(
///   Token::Comma,
///   Span::new(Location::new(1, 10), Location::new(1, 11)),
/// );
/// assert_eq!(tok1, Token::Comma); // can compare the token
///
/// // commas @ line 2, column 20
/// let tok2 = TokenWithSpan::new(
///   Token::Comma,
///   Span::new(Location::new(2, 20), Location::new(2, 21)),
/// );
/// // same token but different locations are not equal
/// assert_ne!(tok1, tok2);
/// ```
#[derive(Debug, Clone, Hash, Ord, PartialOrd, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "visitor", derive(Visit, VisitMut))]
/// A `Token` together with its `Span` (location in the source).
pub struct TokenWithSpan {
    /// The token value.
    pub token: Token,
    /// The span covering the token in the input.
    pub span: Span,
}

impl TokenWithSpan {
    /// Create a new [`TokenWithSpan`] from a [`Token`] and a [`Span`]
    pub fn new(token: Token, span: Span) -> Self {
        Self { token, span }
    }

    /// Wrap a token with an empty span
    pub fn wrap(token: Token) -> Self {
        Self::new(token, Span::empty())
    }

    /// Wrap a token with a location from `start` to `end`
    pub fn at(token: Token, start: Location, end: Location) -> Self {
        Self::new(token, Span::new(start, end))
    }

    /// Return an EOF token with no location
    pub fn new_eof() -> Self {
        Self::wrap(Token::EOF)
    }
}

impl PartialEq<Token> for TokenWithSpan {
    fn eq(&self, other: &Token) -> bool {
        &self.token == other
    }
}

impl PartialEq<TokenWithSpan> for Token {
    fn eq(&self, other: &TokenWithSpan) -> bool {
        self == &other.token
    }
}

impl fmt::Display for TokenWithSpan {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.token.fmt(f)
    }
}

/// The SQLSTATE a tokenizer error carries when it has no more specific one.
/// Everything the lexer rejects is, by default, a syntax error.
pub const DEFAULT_TOKENIZER_SQLSTATE: &str = "42601";

/// An error reported by the tokenizer, with a human-readable `message` and a `location`.
///
/// Most lexer failures are generic syntax errors whose message only reads
/// correctly with the location appended. A few — the escape sequences inside
/// `E'…'` and `U&'…'` — reproduce a specific PostgreSQL diagnostic instead: they
/// set `sqlstate` (and sometimes a `hint`), and their message is PG's wording
/// verbatim, to be surfaced to the client as-is with the location reported
/// separately as a cursor position.
#[derive(Debug, PartialEq, Eq)]
pub struct TokenizerError {
    /// A descriptive error message.
    pub message: String,
    /// The `Location` where the error was detected.
    pub location: Location,
    /// 5-character SQLSTATE when this error reproduces a specific PostgreSQL
    /// diagnostic; `None` for a generic syntax error.
    pub sqlstate: Option<&'static str>,
    /// Optional `HINT:` line, for the diagnostics that PG pairs with one.
    pub hint: Option<String>,
}

impl TokenizerError {
    /// A generic syntax error: the message is only meaningful with the location
    /// appended, which is what [`fmt::Display`] does.
    pub fn new(message: impl Into<String>, location: Location) -> Self {
        Self {
            message: message.into(),
            location,
            sqlstate: None,
            hint: None,
        }
    }

    /// An error reproducing a specific PostgreSQL diagnostic.
    pub fn pg(
        sqlstate: &'static str,
        message: impl Into<String>,
        hint: Option<String>,
        location: Location,
    ) -> Self {
        Self {
            message: message.into(),
            location,
            sqlstate: Some(sqlstate),
            hint,
        }
    }
}

impl fmt::Display for TokenizerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.sqlstate.is_some() {
            return f.write_str(&self.message);
        }
        write!(f, "{}{}", self.message, self.location,)
    }
}

impl core::error::Error for TokenizerError {}

struct State<'a> {
    peekable: Peekable<Chars<'a>>,
    line: u64,
    col: u64,
}

impl State<'_> {
    /// return the next character and advance the stream
    pub fn next(&mut self) -> Option<char> {
        match self.peekable.next() {
            None => None,
            Some(s) => {
                if s == '\n' {
                    self.line += 1;
                    self.col = 1;
                } else {
                    self.col += 1;
                }
                Some(s)
            }
        }
    }

    /// return the next character but do not advance the stream
    pub fn peek(&mut self) -> Option<&char> {
        self.peekable.peek()
    }

    /// Return the current `Location` (line and column)
    pub fn location(&self) -> Location {
        Location {
            line: self.line,
            column: self.col,
        }
    }
}

/// Represents how many quote characters enclose a string literal.
#[derive(Copy, Clone)]
enum NumStringQuoteChars {
    /// e.g. `"abc"`, `'abc'`, `r'abc'`
    One,
    /// e.g. `"""abc"""`, `'''abc'''`, `r'''abc'''`
    Many(NonZeroU8),
}

/// Settings for tokenizing a quoted string literal.
struct TokenizeQuotedStringSettings {
    /// The character used to quote the string.
    quote_style: char,
    /// Represents how many quotes characters enclose the string literal.
    num_quote_chars: NumStringQuoteChars,
    /// The number of opening quotes left to consume, before parsing
    /// the remaining string literal.
    /// For example: given initial string `"""abc"""`. If the caller has
    /// already parsed the first quote for some reason, then this value
    /// is set to 1, flagging to look to consume only 2 leading quotes.
    num_opening_quotes_to_consume: u8,
    /// True if the string uses backslash escaping of special characters
    /// e.g `'abc\ndef\'ghi'
    backslash_escape: bool,
}

/// SQL Tokenizer
pub struct Tokenizer<'a> {
    dialect: &'a dyn Dialect,
    query: &'a str,
    /// If true (the default), the tokenizer will un-escape literal
    /// SQL strings See [`Tokenizer::with_unescape`] for more details.
    unescape: bool,
}

impl<'a> Tokenizer<'a> {
    /// Create a new SQL tokenizer for the specified SQL statement
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # use sqlparser::tokenizer::{Token, Whitespace, Tokenizer};
    /// # use sqlparser::dialect::GenericDialect;
    /// # let dialect = GenericDialect{};
    /// let query = r#"SELECT 'foo'"#;
    ///
    /// // Parsing the query
    /// let tokens = Tokenizer::new(&dialect, &query).tokenize()?;
    ///
    /// assert_eq!(tokens, vec![
    ///   Token::make_word("SELECT", None),
    ///   Token::Whitespace(Whitespace::Space),
    ///   Token::SingleQuotedString("foo".to_string()),
    /// ]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(dialect: &'a dyn Dialect, query: &'a str) -> Self {
        Self {
            dialect,
            query,
            unescape: true,
        }
    }

    /// Set unescape mode
    ///
    /// When true (default) the tokenizer unescapes literal values
    /// (for example, `""` in SQL is unescaped to the literal `"`).
    ///
    /// When false, the tokenizer provides the raw strings as provided
    /// in the query.  This can be helpful for programs that wish to
    /// recover the *exact* original query text without normalizing
    /// the escaping
    ///
    /// # Example
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # use sqlparser::tokenizer::{Token, Tokenizer};
    /// # use sqlparser::dialect::GenericDialect;
    /// # let dialect = GenericDialect{};
    /// let query = r#""Foo "" Bar""#;
    /// let unescaped = Token::make_word(r#"Foo " Bar"#, Some('"'));
    /// let original  = Token::make_word(r#"Foo "" Bar"#, Some('"'));
    ///
    /// // Parsing with unescaping (default)
    /// let tokens = Tokenizer::new(&dialect, &query).tokenize()?;
    /// assert_eq!(tokens, vec![unescaped]);
    ///
    /// // Parsing with unescape = false
    /// let tokens = Tokenizer::new(&dialect, &query)
    ///    .with_unescape(false)
    ///    .tokenize()?;
    /// assert_eq!(tokens, vec![original]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_unescape(mut self, unescape: bool) -> Self {
        self.unescape = unescape;
        self
    }

    /// Tokenize the statement and produce a vector of tokens
    pub fn tokenize(&mut self) -> Result<Vec<Token>, TokenizerError> {
        let twl = self.tokenize_with_location()?;
        Ok(twl.into_iter().map(|t| t.token).collect())
    }

    /// Tokenize the statement and produce a vector of tokens with location information
    pub fn tokenize_with_location(&mut self) -> Result<Vec<TokenWithSpan>, TokenizerError> {
        let mut tokens: Vec<TokenWithSpan> = vec![];
        self.tokenize_with_location_into_buf(&mut tokens)
            .map(|_| tokens)
    }

    /// Tokenize the statement and append tokens with location information into the provided buffer.
    /// If an error is thrown, the buffer will contain all tokens that were successfully parsed before the error.
    pub fn tokenize_with_location_into_buf(
        &mut self,
        buf: &mut Vec<TokenWithSpan>,
    ) -> Result<(), TokenizerError> {
        self.tokenize_with_location_into_buf_with_mapper(buf, |token| token)
    }

    /// Tokenize the statement and produce a vector of tokens, mapping each token
    /// with provided `mapper`
    pub fn tokenize_with_location_into_buf_with_mapper(
        &mut self,
        buf: &mut Vec<TokenWithSpan>,
        mut mapper: impl FnMut(TokenWithSpan) -> TokenWithSpan,
    ) -> Result<(), TokenizerError> {
        let mut state = State {
            peekable: self.query.chars().peekable(),
            line: 1,
            col: 1,
        };

        let mut location = state.location();
        while let Some(token) = self.next_token(&mut state, buf.last().map(|t| &t.token))? {
            let span = location.span_to(state.location());

            // Check if this is a multiline comment hint that should be expanded
            match &token {
                Token::Whitespace(Whitespace::MultiLineComment(comment))
                    if self.dialect.supports_multiline_comment_hints()
                        && comment.starts_with('!') =>
                {
                    // Re-tokenize the hints and add them to the buffer
                    self.tokenize_comment_hints(comment, span, buf, &mut mapper)?;
                }
                _ => {
                    buf.push(mapper(TokenWithSpan { token, span }));
                }
            }

            location = state.location();
        }
        Ok(())
    }

    /// Re-tokenize optimizer hints from a multiline comment and add them to the buffer.
    /// For example, `/*!50110 KEY_BLOCK_SIZE = 1024*/` becomes tokens for `KEY_BLOCK_SIZE = 1024`
    fn tokenize_comment_hints(
        &self,
        comment: &str,
        span: Span,
        buf: &mut Vec<TokenWithSpan>,
        mut mapper: impl FnMut(TokenWithSpan) -> TokenWithSpan,
    ) -> Result<(), TokenizerError> {
        // Strip the leading '!' and any version digits (e.g., "50110")
        let hint_content = comment
            .strip_prefix('!')
            .unwrap_or(comment)
            .trim_start_matches(|c: char| c.is_ascii_digit());

        // If there's no content after stripping, nothing to tokenize
        if hint_content.is_empty() {
            return Ok(());
        }

        // Create a new tokenizer for the hint content
        let inner = Tokenizer::new(self.dialect, hint_content).with_unescape(self.unescape);

        // Create a state for tracking position within the hint
        let mut state = State {
            peekable: hint_content.chars().peekable(),
            line: span.start.line,
            col: span.start.column,
        };

        // Tokenize the hint content and add tokens to the buffer
        let mut location = state.location();
        while let Some(token) = inner.next_token(&mut state, buf.last().map(|t| &t.token))? {
            let token_span = location.span_to(state.location());
            buf.push(mapper(TokenWithSpan {
                token,
                span: token_span,
            }));
            location = state.location();
        }

        Ok(())
    }

    // Tokenize the identifier or keywords in `ch`
    fn tokenize_identifier_or_keyword(
        &self,
        ch: impl IntoIterator<Item = char>,
        chars: &mut State,
    ) -> Result<Option<Token>, TokenizerError> {
        chars.next(); // consume the first char
        let ch: String = ch.into_iter().collect();
        let word = self.tokenize_word(ch, chars);

        // TODO: implement parsing of exponent here
        if word.chars().all(|x| x.is_ascii_digit() || x == '.') {
            let mut inner_state = State {
                peekable: word.chars().peekable(),
                line: 0,
                col: 0,
            };
            let mut s = peeking_take_while(&mut inner_state, |ch| matches!(ch, '0'..='9' | '.'));
            let s2 = peeking_take_while(chars, |ch| matches!(ch, '0'..='9' | '.'));
            s += s2.as_str();
            return Ok(Some(Token::Number(s, false)));
        }

        Ok(Some(Token::make_word_owned(word, None)))
    }

    /// Get the next token or return None
    fn next_token(
        &self,
        chars: &mut State,
        prev_token: Option<&Token>,
    ) -> Result<Option<Token>, TokenizerError> {
        match chars.peek() {
            Some(&ch) => match ch {
                ' ' => self.consume_and_return(chars, Token::Whitespace(Whitespace::Space)),
                '\t' => self.consume_and_return(chars, Token::Whitespace(Whitespace::Tab)),
                '\n' => self.consume_and_return(chars, Token::Whitespace(Whitespace::Newline)),
                '\r' => {
                    // Emit a single Whitespace::Newline token for \r and \r\n
                    chars.next();
                    if let Some('\n') = chars.peek() {
                        chars.next();
                    }
                    Ok(Some(Token::Whitespace(Whitespace::Newline)))
                }
                // BigQuery and MySQL use b or B for byte string literal, Postgres for bit strings
                b @ 'B' | b @ 'b' if dialect_of!(self is PostgreSqlDialect | GenericDialect) => {
                    chars.next(); // consume
                    match chars.peek() {
                        Some('\'') => {
                            if self.dialect.supports_triple_quoted_string() {
                                return self
                                    .tokenize_single_or_triple_quoted_string::<fn(String) -> Token>(
                                        chars,
                                        '\'',
                                        false,
                                        Token::SingleQuotedByteStringLiteral,
                                        Token::TripleSingleQuotedByteStringLiteral,
                                    );
                            }
                            let s = self.tokenize_single_quoted_string(chars, '\'', false)?;
                            Ok(Some(Token::SingleQuotedByteStringLiteral(s)))
                        }
                        Some('\"') => {
                            if self.dialect.supports_triple_quoted_string() {
                                return self
                                    .tokenize_single_or_triple_quoted_string::<fn(String) -> Token>(
                                        chars,
                                        '"',
                                        false,
                                        Token::DoubleQuotedByteStringLiteral,
                                        Token::TripleDoubleQuotedByteStringLiteral,
                                    );
                            }
                            let s = self.tokenize_single_quoted_string(chars, '\"', false)?;
                            Ok(Some(Token::DoubleQuotedByteStringLiteral(s)))
                        }
                        _ => {
                            // regular identifier starting with an "b" or "B"
                            let s = self.tokenize_word(b, chars);
                            Ok(Some(Token::make_word_owned(s, None)))
                        }
                    }
                }
                // BigQuery uses r or R for raw string literal
                b @ 'R' | b @ 'r' if dialect_of!(self is GenericDialect) => {
                    chars.next(); // consume
                    match chars.peek() {
                        Some('\'') => self
                            .tokenize_single_or_triple_quoted_string::<fn(String) -> Token>(
                                chars,
                                '\'',
                                false,
                                Token::SingleQuotedRawStringLiteral,
                                Token::TripleSingleQuotedRawStringLiteral,
                            ),
                        Some('\"') => self
                            .tokenize_single_or_triple_quoted_string::<fn(String) -> Token>(
                                chars,
                                '"',
                                false,
                                Token::DoubleQuotedRawStringLiteral,
                                Token::TripleDoubleQuotedRawStringLiteral,
                            ),
                        _ => {
                            // regular identifier starting with an "r" or "R"
                            let s = self.tokenize_word(b, chars);
                            Ok(Some(Token::make_word_owned(s, None)))
                        }
                    }
                }
                // Redshift uses lower case n for national string literal
                n @ 'N' | n @ 'n' => {
                    chars.next(); // consume, to check the next char
                    match chars.peek() {
                        Some('\'') => {
                            // N'...' - a <national character string literal>
                            let backslash_escape =
                                self.dialect.supports_string_literal_backslash_escape();
                            let s =
                                self.tokenize_single_quoted_string(chars, '\'', backslash_escape)?;
                            Ok(Some(Token::NationalStringLiteral(s)))
                        }
                        Some(&q @ 'q') | Some(&q @ 'Q')
                            if self.dialect.supports_quote_delimited_string() =>
                        {
                            chars.next(); // consume and check the next char
                            if let Some('\'') = chars.peek() {
                                self.tokenize_quote_delimited_string(chars, &[n, q])
                                    .map(|s| Some(Token::NationalQuoteDelimitedStringLiteral(s)))
                            } else {
                                let s = self.tokenize_word(String::from_iter([n, q]), chars);
                                Ok(Some(Token::make_word_owned(s, None)))
                            }
                        }
                        _ => {
                            // regular identifier starting with an "N"
                            let s = self.tokenize_word(n, chars);
                            Ok(Some(Token::make_word_owned(s, None)))
                        }
                    }
                }
                q @ 'Q' | q @ 'q' if self.dialect.supports_quote_delimited_string() => {
                    chars.next(); // consume and check the next char
                    if let Some('\'') = chars.peek() {
                        self.tokenize_quote_delimited_string(chars, &[q])
                            .map(|s| Some(Token::QuoteDelimitedStringLiteral(s)))
                    } else {
                        let s = self.tokenize_word(q, chars);
                        Ok(Some(Token::make_word_owned(s, None)))
                    }
                }
                // PostgreSQL accepts "escape" string constants, which are an extension to the SQL standard.
                x @ 'e' | x @ 'E' if self.dialect.supports_string_escape_constant() => {
                    let starting_loc = chars.location();
                    chars.next(); // consume, to check the next char
                    match chars.peek() {
                        Some('\'') => {
                            let s =
                                self.tokenize_escaped_single_quoted_string(starting_loc, chars)?;
                            Ok(Some(Token::EscapedStringLiteral(s)))
                        }
                        _ => {
                            // regular identifier starting with an "E" or "e"
                            let s = self.tokenize_word(x, chars);
                            Ok(Some(Token::make_word_owned(s, None)))
                        }
                    }
                }
                // Unicode string literals like U&'first \000A second' are supported in some dialects, including PostgreSQL
                x @ 'u' | x @ 'U' if self.dialect.supports_unicode_string_literal() => {
                    chars.next(); // consume, to check the next char
                    if chars.peek() == Some(&'&') {
                        // we cannot advance the iterator here, as we need to consume the '&' later if the 'u' was an identifier
                        let mut chars_clone = chars.peekable.clone();
                        chars_clone.next(); // consume the '&' in the clone
                        if chars_clone.peek() == Some(&'\'') {
                            chars.next(); // consume the '&' in the original iterator
                            let s = unescape_unicode_single_quoted_string(chars)?;
                            return Ok(Some(Token::UnicodeStringLiteral(s)));
                        }
                    }
                    // regular identifier starting with an "U" or "u"
                    let s = self.tokenize_word(x, chars);
                    Ok(Some(Token::make_word_owned(s, None)))
                }
                // The spec only allows an uppercase 'X' to introduce a hex
                // string, but PostgreSQL, at least, allows a lowercase 'x' too.
                x @ 'x' | x @ 'X' => {
                    chars.next(); // consume, to check the next char
                    match chars.peek() {
                        Some('\'') => {
                            // X'...' - a <binary string literal>
                            let s = self.tokenize_single_quoted_string(chars, '\'', true)?;
                            Ok(Some(Token::HexStringLiteral(s)))
                        }
                        _ => {
                            // regular identifier starting with an "X"
                            let s = self.tokenize_word(x, chars);
                            Ok(Some(Token::make_word_owned(s, None)))
                        }
                    }
                }
                // single quoted string
                '\'' => {
                    if self.dialect.supports_triple_quoted_string() {
                        return self
                            .tokenize_single_or_triple_quoted_string::<fn(String) -> Token>(
                                chars,
                                '\'',
                                self.dialect.supports_string_literal_backslash_escape(),
                                Token::SingleQuotedString,
                                Token::TripleSingleQuotedString,
                            );
                    }
                    let s = self.tokenize_single_quoted_string(
                        chars,
                        '\'',
                        self.dialect.supports_string_literal_backslash_escape(),
                    )?;

                    Ok(Some(Token::SingleQuotedString(s)))
                }
                // double quoted string
                '\"' if !self.dialect.is_delimited_identifier_start(ch)
                    && !self.dialect.is_identifier_start(ch) =>
                {
                    if self.dialect.supports_triple_quoted_string() {
                        return self
                            .tokenize_single_or_triple_quoted_string::<fn(String) -> Token>(
                                chars,
                                '"',
                                self.dialect.supports_string_literal_backslash_escape(),
                                Token::DoubleQuotedString,
                                Token::TripleDoubleQuotedString,
                            );
                    }
                    let s = self.tokenize_single_quoted_string(
                        chars,
                        '"',
                        self.dialect.supports_string_literal_backslash_escape(),
                    )?;

                    Ok(Some(Token::DoubleQuotedString(s)))
                }
                // delimited (quoted) identifier
                quote_start if self.dialect.is_delimited_identifier_start(ch) => {
                    let word = self.tokenize_quoted_identifier(quote_start, chars)?;
                    Ok(Some(Token::make_word_owned(word, Some(quote_start))))
                }
                // Potentially nested delimited (quoted) identifier
                quote_start
                    if self
                        .dialect
                        .is_nested_delimited_identifier_start(quote_start)
                        && self
                            .dialect
                            .peek_nested_delimited_identifier_quotes(chars.peekable.clone())
                            .is_some() =>
                {
                    let Some((quote_start, nested_quote_start)) = self
                        .dialect
                        .peek_nested_delimited_identifier_quotes(chars.peekable.clone())
                    else {
                        return self.tokenizer_error(
                            chars.location(),
                            format!("Expected nested delimiter '{quote_start}' before EOF."),
                        );
                    };

                    let Some(nested_quote_start) = nested_quote_start else {
                        let word = self.tokenize_quoted_identifier(quote_start, chars)?;
                        return Ok(Some(Token::make_word_owned(word, Some(quote_start))));
                    };

                    let mut word = vec![];
                    let quote_end = Word::matching_end_quote(quote_start);
                    let nested_quote_end = Word::matching_end_quote(nested_quote_start);
                    let error_loc = chars.location();

                    chars.next(); // skip the first delimiter
                    peeking_take_while(chars, |ch| ch.is_whitespace());
                    if chars.peek() != Some(&nested_quote_start) {
                        return self.tokenizer_error(
                            error_loc,
                            format!("Expected nested delimiter '{nested_quote_start}' before EOF."),
                        );
                    }
                    word.push(nested_quote_start.into());
                    word.push(self.tokenize_quoted_identifier(nested_quote_end, chars)?);
                    word.push(nested_quote_end.into());
                    peeking_take_while(chars, |ch| ch.is_whitespace());
                    if chars.peek() != Some(&quote_end) {
                        return self.tokenizer_error(
                            error_loc,
                            format!("Expected close delimiter '{quote_end}' before EOF."),
                        );
                    }
                    chars.next(); // skip close delimiter

                    Ok(Some(Token::make_word_owned(
                        word.concat(),
                        Some(quote_start),
                    )))
                }
                // numbers and period
                '0'..='9' | '.' => {
                    // Snapshotted before anything is consumed: PostgreSQL's
                    // caret for a malformed numeric literal sits on the
                    // literal's first character, not on the offending one.
                    let literal_start = chars.location();
                    // special case where if ._ is encountered after a word then that word
                    // is a table and the _ is the start of the col name.
                    // if the prev token is not a word, then this is not a valid sql
                    // word or number.
                    if ch == '.' && chars.peekable.clone().nth(1) == Some('_') {
                        if let Some(Token::Word(_)) = prev_token {
                            chars.next();
                            return Ok(Some(Token::Period));
                        }

                        return self.tokenizer_error(
                            chars.location(),
                            "Unexpected character '_'".to_string(),
                        );
                    }

                    // Some dialects support underscore as number separator.
                    // There can only be one at a time and it must be followed by
                    // another digit *of the run's own base* — `radix` is what
                    // makes `1_e5` and `0b1_2` junk rather than 100000 and 3.
                    let is_number_separator = |ch: char, next_char: Option<char>, radix: u32| {
                        self.dialect.supports_numeric_literal_underscores()
                            && ch == '_'
                            && next_char.is_some_and(|next_ch| next_ch.is_digit(radix))
                    };

                    let mut s = peeking_next_take_while(chars, |ch, next_ch| {
                        ch.is_ascii_digit() || is_number_separator(ch, next_ch, 10)
                    });

                    // PostgreSQL 16 and later spell integer constants in hex,
                    // octal and binary as `0x1F` / `0o17` / `0b11`. These are
                    // *numbers*, not the `X'…'` bit string the `0x` case below
                    // yields for other dialects, so the prefix stays in the
                    // token text and the binder converts it through the same
                    // acceptor as the `int4 '0x1F'` input function.
                    if s == "0"
                        && self.dialect.supports_non_decimal_numeric_literals()
                        && chars
                            .peek()
                            .is_some_and(|c| matches!(c, 'x' | 'X' | 'o' | 'O' | 'b' | 'B'))
                    {
                        let prefix = chars.next().expect("peeked above");
                        let (radix, base_name) = match prefix.to_ascii_lowercase() {
                            'x' => (16, "hexadecimal"),
                            'o' => (8, "octal"),
                            _ => (2, "binary"),
                        };
                        let digits = peeking_next_take_while(chars, |ch, next_ch| {
                            ch.is_digit(radix) || is_number_separator(ch, next_ch, radix)
                        });
                        s.push(prefix);
                        s.push_str(&digits);
                        // PG rejects both an empty digit run and a digit run cut
                        // short by an identifier character, rather than handing
                        // back two tokens. Consuming the tail first makes the
                        // message name the whole offending literal, as PG's does.
                        if digits.chars().all(|c| c == '_') {
                            s += &peeking_take_while(chars, |ch| {
                                self.dialect.is_identifier_part(ch)
                            });
                            return Err(TokenizerError::pg(
                                DEFAULT_TOKENIZER_SQLSTATE,
                                format!("invalid {base_name} integer at or near \"{s}\""),
                                None,
                                literal_start,
                            ));
                        }
                        if chars
                            .peek()
                            .is_some_and(|&c| self.dialect.is_identifier_part(c))
                        {
                            s += &peeking_take_while(chars, |ch| {
                                self.dialect.is_identifier_part(ch)
                            });
                            return Err(TokenizerError::pg(
                                DEFAULT_TOKENIZER_SQLSTATE,
                                format!("trailing junk after numeric literal at or near \"{s}\""),
                                None,
                                literal_start,
                            ));
                        }
                        return Ok(Some(Token::Number(s, false)));
                    }

                    // match binary literal that starts with 0x
                    if s == "0" && chars.peek() == Some(&'x') {
                        chars.next();
                        let s2 = peeking_next_take_while(chars, |ch, next_ch| {
                            ch.is_ascii_hexdigit() || is_number_separator(ch, next_ch, 16)
                        });
                        return Ok(Some(Token::HexStringLiteral(s2)));
                    }

                    // match one period
                    if let Some('.') = chars.peek() {
                        s.push('.');
                        chars.next();
                    }

                    // If the dialect supports identifiers that start with a numeric prefix
                    // and we have now consumed a dot, check if the previous token was a Word.
                    // If so, what follows is definitely not part of a decimal number and
                    // we should yield the dot as a dedicated token so compound identifiers
                    // starting with digits can be parsed correctly.
                    if s == "." && self.dialect.supports_numeric_prefix() {
                        if let Some(Token::Word(_)) = prev_token {
                            return Ok(Some(Token::Period));
                        }
                    }

                    // Consume fractional digits. A separator has to sit
                    // *between* digits, so it may not lead the fraction: PG
                    // reads `1_000._5` as junk, not as 1000.5.
                    let mut fraction_started = false;
                    s += &peeking_next_take_while(chars, |ch, next_ch| {
                        let ok = ch.is_ascii_digit()
                            || (fraction_started && is_number_separator(ch, next_ch, 10));
                        fraction_started |= ok;
                        ok
                    });

                    // No fraction -> Token::Period
                    if s == "." {
                        return Ok(Some(Token::Period));
                    }

                    // Parse exponent as number
                    let mut exponent_part = String::new();
                    if chars.peek() == Some(&'e') || chars.peek() == Some(&'E') {
                        let mut char_clone = chars.peekable.clone();
                        if let Some(sign) = char_clone.next() {
                            exponent_part.push(sign);
                        }

                        // Optional sign
                        match char_clone.peek() {
                            Some(&c) if matches!(c, '+' | '-') => {
                                exponent_part.push(c);
                                char_clone.next();
                            }
                            _ => (),
                        }

                        match char_clone.peek() {
                            // Definitely an exponent, get original iterator up to speed and use it
                            Some(&c) if c.is_ascii_digit() => {
                                for _ in 0..exponent_part.len() {
                                    chars.next();
                                }
                                let mut exponent_started = false;
                                exponent_part += &peeking_next_take_while(chars, |ch, next_ch| {
                                    let ok = ch.is_ascii_digit()
                                        || (exponent_started
                                            && is_number_separator(ch, next_ch, 10));
                                    exponent_started |= ok;
                                    ok
                                });
                                s += exponent_part.as_str();
                            }
                            // Not an exponent, discard the work done
                            _ => (),
                        }
                    }

                    // If the dialect supports identifiers that start with a numeric prefix,
                    // we need to check if the value is in fact an identifier and must thus
                    // be tokenized as a word.
                    if self.dialect.supports_numeric_prefix() {
                        if exponent_part.is_empty() {
                            // If it is not a number with an exponent, it may be
                            // an identifier starting with digits.
                            let word =
                                peeking_take_while(chars, |ch| self.dialect.is_identifier_part(ch));

                            if !word.is_empty() {
                                s += word.as_str();
                                return Ok(Some(Token::make_word_owned(s, None)));
                            }
                        } else if prev_token == Some(&Token::Period) {
                            // If the previous token was a period, thus not belonging to a number,
                            // the value we have is part of an identifier.
                            return Ok(Some(Token::make_word_owned(s, None)));
                        }
                    }

                    // A number that runs straight into an identifier character
                    // is one bad literal, not a number beside a word — PG reads
                    // `123abc`, `1x` and `100_` all that way. Gated on the same
                    // flag as the radix prefixes because a dialect without them
                    // (or with `supports_numeric_prefix`, handled above) still
                    // wants the older two-token reading.
                    if self.dialect.supports_non_decimal_numeric_literals()
                        && chars
                            .peek()
                            .is_some_and(|&c| self.dialect.is_identifier_part(c))
                    {
                        s += &peeking_take_while(chars, |ch| self.dialect.is_identifier_part(ch));
                        return Err(TokenizerError::pg(
                            DEFAULT_TOKENIZER_SQLSTATE,
                            format!("trailing junk after numeric literal at or near \"{s}\""),
                            None,
                            literal_start,
                        ));
                    }

                    let long = if chars.peek() == Some(&'L') {
                        chars.next();
                        true
                    } else {
                        false
                    };
                    Ok(Some(Token::Number(s, long)))
                }
                // punctuation
                '(' => self.consume_and_return(chars, Token::LParen),
                ')' => self.consume_and_return(chars, Token::RParen),
                ',' => self.consume_and_return(chars, Token::Comma),
                // operators
                '-' => {
                    chars.next(); // consume the '-'

                    match chars.peek() {
                        Some('-') => {
                            let mut is_comment = true;
                            if self.dialect.requires_single_line_comment_whitespace() {
                                is_comment = chars
                                    .peekable
                                    .clone()
                                    .nth(1)
                                    .is_some_and(char::is_whitespace);
                            }

                            if is_comment {
                                chars.next(); // consume second '-'
                                let comment = self.tokenize_single_line_comment(chars);
                                return Ok(Some(Token::Whitespace(
                                    Whitespace::SingleLineComment {
                                        prefix: "--".to_owned(),
                                        comment,
                                    },
                                )));
                            }

                            self.start_binop(chars, "-", Token::Minus)
                        }
                        Some('>') => {
                            chars.next();
                            match chars.peek() {
                                Some('>') => self.consume_for_binop(chars, "->>", Token::LongArrow),
                                _ => self.start_binop(chars, "->", Token::Arrow),
                            }
                        }
                        // a regular '-' operator
                        _ => self.start_binop(chars, "-", Token::Minus),
                    }
                }
                '/' => {
                    chars.next(); // consume the '/'
                    match chars.peek() {
                        Some('*') => {
                            chars.next(); // consume the '*', starting a multi-line comment
                            self.tokenize_multiline_comment(chars)
                        }
                        Some('/') if dialect_of!(self is GenericDialect) => {
                            self.consume_and_return(chars, Token::DuckIntDiv)
                        }
                        // a regular '/' operator
                        _ => Ok(Some(Token::Div)),
                    }
                }
                '+' => self.consume_and_return(chars, Token::Plus),
                '*' => self.consume_and_return(chars, Token::Mul),
                '%' => {
                    chars.next(); // advance past '%'
                    match chars.peek() {
                        Some(s) if s.is_whitespace() => Ok(Some(Token::Mod)),
                        Some(sch) if self.dialect.is_identifier_start('%') => {
                            self.tokenize_identifier_or_keyword([ch, *sch], chars)
                        }
                        _ => self.start_binop(chars, "%", Token::Mod),
                    }
                }
                '|' => {
                    chars.next(); // consume the '|'
                    match chars.peek() {
                        Some('/') => self.consume_for_binop(chars, "|/", Token::PGSquareRoot),
                        Some('|') => {
                            chars.next(); // consume the second '|'
                            match chars.peek() {
                                Some('/') => {
                                    self.consume_for_binop(chars, "||/", Token::PGCubeRoot)
                                }
                                _ => self.start_binop(chars, "||", Token::StringConcat),
                            }
                        }
                        Some('&') if self.dialect.supports_geometric_types() => {
                            chars.next(); // consume
                            match chars.peek() {
                                Some('>') => self.consume_for_binop(
                                    chars,
                                    "|&>",
                                    Token::VerticalBarAmpersandRightAngleBracket,
                                ),
                                _ => self.start_binop_opt(chars, "|&", None),
                            }
                        }
                        Some('>') if self.dialect.supports_geometric_types() => {
                            chars.next(); // consume
                            match chars.peek() {
                                Some('>') => self.consume_for_binop(
                                    chars,
                                    "|>>",
                                    Token::VerticalBarShiftRight,
                                ),
                                _ => self.start_binop_opt(chars, "|>", None),
                            }
                        }
                        // Bitshift '|' operator
                        _ => self.start_binop(chars, "|", Token::Pipe),
                    }
                }
                '=' => {
                    chars.next(); // consume
                    match chars.peek() {
                        Some('>') => self.consume_and_return(chars, Token::RArrow),
                        Some('=') => self.consume_and_return(chars, Token::DoubleEq),
                        _ => Ok(Some(Token::Eq)),
                    }
                }
                '!' => {
                    chars.next(); // consume
                    match chars.peek() {
                        Some('=') => self.consume_and_return(chars, Token::Neq),
                        Some('!') => self.consume_and_return(chars, Token::DoubleExclamationMark),
                        Some('~') => {
                            chars.next();
                            match chars.peek() {
                                Some('*') => self
                                    .consume_and_return(chars, Token::ExclamationMarkTildeAsterisk),
                                Some('~') => {
                                    chars.next();
                                    match chars.peek() {
                                        Some('*') => self.consume_and_return(
                                            chars,
                                            Token::ExclamationMarkDoubleTildeAsterisk,
                                        ),
                                        _ => Ok(Some(Token::ExclamationMarkDoubleTilde)),
                                    }
                                }
                                _ => Ok(Some(Token::ExclamationMarkTilde)),
                            }
                        }
                        _ => Ok(Some(Token::ExclamationMark)),
                    }
                }
                '<' => {
                    chars.next(); // consume
                    match chars.peek() {
                        Some('=') => {
                            chars.next();
                            match chars.peek() {
                                Some('>') => self.consume_for_binop(chars, "<=>", Token::Spaceship),
                                // `<=+` and `<=-` are not valid combined operators; treat `<=` as
                                // the operator and leave `+`/`-` to be tokenized separately.
                                Some('+') | Some('-') => Ok(Some(Token::LtEq)),
                                _ => self.start_binop(chars, "<=", Token::LtEq),
                            }
                        }
                        Some('|') if self.dialect.supports_geometric_types() => {
                            self.consume_for_binop(chars, "<<|", Token::ShiftLeftVerticalBar)
                        }
                        Some('>') => self.consume_for_binop(chars, "<>", Token::Neq),
                        Some('<') if self.dialect.supports_geometric_types() => {
                            chars.next(); // consume
                            match chars.peek() {
                                Some('|') => self.consume_for_binop(
                                    chars,
                                    "<<|",
                                    Token::ShiftLeftVerticalBar,
                                ),
                                _ => self.start_binop(chars, "<<", Token::ShiftLeft),
                            }
                        }
                        Some('<') => self.consume_for_binop(chars, "<<", Token::ShiftLeft),
                        // `<+` is not a valid combined operator; treat `<` as the operator
                        // and leave `+` to be tokenized separately.
                        Some('+') => Ok(Some(Token::Lt)),
                        Some('-') if self.dialect.supports_geometric_types() => {
                            if chars.peekable.clone().nth(1) == Some('>') {
                                chars.next(); // consume `-`
                                self.consume_for_binop(chars, "<->", Token::TwoWayArrow)
                            } else {
                                Ok(Some(Token::Lt))
                            }
                        }
                        Some('^') if self.dialect.supports_geometric_types() => {
                            self.consume_for_binop(chars, "<^", Token::LeftAngleBracketCaret)
                        }
                        Some('@') => self.consume_for_binop(chars, "<@", Token::ArrowAt),
                        _ => self.start_binop(chars, "<", Token::Lt),
                    }
                }
                '>' => {
                    chars.next(); // consume
                    match chars.peek() {
                        Some('=') => self.consume_for_binop(chars, ">=", Token::GtEq),
                        Some('>') => self.consume_for_binop(chars, ">>", Token::ShiftRight),
                        Some('^') if self.dialect.supports_geometric_types() => {
                            self.consume_for_binop(chars, ">^", Token::RightAngleBracketCaret)
                        }
                        _ => self.start_binop(chars, ">", Token::Gt),
                    }
                }
                ':' => {
                    chars.next();
                    match chars.peek() {
                        Some(':') => self.consume_and_return(chars, Token::DoubleColon),
                        Some('=') => self.consume_and_return(chars, Token::Assignment),
                        _ => Ok(Some(Token::Colon)),
                    }
                }
                ';' => self.consume_and_return(chars, Token::SemiColon),
                '\\' => self.consume_and_return(chars, Token::Backslash),
                '[' => self.consume_and_return(chars, Token::LBracket),
                ']' => self.consume_and_return(chars, Token::RBracket),
                '&' => {
                    chars.next(); // consume the '&'
                    match chars.peek() {
                        Some('>') if self.dialect.supports_geometric_types() => {
                            chars.next();
                            self.consume_and_return(chars, Token::AmpersandRightAngleBracket)
                        }
                        Some('<') if self.dialect.supports_geometric_types() => {
                            chars.next(); // consume
                            match chars.peek() {
                                Some('|') => self.consume_and_return(
                                    chars,
                                    Token::AmpersandLeftAngleBracketVerticalBar,
                                ),
                                _ => {
                                    self.start_binop(chars, "&<", Token::AmpersandLeftAngleBracket)
                                }
                            }
                        }
                        Some('&') => {
                            chars.next(); // consume the second '&'
                            self.start_binop(chars, "&&", Token::Overlap)
                        }
                        // Bitshift '&' operator
                        _ => self.start_binop(chars, "&", Token::Ampersand),
                    }
                }
                '^' => {
                    chars.next(); // consume the '^'
                    match chars.peek() {
                        Some('@') => self.consume_and_return(chars, Token::CaretAt),
                        _ => Ok(Some(Token::Caret)),
                    }
                }
                '{' => self.consume_and_return(chars, Token::LBrace),
                '}' => self.consume_and_return(chars, Token::RBrace),
                '~' => {
                    chars.next(); // consume
                    match chars.peek() {
                        Some('*') => self.consume_for_binop(chars, "~*", Token::TildeAsterisk),
                        Some('=') if self.dialect.supports_geometric_types() => {
                            self.consume_for_binop(chars, "~=", Token::TildeEqual)
                        }
                        Some('~') => {
                            chars.next();
                            match chars.peek() {
                                Some('*') => {
                                    self.consume_for_binop(chars, "~~*", Token::DoubleTildeAsterisk)
                                }
                                _ => self.start_binop(chars, "~~", Token::DoubleTilde),
                            }
                        }
                        _ => self.start_binop(chars, "~", Token::Tilde),
                    }
                }
                '#' => {
                    chars.next();
                    match chars.peek() {
                        Some('-') => self.consume_for_binop(chars, "#-", Token::HashMinus),
                        Some('>') => {
                            chars.next();
                            match chars.peek() {
                                Some('>') => {
                                    self.consume_for_binop(chars, "#>>", Token::HashLongArrow)
                                }
                                _ => self.start_binop(chars, "#>", Token::HashArrow),
                            }
                        }
                        Some(' ') => Ok(Some(Token::Sharp)),
                        Some('#') if self.dialect.supports_geometric_types() => {
                            self.consume_for_binop(chars, "##", Token::DoubleSharp)
                        }
                        Some(sch) if self.dialect.is_identifier_start('#') => {
                            self.tokenize_identifier_or_keyword([ch, *sch], chars)
                        }
                        _ => self.start_binop(chars, "#", Token::Sharp),
                    }
                }
                '@' => {
                    chars.next();
                    match chars.peek() {
                        Some('@') if self.dialect.supports_geometric_types() => {
                            self.consume_and_return(chars, Token::AtAt)
                        }
                        Some('-') if self.dialect.supports_geometric_types() => {
                            chars.next();
                            match chars.peek() {
                                Some('@') => self.consume_and_return(chars, Token::AtDashAt),
                                _ => self.start_binop_opt(chars, "@-", None),
                            }
                        }
                        Some('>') => self.consume_and_return(chars, Token::AtArrow),
                        Some('?') => self.consume_and_return(chars, Token::AtQuestion),
                        Some('@') => {
                            chars.next();
                            match chars.peek() {
                                Some(' ') => Ok(Some(Token::AtAt)),
                                Some(tch) if self.dialect.is_identifier_start('@') => {
                                    self.tokenize_identifier_or_keyword([ch, '@', *tch], chars)
                                }
                                _ => Ok(Some(Token::AtAt)),
                            }
                        }
                        Some(' ') => Ok(Some(Token::AtSign)),
                        // We break on quotes here, because no dialect allows identifiers starting
                        // with @ and containing quotation marks (e.g. `@'foo'`) unless they are
                        // quoted, which is tokenized as a quoted string, not here (e.g.
                        // `"@'foo'"`). Further, at least two dialects parse `@` followed by a
                        // quoted string as two separate tokens, which this allows. For example,
                        // Postgres parses `@'1'` as the absolute value of '1' which is implicitly
                        // cast to a numeric type. And when parsing MySQL-style grantees (e.g.
                        // `GRANT ALL ON *.* to 'root'@'localhost'`), we also want separate tokens
                        // for the user, the `@`, and the host.
                        Some('\'') => Ok(Some(Token::AtSign)),
                        Some('\"') => Ok(Some(Token::AtSign)),
                        Some('`') => Ok(Some(Token::AtSign)),
                        Some(sch) if self.dialect.is_identifier_start('@') => {
                            self.tokenize_identifier_or_keyword([ch, *sch], chars)
                        }
                        _ => Ok(Some(Token::AtSign)),
                    }
                }
                // Postgres uses ? for jsonb operators, not prepared statements
                '?' if self.dialect.supports_geometric_types() => {
                    chars.next(); // consume
                    match chars.peek() {
                        Some('|') => {
                            chars.next();
                            match chars.peek() {
                                Some('|') => self.consume_and_return(
                                    chars,
                                    Token::QuestionMarkDoubleVerticalBar,
                                ),
                                _ => Ok(Some(Token::QuestionPipe)),
                            }
                        }

                        Some('&') => self.consume_and_return(chars, Token::QuestionAnd),
                        Some('-') => {
                            chars.next(); // consume
                            match chars.peek() {
                                Some('|') => self
                                    .consume_and_return(chars, Token::QuestionMarkDashVerticalBar),
                                _ => Ok(Some(Token::QuestionMarkDash)),
                            }
                        }
                        Some('#') => self.consume_and_return(chars, Token::QuestionMarkSharp),
                        _ => Ok(Some(Token::Question)),
                    }
                }
                '?' => {
                    chars.next();
                    let s = peeking_take_while(chars, |ch| ch.is_numeric());
                    Ok(Some(Token::Placeholder(format!("?{s}"))))
                }

                // identifier or keyword
                ch if self.dialect.is_identifier_start(ch) => {
                    self.tokenize_identifier_or_keyword([ch], chars)
                }
                '$' => Ok(Some(self.tokenize_dollar_preceded_value(chars)?)),

                // whitespace check (including unicode chars) should be last as it covers some of the chars above
                ch if ch.is_whitespace() => {
                    self.consume_and_return(chars, Token::Whitespace(Whitespace::Space))
                }
                other => self.consume_and_return(chars, Token::Char(other)),
            },
            None => Ok(None),
        }
    }

    /// Consume the next character, then parse a custom binary operator. The next character should be included in the prefix
    fn consume_for_binop(
        &self,
        chars: &mut State,
        prefix: &str,
        default: Token,
    ) -> Result<Option<Token>, TokenizerError> {
        chars.next(); // consume the first char
        self.start_binop_opt(chars, prefix, Some(default))
    }

    /// parse a custom binary operator
    fn start_binop(
        &self,
        chars: &mut State,
        prefix: &str,
        default: Token,
    ) -> Result<Option<Token>, TokenizerError> {
        self.start_binop_opt(chars, prefix, Some(default))
    }

    /// parse a custom binary operator
    fn start_binop_opt(
        &self,
        chars: &mut State,
        prefix: &str,
        default: Option<Token>,
    ) -> Result<Option<Token>, TokenizerError> {
        let mut custom = None;
        while let Some(&ch) = chars.peek() {
            if !self.dialect.is_custom_operator_part(ch) {
                break;
            }

            custom.get_or_insert_with(|| prefix.to_string()).push(ch);
            chars.next();
        }
        match (custom, default) {
            (Some(custom), _) => Ok(Token::CustomBinaryOperator(custom).into()),
            (None, Some(tok)) => Ok(Some(tok)),
            (None, None) => self.tokenizer_error(
                chars.location(),
                format!("Expected a valid binary operator after '{prefix}'"),
            ),
        }
    }

    /// Tokenize dollar preceded value (i.e: a string/placeholder)
    fn tokenize_dollar_preceded_value(&self, chars: &mut State) -> Result<Token, TokenizerError> {
        let mut s = String::new();
        let mut value = String::new();

        chars.next();

        // If the dialect does not support dollar-quoted strings, then `$$` is rather a placeholder.
        if matches!(chars.peek(), Some('$')) && !self.dialect.supports_dollar_placeholder() {
            chars.next();

            let mut is_terminated = false;
            let mut prev: Option<char> = None;

            while let Some(&ch) = chars.peek() {
                if prev == Some('$') {
                    if ch == '$' {
                        chars.next();
                        is_terminated = true;
                        break;
                    } else {
                        s.push('$');
                        s.push(ch);
                    }
                } else if ch != '$' {
                    s.push(ch);
                }

                prev = Some(ch);
                chars.next();
            }

            return if chars.peek().is_none() && !is_terminated {
                self.tokenizer_error(chars.location(), "Unterminated dollar-quoted string")
            } else {
                Ok(Token::DollarQuotedString(DollarQuotedString {
                    value: s,
                    tag: None,
                }))
            };
        } else {
            value.push_str(&peeking_take_while(chars, |ch| {
                ch.is_alphanumeric()
                    || ch == '_'
                    // Allow $ as a placeholder character if the dialect supports it
                    || matches!(ch, '$' if self.dialect.supports_dollar_placeholder())
            }));

            // If the dialect supports a dollar sign as a money prefix (e.g. SQL Server),
            // and the value so far is all digits, check for a decimal part, e.g. `$123.45`
            if matches!(chars.peek(), Some('.'))
                && self.dialect.supports_dollar_as_money_prefix()
                && !value.is_empty()
                && value.chars().all(|c| c.is_ascii_digit())
            {
                value.push('.');
                chars.next();
                value.push_str(&peeking_take_while(chars, |ch| ch.is_ascii_digit()));
                return Ok(Token::Placeholder(format!("${value}")));
            }

            // If the dialect does not support dollar-quoted strings, don't look for the end delimiter.
            if matches!(chars.peek(), Some('$')) && !self.dialect.supports_dollar_placeholder() {
                chars.next();

                let mut temp = String::new();
                let end_delimiter = format!("${value}$");

                loop {
                    match chars.next() {
                        Some(ch) => {
                            temp.push(ch);

                            if temp.ends_with(&end_delimiter) {
                                if let Some(temp) = temp.strip_suffix(&end_delimiter) {
                                    s.push_str(temp);
                                }
                                break;
                            }
                        }
                        None => {
                            if temp.ends_with(&end_delimiter) {
                                if let Some(temp) = temp.strip_suffix(&end_delimiter) {
                                    s.push_str(temp);
                                }
                                break;
                            }

                            return self.tokenizer_error(
                                chars.location(),
                                "Unterminated dollar-quoted, expected $",
                            );
                        }
                    }
                }
            } else {
                return Ok(Token::Placeholder(format!("${value}")));
            }
        }

        Ok(Token::DollarQuotedString(DollarQuotedString {
            value: s,
            tag: if value.is_empty() { None } else { Some(value) },
        }))
    }

    fn tokenizer_error<R>(
        &self,
        loc: Location,
        message: impl Into<String>,
    ) -> Result<R, TokenizerError> {
        Err(TokenizerError::new(message, loc))
    }

    // Consume characters until newline
    fn tokenize_single_line_comment(&self, chars: &mut State) -> String {
        let mut comment = peeking_take_while(chars, |ch| match ch {
            '\n' => false,                                           // Always stop at \n
            '\r' if dialect_of!(self is PostgreSqlDialect) => false, // Stop at \r for Postgres
            _ => true, // Keep consuming for other characters
        });

        if let Some(ch) = chars.next() {
            assert!(ch == '\n' || ch == '\r');
            comment.push(ch);
        }

        comment
    }

    /// Tokenize an identifier or keyword, after the first char is already consumed.
    fn tokenize_word(&self, first_chars: impl Into<String>, chars: &mut State) -> String {
        let mut s = first_chars.into();
        s.push_str(&peeking_take_while(chars, |ch| {
            self.dialect.is_identifier_part(ch)
        }));
        s
    }

    /// Read a quoted identifier
    fn tokenize_quoted_identifier(
        &self,
        quote_start: char,
        chars: &mut State,
    ) -> Result<String, TokenizerError> {
        let error_loc = chars.location();
        chars.next(); // consume the opening quote
        let quote_end = Word::matching_end_quote(quote_start);
        let (s, last_char) = self.parse_quoted_ident(chars, quote_end);

        if last_char == Some(quote_end) {
            Ok(s)
        } else {
            self.tokenizer_error(
                error_loc,
                format!("Expected close delimiter '{quote_end}' before EOF."),
            )
        }
    }

    /// Read a single quoted string, starting with the opening quote.
    fn tokenize_escaped_single_quoted_string(
        &self,
        starting_loc: Location,
        chars: &mut State,
    ) -> Result<String, TokenizerError> {
        unescape_single_quoted_string(chars, starting_loc)
    }

    /// Reads a string literal quoted by a single or triple quote characters.
    /// Examples: `'abc'`, `'''abc'''`, `"""abc"""`.
    fn tokenize_single_or_triple_quoted_string<F>(
        &self,
        chars: &mut State,
        quote_style: char,
        backslash_escape: bool,
        single_quote_token: F,
        triple_quote_token: F,
    ) -> Result<Option<Token>, TokenizerError>
    where
        F: Fn(String) -> Token,
    {
        let error_loc = chars.location();

        let mut num_opening_quotes = 0u8;
        for _ in 0..3 {
            if Some(&quote_style) == chars.peek() {
                chars.next(); // Consume quote.
                num_opening_quotes += 1;
            } else {
                break;
            }
        }

        let (token_fn, num_quote_chars) = match num_opening_quotes {
            1 => (single_quote_token, NumStringQuoteChars::One),
            2 => {
                // If we matched double quotes, then this is an empty string.
                return Ok(Some(single_quote_token("".into())));
            }
            3 => {
                let Some(num_quote_chars) = NonZeroU8::new(3) else {
                    return self.tokenizer_error(error_loc, "invalid number of opening quotes");
                };
                (
                    triple_quote_token,
                    NumStringQuoteChars::Many(num_quote_chars),
                )
            }
            _ => {
                return self.tokenizer_error(error_loc, "invalid string literal opening");
            }
        };

        let settings = TokenizeQuotedStringSettings {
            quote_style,
            num_quote_chars,
            num_opening_quotes_to_consume: 0,
            backslash_escape,
        };

        self.tokenize_quoted_string(chars, settings)
            .map(token_fn)
            .map(Some)
    }

    /// Reads a string literal quoted by a single quote character.
    fn tokenize_single_quoted_string(
        &self,
        chars: &mut State,
        quote_style: char,
        backslash_escape: bool,
    ) -> Result<String, TokenizerError> {
        self.tokenize_quoted_string(
            chars,
            TokenizeQuotedStringSettings {
                quote_style,
                num_quote_chars: NumStringQuoteChars::One,
                num_opening_quotes_to_consume: 1,
                backslash_escape,
            },
        )
    }

    /// Reads a quote delimited string expecting `chars.next()` to deliver a quote.
    ///
    /// See <https://docs.oracle.com/en/database/oracle/oracle-database/21/sqlrf/Literals.html#GUID-1824CBAA-6E16-4921-B2A6-112FB02248DA>
    fn tokenize_quote_delimited_string(
        &self,
        chars: &mut State,
        // the prefix that introduced the possible literal or word,
        // e.g. "Q" or "nq"
        literal_prefix: &[char],
    ) -> Result<QuoteDelimitedString, TokenizerError> {
        let literal_start_loc = chars.location();
        chars.next();

        let start_quote_loc = chars.location();
        let (start_quote, end_quote) = match chars.next() {
            None | Some(' ') | Some('\t') | Some('\r') | Some('\n') => {
                return self.tokenizer_error(
                    start_quote_loc,
                    format!(
                        "Invalid space, tab, newline, or EOF after '{}''",
                        String::from_iter(literal_prefix)
                    ),
                );
            }
            Some(c) => (
                c,
                match c {
                    '[' => ']',
                    '{' => '}',
                    '<' => '>',
                    '(' => ')',
                    c => c,
                },
            ),
        };

        // read the string literal until the "quote character" following a by literal quote
        let mut value = String::new();
        while let Some(ch) = chars.next() {
            if ch == end_quote {
                if let Some('\'') = chars.peek() {
                    chars.next(); // ~ consume the quote
                    return Ok(QuoteDelimitedString {
                        start_quote,
                        value,
                        end_quote,
                    });
                }
            }
            value.push(ch);
        }

        self.tokenizer_error(literal_start_loc, "Unterminated string literal")
    }

    /// Read a quoted string.
    fn tokenize_quoted_string(
        &self,
        chars: &mut State,
        settings: TokenizeQuotedStringSettings,
    ) -> Result<String, TokenizerError> {
        let mut s = String::new();
        let error_loc = chars.location();

        // Consume any opening quotes.
        for _ in 0..settings.num_opening_quotes_to_consume {
            if Some(settings.quote_style) != chars.next() {
                return self.tokenizer_error(error_loc, "invalid string literal opening");
            }
        }

        let mut num_consecutive_quotes = 0;
        while let Some(&ch) = chars.peek() {
            let pending_final_quote = match settings.num_quote_chars {
                NumStringQuoteChars::One => Some(NumStringQuoteChars::One),
                n @ NumStringQuoteChars::Many(count)
                    if num_consecutive_quotes + 1 == count.get() =>
                {
                    Some(n)
                }
                NumStringQuoteChars::Many(_) => None,
            };

            match ch {
                char if char == settings.quote_style && pending_final_quote.is_some() => {
                    chars.next(); // consume

                    if let Some(NumStringQuoteChars::Many(count)) = pending_final_quote {
                        // For an initial string like `"""abc"""`, at this point we have
                        // `abc""` in the buffer and have now matched the final `"`.
                        // However, the string to return is simply `abc`, so we strip off
                        // the trailing quotes before returning.
                        let mut buf = s.chars();
                        for _ in 1..count.get() {
                            buf.next_back();
                        }
                        return Ok(buf.as_str().to_string());
                    } else if chars
                        .peek()
                        .map(|c| *c == settings.quote_style)
                        .unwrap_or(false)
                    {
                        s.push(ch);
                        if !self.unescape {
                            // In no-escape mode, the given query has to be saved completely
                            s.push(ch);
                        }
                        chars.next();
                    } else {
                        return Ok(s);
                    }
                }
                '\\' if settings.backslash_escape => {
                    // consume backslash
                    chars.next();

                    num_consecutive_quotes = 0;

                    if let Some(next) = chars.peek() {
                        if !self.unescape
                            || (self.dialect.ignores_wildcard_escapes()
                                && (*next == '%' || *next == '_'))
                        {
                            // In no-escape mode, the given query has to be saved completely
                            // including backslashes. Similarly, with ignore_like_wildcard_escapes,
                            // the backslash is not stripped.
                            s.push(ch);
                            s.push(*next);
                            chars.next(); // consume next
                        } else {
                            let n = match next {
                                '0' => '\0',
                                'a' => '\u{7}',
                                'b' => '\u{8}',
                                'f' => '\u{c}',
                                'n' => '\n',
                                'r' => '\r',
                                't' => '\t',
                                'Z' => '\u{1a}',
                                _ => *next,
                            };
                            s.push(n);
                            chars.next(); // consume next
                        }
                    }
                }
                ch => {
                    chars.next(); // consume ch

                    if ch == settings.quote_style {
                        num_consecutive_quotes += 1;
                    } else {
                        num_consecutive_quotes = 0;
                    }

                    s.push(ch);
                }
            }
        }
        self.tokenizer_error(error_loc, "Unterminated string literal")
    }

    fn tokenize_multiline_comment(
        &self,
        chars: &mut State,
    ) -> Result<Option<Token>, TokenizerError> {
        let mut s = String::new();
        let mut nested = 1;
        let supports_nested_comments = self.dialect.supports_nested_comments();
        loop {
            match chars.next() {
                Some('/') if matches!(chars.peek(), Some('*')) && supports_nested_comments => {
                    chars.next(); // consume the '*'
                    s.push('/');
                    s.push('*');
                    nested += 1;
                }
                Some('*') if matches!(chars.peek(), Some('/')) => {
                    chars.next(); // consume the '/'
                    nested -= 1;
                    if nested == 0 {
                        break Ok(Some(Token::Whitespace(Whitespace::MultiLineComment(s))));
                    }
                    s.push('*');
                    s.push('/');
                }
                Some(ch) => {
                    s.push(ch);
                }
                None => {
                    break self.tokenizer_error(
                        chars.location(),
                        "Unexpected EOF while in a multi-line comment",
                    );
                }
            }
        }
    }

    fn parse_quoted_ident(&self, chars: &mut State, quote_end: char) -> (String, Option<char>) {
        let mut last_char = None;
        let mut s = String::new();
        while let Some(ch) = chars.next() {
            if ch == quote_end {
                if chars.peek() == Some(&quote_end) {
                    chars.next();
                    s.push(ch);
                    if !self.unescape {
                        // In no-escape mode, the given query has to be saved completely
                        s.push(ch);
                    }
                } else {
                    last_char = Some(quote_end);
                    break;
                }
            } else {
                s.push(ch);
            }
        }
        (s, last_char)
    }

    #[allow(clippy::unnecessary_wraps)]
    fn consume_and_return(
        &self,
        chars: &mut State,
        t: Token,
    ) -> Result<Option<Token>, TokenizerError> {
        chars.next();
        Ok(Some(t))
    }
}

/// Read from `chars` until `predicate` returns `false` or EOF is hit.
/// Return the characters read as String, and keep the first non-matching
/// char available as `chars.next()`.
fn peeking_take_while(chars: &mut State, mut predicate: impl FnMut(char) -> bool) -> String {
    let mut s = String::new();
    while let Some(&ch) = chars.peek() {
        if predicate(ch) {
            chars.next(); // consume
            s.push(ch);
        } else {
            break;
        }
    }
    s
}

/// Same as peeking_take_while, but also passes the next character to the predicate.
fn peeking_next_take_while(
    chars: &mut State,
    mut predicate: impl FnMut(char, Option<char>) -> bool,
) -> String {
    let mut s = String::new();
    while let Some(&ch) = chars.peek() {
        let next_char = chars.peekable.clone().nth(1);
        if predicate(ch, next_char) {
            chars.next(); // consume
            s.push(ch);
        } else {
            break;
        }
    }
    s
}

/// PG's hint for a malformed `\u`/`\U` escape inside an `E'…'` literal.
const ESCAPE_UNICODE_HINT: &str = r"Unicode escapes must be \uXXXX or \UXXXXXXXX.";

/// PG's hint for a malformed escape in a `U&'…'` literal.
const UESCAPE_HINT: &str = r"Unicode escapes must be \XXXX or \+XXXXXX.";

/// The number of bytes PostgreSQL prints for a malformed UTF-8 sequence: the
/// length the lead byte promises, clipped to what is actually there. So a bare
/// `0x80` (not a lead byte) prints one byte, `0xca 0x44` prints two even though
/// only the `0xca` is meaningful, and a truncated `0xf0 0x9f 0x98` prints three.
fn utf8_report_len(bytes: &[u8]) -> usize {
    let promised = match bytes.first() {
        Some(b) if b & 0xE0 == 0xC0 => 2,
        Some(b) if b & 0xF0 == 0xE0 => 3,
        Some(b) if b & 0xF8 == 0xF0 => 4,
        _ => 1,
    };
    promised.min(bytes.len())
}

/// PG's encoding error, naming the offending bytes as `0xNN` separated by
/// spaces. It carries no cursor position.
fn invalid_encoding(bytes: &[u8]) -> TokenizerError {
    let mut hex = String::new();
    for b in bytes {
        if !hex.is_empty() {
            hex.push(' ');
        }
        hex.push_str(&format!("0x{b:02x}"));
    }
    TokenizerError::pg(
        "22021",
        format!("invalid byte sequence for encoding \"UTF8\": {hex}"),
        None,
        Location::empty(),
    )
}

/// How PostgreSQL words the diagnostics for the two Unicode-escape syntaxes it
/// accepts in string literals: `\uXXXX`/`\UXXXXXXXX` inside `E'…'`, and
/// `\XXXX`/`\+XXXXXX` inside `U&'…'`.
///
/// The rules for the *value* an escape names are identical in both — zero and
/// anything above U+10FFFF are rejected, and a UTF-16 surrogate is only legal
/// as half of a pair — but the wording differs, so it lives here while the
/// digit readers stay with their own syntax.
struct UnicodeEscapeDiag {
    /// SQLSTATE for a malformed escape (a bad or missing hex digit). PG raises
    /// a data exception inside `E'…'` but a syntax error inside `U&'…'`.
    malformed_sqlstate: &'static str,
    hint: &'static str,
    /// `E'…'` diagnostics quote the offending escape; `U&'…'` ones do not.
    names_token: bool,
}

impl UnicodeEscapeDiag {
    const ESCAPE_STRING: Self = Self {
        malformed_sqlstate: "22025",
        hint: ESCAPE_UNICODE_HINT,
        names_token: true,
    };
    const UNICODE_STRING: Self = Self {
        malformed_sqlstate: DEFAULT_TOKENIZER_SQLSTATE,
        hint: UESCAPE_HINT,
        names_token: false,
    };

    /// Too few hex digits, or a non-hex digit where one was required.
    fn malformed(&self, loc: Location) -> TokenizerError {
        TokenizerError::pg(
            self.malformed_sqlstate,
            "invalid Unicode escape",
            Some(self.hint.to_string()),
            loc,
        )
    }

    fn worded(&self, message: &str, token: &str) -> String {
        if self.names_token {
            format!("{message} at or near \"{token}\"")
        } else {
            message.to_string()
        }
    }

    /// A well-formed escape naming something that is not a usable code point.
    /// PG accepts `0 < value <= 0x10FFFF`; surrogates pass here and are paired
    /// by the caller.
    fn check_value(&self, value: u32, loc: Location, token: &str) -> Result<(), TokenizerError> {
        if value == 0 || value > 0x10FFFF {
            return Err(TokenizerError::pg(
                DEFAULT_TOKENIZER_SQLSTATE,
                self.worded("invalid Unicode escape value", token),
                None,
                loc,
            ));
        }
        Ok(())
    }

    /// A surrogate half that nothing completes.
    fn surrogate(&self, loc: Location, token: &str) -> TokenizerError {
        TokenizerError::pg(
            DEFAULT_TOKENIZER_SQLSTATE,
            self.worded("invalid Unicode surrogate pair", token),
            None,
            loc,
        )
    }
}

const HIGH_SURROGATE: std::ops::RangeInclusive<u32> = 0xD800..=0xDBFF;
const LOW_SURROGATE: std::ops::RangeInclusive<u32> = 0xDC00..=0xDFFF;

/// Combine a validated high/low surrogate pair into the code point it encodes.
fn combine_surrogates(high: u32, low: u32) -> char {
    let scalar = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
    char::from_u32(scalar).expect("a paired surrogate always names a scalar")
}

fn unescape_single_quoted_string(
    chars: &mut State<'_>,
    starting_loc: Location,
) -> Result<String, TokenizerError> {
    Unescape::new(chars).unescape(starting_loc)
}

/// Decoder for the body of a PostgreSQL escape string constant, `E'…'`.
///
/// PostgreSQL decodes these escapes into **bytes** and only validates the result
/// as UTF-8 once the whole literal is assembled, which is why `E'\xc3\xa9'` is
/// the single character `é` while `E'\x80'` on its own is a 22021 encoding
/// error. Decoding straight into a Rust `String` cannot express that, so the
/// buffer here is a `Vec<u8>` and the UTF-8 check happens in `finish`.
struct Unescape<'a: 'b, 'b> {
    chars: &'b mut State<'a>,
}

impl<'a: 'b, 'b> Unescape<'a, 'b> {
    fn new(chars: &'b mut State<'a>) -> Self {
        Self { chars }
    }

    fn unescape(mut self, starting_loc: Location) -> Result<String, TokenizerError> {
        let mut buf: Vec<u8> = Vec::new();
        let mut scratch = [0u8; 4];

        self.chars.next(); // consume the opening quote

        loop {
            // Taken before the character is consumed, so it names that
            // character's own column: PG's caret sits on the offending
            // backslash, not on what follows it.
            let esc_loc = self.chars.location();
            let Some(c) = self.chars.next() else { break };

            if c == '\'' {
                // case: ''''
                if self.chars.peek() == Some(&'\'') {
                    self.chars.next();
                    buf.push(b'\'');
                    continue;
                }
                return Self::finish(buf);
            }

            if c != '\\' {
                buf.extend_from_slice(c.encode_utf8(&mut scratch).as_bytes());
                continue;
            }

            let Some(esc) = self.chars.next() else { break };
            match esc {
                'b' => buf.push(0x08),
                'f' => buf.push(0x0C),
                'n' => buf.push(b'\n'),
                'r' => buf.push(b'\r'),
                't' => buf.push(b'\t'),
                'u' | 'U' => {
                    let c = self.unescape_unicode(esc == 'U', esc_loc)?;
                    buf.extend_from_slice(c.encode_utf8(&mut scratch).as_bytes());
                }
                'x' => buf.push(self.unescape_hex()),
                d if d.is_digit(8) => buf.push(self.unescape_octal(d)),
                // Anything else stands for itself, including `\\`, `\'` and
                // `\"`. PG silently drops the backslash: `E'\z'` is `z`.
                other => buf.extend_from_slice(other.encode_utf8(&mut scratch).as_bytes()),
            }
        }

        Err(TokenizerError::new(
            "Unterminated encoded string literal",
            starting_loc,
        ))
    }

    /// Validate the assembled bytes the way PostgreSQL validates a client
    /// string: it must be well-formed in the server encoding (UTF-8 here), and
    /// it may not contain a NUL — which `str::from_utf8` would happily accept,
    /// so it is rejected separately.
    fn finish(buf: Vec<u8>) -> Result<String, TokenizerError> {
        match String::from_utf8(buf) {
            Ok(s) => match s.bytes().position(|b| b == 0) {
                None => Ok(s),
                Some(i) => Err(invalid_encoding(&s.as_bytes()[i..=i])),
            },
            Err(e) => {
                let start = e.utf8_error().valid_up_to();
                let bytes = e.as_bytes();
                Err(invalid_encoding(
                    &bytes[start..][..utf8_report_len(&bytes[start..])],
                ))
            }
        }
    }

    /// Hexadecimal byte value: `\xh`, `\xhh` (h = 0–9, A–F). With no hex digit
    /// at all the `x` stands for itself, as in PG.
    fn unescape_hex(&mut self) -> u8 {
        let mut value: u32 = 0;
        let mut digits = 0;
        while digits < 2 {
            let Some(d) = self.chars.peek().and_then(|c| c.to_digit(16)) else {
                break;
            };
            self.chars.next();
            value = value * 16 + d;
            digits += 1;
        }
        if digits == 0 {
            b'x'
        } else {
            value as u8
        }
    }

    /// Octal byte value: `\o`, `\oo`, `\ooo` (o = 0–7). PG truncates rather than
    /// erroring on overflow, so `\400` is the byte `0x00`.
    fn unescape_octal(&mut self, first: char) -> u8 {
        let mut value: u32 = first.to_digit(8).expect("caller checked");
        for _ in 0..2 {
            let Some(d) = self.chars.peek().and_then(|c| c.to_digit(8)) else {
                break;
            };
            self.chars.next();
            value = value * 8 + d;
        }
        (value & 0xFF) as u8
    }

    /// `\uXXXX` (16-bit) or `\UXXXXXXXX` (32-bit) code point. A UTF-16 high
    /// surrogate must be followed by a `\u` low surrogate; the pair combines
    /// into one scalar, so a high `\ud83d` beside a low `\ude04` decodes to
    /// the single character 😄.
    fn unescape_unicode(&mut self, wide: bool, esc_loc: Location) -> Result<char, TokenizerError> {
        const DIAG: UnicodeEscapeDiag = UnicodeEscapeDiag::ESCAPE_STRING;
        let (value, text) = self.hex_escape(wide, esc_loc)?;

        // A low surrogate with no high half before it is reported on the escape
        // itself; a high surrogate is reported on whatever failed to complete
        // it, which is why the two cases pass different locations and tokens.
        if LOW_SURROGATE.contains(&value) {
            return Err(DIAG.surrogate(esc_loc, &text));
        }
        if !HIGH_SURROGATE.contains(&value) {
            DIAG.check_value(value, esc_loc, &text)?;
            return char::from_u32(value).ok_or_else(|| DIAG.malformed(esc_loc));
        }

        let after = self.chars.location();
        if self.chars.peek() != Some(&'\\') {
            // Whatever stands where the low half should be — possibly the
            // literal's own closing quote.
            let token: String = self.chars.peek().into_iter().collect();
            return Err(DIAG.surrogate(after, &token));
        }
        self.chars.next();
        let low_wide = match self.chars.peek() {
            Some('u') => false,
            Some('U') => true,
            _ => return Err(DIAG.surrogate(after, "\\")),
        };
        self.chars.next();
        let (low, low_text) = self.hex_escape(low_wide, esc_loc)?;
        if !LOW_SURROGATE.contains(&low) {
            return Err(DIAG.surrogate(after, &low_text));
        }
        Ok(combine_surrogates(value, low))
    }

    /// Read the hex digits of a `\u`/`\U` escape, returning both the value and
    /// the escape as written — PG quotes the source text in its error.
    fn hex_escape(
        &mut self,
        wide: bool,
        esc_loc: Location,
    ) -> Result<(u32, String), TokenizerError> {
        let mut value: u32 = 0;
        let mut text = String::from(if wide { "\\U" } else { "\\u" });
        for _ in 0..if wide { 8 } else { 4 } {
            let c = *self
                .chars
                .peek()
                .ok_or_else(|| UnicodeEscapeDiag::ESCAPE_STRING.malformed(esc_loc))?;
            let d = c
                .to_digit(16)
                .ok_or_else(|| UnicodeEscapeDiag::ESCAPE_STRING.malformed(esc_loc))?;
            self.chars.next();
            text.push(c);
            value = value * 16 + d;
        }
        Ok((value, text))
    }
}

fn unescape_unicode_single_quoted_string(chars: &mut State<'_>) -> Result<String, TokenizerError> {
    let mut unescaped = String::new();
    chars.next(); // consume the opening quote
    loop {
        // Taken before the character is consumed, so it names that character's
        // own column — PG puts the caret on the offending backslash.
        let esc_loc = chars.location();
        let Some(c) = chars.next() else { break };
        match c {
            '\'' => {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    unescaped.push('\'');
                } else {
                    return Ok(unescaped);
                }
            }
            '\\' if chars.peek() == Some(&'\\') => {
                chars.next();
                unescaped.push('\\');
            }
            '\\' => unescaped.push(take_char_from_hex_digits(chars, esc_loc)?),
            _ => {
                unescaped.push(c);
            }
        }
    }
    Err(TokenizerError::new(
        "Unterminated unicode encoded string literal",
        chars.location(),
    ))
}

/// Read one `U&'…'` escape — `\XXXX`, or `\+XXXXXX` for the six-digit form —
/// starting just after its backslash, which `esc_loc` names.
///
/// These escapes name a code point directly rather than a byte, and carry the
/// same value rules as their `E'…'` counterparts: zero and anything above
/// U+10FFFF are rejected, and a UTF-16 surrogate is legal only as half of a
/// pair, so `U&'\D83D\DE04'` is one character.
fn take_char_from_hex_digits(
    chars: &mut State<'_>,
    esc_loc: Location,
) -> Result<char, TokenizerError> {
    const DIAG: UnicodeEscapeDiag = UnicodeEscapeDiag::UNICODE_STRING;
    let value = read_uescape_digits(chars, esc_loc)?;

    if LOW_SURROGATE.contains(&value) {
        return Err(DIAG.surrogate(esc_loc, ""));
    }
    if !HIGH_SURROGATE.contains(&value) {
        DIAG.check_value(value, esc_loc, "")?;
        return char::from_u32(value).ok_or_else(|| DIAG.malformed(esc_loc));
    }

    // A high surrogate is reported on whatever failed to complete it, so the
    // low half's own location is what the caret points at.
    let after = chars.location();
    if chars.peek() != Some(&'\\') {
        return Err(DIAG.surrogate(after, ""));
    }
    chars.next();
    let low = read_uescape_digits(chars, after)?;
    if !LOW_SURROGATE.contains(&low) {
        return Err(DIAG.surrogate(after, ""));
    }
    Ok(combine_surrogates(value, low))
}

/// The digits of one `U&'…'` escape: `\+` selects the six-digit form, anything
/// else is four digits.
fn read_uescape_digits(chars: &mut State<'_>, esc_loc: Location) -> Result<u32, TokenizerError> {
    let digits = if chars.peek() == Some(&'+') {
        chars.next();
        6
    } else {
        4
    };
    let mut value = 0u32;
    for _ in 0..digits {
        let digit = chars
            .peek()
            .and_then(|c| c.to_digit(16))
            .ok_or_else(|| UnicodeEscapeDiag::UNICODE_STRING.malformed(esc_loc))?;
        chars.next();
        value = value * 16 + digit;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::PostgreSqlDialect;
    use crate::test_utils::{all_dialects, all_dialects_where};
    use core::fmt::Debug;

    #[test]
    fn tokenizer_error_impl() {
        let err = TokenizerError::new("test", Location { line: 1, column: 1 });
        {
            use core::error::Error;
            assert!(err.source().is_none());
        }
        assert_eq!(err.to_string(), "test at Line: 1, Column: 1");

        // A PG diagnostic stands on its own: the location is reported as a
        // cursor position instead of being appended to the message.
        let pg = TokenizerError::pg(
            "22025",
            "invalid Unicode escape",
            Some(ESCAPE_UNICODE_HINT.to_string()),
            Location {
                line: 1,
                column: 10,
            },
        );
        assert_eq!(pg.to_string(), "invalid Unicode escape");
    }

    #[test]
    fn tokenize_select_1() -> anyhow::Result<()> {
        let sql = String::from("SELECT 1");
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;

        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::Number(String::from("1"), false),
        ];

        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_select_float() -> anyhow::Result<()> {
        let sql = String::from("SELECT .1");
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;

        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::Number(String::from(".1"), false),
        ];

        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_with_mapper() -> anyhow::Result<()> {
        let sql = String::from("SELECT ?");
        let dialect = GenericDialect {};
        let mut param_num = 1;

        let mut tokens = vec![];
        Tokenizer::new(&dialect, &sql).tokenize_with_location_into_buf_with_mapper(
            &mut tokens,
            |mut token_span| {
                token_span.token = match token_span.token {
                    Token::Placeholder(n) => Token::Placeholder(if n == "?" {
                        let ret = format!("${}", param_num);
                        param_num += 1;
                        ret
                    } else {
                        n
                    }),
                    token => token,
                };
                token_span
            },
        )?;
        let actual = tokens.into_iter().map(|t| t.token).collect();
        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::Placeholder("$1".to_string()),
        ];

        compare(expected, actual);

        Ok(())
    }

    #[test]
    fn tokenize_numeric_literal_underscore() -> anyhow::Result<()> {
        let dialect = GenericDialect {};
        let sql = String::from("SELECT 10_000");
        let mut tokenizer = Tokenizer::new(&dialect, &sql);
        let tokens = tokenizer.tokenize()?;
        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::Number("10".to_string(), false),
            Token::make_word("_000", None),
        ];
        compare(expected, tokens);

        // PostgreSQL takes a well-placed separator as part of the number, and a
        // leading one as an identifier (`_10_000` is a column reference, which
        // is why the error for it is "column does not exist", not a syntax
        // error). The malformed placements are errors — see
        // `tokenize_pg_trailing_junk`.
        all_dialects_where(|dialect| dialect.supports_numeric_literal_underscores()).tokenizes_to(
            "SELECT 10_000, _10_000",
            vec![
                Token::make_keyword("SELECT"),
                Token::Whitespace(Whitespace::Space),
                Token::Number("10_000".to_string(), false),
                Token::Comma,
                Token::Whitespace(Whitespace::Space),
                Token::make_word("_10_000", None),
            ],
        );

        Ok(())
    }

    /// PostgreSQL 16 and later read `0x1F` / `0o17` / `0b11` as integers, not as
    /// the `X'…'` bit string other dialects take `0x…` for.
    #[test]
    fn tokenize_pg_non_decimal_literals() {
        let dialect = PostgreSqlDialect {};
        for (sql, want) in [
            ("SELECT 0x42F", "0x42F"),
            ("SELECT 0X42f", "0X42f"),
            ("SELECT 0o273", "0o273"),
            ("SELECT 0b100101", "0b100101"),
            // A separator may lead the digits of a prefixed literal, because the
            // prefix stands in for the digit on its left.
            ("SELECT 0b_10_0101", "0b_10_0101"),
            ("SELECT 0xE_FF", "0xE_FF"),
        ] {
            let tokens = Tokenizer::new(&dialect, sql).tokenize().expect(sql);
            assert_eq!(
                tokens.last(),
                Some(&Token::Number(want.to_string(), false)),
                "for {sql}"
            );
        }
    }

    /// The literals PostgreSQL rejects outright, where a laxer tokenizer would
    /// hand back a number beside a word.
    #[test]
    fn tokenize_pg_trailing_junk() {
        let dialect = PostgreSqlDialect {};
        for (sql, want) in [
            ("SELECT 0x", "invalid hexadecimal integer at or near \"0x\""),
            ("SELECT 0o", "invalid octal integer at or near \"0o\""),
            ("SELECT 0b", "invalid binary integer at or near \"0b\""),
            (
                "SELECT 0x0y",
                "trailing junk after numeric literal at or near \"0x0y\"",
            ),
            (
                "SELECT 123abc",
                "trailing junk after numeric literal at or near \"123abc\"",
            ),
            (
                "SELECT 1x",
                "trailing junk after numeric literal at or near \"1x\"",
            ),
            (
                "SELECT 100_",
                "trailing junk after numeric literal at or near \"100_\"",
            ),
            (
                "SELECT 100__000",
                "trailing junk after numeric literal at or near \"100__000\"",
            ),
            (
                "SELECT 1_000._5",
                "trailing junk after numeric literal at or near \"1_000._5\"",
            ),
            (
                "SELECT 1_000.5e_1",
                "trailing junk after numeric literal at or near \"1_000.5e_1\"",
            ),
            // A separator sits between two digits *of the run's own base*, so
            // `e` does not close one even though it is a hex digit…
            (
                "SELECT 1_e5",
                "trailing junk after numeric literal at or near \"1_e5\"",
            ),
            (
                "SELECT 2_e+3",
                "trailing junk after numeric literal at or near \"2_e\"",
            ),
            (
                "SELECT 1.5_e3",
                "trailing junk after numeric literal at or near \"1.5_e3\"",
            ),
            // …and `2` does not close one inside a binary run.
            (
                "SELECT 0b1_2",
                "trailing junk after numeric literal at or near \"0b1_2\"",
            ),
        ] {
            let err = Tokenizer::new(&dialect, sql)
                .tokenize()
                .expect_err(sql)
                .to_string();
            assert!(err.contains(want), "for {sql}: got {err}");
        }
    }

    #[test]
    fn tokenize_select_exponent() -> anyhow::Result<()> {
        let sql = String::from("SELECT 1e10, 1e-10, 1e+10, 1ea, 1e-10a, 1e-10-10");
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;

        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::Number(String::from("1e10"), false),
            Token::Comma,
            Token::Whitespace(Whitespace::Space),
            Token::Number(String::from("1e-10"), false),
            Token::Comma,
            Token::Whitespace(Whitespace::Space),
            Token::Number(String::from("1e+10"), false),
            Token::Comma,
            Token::Whitespace(Whitespace::Space),
            Token::Number(String::from("1"), false),
            Token::make_word("ea", None),
            Token::Comma,
            Token::Whitespace(Whitespace::Space),
            Token::Number(String::from("1e-10"), false),
            Token::make_word("a", None),
            Token::Comma,
            Token::Whitespace(Whitespace::Space),
            Token::Number(String::from("1e-10"), false),
            Token::Minus,
            Token::Number(String::from("10"), false),
        ];

        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_scalar_function() -> anyhow::Result<()> {
        let sql = String::from("SELECT sqrt(1)");
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;

        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::make_word("sqrt", None),
            Token::LParen,
            Token::Number(String::from("1"), false),
            Token::RParen,
        ];

        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_string_string_concat() -> anyhow::Result<()> {
        let sql = String::from("SELECT 'a' || 'b'");
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;

        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::SingleQuotedString(String::from("a")),
            Token::Whitespace(Whitespace::Space),
            Token::StringConcat,
            Token::Whitespace(Whitespace::Space),
            Token::SingleQuotedString(String::from("b")),
        ];

        compare(expected, tokens);

        Ok(())
    }
    #[test]
    fn tokenize_bitwise_op() -> anyhow::Result<()> {
        let sql = String::from("SELECT one | two ^ three");
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;

        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::make_word("one", None),
            Token::Whitespace(Whitespace::Space),
            Token::Pipe,
            Token::Whitespace(Whitespace::Space),
            Token::make_word("two", None),
            Token::Whitespace(Whitespace::Space),
            Token::Caret,
            Token::Whitespace(Whitespace::Space),
            Token::make_word("three", None),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_logical_xor() -> anyhow::Result<()> {
        let sql =
            String::from("SELECT true XOR true, false XOR false, true XOR false, false XOR true");
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;

        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("true"),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("XOR"),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("true"),
            Token::Comma,
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("false"),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("XOR"),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("false"),
            Token::Comma,
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("true"),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("XOR"),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("false"),
            Token::Comma,
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("false"),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("XOR"),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("true"),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_simple_select() -> anyhow::Result<()> {
        let sql = String::from("SELECT * FROM customer WHERE id = 1 LIMIT 5");
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;

        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::Mul,
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("FROM"),
            Token::Whitespace(Whitespace::Space),
            Token::make_word("customer", None),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("WHERE"),
            Token::Whitespace(Whitespace::Space),
            Token::make_word("id", None),
            Token::Whitespace(Whitespace::Space),
            Token::Eq,
            Token::Whitespace(Whitespace::Space),
            Token::Number(String::from("1"), false),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("LIMIT"),
            Token::Whitespace(Whitespace::Space),
            Token::Number(String::from("5"), false),
        ];

        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_explain_select() -> anyhow::Result<()> {
        let sql = String::from("EXPLAIN SELECT * FROM customer WHERE id = 1");
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;

        let expected = vec![
            Token::make_keyword("EXPLAIN"),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::Mul,
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("FROM"),
            Token::Whitespace(Whitespace::Space),
            Token::make_word("customer", None),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("WHERE"),
            Token::Whitespace(Whitespace::Space),
            Token::make_word("id", None),
            Token::Whitespace(Whitespace::Space),
            Token::Eq,
            Token::Whitespace(Whitespace::Space),
            Token::Number(String::from("1"), false),
        ];

        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_explain_analyze_select() -> anyhow::Result<()> {
        let sql = String::from("EXPLAIN ANALYZE SELECT * FROM customer WHERE id = 1");
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;

        let expected = vec![
            Token::make_keyword("EXPLAIN"),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("ANALYZE"),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::Mul,
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("FROM"),
            Token::Whitespace(Whitespace::Space),
            Token::make_word("customer", None),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("WHERE"),
            Token::Whitespace(Whitespace::Space),
            Token::make_word("id", None),
            Token::Whitespace(Whitespace::Space),
            Token::Eq,
            Token::Whitespace(Whitespace::Space),
            Token::Number(String::from("1"), false),
        ];

        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_string_predicate() -> anyhow::Result<()> {
        let sql = String::from("SELECT * FROM customer WHERE salary != 'Not Provided'");
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;

        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::Mul,
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("FROM"),
            Token::Whitespace(Whitespace::Space),
            Token::make_word("customer", None),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("WHERE"),
            Token::Whitespace(Whitespace::Space),
            Token::make_word("salary", None),
            Token::Whitespace(Whitespace::Space),
            Token::Neq,
            Token::Whitespace(Whitespace::Space),
            Token::SingleQuotedString(String::from("Not Provided")),
        ];

        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_invalid_string() -> anyhow::Result<()> {
        let sql = String::from("\n💝مصطفىh");

        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;
        // println!("tokens: {:#?}", tokens);
        let expected = vec![
            Token::Whitespace(Whitespace::Newline),
            Token::Char('💝'),
            Token::make_word("مصطفىh", None),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_newline_in_string_literal() -> anyhow::Result<()> {
        let sql = String::from("'foo\r\nbar\nbaz'");

        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;
        let expected = vec![Token::SingleQuotedString("foo\r\nbar\nbaz".to_string())];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_unterminated_string_literal() {
        let sql = String::from("select 'foo");

        let dialect = GenericDialect {};
        let mut tokenizer = Tokenizer::new(&dialect, &sql);
        assert_eq!(
            tokenizer.tokenize(),
            Err(TokenizerError::new(
                "Unterminated string literal",
                Location { line: 1, column: 8 }
            ))
        );
    }

    #[test]
    fn tokenize_unterminated_string_literal_utf8() {
        let sql = String::from("SELECT \"なにか\" FROM Y WHERE \"なにか\" = 'test;");

        let dialect = GenericDialect {};
        let mut tokenizer = Tokenizer::new(&dialect, &sql);
        assert_eq!(
            tokenizer.tokenize(),
            Err(TokenizerError::new(
                "Unterminated string literal",
                Location {
                    line: 1,
                    column: 35
                }
            ))
        );
    }

    #[test]
    fn tokenize_invalid_string_cols() -> anyhow::Result<()> {
        let sql = String::from("\n\nSELECT * FROM table\t💝مصطفىh");

        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;
        // println!("tokens: {:#?}", tokens);
        let expected = vec![
            Token::Whitespace(Whitespace::Newline),
            Token::Whitespace(Whitespace::Newline),
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::Mul,
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("FROM"),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("table"),
            Token::Whitespace(Whitespace::Tab),
            Token::Char('💝'),
            Token::make_word("مصطفىh", None),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_dollar_quoted_string_tagged() -> anyhow::Result<()> {
        let test_cases = vec![
            (
                String::from("SELECT $tag$dollar '$' quoted strings have $tags like this$ or like this $$$tag$"),
                vec![
                    Token::make_keyword("SELECT"),
                    Token::Whitespace(Whitespace::Space),
                    Token::DollarQuotedString(DollarQuotedString {
                        value: "dollar '$' quoted strings have $tags like this$ or like this $$".into(),
                        tag: Some("tag".into()),
                    })
                ]
            ),
            (
                String::from("SELECT $abc$x$ab$abc$"),
                vec![
                    Token::make_keyword("SELECT"),
                    Token::Whitespace(Whitespace::Space),
                    Token::DollarQuotedString(DollarQuotedString {
                        value: "x$ab".into(),
                        tag: Some("abc".into()),
                    })
                ]
            ),
            (
                String::from("SELECT $abc$$abc$"),
                vec![
                    Token::make_keyword("SELECT"),
                    Token::Whitespace(Whitespace::Space),
                    Token::DollarQuotedString(DollarQuotedString {
                        value: "".into(),
                        tag: Some("abc".into()),
                    })
                ]
            ),
            (
                String::from("0$abc$$abc$1"),
                vec![
                    Token::Number("0".into(), false),
                    Token::DollarQuotedString(DollarQuotedString {
                        value: "".into(),
                        tag: Some("abc".into()),
                    }),
                    Token::Number("1".into(), false),
                ]
            ),
            (
                String::from("$function$abc$q$data$q$$function$"),
                vec![
                    Token::DollarQuotedString(DollarQuotedString {
                        value: "abc$q$data$q$".into(),
                        tag: Some("function".into()),
                    }),
                ]
            ),
        ];

        let dialect = GenericDialect {};
        for (sql, expected) in test_cases {
            let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;
            compare(expected, tokens);
        }

        Ok(())
    }

    #[test]
    fn tokenize_dollar_quoted_string_tagged_unterminated() {
        let sql = String::from("SELECT $tag$dollar '$' quoted strings have $tags like this$ or like this $$$different tag$");
        let dialect = GenericDialect {};
        assert_eq!(
            Tokenizer::new(&dialect, &sql).tokenize(),
            Err(TokenizerError::new(
                "Unterminated dollar-quoted, expected $",
                Location {
                    line: 1,
                    column: 91
                }
            ))
        );
    }

    #[test]
    fn tokenize_dollar_quoted_string_tagged_unterminated_mirror() {
        let sql = String::from("SELECT $abc$abc$");
        let dialect = GenericDialect {};
        assert_eq!(
            Tokenizer::new(&dialect, &sql).tokenize(),
            Err(TokenizerError::new(
                "Unterminated dollar-quoted, expected $",
                Location {
                    line: 1,
                    column: 17
                }
            ))
        );
    }

    #[test]
    fn tokenize_nested_dollar_quoted_strings() -> anyhow::Result<()> {
        let sql = String::from("SELECT $tag$dollar $nested$ string$tag$");
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;
        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::DollarQuotedString(DollarQuotedString {
                value: "dollar $nested$ string".into(),
                tag: Some("tag".into()),
            }),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_dollar_quoted_string_untagged_empty() -> anyhow::Result<()> {
        let sql = String::from("SELECT $$$$");
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;
        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::DollarQuotedString(DollarQuotedString {
                value: "".into(),
                tag: None,
            }),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_dollar_quoted_string_untagged() -> anyhow::Result<()> {
        let sql =
            String::from("SELECT $$within dollar '$' quoted strings have $tags like this$ $$");
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;
        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::DollarQuotedString(DollarQuotedString {
                value: "within dollar '$' quoted strings have $tags like this$ ".into(),
                tag: None,
            }),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_dollar_quoted_string_untagged_unterminated() {
        let sql = String::from(
            "SELECT $$dollar '$' quoted strings have $tags like this$ or like this $different tag$",
        );
        let dialect = GenericDialect {};
        assert_eq!(
            Tokenizer::new(&dialect, &sql).tokenize(),
            Err(TokenizerError::new(
                "Unterminated dollar-quoted string",
                Location {
                    line: 1,
                    column: 86
                }
            ))
        );
    }

    #[test]
    fn tokenize_right_arrow() -> anyhow::Result<()> {
        let sql = String::from("FUNCTION(key=>value)");
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;
        let expected = vec![
            Token::make_word("FUNCTION", None),
            Token::LParen,
            Token::make_word("key", None),
            Token::RArrow,
            Token::make_word("value", None),
            Token::RParen,
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_is_null() -> anyhow::Result<()> {
        let sql = String::from("a IS NULL");
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;

        let expected = vec![
            Token::make_word("a", None),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("IS"),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("NULL"),
        ];

        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_comment() -> anyhow::Result<()> {
        let test_cases = vec![
            (
                String::from("0--this is a comment\n1"),
                vec![
                    Token::Number("0".to_string(), false),
                    Token::Whitespace(Whitespace::SingleLineComment {
                        prefix: "--".to_string(),
                        comment: "this is a comment\n".to_string(),
                    }),
                    Token::Number("1".to_string(), false),
                ],
            ),
            (
                String::from("0--this is a comment\r1"),
                vec![
                    Token::Number("0".to_string(), false),
                    Token::Whitespace(Whitespace::SingleLineComment {
                        prefix: "--".to_string(),
                        comment: "this is a comment\r1".to_string(),
                    }),
                ],
            ),
            (
                String::from("0--this is a comment\r\n1"),
                vec![
                    Token::Number("0".to_string(), false),
                    Token::Whitespace(Whitespace::SingleLineComment {
                        prefix: "--".to_string(),
                        comment: "this is a comment\r\n".to_string(),
                    }),
                    Token::Number("1".to_string(), false),
                ],
            ),
        ];

        let dialect = GenericDialect {};

        for (sql, expected) in test_cases {
            let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;
            compare(expected, tokens);
        }

        Ok(())
    }

    #[test]
    fn tokenize_comment_postgres() -> anyhow::Result<()> {
        let sql = String::from("1--\r0");

        let dialect = PostgreSqlDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;
        let expected = vec![
            Token::Number("1".to_string(), false),
            Token::Whitespace(Whitespace::SingleLineComment {
                prefix: "--".to_string(),
                comment: "\r".to_string(),
            }),
            Token::Number("0".to_string(), false),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_comment_at_eof() -> anyhow::Result<()> {
        let sql = String::from("--this is a comment");

        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;
        let expected = vec![Token::Whitespace(Whitespace::SingleLineComment {
            prefix: "--".to_string(),
            comment: "this is a comment".to_string(),
        })];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_multiline_comment() -> anyhow::Result<()> {
        let sql = String::from("0/*multi-line\n* /comment*/1");

        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;
        let expected = vec![
            Token::Number("0".to_string(), false),
            Token::Whitespace(Whitespace::MultiLineComment(
                "multi-line\n* /comment".to_string(),
            )),
            Token::Number("1".to_string(), false),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_nested_multiline_comment() {
        all_dialects_where(|d| d.supports_nested_comments()).tokenizes_to(
            "0/*multi-line\n* \n/* comment \n /*comment*/*/ */ /comment*/1",
            vec![
                Token::Number("0".to_string(), false),
                Token::Whitespace(Whitespace::MultiLineComment(
                    "multi-line\n* \n/* comment \n /*comment*/*/ ".into(),
                )),
                Token::Whitespace(Whitespace::Space),
                Token::Div,
                Token::Word(Word {
                    value: "comment".to_string(),
                    quote_style: None,
                    keyword: Keyword::COMMENT,
                }),
                Token::Mul,
                Token::Div,
                Token::Number("1".to_string(), false),
            ],
        );

        all_dialects_where(|d| d.supports_nested_comments()).tokenizes_to(
            "0/*multi-line\n* \n/* comment \n /*comment/**/ */ /comment*/*/1",
            vec![
                Token::Number("0".to_string(), false),
                Token::Whitespace(Whitespace::MultiLineComment(
                    "multi-line\n* \n/* comment \n /*comment/**/ */ /comment*/".into(),
                )),
                Token::Number("1".to_string(), false),
            ],
        );

        all_dialects_where(|d| d.supports_nested_comments()).tokenizes_to(
            "SELECT 1/* a /* b */ c */0",
            vec![
                Token::make_keyword("SELECT"),
                Token::Whitespace(Whitespace::Space),
                Token::Number("1".to_string(), false),
                Token::Whitespace(Whitespace::MultiLineComment(" a /* b */ c ".to_string())),
                Token::Number("0".to_string(), false),
            ],
        );
    }

    #[test]
    fn tokenize_nested_multiline_comment_empty() {
        all_dialects_where(|d| d.supports_nested_comments()).tokenizes_to(
            "select 1/*/**/*/0",
            vec![
                Token::make_keyword("select"),
                Token::Whitespace(Whitespace::Space),
                Token::Number("1".to_string(), false),
                Token::Whitespace(Whitespace::MultiLineComment("/**/".to_string())),
                Token::Number("0".to_string(), false),
            ],
        );
    }

    #[test]
    fn tokenize_multiline_comment_with_even_asterisks() -> anyhow::Result<()> {
        let sql = String::from("\n/** Comment **/\n");

        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;
        let expected = vec![
            Token::Whitespace(Whitespace::Newline),
            Token::Whitespace(Whitespace::MultiLineComment("* Comment *".to_string())),
            Token::Whitespace(Whitespace::Newline),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_unicode_whitespace() -> anyhow::Result<()> {
        let sql = String::from(" \u{2003}\n");

        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;
        let expected = vec![
            Token::Whitespace(Whitespace::Space),
            Token::Whitespace(Whitespace::Space),
            Token::Whitespace(Whitespace::Newline),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_mismatched_quotes() {
        let sql = String::from("\"foo");

        let dialect = GenericDialect {};
        let mut tokenizer = Tokenizer::new(&dialect, &sql);
        assert_eq!(
            tokenizer.tokenize(),
            Err(TokenizerError::new(
                "Expected close delimiter '\"' before EOF.",
                Location { line: 1, column: 1 },
            ))
        );
    }

    #[test]
    fn tokenize_newlines() -> anyhow::Result<()> {
        let sql = String::from("line1\nline2\rline3\r\nline4\r");

        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;
        let expected = vec![
            Token::make_word("line1", None),
            Token::Whitespace(Whitespace::Newline),
            Token::make_word("line2", None),
            Token::Whitespace(Whitespace::Newline),
            Token::make_word("line3", None),
            Token::Whitespace(Whitespace::Newline),
            Token::make_word("line4", None),
            Token::Whitespace(Whitespace::Newline),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_pg_regex_match() -> anyhow::Result<()> {
        let sql = "SELECT col ~ '^a', col ~* '^a', col !~ '^a', col !~* '^a'";
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, sql).tokenize()?;
        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::make_word("col", None),
            Token::Whitespace(Whitespace::Space),
            Token::Tilde,
            Token::Whitespace(Whitespace::Space),
            Token::SingleQuotedString("^a".into()),
            Token::Comma,
            Token::Whitespace(Whitespace::Space),
            Token::make_word("col", None),
            Token::Whitespace(Whitespace::Space),
            Token::TildeAsterisk,
            Token::Whitespace(Whitespace::Space),
            Token::SingleQuotedString("^a".into()),
            Token::Comma,
            Token::Whitespace(Whitespace::Space),
            Token::make_word("col", None),
            Token::Whitespace(Whitespace::Space),
            Token::ExclamationMarkTilde,
            Token::Whitespace(Whitespace::Space),
            Token::SingleQuotedString("^a".into()),
            Token::Comma,
            Token::Whitespace(Whitespace::Space),
            Token::make_word("col", None),
            Token::Whitespace(Whitespace::Space),
            Token::ExclamationMarkTildeAsterisk,
            Token::Whitespace(Whitespace::Space),
            Token::SingleQuotedString("^a".into()),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_pg_like_match() -> anyhow::Result<()> {
        let sql = "SELECT col ~~ '_a%', col ~~* '_a%', col !~~ '_a%', col !~~* '_a%'";
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, sql).tokenize()?;
        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::make_word("col", None),
            Token::Whitespace(Whitespace::Space),
            Token::DoubleTilde,
            Token::Whitespace(Whitespace::Space),
            Token::SingleQuotedString("_a%".into()),
            Token::Comma,
            Token::Whitespace(Whitespace::Space),
            Token::make_word("col", None),
            Token::Whitespace(Whitespace::Space),
            Token::DoubleTildeAsterisk,
            Token::Whitespace(Whitespace::Space),
            Token::SingleQuotedString("_a%".into()),
            Token::Comma,
            Token::Whitespace(Whitespace::Space),
            Token::make_word("col", None),
            Token::Whitespace(Whitespace::Space),
            Token::ExclamationMarkDoubleTilde,
            Token::Whitespace(Whitespace::Space),
            Token::SingleQuotedString("_a%".into()),
            Token::Comma,
            Token::Whitespace(Whitespace::Space),
            Token::make_word("col", None),
            Token::Whitespace(Whitespace::Space),
            Token::ExclamationMarkDoubleTildeAsterisk,
            Token::Whitespace(Whitespace::Space),
            Token::SingleQuotedString("_a%".into()),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_quoted_identifier() -> anyhow::Result<()> {
        let sql = r#" "a "" b" "a """ "c """"" "#;
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, sql).tokenize()?;
        let expected = vec![
            Token::Whitespace(Whitespace::Space),
            Token::make_word(r#"a " b"#, Some('"')),
            Token::Whitespace(Whitespace::Space),
            Token::make_word(r#"a ""#, Some('"')),
            Token::Whitespace(Whitespace::Space),
            Token::make_word(r#"c """#, Some('"')),
            Token::Whitespace(Whitespace::Space),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_single_slash_division() -> anyhow::Result<()> {
        let sql = r#"field/1000"#;
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, sql).tokenize()?;
        let expected = vec![
            Token::make_word(r#"field"#, None),
            Token::Div,
            Token::Number("1000".to_string(), false),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_quoted_identifier_with_no_escape() -> anyhow::Result<()> {
        let sql = r#" "a "" b" "a """ "c """"" "#;
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, sql)
            .with_unescape(false)
            .tokenize()?;
        let expected = vec![
            Token::Whitespace(Whitespace::Space),
            Token::make_word(r#"a "" b"#, Some('"')),
            Token::Whitespace(Whitespace::Space),
            Token::make_word(r#"a """#, Some('"')),
            Token::Whitespace(Whitespace::Space),
            Token::make_word(r#"c """""#, Some('"')),
            Token::Whitespace(Whitespace::Space),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn tokenize_with_location() -> anyhow::Result<()> {
        let sql = "SELECT a,\n b";
        let dialect = GenericDialect {};
        let tokens = Tokenizer::new(&dialect, sql).tokenize_with_location()?;
        let expected = vec![
            TokenWithSpan::at(Token::make_keyword("SELECT"), (1, 1).into(), (1, 7).into()),
            TokenWithSpan::at(
                Token::Whitespace(Whitespace::Space),
                (1, 7).into(),
                (1, 8).into(),
            ),
            TokenWithSpan::at(Token::make_word("a", None), (1, 8).into(), (1, 9).into()),
            TokenWithSpan::at(Token::Comma, (1, 9).into(), (1, 10).into()),
            TokenWithSpan::at(
                Token::Whitespace(Whitespace::Newline),
                (1, 10).into(),
                (2, 1).into(),
            ),
            TokenWithSpan::at(
                Token::Whitespace(Whitespace::Space),
                (2, 1).into(),
                (2, 2).into(),
            ),
            TokenWithSpan::at(Token::make_word("b", None), (2, 2).into(), (2, 3).into()),
        ];
        compare(expected, tokens);

        Ok(())
    }

    fn compare<T: PartialEq + fmt::Debug>(expected: Vec<T>, actual: Vec<T>) {
        //println!("------------------------------");
        //println!("tokens   = {:?}", actual);
        //println!("expected = {:?}", expected);
        //println!("------------------------------");
        assert_eq!(expected, actual);
    }

    fn unescape_body(s: &str) -> Result<String, TokenizerError> {
        let s = format!("'{s}'");
        let mut state = State {
            peekable: s.chars().peekable(),
            line: 1,
            col: 1,
        };
        unescape_single_quoted_string(&mut state, Location { line: 1, column: 1 })
    }

    /// `None` means "rejected" — which of the several rejections is asserted
    /// separately by [`check_unescape_err`].
    fn check_unescape(s: &str, expected: Option<&str>) {
        assert_eq!(
            unescape_body(s).ok(),
            expected.map(|s| s.to_string()),
            "input: {s}"
        );
    }

    fn check_unescape_err(s: &str, sqlstate: &str, message: &str) {
        let err = unescape_body(s).expect_err("expected a rejection");
        assert_eq!(err.sqlstate, Some(sqlstate), "input: {s}");
        assert_eq!(err.message, message, "input: {s}");
    }

    #[test]
    fn test_unescape() {
        check_unescape(r"\b", Some("\u{0008}"));
        check_unescape(r"\f", Some("\u{000C}"));
        check_unescape(r"\t", Some("\t"));
        check_unescape(r"\r\n", Some("\r\n"));
        check_unescape(r"\/", Some("/"));
        check_unescape(r"/", Some("/"));
        check_unescape(r"\\", Some("\\"));

        // 16 and 32-bit hexadecimal Unicode character value
        check_unescape(r"\u0001", Some("\u{0001}"));
        check_unescape(r"\u4c91", Some("\u{4c91}"));
        check_unescape(r"\u4c916", Some("\u{4c91}6"));
        check_unescape(r"\u4c", None);
        check_unescape(r"\u0000", None);
        check_unescape(r"\U0010FFFF", Some("\u{10FFFF}"));
        check_unescape(r"\U00110000", None);
        check_unescape(r"\U00000000", None);
        check_unescape(r"\u", None);
        check_unescape(r"\U", None);
        check_unescape(r"\U1010FFFF", None);

        // hexadecimal byte value
        check_unescape(r"\x4B", Some("\u{004b}"));
        check_unescape(r"\x4", Some("\u{0004}"));
        check_unescape(r"\x4L", Some("\u{0004}L"));
        check_unescape(r"\x", Some("x"));
        check_unescape(r"\xP", Some("xP"));
        check_unescape(r"\x0", None);
        check_unescape(r"\xCAD", None);
        check_unescape(r"\xA9", None);

        // octal byte value
        check_unescape(r"\1", Some("\u{0001}"));
        check_unescape(r"\12", Some("\u{000a}"));
        check_unescape(r"\123", Some("\u{0053}"));
        check_unescape(r"\1232", Some("\u{0053}2"));
        check_unescape(r"\4", Some("\u{0004}"));
        check_unescape(r"\45", Some("\u{0025}"));
        check_unescape(r"\450", Some("\u{0028}"));
        check_unescape(r"\603", None);
        check_unescape(r"\0", None);
        check_unescape(r"\080", None);

        // others
        check_unescape(r"\9", Some("9"));
        check_unescape(r"''", Some("'"));
        check_unescape(
            r"Hello\r\nRust/\u4c91 SQL Parser\U0010ABCD\1232",
            Some("Hello\r\nRust/\u{4c91} SQL Parser\u{10abcd}\u{0053}2"),
        );
        check_unescape(r"Hello\0", None);
        check_unescape(r"Hello\xCADRust", None);
    }

    /// Behaviour checked against PostgreSQL 18.4. `E'…'` escapes decode to
    /// *bytes*, so several escapes above 0x7F combine into one character and a
    /// lone one is an encoding error rather than a lexer quirk.
    #[test]
    fn test_unescape_byte_semantics() {
        // Multi-byte characters spelled out one byte at a time.
        check_unescape(r"\xc3\xa9", Some("é"));
        check_unescape(r"\xe4\xb8\xad", Some("中"));
        check_unescape(r"\303\251", Some("é"));
        // …and written directly.
        check_unescape("é", Some("é"));
        check_unescape("😄", Some("😄"));

        // TODO: assert the UTF-16 pair spelling `\ud83d\ude04` decodes here to
        // the same emoji; only its rejections are covered below.
        check_unescape(r"😄", Some("😄"));
        check_unescape(r"\U0001F604", Some("😄"));

        // A backslash before anything else simply drops out.
        check_unescape(r"\z", Some("z"));
        check_unescape(r"\'", Some("'"));

        // An encoding error names the bytes PG names: the length the lead byte
        // promises, clipped to what is actually present.
        for (bad, bytes) in [
            (r"\x80", "0x80"),
            (r"\xc3", "0xc3"),
            (r"\xCAD", "0xca 0x44"),
            (r"\xe4\xb8", "0xe4 0xb8"),
            (r"\xe4\xb8\x41", "0xe4 0xb8 0x41"),
            (r"\xf0\x9f\x98", "0xf0 0x9f 0x98"),
        ] {
            check_unescape_err(
                bad,
                "22021",
                &format!("invalid byte sequence for encoding \"UTF8\": {bytes}"),
            );
        }
        // NUL is valid UTF-8 but PG still refuses it. A *byte* escape that
        // yields zero is an encoding error — `\400` overflows to 0x00 rather
        // than erroring on the octal value…
        for nul in [r"\0", r"\000", r"\400", r"\x00"] {
            check_unescape_err(
                nul,
                "22021",
                "invalid byte sequence for encoding \"UTF8\": 0x00",
            );
        }
        // …whereas a *code point* escape naming zero is rejected as a bad
        // escape value, before any byte is produced.
        for (bad, token) in [(r"\u0000", r"\u0000"), (r"\U00000000", r"\U00000000")] {
            check_unescape_err(
                bad,
                "42601",
                &format!("invalid Unicode escape value at or near \"{token}\""),
            );
        }

        // Malformed `\u`/`\U`: too few hex digits, or a non-hex digit.
        for bad in [r"\uZZZZ", r"\u4c", r"\U0011"] {
            check_unescape_err(bad, "22025", "invalid Unicode escape");
        }
        // A well-formed escape naming something past the last code point is a
        // different condition from a malformed one, and carries no hint.
        check_unescape_err(
            r"\U00110000",
            "42601",
            "invalid Unicode escape value at or near \"\\U00110000\"",
        );
        // A surrogate half that is never completed. PG names the token that
        // failed to complete the pair: for a high surrogate that is whatever
        // follows it, for an unpaired low surrogate the escape itself.
        for (bad, token) in [
            (r"\ud83d", "'"),
            (r"\ud83dxyz", "x"),
            (r"\ud83dA", r"A"),
            (r"\ud83d\ud83d", r"\ud83d"),
            (r"\udc36", r"\udc36"),
        ] {
            check_unescape_err(
                bad,
                "42601",
                &format!("invalid Unicode surrogate pair at or near \"{token}\""),
            );
        }
    }

    fn uescape_body(s: &str) -> Result<String, TokenizerError> {
        let s = format!("'{s}'");
        let mut state = State {
            peekable: s.chars().peekable(),
            line: 1,
            col: 1,
        };
        unescape_unicode_single_quoted_string(&mut state)
    }

    /// `U&'…'` escapes name code points, not bytes, but carry the same value
    /// rules as their `E'…'` counterparts. Checked against PostgreSQL 18.4 —
    /// note PG does *not* quote the offending escape in these messages, unlike
    /// the `E'…'` ones.
    #[test]
    fn test_uescape_values() {
        assert_eq!(uescape_body(r"d\0061t\+000061").as_deref(), Ok("data"));
        // A surrogate pair combines, exactly as in E'…'.
        assert_eq!(uescape_body(r"\D83D\DE04").as_deref(), Ok("😄"));
        assert_eq!(uescape_body(r"\+01F604").as_deref(), Ok("😄"));

        let err = |s: &str| uescape_body(s).expect_err("expected a rejection");
        // Zero and out-of-range are escape *value* errors, and in particular a
        // NUL must never reach the resulting text.
        for bad in [r"\0000", r"a\0000b", r"\+110000"] {
            let e = err(bad);
            assert_eq!(e.message, "invalid Unicode escape value", "input: {bad}");
            assert_eq!(e.sqlstate, Some("42601"), "input: {bad}");
            assert_eq!(e.hint, None, "input: {bad}");
        }
        // A surrogate half that nothing completes.
        for bad in [r"\D83D", r"\D83DA", r"\DE04", r"\D83D\0041"] {
            let e = err(bad);
            assert_eq!(e.message, "invalid Unicode surrogate pair", "input: {bad}");
            assert_eq!(e.sqlstate, Some("42601"), "input: {bad}");
        }
        // A malformed escape keeps its own hint, naming the U& forms.
        for bad in [r"\ZZZZ", r"\00"] {
            let e = err(bad);
            assert_eq!(e.message, "invalid Unicode escape", "input: {bad}");
            assert_eq!(e.hint.as_deref(), Some(UESCAPE_HINT), "input: {bad}");
        }
    }

    /// The caret PG prints sits on the backslash that opened the bad escape.
    #[test]
    fn test_unescape_error_location() {
        let mut state = State {
            peekable: r"'ab\uZZZZ'".chars().peekable(),
            line: 1,
            col: 8,
        };
        let err = unescape_single_quoted_string(&mut state, Location { line: 1, column: 8 })
            .expect_err("invalid escape");
        // `'` at column 8, `a` 9, `b` 10, `\` 11.
        assert_eq!(
            err.location,
            Location {
                line: 1,
                column: 11
            }
        );
        assert_eq!(err.hint.as_deref(), Some(ESCAPE_UNICODE_HINT));
    }

    #[test]
    fn tokenize_numeric_prefix_trait() {
        #[derive(Debug)]
        struct NumericPrefixDialect;

        impl Dialect for NumericPrefixDialect {
            fn is_identifier_start(&self, ch: char) -> bool {
                ch.is_ascii_lowercase()
                    || ch.is_ascii_uppercase()
                    || ch.is_ascii_digit()
                    || ch == '$'
            }

            fn is_identifier_part(&self, ch: char) -> bool {
                ch.is_ascii_lowercase()
                    || ch.is_ascii_uppercase()
                    || ch.is_ascii_digit()
                    || ch == '_'
                    || ch == '$'
                    || ch == '{'
                    || ch == '}'
            }

            fn supports_numeric_prefix(&self) -> bool {
                true
            }
        }

        tokenize_numeric_prefix_inner(&NumericPrefixDialect {});
    }

    fn tokenize_numeric_prefix_inner(dialect: &dyn Dialect) {
        let sql = r#"SELECT * FROM 1"#;
        let tokens = match Tokenizer::new(dialect, sql).tokenize() {
            Ok(tokens) => tokens,
            Err(error) => panic!("failed to tokenize numeric-prefix test fixture: {error}"),
        };
        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::Mul,
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("FROM"),
            Token::Whitespace(Whitespace::Space),
            Token::Number(String::from("1"), false),
        ];
        compare(expected, tokens);
    }

    #[test]
    fn test_postgres_abs_without_space_and_string_literal() -> anyhow::Result<()> {
        let dialect = PostgreSqlDialect {};

        let sql = "SELECT @'1'";
        let tokens = Tokenizer::new(&dialect, sql).tokenize()?;
        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::AtSign,
            Token::SingleQuotedString("1".to_string()),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn test_postgres_abs_without_space_and_quoted_column() -> anyhow::Result<()> {
        let dialect = PostgreSqlDialect {};

        let sql = r#"SELECT @"bar" FROM foo"#;
        let tokens = Tokenizer::new(&dialect, sql).tokenize()?;
        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::AtSign,
            Token::make_word("bar", Some('"')),
            Token::Whitespace(Whitespace::Space),
            Token::make_keyword("FROM"),
            Token::Whitespace(Whitespace::Space),
            Token::make_word("foo", None),
        ];
        compare(expected, tokens);

        Ok(())
    }

    #[test]
    fn test_national_strings_backslash_escape_not_supported() {
        all_dialects_where(|dialect| !dialect.supports_string_literal_backslash_escape())
            .tokenizes_to(
                "select n'''''\\'",
                vec![
                    Token::make_keyword("select"),
                    Token::Whitespace(Whitespace::Space),
                    Token::NationalStringLiteral("''\\".to_string()),
                ],
            );
    }

    #[test]
    fn test_string_escape_constant_supported() {
        all_dialects_where(|dialect| dialect.supports_string_escape_constant()).tokenizes_to(
            "select e'\\''",
            vec![
                Token::make_keyword("select"),
                Token::Whitespace(Whitespace::Space),
                Token::EscapedStringLiteral("'".to_string()),
            ],
        );

        all_dialects_where(|dialect| dialect.supports_string_escape_constant()).tokenizes_to(
            "select E'\\''",
            vec![
                Token::make_keyword("select"),
                Token::Whitespace(Whitespace::Space),
                Token::EscapedStringLiteral("'".to_string()),
            ],
        );
    }

    #[test]
    fn test_whitespace_not_required_after_single_line_comment() {
        all_dialects_where(|dialect| !dialect.requires_single_line_comment_whitespace())
            .tokenizes_to(
                "SELECT --'abc'",
                vec![
                    Token::make_keyword("SELECT"),
                    Token::Whitespace(Whitespace::Space),
                    Token::Whitespace(Whitespace::SingleLineComment {
                        prefix: "--".to_string(),
                        comment: "'abc'".to_string(),
                    }),
                ],
            );

        all_dialects_where(|dialect| !dialect.requires_single_line_comment_whitespace())
            .tokenizes_to(
                "SELECT -- 'abc'",
                vec![
                    Token::make_keyword("SELECT"),
                    Token::Whitespace(Whitespace::Space),
                    Token::Whitespace(Whitespace::SingleLineComment {
                        prefix: "--".to_string(),
                        comment: " 'abc'".to_string(),
                    }),
                ],
            );

        all_dialects_where(|dialect| !dialect.requires_single_line_comment_whitespace())
            .tokenizes_to(
                "SELECT --",
                vec![
                    Token::make_keyword("SELECT"),
                    Token::Whitespace(Whitespace::Space),
                    Token::Whitespace(Whitespace::SingleLineComment {
                        prefix: "--".to_string(),
                        comment: "".to_string(),
                    }),
                ],
            );
    }

    #[test]
    fn tokenize_period_underscore() -> anyhow::Result<()> {
        let sql = String::from("SELECT table._col");
        // a dialect that supports underscores in numeric literals
        let dialect = PostgreSqlDialect {};
        let tokens = Tokenizer::new(&dialect, &sql).tokenize()?;

        let expected = vec![
            Token::make_keyword("SELECT"),
            Token::Whitespace(Whitespace::Space),
            Token::Word(Word {
                value: "table".to_string(),
                quote_style: None,
                keyword: Keyword::TABLE,
            }),
            Token::Period,
            Token::Word(Word {
                value: "_col".to_string(),
                quote_style: None,
                keyword: Keyword::NoKeyword,
            }),
        ];

        compare(expected, tokens);

        let sql = String::from("SELECT ._123");
        if let Ok(tokens) = Tokenizer::new(&dialect, &sql).tokenize() {
            panic!("Tokenizer should have failed on {sql}, but it succeeded with {tokens:?}");
        }

        let sql = String::from("SELECT ._abc");
        if let Ok(tokens) = Tokenizer::new(&dialect, &sql).tokenize() {
            panic!("Tokenizer should have failed on {sql}, but it succeeded with {tokens:?}");
        }

        Ok(())
    }

    #[test]
    fn tokenize_question_mark() -> anyhow::Result<()> {
        let dialect = PostgreSqlDialect {};
        let sql = "SELECT x ? y";
        let tokens = Tokenizer::new(&dialect, sql).tokenize()?;
        compare(
            tokens,
            vec![
                Token::make_keyword("SELECT"),
                Token::Whitespace(Whitespace::Space),
                Token::make_word("x", None),
                Token::Whitespace(Whitespace::Space),
                Token::Question,
                Token::Whitespace(Whitespace::Space),
                Token::make_word("y", None),
            ],
        );

        Ok(())
    }

    #[test]
    fn tokenize_lt() {
        all_dialects().tokenizes_to(
            "select a <-50",
            vec![
                Token::make_keyword("select"),
                Token::Whitespace(Whitespace::Space),
                Token::make_word("a", None),
                Token::Whitespace(Whitespace::Space),
                Token::Lt,
                Token::Minus,
                Token::Number("50".to_string(), false),
            ],
        );
        all_dialects().tokenizes_to(
            "select a <+50",
            vec![
                Token::make_keyword("select"),
                Token::Whitespace(Whitespace::Space),
                Token::make_word("a", None),
                Token::Whitespace(Whitespace::Space),
                Token::Lt,
                Token::Plus,
                Token::Number("50".to_string(), false),
            ],
        );
        all_dialects().tokenizes_to(
            "select a <=-50",
            vec![
                Token::make_keyword("select"),
                Token::Whitespace(Whitespace::Space),
                Token::make_word("a", None),
                Token::Whitespace(Whitespace::Space),
                Token::LtEq,
                Token::Minus,
                Token::Number("50".to_string(), false),
            ],
        );
        all_dialects().tokenizes_to(
            "select a <=+50",
            vec![
                Token::make_keyword("select"),
                Token::Whitespace(Whitespace::Space),
                Token::make_word("a", None),
                Token::Whitespace(Whitespace::Space),
                Token::LtEq,
                Token::Plus,
                Token::Number("50".to_string(), false),
            ],
        );
        all_dialects_where(|d| d.supports_geometric_types()).tokenizes_to(
            "select a <->b",
            vec![
                Token::make_keyword("select"),
                Token::Whitespace(Whitespace::Space),
                Token::make_word("a", None),
                Token::Whitespace(Whitespace::Space),
                Token::TwoWayArrow,
                Token::make_word("b", None),
            ],
        );

        all_dialects().tokenizes_to(
            "select a <-b",
            vec![
                Token::make_keyword("select"),
                Token::Whitespace(Whitespace::Space),
                Token::make_word("a", None),
                Token::Whitespace(Whitespace::Space),
                Token::Lt,
                Token::Minus,
                Token::make_word("b", None),
            ],
        );
        all_dialects().tokenizes_to(
            "select a <+b",
            vec![
                Token::make_keyword("select"),
                Token::Whitespace(Whitespace::Space),
                Token::make_word("a", None),
                Token::Whitespace(Whitespace::Space),
                Token::Lt,
                Token::Plus,
                Token::make_word("b", None),
            ],
        );
    }
}
