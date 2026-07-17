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

/// One column in a `RowDescription` message. Always reports text format and
/// leaves the table/attribute origin zeroed (no catalog yet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDescription {
    pub name: String,
    pub type_oid: u32,
    pub type_len: i16,
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
