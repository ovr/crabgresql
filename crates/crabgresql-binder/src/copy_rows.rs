//! The row container a `COPY FROM` load hands to
//! [`CopyFromPlan::build_insert`](crate::CopyFromPlan::build_insert).
//!
//! A load decodes a batch of rows and then parses every field through its
//! column's input function. Only the *text* family's value is the field string
//! itself; for an `int`, a `date` or a `numeric` the string is parsed and
//! dropped. Handing the batch over as `Vec<Vec<Option<String>>>` therefore
//! allocated once per field — and threw most of those allocations away one
//! statement later, which on a bulk load is the dominant malloc traffic.
//!
//! [`RowBatch`] holds the batch's field text in one arena and addresses each
//! field by span, so decoding a batch costs a handful of amortized `Vec` growth
//! steps rather than a `String` per cell. A text column still pays its one
//! allocation, but it pays it where the value is actually built.
//!
//! Because a field is only ever a span, a decoder that already has a whole
//! record in hand can hand the *record* to the arena once
//! ([`RowBatch::push_line`]) and address its fields inside it
//! ([`RowBatch::push_field_at`]) — one copy of the input instead of one for the
//! record and another for every field it holds.
//!
//! The batch is the unit of reuse: a decoder fills one, the load consumes it by
//! reference, and [`RowBatch::split_into`] hands the allocations back for the
//! next batch.

/// One field's text inside a [`RowBatch`]'s arena, or the NULL marker.
///
/// A NULL is a sentinel span rather than an `Option<FieldSpan>` so the field
/// vector stays a flat array of two `usize`s; `start == NULL_FIELD` cannot
/// collide with a real span, because a real `start` is at most the arena's
/// length.
#[derive(Clone, Copy, Debug)]
struct FieldSpan {
    start: usize,
    end: usize,
}

/// The `start` sentinel marking a field that matched the NULL representation.
const NULL_FIELD: usize = usize::MAX;

impl FieldSpan {
    fn null() -> Self {
        FieldSpan {
            start: NULL_FIELD,
            end: NULL_FIELD,
        }
    }

    fn is_null(self) -> bool {
        self.start == NULL_FIELD
    }

    /// The same field, addressed from `base` rather than from 0. A NULL has no
    /// offset to move.
    fn rebased(self, base: usize) -> Self {
        if self.is_null() {
            self
        } else {
            FieldSpan {
                start: self.start - base,
                end: self.end - base,
            }
        }
    }
}

/// Where a row begins, in both of the batch's arrays. The byte offset is
/// recorded per row rather than derived from the row's first field, so a row
/// whose every field is NULL still has a splitting point — see
/// [`RowBatch::split_into`].
#[derive(Clone, Copy, Debug)]
struct RowStart {
    field: usize,
    byte: usize,
}

/// The boundary a batch with no rows starts from.
const ORIGIN: RowStart = RowStart { field: 0, byte: 0 };

/// A batch of decoded `COPY` rows: the field text in one arena, addressed by
/// span.
///
/// Fields are appended left to right through [`push_field`](Self::push_field)
/// and [`push_null`](Self::push_null), and a row is closed with
/// [`end_row`](Self::end_row). Only *completed* fields ever reach the arena —
/// a decoder that builds a field incrementally keeps it in its own scratch
/// buffer — which is what lets [`split_into`](Self::split_into) be a matter of
/// moving whole rows rather than of rebasing a half-built one.
///
/// ```
/// use crabgresql_binder::RowBatch;
///
/// let mut batch = RowBatch::new();
/// batch.push_field("1");
/// batch.push_null();
/// batch.end_row();
///
/// assert_eq!(batch.len(), 1);
/// let row = batch.row(0);
/// assert_eq!(row.len(), 2);
/// assert_eq!(row.get(0), Some("1"));
/// assert_eq!(row.get(1), None);
/// ```
#[derive(Debug)]
pub struct RowBatch {
    /// Every completed field's text — and, for a decoder that fills the arena a
    /// record at a time, the bytes between them.
    ///
    /// Nothing reads the arena except through a [`FieldSpan`], so a byte that
    /// belongs to no field (a delimiter, a NULL marker, a field superseded by
    /// its de-escaped form) is simply never addressed. What the readers do
    /// depend on is that a row's spans all fall past the row's recorded byte
    /// boundary — see [`RowBatch::push_field_at`].
    ///
    /// A `String` and not a `Vec<u8>`: the decoder has to run the UTF-8 check
    /// anyway — it is the layer that can report a bad byte as PostgreSQL's
    /// `CHARACTER_NOT_IN_REPERTOIRE` at the right point in the stream — so
    /// taking its `&str` here means the input is validated once for the load
    /// rather than again on every read. Field spans fall on field boundaries,
    /// which are char boundaries, so every slice below is infallible.
    buf: String,
    fields: Vec<FieldSpan>,
    /// Row boundaries: `n + 1` entries for `n` completed rows, so row `i`
    /// spans `bounds[i]..bounds[i + 1]`. The trailing entry is where the row
    /// under construction begins — keeping it here rather than in a separate
    /// pair of "open row" fields is what removes the last-row special case
    /// from the readers and the hand-sync from every mutator.
    bounds: Vec<RowStart>,
}

impl Default for RowBatch {
    fn default() -> Self {
        RowBatch::new()
    }
}

impl RowBatch {
    pub fn new() -> Self {
        RowBatch {
            buf: String::new(),
            fields: Vec::new(),
            bounds: vec![ORIGIN],
        }
    }

    /// A batch sized for `bytes` of input, for the one-shot path that has the
    /// whole stream in hand and would otherwise grow the arena by doubling.
    pub fn with_capacity(bytes: usize) -> Self {
        RowBatch {
            buf: String::with_capacity(bytes),
            ..RowBatch::new()
        }
    }

    /// Completed rows. Fields of a row still being built do not count.
    pub fn len(&self) -> usize {
        self.bounds.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The fields of completed row `index`.
    ///
    /// # Panics
    /// If `index` is not a completed row.
    pub fn row(&self, index: usize) -> CopyRow<'_> {
        CopyRow {
            buf: &self.buf,
            fields: &self.fields[self.bounds[index].field..self.bounds[index + 1].field],
        }
    }

    /// Every completed row, in order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = CopyRow<'_>> {
        (0..self.len()).map(|i| self.row(i))
    }

    /// Number of fields pushed into the row currently under construction —
    /// equivalently, the index the next field will take, which is what
    /// `FORCE_NOT_NULL` is keyed on.
    pub fn current_row_len(&self) -> usize {
        self.fields.len() - self.open().field
    }

    /// Bytes the arena currently holds.
    ///
    /// Nothing in production reads this; it exists so a decoder's tests can
    /// assert what did — and did not — reach the arena, which is the invariant
    /// [`split_into`](Self::split_into) rests on.
    pub fn arena_len(&self) -> usize {
        self.buf.len()
    }

    /// Where the row under construction begins.
    fn open(&self) -> RowStart {
        *self
            .bounds
            .last()
            .expect("the origin boundary is never popped")
    }

    /// Append a completed field.
    pub fn push_field(&mut self, text: &str) {
        let start = self.buf.len();
        self.buf.push_str(text);
        self.fields.push(FieldSpan {
            start,
            end: self.buf.len(),
        });
    }

    /// Copy a whole raw record into the arena, returning the offset it landed
    /// at, so its fields can be addressed in place with
    /// [`push_field_at`](Self::push_field_at).
    ///
    /// This is what keeps a text-format load down to one copy of its input: the
    /// decoder already holds the record, and every field that needs no
    /// rewriting is a slice of it. The bytes between the fields ride along —
    /// see [`buf`](Self::buf).
    pub fn push_line(&mut self, line: &str) -> usize {
        let at = self.buf.len();
        self.buf.push_str(line);
        at
    }

    /// Append a completed field whose text is *already* in the arena — the
    /// offsets [`push_line`](Self::push_line) handed out.
    ///
    /// `start` must not point before the row under construction: a row's bytes
    /// living past its own boundary is what lets
    /// [`split_into`](Self::split_into) move rows by rebasing whole ranges, and
    /// what lets [`truncate_rows`](Self::truncate_rows) drop a row by cutting
    /// the arena. Appending the record and only then its fields satisfies this
    /// by construction.
    pub fn push_field_at(&mut self, start: usize, end: usize) {
        debug_assert!(start <= end && end <= self.buf.len(), "span outside arena");
        debug_assert!(
            start >= self.open().byte,
            "a field cannot start before its own row"
        );
        debug_assert!(
            self.buf.is_char_boundary(start) && self.buf.is_char_boundary(end),
            "span is not on char boundaries"
        );
        self.fields.push(FieldSpan { start, end });
    }

    /// Append a field that matched the format's NULL representation.
    pub fn push_null(&mut self) {
        self.fields.push(FieldSpan::null());
    }

    /// Close the row built by the pushes since the last `end_row`.
    pub fn end_row(&mut self) {
        self.bounds.push(RowStart {
            field: self.fields.len(),
            byte: self.buf.len(),
        });
    }

    /// Drop the row just closed by [`end_row`](Self::end_row), releasing its
    /// bytes. This is `HEADER`: the line has to be decoded (so a CSV header is
    /// UTF-8 checked exactly as PostgreSQL checks it) and then must not become
    /// a data row.
    ///
    /// # Panics
    /// If no row has been completed.
    pub fn pop_row(&mut self) {
        assert!(!self.is_empty(), "no completed row to drop");
        self.truncate_rows(self.len() - 1);
    }

    /// Split the batch in two: the first `n` rows move to `out`, and the rest
    /// — including the row still under construction, whose completed fields
    /// are already in the arena — stay here.
    ///
    /// A split lands wherever the batch filled up, which for CSV is routinely
    /// in the middle of a record, so carrying the open row is the common case
    /// rather than an edge one.
    ///
    /// `out`'s previous contents are dropped and its buffers are handed over
    /// here, so a load that flushes batch after batch settles into reusing two
    /// sets of allocations. The bulk of the data is *swapped* rather than
    /// copied; only the tail past `n` is moved byte-wise.
    ///
    /// # Panics
    /// If `n` exceeds the completed rows.
    pub fn split_into(&mut self, n: usize, out: &mut RowBatch) {
        out.clear();
        std::mem::swap(self, out);
        // `self` now holds `out`'s recycled — and empty — buffers, so the tail
        // rebases onto offset zero and needs no shift beyond `base`.
        let base = out.bounds[n];
        self.buf.push_str(&out.buf[base.byte..]);
        self.fields.extend(
            out.fields[base.field..]
                .iter()
                .map(|f| f.rebased(base.byte)),
        );
        self.bounds
            .extend(out.bounds[n + 1..].iter().map(|b| RowStart {
                field: b.field - base.field,
                byte: b.byte - base.byte,
            }));
        out.truncate_rows(n);
    }

    /// Drop every row, keeping the allocations for the next batch.
    fn clear(&mut self) {
        self.buf.clear();
        self.fields.clear();
        self.bounds.truncate(1);
    }

    /// Keep only the first `n` completed rows, dropping the rest **and** any
    /// row under construction.
    fn truncate_rows(&mut self, n: usize) {
        let start = self.bounds[n.min(self.len())];
        self.bounds.truncate(n + 1);
        self.fields.truncate(start.field);
        self.buf.truncate(start.byte);
    }
}

/// The fields of one `COPY` row, borrowed from the batch's arena.
#[derive(Clone, Copy, Debug)]
pub struct CopyRow<'a> {
    buf: &'a str,
    fields: &'a [FieldSpan],
}

impl<'a> CopyRow<'a> {
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Field `index`, or `None` when it matched the NULL representation.
    pub fn get(&self, index: usize) -> Option<&'a str> {
        self.field(self.fields[index])
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = Option<&'a str>> {
        let row = *self;
        self.fields.iter().map(move |&span| row.field(span))
    }

    fn field(&self, span: FieldSpan) -> Option<&'a str> {
        (!span.is_null()).then(|| &self.buf[span.start..span.end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a batch the way a decoder would, for the tests below.
    fn batch_of(rows: &[Vec<Option<&str>>]) -> RowBatch {
        let mut batch = RowBatch::new();
        for row in rows {
            for field in row {
                match field {
                    Some(text) => batch.push_field(text),
                    None => batch.push_null(),
                }
            }
            batch.end_row();
        }
        batch
    }

    fn owned(batch: &RowBatch) -> Vec<Vec<Option<String>>> {
        batch
            .iter()
            .map(|row| row.iter().map(|f| f.map(String::from)).collect())
            .collect()
    }

    #[test]
    fn rows_read_back_as_they_were_pushed() {
        let want = vec![
            vec![Some("1"), Some("hello"), None],
            vec![None, None],
            vec![],
            vec![Some("")],
        ];
        let batch = batch_of(&want);
        assert_eq!(batch.len(), 4);
        assert_eq!(
            owned(&batch),
            want.iter()
                .map(|r| r.iter().map(|f| f.map(String::from)).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_reused_batch_does_not_leak_the_previous_one() {
        let mut batch = batch_of(&[vec![Some("aaaa"), Some("bbbb")]]);
        batch.clear();
        assert!(batch.is_empty());
        batch.push_field("x");
        batch.end_row();
        assert_eq!(owned(&batch), vec![vec![Some("x".to_string())]]);
    }

    #[test]
    fn popping_the_header_row_releases_its_bytes() {
        let mut batch = batch_of(&[vec![Some("h1"), Some("h2")]]);
        batch.pop_row();
        assert!(batch.is_empty());
        batch.push_field("1");
        batch.push_field("2");
        batch.end_row();
        assert_eq!(
            owned(&batch),
            vec![vec![Some("1".to_string()), Some("2".to_string())]]
        );
    }

    /// The head goes to the consumer, the tail stays behind, rebased.
    #[test]
    fn a_split_batch_keeps_every_row_intact() {
        let all: Vec<Vec<Option<&str>>> = (0..7)
            .map(|i| vec![Some("row"), None, Some(["a", "bb", "ccc"][i % 3])])
            .collect();
        for at in 0..=all.len() {
            let mut tail = batch_of(&all);
            let mut head = RowBatch::new();
            tail.split_into(at, &mut head);

            assert_eq!(head.len(), at);
            assert_eq!(tail.len(), all.len() - at);
            let mut rejoined = owned(&head);
            rejoined.extend(owned(&tail));
            assert_eq!(rejoined, owned(&batch_of(&all)), "split at {at}");

            // And the tail is a batch like any other: it keeps taking rows.
            tail.push_field("more");
            tail.end_row();
            assert_eq!(tail.row(tail.len() - 1).get(0), Some("more"));
        }
    }

    /// A split lands wherever the batch filled up, which is routinely inside a
    /// record whose earlier fields are already in the arena. Those fields must
    /// travel with the tail, not be handed to the consumer or dropped.
    #[test]
    fn a_split_carries_the_row_under_construction() {
        for at in 0..=2 {
            let mut tail = batch_of(&[vec![Some("a")], vec![Some("b")]]);
            tail.push_field("partial");
            tail.push_null();

            let mut head = RowBatch::new();
            tail.split_into(at, &mut head);

            assert_eq!(head.len(), at, "split at {at}");
            assert_eq!(tail.current_row_len(), 2, "split at {at}");
            tail.push_field("last");
            tail.end_row();
            let finished = tail.row(tail.len() - 1);
            assert_eq!(finished.len(), 3);
            assert_eq!(finished.get(0), Some("partial"));
            assert_eq!(finished.get(1), None);
            assert_eq!(finished.get(2), Some("last"));
        }
    }

    /// Build a row the way the text decoder does: the whole record into the
    /// arena once, then a span per field — and a rewritten field appended after
    /// it, out of the record's own order.
    fn push_record(batch: &mut RowBatch, record: &str, fields: &[Result<(usize, usize), &str>]) {
        let base = batch.push_line(record);
        for field in fields {
            match field {
                Ok((start, end)) => batch.push_field_at(base + start, base + end),
                Err(rewritten) => batch.push_field(rewritten),
            }
        }
        batch.end_row();
    }

    #[test]
    fn a_field_addressed_in_place_reads_as_its_own_text() {
        let mut batch = RowBatch::new();
        // `1\thello\tc\\d`, whose third field the decoder rewrote.
        push_record(
            &mut batch,
            "1\thello\tc\\\\d",
            &[Ok((0, 1)), Ok((2, 7)), Err("c\\d")],
        );
        batch.push_null();
        batch.end_row();
        assert_eq!(
            owned(&batch),
            vec![
                vec![
                    Some("1".to_string()),
                    Some("hello".to_string()),
                    Some("c\\d".to_string())
                ],
                vec![None],
            ]
        );
    }

    /// The bytes between the fields — delimiters, NULL markers, a superseded
    /// field — ride along in the arena, so a split has to move them with the
    /// row they belong to rather than trip over them.
    #[test]
    fn a_split_batch_of_in_place_rows_keeps_every_row_intact() {
        let build = || {
            let mut batch = RowBatch::new();
            for i in 0..5 {
                push_record(
                    &mut batch,
                    "aa\t\\N\tb\\tc",
                    &[Ok((0, 2)), Err(&format!("row{i}")), Ok((7, 8))],
                );
            }
            batch
        };
        for at in 0..=5 {
            let mut tail = build();
            let mut head = RowBatch::new();
            tail.split_into(at, &mut head);

            let mut rejoined = owned(&head);
            rejoined.extend(owned(&tail));
            assert_eq!(rejoined, owned(&build()), "split at {at}");
        }
    }

    #[test]
    fn popping_an_in_place_row_releases_the_whole_record() {
        let mut batch = RowBatch::new();
        push_record(&mut batch, "h1\th2", &[Ok((0, 2)), Ok((3, 5))]);
        assert_eq!(batch.arena_len(), "h1\th2".len());
        batch.pop_row();
        assert_eq!(batch.arena_len(), 0, "not one byte of the record survives");
        push_record(&mut batch, "1\t2", &[Ok((0, 1)), Ok((2, 3))]);
        assert_eq!(
            owned(&batch),
            vec![vec![Some("1".to_string()), Some("2".to_string())]]
        );
    }

    /// A row under construction is not a row, and a truncation drops it.
    #[test]
    fn an_unclosed_row_is_invisible() {
        let mut batch = batch_of(&[vec![Some("1")]]);
        batch.push_field("2");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch.current_row_len(), 1);
        batch.truncate_rows(0);
        assert!(batch.is_empty());
        assert_eq!(batch.current_row_len(), 0);
    }
}
