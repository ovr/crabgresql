//! COPY FROM STDIN text/CSV row decoding.
//!
//! The wire layer hands us the raw bytes streamed in `CopyData` frames; this
//! module splits them into logical rows of field strings per a resolved
//! [`CopyFormat`] — text-format backslash escapes and the `\N` NULL marker, or
//! CSV quoting with `""` doubling. Decoding is **byte-oriented** (as PostgreSQL's
//! COPY is): escapes produce raw bytes, multi-byte UTF-8 flows through untouched,
//! and each completed field is validated as UTF-8 only at the end — so an escaped
//! multi-byte character round-trips and an invalid byte (or NUL) errors exactly
//! as PG does. It never parses values into a type: that is
//! [`crabgresql_binder::CopyFromPlan::build_insert`]'s job. `None` marks a field
//! that matched the NULL representation.
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

/// PG's error for a byte the server encoding (UTF-8) cannot accept.
fn invalid_utf8(byte: u8) -> PgError {
    PgError::new(
        sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
        format!("invalid byte sequence for encoding \"UTF8\": 0x{byte:02x}"),
    )
}

/// Turn a completed field's raw bytes into a `String`, erroring exactly as PG
/// does on a byte sequence that is not valid UTF-8 or on an embedded NUL.
fn field_string(bytes: Vec<u8>) -> Result<String, PgError> {
    let s = String::from_utf8(bytes).map_err(|e| {
        // The first byte the decoder rejected.
        let byte = e.as_bytes()[e.utf8_error().valid_up_to()];
        invalid_utf8(byte)
    })?;
    if s.as_bytes().contains(&0) {
        return Err(invalid_utf8(0));
    }
    Ok(s)
}

/// Decode the full stdin byte stream into rows of fields.
///
/// A trailing `\.` line (text format's end-of-data marker) and the empty segment
/// after a final newline are dropped. `HEADER` skips the first data line.
pub fn decode(format: &CopyFormat, bytes: &[u8]) -> Result<Vec<Vec<Field>>, PgError> {
    if format.csv {
        decode_csv(format, bytes)
    } else {
        decode_text(format, bytes)
    }
}

/// Split a stream into logical lines on `\n`, stripping a single trailing `\r`
/// (CRLF), and dropping the empty trailing segment left by a final newline.
fn text_lines(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    // A terminating '\n' yields a trailing "" that is not its own row.
    if matches!(lines.last(), Some(l) if l.is_empty()) {
        lines.pop();
    }
    lines
        .into_iter()
        .map(|l| match l.last() {
            Some(&b'\r') => &l[..l.len() - 1],
            _ => l,
        })
        .collect()
}

fn decode_text(format: &CopyFormat, bytes: &[u8]) -> Result<Vec<Vec<Field>>, PgError> {
    let delimiter = format.delimiter;
    let null = format.null.as_bytes();
    let mut rows = Vec::new();
    let mut skip_header = format.header;
    for line in text_lines(bytes) {
        // The text end-of-data marker `\.` terminates the stream.
        if line == b"\\." {
            break;
        }
        if skip_header {
            skip_header = false;
            continue;
        }
        rows.push(decode_text_line(line, delimiter, null)?);
    }
    Ok(rows)
}

/// Split one text line into fields on unescaped delimiters, then map the NULL
/// marker (compared against the raw, still-escaped field) and de-escape.
fn decode_text_line(line: &[u8], delimiter: u8, null: &[u8]) -> Result<Vec<Field>, PgError> {
    // Split into raw field slices on unescaped delimiters: a backslash always
    // consumes the next byte, so a `\<delim>` never splits.
    let mut raw_fields: Vec<&[u8]> = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < line.len() {
        match line[i] {
            b'\\' => i += 2,
            b if b == delimiter => {
                raw_fields.push(&line[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    raw_fields.push(&line[start..]);

    let mut fields = Vec::with_capacity(raw_fields.len());
    for raw in raw_fields {
        if raw == null {
            fields.push(None);
        } else {
            fields.push(Some(field_string(unescape_text(raw))?));
        }
    }
    Ok(fields)
}

/// Translate PostgreSQL text-format backslash escapes into raw bytes.
fn unescape_text(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let b = raw[i];
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&esc) = raw.get(i) else {
            // A trailing backslash is a literal backslash.
            out.push(b'\\');
            break;
        };
        i += 1;
        match esc {
            b'b' => out.push(0x08),
            b'f' => out.push(0x0C),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0B),
            b'x' => {
                // `\xHH`: one or two hex digits.
                let mut val: u8 = 0;
                let mut seen = 0;
                while seen < 2 {
                    match raw.get(i).and_then(|d| (*d as char).to_digit(16)) {
                        Some(d) => {
                            val = val.wrapping_mul(16).wrapping_add(d as u8);
                            i += 1;
                            seen += 1;
                        }
                        None => break,
                    }
                }
                if seen == 0 {
                    // `\x` with no hex digit is a literal `x`, as in PG.
                    out.push(b'x');
                } else {
                    out.push(val);
                }
            }
            b'0'..=b'7' => {
                // `\NNN`: up to three octal digits, including the first.
                let mut val: u32 = (esc - b'0') as u32;
                let mut seen = 1;
                while seen < 3 {
                    match raw.get(i) {
                        Some(&d @ b'0'..=b'7') => {
                            val = val * 8 + (d - b'0') as u32;
                            i += 1;
                            seen += 1;
                        }
                        _ => break,
                    }
                }
                out.push((val & 0xFF) as u8);
            }
            // Any other escaped byte is itself (`\\` → `\`, `\d` → `d`).
            other => out.push(other),
        }
    }
    out
}

fn decode_csv(format: &CopyFormat, bytes: &[u8]) -> Result<Vec<Vec<Field>>, PgError> {
    let delimiter = format.delimiter;
    let quote = format.quote;
    let escape = format.escape;
    let null = format.null.as_bytes();
    let mut rows: Vec<Vec<Field>> = Vec::new();
    let mut row: Vec<Field> = Vec::new();
    let mut field: Vec<u8> = Vec::new();
    let mut in_quote = false; // currently inside a quoted section
    let mut was_quoted = false; // this field had a quoted section (never NULL)
    let mut i = 0;

    while i < bytes.len() {
        // `\.` on its own line (outside quotes, at the start of a row) is
        // end-of-data, matching the text format.
        if !in_quote && row.is_empty() && field.is_empty() && is_eod_line(bytes, i) {
            break;
        }
        let b = bytes[i];

        if in_quote {
            if b == escape {
                let next = bytes.get(i + 1).copied();
                if escape != quote {
                    // A custom escape before a quote/escape emits that literal.
                    if next == Some(quote) || next == Some(escape) {
                        field.push(next.unwrap_or(b));
                        i += 2;
                    } else {
                        field.push(b);
                        i += 1;
                    }
                    continue;
                }
                // escape == quote (the default): `""` is one quote, a lone quote
                // closes the section.
                if next == Some(quote) {
                    field.push(quote);
                    i += 2;
                } else {
                    in_quote = false;
                    i += 1;
                }
                continue;
            }
            if b == quote {
                // Reachable only when escape != quote: a bare closing quote.
                in_quote = false;
                i += 1;
                continue;
            }
            field.push(b);
            i += 1;
            continue;
        }

        // Not inside quotes. A quote (anywhere in the field) opens a quoted
        // section; PG concatenates unquoted and quoted runs within one field, so
        // `1, "x"` and `ab"cd"` are accepted (they are not errors).
        if b == quote {
            in_quote = true;
            was_quoted = true;
            i += 1;
            continue;
        }
        if b == delimiter {
            let force = force_not_null_at(format, row.len());
            finish_csv_field(&mut field, &mut was_quoted, &mut row, null, force)?;
            i += 1;
            continue;
        }
        if b == b'\n' || b == b'\r' {
            // Consume a CRLF pair as one row terminator.
            let advance = if b == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
                2
            } else {
                1
            };
            let force = force_not_null_at(format, row.len());
            finish_csv_field(&mut field, &mut was_quoted, &mut row, null, force)?;
            rows.push(std::mem::take(&mut row));
            i += advance;
            continue;
        }
        field.push(b);
        i += 1;
    }

    if in_quote {
        return Err(bad_copy("unterminated CSV quoted field"));
    }
    // A final row without a trailing newline still counts (unless the whole
    // stream was empty / ended exactly on a newline / at a `\.` marker).
    if was_quoted || !field.is_empty() || !row.is_empty() {
        let force = force_not_null_at(format, row.len());
        finish_csv_field(&mut field, &mut was_quoted, &mut row, null, force)?;
        rows.push(row);
    }

    if format.header && !rows.is_empty() {
        rows.remove(0);
    }
    Ok(rows)
}

/// Finish the current CSV field, applying the NULL rule (an unquoted match only)
/// and validating the bytes as UTF-8.
fn finish_csv_field(
    field: &mut Vec<u8>,
    was_quoted: &mut bool,
    row: &mut Vec<Field>,
    null: &[u8],
    force_not_null: bool,
) -> Result<(), PgError> {
    let is_null = !*was_quoted && !force_not_null && field.as_slice() == null;
    let taken = std::mem::take(field);
    row.push(if is_null {
        None
    } else {
        Some(field_string(taken)?)
    });
    *was_quoted = false;
    Ok(())
}

/// Whether `bytes[i..]` begins a lone `\.` end-of-data line.
fn is_eod_line(bytes: &[u8], i: usize) -> bool {
    let rest = &bytes[i..];
    rest.starts_with(b"\\.") && matches!(rest.get(2), None | Some(b'\n') | Some(b'\r'))
}

fn force_not_null_at(format: &CopyFormat, field_index: usize) -> bool {
    format.force_not_null.contains(&field_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_format() -> CopyFormat {
        CopyFormat::text()
    }

    fn csv_format() -> CopyFormat {
        CopyFormat::csv()
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
    fn text_octal_and_hex_escapes_form_multibyte_utf8() {
        // The three UTF-8 bytes of 日 written as octal, and é as hex — each must
        // round-trip to the single character (byte-oriented decode), not mojibake.
        let rows = decode(&text_format(), b"\\346\\227\\245\t\\xc3\\xa9\n").unwrap();
        assert_eq!(rows, vec![vec![Some("日".into()), Some("é".into())]]);
    }

    #[test]
    fn text_invalid_utf8_byte_errors() {
        let err = decode(&text_format(), b"\\351\n").unwrap_err();
        assert_eq!(err.code, sqlstate::CHARACTER_NOT_IN_REPERTOIRE);
        assert!(err.message.contains("0xe9"), "{}", err.message);
    }

    #[test]
    fn text_nul_byte_errors() {
        let err = decode(&text_format(), b"a\\000b\n").unwrap_err();
        assert_eq!(err.code, sqlstate::CHARACTER_NOT_IN_REPERTOIRE);
        assert!(err.message.contains("0x00"), "{}", err.message);
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
    fn csv_concatenates_unquoted_and_quoted_runs() {
        // PG accepts a quote adjacent to unquoted content, concatenating the
        // runs: `1, "two"` -> ` two`, `ab"cd"` -> `abcd`, `"a"b"c"` -> `abc`.
        let rows = decode(&csv_format(), b"1, \"two\"\nab\"cd\",\"a\"b\"c\"\n").unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Some("1".into()), Some(" two".into())],
                vec![Some("abcd".into()), Some("abc".into())],
            ]
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

    #[test]
    fn csv_end_of_data_marker() {
        let rows = decode(&csv_format(), b"1,a\n\\.\n2,b\n").unwrap();
        assert_eq!(rows, vec![vec![Some("1".into()), Some("a".into())]]);
        // A quoted `\.` is data, not a terminator.
        let rows = decode(&csv_format(), b"\"\\.\",x\n").unwrap();
        assert_eq!(rows, vec![vec![Some("\\.".into()), Some("x".into())]]);
    }
}
