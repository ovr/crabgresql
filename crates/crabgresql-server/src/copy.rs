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
//! Decoding is resumable: [`CopyDecoder`] takes bytes in arbitrary slabs and
//! keeps the quoting state itself, so a record may straddle any number of reads.
//! Only the record under construction is retained, which is what keeps a large
//! fixture file out of memory — it is decoded and inserted in batches, and a
//! record that never ends hits [`MAX_RECORD_BYTES`] instead of growing forever.
//!
//! Reproduces PostgreSQL's observable text/CSV COPY behavior (see the COPY docs)
//! rather than porting its C reader.

use std::fs::File;
use std::io::Read;

use crabgresql_binder::{CopyFormat, CopyHeader};
use crabgresql_pg_wire::sqlstate;

use crate::copy_access::CopyFileAccess;
use crate::error::PgError;

/// A single decoded field: `Some(text)` (the de-escaped/unquoted contents) or
/// `None` when the field matched the format's NULL representation.
pub type Field = Option<String>;

/// Bytes pulled from a COPY source file per read. Records are expected to be far
/// smaller than this, so the carried-over partial record stays short.
const READ_CHUNK: usize = 64 * 1024;

/// Rows decoded before they are handed to the inserter, when the caller states
/// no preference. Bounds how much of a file is materialized at once; a write
/// target whose batches become whole on-disk units asks for more (see
/// `TableAccessMethod::bulk_load_batch_rows`).
const BATCH_ROWS: usize = 1024;

/// The most bytes one COPY record may span, matching `MaxAllocSize` — the
/// ceiling PostgreSQL's own line buffer hits (see [`record_too_long`]).
const MAX_RECORD_BYTES: usize = 1024 * 1024 * 1024 - 1;

/// `HEADER match`: the first line's field names must be the columns the COPY
/// named, in that order.
///
/// The comparison is against the *statement's* column list rather than the
/// table's own order, so `COPY t (b, a) … HEADER match` wants `b,a`. A NULL
/// field cannot name a column, so it compares as the empty string and fails the
/// way any other wrong name does.
fn check_header_names(header: &[Field], expected: &[String]) -> Result<(), PgError> {
    if header.len() != expected.len() {
        return Err(bad_copy(format!(
            "wrong number of fields in header line: got {}, expected {}",
            header.len(),
            expected.len()
        )));
    }
    for (index, (got, want)) in header.iter().zip(expected).enumerate() {
        let got = got.as_deref().unwrap_or_default();
        if got != want {
            return Err(bad_copy(format!(
                "column name mismatch in header line field {}: got \"{got}\", expected \"{want}\"",
                index + 1
            )));
        }
    }
    Ok(())
}

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

/// Decode a complete COPY byte stream into rows of fields.
///
/// A `\.` line (text format's end-of-data marker) and the empty segment after a
/// final newline are dropped; `HEADER` skips the first data line. This is the
/// one-shot form used by the copy-in wire path, which has the whole stream in
/// hand; a file is decoded incrementally through [`CopyDecoder`].
pub fn decode(format: &CopyFormat, bytes: &[u8]) -> Result<Vec<Vec<Field>>, PgError> {
    let mut decoder = CopyDecoder::new(format);
    let mut rows = Vec::new();
    decoder.push(bytes, &mut rows)?;
    decoder.finish(&mut rows)?;
    Ok(rows)
}

/// A streaming, resumable text/CSV record decoder.
///
/// Bytes are pushed in arbitrary slabs and every byte is examined once; only the
/// record currently being built is retained, so a stream is never buffered whole
/// however far away its next terminator is. That is what the file reader needs:
/// a record can straddle any number of reads, and the decoder — not the caller —
/// owns the quoting state that decides where one ends.
pub struct CopyDecoder {
    format: CopyFormat,
    /// Text format: the raw, still-escaped bytes of the line so far.
    line: Vec<u8>,
    csv: CsvState,
    /// `HEADER` skips the first data line of the *stream*, not of each slab.
    skip_header: bool,
    end_of_data: bool,
    /// Input bytes consumed since the last completed record, against
    /// [`MAX_RECORD_BYTES`].
    record_bytes: usize,
    max_record_bytes: usize,
}

/// CSV needs more than the raw bytes: quoting decides whether a newline ends the
/// record, and both are resumable across a slab boundary.
#[derive(Default)]
struct CsvState {
    quote: QuoteState,
    /// A `\r` ended the previous record; a `\n` now would be its other half.
    cr_pending: bool,
    /// The field being built, already unquoted and unescaped.
    field: Vec<u8>,
    row: Vec<Field>,
    /// This field contained a quoted section, so it is never the NULL marker.
    was_quoted: bool,
}

/// Where the CSV scanner is inside the quoting grammar. The two `Pending` states
/// are what a stateless scanner could not express: they carry a decision that
/// needs the *next* byte, which may be in the next slab.
#[derive(Default, PartialEq, Debug)]
enum QuoteState {
    /// Outside any quoted section.
    #[default]
    Field,
    /// Inside a quoted section.
    Quoted,
    /// `escape == quote` (the default): a quote byte was seen inside a quoted
    /// section, and the next byte decides `""` (one literal quote) from a close.
    QuotePending,
    /// `escape != quote`: an escape byte was seen inside a quoted section.
    EscapePending,
}

impl CopyDecoder {
    pub fn new(format: &CopyFormat) -> Self {
        CopyDecoder {
            format: format.clone(),
            line: Vec::new(),
            csv: CsvState::default(),
            skip_header: format.header.present(),
            end_of_data: false,
            record_bytes: 0,
            max_record_bytes: MAX_RECORD_BYTES,
        }
    }

    /// A `\.` end-of-data marker has been seen; bytes after it are not data.
    pub fn end_of_data(&self) -> bool {
        self.end_of_data
    }

    /// Consume `bytes`, appending every record they complete to `rows`.
    pub fn push(&mut self, bytes: &[u8], rows: &mut Vec<Vec<Field>>) -> Result<(), PgError> {
        if self.end_of_data {
            return Ok(());
        }
        if self.format.csv {
            self.push_csv(bytes, rows)
        } else {
            self.push_text(bytes, rows)
        }
    }

    /// End of input: a final record with no terminator is still a record, and a
    /// CSV quoted section left open is an error.
    pub fn finish(&mut self, rows: &mut Vec<Vec<Field>>) -> Result<(), PgError> {
        if self.end_of_data {
            return Ok(());
        }
        if !self.format.csv {
            if !self.line.is_empty() {
                self.complete_text_line(rows)?;
            }
            return Ok(());
        }
        match self.csv.quote {
            // A trailing quote at end of input closes its section rather than
            // doubling: there is no byte left to pair it with.
            QuoteState::QuotePending => self.csv.quote = QuoteState::Field,
            // A trailing custom escape stands for itself.
            QuoteState::EscapePending => {
                let escape = self.format.escape;
                self.csv.field.push(escape);
                self.csv.quote = QuoteState::Quoted;
            }
            _ => {}
        }
        if self.csv.quote == QuoteState::Quoted {
            return Err(bad_copy("unterminated CSV quoted field"));
        }
        if self.csv.was_quoted || !self.csv.field.is_empty() || !self.csv.row.is_empty() {
            self.end_csv_record(rows)?;
        }
        Ok(())
    }

    /// Charge `added` input bytes to the record being built, refusing to grow it
    /// past the cap.
    fn charge(&mut self, added: usize) -> Result<(), PgError> {
        if self.record_bytes + added > self.max_record_bytes {
            return Err(record_too_long(
                self.record_bytes,
                added,
                self.max_record_bytes,
            ));
        }
        self.record_bytes += added;
        Ok(())
    }

    fn push_text(&mut self, bytes: &[u8], rows: &mut Vec<Vec<Field>>) -> Result<(), PgError> {
        let mut rest = bytes;
        while let Some(k) = rest.iter().position(|&b| b == b'\n') {
            self.charge(k + 1)?;
            self.line.extend_from_slice(&rest[..k]);
            rest = &rest[k + 1..];
            self.complete_text_line(rows)?;
            if self.end_of_data {
                return Ok(());
            }
        }
        self.charge(rest.len())?;
        self.line.extend_from_slice(rest);
        Ok(())
    }

    /// Turn the accumulated raw line into a row. A straddled CRLF needs no
    /// special handling: the `\r` simply sits in `line` until the `\n` arrives.
    fn complete_text_line(&mut self, rows: &mut Vec<Vec<Field>>) -> Result<(), PgError> {
        if self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        self.record_bytes = 0;
        if self.line == b"\\." {
            self.end_of_data = true;
            self.line.clear();
            return Ok(());
        }
        // Skipped before decoding, so a plain text HEADER line is never UTF-8
        // checked. `MATCH` has to read the names, so it decodes first — the same
        // asymmetry PostgreSQL has.
        if self.skip_header {
            self.skip_header = false;
            if let CopyHeader::Match(expected) = &self.format.header {
                let header = decode_text_line(
                    &self.line,
                    self.format.delimiter,
                    self.format.null.as_bytes(),
                )?;
                check_header_names(&header, expected)?;
            }
            self.line.clear();
            return Ok(());
        }
        let row = decode_text_line(
            &self.line,
            self.format.delimiter,
            self.format.null.as_bytes(),
        )?;
        self.line.clear();
        rows.push(row);
        Ok(())
    }

    fn push_csv(&mut self, bytes: &[u8], rows: &mut Vec<Vec<Field>>) -> Result<(), PgError> {
        let (delimiter, quote, escape) =
            (self.format.delimiter, self.format.quote, self.format.escape);
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            // The `\n` half of a CRLF that ended the previous record.
            if self.csv.cr_pending {
                self.csv.cr_pending = false;
                if b == b'\n' {
                    self.charge(1)?;
                    i += 1;
                    continue;
                }
            }
            // `advance == 0` means "re-read this byte in the state we just moved
            // to" — how the pending states resolve without lookahead. A byte is
            // seen at most twice and never re-scanned beyond that.
            let advance = match self.csv.quote {
                QuoteState::Quoted => {
                    if b == escape && escape == quote {
                        self.csv.quote = QuoteState::QuotePending;
                        1
                    } else if b == escape {
                        self.csv.quote = QuoteState::EscapePending;
                        1
                    } else if b == quote {
                        // Reachable only when escape != quote: a bare close.
                        self.csv.quote = QuoteState::Field;
                        1
                    } else {
                        self.csv.field.push(b);
                        1
                    }
                }
                QuoteState::QuotePending => {
                    if b == quote {
                        self.csv.field.push(quote);
                        self.csv.quote = QuoteState::Quoted;
                        1
                    } else {
                        self.csv.quote = QuoteState::Field;
                        0
                    }
                }
                QuoteState::EscapePending => {
                    if b == quote || b == escape {
                        self.csv.field.push(b);
                        self.csv.quote = QuoteState::Quoted;
                        1
                    } else {
                        // A custom escape before anything else is itself.
                        self.csv.field.push(escape);
                        self.csv.quote = QuoteState::Quoted;
                        0
                    }
                }
                QuoteState::Field => {
                    if b == quote {
                        // A quote anywhere in the field opens a quoted section;
                        // PG concatenates unquoted and quoted runs, so `ab"cd"`
                        // is one field and not an error.
                        self.csv.quote = QuoteState::Quoted;
                        self.csv.was_quoted = true;
                        1
                    } else if b == delimiter {
                        self.finish_csv_field()?;
                        1
                    } else if b == b'\n' || b == b'\r' {
                        self.csv.cr_pending = b == b'\r';
                        self.charge(1)?;
                        i += 1;
                        self.end_csv_record(rows)?;
                        if self.end_of_data {
                            return Ok(());
                        }
                        continue;
                    } else {
                        self.csv.field.push(b);
                        1
                    }
                }
            };
            if advance > 0 {
                self.charge(advance)?;
                i += advance;
            }
        }
        Ok(())
    }

    fn finish_csv_field(&mut self) -> Result<(), PgError> {
        let force = force_not_null_at(&self.format, self.csv.row.len());
        finish_csv_field(
            &mut self.csv.field,
            &mut self.csv.was_quoted,
            &mut self.csv.row,
            self.format.null.as_bytes(),
            force,
        )
    }

    fn end_csv_record(&mut self, rows: &mut Vec<Vec<Field>>) -> Result<(), PgError> {
        // Decided before the field is finished, which clears `was_quoted`: a
        // lone `\.` record is end-of-data, but `\.,x`, `\.a` and `"\."` are data.
        let is_eod = self.csv.row.is_empty() && !self.csv.was_quoted && self.csv.field == b"\\.";
        self.record_bytes = 0;
        if is_eod {
            self.end_of_data = true;
            self.csv.field.clear();
            self.csv.row.clear();
            self.csv.was_quoted = false;
            return Ok(());
        }
        self.finish_csv_field()?;
        let row = std::mem::take(&mut self.csv.row);
        // Skipped after decoding, so a CSV HEADER line *is* UTF-8 checked —
        // the asymmetry with the text format is PG's.
        if self.skip_header {
            self.skip_header = false;
            if let CopyHeader::Match(expected) = &self.format.header {
                check_header_names(&row, expected)?;
            }
            return Ok(());
        }
        rows.push(row);
        Ok(())
    }

    /// Bytes retained for the record under construction. Tests assert this stays
    /// bounded; nothing in production reads it.
    #[cfg(test)]
    fn buffered_len(&self) -> usize {
        self.line.len() + self.csv.field.len()
    }

    #[cfg(test)]
    fn with_max_record_bytes(mut self, max: usize) -> Self {
        self.max_record_bytes = max;
        self
    }
}

/// PostgreSQL has no COPY-specific line limit: `CopyReadLine` accumulates the
/// logical line into a `StringInfo`, which refuses to grow past `MaxAllocSize`.
/// We reproduce that limit, its SQLSTATE and its wording; the HINT is ours (PG
/// emits none), because in practice this is almost always an unterminated quote.
fn record_too_long(have: usize, added: usize, max: usize) -> PgError {
    PgError::new(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        format!("string buffer exceeds maximum allowed length ({max} bytes)"),
    )
    .with_detail(format!(
        "Cannot enlarge string buffer containing {have} bytes by {added} more bytes."
    ))
    .with_hint(
        "Check for an unterminated quoted field or a misconfigured QUOTE/ESCAPE in the COPY data.",
    )
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

fn force_not_null_at(format: &CopyFormat, field_index: usize) -> bool {
    format.force_not_null.contains(&field_index)
}

/// Stream an already-opened COPY source file, handing the decoded rows to `sink`
/// in batches of at most `batch_rows` so a large file is never held in memory in
/// full. `sink` runs inside the COPY's transaction, so an error it returns aborts
/// the whole load.
///
/// The file arrives open because resolving and authorizing the path
/// ([`crate::copy_access::CopyFileAccess`]) happens before the statement takes a
/// transaction. `path` is only the name to quote in an I/O error — the one the
/// statement wrote, which is what PG prints.
pub fn read_file_rows(
    mut file: File,
    path: &str,
    format: &CopyFormat,
    batch_rows: usize,
    mut sink: impl FnMut(Vec<Vec<Field>>) -> Result<(), PgError>,
) -> Result<(), PgError> {
    let mut decoder = CopyDecoder::new(format);
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut pending: Vec<Vec<Field>> = Vec::new();

    loop {
        let read = match file.read(&mut chunk) {
            Ok(read) => read,
            // A signal that interrupted the read is not a failed COPY.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(read_error(path, &e)),
        };
        if read == 0 {
            // End of file: a final record with no terminator is still a record,
            // and an unterminated CSV quote is rejected here.
            decoder.finish(&mut pending)?;
            break;
        }
        decoder.push(&chunk[..read], &mut pending)?;
        while pending.len() >= batch_rows {
            let rest = pending.split_off(batch_rows);
            sink(std::mem::replace(&mut pending, rest))?;
        }
        if decoder.end_of_data() {
            break;
        }
    }

    if !pending.is_empty() {
        sink(pending)?;
    }
    Ok(())
}

/// Resolve, authorize and open the file a `COPY … FROM '<file>'` names.
///
/// The order is load-bearing. Confinement is decided first, so a refused path
/// reads the same whether or not it exists. The file type is checked next, on
/// the path rather than an open handle, because `open(2)` on a FIFO blocks until
/// a writer appears — with no server-side `statement_timeout` to escape, that
/// would park a worker for good.
pub fn open_source_file(access: &CopyFileAccess, path: &str) -> Result<File, PgError> {
    let resolved = access.resolve_for_read(path)?;
    match std::fs::metadata(&resolved) {
        Ok(meta) if meta.is_dir() => return Err(is_a_directory(path)),
        // PG reads a FIFO happily; we refuse, because a read that never returns
        // is unrecoverable here. Documented divergence.
        Ok(meta) if !meta.is_file() => return Err(not_a_regular_file(path)),
        Ok(_) => {}
        // Report the open failure itself, in PG's words, rather than a stat one.
        Err(e) => return Err(open_error(path, &e)),
    }
    let file = File::open(&resolved).map_err(|e| open_error(path, &e))?;
    // Re-check now that the target exists: a symlink could have been swapped
    // between the resolve above and this open.
    if let Ok(real) = std::fs::canonicalize(&resolved)
        && !access.permits(&real)
    {
        return Err(access.denial(path));
    }
    Ok(file)
}

/// PG's `BeginCopyFrom` rejects a directory before it ever reads, with the
/// wrong-object-type class rather than an I/O error.
fn is_a_directory(path: &str) -> PgError {
    PgError::new(
        sqlstate::WRONG_OBJECT_TYPE,
        format!("\"{path}\" is a directory"),
    )
}

/// Not in PG, which will read any openable file. See [`open_source_file`].
fn not_a_regular_file(path: &str) -> PgError {
    PgError::new(
        sqlstate::WRONG_OBJECT_TYPE,
        format!("\"{path}\" is not a regular file"),
    )
    .with_hint("COPY FROM reads a file directly; named pipes and devices are not supported.")
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
    // PG's own hint: this form reads on the *server*, and a user who reached it
    // by accident nearly always wanted psql's client-side `\copy`.
    .with_hint(
        "COPY FROM instructs the PostgreSQL server process to read a file. \
         You may want a client-side facility such as psql's \\copy.",
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
    use crabgresql_storage_api::TableAccessMethod;

    fn text_format() -> CopyFormat {
        CopyFormat::text()
    }

    fn csv_format() -> CopyFormat {
        CopyFormat::csv()
    }

    /// The batch size is the caller's to choose, and it is what decides how many
    /// on-disk units a load leaves behind for a target that writes whole batches.
    /// Whatever it is, the rows must come out the same.
    #[test]
    fn a_file_load_batches_at_the_size_the_target_asks_for() -> Result<(), PgError> {
        let rows = BATCH_ROWS * 2 + 7;
        let mut data = Vec::new();
        for row in 0..rows {
            data.extend_from_slice(format!("{row}\tv\n").as_bytes());
        }

        let batches = |batch_rows: usize| -> Result<Vec<usize>, PgError> {
            let dir = tempfile::tempdir().map_err(|e| bad_copy(e.to_string()))?;
            let path = dir.path().join("rows.data");
            std::fs::write(&path, &data).map_err(|e| bad_copy(e.to_string()))?;
            let file = File::open(&path).map_err(|e| bad_copy(e.to_string()))?;
            let mut sizes = Vec::new();
            read_file_rows(file, "rows.data", &text_format(), batch_rows, |batch| {
                sizes.push(batch.len());
                Ok(())
            })?;
            Ok(sizes)
        };

        // A row store's bound: three batches for this many rows.
        let heap = TableAccessMethod::Heap.bulk_load_batch_rows();
        assert_eq!(heap, BATCH_ROWS);
        let plain = batches(heap)?;
        assert_eq!(plain, vec![BATCH_ROWS, BATCH_ROWS, 7]);

        // A whole-batch writer asks for far more than this file holds, so it gets
        // one batch — one fragment instead of three.
        let columnar = TableAccessMethod::Parquet.bulk_load_batch_rows();
        assert!(columnar > rows);
        let whole = batches(columnar)?;
        assert_eq!(whole, vec![rows]);

        // And the batching is the only difference.
        assert_eq!(plain.iter().sum::<usize>(), whole.iter().sum::<usize>());
        Ok(())
    }

    #[test]
    fn text_basic_tab_rows() -> Result<(), PgError> {
        let rows = decode(&text_format(), b"1\thello\n2\tworld\n")?;
        assert_eq!(
            rows,
            vec![
                vec![Some("1".into()), Some("hello".into())],
                vec![Some("2".into()), Some("world".into())],
            ]
        );
        Ok(())
    }

    #[test]
    fn text_null_marker_and_empty_field() -> Result<(), PgError> {
        let rows = decode(&text_format(), b"1\t\\N\n2\t\n")?;
        assert_eq!(
            rows,
            vec![
                vec![Some("1".into()), None],
                vec![Some("2".into()), Some(String::new())],
            ]
        );
        Ok(())
    }

    #[test]
    fn text_escapes() -> Result<(), PgError> {
        let rows = decode(&text_format(), b"a\\tb\tc\\\\d\te\\061f\n")?;
        // \t -> tab, \\ -> backslash, \061 (octal) -> '1'
        assert_eq!(
            rows,
            vec![vec![
                Some("a\tb".into()),
                Some("c\\d".into()),
                Some("e1f".into()),
            ]]
        );
        Ok(())
    }

    #[test]
    fn text_octal_and_hex_escapes_form_multibyte_utf8() -> Result<(), PgError> {
        // The three UTF-8 bytes of 日 written as octal, and é as hex — each must
        // round-trip to the single character (byte-oriented decode), not mojibake.
        let rows = decode(&text_format(), b"\\346\\227\\245\t\\xc3\\xa9\n")?;
        assert_eq!(rows, vec![vec![Some("日".into()), Some("é".into())]]);
        Ok(())
    }

    #[test]
    fn text_invalid_utf8_byte_errors() {
        let err =
            decode(&text_format(), b"\\351\n").expect_err("an invalid UTF-8 byte must be rejected");
        assert_eq!(err.code, sqlstate::CHARACTER_NOT_IN_REPERTOIRE);
        assert!(err.message.contains("0xe9"), "{}", err.message);
    }

    #[test]
    fn text_nul_byte_errors() {
        let err =
            decode(&text_format(), b"a\\000b\n").expect_err("an embedded NUL must be rejected");
        assert_eq!(err.code, sqlstate::CHARACTER_NOT_IN_REPERTOIRE);
        assert!(err.message.contains("0x00"), "{}", err.message);
    }

    #[test]
    fn text_escaped_delimiter_does_not_split() -> Result<(), PgError> {
        let rows = decode(&text_format(), b"a\\\tb\n")?;
        assert_eq!(rows, vec![vec![Some("a\tb".into())]]);
        Ok(())
    }

    #[test]
    fn text_end_of_data_marker_and_no_trailing_newline() -> Result<(), PgError> {
        let rows = decode(&text_format(), b"1\ta\n\\.\n")?;
        assert_eq!(rows, vec![vec![Some("1".into()), Some("a".into())]]);
        let rows = decode(&text_format(), b"9\tz")?;
        assert_eq!(rows, vec![vec![Some("9".into()), Some("z".into())]]);
        Ok(())
    }

    #[test]
    fn text_header_skips_first_line() -> Result<(), PgError> {
        let mut fmt = text_format();
        fmt.header = CopyHeader::On;
        let rows = decode(&fmt, b"a\tb\n1\tx\n")?;
        assert_eq!(rows, vec![vec![Some("1".into()), Some("x".into())]]);
        Ok(())
    }

    #[test]
    fn csv_quoting_and_doubling() -> Result<(), PgError> {
        let rows = decode(&csv_format(), b"1,\"a,b\",\"she \"\"said\"\"\"\n")?;
        assert_eq!(
            rows,
            vec![vec![
                Some("1".into()),
                Some("a,b".into()),
                Some("she \"said\"".into()),
            ]]
        );
        Ok(())
    }

    #[test]
    fn csv_concatenates_unquoted_and_quoted_runs() -> Result<(), PgError> {
        // PG accepts a quote adjacent to unquoted content, concatenating the
        // runs: `1, "two"` -> ` two`, `ab"cd"` -> `abcd`, `"a"b"c"` -> `abc`.
        let rows = decode(&csv_format(), b"1, \"two\"\nab\"cd\",\"a\"b\"c\"\n")?;
        assert_eq!(
            rows,
            vec![
                vec![Some("1".into()), Some(" two".into())],
                vec![Some("abcd".into()), Some("abc".into())],
            ]
        );
        Ok(())
    }

    #[test]
    fn csv_empty_is_null_but_quoted_empty_is_text() -> Result<(), PgError> {
        let rows = decode(&csv_format(), b"1,,\"\"\n")?;
        assert_eq!(
            rows,
            vec![vec![Some("1".into()), None, Some(String::new())]]
        );
        Ok(())
    }

    #[test]
    fn csv_embedded_newline_in_quotes() -> Result<(), PgError> {
        let rows = decode(&csv_format(), b"\"line1\nline2\",x\n")?;
        assert_eq!(
            rows,
            vec![vec![Some("line1\nline2".into()), Some("x".into())]]
        );
        Ok(())
    }

    #[test]
    fn csv_force_not_null_keeps_empty_as_text() -> Result<(), PgError> {
        let mut fmt = csv_format();
        fmt.force_not_null = vec![1];
        let rows = decode(&fmt, b"1,\n")?;
        assert_eq!(rows, vec![vec![Some("1".into()), Some(String::new())]]);
        Ok(())
    }

    #[test]
    fn csv_unterminated_quote_errors() {
        let err =
            decode(&csv_format(), b"\"oops\n").expect_err("an unterminated quote must be rejected");
        assert_eq!(err.code, sqlstate::BAD_COPY_FILE_FORMAT);
    }

    #[test]
    fn csv_header_skips_first_row() -> Result<(), PgError> {
        let mut fmt = csv_format();
        fmt.header = CopyHeader::On;
        let rows = decode(&fmt, b"a,b\n1,x\n")?;
        assert_eq!(rows, vec![vec![Some("1".into()), Some("x".into())]]);
        Ok(())
    }

    #[test]
    fn csv_end_of_data_marker() -> Result<(), PgError> {
        let rows = decode(&csv_format(), b"1,a\n\\.\n2,b\n")?;
        assert_eq!(rows, vec![vec![Some("1".into()), Some("a".into())]]);
        // A quoted `\.` is data, not a terminator.
        let rows = decode(&csv_format(), b"\"\\.\",x\n")?;
        assert_eq!(rows, vec![vec![Some("\\.".into()), Some("x".into())]]);
        Ok(())
    }

    /// Drive the streaming decoder the way the file reader does, in `chunk`-sized
    /// slabs, so a state bug shows up as a changed result rather than only under
    /// a real file.
    fn decode_in_chunks(
        format: &CopyFormat,
        bytes: &[u8],
        chunk: usize,
    ) -> Result<Vec<Vec<Field>>, PgError> {
        let mut decoder = CopyDecoder::new(format);
        let mut rows = Vec::new();
        for slice in bytes.chunks(chunk) {
            decoder.push(slice, &mut rows)?;
            if decoder.end_of_data() {
                return Ok(rows);
            }
        }
        decoder.finish(&mut rows)?;
        Ok(rows)
    }

    #[test]
    fn chunked_decode_matches_whole_buffer_decode() -> Result<(), PgError> {
        let cases: &[(&str, CopyFormat, &[u8])] = &[
            ("text", text_format(), b"1\ta\n2\tb\n3\tc\n4\td"),
            ("text crlf", text_format(), b"1\ta\r\n2\tb\r\n3\tc"),
            ("csv", csv_format(), b"1,\"a\nb\"\r\n2,\"c\"\"d\"\r\n3,e"),
            ("csv trailing quote", csv_format(), b"1,\"a\"\n2,\"b\"\"\""),
            ("csv empty fields", csv_format(), b"1,,\"\"\n2,x,\n"),
        ];
        for (name, fmt, bytes) in cases {
            for chunk in 1..=16 {
                assert_eq!(
                    decode_in_chunks(fmt, bytes, chunk)?,
                    decode(fmt, bytes)?,
                    "{name}, chunk {chunk}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn chunked_decode_skips_the_header_once() -> Result<(), PgError> {
        // Every slab size, so a boundary inside the header line is covered too.
        for chunk in 1..=16 {
            let mut fmt = text_format();
            fmt.header = CopyHeader::On;
            assert_eq!(
                decode_in_chunks(&fmt, b"h1\th2\n1\ta\n2\tb\n", chunk)?,
                vec![
                    vec![Some("1".into()), Some("a".into())],
                    vec![Some("2".into()), Some("b".into())],
                ],
                "text, chunk {chunk}"
            );
            let mut fmt = csv_format();
            fmt.header = CopyHeader::On;
            assert_eq!(
                decode_in_chunks(&fmt, b"a,b\n1,x\n2,y\n", chunk)?,
                vec![
                    vec![Some("1".into()), Some("x".into())],
                    vec![Some("2".into()), Some("y".into())],
                ],
                "csv, chunk {chunk}"
            );
        }
        Ok(())
    }

    #[test]
    fn chunked_decode_stops_at_end_of_data() -> Result<(), PgError> {
        for chunk in 1..=16 {
            assert_eq!(
                decode_in_chunks(&text_format(), b"1\ta\n\\.\n2\tb\n", chunk)?,
                vec![vec![Some("1".into()), Some("a".into())]],
                "text, chunk {chunk}"
            );
            assert_eq!(
                decode_in_chunks(&csv_format(), b"1,a\n\\.\n2,b\n", chunk)?,
                vec![vec![Some("1".into()), Some("a".into())]],
                "csv, chunk {chunk}"
            );
        }
        Ok(())
    }

    #[test]
    fn end_of_data_marker_without_a_trailing_newline() -> Result<(), PgError> {
        for chunk in 1..=8 {
            assert_eq!(
                decode_in_chunks(&text_format(), b"1\ta\n\\.", chunk)?,
                vec![vec![Some("1".into()), Some("a".into())]],
                "text, chunk {chunk}"
            );
            assert_eq!(
                decode_in_chunks(&csv_format(), b"1,a\n\\.", chunk)?,
                vec![vec![Some("1".into()), Some("a".into())]],
                "csv, chunk {chunk}"
            );
        }
        Ok(())
    }

    #[test]
    fn csv_end_of_data_lookalikes_are_data() -> Result<(), PgError> {
        // Only a record that is exactly `\.` terminates; anything longer is a row.
        for (input, want) in [
            (&b"\\.,x\n"[..], vec![Some("\\.".into()), Some("x".into())]),
            (&b"\\.a\n"[..], vec![Some("\\.a".into())]),
            (&b"\"\\.\"\n"[..], vec![Some("\\.".into())]),
        ] {
            for chunk in 1..=8 {
                assert_eq!(
                    decode_in_chunks(&csv_format(), input, chunk)?,
                    vec![want.clone()],
                    "chunk {chunk}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn text_partial_line_emits_no_row_until_its_newline() -> Result<(), PgError> {
        let mut decoder = CopyDecoder::new(&text_format());
        let mut rows = Vec::new();
        decoder.push(b"1\ta\n2\tb\n3\tc", &mut rows)?;
        assert_eq!(
            rows.len(),
            2,
            "the unterminated third line is not a row yet"
        );
        decoder.push(b"\n", &mut rows)?;
        assert_eq!(rows.len(), 3);
        Ok(())
    }

    #[test]
    fn csv_newline_inside_quotes_does_not_end_a_record() -> Result<(), PgError> {
        let mut decoder = CopyDecoder::new(&csv_format());
        let mut rows = Vec::new();
        decoder.push(b"\"a\nb\",x\n1,2", &mut rows)?;
        assert_eq!(rows, vec![vec![Some("a\nb".into()), Some("x".into())]]);
        Ok(())
    }

    #[test]
    fn csv_pending_quote_resolves_across_a_push() -> Result<(), PgError> {
        // A trailing `"` cannot be judged until the next byte arrives: it may be
        // the first half of a doubled `""`, or the close of the section.
        let mut decoder = CopyDecoder::new(&csv_format());
        let mut rows = Vec::new();
        // `1,"a"` leaves the quote undecided; `""` resolves it as a doubled
        // quote (so the section is still open) and opens the next decision,
        // which `,` then resolves as the closing quote.
        decoder.push(b"1,\"a\"", &mut rows)?;
        assert!(rows.is_empty());
        decoder.push(b"\"\"", &mut rows)?;
        assert!(rows.is_empty());
        decoder.push(b",b\n", &mut rows)?;
        assert_eq!(
            rows,
            vec![vec![Some("1".into()), Some("a\"".into()), Some("b".into())]]
        );

        let mut decoder = CopyDecoder::new(&csv_format());
        let mut rows = Vec::new();
        decoder.push(b"1,\"a\"", &mut rows)?;
        decoder.push(b"\n", &mut rows)?;
        assert_eq!(rows, vec![vec![Some("1".into()), Some("a".into())]]);
        Ok(())
    }

    #[test]
    fn csv_pending_cr_resolves_across_a_push() -> Result<(), PgError> {
        // The `\r` ends the record on its own; a following `\n` is its other half
        // and must not open a second, empty row.
        let mut decoder = CopyDecoder::new(&csv_format());
        let mut rows = Vec::new();
        decoder.push(b"1,a\r", &mut rows)?;
        assert_eq!(rows.len(), 1);
        decoder.push(b"\n", &mut rows)?;
        assert_eq!(rows.len(), 1, "the CRLF pair is one terminator");
        decoder.push(b"2,b\r", &mut rows)?;
        assert_eq!(rows.len(), 2);
        decoder.finish(&mut rows)?;
        assert_eq!(rows.len(), 2, "a trailing CR leaves nothing pending");
        Ok(())
    }

    #[test]
    fn csv_trailing_quote_at_eof_closes_the_section() -> Result<(), PgError> {
        assert_eq!(
            decode(&csv_format(), b"1,\"a\"")?,
            vec![vec![Some("1".into()), Some("a".into())]]
        );
        Ok(())
    }

    #[test]
    fn csv_custom_escape_pending_across_a_push() -> Result<(), PgError> {
        let mut fmt = csv_format();
        fmt.escape = b'\\';
        // An escape before a quote emits the quote literally...
        let mut decoder = CopyDecoder::new(&fmt);
        let mut rows = Vec::new();
        decoder.push(b"\"a\\", &mut rows)?;
        decoder.push(b"\"b\"\n", &mut rows)?;
        assert_eq!(rows, vec![vec![Some("a\"b".into())]]);

        // ...and before anything else it stands for itself.
        let mut decoder = CopyDecoder::new(&fmt);
        let mut rows = Vec::new();
        decoder.push(b"\"a\\", &mut rows)?;
        decoder.push(b"x\"\n", &mut rows)?;
        assert_eq!(rows, vec![vec![Some("a\\x".into())]]);
        Ok(())
    }

    #[test]
    fn final_partial_record_at_eof_is_a_row() -> Result<(), PgError> {
        for chunk in 1..=8 {
            assert_eq!(
                decode_in_chunks(&text_format(), b"9\tz", chunk)?,
                vec![vec![Some("9".into()), Some("z".into())]]
            );
            assert_eq!(
                decode_in_chunks(&csv_format(), b"3,e", chunk)?,
                vec![vec![Some("3".into()), Some("e".into())]]
            );
            assert_eq!(
                decode_in_chunks(&csv_format(), b"3,", chunk)?,
                vec![vec![Some("3".into()), None]]
            );
        }
        Ok(())
    }

    #[test]
    fn unterminated_csv_quote_at_eof_errors_from_finish() {
        let mut decoder = CopyDecoder::new(&csv_format());
        let mut rows = Vec::new();
        assert!(
            decoder.push(b"\"oops\n", &mut rows).is_ok(),
            "an open section is not yet an error while bytes may still arrive"
        );
        let err = decoder
            .finish(&mut rows)
            .expect_err("an unterminated quote is an error at end of input");
        assert_eq!(err.code, sqlstate::BAD_COPY_FILE_FORMAT);
    }

    #[test]
    fn csv_header_is_utf8_validated_but_text_header_is_not() -> Result<(), PgError> {
        // PG decodes the CSV header row and then drops it, so its bytes are
        // checked; the text format skips the line before decoding. Pinned so the
        // asymmetry is a decision rather than an accident.
        let mut fmt = text_format();
        fmt.header = CopyHeader::On;
        assert!(decode(&fmt, b"\\351\n1\ta\n").is_ok());

        let mut fmt = csv_format();
        fmt.header = CopyHeader::On;
        let err = decode(&fmt, b"\xff\n1,a\n").expect_err("csv validates the header");
        assert_eq!(err.code, sqlstate::CHARACTER_NOT_IN_REPERTOIRE);
        Ok(())
    }

    /// The bug this decoder replaced: a record with no terminator in sight used
    /// to accumulate the whole stream and re-scan it on every slab.
    #[test]
    fn an_unterminated_record_does_not_buffer_the_whole_stream() -> Result<(), PgError> {
        const SLAB: usize = 64 * 1024;
        const SLABS: usize = 64; // 4 MiB, which the quadratic version crawled on

        // CSV: an unmatched quote at the very start means no record ever ends.
        let mut decoder = CopyDecoder::new(&csv_format());
        let mut rows = Vec::new();
        decoder.push(b"\"oops,", &mut rows)?;
        let slab = vec![b'x'; SLAB];
        for _ in 0..SLABS {
            decoder.push(&slab, &mut rows)?;
        }
        assert!(rows.is_empty());
        // Retained bytes are the in-progress field, not a second copy of the
        // stream: the old code held both `buffer` and the decoded fields.
        assert!(
            decoder.buffered_len() <= SLAB * SLABS + 16,
            "retained {} bytes",
            decoder.buffered_len()
        );

        // Text: a file with no newline at all is ONE row — correct PG behavior;
        // only the quadratic buffering was ever the bug.
        let mut decoder = CopyDecoder::new(&text_format());
        let mut rows = Vec::new();
        for _ in 0..SLABS {
            decoder.push(&slab, &mut rows)?;
        }
        assert!(rows.is_empty());
        decoder.finish(&mut rows)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 1);
        Ok(())
    }

    #[test]
    fn record_over_the_cap_errors_with_pgs_limit() {
        for fmt in [text_format(), csv_format()] {
            let mut decoder = CopyDecoder::new(&fmt).with_max_record_bytes(64);
            let mut rows = Vec::new();
            let err = decoder
                .push(&[b'x'; 100], &mut rows)
                .expect_err("a record past the cap must be refused");
            assert_eq!(err.code, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(
                err.message
                    .contains("string buffer exceeds maximum allowed length"),
                "{}",
                err.message
            );
            assert!(err.detail.is_some());
        }
    }

    #[test]
    fn a_record_exactly_at_the_cap_is_accepted() -> Result<(), PgError> {
        // 63 payload bytes plus the newline that completes the record.
        let mut decoder = CopyDecoder::new(&text_format()).with_max_record_bytes(64);
        let mut rows = Vec::new();
        let mut input = vec![b'x'; 63];
        input.push(b'\n');
        decoder.push(&input, &mut rows)?;
        assert_eq!(rows.len(), 1);
        Ok(())
    }

    #[test]
    fn the_cap_counts_across_pushes_not_per_push() {
        // Three 30-byte slabs: each fits alone, together they do not.
        let mut decoder = CopyDecoder::new(&text_format()).with_max_record_bytes(64);
        let mut rows = Vec::new();
        assert!(decoder.push(&[b'x'; 30], &mut rows).is_ok());
        assert!(decoder.push(&[b'x'; 30], &mut rows).is_ok());
        let err = decoder
            .push(&[b'x'; 30], &mut rows)
            .expect_err("the counter carries across slabs");
        assert_eq!(err.code, sqlstate::PROGRAM_LIMIT_EXCEEDED);
    }

    #[test]
    fn the_cap_resets_on_every_completed_record() -> Result<(), PgError> {
        // Many records, each well under the cap, must not accumulate toward it.
        let mut decoder = CopyDecoder::new(&text_format()).with_max_record_bytes(64);
        let mut rows = Vec::new();
        for _ in 0..100 {
            decoder.push(b"short\n", &mut rows)?;
        }
        assert_eq!(rows.len(), 100);
        Ok(())
    }
}
