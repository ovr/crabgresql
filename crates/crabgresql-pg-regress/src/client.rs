//! Minimal pgwire client for driving the server the way psql does.
//!
//! tokio-postgres's simple-query API hides the column type OIDs that psql's
//! aligned output needs for numeric right-alignment, so the runner speaks the
//! wire protocol directly: a startup handshake (trust auth), then one `Q`
//! message per statement.

use std::io;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

const PROTOCOL_VERSION_3: i32 = 196608;

/// Reject absurd message lengths instead of allocating them.
const MAX_MESSAGE_LEN: usize = 64 * 1024 * 1024;

/// One column of a RowDescription: the name plus the type OID that decides
/// left vs right alignment in psql's aligned format.
#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub type_oid: u32,
}

/// Fields of an ErrorResponse / NoticeResponse, keyed by the protocol's
/// single-byte field codes (`S`everity, `C`ode, `M`essage, `D`etail, `H`int,
/// `P`osition, ...).
#[derive(Debug, Default)]
pub struct ErrorFields {
    fields: Vec<(u8, String)>,
}

impl ErrorFields {
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

    pub fn message(&self) -> &str {
        self.get(b'M').unwrap_or("")
    }
}

/// Everything the server sends in response to one simple-query message, in
/// arrival order.
#[derive(Debug)]
pub enum QueryEvent {
    RowDescription(Vec<Field>),
    Row(Vec<Option<String>>),
    /// A completed `COPY … TO STDOUT`: every `CopyData` frame's payload,
    /// concatenated. Per-frame granularity would be meaningless — psql renders a
    /// copy-out by writing the bytes, so where the server split them cannot show.
    CopyOut(Vec<u8>),
    CommandComplete(String),
    EmptyQuery,
    Error(ErrorFields),
    Notice(ErrorFields),
}

pub struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Client {
    /// Connect and complete the startup handshake.
    pub async fn connect(port: u16) -> io::Result<Self> {
        let socket = TcpStream::connect(("127.0.0.1", port)).await?;
        socket.set_nodelay(true).ok();
        let (read_half, write_half) = socket.into_split();
        let mut client = Self {
            reader: BufReader::new(read_half),
            writer: write_half,
        };

        let mut body = PROTOCOL_VERSION_3.to_be_bytes().to_vec();
        for (key, value) in [("user", "postgres"), ("database", "regression")] {
            body.extend_from_slice(key.as_bytes());
            body.push(0);
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        }
        body.push(0);
        let mut packet = ((body.len() + 4) as i32).to_be_bytes().to_vec();
        packet.extend_from_slice(&body);
        client.writer.write_all(&packet).await?;
        client.writer.flush().await?;

        loop {
            let (tag, body) = client.read_message().await?;
            match tag {
                b'R' if body.as_slice() == [0, 0, 0, 0] => {}
                b'R' => return Err(protocol_error("server requested authentication")),
                b'S' | b'K' | b'N' => {}
                b'Z' => return Ok(client),
                b'E' => {
                    let fields = parse_error_fields(&body);
                    return Err(protocol_error(format!(
                        "handshake failed: {}",
                        fields.message()
                    )));
                }
                other => {
                    return Err(protocol_error(format!(
                        "unexpected message '{}' during startup",
                        other as char
                    )));
                }
            }
        }
    }

    /// Send one `Q` message and collect every response up to ReadyForQuery.
    pub async fn simple_query(&mut self, sql: &str) -> io::Result<Vec<QueryEvent>> {
        let mut packet = vec![b'Q'];
        packet.extend_from_slice(&((4 + sql.len() + 1) as i32).to_be_bytes());
        packet.extend_from_slice(sql.as_bytes());
        packet.push(0);
        self.writer.write_all(&packet).await?;
        self.writer.flush().await?;

        let mut events = Vec::new();
        // `Some` between CopyOutResponse and CopyDone; the payload accumulates
        // here so a stray `d`/`c` outside a copy still reads as a violation.
        let mut copy_out: Option<Vec<u8>> = None;
        loop {
            let (tag, body) = self.read_message().await?;
            match tag {
                b'T' => events.push(QueryEvent::RowDescription(parse_row_description(&body)?)),
                b'D' => events.push(QueryEvent::Row(parse_data_row(&body)?)),
                // CopyOutResponse: the body only restates formats we already
                // know (this client never asks for binary), so it is discarded.
                b'H' => copy_out = Some(Vec::new()),
                b'd' => match copy_out.as_mut() {
                    Some(buffer) => buffer.extend_from_slice(&body),
                    None => return Err(protocol_error("CopyData outside a copy-out")),
                },
                b'c' => match copy_out.take() {
                    Some(buffer) => events.push(QueryEvent::CopyOut(buffer)),
                    None => return Err(protocol_error("CopyDone outside a copy-out")),
                },
                b'C' => {
                    events.push(QueryEvent::CommandComplete(Cursor::new(&body).cstr()?));
                }
                b'I' => events.push(QueryEvent::EmptyQuery),
                // A copy-out can also end in an error: the frames already sent
                // stay sent (the server cannot un-send them) and no CopyDone
                // follows. psql prints those rows and then the error, so the
                // payload is flushed here rather than dropped.
                b'E' => {
                    if let Some(buffer) = copy_out.take() {
                        events.push(QueryEvent::CopyOut(buffer));
                    }
                    events.push(QueryEvent::Error(parse_error_fields(&body)));
                }
                b'N' => events.push(QueryEvent::Notice(parse_error_fields(&body))),
                // ParameterStatus, e.g. after SET once GUCs are reported.
                b'S' => {}
                b'Z' => {
                    if copy_out.is_some() {
                        return Err(protocol_error(
                            "copy-out ended without CopyDone or an error",
                        ));
                    }
                    return Ok(events);
                }
                other => {
                    return Err(protocol_error(format!(
                        "unexpected message '{}' in query response",
                        other as char
                    )));
                }
            }
        }
    }

    /// Drive one `COPY … FROM STDIN` like psql: send the COPY as a `Q` message,
    /// wait for `CopyInResponse` (`G`), stream `data` as one `CopyData` (`d`)
    /// frame plus `CopyDone` (`c`), then collect the completion events up to
    /// ReadyForQuery. If the server errors before entering copy mode (`E` before
    /// `G` — e.g. a missing table), no data is sent.
    pub async fn copy_in(&mut self, sql: &str, data: &str) -> io::Result<Vec<QueryEvent>> {
        let mut packet = vec![b'Q'];
        packet.extend_from_slice(&((4 + sql.len() + 1) as i32).to_be_bytes());
        packet.extend_from_slice(sql.as_bytes());
        packet.push(0);
        self.writer.write_all(&packet).await?;
        self.writer.flush().await?;

        let mut events = Vec::new();
        // Wait for CopyInResponse, surfacing a pre-copy error/notice.
        loop {
            let (tag, body) = self.read_message().await?;
            match tag {
                b'G' => break, // CopyInResponse: the server is ready for data.
                b'E' => {
                    events.push(QueryEvent::Error(parse_error_fields(&body)));
                    // The command failed before copy mode; drain to ReadyForQuery.
                    return self.drain_to_ready(events).await;
                }
                b'N' => events.push(QueryEvent::Notice(parse_error_fields(&body))),
                b'S' => {}
                other => {
                    return Err(protocol_error(format!(
                        "unexpected message '{}' before CopyInResponse",
                        other as char
                    )));
                }
            }
        }

        // One CopyData frame carrying the whole payload, then CopyDone.
        let mut copy_data = vec![b'd'];
        copy_data.extend_from_slice(&((4 + data.len()) as i32).to_be_bytes());
        copy_data.extend_from_slice(data.as_bytes());
        self.writer.write_all(&copy_data).await?;
        self.writer.write_all(&[b'c', 0, 0, 0, 4]).await?; // CopyDone (len 4)
        self.writer.flush().await?;

        self.drain_to_ready(events).await
    }

    /// Collect events (CommandComplete / Error / Notice) up to ReadyForQuery.
    async fn drain_to_ready(&mut self, mut events: Vec<QueryEvent>) -> io::Result<Vec<QueryEvent>> {
        loop {
            let (tag, body) = self.read_message().await?;
            match tag {
                b'C' => events.push(QueryEvent::CommandComplete(Cursor::new(&body).cstr()?)),
                b'E' => events.push(QueryEvent::Error(parse_error_fields(&body))),
                b'N' => events.push(QueryEvent::Notice(parse_error_fields(&body))),
                b'S' => {}
                b'Z' => return Ok(events),
                other => {
                    return Err(protocol_error(format!(
                        "unexpected message '{}' in copy response",
                        other as char
                    )));
                }
            }
        }
    }

    async fn read_message(&mut self) -> io::Result<(u8, Vec<u8>)> {
        let tag = self.reader.read_u8().await?;
        let len = self.reader.read_i32().await?;
        if !(4..=MAX_MESSAGE_LEN as i32).contains(&len) {
            return Err(protocol_error(format!("invalid message length {len}")));
        }
        let mut body = vec![0u8; len as usize - 4];
        self.reader.read_exact(&mut body).await?;
        Ok((tag, body))
    }
}

fn protocol_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn parse_row_description(body: &[u8]) -> io::Result<Vec<Field>> {
    let mut cur = Cursor::new(body);
    let count = cur.i16()?;
    let mut fields = Vec::with_capacity(count.max(0) as usize);
    for _ in 0..count {
        let name = cur.cstr()?;
        cur.skip(4 + 2)?; // table oid, attribute number
        let type_oid = cur.u32()?;
        cur.skip(2 + 4 + 2)?; // typlen, typmod, format
        fields.push(Field { name, type_oid });
    }
    Ok(fields)
}

fn parse_data_row(body: &[u8]) -> io::Result<Vec<Option<String>>> {
    let mut cur = Cursor::new(body);
    let count = cur.i16()?;
    let mut columns = Vec::with_capacity(count.max(0) as usize);
    for _ in 0..count {
        let len = cur.i32()?;
        if len < 0 {
            columns.push(None);
        } else {
            let bytes = cur.bytes(len as usize)?;
            columns.push(Some(String::from_utf8_lossy(bytes).into_owned()));
        }
    }
    Ok(columns)
}

/// Body: `(code_byte, cstring)*` terminated by a zero byte.
pub(crate) fn parse_error_fields(body: &[u8]) -> ErrorFields {
    let mut cur = Cursor::new(body);
    let mut fields = Vec::new();
    while let Ok(code) = cur.u8() {
        if code == 0 {
            break;
        }
        match cur.cstr() {
            Ok(value) => fields.push((code, value)),
            Err(_) => break,
        }
    }
    ErrorFields { fields }
}

struct Cursor<'a> {
    buf: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    fn bytes(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.buf.len() < n {
            return Err(protocol_error("truncated message"));
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        self.bytes(N)?
            .try_into()
            .map_err(|_| protocol_error("truncated message"))
    }

    fn skip(&mut self, n: usize) -> io::Result<()> {
        self.bytes(n).map(|_| ())
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    fn i16(&mut self) -> io::Result<i16> {
        Ok(i16::from_be_bytes(self.array()?))
    }

    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn cstr(&mut self) -> io::Result<String> {
        let end = self
            .buf
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| protocol_error("unterminated string"))?;
        let s = String::from_utf8_lossy(&self.buf[..end]).into_owned();
        self.buf = &self.buf[end + 1..];
        Ok(s)
    }
}
