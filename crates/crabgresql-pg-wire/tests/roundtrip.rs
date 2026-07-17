//! End-to-end round-trips over an in-memory pipe: encode a message on one side,
//! decode it on the other, and assert the value survives. Exercises the async
//! IO wrappers (`FrontendWriter`/`FrontendReader`, `BackendWriter`/
//! `BackendReader`) on top of the message model's `encode`/`decode`.

use std::collections::HashMap;

use crabgresql_pg_wire::{
    AuthRequest, BackendMessage, BackendReader, BackendWriter, CopyResponse, ErrorFields,
    FieldDescription, Format, FrontendMessage, FrontendReader, FrontendWriter, StartupRequest,
    Target, TransactionStatus,
};
use tokio::io::duplex;

#[tokio::test]
async fn frontend_messages_survive_the_wire() {
    let (client, server) = duplex(64 * 1024);
    let mut writer = FrontendWriter::new(client);
    let mut reader = FrontendReader::new(server);

    let startup = StartupRequest::Startup {
        params: HashMap::from([
            ("user".to_string(), "postgres".to_string()),
            ("database".to_string(), "regression".to_string()),
        ]),
    };
    let messages = vec![
        FrontendMessage::Query("SELECT 1".to_string()),
        FrontendMessage::Parse {
            name: "s1".to_string(),
            query: "SELECT $1::int".to_string(),
            param_types: vec![23],
        },
        FrontendMessage::Bind {
            portal: String::new(),
            statement: "s1".to_string(),
            param_formats: vec![Format::Text],
            params: vec![Some(b"42".to_vec()), None],
            result_formats: vec![Format::Binary],
        },
        FrontendMessage::Describe {
            target: Target::Portal,
            name: String::new(),
        },
        FrontendMessage::Execute {
            portal: String::new(),
            max_rows: 0,
        },
        FrontendMessage::Sync,
        FrontendMessage::CopyData(vec![1, 2, 3]),
        FrontendMessage::CopyDone,
        FrontendMessage::FunctionCall {
            oid: 42,
            arg_formats: vec![],
            args: vec![None],
            result_format: Format::Text,
        },
        FrontendMessage::Terminate,
    ];

    writer.write_startup(&startup);
    for m in &messages {
        writer.write_message(m);
    }
    writer.flush().await.unwrap();
    drop(writer); // close the write half so the final read hits EOF

    assert_eq!(reader.read_startup().await.unwrap(), Some(startup));
    for expected in &messages {
        assert_eq!(reader.read_message().await.unwrap().as_ref(), Some(expected));
    }
    assert!(reader.read_message().await.unwrap().is_none());
}

#[tokio::test]
async fn backend_messages_survive_the_wire() {
    let (server, client) = duplex(64 * 1024);
    let mut writer = BackendWriter::new(server);
    let mut reader = BackendReader::new(client);

    // A realistic startup + simple-query response, driven through both the
    // convenience methods and the generic `write`.
    writer.authentication_ok();
    writer.parameter_status("client_encoding", "UTF8");
    writer.backend_key_data(7, 12345);
    writer.write(&BackendMessage::NegotiateProtocolVersion {
        minor: 0,
        unrecognized: vec!["_pq_.foo".to_string()],
    });
    writer.ready_for_query(TransactionStatus::Idle);
    writer.row_description(&[FieldDescription::new("?column?".to_string(), 23, 4)]);
    writer.data_row(&[Some("1".to_string()), None]);
    writer.write(&BackendMessage::CopyOutResponse(CopyResponse {
        format: Format::Binary,
        column_formats: vec![Format::Binary],
    }));
    writer.command_complete("SELECT 1");
    writer.notice_response("00000", "heads up", Some("more detail"), Some(3));
    writer.error_response("42601", "syntax error");
    writer.write(&BackendMessage::NotificationResponse {
        pid: 99,
        channel: "chan".to_string(),
        payload: "ping".to_string(),
    });
    writer.flush().await.unwrap();
    drop(writer);

    let expected = vec![
        BackendMessage::Authentication(AuthRequest::Ok),
        BackendMessage::ParameterStatus {
            name: "client_encoding".to_string(),
            value: "UTF8".to_string(),
        },
        BackendMessage::BackendKeyData {
            pid: 7,
            secret: 12345,
        },
        BackendMessage::NegotiateProtocolVersion {
            minor: 0,
            unrecognized: vec!["_pq_.foo".to_string()],
        },
        BackendMessage::ReadyForQuery(TransactionStatus::Idle),
        BackendMessage::RowDescription(vec![FieldDescription::new("?column?".to_string(), 23, 4)]),
        BackendMessage::DataRow(vec![Some(b"1".to_vec()), None]),
        BackendMessage::CopyOutResponse(CopyResponse {
            format: Format::Binary,
            column_formats: vec![Format::Binary],
        }),
        BackendMessage::CommandComplete("SELECT 1".to_string()),
        BackendMessage::NoticeResponse(
            ErrorFields::notice("00000", "heads up")
                .with_detail("more detail")
                .with_position(3),
        ),
        BackendMessage::ErrorResponse(ErrorFields::error("42601", "syntax error")),
        BackendMessage::NotificationResponse {
            pid: 99,
            channel: "chan".to_string(),
            payload: "ping".to_string(),
        },
    ];

    for want in &expected {
        assert_eq!(reader.read_message().await.unwrap().as_ref(), Some(want));
    }
    assert!(reader.read_message().await.unwrap().is_none());
}
