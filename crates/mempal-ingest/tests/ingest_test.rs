use mempal_ingest::{
    chunk::{chunk_conversation, chunk_text},
    detect::{Format, detect_format},
    normalize::normalize_content,
};

#[test]
fn test_fixed_window_chunk() {
    let text = "a".repeat(2000);
    let chunks = chunk_text(&text, 800, 100);

    assert!(chunks.len() >= 2);
    assert!(chunks[0].len() <= 800);
}

#[test]
fn test_qa_pair_chunk() {
    let transcript =
        "> How do I fix this?\nTry restarting.\n\n> What about the config?\nCheck settings.toml.";
    let chunks = chunk_conversation(transcript);

    assert_eq!(chunks.len(), 2);
    assert!(chunks[0].contains("How do I fix"));
    assert!(chunks[1].contains("config"));
}

#[test]
fn test_detect_claude_jsonl() {
    let content = r#"{"type":"human","message":"hello"}
{"type":"assistant","message":"hi"}"#;

    assert_eq!(detect_format(content), Format::ClaudeJsonl);
}

#[test]
fn test_detect_plain_text() {
    let content = "This is a regular markdown file.";

    assert_eq!(detect_format(content), Format::PlainText);
}

#[test]
fn test_normalize_claude_jsonl() {
    let content = r#"{"type":"human","message":"hello"}
{"type":"assistant","message":"hi"}"#;

    let normalized =
        normalize_content(content, Format::ClaudeJsonl).expect("claude jsonl should normalize");

    assert_eq!(normalized, "> hello\nhi");
}

#[test]
fn test_normalize_chatgpt_json() {
    let content = r#"[
  {"role":"user","content":"how do I fix this?"},
  {"role":"assistant","content":"restart the process"}
]"#;

    let normalized =
        normalize_content(content, Format::ChatGptJson).expect("chatgpt json should normalize");

    assert_eq!(normalized, "> how do I fix this?\nrestart the process");
}

// --- Codex JSONL ---

#[test]
fn test_detect_codex_jsonl() {
    let content = r#"{"type":"session_meta","session_id":"abc"}
{"type":"event_msg","payload":{"type":"user_message","message":"fix the bug"}}
{"type":"event_msg","payload":{"type":"agent_message","message":"I'll look into it"}}
{"type":"response_item","data":"synthetic context"}
"#;
    assert_eq!(detect_format(content), Format::CodexJsonl);
}

#[test]
fn test_normalize_codex_jsonl() {
    let content = r#"{"type":"session_meta","session_id":"abc"}
{"type":"event_msg","payload":{"type":"user_message","message":"what caused the CI failure?"}}
{"type":"event_msg","payload":{"type":"agent_message","message":"the --all-features flag was missing"}}
{"type":"response_item","data":"skip this"}
{"type":"event_msg","payload":{"type":"user_message","message":"fix it"}}
{"type":"event_msg","payload":{"type":"agent_message","message":"done, committed 4fac199"}}
"#;
    let normalized =
        normalize_content(content, Format::CodexJsonl).expect("codex jsonl should normalize");
    assert!(normalized.contains("> what caused the CI failure?"));
    assert!(normalized.contains("the --all-features flag was missing"));
    assert!(normalized.contains("> fix it"));
    assert!(normalized.contains("done, committed 4fac199"));
    assert!(!normalized.contains("skip this"));
}

// --- Slack JSON ---

#[test]
fn test_detect_slack_json() {
    let content = r#"[
  {"type":"message","user":"U123","text":"hey, check the deploy"},
  {"type":"message","user":"U456","text":"on it, looks like a config issue"}
]"#;
    assert_eq!(detect_format(content), Format::SlackJson);
}

#[test]
fn test_normalize_slack_json() {
    let content = r#"[
  {"type":"message","user":"U123","text":"should we use Clerk or Auth0?"},
  {"type":"message","user":"U456","text":"Clerk, better pricing and DX"},
  {"type":"message","user":"U123","text":"agreed, let's go with Clerk"}
]"#;
    let normalized =
        normalize_content(content, Format::SlackJson).expect("slack json should normalize");
    assert!(normalized.contains("> should we use Clerk or Auth0?"));
    assert!(normalized.contains("Clerk, better pricing and DX"));
    assert!(normalized.contains("> agreed, let's go with Clerk"));
}

#[test]
fn test_slack_skips_non_message_types() {
    let content = r#"[
  {"type":"message","user":"U1","text":"hello"},
  {"type":"thread_reply","user":"U2","text":"this should be skipped"},
  {"type":"message","user":"U2","text":"world"}
]"#;
    let normalized =
        normalize_content(content, Format::SlackJson).expect("slack should skip non-message");
    assert!(normalized.contains("> hello"));
    assert!(normalized.contains("world"));
    assert!(!normalized.contains("this should be skipped"));
}
