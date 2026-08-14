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
//! [`RowBatch`] holds the batch's field bytes in one arena and addresses each
//! field by span, so decoding a batch costs a handful of amortized `Vec` growth
//! steps rather than a `String` per cell. A text column still pays its one
//! allocation, but it pays it where the value is actually built.
//!
//! The batch is the unit of reuse: a decoder fills one, the load consumes it by
//! reference, and [`RowBatch::clear`] hands the allocations back for the next
//! batch.

/// One field's bytes inside a [`RowBatch`]'s arena, or the NULL marker.
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
}

/// Where a row begins, in both of the batch's arrays. The byte offset is
/// recorded per row rather than derived from the row's first field, so a row
/// whose every field is NULL still has a splitting point — see
/// [`RowBatch::move_rows_from`].
#[derive(Clone, Copy, Debug)]
struct RowStart {
    field: usize,
    byte: usize,
}

/// A batch of decoded `COPY` rows: the field bytes in one arena, addressed by
/// span.
///
/// Fields are appended left to right through [`push_field`](Self::push_field)
/// and [`push_null`](Self::push_null), and a row is closed with
/// [`end_row`](Self::end_row). Only *completed* fields ever reach the arena —
/// a decoder that builds a field incrementally keeps it in its own scratch
/// buffer — which is what lets [`clear`](Self::clear) and the batch split be
/// unconditional.
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
#[derive(Default, Debug)]
pub struct RowBatch {
    /// Every completed field's text, concatenated.
    ///
    /// A `String` and not a `Vec<u8>`: the decoder has to run the UTF-8 check
    /// anyway — it is the layer that can report a bad byte as PostgreSQL's
    /// `CHARACTER_NOT_IN_REPERTOIRE` at the right point in the stream — so
    /// taking its `&str` here means a field is validated once for the load
    /// rather than again on every read. Field spans are on field boundaries,
    /// which are char boundaries, so every slice below is infallible.
    buf: String,
    fields: Vec<FieldSpan>,
    /// One entry per *completed* row. Fields pushed since the last
    /// [`end_row`](Self::end_row) belong to the row under construction and are
    /// not visible to readers yet.
    rows: Vec<RowStart>,
    /// Where the row under construction starts — one past the last field, and
    /// one past the last byte, of the last completed row.
    open_field: usize,
    open_byte: usize,
}

impl RowBatch {
    pub fn new() -> Self {
        RowBatch::default()
    }

    /// Completed rows. Fields of a row still being built do not count.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The fields of completed row `index`.
    ///
    /// # Panics
    /// If `index` is not a completed row.
    pub fn row(&self, index: usize) -> Row<'_> {
        let start = self.rows[index].field;
        let end = match self.rows.get(index + 1) {
            Some(next) => next.field,
            None => self.open_field,
        };
        Row {
            buf: &self.buf,
            fields: &self.fields[start..end],
        }
    }

    /// Every completed row, in order.
    pub fn iter(&self) -> RowBatchIter<'_> {
        RowBatchIter {
            batch: self,
            next: 0,
            end: self.rows.len(),
        }
    }

    /// Number of fields pushed into the row currently under construction —
    /// equivalently, the index the next field will take, which is what
    /// `FORCE_NOT_NULL` is keyed on.
    pub fn current_row_len(&self) -> usize {
        self.fields.len() - self.open_field
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

    /// Append a field that matched the format's NULL representation.
    pub fn push_null(&mut self) {
        self.fields.push(FieldSpan::null());
    }

    /// Close the row built by the pushes since the last `end_row`.
    pub fn end_row(&mut self) {
        self.rows.push(RowStart {
            field: self.open_field,
            byte: self.open_byte,
        });
        self.open_field = self.fields.len();
        self.open_byte = self.buf.len();
    }

    /// Drop the row just closed by [`end_row`](Self::end_row), releasing its
    /// bytes. This is `HEADER`: the line has to be decoded (so a CSV header is
    /// UTF-8 checked exactly as PostgreSQL checks it) and then must not become
    /// a data row.
    ///
    /// # Panics
    /// If no row has been completed.
    pub fn pop_row(&mut self) {
        let start = self.rows.pop().expect("a completed row to drop");
        self.rewind_to(start);
    }

    /// Drop every row, keeping the allocations for the next batch.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.fields.clear();
        self.rows.clear();
        self.open_field = 0;
        self.open_byte = 0;
    }

    /// Keep only the first `n` completed rows, dropping the rest **and** any
    /// row under construction.
    pub fn truncate_rows(&mut self, n: usize) {
        if n < self.rows.len() {
            let start = self.rows[n];
            self.rows.truncate(n);
            self.rewind_to(start);
        } else {
            // Every completed row is kept; only the open row's fields go.
            self.fields.truncate(self.open_field);
            self.buf.truncate(self.open_byte);
        }
    }

    fn rewind_to(&mut self, start: RowStart) {
        self.fields.truncate(start.field);
        self.buf.truncate(start.byte);
        self.open_field = start.field;
        self.open_byte = start.byte;
    }

    /// Append `src`'s rows from `from` onward — completed rows **and** the row
    /// still under construction — rebasing their spans onto this arena.
    ///
    /// With [`truncate_rows`](Self::truncate_rows) this is the batch split:
    /// swap the full batch out to the consumer, move the short tail back,
    /// truncate the consumer's copy to the batch size. The large half is moved
    /// rather than copied; only the tail — at most one read chunk's worth of
    /// rows — is memcpy'd.
    ///
    /// Carrying the open row is not an edge case but the common one: a split
    /// lands wherever the batch filled up, which for CSV is routinely in the
    /// middle of a record whose earlier fields are already in the arena.
    ///
    /// # Panics
    /// If `from` is past `src`'s completed rows.
    pub fn move_rows_from(&mut self, src: &RowBatch, from: usize) {
        let base = match src.rows.get(from) {
            Some(&start) => start,
            None => {
                assert_eq!(from, src.rows.len(), "split point past the batch");
                RowStart {
                    field: src.open_field,
                    byte: src.open_byte,
                }
            }
        };

        let byte_shift = self.buf.len();
        self.buf.push_str(&src.buf[base.byte..]);

        let field_shift = self.fields.len();
        self.fields.extend(src.fields[base.field..].iter().map(|f| {
            if f.is_null() {
                FieldSpan::null()
            } else {
                FieldSpan {
                    start: f.start - base.byte + byte_shift,
                    end: f.end - base.byte + byte_shift,
                }
            }
        }));

        self.rows.extend(src.rows[from..].iter().map(|r| RowStart {
            field: r.field - base.field + field_shift,
            byte: r.byte - base.byte + byte_shift,
        }));
        // The open row keeps its identity across the move: the fields it has
        // collected so far are now this batch's open row.
        self.open_field = src.open_field - base.field + field_shift;
        self.open_byte = src.open_byte - base.byte + byte_shift;
    }
}

/// [`RowBatch::iter`]'s iterator: an owned cursor rather than a closure, so its
/// item borrows the batch and not the iterator.
pub struct RowBatchIter<'a> {
    batch: &'a RowBatch,
    next: usize,
    end: usize,
}

impl<'a> Iterator for RowBatchIter<'a> {
    type Item = Row<'a>;

    fn next(&mut self) -> Option<Row<'a>> {
        if self.next == self.end {
            return None;
        }
        let row = self.batch.row(self.next);
        self.next += 1;
        Some(row)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.end - self.next;
        (left, Some(left))
    }
}

impl ExactSizeIterator for RowBatchIter<'_> {}

/// The fields of one row, borrowed from the batch's arena.
#[derive(Clone, Copy, Debug)]
pub struct Row<'a> {
    buf: &'a str,
    fields: &'a [FieldSpan],
}

impl<'a> Row<'a> {
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Field `index`, or `None` when it matched the NULL representation.
    pub fn get(&self, index: usize) -> Option<&'a str> {
        let span = self.fields[index];
        if span.is_null() {
            return None;
        }
        Some(&self.buf[span.start..span.end])
    }

    pub fn iter(&self) -> RowIter<'a> {
        RowIter {
            row: *self,
            next: 0,
        }
    }
}

impl<'a> IntoIterator for Row<'a> {
    type Item = Option<&'a str>;
    type IntoIter = RowIter<'a>;

    fn into_iter(self) -> RowIter<'a> {
        self.iter()
    }
}

/// [`Row::iter`]'s iterator over `Option<&str>` fields.
pub struct RowIter<'a> {
    row: Row<'a>,
    next: usize,
}

impl<'a> Iterator for RowIter<'a> {
    type Item = Option<&'a str>;

    fn next(&mut self) -> Option<Option<&'a str>> {
        if self.next == self.row.fields.len() {
            return None;
        }
        let field = self.row.get(self.next);
        self.next += 1;
        Some(field)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.row.fields.len() - self.next;
        (left, Some(left))
    }
}

impl ExactSizeIterator for RowIter<'_> {}

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
            .map(|row| {
                row.iter()
                    .map(|f| f.map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
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

    /// The split: the head goes to the consumer, the tail comes back rebased.
    #[test]
    fn a_split_batch_keeps_every_row_intact() {
        let all: Vec<Vec<Option<&str>>> = (0..7)
            .map(|i| vec![Some("row"), None, Some(["a", "bb", "ccc"][i % 3])])
            .collect();
        for at in 0..=all.len() {
            let mut head = batch_of(&all);
            let mut tail = RowBatch::new();
            tail.move_rows_from(&head, at);
            head.truncate_rows(at);

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
            let mut head = batch_of(&[vec![Some("a")], vec![Some("b")]]);
            head.push_field("partial");
            head.push_null();

            let mut tail = RowBatch::new();
            tail.move_rows_from(&head, at);
            head.truncate_rows(at);

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
