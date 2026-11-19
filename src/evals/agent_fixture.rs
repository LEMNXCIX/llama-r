//! A fake Ollama socket that plays both halves of a real tool loop.
//!
//! This is the agent-case counterpart of [`super::fixture::FixtureServer`].
//! The first completion asks for the built-in `llama_r__skill` local tool; once
//! the engine has executed it and sent the result back, the second completion
//! returns a final answer. The eval runner drives this through the **real**
//! `RigAgentEngine` — nothing here simulates the engine or the tool bridges.
//!
//! The case's prompt doubles as the tool's arguments when it is a JSON object,
//! which is how a fixture can plant a credential-bearing arg without a live
//! service. Otherwise a benign `{"name": "eval-tool"}` is used. The final answer
//! echoes the prompt and the arguments as structured JSON so redaction of a
//! credential field can be asserted end to end in a report.

use std::net::SocketAddr;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// The local tool the canned completion calls (bare id, no server).
pub const EVAL_AGENT_TOOL: &str = "llama_r__skill";
/// A real skill on disk with this id is loaded so the tool executes.
pub const EVAL_AGENT_SKILL: &str = "eval-tool";
/// Marker in the final answer, used by `contains` checks on agent cases.
pub const EVAL_AGENT_ANSWER: &str = "eval-tool-final-answer";

/// One-shot-per-turn fake Ollama that serves a tool call followed by an answer.
pub struct AgentToolLoopServer {
    addr: SocketAddr,
    handle: JoinHandle<()>,
}

impl AgentToolLoopServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let Some(body) = read_request(&mut socket).await else {
                    return;
                };
                // Rig's Ollama streaming client sends `"stream":true` and reads
                // NDJSON, so a single JSON object is not enough: serve one real
                // chunk plus the terminal `done` chunk.
                let payload = response_for(&body);
                let mut ndjson = format!("{payload}\n");
                ndjson.push_str(
                    "{\"model\":\"eval-model\",\"created_at\":\"2026-01-01T00:00:00Z\",\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true}\n",
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    ndjson.len(),
                    ndjson
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        Self { addr, handle }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for AgentToolLoopServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Build the canned Ollama completion for one leg of the loop.
fn response_for(body: &str) -> String {
    let request: Value = serde_json::from_str(body).unwrap_or_else(|_| json!({}));
    let messages = request["messages"].as_array();
    let user_message = messages
        .and_then(|items| {
            items.iter().rev().find_map(|message| {
                (message["role"] == "user")
                    .then(|| message["content"].as_str().map(str::to_string))
                    .flatten()
            })
        })
        .unwrap_or_default();

    // The prompt is the tool arguments when it is a JSON object; a credential
    // field there flows into the answer and must be redacted in the report.
    let parsed_prompt = serde_json::from_str::<Value>(&user_message)
        .ok()
        .filter(|value| value.is_object());
    let mut args = parsed_prompt.clone().unwrap_or_else(|| json!({}));
    if args.get("name").is_none() {
        args["name"] = json!(EVAL_AGENT_SKILL);
    }

    let saw_tool_result =
        messages.is_some_and(|items| items.iter().any(|message| message["role"] == "tool"));
    // A fixture can deliberately skip the tool to prove a `called_tool` case
    // fails loudly when the turn never called one.
    let skip_tool = args
        .get("skip_tool")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let message = if saw_tool_result || skip_tool {
        // Echo the arguments as *structured* JSON, not as a nested string: a
        // string that happens to contain JSON is prose to `redact_secrets`, and
        // a credential in it would slip into a report detail verbatim.
        let mut answer = args.clone();
        answer["answer"] = json!(EVAL_AGENT_ANSWER);
        if parsed_prompt.is_none() {
            answer["prompt"] = json!(user_message);
        }
        json!({
            "role": "assistant",
            "content": serde_json::to_string(&answer).unwrap_or_default(),
        })
    } else {
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "type": "function",
                "function": {
                    "name": EVAL_AGENT_TOOL,
                    "arguments": args,
                }
            }]
        })
    };

    json!({
        "model": "eval-model",
        "created_at": "2026-01-01T00:00:00Z",
        "message": message,
        "done": false,
    })
    .to_string()
}

/// Drain one full HTTP/1.1 POST (headers plus `Content-Length` body) and return
/// the body, mirroring the helper in `tests/agent_runtime_tests.rs`.
async fn read_request(socket: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let header_end = loop {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let len = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let total = header_end + len;
    while buf.len() < total {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Some(String::from_utf8_lossy(&buf[header_end..total]).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_leg_calls_the_tool_and_second_leg_answers() {
        let first = response_for(r#"{"messages":[{"role":"user","content":"use it"}]}"#);
        let value: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(
            value["message"]["tool_calls"][0]["function"]["name"],
            json!(EVAL_AGENT_TOOL)
        );
        assert_eq!(
            value["message"]["tool_calls"][0]["function"]["arguments"]["name"],
            json!(EVAL_AGENT_SKILL)
        );

        let second = response_for(
            r#"{"messages":[{"role":"user","content":"use it"},
                {"role":"tool","content":"skill body"}]}"#,
        );
        let value: Value = serde_json::from_str(&second).unwrap();
        assert!(value["message"]["content"]
            .as_str()
            .unwrap()
            .contains(EVAL_AGENT_ANSWER));
    }

    #[test]
    fn a_json_prompt_becomes_the_tool_arguments() {
        let first = response_for(
            r#"{"messages":[{"role":"user","content":"{\"name\":\"eval-tool\",\"api_key\":\"secret\"}"}]}"#,
        );
        let value: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(
            value["message"]["tool_calls"][0]["function"]["arguments"]["api_key"],
            json!("secret")
        );
    }
}
