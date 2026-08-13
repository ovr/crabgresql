//! The server the suites run against is a child process: it has to come up on
//! a port of the runner's choosing, answer queries, and be gone afterwards.
//! Without this test the whole mechanism is only exercised through the suites,
//! where a startup bug looks like every test failing at once.

use std::time::Duration;

use crabgresql_pg_regress::client::{Client, QueryEvent};
use crabgresql_pg_regress::server::{ServerProcess, locate_server_binary};

#[tokio::test]
async fn starts_answers_and_stops() -> anyhow::Result<()> {
    let binary = locate_server_binary()?;
    let data_dir = tempfile::tempdir()?;
    let outdir = tempfile::tempdir()?;
    let log_path = outdir.path().join("server.log");
    let server = ServerProcess::start(&binary, data_dir.path(), data_dir.path(), &log_path).await?;
    let port = server.port();

    let mut client = Client::connect(port).await?;
    let events = client.simple_query("SELECT 1").await?;
    let rows: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            QueryEvent::Row(row) => Some(row.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(rows, [vec![Some("1".to_string())]]);

    // The log is the crash-forensics channel, so it must actually be written to
    // — an empty file after a successful boot means the redirection is broken.
    let log = std::fs::read_to_string(&log_path)?;
    assert!(
        log.contains(&format!("listening on 127.0.0.1:{port}")),
        "server.log does not show the bound port: {log}"
    );

    drop(client);
    server.shutdown().await;
    assert!(
        Client::connect(port).await.is_err(),
        "the server is still accepting connections after shutdown"
    );
    Ok(())
}

/// A server killed under the runner's feet has to turn into an exit status the
/// runner can report. The status appears a beat *after* the socket dies, which
/// is why `exited_within` waits at all — and why this test kills the process
/// externally instead of calling `shutdown`.
#[tokio::test]
async fn a_killed_server_becomes_an_exit_status() -> anyhow::Result<()> {
    let binary = locate_server_binary()?;
    let dir = tempfile::tempdir()?;
    let mut server = ServerProcess::start(
        &binary,
        dir.path(),
        dir.path(),
        &dir.path().join("server.log"),
    )
    .await?;
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
