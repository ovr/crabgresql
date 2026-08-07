//! Owned, symmetric message model for the PostgreSQL v3 frontend/backend
//! protocol. Each message type has both `encode` (append a fully framed message
//! to a buffer) and `decode` (parse one message body), so the same layout serves
//! a server reading requests / writing responses and a client doing the reverse.
//!
//! Layouts follow the published message-format reference and are pinned by the
//! round-trip and exact-byte tests, not ported from PostgreSQL's C source.
//!
//! Reference: https://www.postgresql.org/docs/current/protocol-message-formats.html

use std::collections::HashMap;

use bytes::{BufMut, BytesMut};

use crate::{
    CANCEL_REQUEST_CODE, FieldDescription, GSSENC_REQUEST_CODE, PROTOCOL_VERSION_3, ProtocolError,
    SSL_REQUEST_CODE, TransactionStatus,
};

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

/// Append a NUL-terminated string.
pub(crate) fn put_cstr(buf: &mut BytesMut, s: &str) {
    buf.put_slice(s.as_bytes());
    buf.put_u8(0);
}

/// Write a tagged message: one tag byte, an `i32` length covering the length
/// field and body (but not the tag), then the body produced by `f`.
fn framed(buf: &mut BytesMut, tag: u8, f: impl FnOnce(&mut BytesMut)) {
    buf.put_u8(tag);
    framed_untagged(buf, f);
}

/// Write an untagged (startup-phase) packet: an `i32` length covering the length
/// field and body, then the body produced by `f`.
fn framed_untagged(buf: &mut BytesMut, f: impl FnOnce(&mut BytesMut)) {
    let len_at = buf.len();
    buf.put_i32(0); // patched below
    f(buf);
    let len = (buf.len() - len_at) as i32;
    buf[len_at..len_at + 4].copy_from_slice(&len.to_be_bytes());
}

/// A forward-only cursor over a message body with bounds-checked reads. Every
/// short read is a `Malformed` protocol error rather than a panic.
struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtocolError> {
        if self.buf.len() < n {
            return Err(ProtocolError::Malformed("truncated message".into()));
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ProtocolError> {
        self.take(N)?
            .try_into()
            .map_err(|_| ProtocolError::Malformed("truncated message".into()))
    }

    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }

    fn i16(&mut self) -> Result<i16, ProtocolError> {
        Ok(i16::from_be_bytes(self.array()?))
    }

    fn i32(&mut self) -> Result<i32, ProtocolError> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, ProtocolError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn cstr(&mut self) -> Result<String, ProtocolError> {
        let end = self
            .buf
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| ProtocolError::Malformed("unterminated string".into()))?;
        let s = String::from_utf8(self.buf[..end].to_vec())
            .map_err(|_| ProtocolError::Malformed("invalid utf-8".into()))?;
        self.buf = &self.buf[end + 1..];
        Ok(s)
    }

    /// Consume and return the remaining bytes.
    fn rest(&mut self) -> Vec<u8> {
        let out = self.buf.to_vec();
        self.buf = &[];
        out
    }

    /// A length-prefixed value: `i32` length then that many bytes, with -1
    /// meaning SQL NULL / "no value".
    fn opt_bytes(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        let len = self.i32()?;
        if len < 0 {
            Ok(None)
        } else {
            Ok(Some(self.take(len as usize)?.to_vec()))
        }
    }

    /// A non-negative `i16` element count. A negative value is a malformed or
    /// oversized (wrapped past `i16::MAX`) message; reject it rather than
    /// clamping to 0, which would silently drop the elements and then misread
    /// the rest of the body.
    fn count(&mut self) -> Result<usize, ProtocolError> {
        let n = self.i16()?;
        if n < 0 {
            return Err(ProtocolError::Malformed(format!(
                "negative element count {n}"
            )));
        }
        Ok(n as usize)
    }
}

// ---------------------------------------------------------------------------
// Small shared value types
// ---------------------------------------------------------------------------

/// Column value transfer format, used in Bind, RowDescription and the COPY
/// response messages. `Text` is 0 on the wire, `Binary` is 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    Binary,
}

impl Format {
    fn from_i16(v: i16) -> Result<Self, ProtocolError> {
        match v {
            0 => Ok(Format::Text),
            1 => Ok(Format::Binary),
            other => Err(ProtocolError::Malformed(format!(
                "invalid format code {other}"
            ))),
        }
    }

    fn as_i16(self) -> i16 {
        match self {
            Format::Text => 0,
            Format::Binary => 1,
        }
    }
}

/// The object a Describe or Close message targets: a prepared `Statement` (`S`)
/// or a `Portal` (`P`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Statement,
    Portal,
}

impl Target {
    fn from_byte(b: u8) -> Result<Self, ProtocolError> {
        match b {
            b'S' => Ok(Target::Statement),
            b'P' => Ok(Target::Portal),
            other => Err(ProtocolError::Malformed(format!(
                "invalid describe/close target {:?}",
                other as char
            ))),
        }
    }

    fn as_byte(self) -> u8 {
        match self {
            Target::Statement => b'S',
            Target::Portal => b'P',
        }
    }
}

fn put_format_list(body: &mut BytesMut, formats: &[Format]) {
    body.put_i16(formats.len() as i16);
    for f in formats {
        body.put_i16(f.as_i16());
    }
}

fn read_format_list(r: &mut Reader) -> Result<Vec<Format>, ProtocolError> {
    let count = r.count()?;
    let mut formats = Vec::with_capacity(count);
    for _ in 0..count {
        formats.push(Format::from_i16(r.i16()?)?);
    }
    Ok(formats)
}

/// Append a length-prefixed optional value: `i32` length then the bytes, with
/// `None` written as length -1 (SQL NULL / "no value"). Mirrors the read side,
/// [`Reader::opt_bytes`].
fn put_opt_bytes(buf: &mut BytesMut, value: Option<&[u8]>) {
    match value {
        None => buf.put_i32(-1),
        Some(bytes) => {
            buf.put_i32(bytes.len() as i32);
            buf.put_slice(bytes);
        }
    }
}

// ---------------------------------------------------------------------------
// Error / notice fields
// ---------------------------------------------------------------------------

/// The body of an ErrorResponse / NoticeResponse: a list of `(field code, value)`
/// pairs terminated by a zero byte. Common codes: `S`everity (localized),
/// `V` severity (non-localized), sqlstate `C`ode, `M`essage, `D`etail,
/// `H`int, `P`osition, and `W` — the context traceback (spelled `W` because
/// PostgreSQL calls it "Where"). Kept as an ordered list so it round-trips
/// exactly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ErrorFields {
    pub fields: Vec<(u8, String)>,
}

impl ErrorFields {
    /// An ERROR with the mandatory severity / sqlstate / message fields.
    pub fn error(code: &str, message: &str) -> Self {
        Self::with_severity("ERROR", code, message)
    }

    /// A NOTICE with the mandatory severity / sqlstate / message fields.
    pub fn notice(code: &str, message: &str) -> Self {
        Self::with_severity("NOTICE", code, message)
    }

    /// A WARNING with the mandatory severity / sqlstate / message fields.
    /// Delivered as a NoticeResponse on the wire, like NOTICE.
    pub fn warning(code: &str, message: &str) -> Self {
        Self::with_severity("WARNING", code, message)
    }

    fn with_severity(severity: &str, code: &str, message: &str) -> Self {
        Self {
            fields: vec![
                (b'S', severity.to_string()),
                (b'V', severity.to_string()),
                (b'C', code.to_string()),
                (b'M', message.to_string()),
            ],
        }
    }

    /// Append a DETAIL (`D`) field.
    pub fn with_detail(mut self, detail: &str) -> Self {
        self.fields.push((b'D', detail.to_string()));
        self
    }

    /// Append a HINT (`H`) field.
    pub fn with_hint(mut self, hint: &str) -> Self {
        self.fields.push((b'H', hint.to_string()));
        self
    }

    /// Append a cursor POSITION (`P`) field — a 1-based character offset.
    pub fn with_position(mut self, position: usize) -> Self {
        self.fields.push((b'P', position.to_string()));
        self
    }

    /// Append a CONTEXT (`W`) field — the call-stack traceback a client renders
    /// as `CONTEXT: ...`. Nested call frames are one field value, newline
    /// separated and innermost first, which is how PostgreSQL stacks them.
    pub fn with_context(mut self, context: &str) -> Self {
        self.fields.push((b'W', context.to_string()));
        self
    }

    pub fn get(&self, code: u8) -> Option<&str> {
        self.fields
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, v)| v.as_str())
    }

    /// Non-localized severity (`V`), falling back to `S`.
    pub fn severity(&self) -> &str {
        self.get(b'V').or_else(|| self.get(b'S')).unwrap_or("ERROR")
    }

    pub fn code(&self) -> &str {
        self.get(b'C').unwrap_or("")
    }

    pub fn message(&self) -> &str {
        self.get(b'M').unwrap_or("")
    }

    fn encode_body(&self, body: &mut BytesMut) {
        for (code, value) in &self.fields {
            body.put_u8(*code);
            put_cstr(body, value);
        }
        body.put_u8(0);
    }

    fn decode_body(r: &mut Reader) -> Result<Self, ProtocolError> {
        let mut fields = Vec::new();
        loop {
            let code = r.u8()?;
            if code == 0 {
                break;
            }
            fields.push((code, r.cstr()?));
        }
        Ok(Self { fields })
    }
}

// ---------------------------------------------------------------------------
// Authentication request (the `R` message subtypes)
// ---------------------------------------------------------------------------

/// The `Authentication*` backend messages, which all share tag `R` and are
/// distinguished by a leading `i32` subtype code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthRequest {
    Ok,
    KerberosV5,
    CleartextPassword,
    Md5Password { salt: [u8; 4] },
    Gss,
    GssContinue { data: Vec<u8> },
    Sspi,
    Sasl { mechanisms: Vec<String> },
    SaslContinue { data: Vec<u8> },
    SaslFinal { data: Vec<u8> },
}

impl AuthRequest {
    fn encode_body(&self, body: &mut BytesMut) {
        match self {
            AuthRequest::Ok => body.put_i32(0),
            AuthRequest::KerberosV5 => body.put_i32(2),
            AuthRequest::CleartextPassword => body.put_i32(3),
            AuthRequest::Md5Password { salt } => {
                body.put_i32(5);
                body.put_slice(salt);
            }
            AuthRequest::Gss => body.put_i32(7),
            AuthRequest::GssContinue { data } => {
                body.put_i32(8);
                body.put_slice(data);
            }
            AuthRequest::Sspi => body.put_i32(9),
            AuthRequest::Sasl { mechanisms } => {
                body.put_i32(10);
                for m in mechanisms {
                    put_cstr(body, m);
                }
                body.put_u8(0); // list terminator
            }
            AuthRequest::SaslContinue { data } => {
                body.put_i32(11);
                body.put_slice(data);
            }
            AuthRequest::SaslFinal { data } => {
                body.put_i32(12);
                body.put_slice(data);
            }
        }
    }

    fn decode_body(r: &mut Reader) -> Result<Self, ProtocolError> {
        let subtype = r.i32()?;
        Ok(match subtype {
            0 => AuthRequest::Ok,
            2 => AuthRequest::KerberosV5,
            3 => AuthRequest::CleartextPassword,
            5 => {
                let salt = r.array()?;
                AuthRequest::Md5Password { salt }
            }
            7 => AuthRequest::Gss,
            8 => AuthRequest::GssContinue { data: r.rest() },
            9 => AuthRequest::Sspi,
            10 => {
                let mut mechanisms = Vec::new();
                loop {
                    if r.is_empty() {
                        break;
                    }
                    let m = r.cstr()?;
                    if m.is_empty() {
                        break; // list terminator
                    }
                    mechanisms.push(m);
                }
                AuthRequest::Sasl { mechanisms }
            }
            11 => AuthRequest::SaslContinue { data: r.rest() },
            12 => AuthRequest::SaslFinal { data: r.rest() },
            other => {
                return Err(ProtocolError::Malformed(format!(
                    "unsupported authentication subtype {other}"
                )));
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Startup-phase packets (untagged)
// ---------------------------------------------------------------------------

/// The first packet(s) a client sends before the connection is established.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartupRequest {
    /// SSLRequest — a server that speaks no TLS answers with a single `N` byte.
    Ssl,
    /// GSSENCRequest — likewise refused with `N`.
    GssEnc,
    /// CancelRequest — arrives on a separate connection which is then closed.
    Cancel { pid: i32, secret: i32 },
    /// The real StartupMessage with `user`, `database`, options.
    Startup { params: HashMap<String, String> },
}

impl StartupRequest {
    /// Append this startup packet (length-prefixed, no tag byte).
    pub fn encode(&self, buf: &mut BytesMut) {
        framed_untagged(buf, |body| match self {
            StartupRequest::Ssl => body.put_i32(SSL_REQUEST_CODE),
            StartupRequest::GssEnc => body.put_i32(GSSENC_REQUEST_CODE),
            StartupRequest::Cancel { pid, secret } => {
                body.put_i32(CANCEL_REQUEST_CODE);
                body.put_i32(*pid);
                body.put_i32(*secret);
            }
            StartupRequest::Startup { params } => {
                body.put_i32(PROTOCOL_VERSION_3);
                for (k, v) in params {
                    put_cstr(body, k);
                    put_cstr(body, v);
                }
                body.put_u8(0); // params terminator
            }
        });
    }

    /// Parse a startup packet body (everything after the length prefix).
    pub fn decode(body: &[u8]) -> Result<Self, ProtocolError> {
        if body.len() < 4 {
            return Err(ProtocolError::Malformed("short startup packet".into()));
        }
        let mut r = Reader::new(body);
        let code = r.i32()?;
        match code {
            SSL_REQUEST_CODE => Ok(StartupRequest::Ssl),
            GSSENC_REQUEST_CODE => Ok(StartupRequest::GssEnc),
            CANCEL_REQUEST_CODE => {
                if body.len() < 12 {
                    return Err(ProtocolError::Malformed("short CancelRequest".into()));
                }
                Ok(StartupRequest::Cancel {
                    pid: r.i32()?,
                    secret: r.i32()?,
                })
            }
            PROTOCOL_VERSION_3 => Ok(StartupRequest::Startup {
                params: parse_startup_params(&body[4..])?,
            }),
            other => Err(ProtocolError::UnsupportedProtocolVersion(other)),
        }
    }
}

/// Startup body: a sequence of `key\0value\0` pairs closed by an extra `\0`.
fn parse_startup_params(body: &[u8]) -> Result<HashMap<String, String>, ProtocolError> {
    let mut r = Reader::new(body);
    let mut params = HashMap::new();
    loop {
        match r.buf.first() {
            None | Some(0) => return Ok(params),
            _ => {}
        }
        let key = r.cstr()?;
        let value = r.cstr()?;
        params.insert(key, value);
    }
}

// ---------------------------------------------------------------------------
// Frontend messages (client -> server)
// ---------------------------------------------------------------------------

/// A message a frontend sends after startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontendMessage {
    /// Simple query (`Q`).
    Query(String),
    /// Parse (`P`): create a prepared statement.
    Parse {
        name: String,
        query: String,
        param_types: Vec<u32>,
    },
    /// Bind (`B`): create a portal from a prepared statement.
    Bind {
        portal: String,
        statement: String,
        param_formats: Vec<Format>,
        params: Vec<Option<Vec<u8>>>,
        result_formats: Vec<Format>,
    },
    /// Describe (`D`) a statement or portal.
    Describe { target: Target, name: String },
    /// Execute (`E`) a portal, returning at most `max_rows` rows (0 = unlimited).
    Execute { portal: String, max_rows: i32 },
    /// Close (`C`) a statement or portal.
    Close { target: Target, name: String },
    /// Flush (`H`).
    Flush,
    /// Sync (`S`): ends an extended-query batch; answered with ReadyForQuery.
    Sync,
    /// PasswordMessage / SASLInitialResponse / SASLResponse (all tag `p`). The
    /// body is kept raw since the three share a tag and are told apart by the
    /// preceding Authentication request.
    PasswordMessage(Vec<u8>),
    /// CopyData (`d`).
    CopyData(Vec<u8>),
    /// CopyDone (`c`).
    CopyDone,
    /// CopyFail (`f`) with an error message.
    CopyFail(String),
    /// FunctionCall (`F`, legacy fast-path).
    FunctionCall {
        oid: u32,
        arg_formats: Vec<Format>,
        args: Vec<Option<Vec<u8>>>,
        result_format: Format,
    },
    /// Terminate (`X`).
    Terminate,
    /// A message whose tag this codec does not recognize; preserved verbatim.
    Unknown { tag: u8, body: Vec<u8> },
}

impl FrontendMessage {
    /// The wire tag byte for this message. Single source of truth for the
    /// variant→tag mapping, used by both [`encode`](Self::encode) and callers
    /// that need the tag without serializing (e.g. an "unsupported message"
    /// error).
    pub fn tag(&self) -> u8 {
        match self {
            FrontendMessage::Query(_) => b'Q',
            FrontendMessage::Parse { .. } => b'P',
            FrontendMessage::Bind { .. } => b'B',
            FrontendMessage::Describe { .. } => b'D',
            FrontendMessage::Execute { .. } => b'E',
            FrontendMessage::Close { .. } => b'C',
            FrontendMessage::Flush => b'H',
            FrontendMessage::Sync => b'S',
            FrontendMessage::PasswordMessage(_) => b'p',
            FrontendMessage::CopyData(_) => b'd',
            FrontendMessage::CopyDone => b'c',
            FrontendMessage::CopyFail(_) => b'f',
            FrontendMessage::FunctionCall { .. } => b'F',
            FrontendMessage::Terminate => b'X',
            FrontendMessage::Unknown { tag, .. } => *tag,
        }
    }

    /// Append this message, fully framed (tag + length + body).
    pub fn encode(&self, buf: &mut BytesMut) {
        framed(buf, self.tag(), |b| self.encode_body(b));
    }

    fn encode_body(&self, b: &mut BytesMut) {
        match self {
            FrontendMessage::Query(sql) => put_cstr(b, sql),
            FrontendMessage::Parse {
                name,
                query,
                param_types,
            } => {
                put_cstr(b, name);
                put_cstr(b, query);
                b.put_i16(param_types.len() as i16);
                for oid in param_types {
                    b.put_u32(*oid);
                }
            }
            FrontendMessage::Bind {
                portal,
                statement,
                param_formats,
                params,
                result_formats,
            } => {
                put_cstr(b, portal);
                put_cstr(b, statement);
                put_format_list(b, param_formats);
                b.put_i16(params.len() as i16);
                for p in params {
                    put_opt_bytes(b, p.as_deref());
                }
                put_format_list(b, result_formats);
            }
            FrontendMessage::Describe { target, name } => {
                b.put_u8(target.as_byte());
                put_cstr(b, name);
            }
            FrontendMessage::Execute { portal, max_rows } => {
                put_cstr(b, portal);
                b.put_i32(*max_rows);
            }
            FrontendMessage::Close { target, name } => {
                b.put_u8(target.as_byte());
                put_cstr(b, name);
            }
            FrontendMessage::Flush | FrontendMessage::Sync | FrontendMessage::CopyDone => {}
            FrontendMessage::PasswordMessage(data) | FrontendMessage::CopyData(data) => {
                b.put_slice(data)
            }
            FrontendMessage::CopyFail(msg) => put_cstr(b, msg),
            FrontendMessage::FunctionCall {
                oid,
                arg_formats,
                args,
                result_format,
            } => {
                b.put_u32(*oid);
                put_format_list(b, arg_formats);
                b.put_i16(args.len() as i16);
                for a in args {
                    put_opt_bytes(b, a.as_deref());
                }
                b.put_i16(result_format.as_i16());
            }
            FrontendMessage::Terminate => {}
            FrontendMessage::Unknown { body, .. } => b.put_slice(body),
        }
    }

    /// Parse one message from its tag and body (everything after the length).
    pub fn decode(tag: u8, body: &[u8]) -> Result<Self, ProtocolError> {
        let mut r = Reader::new(body);
        Ok(match tag {
            b'Q' => FrontendMessage::Query(r.cstr()?),
            b'P' => {
                let name = r.cstr()?;
                let query = r.cstr()?;
                let count = r.count()?;
                let mut param_types = Vec::with_capacity(count);
                for _ in 0..count {
                    param_types.push(r.u32()?);
                }
                FrontendMessage::Parse {
                    name,
                    query,
                    param_types,
                }
            }
            b'B' => {
                let portal = r.cstr()?;
                let statement = r.cstr()?;
                let param_formats = read_format_list(&mut r)?;
                let param_count = r.count()?;
                let mut params = Vec::with_capacity(param_count);
                for _ in 0..param_count {
                    params.push(r.opt_bytes()?);
                }
                let result_formats = read_format_list(&mut r)?;
                FrontendMessage::Bind {
                    portal,
                    statement,
                    param_formats,
                    params,
                    result_formats,
                }
            }
            b'D' => FrontendMessage::Describe {
                target: Target::from_byte(r.u8()?)?,
                name: r.cstr()?,
            },
            b'E' => FrontendMessage::Execute {
                portal: r.cstr()?,
                max_rows: r.i32()?,
            },
            b'C' => FrontendMessage::Close {
                target: Target::from_byte(r.u8()?)?,
                name: r.cstr()?,
            },
            b'H' => FrontendMessage::Flush,
            b'S' => FrontendMessage::Sync,
            b'p' => FrontendMessage::PasswordMessage(r.rest()),
            b'd' => FrontendMessage::CopyData(r.rest()),
            b'c' => FrontendMessage::CopyDone,
            b'f' => FrontendMessage::CopyFail(r.cstr()?),
            b'F' => {
                let oid = r.u32()?;
                let arg_formats = read_format_list(&mut r)?;
                let arg_count = r.count()?;
                let mut args = Vec::with_capacity(arg_count);
                for _ in 0..arg_count {
                    args.push(r.opt_bytes()?);
                }
                let result_format = Format::from_i16(r.i16()?)?;
                FrontendMessage::FunctionCall {
                    oid,
                    arg_formats,
                    args,
                    result_format,
                }
            }
            b'X' => FrontendMessage::Terminate,
            other => FrontendMessage::Unknown {
                tag: other,
                body: body.to_vec(),
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Backend messages (server -> client)
// ---------------------------------------------------------------------------

/// The overall format plus per-column formats shared by the COPY response
/// messages (CopyInResponse `G`, CopyOutResponse `H`, CopyBothResponse `W`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyResponse {
    pub format: Format,
    pub column_formats: Vec<Format>,
}

impl CopyResponse {
    fn encode_body(&self, body: &mut BytesMut) {
        // Overall format is a single byte; per-column formats are i16 each.
        body.put_u8(self.format.as_i16() as u8);
        body.put_i16(self.column_formats.len() as i16);
        for f in &self.column_formats {
            body.put_i16(f.as_i16());
        }
    }

    fn decode_body(r: &mut Reader) -> Result<Self, ProtocolError> {
        let format = Format::from_i16(r.u8()? as i16)?;
        let count = r.i16()?.max(0) as usize;
        let mut column_formats = Vec::with_capacity(count);
        for _ in 0..count {
            column_formats.push(Format::from_i16(r.i16()?)?);
        }
        Ok(Self {
            format,
            column_formats,
        })
    }
}

/// A message a backend sends to a frontend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendMessage {
    /// Authentication request/status (`R`).
    Authentication(AuthRequest),
    /// ParameterStatus (`S`): a runtime GUC report.
    ParameterStatus { name: String, value: String },
    /// BackendKeyData (`K`): the pid/secret used by CancelRequest.
    BackendKeyData { pid: i32, secret: i32 },
    /// ReadyForQuery (`Z`).
    ReadyForQuery(TransactionStatus),
    /// NegotiateProtocolVersion (`v`).
    NegotiateProtocolVersion {
        minor: i32,
        unrecognized: Vec<String>,
    },
    /// RowDescription (`T`).
    RowDescription(Vec<FieldDescription>),
    /// DataRow (`D`); `None` columns are SQL NULL.
    DataRow(Vec<Option<Vec<u8>>>),
    /// CommandComplete (`C`) with its command tag.
    CommandComplete(String),
    /// EmptyQueryResponse (`I`).
    EmptyQueryResponse,
    /// ParseComplete (`1`).
    ParseComplete,
    /// BindComplete (`2`).
    BindComplete,
    /// CloseComplete (`3`).
    CloseComplete,
    /// PortalSuspended (`s`).
    PortalSuspended,
    /// NoData (`n`).
    NoData,
    /// ParameterDescription (`t`): the parameter type OIDs of a statement.
    ParameterDescription(Vec<u32>),
    /// CopyInResponse (`G`).
    CopyInResponse(CopyResponse),
    /// CopyOutResponse (`H`).
    CopyOutResponse(CopyResponse),
    /// CopyBothResponse (`W`).
    CopyBothResponse(CopyResponse),
    /// CopyData (`d`).
    CopyData(Vec<u8>),
    /// CopyDone (`c`).
    CopyDone,
    /// ErrorResponse (`E`).
    ErrorResponse(ErrorFields),
    /// NoticeResponse (`N`).
    NoticeResponse(ErrorFields),
    /// NotificationResponse (`A`): an async LISTEN/NOTIFY delivery.
    NotificationResponse {
        pid: i32,
        channel: String,
        payload: String,
    },
    /// FunctionCallResponse (`V`, legacy); `None` is a NULL result.
    FunctionCallResponse(Option<Vec<u8>>),
}

impl BackendMessage {
    /// Append this message, fully framed (tag + length + body).
    pub fn encode(&self, buf: &mut BytesMut) {
        match self {
            BackendMessage::Authentication(auth) => framed(buf, b'R', |b| auth.encode_body(b)),
            BackendMessage::ParameterStatus { name, value } => framed(buf, b'S', |b| {
                put_cstr(b, name);
                put_cstr(b, value);
            }),
            BackendMessage::BackendKeyData { pid, secret } => framed(buf, b'K', |b| {
                b.put_i32(*pid);
                b.put_i32(*secret);
            }),
            BackendMessage::ReadyForQuery(status) => {
                framed(buf, b'Z', |b| b.put_u8(status.as_byte()))
            }
            BackendMessage::NegotiateProtocolVersion {
                minor,
                unrecognized,
            } => framed(buf, b'v', |b| {
                b.put_i32(*minor);
                b.put_i32(unrecognized.len() as i32);
                for opt in unrecognized {
                    put_cstr(b, opt);
                }
            }),
            BackendMessage::RowDescription(fields) => put_row_description(buf, fields),
            BackendMessage::DataRow(columns) => {
                put_data_row(buf, columns.iter().map(|c| c.as_deref()))
            }
            BackendMessage::CommandComplete(tag) => framed(buf, b'C', |b| put_cstr(b, tag)),
            BackendMessage::EmptyQueryResponse => framed(buf, b'I', |_| {}),
            BackendMessage::ParseComplete => framed(buf, b'1', |_| {}),
            BackendMessage::BindComplete => framed(buf, b'2', |_| {}),
            BackendMessage::CloseComplete => framed(buf, b'3', |_| {}),
            BackendMessage::PortalSuspended => framed(buf, b's', |_| {}),
            BackendMessage::NoData => framed(buf, b'n', |_| {}),
            BackendMessage::ParameterDescription(oids) => framed(buf, b't', |b| {
                b.put_i16(oids.len() as i16);
                for oid in oids {
                    b.put_u32(*oid);
                }
            }),
            BackendMessage::CopyInResponse(c) => framed(buf, b'G', |b| c.encode_body(b)),
            BackendMessage::CopyOutResponse(c) => framed(buf, b'H', |b| c.encode_body(b)),
            BackendMessage::CopyBothResponse(c) => framed(buf, b'W', |b| c.encode_body(b)),
            BackendMessage::CopyData(data) => framed(buf, b'd', |b| b.put_slice(data)),
            BackendMessage::CopyDone => framed(buf, b'c', |_| {}),
            BackendMessage::ErrorResponse(fields) => framed(buf, b'E', |b| fields.encode_body(b)),
            BackendMessage::NoticeResponse(fields) => framed(buf, b'N', |b| fields.encode_body(b)),
            BackendMessage::NotificationResponse {
                pid,
                channel,
                payload,
            } => framed(buf, b'A', |b| {
                b.put_i32(*pid);
                put_cstr(b, channel);
                put_cstr(b, payload);
            }),
            BackendMessage::FunctionCallResponse(result) => {
                framed(buf, b'V', |b| put_opt_bytes(b, result.as_deref()))
            }
        }
    }

    /// Parse one message from its tag and body (everything after the length).
    pub fn decode(tag: u8, body: &[u8]) -> Result<Self, ProtocolError> {
        let mut r = Reader::new(body);
        Ok(match tag {
            b'R' => BackendMessage::Authentication(AuthRequest::decode_body(&mut r)?),
            b'S' => BackendMessage::ParameterStatus {
                name: r.cstr()?,
                value: r.cstr()?,
            },
            b'K' => BackendMessage::BackendKeyData {
                pid: r.i32()?,
                secret: r.i32()?,
            },
            b'Z' => BackendMessage::ReadyForQuery(TransactionStatus::from_byte(r.u8()?)?),
            b'v' => {
                let minor = r.i32()?;
                let count = r.i32()?.max(0) as usize;
                let mut unrecognized = Vec::with_capacity(count);
                for _ in 0..count {
                    unrecognized.push(r.cstr()?);
                }
                BackendMessage::NegotiateProtocolVersion {
                    minor,
                    unrecognized,
                }
            }
            b'T' => {
                let count = r.count()?;
                let mut fields = Vec::with_capacity(count);
                for _ in 0..count {
                    fields.push(FieldDescription {
                        name: r.cstr()?,
                        table_oid: r.u32()?,
                        column_id: r.i16()?,
                        type_oid: r.u32()?,
                        type_len: r.i16()?,
                        type_modifier: r.i32()?,
                        format: Format::from_i16(r.i16()?)?,
                    });
                }
                BackendMessage::RowDescription(fields)
            }
            b'D' => {
                let count = r.count()?;
                let mut columns = Vec::with_capacity(count);
                for _ in 0..count {
                    columns.push(r.opt_bytes()?);
                }
                BackendMessage::DataRow(columns)
            }
            b'C' => BackendMessage::CommandComplete(r.cstr()?),
            b'I' => BackendMessage::EmptyQueryResponse,
            b'1' => BackendMessage::ParseComplete,
            b'2' => BackendMessage::BindComplete,
            b'3' => BackendMessage::CloseComplete,
            b's' => BackendMessage::PortalSuspended,
            b'n' => BackendMessage::NoData,
            b't' => {
                let count = r.count()?;
                let mut oids = Vec::with_capacity(count);
                for _ in 0..count {
                    oids.push(r.u32()?);
                }
                BackendMessage::ParameterDescription(oids)
            }
            b'G' => BackendMessage::CopyInResponse(CopyResponse::decode_body(&mut r)?),
            b'H' => BackendMessage::CopyOutResponse(CopyResponse::decode_body(&mut r)?),
            b'W' => BackendMessage::CopyBothResponse(CopyResponse::decode_body(&mut r)?),
            b'd' => BackendMessage::CopyData(r.rest()),
            b'c' => BackendMessage::CopyDone,
            b'E' => BackendMessage::ErrorResponse(ErrorFields::decode_body(&mut r)?),
            b'N' => BackendMessage::NoticeResponse(ErrorFields::decode_body(&mut r)?),
            b'A' => BackendMessage::NotificationResponse {
                pid: r.i32()?,
                channel: r.cstr()?,
                payload: r.cstr()?,
            },
            b'V' => BackendMessage::FunctionCallResponse(r.opt_bytes()?),
            other => {
                return Err(ProtocolError::Malformed(format!(
                    "unknown backend message tag {:?}",
                    other as char
                )));
            }
        })
    }
}

/// Encode a RowDescription (`T`). Shared by `BackendMessage` and the
/// `BackendWriter` convenience method so the layout lives in one place. Each
/// field carries its own catalog origin / typmod / format, so a value decoded
/// from a real server re-encodes to the same bytes; query results built by the
/// server default those to "no origin / text" via [`FieldDescription::new`].
pub(crate) fn put_row_description(buf: &mut BytesMut, fields: &[FieldDescription]) {
    framed(buf, b'T', |body| {
        body.put_i16(fields.len() as i16);
        for f in fields {
            put_cstr(body, &f.name);
            body.put_u32(f.table_oid);
            body.put_i16(f.column_id);
            body.put_u32(f.type_oid);
            body.put_i16(f.type_len);
            body.put_i32(f.type_modifier);
            body.put_i16(f.format.as_i16());
        }
    });
}

/// Encode a DataRow (`D`) from any iterator of optional byte columns. Shared by
/// `BackendMessage` and the `BackendWriter` text convenience method so the
/// server can stream `&str` columns without cloning each into a `Vec<u8>`.
pub(crate) fn put_data_row<'a>(
    buf: &mut BytesMut,
    columns: impl ExactSizeIterator<Item = Option<&'a [u8]>>,
) {
    framed(buf, b'D', |body| {
        body.put_i16(columns.len() as i16);
        for col in columns {
            put_opt_bytes(body, col);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Split a fully-framed tagged message into `(tag, body)` and assert the
    /// length prefix covers the length field plus the body (but not the tag).
    fn frame_parts(buf: &[u8]) -> (u8, &[u8]) {
        let Some((&tag, rest)) = buf.split_first() else {
            panic!("encoded tagged message must contain a tag");
        };
        let Some(length_bytes) = rest.get(..4) else {
            panic!("encoded tagged message must contain a length");
        };
        let len = i32::from_be_bytes([
            length_bytes[0],
            length_bytes[1],
            length_bytes[2],
            length_bytes[3],
        ]) as usize;
        assert_eq!(len, buf.len() - 1, "tagged length must cover len + body");
        (tag, &buf[5..])
    }

    #[track_caller]
    fn rt_frontend(msg: FrontendMessage) {
        let mut buf = BytesMut::new();
        msg.encode(&mut buf);
        let (tag, body) = frame_parts(&buf);
        match FrontendMessage::decode(tag, body) {
            Ok(decoded) => assert_eq!(decoded, msg),
            Err(error) => panic!("failed to decode frontend round-trip: {error}"),
        }
    }

    #[track_caller]
    fn rt_backend(msg: BackendMessage) {
        let mut buf = BytesMut::new();
        msg.encode(&mut buf);
        let (tag, body) = frame_parts(&buf);
        match BackendMessage::decode(tag, body) {
            Ok(decoded) => assert_eq!(decoded, msg),
            Err(error) => panic!("failed to decode backend round-trip: {error}"),
        }
    }

    #[track_caller]
    fn rt_startup(req: StartupRequest) {
        let mut buf = BytesMut::new();
        req.encode(&mut buf);
        let Some(length_bytes) = buf.get(..4) else {
            panic!("encoded startup message must contain a length");
        };
        let len = i32::from_be_bytes([
            length_bytes[0],
            length_bytes[1],
            length_bytes[2],
            length_bytes[3],
        ]) as usize;
        assert_eq!(len, buf.len(), "untagged length must cover len + body");
        match StartupRequest::decode(&buf[4..]) {
            Ok(decoded) => assert_eq!(decoded, req),
            Err(error) => panic!("failed to decode startup round-trip: {error}"),
        }
    }

    #[test]
    fn startup_round_trips() {
        rt_startup(StartupRequest::Ssl);
        rt_startup(StartupRequest::GssEnc);
        rt_startup(StartupRequest::Cancel {
            pid: 4242,
            secret: -1,
        });
        let params = HashMap::from([
            ("user".to_string(), "alice".to_string()),
            ("database".to_string(), "db1".to_string()),
        ]);
        rt_startup(StartupRequest::Startup { params });
        rt_startup(StartupRequest::Startup {
            params: HashMap::new(),
        });
    }

    #[test]
    fn frontend_round_trips() {
        rt_frontend(FrontendMessage::Query("SELECT 1".to_string()));
        rt_frontend(FrontendMessage::Parse {
            name: "s1".to_string(),
            query: "SELECT $1".to_string(),
            param_types: vec![23, 0, 25],
        });
        rt_frontend(FrontendMessage::Bind {
            portal: "p".to_string(),
            statement: "s1".to_string(),
            param_formats: vec![Format::Text, Format::Binary],
            params: vec![None, Some(b"hello".to_vec()), Some(vec![0, 1, 2, 255])],
            result_formats: vec![Format::Binary],
        });
        rt_frontend(FrontendMessage::Describe {
            target: Target::Statement,
            name: "s1".to_string(),
        });
        rt_frontend(FrontendMessage::Describe {
            target: Target::Portal,
            name: String::new(),
        });
        rt_frontend(FrontendMessage::Execute {
            portal: "p".to_string(),
            max_rows: 100,
        });
        rt_frontend(FrontendMessage::Close {
            target: Target::Portal,
            name: "p".to_string(),
        });
        rt_frontend(FrontendMessage::Flush);
        rt_frontend(FrontendMessage::Sync);
        rt_frontend(FrontendMessage::PasswordMessage(b"s3cr3t".to_vec()));
        rt_frontend(FrontendMessage::CopyData(vec![1, 2, 3]));
        rt_frontend(FrontendMessage::CopyDone);
        rt_frontend(FrontendMessage::CopyFail("disk full".to_string()));
        rt_frontend(FrontendMessage::FunctionCall {
            oid: 42,
            arg_formats: vec![Format::Text],
            args: vec![Some(b"1".to_vec()), None],
            result_format: Format::Binary,
        });
        rt_frontend(FrontendMessage::Terminate);
        rt_frontend(FrontendMessage::Unknown {
            tag: b'?',
            body: vec![9, 8, 7],
        });
    }

    #[test]
    fn auth_subtypes_round_trip() {
        for auth in [
            AuthRequest::Ok,
            AuthRequest::KerberosV5,
            AuthRequest::CleartextPassword,
            AuthRequest::Md5Password { salt: [1, 2, 3, 4] },
            AuthRequest::Gss,
            AuthRequest::GssContinue {
                data: vec![10, 20, 30],
            },
            AuthRequest::Sspi,
            AuthRequest::Sasl {
                mechanisms: vec![
                    "SCRAM-SHA-256".to_string(),
                    "SCRAM-SHA-256-PLUS".to_string(),
                ],
            },
            AuthRequest::Sasl { mechanisms: vec![] },
            AuthRequest::SaslContinue {
                data: b"r=abc,s=def".to_vec(),
            },
            AuthRequest::SaslFinal {
                data: b"v=xyz".to_vec(),
            },
        ] {
            rt_backend(BackendMessage::Authentication(auth));
        }
    }

    #[test]
    fn backend_round_trips() {
        rt_backend(BackendMessage::ParameterStatus {
            name: "client_encoding".to_string(),
            value: "UTF8".to_string(),
        });
        rt_backend(BackendMessage::BackendKeyData {
            pid: 7,
            secret: 12345,
        });
        for status in [
            TransactionStatus::Idle,
            TransactionStatus::InTransaction,
            TransactionStatus::Failed,
        ] {
            rt_backend(BackendMessage::ReadyForQuery(status));
        }
        rt_backend(BackendMessage::NegotiateProtocolVersion {
            minor: 2,
            unrecognized: vec!["_pq_.foo".to_string(), "_pq_.bar".to_string()],
        });
        rt_backend(BackendMessage::RowDescription(vec![
            // A plain query-result column (no catalog origin, text format)...
            FieldDescription::new("id".to_string(), 23, 4),
            // ...and a fully-populated column as a real server would send it,
            // proving table oid / attnum / typmod / binary format all survive.
            FieldDescription {
                name: "amount".to_string(),
                table_oid: 16384,
                column_id: 2,
                type_oid: 1700, // numeric
                type_len: -1,
                type_modifier: 655366, // numeric(10,2)
                format: Format::Binary,
            },
        ]));
        rt_backend(BackendMessage::DataRow(vec![
            None,
            Some(b"text".to_vec()),
            Some(vec![0, 255, 0, 128]),
        ]));
        rt_backend(BackendMessage::CommandComplete("SELECT 3".to_string()));
        rt_backend(BackendMessage::EmptyQueryResponse);
        rt_backend(BackendMessage::ParseComplete);
        rt_backend(BackendMessage::BindComplete);
        rt_backend(BackendMessage::CloseComplete);
        rt_backend(BackendMessage::PortalSuspended);
        rt_backend(BackendMessage::NoData);
        rt_backend(BackendMessage::ParameterDescription(vec![23, 25, 1700]));
        for ctor in [
            BackendMessage::CopyInResponse as fn(CopyResponse) -> BackendMessage,
            BackendMessage::CopyOutResponse,
            BackendMessage::CopyBothResponse,
        ] {
            rt_backend(ctor(CopyResponse {
                format: Format::Binary,
                column_formats: vec![Format::Text, Format::Binary],
            }));
        }
        rt_backend(BackendMessage::CopyData(vec![5, 6, 7]));
        rt_backend(BackendMessage::CopyDone);
        rt_backend(BackendMessage::ErrorResponse(
            ErrorFields::error("42601", "syntax error")
                .with_detail("near SELECT")
                .with_position(8),
        ));
        rt_backend(BackendMessage::NoticeResponse(ErrorFields::notice(
            "00000",
            "table will be created",
        )));
        rt_backend(BackendMessage::NoticeResponse(ErrorFields::warning(
            "25P01",
            "there is no transaction in progress",
        )));
        rt_backend(BackendMessage::NotificationResponse {
            pid: 99,
            channel: "chan".to_string(),
            payload: "hi".to_string(),
        });
        rt_backend(BackendMessage::FunctionCallResponse(Some(
            b"result".to_vec(),
        )));
        rt_backend(BackendMessage::FunctionCallResponse(None));
    }

    #[test]
    fn empty_body_messages_are_five_bytes() {
        // Tag + i32 length of 4 (the length field only), no body.
        let cases: &[(BackendMessage, u8)] = &[
            (BackendMessage::EmptyQueryResponse, b'I'),
            (BackendMessage::ParseComplete, b'1'),
            (BackendMessage::BindComplete, b'2'),
            (BackendMessage::CloseComplete, b'3'),
            (BackendMessage::PortalSuspended, b's'),
            (BackendMessage::NoData, b'n'),
            (BackendMessage::CopyDone, b'c'),
        ];
        for (msg, tag) in cases {
            let mut buf = BytesMut::new();
            msg.encode(&mut buf);
            assert_eq!(&buf[..], &[*tag, 0, 0, 0, 4]);
        }
    }

    #[test]
    fn data_row_encodes_null_as_minus_one() {
        let mut buf = BytesMut::new();
        BackendMessage::DataRow(vec![None]).encode(&mut buf);
        // tag, len=10, ncols=1, collen=-1
        assert_eq!(&buf[..], &[b'D', 0, 0, 0, 10, 0, 1, 255, 255, 255, 255]);
    }

    #[test]
    fn error_response_carries_sqlstate() -> anyhow::Result<()> {
        let mut buf = BytesMut::new();
        BackendMessage::ErrorResponse(ErrorFields::error("42601", "syntax error")).encode(&mut buf);
        assert_eq!(buf[0], b'E');
        let len = i32::from_be_bytes(buf[1..5].try_into()?) as usize;
        assert_eq!(buf.len(), 1 + len);
        assert!(buf.windows(6).any(|w| w == b"42601\0"));

        Ok(())
    }

    #[test]
    fn ssl_request_exact_bytes() {
        let mut buf = BytesMut::new();
        StartupRequest::Ssl.encode(&mut buf);
        // len 8, code 80877103
        assert_eq!(&buf[..], &[0, 0, 0, 8, 4, 210, 22, 47]);
    }

    #[test]
    fn protocol_v2_is_rejected() {
        let mut body = (2i32 << 16).to_be_bytes().to_vec();
        body.extend_from_slice(b"user\0me\0\0");
        assert!(matches!(
            StartupRequest::decode(&body),
            Err(ProtocolError::UnsupportedProtocolVersion(_))
        ));
    }

    #[test]
    fn unknown_backend_tag_errors() {
        assert!(matches!(
            BackendMessage::decode(b'?', &[]),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn truncated_body_errors_not_panics() {
        // A DataRow claiming one column but with no column data.
        assert!(BackendMessage::decode(b'D', &[0, 1]).is_err());
    }
}
