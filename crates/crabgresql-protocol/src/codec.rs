//! Async reading of frontend messages and writing of backend messages.

use std::collections::HashMap;

use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{
    CANCEL_REQUEST_CODE, FieldDescription, GSSENC_REQUEST_CODE, PROTOCOL_VERSION_3, ProtocolError,
    SSL_REQUEST_CODE, TransactionStatus,
};

/// Upper bound on any frontend message body; a startup packet or query longer
/// than this is treated as a protocol violation rather than an allocation.
const MAX_MESSAGE_LEN: usize = 64 * 1024 * 1024;

/// The first packet(s) a client sends before the connection is established.
#[derive(Debug)]
pub enum StartupRequest {
    /// SSLRequest — M0 answers `N` (no TLS) and the client retries in clear.
    Ssl,
    /// GSSENCRequest — likewise refused with `N`.
    GssEnc,
    /// CancelRequest — arrives on a separate connection which is then closed.
    Cancel { pid: i32, secret: i32 },
    /// The real StartupMessage with `user`, `database`, options.
    Startup { params: HashMap<String, String> },
}

/// Messages a frontend can send after startup. Anything M0 does not handle is
/// surfaced as `Unsupported` so the session can answer with a proper error.
#[derive(Debug)]
pub enum FrontendMessage {
    Query(String),
    /// Extended-protocol Sync: always answered with ReadyForQuery, and ends
    /// the error-recovery skip after a failed extended-protocol message.
    Sync,
    Terminate,
    Unsupported(u8),
}

pub struct FrontendReader<R> {
    inner: R,
}

impl<R: AsyncRead + Unpin> FrontendReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Read one startup-phase packet (it has no tag byte). The caller loops:
    /// after refusing `Ssl`/`GssEnc` the client sends another startup packet.
    pub async fn read_startup(&mut self) -> Result<StartupRequest, ProtocolError> {
        let len = self.inner.read_i32().await? as usize;
        if !(8..=MAX_MESSAGE_LEN).contains(&len) {
            return Err(ProtocolError::Malformed(format!(
                "invalid startup packet length {len}"
            )));
        }
        let mut body = vec![0u8; len - 4];
        self.inner.read_exact(&mut body).await?;
        let code = i32::from_be_bytes(body[..4].try_into().unwrap());

        match code {
            SSL_REQUEST_CODE => Ok(StartupRequest::Ssl),
            GSSENC_REQUEST_CODE => Ok(StartupRequest::GssEnc),
            CANCEL_REQUEST_CODE => {
                if body.len() < 12 {
                    return Err(ProtocolError::Malformed("short CancelRequest".into()));
                }
                Ok(StartupRequest::Cancel {
                    pid: i32::from_be_bytes(body[4..8].try_into().unwrap()),
                    secret: i32::from_be_bytes(body[8..12].try_into().unwrap()),
                })
            }
            PROTOCOL_VERSION_3 => Ok(StartupRequest::Startup {
                params: parse_startup_params(&body[4..])?,
            }),
            other => Err(ProtocolError::UnsupportedProtocolVersion(other)),
        }
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
        if !(4..=MAX_MESSAGE_LEN).contains(&len) {
            return Err(ProtocolError::Malformed(format!(
                "invalid message length {len}"
            )));
        }
        let mut body = vec![0u8; len - 4];
        self.inner.read_exact(&mut body).await?;

        match tag {
            b'Q' => Ok(Some(FrontendMessage::Query(read_cstr(&body)?))),
            b'S' => Ok(Some(FrontendMessage::Sync)),
            b'X' => Ok(Some(FrontendMessage::Terminate)),
            other => Ok(Some(FrontendMessage::Unsupported(other))),
        }
    }
}

/// Startup body: a sequence of `key\0value\0` pairs closed by an extra `\0`.
fn parse_startup_params(mut body: &[u8]) -> Result<HashMap<String, String>, ProtocolError> {
    let mut params = HashMap::new();
    loop {
        match body.first() {
            None | Some(0) => return Ok(params),
            _ => {}
        }
        let key = take_cstr(&mut body)?;
        let value = take_cstr(&mut body)?;
        params.insert(key, value);
    }
}

fn take_cstr(buf: &mut &[u8]) -> Result<String, ProtocolError> {
    let end = buf
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| ProtocolError::Malformed("unterminated string".into()))?;
    let s = String::from_utf8(buf[..end].to_vec())
        .map_err(|_| ProtocolError::Malformed("invalid utf-8".into()))?;
    *buf = &buf[end + 1..];
    Ok(s)
}

fn read_cstr(buf: &[u8]) -> Result<String, ProtocolError> {
    let mut view = buf;
    take_cstr(&mut view)
}

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

    /// Single-byte reply to SSLRequest / GSSENCRequest: we do not speak TLS.
    pub async fn refuse_encryption(&mut self) -> Result<(), ProtocolError> {
        self.buf.put_u8(b'N');
        self.flush().await
    }

    pub fn authentication_ok(&mut self) {
        self.message(b'R', |body| body.put_i32(0));
    }

    pub fn parameter_status(&mut self, name: &str, value: &str) {
        self.message(b'S', |body| {
            put_cstr(body, name);
            put_cstr(body, value);
        });
    }

    pub fn backend_key_data(&mut self, pid: i32, secret: i32) {
        self.message(b'K', |body| {
            body.put_i32(pid);
            body.put_i32(secret);
        });
    }

    pub fn ready_for_query(&mut self, status: TransactionStatus) {
        self.message(b'Z', |body| body.put_u8(status.as_byte()));
    }

    pub fn row_description(&mut self, fields: &[FieldDescription]) {
        self.message(b'T', |body| {
            body.put_i16(fields.len() as i16);
            for f in fields {
                put_cstr(body, &f.name);
                body.put_u32(0); // table oid — no catalog yet
                body.put_i16(0); // attribute number
                body.put_u32(f.type_oid);
                body.put_i16(f.type_len);
                body.put_i32(-1); // typmod
                body.put_i16(0); // format: text
            }
        });
    }

    /// `None` encodes SQL NULL (length -1 on the wire).
    pub fn data_row(&mut self, columns: &[Option<String>]) {
        self.message(b'D', |body| {
            body.put_i16(columns.len() as i16);
            for col in columns {
                match col {
                    None => body.put_i32(-1),
                    Some(text) => {
                        body.put_i32(text.len() as i32);
                        body.put_slice(text.as_bytes());
                    }
                }
            }
        });
    }

    pub fn command_complete(&mut self, tag: &str) {
        self.message(b'C', |body| put_cstr(body, tag));
    }

    pub fn empty_query_response(&mut self) {
        self.message(b'I', |_| {});
    }

    /// ErrorResponse with severity ERROR. `code` is a 5-char SQLSTATE.
    pub fn error_response(&mut self, code: &str, message: &str) {
        self.error_like(b'E', "ERROR", code, message, None, None);
    }

    /// ErrorResponse carrying a cursor position (`P` field, 1-based character
    /// offset) — rendered by clients as `LINE n: ... ^`.
    pub fn error_response_at(&mut self, code: &str, message: &str, position: Option<usize>) {
        self.error_like(b'E', "ERROR", code, message, None, position);
    }

    /// NoticeResponse (severity NOTICE) with optional DETAIL and position.
    pub fn notice_response(
        &mut self,
        code: &str,
        message: &str,
        detail: Option<&str>,
        position: Option<usize>,
    ) {
        self.error_like(b'N', "NOTICE", code, message, detail, position);
    }

    fn error_like(
        &mut self,
        tag: u8,
        severity: &str,
        code: &str,
        message: &str,
        detail: Option<&str>,
        position: Option<usize>,
    ) {
        self.message(tag, |body| {
            body.put_u8(b'S');
            put_cstr(body, severity);
            body.put_u8(b'V');
            put_cstr(body, severity);
            body.put_u8(b'C');
            put_cstr(body, code);
            body.put_u8(b'M');
            put_cstr(body, message);
            if let Some(detail) = detail {
                body.put_u8(b'D');
                put_cstr(body, detail);
            }
            if let Some(position) = position {
                body.put_u8(b'P');
                put_cstr(body, &position.to_string());
            }
            body.put_u8(0);
        });
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

    fn message(&mut self, tag: u8, write_body: impl FnOnce(&mut BytesMut)) {
        self.buf.put_u8(tag);
        let len_at = self.buf.len();
        self.buf.put_i32(0); // patched below
        write_body(&mut self.buf);
        let len = (self.buf.len() - len_at) as i32;
        self.buf[len_at..len_at + 4].copy_from_slice(&len.to_be_bytes());
    }
}

fn put_cstr(buf: &mut BytesMut, s: &str) {
    buf.put_slice(s.as_bytes());
    buf.put_u8(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn startup_packet(params: &[(&str, &str)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&PROTOCOL_VERSION_3.to_be_bytes());
        for (k, v) in params {
            body.extend_from_slice(k.as_bytes());
            body.push(0);
            body.extend_from_slice(v.as_bytes());
            body.push(0);
        }
        body.push(0);
        let mut packet = ((body.len() + 4) as i32).to_be_bytes().to_vec();
        packet.extend_from_slice(&body);
        packet
    }

    #[tokio::test]
    async fn parses_startup_message() {
        let packet = startup_packet(&[("user", "alice"), ("database", "db1")]);
        let mut reader = FrontendReader::new(packet.as_slice());
        match reader.read_startup().await.unwrap() {
            StartupRequest::Startup { params } => {
                assert_eq!(params["user"], "alice");
                assert_eq!(params["database"], "db1");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn parses_ssl_request() {
        let packet = [0, 0, 0, 8, 4, 210, 22, 47]; // len 8, code 80877103
        let mut reader = FrontendReader::new(packet.as_slice());
        assert!(matches!(
            reader.read_startup().await.unwrap(),
            StartupRequest::Ssl
        ));
    }

    #[tokio::test]
    async fn rejects_protocol_2() {
        let mut packet = 8i32.to_be_bytes().to_vec();
        packet.extend_from_slice(&(2i32 << 16).to_be_bytes());
        let mut reader = FrontendReader::new(packet.as_slice());
        assert!(matches!(
            reader.read_startup().await,
            Err(ProtocolError::UnsupportedProtocolVersion(_))
        ));
    }

    #[tokio::test]
    async fn parses_query_message() {
        let sql = b"SELECT 1\0";
        let mut packet = vec![b'Q'];
        packet.extend_from_slice(&((sql.len() + 4) as i32).to_be_bytes());
        packet.extend_from_slice(sql);
        let mut reader = FrontendReader::new(packet.as_slice());
        match reader.read_message().await.unwrap().unwrap() {
            FrontendMessage::Query(q) => assert_eq!(q, "SELECT 1"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn clean_eof_is_none() {
        let mut reader = FrontendReader::new(&[][..]);
        assert!(reader.read_message().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn error_response_layout() {
        let mut writer = BackendWriter::new(Vec::new());
        writer.error_response("42601", "syntax error");
        writer.flush().await.unwrap();
        let out = writer.inner;
        assert_eq!(out[0], b'E');
        let len = i32::from_be_bytes(out[1..5].try_into().unwrap()) as usize;
        assert_eq!(out.len(), 1 + len);
        assert!(out.windows(6).any(|w| w == b"42601\0"));
    }

    #[tokio::test]
    async fn data_row_encodes_null_as_minus_one() {
        let mut writer = BackendWriter::new(Vec::new());
        writer.data_row(&[None]);
        writer.flush().await.unwrap();
        let out = writer.inner;
        // tag, len=10, ncols=1, collen=-1
        assert_eq!(out, [b'D', 0, 0, 0, 10, 0, 1, 255, 255, 255, 255]);
    }
}
