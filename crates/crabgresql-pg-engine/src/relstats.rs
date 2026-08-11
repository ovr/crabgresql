//! What `ANALYZE` measured, one file per relation.
//!
//! PostgreSQL keeps statistics in `pg_statistic`, an ordinary catalog relation
//! with a row per `(relation, column)`: an `ANALYZE` rewrites that relation's
//! own rows and touches nothing else. This store reproduces the property rather
//! than the mechanism — a file per relfilenode under `stats/`, written by
//! `ANALYZE` and read at open.
//!
//! The property is the point. These numbers used to ride in the relation
//! catalog, which is a single file that `RelCatalog::persist` re-encodes and
//! fsyncs *in full* on every DDL. Distributions are the largest thing a
//! measurement produces — up to a hundred most-common values and a hundred
//! histogram bounds per column — so parking them there made every `CREATE
//! TABLE` in the database pay for every other relation's histogram.
//!
//! # Keyed by relfilenode
//!
//! Which also gives `TRUNCATE` its behaviour for free. A truncation swaps the
//! relation to a fresh relfilenode, so it looks up a file that does not exist
//! and reads as never-analyzed — which is what PostgreSQL reports after a
//! `TRUNCATE`, and what the catalog used to arrange by clearing a field. The
//! file left behind belongs to a relfilenode nothing references any more, and
//! goes the same way as the heap file it was named after: swept by
//! `PgEngine::gc_orphan_relfiles` against the catalog's live set.
//!
//! # Durability
//!
//! Statistics are advisory, so this store never fails a statement: a write that
//! cannot land is logged and dropped, and a file that cannot be parsed reads as
//! never-analyzed. What it does guarantee is that a *partial* write is never
//! read back, by landing every file through a temporary and a rename.

use std::path::{Path, PathBuf};

use crabgresql_storage_api::{ColStats, RelStats};
use crabgresql_types::Value;
use crabgresql_types::datum::{decode_datum, encode_datum};

use crate::smgr::RelFileNode;

/// Directory under the data directory holding one file per analyzed relation.
const STATS_SUBDIR: &str = "stats";

/// File header. A file that does not start with it is from a future or foreign
/// writer and is ignored, exactly as an unparseable one is.
const MAGIC: &[u8; 4] = b"RST1";

/// The `ANALYZE` results on disk.
pub struct StatsStore {
    dir: PathBuf,
}

impl StatsStore {
    /// Open (and create) the store under `data_dir`.
    pub fn open(data_dir: &Path) -> std::io::Result<Self> {
        let dir = data_dir.join(STATS_SUBDIR);
        std::fs::create_dir_all(&dir)?;
        Ok(StatsStore { dir })
    }

    fn path(&self, rel: RelFileNode) -> PathBuf {
        self.dir.join(rel.0.to_string())
    }

    /// What `ANALYZE` last measured for `rel`, or `None` if it never ran (or the
    /// file is unreadable — a lost measurement costs plan quality and nothing
    /// else, so it is not worth failing an open over).
    pub fn read(&self, rel: RelFileNode) -> Option<RelStats> {
        let bytes = std::fs::read(self.path(rel)).ok()?;
        decode(&bytes)
    }

    /// Record a fresh measurement. Lands through a temporary and a rename, so a
    /// crash mid-write leaves either the previous measurement or none — never
    /// half of one.
    pub fn write(&self, rel: RelFileNode, stats: &RelStats) -> std::io::Result<()> {
        let path = self.path(rel);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, encode(stats))?;
        std::fs::rename(&tmp, &path)
    }

    /// Forget `rel`'s measurement, for a relation being dropped or a file being
    /// reclaimed. Absent is success: the relation may never have been analyzed.
    pub fn unlink(&self, rel: RelFileNode) {
        let _ = std::fs::remove_file(self.path(rel));
    }

    /// Every relfilenode this store holds a measurement for. The caller
    /// intersects it with the catalog's live set to reclaim what a `TRUNCATE`
    /// or a lost `DROP` left behind.
    pub fn stored_relfilenodes(&self) -> Vec<u32> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()))
            .collect()
    }
}

/// `[magic][crc32c of the payload][payload]`, the payload being
/// `[relpages u32][reltuples f64][ncols u32][column…]`.
///
/// Values go through the same self-describing datum codec the heap writes
/// tuples with, never their SQL text: rendering a value depends on session GUCs
/// (`TimeZone`, `IntervalStyle`), so text written by one session could read back
/// as a different value in another.
///
/// The checksum is what makes reading one safe. A datum is self-delimiting, so
/// its decoder has to trust its input and panics on a buffer that ends
/// mid-value; verifying the whole payload first means the decoder only ever sees
/// bytes this writer produced. Without it a file truncated by a crash would take
/// the process down at the next open — for a statistic, of all things.
fn encode(stats: &RelStats) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&stats.relpages.to_le_bytes());
    out.extend_from_slice(&stats.reltuples.to_bits().to_le_bytes());
    out.extend_from_slice(&(stats.columns.len() as u32).to_le_bytes());
    for column in stats.columns.iter() {
        out.extend_from_slice(&column.null_frac.to_bits().to_le_bytes());
        out.extend_from_slice(&column.avg_width.to_le_bytes());
        out.extend_from_slice(&column.n_distinct.to_bits().to_le_bytes());
        out.extend_from_slice(&column.correlation.to_bits().to_le_bytes());
        out.extend_from_slice(&(column.mcv.len() as u32).to_le_bytes());
        for (value, freq) in &column.mcv {
            encode_datum(value, &mut out);
            out.extend_from_slice(&freq.to_bits().to_le_bytes());
        }
        out.extend_from_slice(&(column.histogram.len() as u32).to_le_bytes());
        for value in &column.histogram {
            encode_datum(value, &mut out);
        }
    }
    let mut file = Vec::with_capacity(MAGIC.len() + 4 + out.len());
    file.extend_from_slice(MAGIC);
    file.extend_from_slice(&crc32c::crc32c(&out).to_le_bytes());
    file.extend_from_slice(&out);
    file
}

/// Read a file back, or `None` if it is truncated, corrupt, or not ours.
///
/// A damaged statistics file has a perfectly good answer — "never analyzed" —
/// unlike the relation catalog, whose loss is fatal anyway. So nothing here
/// raises: the header and the checksum are verified first, and only then are the
/// bytes handed to a decoder that assumes them well-formed.
fn decode(bytes: &[u8]) -> Option<RelStats> {
    let mut d = Dec { b: bytes, p: 0 };
    if d.take(MAGIC.len())? != MAGIC {
        return None;
    }
    let checksum = d.u32()?;
    if crc32c::crc32c(&bytes[d.p..]) != checksum {
        return None;
    }
    let relpages = d.u32()?;
    let reltuples = f64::from_bits(u64::from_le_bytes(d.array()?));
    let ncols = d.u32()? as usize;
    let mut columns = Vec::with_capacity(ncols.min(1024));
    for _ in 0..ncols {
        let null_frac = d.f32()?;
        let avg_width = i32::from_le_bytes(d.array()?);
        let n_distinct = d.f32()?;
        let correlation = d.f32()?;
        let nmcv = d.u32()? as usize;
        let mut mcv = Vec::with_capacity(nmcv.min(1024));
        for _ in 0..nmcv {
            let value = d.datum()?;
            mcv.push((value, d.f32()?));
        }
        let nhist = d.u32()? as usize;
        let mut histogram = Vec::with_capacity(nhist.min(1024));
        for _ in 0..nhist {
            histogram.push(d.datum()?);
        }
        columns.push(ColStats {
            null_frac,
            avg_width,
            n_distinct,
            mcv,
            histogram,
            correlation,
        });
    }
    Some(RelStats {
        relpages,
        reltuples,
        analyzed: true,
        // The measurement, not the relation's size now; the table access method
        // fills that in when it reports (see `RelStats::curpages`).
        curpages: None,
        columns: columns.into(),
    })
}

/// A bounds-checked reader: every accessor answers `None` past the end rather
/// than panicking, because this input is a file that may be truncated.
struct Dec<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Dec<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let slice = self.b.get(self.p..self.p + n)?;
        self.p += n;
        Some(slice)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Some(out)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.array()?))
    }

    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_bits(u32::from_le_bytes(self.array()?)))
    }

    /// One self-describing datum. Safe only because [`decode`] verified the
    /// payload's checksum first: this decoder trusts its input and panics on a
    /// buffer that ends mid-value.
    fn datum(&mut self) -> Option<Value> {
        (self.p < self.b.len()).then(|| decode_datum(self.b, &mut self.p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_types::Value;

    fn measured() -> RelStats {
        RelStats {
            relpages: 17,
            reltuples: 1234.5,
            analyzed: true,
            curpages: None,
            columns: std::sync::Arc::from([
                ColStats {
                    null_frac: 0.25,
                    avg_width: 4,
                    n_distinct: -1.0,
                    // Mixed types on purpose: the datum codec is
                    // self-describing, so nothing here may assume one type.
                    mcv: vec![(Value::Int4(7), 0.5), (Value::Text("x".into()), 0.25)],
                    histogram: vec![Value::Int4(1), Value::Int4(2), Value::Int4(9)],
                    correlation: 0.5,
                },
                ColStats {
                    null_frac: 0.0,
                    avg_width: 8,
                    n_distinct: 3.0,
                    mcv: Vec::new(),
                    histogram: Vec::new(),
                    correlation: -1.0,
                },
            ]),
        }
    }

    #[test]
    fn a_measurement_round_trips() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = StatsStore::open(dir.path())?;
        let rel = RelFileNode(42);
        assert!(store.read(rel).is_none(), "nothing written yet");

        store.write(rel, &measured())?;
        let back = store.read(rel).expect("the measurement reads back");
        assert_eq!(back, measured());

        // A second write replaces the first rather than appending to it.
        let mut smaller = measured();
        smaller.relpages = 1;
        store.write(rel, &smaller)?;
        assert_eq!(store.read(rel).expect("still readable").relpages, 1);
        Ok(())
    }

    #[test]
    fn a_relation_with_no_column_statistics_round_trips() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = StatsStore::open(dir.path())?;
        let stats = RelStats {
            relpages: 3,
            reltuples: 9.0,
            analyzed: true,
            curpages: None,
            columns: std::sync::Arc::from([]),
        };
        store.write(RelFileNode(1), &stats)?;
        assert_eq!(store.read(RelFileNode(1)).expect("reads back"), stats);
        Ok(())
    }

    /// A measurement belongs to a relfilenode, so a `TRUNCATE`'s swap reads as
    /// never-analyzed without anything having to clear it, and the file it left
    /// behind is visible to the sweeper.
    #[test]
    fn a_different_relfilenode_has_no_measurement_and_the_old_file_is_reclaimable()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = StatsStore::open(dir.path())?;
        store.write(RelFileNode(7), &measured())?;
        assert!(store.read(RelFileNode(8)).is_none());

        let mut stored = store.stored_relfilenodes();
        stored.sort_unstable();
        assert_eq!(stored, vec![7]);

        store.unlink(RelFileNode(7));
        assert!(store.read(RelFileNode(7)).is_none());
        assert!(store.stored_relfilenodes().is_empty());
        // Unlinking what is not there is not an error.
        store.unlink(RelFileNode(7));
        Ok(())
    }

    /// A damaged file has a perfectly good answer — "never analyzed" — so it
    /// must not panic or propagate. Every truncation of a valid file is tried,
    /// since a crash can leave any of them.
    #[test]
    fn a_damaged_file_reads_as_never_analyzed() -> anyhow::Result<()> {
        let full = encode(&measured());
        for cut in 0..full.len() {
            assert!(
                decode(&full[..cut]).is_none(),
                "a file truncated to {cut} bytes decoded as a measurement"
            );
        }
        assert!(decode(b"NOPE....").is_none(), "a foreign magic is not ours");
        assert_eq!(decode(&full), Some(measured()));
        Ok(())
    }
}
