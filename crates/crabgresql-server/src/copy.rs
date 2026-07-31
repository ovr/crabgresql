//! COPY FROM text/CSV row decoding, for both row sources: the wire's copy-in
//! stream and a server-side file.
//!
//! The caller hands us raw bytes; this module splits them into logical rows of
//! field strings per a resolved [`CopyFormat`] — text-format backslash escapes
//! and the `\N` NULL marker, or CSV quoting with `""` doubling. Decoding is
//! **byte-oriented** (as PostgreSQL's COPY is): escapes produce raw bytes,
//! multi-byte UTF-8 flows through untouched, and each completed field is
//! validated as UTF-8 only at the end — so an escaped multi-byte character
//! round-trips and an invalid byte (or NUL) errors exactly as PG does. It never
//! parses values into a type: that is
//! [`crabgresql_binder::CopyFromPlan::build_insert`]'s job. `None` marks a field
//! that matched the NULL representation.
//!
//! [`decode`] needs whole records, so the file reader never hands it a partial
//! one: [`record_boundary`] finds where the last complete record ends and the
//! remainder is carried over to the next read. That keeps a large fixture file
//! out of memory — it is decoded and inserted in batches.
//!
//! Reproduces PostgreSQL's observable text/CSV COPY behavior (see the COPY docs)
//! rather than porting its C reader.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use crabgresql_binder::CopyFormat;
use crabgresql_pg_wire::sqlstate;

use crate::error::PgError;

/// A single decoded field: `Some(text)` (the de-escaped/unquoted contents) or
/// `None` when the field matched the format's NULL representation.
pub type Field = Option<String>;

/// Bytes pulled from a COPY source file per read. Records are expected to be far
/// smaller than this, so the carried-over partial record stays short.
const READ_CHUNK: usize = 64 * 1024;

/// Rows decoded before they are handed to the inserter. Bounds how much of a
/// file is materialized at once.
const BATCH_ROWS: usize = 1024;

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

/// The rows one slab of COPY bytes decoded to, plus whether the slab ended the
/// data stream.
#[derive(Debug)]
pub struct DecodedChunk {
    pub rows: Vec<Vec<Field>>,
    /// A `\.` end-of-data marker was reached; anything after it is not data. The
    /// file reader stops on this; the stdin path has already seen `CopyDone`.
    pub end_of_data: bool,
}

/// Decode a slab of COPY bytes into rows of fields. The slab must end on a
/// record boundary (see [`record_boundary`]) or at end of data.
///
/// A `\.` line (text format's end-of-data marker) and the empty segment after a
/// final newline are dropped. `HEADER` skips the first data line, so a caller
/// decoding a file in slabs must clear it after the first slab.
pub fn decode(format: &CopyFormat, bytes: &[u8]) -> Result<DecodedChunk, PgError> {
    if format.csv {
        decode_csv(format, bytes)
    } else {
        decode_text(format, bytes)
    }
}

/// The length of the longest prefix of `buf` that ends on a record boundary, or
/// `0` when `buf` holds no complete record.
///
/// Text format: a raw newline always ends a record (a newline *inside* a field
/// is written escaped, as `\n`). CSV: a newline inside a quoted section is data,
/// so quoting has to be tracked — but only up to the boundary, since the caller
/// re-scans the carried-over remainder from a boundary, where quoting is always
/// closed. A trailing lone `\r` is held back: the `\n` of a CRLF pair may be in
/// the next read.
pub fn record_boundary(format: &CopyFormat, buf: &[u8]) -> usize {
    if !format.csv {
        return match buf.iter().rposition(|&b| b == b'\n') {
            Some(i) => i + 1,
            None => 0,
        };
    }

    // Only record terminators and quoting move the boundary, so the delimiter
    // and NULL marker play no part here.
    let (quote, escape) = (format.quote, format.escape);
    let mut in_quote = false;
    let mut boundary = 0;
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        if in_quote {
            if b == escape {
                let next = buf.get(i + 1).copied();
                if escape != quote {
                    i += if next == Some(quote) || next == Some(escape) {
                        2
                    } else {
                        1
                    };
                } else if next == Some(quote) {
                    // `""` is one literal quote, still inside the section.
                    i += 2;
                } else if next.is_none() {
                    // Undecidable without the next read: `"` may yet be doubled.
                    break;
                } else {
                    in_quote = false;
                    i += 1;
                }
                continue;
            }
            if b == quote {
                in_quote = false;
            }
            i += 1;
            continue;
        }
        if b == quote {
            in_quote = true;
            i += 1;
            continue;
        }
        if b == b'\n' {
            i += 1;
            boundary = i;
            continue;
        }
        if b == b'\r' {
            match buf.get(i + 1) {
                Some(b'\n') => i += 2,
                // A CRLF may straddle the read; decide next time.
                None => break,
                _ => i += 1,
            }
            boundary = i;
            continue;
        }
        i += 1;
    }
    boundary
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

fn decode_text(format: &CopyFormat, bytes: &[u8]) -> Result<DecodedChunk, PgError> {
    let delimiter = format.delimiter;
    let null = format.null.as_bytes();
    let mut rows = Vec::new();
    let mut skip_header = format.header;
    let mut end_of_data = false;
    for line in text_lines(bytes) {
        // The text end-of-data marker `\.` terminates the stream.
        if line == b"\\." {
            end_of_data = true;
            break;
        }
        if skip_header {
            skip_header = false;
            continue;
        }
        rows.push(decode_text_line(line, delimiter, null)?);
    }
    Ok(DecodedChunk { rows, end_of_data })
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

fn decode_csv(format: &CopyFormat, bytes: &[u8]) -> Result<DecodedChunk, PgError> {
    let delimiter = format.delimiter;
    let quote = format.quote;
    let escape = format.escape;
    let null = format.null.as_bytes();
    let mut rows: Vec<Vec<Field>> = Vec::new();
    let mut row: Vec<Field> = Vec::new();
    let mut field: Vec<u8> = Vec::new();
    let mut in_quote = false; // currently inside a quoted section
    let mut was_quoted = false; // this field had a quoted section (never NULL)
    let mut end_of_data = false;
    let mut i = 0;

    while i < bytes.len() {
        // `\.` on its own line (outside quotes, at the start of a row) is
        // end-of-data, matching the text format.
        if !in_quote && row.is_empty() && field.is_empty() && is_eod_line(bytes, i) {
            end_of_data = true;
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
    Ok(DecodedChunk { rows, end_of_data })
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

/// Read `COPY <table> FROM '<path>'` and hand the decoded rows to `sink` in
/// batches of at most [`BATCH_ROWS`], so a large file is never held in memory in
/// full. `sink` runs inside the COPY's transaction, so an error it returns
/// aborts the whole load.
///
/// PostgreSQL resolves a relative path against the data directory and restricts
/// this form to superusers (`pg_read_server_files`). This project has no roles,
/// so the read is unconditional, and a relative path is rejected rather than
/// silently resolved against a directory the statement never named — both
/// deliberate divergences.
pub fn read_file_rows(
    path: &str,
    format: &CopyFormat,
    mut sink: impl FnMut(Vec<Vec<Field>>) -> Result<(), PgError>,
) -> Result<(), PgError> {
    if !Path::new(path).is_absolute() {
        return Err(PgError::feature_not_supported(format!(
            "COPY from a relative path is not supported yet: \"{path}\""
        )));
    }
    let file = File::open(path).map_err(|e| open_error(path, &e))?;
    let mut reader = BufReader::with_capacity(READ_CHUNK, file);

    // HEADER applies to the first data line of the file, not of every slab.
    let mut format = format.clone();
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut pending: Vec<Vec<Field>> = Vec::new();
    let mut end_of_data = false;

    while !end_of_data {
        let read = reader
            .read(&mut chunk)
            .map_err(|e| read_error(path, &e))?;
        if read == 0 {
            // End of file: whatever is left is a final record with no
            // terminator (or, for CSV, an unterminated quote decode rejects).
            if !buffer.is_empty() {
                let decoded = decode(&format, &buffer)?;
                pending.extend(decoded.rows);
            }
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        let cut = record_boundary(&format, &buffer);
        if cut == 0 {
            continue;
        }
        let decoded = decode(&format, &buffer[..cut])?;
        buffer.drain(..cut);
        format.header = false;
        end_of_data = decoded.end_of_data;
        pending.extend(decoded.rows);
        while pending.len() >= BATCH_ROWS {
            let rest = pending.split_off(BATCH_ROWS);
            sink(std::mem::replace(&mut pending, rest))?;
        }
    }

    if !pending.is_empty() {
        sink(pending)?;
    }
    Ok(())
}

/// PostgreSQL's wording for a COPY source file that could not be opened. The
/// path is quoted exactly as the statement wrote it.
fn open_error(path: &str, e: &std::io::Error) -> PgError {
    PgError::new(
        sqlstate::UNDEFINED_FILE,
        format!(
            "could not open file \"{path}\" for reading: {}",
            strerror(e)
        ),
    )
}

/// PostgreSQL's wording for a COPY source file that opened but could not be
/// read (a directory on platforms where opening one succeeds, an I/O fault).
fn read_error(path: &str, e: &std::io::Error) -> PgError {
    PgError::new(
        sqlstate::UNDEFINED_FILE,
        format!("could not read from file \"{path}\": {}", strerror(e)),
    )
}

/// The C `strerror` text PG appends via `%m`. Rust's `io::Error` Display adds a
/// ` (os error N)` suffix PG never prints, so the common codes are spelled out
/// (their values agree across the platforms we build for) and anything else
/// falls back to the Display text with that suffix trimmed.
fn strerror(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(2) => "No such file or directory".to_string(),
        Some(13) => "Permission denied".to_string(),
        Some(21) => "Is a directory".to_string(),
        _ => {
            let text = e.to_string();
            match text.find(" (os error ") {
                Some(i) => text[..i].to_string(),
                None => text,
            }
        }
    }
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
        let rows = decode(&text_format(), b"1\thello\n2\tworld\n")
            .unwrap()
            .rows;
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
        let rows = decode(&text_format(), b"1\t\\N\n2\t\n").unwrap().rows;
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
        let rows = decode(&text_format(), b"a\\tb\tc\\\\d\te\\061f\n")
            .unwrap()
            .rows;
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
        let rows = decode(&text_format(), b"\\346\\227\\245\t\\xc3\\xa9\n")
            .unwrap()
            .rows;
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
        let rows = decode(&text_format(), b"a\\\tb\n").unwrap().rows;
        assert_eq!(rows, vec![vec![Some("a\tb".into())]]);
    }

    #[test]
    fn text_end_of_data_marker_and_no_trailing_newline() {
        let rows = decode(&text_format(), b"1\ta\n\\.\n").unwrap().rows;
        assert_eq!(rows, vec![vec![Some("1".into()), Some("a".into())]]);
        let rows = decode(&text_format(), b"9\tz").unwrap().rows;
        assert_eq!(rows, vec![vec![Some("9".into()), Some("z".into())]]);
    }

    #[test]
    fn text_header_skips_first_line() {
        let mut fmt = text_format();
        fmt.header = true;
        let rows = decode(&fmt, b"a\tb\n1\tx\n").unwrap().rows;
        assert_eq!(rows, vec![vec![Some("1".into()), Some("x".into())]]);
    }

    #[test]
    fn csv_quoting_and_doubling() {
        let rows = decode(&csv_format(), b"1,\"a,b\",\"she \"\"said\"\"\"\n")
            .unwrap()
            .rows;
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
        let rows = decode(&csv_format(), b"1, \"two\"\nab\"cd\",\"a\"b\"c\"\n")
            .unwrap()
            .rows;
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
        let rows = decode(&csv_format(), b"1,,\"\"\n").unwrap().rows;
        assert_eq!(
            rows,
            vec![vec![Some("1".into()), None, Some(String::new())]]
        );
    }

    #[test]
    fn csv_embedded_newline_in_quotes() {
        let rows = decode(&csv_format(), b"\"line1\nline2\",x\n").unwrap().rows;
        assert_eq!(
            rows,
            vec![vec![Some("line1\nline2".into()), Some("x".into())]]
        );
    }

    #[test]
    fn csv_force_not_null_keeps_empty_as_text() {
        let mut fmt = csv_format();
        fmt.force_not_null = vec![1];
        let rows = decode(&fmt, b"1,\n").unwrap().rows;
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
        let rows = decode(&fmt, b"a,b\n1,x\n").unwrap().rows;
        assert_eq!(rows, vec![vec![Some("1".into()), Some("x".into())]]);
    }

    #[test]
    fn csv_end_of_data_marker() {
        let rows = decode(&csv_format(), b"1,a\n\\.\n2,b\n").unwrap().rows;
        assert_eq!(rows, vec![vec![Some("1".into()), Some("a".into())]]);
        // A quoted `\.` is data, not a terminator.
        let rows = decode(&csv_format(), b"\"\\.\",x\n").unwrap().rows;
        assert_eq!(rows, vec![vec![Some("\\.".into()), Some("x".into())]]);
    }

    #[test]
    fn text_boundary_stops_at_the_last_newline() {
        // The partial third line is carried over, not handed to the decoder.
        assert_eq!(record_boundary(&text_format(), b"1\ta\n2\tb\n3\tc"), 8);
        assert_eq!(record_boundary(&text_format(), b"1\ta"), 0);
    }

    #[test]
    fn csv_boundary_ignores_newlines_inside_quotes() {
        let fmt = csv_format();
        // The newline inside the quoted field is data; only the closing one ends
        // a record.
        assert_eq!(record_boundary(&fmt, b"\"a\nb\",x\n1,2"), 8);
        // A record that is still inside its quoted section has no boundary at all.
        assert_eq!(record_boundary(&fmt, b"\"a\nb"), 0);
    }

    #[test]
    fn csv_boundary_holds_back_undecidable_tails() {
        let fmt = csv_format();
        // A trailing `"` may yet be the first half of a doubled `""`.
        assert_eq!(record_boundary(&fmt, b"1,\"a\""), 0);
        // A trailing `\r` may yet be the first half of a CRLF pair.
        assert_eq!(record_boundary(&fmt, b"1,a\r"), 0);
        assert_eq!(record_boundary(&fmt, b"1,a\r\n"), 5);
    }

    /// Feed a stream through the boundary/decode loop the file reader runs,
    /// cutting it into `chunk` -sized reads, so a boundary bug shows up as a
    /// changed result rather than only under a real file.
    fn decode_in_chunks(
        format: &CopyFormat,
        bytes: &[u8],
        chunk: usize,
    ) -> Result<Vec<Vec<Field>>, PgError> {
        let mut format = format.clone();
        let mut buffer: Vec<u8> = Vec::new();
        let mut rows = Vec::new();
        for slice in bytes.chunks(chunk) {
            buffer.extend_from_slice(slice);
            let cut = record_boundary(&format, &buffer);
            if cut == 0 {
                continue;
            }
            let decoded = decode(&format, &buffer[..cut])?;
            buffer.drain(..cut);
            format.header = false;
            rows.extend(decoded.rows);
            if decoded.end_of_data {
                return Ok(rows);
            }
        }
        if !buffer.is_empty() {
            rows.extend(decode(&format, &buffer)?.rows);
        }
        Ok(rows)
    }

    #[test]
    fn chunked_decode_matches_whole_buffer_decode() -> Result<(), PgError> {
        let text = b"1\ta\n2\tb\n3\tc\n4\td";
        let csv = b"1,\"a\nb\"\r\n2,\"c\"\"d\"\r\n3,e";
        for chunk in 1..=8 {
            assert_eq!(
                decode_in_chunks(&text_format(), text, chunk)?,
                decode(&text_format(), text)?.rows,
                "text, chunk {chunk}"
            );
            assert_eq!(
                decode_in_chunks(&csv_format(), csv, chunk)?,
                decode(&csv_format(), csv)?.rows,
                "csv, chunk {chunk}"
            );
        }
        Ok(())
    }

    #[test]
    fn chunked_decode_skips_the_header_once() -> Result<(), PgError> {
        let mut fmt = text_format();
        fmt.header = true;
        // One row per chunk: a per-chunk header skip would eat every row.
        let rows = decode_in_chunks(&fmt, b"h1\th2\n1\ta\n2\tb\n", 6)?;
        assert_eq!(
            rows,
            vec![
                vec![Some("1".into()), Some("a".into())],
                vec![Some("2".into()), Some("b".into())],
            ]
        );
        Ok(())
    }

    #[test]
    fn chunked_decode_stops_at_end_of_data() -> Result<(), PgError> {
        let rows = decode_in_chunks(&text_format(), b"1\ta\n\\.\n2\tb\n", 4)?;
        assert_eq!(rows, vec![vec![Some("1".into()), Some("a".into())]]);
        Ok(())
    }

    #[test]
    fn relative_path_is_rejected_before_any_read() {
        let err = read_file_rows("data/onek.data", &text_format(), |_| Ok(()))
            .expect_err("a relative path must not be read");
        assert_eq!(err.code, sqlstate::FEATURE_NOT_SUPPORTED);
    }

    #[test]
    fn missing_file_reports_pg_wording() {
        let err = read_file_rows("/nonexistent/crabgresql-copy.data", &text_format(), |_| {
            Ok(())
        })
        .expect_err("a missing file must error");
        assert_eq!(err.code, sqlstate::UNDEFINED_FILE);
        assert_eq!(
            err.message,
            "could not open file \"/nonexistent/crabgresql-copy.data\" for reading: \
             No such file or directory"
        );
    }
}
