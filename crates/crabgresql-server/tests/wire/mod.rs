//! The raw pgwire client the socket-level tests share. tokio-postgres cannot
//! reach a `CommandComplete` tag, a ReadyForQuery status byte or a
//! client-chosen statement name, so those tests drive the socket themselves.

use anyhow::Context as _;
use crabgresql_pg_wire::{FrontendMessage, StartupRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub async fn read_backend_message(socket: &mut tokio::net::TcpStream) -> (u8, Vec<u8>) {
    let tag = match socket.read_u8().await {
        Ok(tag) => tag,
        Err(error) => panic!("failed to read backend message tag: {error}"),
    };
    let len = match socket.read_i32().await {
        Ok(len) => len as usize,
        Err(error) => panic!("failed to read backend message length: {error}"),
    };
    let mut body = vec![0u8; len - 4];
    if let Err(error) = socket.read_exact(&mut body).await {
        panic!("failed to read backend message body: {error}");
    }
    (tag, body)
}

/// A StartupMessage for `user=postgres`.
pub fn startup_packet() -> bytes::BytesMut {
    let mut buf = bytes::BytesMut::new();
    StartupRequest::Startup {
        params: [("user".to_string(), "postgres".to_string())]
            .into_iter()
            .collect(),
    }
    .encode(&mut buf);
    buf
}

/// Cleartext startup on a raw socket, draining until ReadyForQuery.
pub async fn raw_session(port: u16) -> tokio::net::TcpStream {
    let mut socket = match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
        Ok(socket) => socket,
        Err(error) => panic!("failed to connect raw test session: {error}"),
    };
    if let Err(error) = socket.write_all(&startup_packet()).await {
        panic!("failed to write startup message: {error}");
    }
    loop {
        let (tag, _) = read_backend_message(&mut socket).await;
        if tag == b'Z' {
            return socket;
        }
    }
}

pub fn frontend_message(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut msg = vec![tag];
    msg.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    msg.extend_from_slice(body);
    msg
}

pub fn frontend_batch(messages: &[FrontendMessage]) -> bytes::BytesMut {
    let mut buf = bytes::BytesMut::new();
    for message in messages {
        message.encode(&mut buf);
    }
    buf
}

/// `Parse` with inferred parameter types, the only kind these tests send.
pub fn parse_message(name: &str, query: &str) -> FrontendMessage {
    FrontendMessage::Parse {
        name: name.to_string(),
        query: query.to_string(),
        param_types: Vec::new(),
    }
}

/// `Bind` with no formats and no parameters — everything defaults to text.
pub fn bind_message(portal: &str, statement: &str) -> FrontendMessage {
    FrontendMessage::Bind {
        portal: portal.to_string(),
        statement: statement.to_string(),
        param_formats: Vec::new(),
        params: Vec::new(),
        result_formats: Vec::new(),
    }
}

/// `Execute` with no row limit.
pub fn execute_message(portal: &str) -> FrontendMessage {
    FrontendMessage::Execute {
        portal: portal.to_string(),
        max_rows: 0,
    }
}

/// Collect every backend `(tag, body)` up to and including the terminating
/// ReadyForQuery, after the caller has written an extended-query batch.
pub async fn read_until_ready(socket: &mut tokio::net::TcpStream) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    loop {
        let (tag, body) = read_backend_message(socket).await;
        let done = tag == b'Z';
        out.push((tag, body));
        if done {
            return out;
        }
    }
}

/// The `CommandComplete` tag of the last completed command in a batch.
pub fn command_tag(msgs: &[(u8, Vec<u8>)]) -> anyhow::Result<String> {
    let body = &msgs
        .iter()
        .rev()
        .find(|(t, _)| *t == b'C')
        .context("CommandComplete is missing")?
        .1;
    Ok(String::from_utf8_lossy(body.split_last().map_or(&body[..], |(_, s)| s)).into_owned())
}
/// Send a simple `Query` and collect every backend `(tag, body)` up to and
/// including the terminating ReadyForQuery.
pub async fn simple_query_raw(socket: &mut tokio::net::TcpStream, sql: &str) -> Vec<(u8, Vec<u8>)> {
    let query = frontend_batch(&[FrontendMessage::Query(sql.to_string())]);
    if let Err(error) = socket.write_all(&query).await {
        panic!("failed to write simple-query test message: {error}");
    }
    let mut out = Vec::new();
    loop {
        let (tag, body) = read_backend_message(socket).await;
        let done = tag == b'Z';
        out.push((tag, body));
        if done {
            return out;
        }
    }
}

/// CommandComplete (`C`) tag strings, in order (NUL terminator stripped).
pub fn command_tags(msgs: &[(u8, Vec<u8>)]) -> Vec<String> {
    msgs.iter()
        .filter(|(tag, _)| *tag == b'C')
        .map(|(_, body)| String::from_utf8_lossy(body.strip_suffix(&[0]).unwrap_or(body)).into())
        .collect()
}

/// The status byte of the terminating ReadyForQuery (`I`/`T`/`E`).
pub fn ready_status(msgs: &[(u8, Vec<u8>)]) -> u8 {
    msgs.iter()
        .rev()
        .find(|(tag, _)| *tag == b'Z')
        .map(|(_, body)| body[0])
        .expect("a ReadyForQuery must terminate the batch")
}

/// Decode an ErrorResponse / NoticeResponse `(tag, body)` into its fields using
/// the wire codec, so the tests read errors the same way a client does.
pub fn fields(msg: &(u8, Vec<u8>)) -> crabgresql_pg_wire::ErrorFields {
    let decoded = match crabgresql_pg_wire::BackendMessage::decode(msg.0, &msg.1) {
        Ok(decoded) => decoded,
        Err(error) => panic!("failed to decode backend test message: {error}"),
    };
    match decoded {
        crabgresql_pg_wire::BackendMessage::ErrorResponse(f)
        | crabgresql_pg_wire::BackendMessage::NoticeResponse(f) => f,
        other => panic!("expected an ErrorResponse/NoticeResponse, got {other:?}"),
    }
}

/// Every `DataRow`'s first column, as text, in arrival order.
pub fn data_row_values(msgs: &[(u8, Vec<u8>)]) -> Vec<String> {
    msgs.iter()
        .filter(|(t, _)| *t == b'D')
        .map(|(_, body)| {
            // int16 column count, then int32 length + bytes per column.
            let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]) as usize;
            String::from_utf8_lossy(&body[6..6 + len]).into_owned()
        })
        .collect()
}
