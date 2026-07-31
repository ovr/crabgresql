//! psql variables: the `\set` / `\unset` / `\getenv` store and the `:var`,
//! `:'var'`, `:"var"` substitutions psql performs on statement text and on
//! backslash-command arguments.
//!
//! Behavior here was derived by probing a real psql, as the project requires:
//! an undefined variable is left in the text verbatim (not replaced with an
//! empty string), substitution never happens inside a string literal, a quoted
//! identifier, a dollar-quoted body or a comment, and `::` is a cast rather
//! than a variable because `:` is not a valid variable character.
//!
//! Two divergences from psql are deliberate:
//!
//! 1. **Substitution runs after statement splitting.** psql pushes a
//!    plain-`:var` expansion back onto its input and rescans it, so a value
//!    containing `;` splits into two statements; here it stays one. The
//!    vendored corpus never puts a `;` in a plain-`:var` value —
//!    `largeobject.sql`'s `\set dobody 'DECLARE loid oid; BEGIN '` is only ever
//!    consumed through the quoted `:'dobody'` form, which psql does not rescan
//!    either. [`substitute_is_not_rescanned`] pins the current behavior.
//! 2. **Single-quoted metacommand arguments decode only `\'` and `\\`.** psql
//!    also decodes `\n`, `\t`, `\xNN` and octal escapes there. No argument in
//!    the corpus uses those.

use std::collections::BTreeMap;

/// The `\set` variable store for one script. psql keeps a set of built-in
/// variables (`SQLSTATE`, `ERROR`, `ROW_COUNT`, `VERBOSITY`, …) that this map
/// does not pre-populate: the runner neither reports query status through them
/// nor honors the ones that change psql's output shape, so leaving them unset
/// (and therefore un-substituted) is the honest rendering.
#[derive(Debug, Default, Clone)]
pub struct Variables(BTreeMap<String, String>);

impl Variables {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    pub fn set(&mut self, name: &str, value: String) {
        self.0.insert(name.to_string(), value);
    }

    pub fn unset(&mut self, name: &str) {
        self.0.remove(name);
    }
}

/// psql's `VALID_VARIABLE_CHARS`. Notably excludes `:`, which is what keeps
/// `a::int` a cast rather than a reference to a variable named `:int`.
fn is_variable_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Quote `value` as a SQL string literal, the way `:'var'` does. Mirrors
/// libpq's `PQescapeLiteral`: `'` is doubled, and a value containing a
/// backslash switches to the `E'…'` form with backslashes doubled — including
/// the leading space psql emits so `E` cannot glue onto a preceding token.
pub fn quote_literal(value: &str) -> String {
    let escaped = value.replace('\'', "''");
    if value.contains('\\') {
        format!(" E'{}'", escaped.replace('\\', "\\\\"))
    } else {
        format!("'{escaped}'")
    }
}

/// Quote `value` as a SQL identifier, the way `:"var"` does: wrap in double
/// quotes and double any inside.
pub fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// How a `:`-reference renders its value.
#[derive(Clone, Copy, PartialEq)]
enum Quoting {
    /// `:var` — the raw value.
    Plain,
    /// `:'var'` — a SQL string literal.
    Literal,
    /// `:"var"` — a quoted identifier.
    Ident,
}

impl Quoting {
    fn apply(self, value: &str) -> String {
        match self {
            Quoting::Plain => value.to_string(),
            Quoting::Literal => quote_literal(value),
            Quoting::Ident => quote_ident(value),
        }
    }
}

/// A `:`-reference starting at `chars[i]` (which is `:`), as
/// `(quoting, name, length in chars)`. `None` when the text is not a reference
/// — a bare `:`, a `::` cast, or an unterminated `:'…` / `:"…`.
fn variable_at(chars: &[char], i: usize) -> Option<(Quoting, String, usize)> {
    let (quoting, close) = match chars.get(i + 1) {
        Some('\'') => (Quoting::Literal, Some('\'')),
        Some('"') => (Quoting::Ident, Some('"')),
        _ => (Quoting::Plain, None),
    };
    let start = if close.is_some() { i + 2 } else { i + 1 };
    let mut end = start;
    while end < chars.len() && is_variable_char(chars[end]) {
        end += 1;
    }
    if end == start {
        return None;
    }
    let name: String = chars[start..end].iter().collect();
    match close {
        // The closing quote must actually be there; `:'a` is literal text.
        Some(quote) if chars.get(end) == Some(&quote) => Some((quoting, name, end + 1 - i)),
        Some(_) => None,
        None => Some((quoting, name, end - i)),
    }
}

/// Quoting/comment context, so substitution only fires where psql's lexer would
/// be in its initial state. Mirrors the states [`crate::script`] tracks while
/// splitting statements; kept separate because this scanner only needs to know
/// whether it is inside something, not how to build a statement buffer.
#[derive(Clone, Copy, PartialEq)]
enum Context {
    Normal,
    SingleQuote { escapes: bool },
    DoubleQuote,
    DollarQuote,
    LineComment,
    BlockComment(u32),
}

/// Expand every `:var` / `:'var'` / `:"var"` in `sql` that refers to a defined
/// variable. Undefined references, and anything inside a string, identifier,
/// dollar-quoted body or comment, are copied through untouched.
pub fn substitute(sql: &str, vars: &Variables) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut context = Context::Normal;
    let mut dollar_tag = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match context {
            Context::Normal => {
                // psql lexes `::` as one typecast token, so the second colon
                // cannot open a variable reference: `1::int4` is a cast even
                // when a variable named `int4` exists.
                if c == ':' && chars.get(i + 1) == Some(&':') {
                    out.push_str("::");
                    i += 2;
                    continue;
                }
                if c == ':'
                    && let Some((quoting, name, len)) = variable_at(&chars, i)
                    && let Some(value) = vars.get(&name)
                {
                    out.push_str(&quoting.apply(value));
                    i += len;
                    continue;
                }
                if c == '-' && chars.get(i + 1) == Some(&'-') {
                    context = Context::LineComment;
                    out.push_str("--");
                    i += 2;
                    continue;
                }
                if c == '/' && chars.get(i + 1) == Some(&'*') {
                    context = Context::BlockComment(1);
                    out.push_str("/*");
                    i += 2;
                    continue;
                }
                if c == '\'' {
                    let escapes = i >= 1
                        && matches!(chars[i - 1], 'e' | 'E')
                        && (i < 2 || !is_variable_char(chars[i - 2]));
                    context = Context::SingleQuote { escapes };
                } else if c == '"' {
                    context = Context::DoubleQuote;
                } else if c == '$'
                    && let Some(len) = dollar_tag_at(&chars, i)
                {
                    dollar_tag = chars[i..i + len].iter().collect();
                    out.push_str(&dollar_tag);
                    context = Context::DollarQuote;
                    i += len;
                    continue;
                }
                out.push(c);
            }
            Context::SingleQuote { escapes } => {
                out.push(c);
                if escapes && c == '\\' {
                    if let Some(&next) = chars.get(i + 1) {
                        out.push(next);
                        i += 2;
                        continue;
                    }
                } else if c == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        out.push('\'');
                        i += 2;
                        continue;
                    }
                    context = Context::Normal;
                }
            }
            Context::DoubleQuote => {
                out.push(c);
                if c == '"' {
                    if chars.get(i + 1) == Some(&'"') {
                        out.push('"');
                        i += 2;
                        continue;
                    }
                    context = Context::Normal;
                }
            }
            Context::DollarQuote => {
                if c == '$' && starts_with_at(&chars, i, &dollar_tag) {
                    out.push_str(&dollar_tag);
                    i += dollar_tag.chars().count();
                    context = Context::Normal;
                    continue;
                }
                out.push(c);
            }
            Context::LineComment => {
                out.push(c);
                if c == '\n' {
                    context = Context::Normal;
                }
            }
            Context::BlockComment(depth) => {
                if c == '*' && chars.get(i + 1) == Some(&'/') {
                    out.push_str("*/");
                    context = match depth {
                        1 => Context::Normal,
                        d => Context::BlockComment(d - 1),
                    };
                    i += 2;
                    continue;
                }
                if c == '/' && chars.get(i + 1) == Some(&'*') {
                    out.push_str("/*");
                    context = Context::BlockComment(depth + 1);
                    i += 2;
                    continue;
                }
                out.push(c);
            }
        }
        i += 1;
    }
    out
}

/// Length of a `$tag$` opener at `chars[i]` (which is `$`), if there is one.
/// `$1` and friends are positional parameters, not quote openers.
fn dollar_tag_at(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < chars.len() && is_variable_char(chars[j]) {
        j += 1;
    }
    let is_tag =
        j < chars.len() && chars[j] == '$' && chars.get(i + 1).is_none_or(|c| !c.is_ascii_digit());
    is_tag.then(|| j - i + 1)
}

fn starts_with_at(chars: &[char], i: usize, tag: &str) -> bool {
    tag.chars()
        .enumerate()
        .all(|(k, t)| chars.get(i + k) == Some(&t))
}

/// The arguments of a backslash command, plus whatever follows the unquoted
/// backslash that ended them.
pub struct MetaArgs {
    /// One entry per whitespace-separated argument. Adjacent quoted and
    /// unquoted runs concatenate into a single argument, so `x/ 'y' :a` with
    /// `a = hi` is three arguments (`x/`, `y`, `hi`), while `x/'y'` is one.
    pub args: Vec<String>,
    /// The rest of the line starting at an unquoted `\`, which psql reads as
    /// the next backslash command. Empty when the line ended normally.
    pub rest: String,
}

/// Split a backslash command's argument list the way psql does: whitespace
/// separates arguments, single quotes group without appearing in the value,
/// `:var` forms expand, and an *unquoted* backslash terminates the list and
/// begins the next command (which is how `\set VERBOSITY sqlstate \\ -- note`
/// in `regproc.sql` hangs a comment off a `\set`).
pub fn split_args(input: &str, vars: &Variables) -> MetaArgs {
    let chars: Vec<char> = input.chars().collect();
    let mut args = Vec::new();
    let mut current = String::new();
    // Distinguishes "no argument yet" from "an argument that is the empty
    // string", which `\set x ''` needs.
    let mut started = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            break;
        }
        if c.is_whitespace() {
            if started {
                args.push(std::mem::take(&mut current));
                started = false;
            }
            i += 1;
            continue;
        }
        started = true;
        if c == '\'' {
            i += 1;
            while i < chars.len() {
                match chars[i] {
                    // As in SQL, a doubled quote is one literal quote — so
                    // `'it''s'` is a single argument, not `it` next to `s`.
                    '\'' if chars.get(i + 1) == Some(&'\'') => {
                        current.push('\'');
                        i += 2;
                    }
                    '\'' => {
                        i += 1;
                        break;
                    }
                    // Inside quotes a backslash escapes the next character
                    // rather than starting a command.
                    '\\' => {
                        current.extend(chars.get(i + 1));
                        i += 2;
                    }
                    other => {
                        current.push(other);
                        i += 1;
                    }
                }
            }
            continue;
        }
        if c == ':' && chars.get(i + 1) == Some(&':') {
            current.push_str("::");
            i += 2;
            continue;
        }
        if c == ':'
            && let Some((quoting, name, len)) = variable_at(&chars, i)
        {
            // An undefined variable stays verbatim, as in statement text.
            match vars.get(&name) {
                Some(value) => current.push_str(&quoting.apply(value)),
                None => current.extend(&chars[i..i + len]),
            }
            i += len;
            continue;
        }
        current.push(c);
        i += 1;
    }
    if started {
        args.push(current);
    }
    MetaArgs {
        args,
        rest: chars[i..].iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Variables {
        let mut vars = Variables::new();
        for (name, value) in pairs {
            vars.set(name, (*value).to_string());
        }
        vars
    }

    #[track_caller]
    fn assert_subst(pairs: &[(&str, &str)], sql: &str, expected: &str) {
        assert_eq!(substitute(sql, &vars(pairs)), expected);
    }

    #[test]
    fn substitutes_the_three_forms() {
        assert_subst(
            &[("a", "hi")],
            "SELECT :a, :'a', :\"a\";",
            "SELECT hi, 'hi', \"hi\";",
        );
    }

    #[test]
    fn undefined_variables_stay_verbatim() {
        // psql leaves the reference in place; the server then reports a syntax
        // error at the `:`. It does not expand to an empty string.
        assert_subst(&[], "SELECT :nope;", "SELECT :nope;");
        assert_subst(&[], "SELECT :'nope';", "SELECT :'nope';");
        assert_subst(&[], "SELECT :\"nope\";", "SELECT :\"nope\";");
    }

    #[test]
    fn no_substitution_inside_quotes_or_comments() {
        assert_subst(&[("a", "hi")], "SELECT ':a';", "SELECT ':a';");
        assert_subst(&[("a", "hi")], "SELECT \":a\";", "SELECT \":a\";");
        assert_subst(&[("a", "hi")], "SELECT $$:a$$;", "SELECT $$:a$$;");
        assert_subst(&[("a", "hi")], "SELECT $q$:a$q$;", "SELECT $q$:a$q$;");
        assert_subst(&[("a", "hi")], "-- :a\nSELECT :a;", "-- :a\nSELECT hi;");
        assert_subst(
            &[("a", "hi")],
            "/* :a /* :a */ */ :a",
            "/* :a /* :a */ */ hi",
        );
        assert_subst(&[("a", "hi")], r"SELECT E'\':a', :a", r"SELECT E'\':a', hi");
    }

    #[test]
    fn cast_operator_is_not_a_variable() {
        assert_subst(&[("int4", "boom")], "SELECT 1::int4;", "SELECT 1::int4;");
        // A lone colon and an unterminated quoted form are literal text too.
        assert_subst(&[("a", "hi")], "SELECT : a;", "SELECT : a;");
        assert_subst(&[("a", "hi")], "SELECT :'a;", "SELECT :'a;");
    }

    #[test]
    fn literal_quoting_matches_pqescapeliteral() {
        assert_eq!(quote_literal("it's"), "'it''s'");
        assert_eq!(quote_literal(""), "''");
        // A backslash forces the E'' form, with psql's leading space.
        assert_eq!(quote_literal(r"a\b"), r" E'a\\b'");
    }

    #[test]
    fn ident_quoting_doubles_double_quotes() {
        assert_eq!(quote_ident("he\"re"), "\"he\"\"re\"");
        assert_eq!(quote_ident(""), "\"\"");
    }

    #[test]
    fn substitute_is_not_rescanned() {
        // Divergence (1) in the module docs: psql rescans a plain expansion and
        // would split this into `SELECT 1;` and `SELECT2;`. Here the value is
        // spliced in and the statement stays whole.
        assert_subst(
            &[("semi", "1;SELECT2")],
            "SELECT :semi;",
            "SELECT 1;SELECT2;",
        );
    }

    #[test]
    fn args_split_on_whitespace_and_expand_variables() {
        let parsed = split_args(
            "filename :abs_srcdir '/data/onek.data'",
            &vars(&[("abs_srcdir", "/src")]),
        );
        assert_eq!(parsed.args, ["filename", "/src", "/data/onek.data"]);
        assert_eq!(parsed.rest, "");
    }

    #[test]
    fn adjacent_runs_concatenate_into_one_argument() {
        let parsed = split_args("p x/'y':a", &vars(&[("a", "hi")]));
        assert_eq!(parsed.args, ["p", "x/yhi"]);
    }

    #[test]
    fn quotes_group_and_escapes_survive() {
        let parsed = split_args(r"x 'it\'s' 'it''s' 'a\\b' ''", &Variables::new());
        assert_eq!(parsed.args, ["x", "it's", "it's", r"a\b", ""]);
    }

    #[test]
    fn unquoted_backslash_ends_the_argument_list() {
        // regproc.sql:108 uses this to put a comment after a `\set`.
        let parsed = split_args(
            r"VERBOSITY sqlstate \\ -- encoding-dependent",
            &Variables::new(),
        );
        assert_eq!(parsed.args, ["VERBOSITY", "sqlstate"]);
        assert_eq!(parsed.rest, r"\\ -- encoding-dependent");
    }

    #[test]
    fn undefined_variable_in_an_argument_stays_verbatim() {
        let parsed = split_args("res :nope :'nope'", &Variables::new());
        assert_eq!(parsed.args, ["res", ":nope", ":'nope'"]);
    }
}
