//! `--server-only` must keep the HTTP and gRPC servers alive with no TTY.
//!
//! The bug this pins: `main` used to start `TuiApp` unconditionally and abort
//! both servers after `tui.run()` returned. On a headless host the TUI fails
//! immediately (`/dev/tty` is unavailable), so the servers were torn down
//! before any client could reach them. `AGENTS.md` documents systemd, Docker
//! and CI as supported deployments, none of which have a terminal.
//!
//! This test spawns the real binary with no TTY and polls the real HTTP API.
//! A tiny local stand-in answers Ollama's `/api/tags` so the boot health check
//! never depends on a provider being installed.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Finds a port that is free *now*. There is an unavoidable TOCTOU window,
/// but the server fails loudly if it loses the race rather than hanging.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Answers `GET /api/tags` with an empty model list — enough for the boot
/// health check, which only looks at the status code. Runs on a real socket so
/// the spawned binary reaches it the same way it would reach Ollama.
fn spawn_fake_ollama() -> (u16, std::thread::JoinHandle<()>) {
    let port = free_port();
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let body = r#"{"models":[]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (port, handle)
}

/// The flag is accepted both at the top level and after `run` — clap's
/// `global = true` is what makes the second form work. `Cli::parse_from`
/// is testable without spawning anything, so pin both spellings here.
#[test]
fn server_only_is_accepted_before_and_after_the_run_subcommand() {
    use clap::Parser;
    use llama_r::cli::commands::Cli;

    let bare = Cli::parse_from(["llama-r", "--server-only"]);
    assert!(bare.server_only);
    assert!(bare.command.is_none());

    let with_run = Cli::parse_from(["llama-r", "run", "--server-only"]);
    assert!(with_run.server_only);
    assert!(matches!(
        with_run.command,
        Some(llama_r::cli::commands::Commands::Run)
    ));

    let plain = Cli::parse_from(["llama-r", "run"]);
    assert!(
        !plain.server_only,
        "the interactive default must not change"
    );
}

#[test]
fn a_headless_server_stays_up_and_answers_http() {
    let (ollama_port, _fake) = spawn_fake_ollama();
    let port = free_port();
    let base = tempfile::tempdir().unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_llama-r"))
        .arg("--server-only")
        .env("LLAMA_R_DIR", base.path())
        .env("PORT", port.to_string())
        .env("OLLAMA_URL", format!("http://127.0.0.1:{ollama_port}"))
        .env("DEFAULT_MODEL", "headless-test-model")
        .env("RAG_ENABLED", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn llama-r --server-only");
    let _server = ServerProcess(child);

    let url = format!("http://127.0.0.1:{port}/api/health");
    let deadline = Instant::now() + Duration::from_secs(20);

    loop {
        if let Ok(response) = curl_health(&url) {
            assert!(
                response.contains("\"api_running\":true"),
                "the HTTP listener must be marked running: {response}"
            );
            assert!(
                response.contains("\"grpc_running\":true"),
                "the gRPC server must stay up too: {response}"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "a headless server never answered on {url}. The TUI failure is \
             aborting the servers again"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `curl` is present on every platform this repo targets, and using it keeps
/// this test free of an HTTP-client dependency in the integration crate.
fn curl_health(url: &str) -> Result<String, ()> {
    let output = Command::new("curl")
        .args(["-s", "--max-time", "2", url])
        .output()
        .map_err(|_| ())?;
    if !output.status.success() {
        return Err(());
    }
    String::from_utf8(output.stdout).map_err(|_| ())
}
