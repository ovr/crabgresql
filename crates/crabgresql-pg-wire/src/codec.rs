//! Async read/write wrappers around the message model in [`crate::message`].
//!
//! Reads frame a message off the wire (a startup packet, or a `tag + length +
//! body` message) and hand the body to the matching `decode`; writes buffer the
//! matching `encode` into a [`BytesMut`] and flush it. All four combinations
//! exist so either side of the protocol can be built from this crate:
//!
//! - Server: [`FrontendReader`] (requests in) + [`BackendWriter`] (responses out)
//! - Client: [`FrontendWriter`] (requests out) + [`BackendReader`] (responses in)

use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::message::{put_data_row, put_row_description};
use crate::{
    AuthRequest, BackendMessage, ErrorFields, FieldDescription, FrontendMessage, ProtocolError,
    StartupRequest, TransactionStatus,
};

/// Upper bound on any message body; a startup packet or message longer than
/// this is treated as a protocol violation rather than an allocation.
const MAX_MESSAGE_LEN: usize = 64 * 1024 * 1024;

/// Read a message body: `len` (the self-inclusive length prefix, already
/// consumed) minus the four length bytes, rejecting absurd lengths first.
async fn read_body<R: AsyncRead + Unpin>(
    inner: &mut R,
    len: usize,
    min: usize,
) -> Result<Vec<u8>, ProtocolError> {
    if !(min..=MAX_MESSAGE_LEN).contains(&len) {
        return Err(ProtocolError::Malformed(format!(
            "invalid message length {len}"
        )));
    }
    let mut body = vec![0u8; len - 4];
    inner.read_exact(&mut body).await?;
    Ok(body)
}

// ---------------------------------------------------------------------------
// Frontend side
// ---------------------------------------------------------------------------

/// Reads frontend messages from a client (server side).
pub struct FrontendReader<R> {
    inner: R,
}

impl<R: AsyncRead + Unpin> FrontendReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Read one startup-phase packet (it has no tag byte). `None` means the
    /// client opened the connection and closed it without sending anything —
    /// a clean disconnect (health checks, port scans), not an error. The caller
    /// loops: after refusing `Ssl`/`GssEnc` the client sends another packet.
    pub async fn read_startup(&mut self) -> Result<Option<StartupRequest>, ProtocolError> {
        let len = match self.inner.read_i32().await {
            Ok(len) => len as usize,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let body = read_body(&mut self.inner, len, 8).await?;
        Ok(Some(StartupRequest::decode(&body)?))
    }

    /// Read one regular (tagged) frontend message. `None` means the client
    /// closed the connection cleanly between messages.
    pub async fn read_message(&mut self) -> Result<Option<FrontendMessage>, ProtocolError> {
        let tag = match self.inner.read_u8().await {
            Ok(tag) => tag,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let len = self.inner.read_i32().await? as usize;
        let body = read_body(&mut self.inner, len, 4).await?;
        // The frame is fully consumed, so the stream stays in sync even when the
        // body is malformed. Surface a body we can frame but not parse (bad
        // target/format byte, truncation) as `Unknown` rather than a hard error,
        // so the server answers it like any unsupported message and keeps the
        // connection alive. Framing and IO errors above stay fatal.
        let message = match FrontendMessage::decode(tag, &body) {
            Ok(message) => message,
            Err(_) => FrontendMessage::Unknown { tag, body },
        };
        Ok(Some(message))
    }
}

/// Writes frontend messages to a server (client side).
pub struct FrontendWriter<W> {
    inner: W,
    buf: BytesMut,
}

impl<W: AsyncWrite + Unpin> FrontendWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            buf: BytesMut::with_capacity(8 * 1024),
        }
    }

    /// Buffer a startup-phase packet (StartupMessage / SSLRequest / …).
    pub fn write_startup(&mut self, request: &StartupRequest) {
        request.encode(&mut self.buf);
    }

    /// Buffer a regular frontend message.
    pub fn write_message(&mut self, message: &FrontendMessage) {
        message.encode(&mut self.buf);
    }

    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    pub async fn flush(&mut self) -> Result<(), ProtocolError> {
        self.inner.write_all(&self.buf).await?;
        self.buf.clear();
        self.inner.flush().await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Backend side
// ---------------------------------------------------------------------------

/// Writes backend messages to a client (server side). The convenience methods
/// cover the messages the server emits today; [`BackendWriter::write`] handles
/// any [`BackendMessage`].
pub struct BackendWriter<W> {
    inner: W,
    buf: BytesMut,
}

impl<W: AsyncWrite + Unpin> BackendWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            buf: BytesMut::with_capacity(8 * 1024),
        }
    }

    /// Buffer any backend message.
    pub fn write(&mut self, message: &BackendMessage) {
        message.encode(&mut self.buf);
    }

    /// Single-byte reply to SSLRequest / GSSENCRequest: we do not speak TLS.
    /// This is a bare `N`, not a framed message, so it is written directly.
    pub async fn refuse_encryption(&mut self) -> Result<(), ProtocolError> {
        self.buf.put_u8(b'N');
        self.flush().await
    }

    pub fn authentication_ok(&mut self) {
        self.write(&BackendMessage::Authentication(AuthRequest::Ok));
    }

    pub fn parameter_status(&mut self, name: &str, value: &str) {
        self.write(&BackendMessage::ParameterStatus {
            name: name.to_string(),
            value: value.to_string(),
        });
    }

    pub fn backend_key_data(&mut self, pid: i32, secret: i32) {
        self.write(&BackendMessage::BackendKeyData { pid, secret });
    }

    pub fn ready_for_query(&mut self, status: TransactionStatus) {
        self.write(&BackendMessage::ReadyForQuery(status));
    }

    pub fn row_description(&mut self, fields: &[FieldDescription]) {
        // Shared layout with `BackendMessage::RowDescription` encode.
        put_row_description(&mut self.buf, fields);
    }

    /// `None` encodes SQL NULL (length -1 on the wire). Text columns are written
    /// as their UTF-8 bytes without cloning into owned buffers.
    pub fn data_row(&mut self, columns: &[Option<String>]) {
        put_data_row(
            &mut self.buf,
            columns.iter().map(|c| c.as_deref().map(str::as_bytes)),
        );
    }

    pub fn command_complete(&mut self, tag: &str) {
        self.write(&BackendMessage::CommandComplete(tag.to_string()));
    }

    pub fn empty_query_response(&mut self) {
        self.write(&BackendMessage::EmptyQueryResponse);
    }

    /// ErrorResponse with severity ERROR. `code` is a 5-char SQLSTATE.
    pub fn error_response(&mut self, code: &str, message: &str) {
        self.write(&BackendMessage::ErrorResponse(ErrorFields::error(
            code, message,
        )));
    }

    /// ErrorResponse carrying a cursor position (`P` field, 1-based character
    /// offset) — rendered by clients as `LINE n: ... ^`.
    pub fn error_response_at(&mut self, code: &str, message: &str, position: Option<usize>) {
        let mut fields = ErrorFields::error(code, message);
        if let Some(position) = position {
            fields = fields.with_position(position);
        }
        self.write(&BackendMessage::ErrorResponse(fields));
    }

    /// NoticeResponse (severity NOTICE) with optional DETAIL and position.
    pub fn notice_response(
        &mut self,
        code: &str,
        message: &str,
        detail: Option<&str>,
        position: Option<usize>,
    ) {
        let mut fields = ErrorFields::notice(code, message);
        if let Some(detail) = detail {
            fields = fields.with_detail(detail);
        }
        if let Some(position) = position {
            fields = fields.with_position(position);
        }
        self.write(&BackendMessage::NoticeResponse(fields));
    }

    /// Bytes queued but not yet flushed — lets callers flush in batches when
    /// streaming large result sets.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    pub async fn flush(&mut self) -> Result<(), ProtocolError> {
        self.inner.write_all(&self.buf).await?;
        self.buf.clear();
        self.inner.flush().await?;
        Ok(())
    }
}

/// Reads backend messages from a server (client side).
pub struct BackendReader<R> {
    inner: R,
}

impl<R: AsyncRead + Unpin> BackendReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Read one backend message. `None` means the server closed the connection
    /// cleanly between messages.
    pub async fn read_message(&mut self) -> Result<Option<BackendMessage>, ProtocolError> {
        let tag = match self.inner.read_u8().await {
            Ok(tag) => tag,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let len = self.inner.read_i32().await? as usize;
        let body = read_body(&mut self.inner, len, 4).await?;
        Ok(Some(BackendMessage::decode(tag, &body)?))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[tokio::test]
    async fn frontend_reader_reads_startup_then_query() {
        // Encode a startup packet + a Query the way a client would, then read
        // them back through the server-side reader.
        let mut out = BytesMut::new();
        StartupRequest::Startup {
            params: HashMap::from([("user".to_string(), "alice".to_string())]),
        }
        .encode(&mut out);
        FrontendMessage::Query("SELECT 1".to_string()).encode(&mut out);
        let bytes = out.to_vec();

        let mut reader = FrontendReader::new(bytes.as_slice());
        match reader.read_startup().await.unwrap().unwrap() {
            StartupRequest::Startup { params } => assert_eq!(params["user"], "alice"),
            other => panic!("unexpected: {other:?}"),
        }
        match reader.read_message().await.unwrap().unwrap() {
            FrontendMessage::Query(q) => assert_eq!(q, "SELECT 1"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn clean_eof_is_none_both_directions() {
        // A client that connects and closes without sending is a clean
        // disconnect at every read entry point, not an error.
        let mut fr = FrontendReader::new(&[][..]);
        assert!(fr.read_startup().await.unwrap().is_none());
        assert!(fr.read_message().await.unwrap().is_none());
        let mut br = BackendReader::new(&[][..]);
        assert!(br.read_message().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn malformed_extended_body_becomes_unknown_not_error() {
        // A Bind ('B') whose body is truncated must not fail the read (which
        // would drop the connection); the frame is consumed and the message is
        // surfaced as Unknown so the server can answer it and stay alive.
        let mut out = BytesMut::new();
        out.put_u8(b'B');
        out.put_i32(4 + 3); // length covers itself + 3 body bytes
        out.put_slice(b"xyz"); // not a valid Bind body
        let bytes = out.to_vec();
        let mut reader = FrontendReader::new(bytes.as_slice());
        match reader.read_message().await.unwrap().unwrap() {
            FrontendMessage::Unknown { tag, .. } => assert_eq!(tag, b'B'),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn refuse_encryption_writes_single_byte() {
        let mut writer = BackendWriter::new(Vec::new());
        writer.refuse_encryption().await.unwrap();
        assert_eq!(writer.inner, b"N");
    }

    #[tokio::test]
    async fn backend_writer_convenience_matches_enum_encode() {
        // The `data_row` convenience method must produce the same bytes as
        // `BackendMessage::DataRow` so there is one wire layout, not two.
        let mut writer = BackendWriter::new(Vec::new());
        writer.data_row(&[Some("x".to_string()), None]);
        writer.flush().await.unwrap();

        let mut expected = BytesMut::new();
        BackendMessage::DataRow(vec![Some(b"x".to_vec()), None]).encode(&mut expected);
        assert_eq!(writer.inner, expected.to_vec());
    }
}
