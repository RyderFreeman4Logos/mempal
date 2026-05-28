// Legacy CC session ID helpers; xurl parsers use their own session discovery.

use std::path::Path;

use serde_json::Value;

const SESSION_ID_SCAN_LINE_LIMIT: usize = 64;

/// Extract the session ID from JSONL content by scanning for a `sessionId` field.
/// Returns `None` when no non-empty `sessionId` is found.
pub fn extract_session_id_from_content(content: &str) -> Option<String> {
    for raw_line in content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(SESSION_ID_SCAN_LINE_LIMIT)
    {
        let Ok(value) = serde_json::from_str::<Value>(raw_line) else {
            continue;
        };
        if let Some(sid) = value.get("sessionId").and_then(Value::as_str) {
            if !sid.is_empty() {
                return Some(sid.to_string());
            }
        }
    }
    None
}

/// Extract a session ID from the file path using the filename stem (without extension).
/// Example: `/path/to/abc123def.jsonl` → `"abc123def"`.
pub fn extract_session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Derive the room identifier for a CC session file. Checks the file content
/// for a `sessionId` field first, then falls back to the filename stem.
pub fn session_id_for_path(path: &Path, content: &str) -> String {
    extract_session_id_from_content(content).unwrap_or_else(|| extract_session_id_from_path(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_extract_session_id_from_content_present() {
        let content =
            r#"{"type":"user","sessionId":"abc-123","message":{"role":"user","content":[]}}"#;
        assert_eq!(
            extract_session_id_from_content(content),
            Some("abc-123".to_string())
        );
    }

    #[test]
    fn test_extract_session_id_from_content_first_line_wins() {
        let content = concat!(
            "{\"type\":\"user\",\"sessionId\":\"first\",\"message\":{}}\n",
            "{\"type\":\"assistant\",\"sessionId\":\"second\",\"message\":{}}"
        );
        assert_eq!(
            extract_session_id_from_content(content),
            Some("first".to_string())
        );
    }

    #[test]
    fn test_extract_session_id_from_content_absent() {
        let content = r#"{"type":"user","message":{"role":"user","content":[]}}"#;
        assert_eq!(extract_session_id_from_content(content), None);
    }

    #[test]
    fn test_extract_session_id_from_content_plain_text() {
        let content = "this is not json";
        assert_eq!(extract_session_id_from_content(content), None);
    }

    #[test]
    fn test_extract_session_id_from_content_limits_scan_window() {
        let mut lines = vec![r#"{"type":"summary"}"#; SESSION_ID_SCAN_LINE_LIMIT];
        lines.push(r#"{"type":"user","sessionId":"late","message":{"role":"user","content":[]}}"#);
        let content = lines.join("\n");

        assert_eq!(extract_session_id_from_content(&content), None);
    }

    #[test]
    fn test_extract_session_id_from_path_jsonl() {
        let path = Path::new("/home/user/.claude/projects/abc123def456.jsonl");
        assert_eq!(extract_session_id_from_path(path), "abc123def456");
    }

    #[test]
    fn test_extract_session_id_from_path_no_extension() {
        let path = Path::new("/tmp/mysession");
        assert_eq!(extract_session_id_from_path(path), "mysession");
    }

    #[test]
    fn test_session_id_for_path_prefers_content() {
        let content =
            r#"{"type":"user","sessionId":"content-id","message":{"role":"user","content":[]}}"#;
        let path = Path::new("/tmp/file-id.jsonl");
        assert_eq!(session_id_for_path(path, content), "content-id");
    }

    #[test]
    fn test_session_id_for_path_falls_back_to_filename() {
        let content = r#"{"type":"user","message":{"role":"user","content":[]}}"#;
        let path = Path::new("/tmp/file-id.jsonl");
        assert_eq!(session_id_for_path(path, content), "file-id");
    }
}
