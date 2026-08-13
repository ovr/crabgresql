//! The server has to come up on a port of the caller's choosing, answer
//! queries, and be gone afterwards. Without this the mechanism is only
//! exercised through the suites, where a startup bug looks like every test
//! failing at once.

use std::path::Path;
use std::time::Duration;

use crabgresql_server_process::{ServerProcess, listening_line, locate_server_binary};
use tokio_postgres::NoTls;

async fn start(dir: &Path) -> anyhow::Result<ServerProcess> {
    let binary = locate_server_binary(None)?;
    Ok(ServerProcess::start(&binary, dir, &[], &dir.join("server.log")).await?)
}

#[tokio::test]
async fn starts_answers_and_stops() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = start(dir.path()).await?;
    let port = server.port();

    let conninfo = format!("host=127.0.0.1 port={port} user=postgres dbname=test");
    let (client, connection) = tokio_postgres::connect(&conninfo, NoTls).await?;
    let driver = tokio::spawn(connection);
    let messages = client.simple_query("SELECT 1").await?;
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, tokio_postgres::SimpleQueryMessage::Row(_))),
        "the server returned no rows for SELECT 1"
    );

    // The log is the crash-forensics channel, so an empty file after a
    // successful boot would mean the redirection is broken.
    let log = std::fs::read_to_string(server.log_path())?;
    assert!(
        log.contains(&listening_line(port)),
        "the log does not show the bound port: {log}"
    );

    drop(client);
    driver.abort();
    server.shutdown().await;
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "the server is still accepting connections after shutdown"
    );
    Ok(())
}

/// A server killed under the caller's feet has to turn into an exit status it
/// can report. The status appears a beat *after* the socket dies, which is why
/// `exited_within` waits at all — and why this kills the process externally
/// instead of calling `shutdown`.
#[tokio::test]
async fn a_killed_server_becomes_an_exit_status() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut server = start(dir.path()).await?;
    assert!(
        server.exited_within(Duration::ZERO).await?.is_none(),
        "a healthy server was reported as exited"
    );

    let pid = server.pid().expect("a running child has a pid");
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()?;
    assert!(killed.success(), "kill -9 {pid} failed");

    let status = server
        .exited_within(Duration::from_secs(5))
        .await?
        .expect("a killed server exits");
    assert!(!status.success(), "SIGKILL is not a successful exit");
    Ok(())
}
