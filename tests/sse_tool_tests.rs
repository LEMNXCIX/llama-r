use llama_r::domain::models::{ChatMessage, ChatStreamEvent};

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
