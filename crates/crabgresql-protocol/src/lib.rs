//! pgwire v3: message codecs for the PostgreSQL frontend/backend protocol.
//!
//! M0 scope: startup phase (StartupMessage / SSLRequest / GSSENCRequest /
//! CancelRequest), trust auth, the simple-query cycle and ErrorResponse.
//! SCRAM, TLS, the extended-query protocol and COPY come later.
//!
//! Reference: https://www.postgresql.org/docs/current/protocol-message-formats.html

mod codec;
pub mod sqlstate;

pub use codec::{BackendWriter, FrontendMessage, FrontendReader, StartupRequest};

pub const PROTOCOL_VERSION_3: i32 = 196608; // 3 << 16
pub const SSL_REQUEST_CODE: i32 = 80877103;
pub const GSSENC_REQUEST_CODE: i32 = 80877104;
pub const CANCEL_REQUEST_CODE: i32 = 80877102;

/// One column in a `RowDescription` message. M0 always reports text format
/// and leaves the table/attribute origin zeroed (no catalog yet).
#[derive(Clone, Debug)]
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
    pub(crate) fn as_byte(self) -> u8 {
        match self {
            TransactionStatus::Idle => b'I',
            TransactionStatus::InTransaction => b'T',
            TransactionStatus::Failed => b'E',
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
