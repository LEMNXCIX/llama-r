use llama_r::api::chat_core::engine_events_to_chat_stream;
use llama_r::domain::models::{ChatMessage, ChatStreamEvent};
use llama_r::ports::engine::AgentRunEvent;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

#[test]
fn chat_stream_event_without_tool_serializes_no_tool_key() {
    let ev = ChatStreamEvent {
        model: "m".into(),
        created_at: "t".into(),
        message: ChatMessage {
            role: "assistant".into(),
            content: "hi".into(),
        },
        done: false,
        approval: None,
        tool: None,
    };
    let json = serde_json::to_string(&ev).unwrap();
    assert!(!json.contains("\"tool\""), "{json}");
    assert!(!json.contains("\"approval\""), "{json}");
}

#[tokio::test]
async fn engine_events_map_tool_call_and_result_without_leaking_arguments() {
    let (tx, rx) = mpsc::channel(8);
    tx.send(AgentRunEvent::ToolCall {
        server_id: "srv".into(),
        tool_name: "search".into(),
        arguments: serde_json::json!({"q": "x", "api_key": "SECRET_VALUE"}),
    })
    .await
    .unwrap();
    tx.send(AgentRunEvent::ToolResult {
        server_id: "srv".into(),
        tool_name: "search".into(),
        is_error: false,
        preview: "hola".into(),
    })
    .await
    .unwrap();
    tx.send(AgentRunEvent::ToolCall {
        server_id: String::new(),
        tool_name: "local_echo".into(),
        arguments: serde_json::json!({"x": 1}),
    })
    .await
    .unwrap();
    tx.send(AgentRunEvent::Token {
        text: "final".into(),
    })
    .await
    .unwrap();
    drop(tx);

    let mut stream = engine_events_to_chat_stream("m".into(), rx);
    let mut events: Vec<ChatStreamEvent> = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.expect("stream must not error"));
    }
    // No `Completed` is sent, so the mapper appends its usual trailing `done`
    // marker after the token (unchanged legacy behaviour).
    assert_eq!(events.len(), 5);

    // 0: MCP call — qualified name and hash only, never the raw arguments.
    let call = &events[0];
    assert_eq!(call.message.content, "");
    assert!(!call.done);
    let info = call.tool.as_ref().expect("ToolCall carries tool info");
    assert_eq!(info.kind, "call");
    assert_eq!(info.tool_name, "srv__search");
    assert!(info.is_error.is_none());
    let hash = info.args_hash.as_ref().expect("call carries an args hash");
    assert_eq!(hash.len(), 64);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    let json = serde_json::to_string(call).unwrap();
    assert!(!json.contains("SECRET_VALUE"), "{json}");
    assert!(!json.contains("\"arguments\""), "{json}");
    assert!(!json.contains("\"approval\""), "{json}");

    // 1: result — is_error, no hash, no preview.
    let result = &events[1];
    assert_eq!(result.message.content, "");
    assert!(!result.done);
    let info = result.tool.as_ref().expect("ToolResult carries tool info");
    assert_eq!(info.kind, "result");
    assert_eq!(info.tool_name, "srv__search");
    assert_eq!(info.is_error, Some(false));
    assert!(info.args_hash.is_none());
    let json = serde_json::to_string(result).unwrap();
    assert!(!json.contains("preview"), "{json}");
    assert!(!json.contains("hola"), "{json}");

    // 2: local call — bare tool_name, no server prefix.
    let local = &events[2];
    let info = local
        .tool
        .as_ref()
        .expect("local ToolCall carries tool info");
    assert_eq!(info.kind, "call");
    assert_eq!(info.tool_name, "local_echo");
    assert!(!info.tool_name.contains("__"));
    assert!(info.args_hash.is_some());

    // 3: ordinary token — additive fields still absent (backward compatibility).
    let token = &events[3];
    assert_eq!(token.message.content, "final");
    assert!(!token.done);
    let json = serde_json::to_string(token).unwrap();
    assert!(!json.contains("\"tool\""), "{json}");
    assert!(!json.contains("\"approval\""), "{json}");
}
