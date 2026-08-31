//! pgwire v3: a symmetric message codec for the PostgreSQL frontend/backend
//! protocol. Every message type can be both encoded and decoded, in both
//! directions, so this crate serves a server (read requests, write responses)
//! and a client (write requests, read responses) equally.
//!
//! [`message`] holds the owned message model — [`FrontendMessage`],
//! [`BackendMessage`], [`StartupRequest`] — each with `encode`/`decode` as the
//! single source of truth for the wire layout. [`codec`] wraps them in async
//! read/write helpers ([`FrontendReader`]/[`FrontendWriter`],
//! [`BackendWriter`]/[`BackendReader`]).
//!
//! Reference: https://www.postgresql.org/docs/current/protocol-message-formats.html

mod codec;
mod message;
pub mod sqlstate;

pub use codec::{BackendReader, BackendWriter, FrontendReader, FrontendWriter};
pub use message::{
    AuthRequest, BackendMessage, CopyResponse, ErrorFields, Format, FrontendMessage,
    StartupRequest, Target,
};

pub const PROTOCOL_VERSION_3: i32 = 196608; // 3 << 16
pub const SSL_REQUEST_CODE: i32 = 80877103;
pub const GSSENC_REQUEST_CODE: i32 = 80877104;
pub const CANCEL_REQUEST_CODE: i32 = 80877102;

/// One column in a `RowDescription` message. Carries every field on the wire so
/// a client decoding a real server's RowDescription round-trips losslessly; the
/// server builds query-result columns with [`FieldDescription::new`], which
/// zeroes the catalog origin and reports text format.
///
/// TODO: report the source table OID and attribute number for result columns
/// that are plain table column references — [`FieldDescription::new`] reports 0
/// for both. The server's own builder fills the type modifier in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDescription {
    pub name: String,
    /// OID of the table the column belongs to, or 0 if not a simple column ref.
    pub table_oid: u32,
    /// Attribute number of the column within its table, or 0 if not a ref.
    pub column_id: i16,
    pub type_oid: u32,
    pub type_len: i16,
    /// Type-specific modifier (e.g. `numeric` precision/scale), or -1 if none.
    pub type_modifier: i32,
    /// Transfer format of this column's values (text or binary).
    pub format: Format,
}

impl FieldDescription {
    /// A column with no catalog origin: no table/attnum, no type modifier, text
    /// format.
    pub fn new(name: String, type_oid: u32, type_len: i16) -> Self {
        Self {
            name,
            table_oid: 0,
            column_id: 0,
            type_oid,
            type_len,
            type_modifier: -1,
            format: Format::Text,
        }
    }
}

/// Transaction status carried in `ReadyForQuery`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionStatus {
    Idle,
    InTransaction,
    Failed,
}

impl TransactionStatus {
    pub fn as_byte(self) -> u8 {
        match self {
            TransactionStatus::Idle => b'I',
            TransactionStatus::InTransaction => b'T',
            TransactionStatus::Failed => b'E',
        }
    }

    pub fn from_byte(b: u8) -> Result<Self, ProtocolError> {
        match b {
            b'I' => Ok(TransactionStatus::Idle),
            b'T' => Ok(TransactionStatus::InTransaction),
            b'E' => Ok(TransactionStatus::Failed),
            other => Err(ProtocolError::Malformed(format!(
                "invalid transaction status {:?}",
                other as char
            ))),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported protocol version {0}")]
    UnsupportedProtocolVersion(i32),
    #[error("malformed message: {0}")]
    Malformed(String),
}
