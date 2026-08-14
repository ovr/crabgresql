//! COPY FROM text/CSV row decoding, for both row sources: the wire's copy-in
//! stream and a server-side file.
//!
//! The caller hands us raw bytes; this module splits them into logical rows of
//! field strings per a resolved [`CopyFormat`] — text-format backslash escapes
//! and the `\N` NULL marker, or CSV quoting with `""` doubling. Decoding is
//! **byte-oriented** (as PostgreSQL's COPY is): escapes produce raw bytes,
//! multi-byte UTF-8 flows through untouched, and the encoding check runs over
//! the raw input rather than per parsed value — so an escaped multi-byte
//! character round-trips and an invalid byte (or NUL) errors as PG does. It never
//! parses values into a type: that is
//! [`crabgresql_binder::CopyFromPlan::build_insert`]'s job. `None` marks a field
//! that matched the NULL representation.
//!
//! Fields come out as spans into a [`RowBatch`]'s arena rather than as owned
//! `String`s. Only the text family's value *is* the field string; an `int`, a
//! `date` or a `numeric` parses the text and drops it, so a `String` per field
//! meant allocating for every cell of a load and freeing most of them again one
//! statement later. A text column still allocates once — where the [`Value`] is
//! built, not here.
//!
//! [`Value`]: crabgresql_types::Value
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

use crabgresql_binder::{CopyFormat, CopyHeader, CopyRow, RowBatch};
use crabgresql_pg_wire::sqlstate;

use crate::copy_access::CopyFileAccess;
use crate::error::PgError;

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
fn check_header_names(header: CopyRow<'_>, expected: &[String]) -> Result<(), PgError> {
    if header.len() != expected.len() {
        return Err(bad_copy(format!(
            "wrong number of fields in header line: got {}, expected {}",
            header.len(),
            expected.len()
        )));
    }
    for (index, (got, want)) in header.iter().zip(expected).enumerate() {
        let got = got.unwrap_or_default();
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

/// Read a completed field's raw bytes as text, erroring exactly as PG does on a
/// byte sequence that is not valid UTF-8 or on an embedded NUL.
///
/// The `&str` goes straight into the batch, so a field is decoded once for the
/// load rather than checked here and re-checked on every read.
fn validate_field(bytes: &[u8]) -> Result<&str, PgError> {
    let text = std::str::from_utf8(bytes).map_err(|e| {
        // The first byte the decoder rejected.
        invalid_utf8(bytes[e.valid_up_to()])
    })?;
    if bytes.contains(&0) {
        return Err(invalid_utf8(0));
    }
    Ok(text)
}

/// Decode a complete COPY byte stream into rows of fields.
///
/// A `\.` line (text format's end-of-data marker) and the empty segment after a
/// final newline are dropped; `HEADER` skips the first data line. This is the
/// one-shot form used by the copy-in wire path, which has the whole stream in
/// hand; a file is decoded incrementally through [`CopyDecoder`].
pub fn decode(format: &CopyFormat, bytes: &[u8]) -> Result<RowBatch, PgError> {
    let mut decoder = CopyDecoder::new(format);
    // The whole stream is in hand, so the arena is sized once instead of
    // doubling its way there. It cannot need more than the input.
    decoder.batch = RowBatch::with_capacity(bytes.len());
    decoder.push(bytes)?;
    decoder.finish()?;
    Ok(decoder.into_batch())
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
    /// Text format: where a field containing a backslash is de-escaped. One
    /// buffer reused for every such field, so the escape path costs no
    /// allocation either; a field with no backslash never touches it.
    unescaped: Vec<u8>,
    csv: CsvState,
    /// Rows completed since the last [`CopyDecoder::take_batch_into`].
    batch: RowBatch,
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
    ///
    /// It stays here rather than growing straight into the batch's arena
    /// because the arena must hold only *completed* fields: that is what makes
    /// a batch split — which routinely lands mid-record — a matter of moving
    /// whole rows rather than of rebasing a half-built one. The buffer itself
    /// is reused across fields, so it allocates once per load, not once per
    /// field.
    field: Vec<u8>,
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
            unescaped: Vec::new(),
            csv: CsvState::default(),
            batch: RowBatch::new(),
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

    /// The rows completed so far and not yet taken.
    pub fn batch(&self) -> &RowBatch {
        &self.batch
    }

    /// The rows completed so far, for a caller that is done with the decoder.
    pub fn into_batch(self) -> RowBatch {
        self.batch
    }

    /// Move the first `n` completed rows into `out`, leaving the decoder with
    /// the rest — including the record it is still building, whose finished
    /// fields are already in the batch. See [`RowBatch::split_into`].
    pub fn take_batch_into(&mut self, n: usize, out: &mut RowBatch) {
        self.batch.split_into(n, out);
    }

    /// Consume `bytes`, adding every record they complete to the batch.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), PgError> {
        if self.end_of_data {
            return Ok(());
        }
        if self.format.csv {
            self.push_csv(bytes)
        } else {
            self.push_text(bytes)
        }
    }

    /// End of input: a final record with no terminator is still a record, and a
    /// CSV quoted section left open is an error.
    pub fn finish(&mut self) -> Result<(), PgError> {
        if self.end_of_data {
            return Ok(());
        }
        if !self.format.csv {
            if !self.line.is_empty() {
                self.complete_text_line()?;
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
        if self.csv.was_quoted || !self.csv.field.is_empty() || self.batch.current_row_len() > 0 {
            self.end_csv_record()?;
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

    fn push_text(&mut self, bytes: &[u8]) -> Result<(), PgError> {
        let mut rest = bytes;
        while let Some(k) = rest.iter().position(|&b| b == b'\n') {
            self.charge(k + 1)?;
            self.line.extend_from_slice(&rest[..k]);
            rest = &rest[k + 1..];
            self.complete_text_line()?;
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
    fn complete_text_line(&mut self) -> Result<(), PgError> {
        if self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        self.record_bytes = 0;
        let outcome = self.take_text_line();
        // One clear on every path, including the error one, rather than an
        // invariant each early return has to remember.
        self.line.clear();
        outcome
    }

    /// The accumulated line as a row — or as the end-of-data marker, or as the
    /// header. The caller owns clearing `line`.
    fn take_text_line(&mut self) -> Result<(), PgError> {
        if self.line == b"\\." {
            self.end_of_data = true;
            return Ok(());
        }
        if self.skip_header {
            self.skip_header = false;
            // Skipped before decoding, so a plain text HEADER line is never
            // UTF-8 checked. `MATCH` has to read the names, so it decodes
            // first — the same asymmetry PostgreSQL has.
            if !matches!(self.format.header, CopyHeader::Match(_)) {
                return Ok(());
            }
            self.decode_text_line()?;
            return self.consume_header_row();
        }
        self.decode_text_line()
    }

    /// Check the row just closed as the `HEADER` line against the statement's
    /// column list, then drop it — a header is not data.
    fn consume_header_row(&mut self) -> Result<(), PgError> {
        let check = match &self.format.header {
            CopyHeader::Match(expected) => {
                check_header_names(self.batch.row(self.batch.len() - 1), expected)
            }
            _ => Ok(()),
        };
        self.batch.pop_row();
        check
    }

    /// Split the accumulated raw line into fields on unescaped delimiters and
    /// append it to the batch as one row.
    ///
    /// The NULL marker is compared against the *raw*, still-escaped field, and
    /// a field is de-escaped only when the scan actually saw a backslash in it
    /// — which for a typical load is no field at all.
    fn decode_text_line(&mut self) -> Result<(), PgError> {
        // Destructured so the line can be read while the batch is written; they
        // are disjoint fields, which a method call could not express.
        let CopyDecoder {
            line,
            unescaped,
            batch,
            format,
            ..
        } = self;
        let (delimiter, null) = (format.delimiter, format.null.as_str());
        // The encoding check runs once for the line rather than once per field.
        // It reports the same leftmost bad byte either way — and it is what PG
        // does, which converts the input's encoding before it parses fields —
        // while a 100-column row would otherwise pay 100 calls over a handful
        // of bytes each, never reaching the vectorized path. Only a field the
        // escape decoder rewrites is re-checked, below.
        let line = validate_field(line)?;

        let mut start = 0;
        let mut i = 0;
        // Whether the field starting at `start` contains a backslash.
        let mut escaped = false;
        // Splitting on bytes keeps every index on a char boundary: the
        // delimiter is a single ASCII byte (the binder rejects any other), and
        // a UTF-8 continuation byte can never equal one.
        while i < line.len() {
            match line.as_bytes()[i] {
                // A backslash always consumes the next byte, so a `\<delim>`
                // never splits.
                b'\\' => {
                    escaped = true;
                    i += 2;
                }
                b if b == delimiter => {
                    push_text_field(&line[start..i], escaped, null, batch, unescaped)?;
                    i += 1;
                    start = i;
                    escaped = false;
                }
                _ => i += 1,
            }
        }
        push_text_field(&line[start..], escaped, null, batch, unescaped)?;
        batch.end_row();
        Ok(())
    }

    fn push_csv(&mut self, bytes: &[u8]) -> Result<(), PgError> {
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
                        self.end_csv_record()?;
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

    /// Finish the current CSV field, applying the NULL rule (an unquoted match
    /// only) and validating the bytes as UTF-8.
    ///
    /// The scratch buffer is emptied for the next field whether or not the
    /// validation passed, so its state never depends on the error path.
    fn finish_csv_field(&mut self) -> Result<(), PgError> {
        let CopyDecoder {
            csv, batch, format, ..
        } = self;
        let force_not_null = format.force_not_null.contains(&batch.current_row_len());
        let is_null =
            !csv.was_quoted && !force_not_null && csv.field.as_slice() == format.null.as_bytes();
        csv.was_quoted = false;
        let outcome = if is_null {
            batch.push_null();
            Ok(())
        } else {
            validate_field(&csv.field).map(|text| batch.push_field(text))
        };
        csv.field.clear();
        outcome
    }

    fn end_csv_record(&mut self) -> Result<(), PgError> {
        // Decided before the field is finished, which clears `was_quoted`: a
        // lone `\.` record is end-of-data, but `\.,x`, `\.a` and `"\."` are data.
        let is_eod =
            self.batch.current_row_len() == 0 && !self.csv.was_quoted && self.csv.field == b"\\.";
        self.record_bytes = 0;
        if is_eod {
            self.end_of_data = true;
            self.csv.field.clear();
            self.csv.was_quoted = false;
            return Ok(());
        }
        self.finish_csv_field()?;
        self.batch.end_row();
        // Skipped after decoding, so a CSV HEADER line *is* UTF-8 checked —
        // the asymmetry with the text format is PG's.
        if self.skip_header {
            self.skip_header = false;
            return self.consume_header_row();
        }
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

/// PostgreSQL has no COPY-specific line limit: a logical line accumulates in a
/// string buffer that refuses to grow past `MaxAllocSize` (1 GB − 1). We
/// reproduce that limit, its SQLSTATE and its wording; the HINT is ours (PG
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

/// Append one raw (still-escaped) text field to the batch.
///
/// `escaped` is what the splitting scan already learned — whether this field
/// contains a backslash — so the common field goes into the arena as the text
/// that arrived, with no de-escaping pass, no buffer of its own, and no second
/// encoding check: the line it is a slice of has already passed one. Only an
/// escape decode can produce bytes the line did not contain (`\351`), so only
/// that path validates.
fn push_text_field(
    raw: &str,
    escaped: bool,
    null: &str,
    batch: &mut RowBatch,
    unescaped: &mut Vec<u8>,
) -> Result<(), PgError> {
    if raw == null {
        batch.push_null();
    } else if escaped {
        unescaped.clear();
        unescape_text_into(raw.as_bytes(), unescaped);
        batch.push_field(validate_field(unescaped)?);
    } else {
        batch.push_field(raw);
    }
    Ok(())
}

/// Translate PostgreSQL text-format backslash escapes into raw bytes.
fn unescape_text_into(raw: &[u8], out: &mut Vec<u8>) {
    out.reserve(raw.len());
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
    mut sink: impl FnMut(&RowBatch) -> Result<(), PgError>,
) -> Result<(), PgError> {
    let mut decoder = CopyDecoder::new(format);
    let mut chunk = vec![0u8; READ_CHUNK];
    // One batch handed to the sink over and over: `take_batch_into` swaps the
    // decoder's rows into it and gives its allocations back, so the load's
    // buffers are acquired once however many batches it takes.
    let mut ready = RowBatch::new();

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
            decoder.finish()?;
            break;
        }
        decoder.push(&chunk[..read])?;
        while decoder.batch().len() >= batch_rows {
            decoder.take_batch_into(batch_rows, &mut ready);
            sink(&ready)?;
        }
        if decoder.end_of_data() {
            break;
        }
    }

    if !decoder.batch().is_empty() {
        sink(decoder.batch())?;
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

/// PG rejects a directory before it ever reads a byte, with the
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

    /// One decoded field, owned. The decoder hands out spans into a batch's
    /// arena; the assertions below are written against the *values*, which is
    /// what makes them readable and what makes them survive a change of
    /// representation like this one.
    type Field = Option<String>;

    fn owned(batch: &RowBatch) -> Vec<Vec<Field>> {
        batch
            .iter()
            .map(|row| row.iter().map(|f| f.map(String::from)).collect())
            .collect()
    }

    /// [`decode`] with its rows read back as owned values, which is how every
    /// expectation below is written.
    fn decode_owned(format: &CopyFormat, bytes: &[u8]) -> Result<Vec<Vec<Field>>, PgError> {
        decode(format, bytes).map(|batch| owned(&batch))
    }

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
        let rows = decode_owned(&text_format(), b"1\thello\n2\tworld\n")?;
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
        let rows = decode_owned(&text_format(), b"1\t\\N\n2\t\n")?;
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
        let rows = decode_owned(&text_format(), b"a\\tb\tc\\\\d\te\\061f\n")?;
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
        let rows = decode_owned(&text_format(), b"\\346\\227\\245\t\\xc3\\xa9\n")?;
        assert_eq!(rows, vec![vec![Some("日".into()), Some("é".into())]]);
        Ok(())
    }

    #[test]
    fn text_invalid_utf8_byte_errors() {
        let err = decode_owned(&text_format(), b"\\351\n")
            .expect_err("an invalid UTF-8 byte must be rejected");
        assert_eq!(err.code, sqlstate::CHARACTER_NOT_IN_REPERTOIRE);
        assert!(err.message.contains("0xe9"), "{}", err.message);
    }

    #[test]
    fn text_nul_byte_errors() {
        let err = decode_owned(&text_format(), b"a\\000b\n")
            .expect_err("an embedded NUL must be rejected");
        assert_eq!(err.code, sqlstate::CHARACTER_NOT_IN_REPERTOIRE);
        assert!(err.message.contains("0x00"), "{}", err.message);
    }

    /// The text line is encoding-checked as a whole, before it is split, so a
    /// raw bad byte anywhere in the line is reported ahead of one an escape
    /// would have produced later — the order PostgreSQL has, which converts the
    /// input's encoding before it parses fields at all.
    #[test]
    fn the_raw_line_is_checked_before_any_escape_is_decoded() {
        let err = decode_owned(&text_format(), b"\\351\t\xff\n")
            .expect_err("the raw byte must be rejected");
        assert_eq!(err.code, sqlstate::CHARACTER_NOT_IN_REPERTOIRE);
        assert!(err.message.contains("0xff"), "{}", err.message);
    }

    #[test]
    fn text_escaped_delimiter_does_not_split() -> Result<(), PgError> {
        let rows = decode_owned(&text_format(), b"a\\\tb\n")?;
        assert_eq!(rows, vec![vec![Some("a\tb".into())]]);
        Ok(())
    }

    #[test]
    fn text_end_of_data_marker_and_no_trailing_newline() -> Result<(), PgError> {
        let rows = decode_owned(&text_format(), b"1\ta\n\\.\n")?;
        assert_eq!(rows, vec![vec![Some("1".into()), Some("a".into())]]);
        let rows = decode_owned(&text_format(), b"9\tz")?;
        assert_eq!(rows, vec![vec![Some("9".into()), Some("z".into())]]);
        Ok(())
    }

    #[test]
    fn text_header_skips_first_line() -> Result<(), PgError> {
        let mut fmt = text_format();
        fmt.header = CopyHeader::On;
        let rows = decode_owned(&fmt, b"a\tb\n1\tx\n")?;
        assert_eq!(rows, vec![vec![Some("1".into()), Some("x".into())]]);
        Ok(())
    }

    #[test]
    fn csv_quoting_and_doubling() -> Result<(), PgError> {
        let rows = decode_owned(&csv_format(), b"1,\"a,b\",\"she \"\"said\"\"\"\n")?;
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
        let rows = decode_owned(&csv_format(), b"1, \"two\"\nab\"cd\",\"a\"b\"c\"\n")?;
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
        let rows = decode_owned(&csv_format(), b"1,,\"\"\n")?;
        assert_eq!(
            rows,
            vec![vec![Some("1".into()), None, Some(String::new())]]
        );
        Ok(())
    }

    #[test]
    fn csv_embedded_newline_in_quotes() -> Result<(), PgError> {
        let rows = decode_owned(&csv_format(), b"\"line1\nline2\",x\n")?;
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
        let rows = decode_owned(&fmt, b"1,\n")?;
        assert_eq!(rows, vec![vec![Some("1".into()), Some(String::new())]]);
        Ok(())
    }

    #[test]
    fn csv_unterminated_quote_errors() {
        let err = decode_owned(&csv_format(), b"\"oops\n")
            .expect_err("an unterminated quote must be rejected");
        assert_eq!(err.code, sqlstate::BAD_COPY_FILE_FORMAT);
    }

    #[test]
    fn csv_header_skips_first_row() -> Result<(), PgError> {
        let mut fmt = csv_format();
        fmt.header = CopyHeader::On;
        let rows = decode_owned(&fmt, b"a,b\n1,x\n")?;
        assert_eq!(rows, vec![vec![Some("1".into()), Some("x".into())]]);
        Ok(())
    }

    #[test]
    fn csv_end_of_data_marker() -> Result<(), PgError> {
        let rows = decode_owned(&csv_format(), b"1,a\n\\.\n2,b\n")?;
        assert_eq!(rows, vec![vec![Some("1".into()), Some("a".into())]]);
        // A quoted `\.` is data, not a terminator.
        let rows = decode_owned(&csv_format(), b"\"\\.\",x\n")?;
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
        for slice in bytes.chunks(chunk) {
            decoder.push(slice)?;
            if decoder.end_of_data() {
                return Ok(owned(decoder.batch()));
            }
        }
        decoder.finish()?;
        Ok(owned(decoder.batch()))
    }

    /// Drive the decoder the way [`read_file_rows`] does — slabs in, fixed-size
    /// batches out — and rejoin the batches. The split lands wherever the row
    /// count says, which for a small `batch_rows` is routinely in the middle of
    /// a record whose earlier fields are already decoded; nothing may be lost,
    /// duplicated or reordered there.
    fn decode_in_batches(
        format: &CopyFormat,
        bytes: &[u8],
        chunk: usize,
        batch_rows: usize,
    ) -> Result<Vec<Vec<Field>>, PgError> {
        let mut decoder = CopyDecoder::new(format);
        let mut ready = RowBatch::new();
        let mut all = Vec::new();
        for slice in bytes.chunks(chunk) {
            decoder.push(slice)?;
            while decoder.batch().len() >= batch_rows {
                decoder.take_batch_into(batch_rows, &mut ready);
                assert_eq!(ready.len(), batch_rows, "a batch is exactly what was asked");
                all.extend(owned(&ready));
            }
            if decoder.end_of_data() {
                all.extend(owned(decoder.batch()));
                return Ok(all);
            }
        }
        decoder.finish()?;
        all.extend(owned(decoder.batch()));
        Ok(all)
    }

    #[test]
    fn batching_does_not_change_the_rows() -> Result<(), PgError> {
        let cases: &[(&str, CopyFormat, &[u8])] = &[
            ("text", text_format(), b"1\ta\n2\t\\N\n3\tc\\td\n4\td"),
            ("text crlf", text_format(), b"1\ta\r\n2\tb\r\n3\tc"),
            ("csv", csv_format(), b"1,\"a\nb\"\r\n2,\"c\"\"d\"\r\n3,e"),
            ("csv empty fields", csv_format(), b"1,,\"\"\n2,x,\n3,,y\n"),
        ];
        for (name, fmt, bytes) in cases {
            let want = decode_owned(fmt, bytes)?;
            for chunk in 1..=8 {
                for batch_rows in 1..=4 {
                    assert_eq!(
                        decode_in_batches(fmt, bytes, chunk, batch_rows)?,
                        want,
                        "{name}, chunk {chunk}, batch {batch_rows}"
                    );
                }
            }
        }
        Ok(())
    }

    /// A field that needed de-escaping is built in a scratch buffer and a field
    /// that did not is copied straight from the line. Both must land in the
    /// arena as their own bytes — a shared scratch that leaked between them
    /// would show up as one field wearing another's value.
    #[test]
    fn escaped_and_plain_fields_do_not_share_a_buffer() -> Result<(), PgError> {
        let rows = decode_owned(&text_format(), b"a\\tb\tplain\tc\\\\d\tlonger-than-any\n")?;
        assert_eq!(
            rows,
            vec![vec![
                Some("a\tb".into()),
                Some("plain".into()),
                Some("c\\d".into()),
                Some("longer-than-any".into()),
            ]]
        );
        Ok(())
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
                    decode_owned(fmt, bytes)?,
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
        decoder.push(b"1\ta\n2\tb\n3\tc")?;
        assert_eq!(
            decoder.batch().len(),
            2,
            "the unterminated third line is not a row yet"
        );
        decoder.push(b"\n")?;
        assert_eq!(decoder.batch().len(), 3);
        Ok(())
    }

    #[test]
    fn csv_newline_inside_quotes_does_not_end_a_record() -> Result<(), PgError> {
        let mut decoder = CopyDecoder::new(&csv_format());
        decoder.push(b"\"a\nb\",x\n1,2")?;
        assert_eq!(
            owned(decoder.batch()),
            vec![vec![Some("a\nb".into()), Some("x".into())]]
        );
        Ok(())
    }

    #[test]
    fn csv_pending_quote_resolves_across_a_push() -> Result<(), PgError> {
        // A trailing `"` cannot be judged until the next byte arrives: it may be
        // the first half of a doubled `""`, or the close of the section.
        let mut decoder = CopyDecoder::new(&csv_format());
        // `1,"a"` leaves the quote undecided; `""` resolves it as a doubled
        // quote (so the section is still open) and opens the next decision,
        // which `,` then resolves as the closing quote.
        decoder.push(b"1,\"a\"")?;
        assert!(decoder.batch().is_empty());
        decoder.push(b"\"\"")?;
        assert!(decoder.batch().is_empty());
        decoder.push(b",b\n")?;
        assert_eq!(
            owned(decoder.batch()),
            vec![vec![Some("1".into()), Some("a\"".into()), Some("b".into())]]
        );

        let mut decoder = CopyDecoder::new(&csv_format());
        decoder.push(b"1,\"a\"")?;
        decoder.push(b"\n")?;
        assert_eq!(
            owned(decoder.batch()),
            vec![vec![Some("1".into()), Some("a".into())]]
        );
        Ok(())
    }

    #[test]
    fn csv_pending_cr_resolves_across_a_push() -> Result<(), PgError> {
        // The `\r` ends the record on its own; a following `\n` is its other half
        // and must not open a second, empty row.
        let mut decoder = CopyDecoder::new(&csv_format());
        decoder.push(b"1,a\r")?;
        assert_eq!(decoder.batch().len(), 1);
        decoder.push(b"\n")?;
        assert_eq!(decoder.batch().len(), 1, "the CRLF pair is one terminator");
        decoder.push(b"2,b\r")?;
        assert_eq!(decoder.batch().len(), 2);
        decoder.finish()?;
        assert_eq!(
            decoder.batch().len(),
            2,
            "a trailing CR leaves nothing pending"
        );
        Ok(())
    }

    #[test]
    fn csv_trailing_quote_at_eof_closes_the_section() -> Result<(), PgError> {
        assert_eq!(
            decode_owned(&csv_format(), b"1,\"a\"")?,
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
        decoder.push(b"\"a\\")?;
        decoder.push(b"\"b\"\n")?;
        assert_eq!(owned(decoder.batch()), vec![vec![Some("a\"b".into())]]);

        // ...and before anything else it stands for itself.
        let mut decoder = CopyDecoder::new(&fmt);
        decoder.push(b"\"a\\")?;
        decoder.push(b"x\"\n")?;
        assert_eq!(owned(decoder.batch()), vec![vec![Some("a\\x".into())]]);
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
        assert!(
            decoder.push(b"\"oops\n").is_ok(),
            "an open section is not yet an error while bytes may still arrive"
        );
        let err = decoder
            .finish()
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
        assert!(decode_owned(&fmt, b"\\351\n1\ta\n").is_ok());

        let mut fmt = csv_format();
        fmt.header = CopyHeader::On;
        let err = decode_owned(&fmt, b"\xff\n1,a\n").expect_err("csv validates the header");
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
        decoder.push(b"\"oops,")?;
        let slab = vec![b'x'; SLAB];
        for _ in 0..SLABS {
            decoder.push(&slab)?;
        }
        assert!(decoder.batch().is_empty());
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
        for _ in 0..SLABS {
            decoder.push(&slab)?;
        }
        assert!(decoder.batch().is_empty());
        decoder.finish()?;
        assert_eq!(decoder.batch().len(), 1);
        assert_eq!(decoder.batch().row(0).len(), 1);
        Ok(())
    }

    #[test]
    fn record_over_the_cap_errors_with_pgs_limit() {
        for fmt in [text_format(), csv_format()] {
            let mut decoder = CopyDecoder::new(&fmt).with_max_record_bytes(64);
            let err = decoder
                .push(&[b'x'; 100])
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
        let mut input = vec![b'x'; 63];
        input.push(b'\n');
        decoder.push(&input)?;
        assert_eq!(decoder.batch().len(), 1);
        Ok(())
    }

    #[test]
    fn the_cap_counts_across_pushes_not_per_push() {
        // Three 30-byte slabs: each fits alone, together they do not.
        let mut decoder = CopyDecoder::new(&text_format()).with_max_record_bytes(64);
        assert!(decoder.push(&[b'x'; 30]).is_ok());
        assert!(decoder.push(&[b'x'; 30]).is_ok());
        let err = decoder
            .push(&[b'x'; 30])
            .expect_err("the counter carries across slabs");
        assert_eq!(err.code, sqlstate::PROGRAM_LIMIT_EXCEEDED);
    }

    #[test]
    fn the_cap_resets_on_every_completed_record() -> Result<(), PgError> {
        // Many records, each well under the cap, must not accumulate toward it.
        let mut decoder = CopyDecoder::new(&text_format()).with_max_record_bytes(64);
        for _ in 0..100 {
            decoder.push(b"short\n")?;
        }
        assert_eq!(decoder.batch().len(), 100);
        Ok(())
    }
}
