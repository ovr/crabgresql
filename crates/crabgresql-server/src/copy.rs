//! COPY FROM STDIN text/CSV row decoding.
//!
//! The wire layer hands us the raw bytes streamed in `CopyData` frames; this
//! module splits them into logical rows of field strings per a resolved
//! [`CopyFormat`] — text-format backslash escapes and the `\N` NULL marker, or
//! CSV quoting with `""` doubling. It never parses values into a type: that is
//! [`crabgresql_binder::CopyFromPlan::build_insert`]'s job, which runs each
//! field through the column's input function. `None` marks a field that matched
//! the NULL representation.
//!
//! Reproduces PostgreSQL's observable text/CSV COPY behavior (see the COPY docs)
//! rather than porting its C reader.

use crabgresql_binder::CopyFormat;
use crabgresql_pg_wire::sqlstate;

use crate::error::PgError;

/// A single decoded field: `Some(text)` (the de-escaped/unquoted contents) or
/// `None` when the field matched the format's NULL representation.
type Field = Option<String>;

fn bad_copy(message: impl Into<String>) -> PgError {
    PgError::new(sqlstate::BAD_COPY_FILE_FORMAT, message)
}

/// Decode the full stdin byte stream into rows of fields.
///
/// The bytes must be valid UTF-8 (COPY here only speaks UTF-8). A trailing `\.`
/// line (text format's end-of-data marker) and the empty segment after a final
/// newline are dropped. `HEADER` skips the first data line.
pub fn decode(format: &CopyFormat, bytes: &[u8]) -> Result<Vec<Vec<Field>>, PgError> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        PgError::new(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            "invalid byte sequence for encoding \"UTF8\"",
        )
    })?;
    if format.csv {
        decode_csv(format, text)
    } else {
        decode_text(format, text)
    }
}

/// Split a stream into logical lines on `\n`, stripping a single trailing `\r`
/// from each (CRLF), and dropping the empty trailing segment left by a final
/// newline. Used for text format, where a newline always ends a row.
fn logical_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    // A terminating '\n' yields a trailing "" that is not its own row.
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
        .into_iter()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

fn decode_text(format: &CopyFormat, text: &str) -> Result<Vec<Vec<Field>>, PgError> {
    let delimiter = format.delimiter;
    let mut rows = Vec::new();
    let mut skip_header = format.header;
    for line in logical_lines(text) {
        // The text end-of-data marker `\.` terminates the stream.
        if line == "\\." {
            break;
        }
        if skip_header {
            skip_header = false;
            continue;
        }
        rows.push(decode_text_line(line, delimiter, &format.null)?);
    }
    Ok(rows)
}

/// Split one text line into fields on unescaped delimiters, then map the NULL
/// marker and translate backslash escapes.
fn decode_text_line(line: &str, delimiter: char, null: &str) -> Result<Vec<Field>, PgError> {
    let mut fields = Vec::new();
    let mut raw = String::new();
    let mut chars = line.chars().peekable();
    // Split on unescaped delimiters, carrying the raw (still-escaped) field so
    // the NULL comparison sees `\N` as written.
    loop {
        match chars.next() {
            None => {
                fields.push(finish_text_field(&raw, null)?);
                break;
            }
            Some('\\') => {
                raw.push('\\');
                // Keep the escaped char attached so a `\<delim>` never splits.
                if let Some(c) = chars.next() {
                    raw.push(c);
                }
            }
            Some(c) if c == delimiter => {
                fields.push(finish_text_field(&raw, null)?);
                raw.clear();
            }
            Some(c) => raw.push(c),
        }
    }
    Ok(fields)
}

/// Resolve one raw text field: the NULL marker (compared before de-escaping) or
/// the de-escaped contents.
fn finish_text_field(raw: &str, null: &str) -> Result<Field, PgError> {
    if raw == null {
        return Ok(None);
    }
    Ok(Some(unescape_text(raw)?))
}

/// Translate PostgreSQL text-format backslash escapes.
fn unescape_text(raw: &str) -> Result<String, PgError> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(esc) = chars.next() else {
            // A trailing backslash is a literal backslash.
            out.push('\\');
            break;
        };
        match esc {
            'b' => out.push('\u{08}'),
            'f' => out.push('\u{0C}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'v' => out.push('\u{0B}'),
            'x' => {
                // `\xHH`: one or two hex digits.
                let mut val: u32 = 0;
                let mut seen = 0;
                while seen < 2 {
                    match chars.peek().and_then(|d| d.to_digit(16)) {
                        Some(d) => {
                            val = val * 16 + d;
                            chars.next();
                            seen += 1;
                        }
                        None => break,
                    }
                }
                if seen == 0 {
                    // `\x` with no hex digit is a literal `x`, as in PG.
                    out.push('x');
                } else {
                    out.push(byte_to_char(val as u8));
                }
            }
            '0'..='7' => {
                // `\NNN`: up to three octal digits, including the first (which
                // the `'0'..='7'` arm guarantees is a valid octal digit).
                let mut val: u32 = esc.to_digit(8).unwrap_or(0);
                let mut seen = 1;
                while seen < 3 {
                    match chars.peek().and_then(|d| d.to_digit(8)) {
                        Some(d) => {
                            val = val * 8 + d;
                            chars.next();
                            seen += 1;
                        }
                        None => break,
                    }
                }
                out.push(byte_to_char((val & 0xFF) as u8));
            }
            // Any other escaped character is itself (`\\` → `\`, `\d` → `d`).
            other => out.push(other),
        }
    }
    Ok(out)
}

/// Map a decoded byte value back to a `char`. Values ≥ 0x80 stand for a raw
/// byte; we surface them as the corresponding Latin-1 code point so the string
/// round-trips through UTF-8 without loss for the common ASCII escapes.
fn byte_to_char(b: u8) -> char {
    b as char
}

fn decode_csv(format: &CopyFormat, text: &str) -> Result<Vec<Vec<Field>>, PgError> {
    let delimiter = format.delimiter;
    let quote = format.quote;
    let escape = format.escape;
    let mut rows: Vec<Vec<Field>> = Vec::new();
    let mut row: Vec<Field> = Vec::new();
    let mut field = String::new();
    let mut quoted = false; // currently inside a quoted field
    let mut was_quoted = false; // this field had a quoted section (never NULL)
    let mut field_started = false; // any char seen for the current field
    let mut chars = text.chars().peekable();

    // Finish the current field, applying the NULL rule (unquoted match only).
    let finish_field =
        |field: &mut String, was_quoted: &mut bool, row: &mut Vec<Field>, force_not_null: bool| {
            let is_null = !*was_quoted && !force_not_null && field.as_str() == format.null;
            row.push(if is_null {
                None
            } else {
                Some(std::mem::take(field))
            });
            field.clear();
            *was_quoted = false;
        };

    while let Some(c) = chars.next() {
        if quoted {
            if c == escape {
                // An escape before a quote/escape emits that literal char. When
                // escape == quote (the default), a doubled quote is one quote and
                // a lone quote ends the field — handled below.
                if escape != quote {
                    if let Some(&n) = chars.peek()
                        && (n == quote || n == escape)
                    {
                        field.push(n);
                        chars.next();
                        continue;
                    }
                    field.push(c);
                    continue;
                }
                // escape == quote: peek to distinguish `""` (literal) from close.
                if chars.peek() == Some(&quote) {
                    field.push(quote);
                    chars.next();
                } else {
                    quoted = false;
                }
                continue;
            }
            if c == quote {
                // Reachable only when escape != quote: a bare closing quote.
                quoted = false;
                continue;
            }
            field.push(c);
            continue;
        }

        // Not inside quotes.
        if c == quote {
            if field_started && !field.is_empty() {
                // A quote in the middle of an unquoted field is a data error in
                // PG's CSV reader.
                return Err(bad_copy("unquoted quote character in CSV field"));
            }
            quoted = true;
            was_quoted = true;
            field_started = true;
            continue;
        }
        if c == delimiter {
            let force = force_not_null_at(format, row.len());
            finish_field(&mut field, &mut was_quoted, &mut row, force);
            field_started = false;
            continue;
        }
        if c == '\n' || c == '\r' {
            // Consume a CRLF pair as one row terminator.
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            let force = force_not_null_at(format, row.len());
            finish_field(&mut field, &mut was_quoted, &mut row, force);
            rows.push(std::mem::take(&mut row));
            field_started = false;
            continue;
        }
        field.push(c);
        field_started = true;
    }

    if quoted {
        return Err(bad_copy("unterminated CSV quoted field"));
    }
    // A final row without a trailing newline still counts (unless the whole
    // stream was empty / ended exactly on a newline).
    if field_started || !field.is_empty() || !row.is_empty() {
        let force = force_not_null_at(format, row.len());
        finish_field(&mut field, &mut was_quoted, &mut row, force);
        rows.push(row);
    }

    if format.header && !rows.is_empty() {
        rows.remove(0);
    }
    Ok(rows)
}

fn force_not_null_at(format: &CopyFormat, field_index: usize) -> bool {
    format.force_not_null.contains(&field_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_format() -> CopyFormat {
        // Mirrors CopyFormat::text() defaults (that constructor is private to
        // the binder, so build the equivalent here via the public fields).
        CopyFormat {
            csv: false,
            delimiter: '\t',
            null: "\\N".to_string(),
            header: false,
            quote: '"',
            escape: '"',
            force_not_null: Vec::new(),
        }
    }

    fn csv_format() -> CopyFormat {
        CopyFormat {
            csv: true,
            delimiter: ',',
            null: String::new(),
            header: false,
            quote: '"',
            escape: '"',
            force_not_null: Vec::new(),
        }
    }

    #[test]
    fn text_basic_tab_rows() {
        let rows = decode(&text_format(), b"1\thello\n2\tworld\n").unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Some("1".into()), Some("hello".into())],
                vec![Some("2".into()), Some("world".into())],
            ]
        );
    }

    #[test]
    fn text_null_marker_and_empty_field() {
        let rows = decode(&text_format(), b"1\t\\N\n2\t\n").unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Some("1".into()), None],
                vec![Some("2".into()), Some(String::new())],
            ]
        );
    }

    #[test]
    fn text_escapes() {
        let rows = decode(&text_format(), b"a\\tb\tc\\\\d\te\\061f\n").unwrap();
        // \t -> tab, \\ -> backslash, \061 (octal) -> '1'
        assert_eq!(
            rows,
            vec![vec![
                Some("a\tb".into()),
                Some("c\\d".into()),
                Some("e1f".into()),
            ]]
        );
    }

    #[test]
    fn text_escaped_delimiter_does_not_split() {
        let rows = decode(&text_format(), b"a\\\tb\n").unwrap();
        assert_eq!(rows, vec![vec![Some("a\tb".into())]]);
    }

    #[test]
    fn text_end_of_data_marker_and_no_trailing_newline() {
        let rows = decode(&text_format(), b"1\ta\n\\.\n").unwrap();
        assert_eq!(rows, vec![vec![Some("1".into()), Some("a".into())]]);
        let rows = decode(&text_format(), b"9\tz").unwrap();
        assert_eq!(rows, vec![vec![Some("9".into()), Some("z".into())]]);
    }

    #[test]
    fn text_header_skips_first_line() {
        let mut fmt = text_format();
        fmt.header = true;
        let rows = decode(&fmt, b"a\tb\n1\tx\n").unwrap();
        assert_eq!(rows, vec![vec![Some("1".into()), Some("x".into())]]);
    }

    #[test]
    fn csv_quoting_and_doubling() {
        let rows = decode(&csv_format(), b"1,\"a,b\",\"she \"\"said\"\"\"\n").unwrap();
        assert_eq!(
            rows,
            vec![vec![
                Some("1".into()),
                Some("a,b".into()),
                Some("she \"said\"".into()),
            ]]
        );
    }

    #[test]
    fn csv_empty_is_null_but_quoted_empty_is_text() {
        let rows = decode(&csv_format(), b"1,,\"\"\n").unwrap();
        assert_eq!(
            rows,
            vec![vec![Some("1".into()), None, Some(String::new())]]
        );
    }

    #[test]
    fn csv_embedded_newline_in_quotes() {
        let rows = decode(&csv_format(), b"\"line1\nline2\",x\n").unwrap();
        assert_eq!(
            rows,
            vec![vec![Some("line1\nline2".into()), Some("x".into())]]
        );
    }

    #[test]
    fn csv_force_not_null_keeps_empty_as_text() {
        let mut fmt = csv_format();
        fmt.force_not_null = vec![1];
        let rows = decode(&fmt, b"1,\n").unwrap();
        assert_eq!(rows, vec![vec![Some("1".into()), Some(String::new())]]);
    }

    #[test]
    fn csv_unterminated_quote_errors() {
        let err = decode(&csv_format(), b"\"oops\n").unwrap_err();
        assert_eq!(err.code, sqlstate::BAD_COPY_FILE_FORMAT);
    }

    #[test]
    fn csv_header_skips_first_row() {
        let mut fmt = csv_format();
        fmt.header = true;
        let rows = decode(&fmt, b"a,b\n1,x\n").unwrap();
        assert_eq!(rows, vec![vec![Some("1".into()), Some("x".into())]]);
    }
}
