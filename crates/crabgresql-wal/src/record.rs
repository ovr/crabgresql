//! The WAL record envelope and the [`Lsn`] type.

use crabgresql_txn::Xid;

/// A position in the logical WAL byte stream — PostgreSQL's `XLogRecPtr`. It is
/// a plain byte offset into the stream, so a record that starts at offset `o`
/// with length `n` has start-LSN `o` and **end-LSN `o + n`**. [`Lsn::INVALID`]
/// (`0`) doubles as "empty stream / never written": no record's end-LSN is ever
/// `0` (a record is always at least [`WalRecord::HEADER_LEN`]` + 4` bytes), and
/// a freshly initialized data page carries `pd_lsn = 0`, which is therefore
/// older than any real record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Lsn(pub u64);

impl Lsn {
    pub const INVALID: Lsn = Lsn(0);

    pub fn is_valid(self) -> bool {
        self.0 != 0
    }
}

impl std::fmt::Display for Lsn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // PG-style X/Y hex, purely for logs.
        write!(f, "{:X}/{:X}", self.0 >> 32, self.0 & 0xffff_ffff)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error("wal io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wal record for resource manager {0} has no registered redo handler")]
    UnknownRmgr(u8),
    #[error("wal redo failed: {0}")]
    Redo(String),
}

/// One decoded WAL record, borrowing its payload from the replay buffer.
///
/// On-disk layout (little-endian), header is [`WalRecord::HEADER_LEN`] = 24
/// bytes, followed by the payload and a trailing CRC-32C:
///
/// | Off | Size | Field |
/// |----|----|----|
/// | 0  | 4 | `total_len` (whole record incl. this field and the trailing CRC) |
/// | 4  | 8 | `prev_lsn` (start-LSN of the previous record; `0` for the first) |
/// | 12 | 8 | `xid` (owning transaction, `0` = none, e.g. a checkpoint) |
/// | 20 | 1 | `rmgr_id` |
/// | 21 | 1 | `info` (rmgr-private opcode/flags) |
/// | 22 | 2 | reserved (`0`) |
/// | 24 | N | `payload` |
/// | 24+N | 4 | `crc` (CRC-32C over bytes `[0 .. total_len-4]`) |
#[derive(Clone, Debug)]
pub struct WalRecord<'a> {
    pub prev_lsn: Lsn,
    pub xid: Xid,
    pub rmgr: u8,
    pub info: u8,
    pub payload: &'a [u8],
}

impl<'a> WalRecord<'a> {
    pub const HEADER_LEN: usize = 24;
    const CRC_LEN: usize = 4;

    /// Serialize this record onto the end of `buf`, given the LSN at which it
    /// starts, and return its total encoded length.
    pub fn encode(&self, start: Lsn, buf: &mut Vec<u8>) -> usize {
        let total_len = Self::HEADER_LEN + self.payload.len() + Self::CRC_LEN;
        let begin = buf.len();
        buf.extend_from_slice(&(total_len as u32).to_le_bytes());
        buf.extend_from_slice(&self.prev_lsn.0.to_le_bytes());
        buf.extend_from_slice(&self.xid.0.to_le_bytes());
        buf.push(self.rmgr);
        buf.push(self.info);
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(self.payload);
        let crc = crc32c::crc32c(&buf[begin..begin + Self::HEADER_LEN + self.payload.len()]);
        buf.extend_from_slice(&crc.to_le_bytes());
        debug_assert_eq!(buf.len() - begin, total_len);
        let _ = start; // start LSN is the caller's bookkeeping; not stored in the record
        total_len
    }

    /// Parse one record at the front of `bytes`. Returns:
    /// - `Ok(Some((record, len)))` on a complete, CRC-valid record;
    /// - `Ok(None)` when `bytes` is too short, `total_len` is nonsensical, or the
    ///   CRC does not match — all of which mean "the log validly ends here"
    ///   (a crash tore the tail); recovery stops cleanly at that point.
    pub fn decode(bytes: &'a [u8]) -> Option<(WalRecord<'a>, usize)> {
        if bytes.len() < Self::HEADER_LEN + Self::CRC_LEN {
            return None;
        }
        let total_len = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        if total_len < Self::HEADER_LEN + Self::CRC_LEN || total_len > bytes.len() {
            return None;
        }
        let stored_crc = u32::from_le_bytes(bytes[total_len - 4..total_len].try_into().unwrap());
        let actual_crc = crc32c::crc32c(&bytes[0..total_len - 4]);
        if stored_crc != actual_crc {
            return None;
        }
        let prev_lsn = Lsn(u64::from_le_bytes(bytes[4..12].try_into().unwrap()));
        let xid = Xid(u64::from_le_bytes(bytes[12..20].try_into().unwrap()));
        let rmgr = bytes[20];
        let info = bytes[21];
        let payload = &bytes[Self::HEADER_LEN..total_len - 4];
        Some((WalRecord { prev_lsn, xid, rmgr, info, payload }, total_len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_various_payloads() {
        for payload in [vec![], vec![0u8], vec![1, 2, 3, 4, 5], vec![7u8; 5000]] {
            let rec = WalRecord {
                prev_lsn: Lsn(42),
                xid: Xid(9),
                rmgr: 10,
                info: 0x01,
                payload: &payload,
            };
            let mut buf = Vec::new();
            let n = rec.encode(Lsn(0), &mut buf);
            assert_eq!(n, buf.len());
            let (got, len) = WalRecord::decode(&buf).expect("decodes");
            assert_eq!(len, n);
            assert_eq!(got.prev_lsn, Lsn(42));
            assert_eq!(got.xid, Xid(9));
            assert_eq!(got.rmgr, 10);
            assert_eq!(got.info, 0x01);
            assert_eq!(got.payload, &payload[..]);
        }
    }

    #[test]
    fn corruption_is_treated_as_end_of_log() {
        let rec = WalRecord {
            prev_lsn: Lsn(0),
            xid: Xid(3),
            rmgr: 10,
            info: 0,
            payload: &[1, 2, 3, 4],
        };
        let mut buf = Vec::new();
        rec.encode(Lsn(0), &mut buf);
        // Flip a payload byte -> CRC mismatch -> None (clean end).
        let mut bad = buf.clone();
        bad[WalRecord::HEADER_LEN] ^= 0xff;
        assert!(WalRecord::decode(&bad).is_none());
        // A zeroed / short tail also decodes to None.
        assert!(WalRecord::decode(&[0u8; 8]).is_none());
        assert!(WalRecord::decode(&buf[..buf.len() - 1]).is_none());
    }

    #[test]
    fn two_records_back_to_back() {
        let mut buf = Vec::new();
        let a = WalRecord { prev_lsn: Lsn(0), xid: Xid(3), rmgr: 1, info: 0, payload: &[9] };
        let b = WalRecord { prev_lsn: Lsn(0), xid: Xid(4), rmgr: 2, info: 5, payload: &[8, 8] };
        let la = a.encode(Lsn(0), &mut buf);
        let lb = b.encode(Lsn(la as u64), &mut buf);
        let (ra, na) = WalRecord::decode(&buf).unwrap();
        assert_eq!(na, la);
        assert_eq!(ra.xid, Xid(3));
        let (rb, nb) = WalRecord::decode(&buf[na..]).unwrap();
        assert_eq!(nb, lb);
        assert_eq!(rb.xid, Xid(4));
        assert_eq!(rb.info, 5);
    }
}
