//! Graceful shutdown on Ctrl-C / SIGTERM.
//!
//! Pins that `llama_r::runtime::shutdown_signal()` resolves when the process
//! receives SIGTERM, and that a `--server-only` binary drains and exits 0.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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

#[cfg(unix)]
#[tokio::test]
async fn shutdown_signal_resolves_on_request_shutdown() {
    llama_r::runtime::request_shutdown();
    let signal =
        tokio::time::timeout(Duration::from_secs(2), llama_r::runtime::shutdown_signal()).await;
    assert!(
        signal.is_ok(),
        "request_shutdown did not resolve shutdown_signal"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_signal_completes_on_sigterm() {
    // If a previous test in this binary already triggered shutdown in-process,
    // shutdown_signal() resolves immediately and we must not send a real
    // SIGTERM to the whole test process (it could terminate parallel tests).
    if llama_r::runtime::shutdown_requested() {
        let signal =
            tokio::time::timeout(Duration::from_secs(2), llama_r::runtime::shutdown_signal()).await;
        assert!(signal.is_ok(), "shutdown_signal did not resolve");
        return;
    }

    let pid = std::process::id();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
    });

    let signal =
        tokio::time::timeout(Duration::from_secs(2), llama_r::runtime::shutdown_signal()).await;
    assert!(signal.is_ok(), "SIGTERM did not resolve shutdown_signal");
}

#[test]
fn server_only_exits_zero_on_sigterm() {
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
    let mut _server = ServerProcess(child);

    // Wait — re-borrow child from _server? We stored child moved. Need handle.
    // Rework: keep child mutable separately.
    // The above moved child into ServerProcess; get it back via field.
    let child = &mut _server.0;

    let url = format!("http://127.0.0.1:{port}/api/health");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(response) = curl_health(&url) {
            assert!(
                response.contains("\"api_running\":true"),
                "http listener must be running: {response}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "headless server never answered on {url}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status();

    let wait_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                assert!(status.success(), "process exited with {status:?}");
                return;
            }
            Ok(None) => {
                if Instant::now() > wait_deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("process did not exit within 5s after SIGTERM");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("try_wait failed: {e}");
            }
        }
    }
}
