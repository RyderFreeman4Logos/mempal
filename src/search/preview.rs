#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewText {
    pub content: String,
    pub truncated: bool,
}

pub fn truncate(content: &str, preview_chars: usize) -> PreviewText {
    let total_chars = content.chars().count();
    if total_chars <= preview_chars {
        return PreviewText {
            content: content.to_string(),
            truncated: false,
        };
    }

    let mut exact_end = 0usize;
    let mut boundary_end = None;
    let mut prev = None;

    for (count, (idx, ch)) in content.char_indices().enumerate() {
        if count >= preview_chars {
            break;
        }

        if prev.is_some_and(is_boundary_char) {
            boundary_end = Some(idx);
        }

        exact_end = idx + ch.len_utf8();

        if is_cjk(ch) || is_boundary_char(ch) {
            boundary_end = Some(exact_end);
        }

        prev = Some(ch);
    }

    let mut preview = content[..boundary_end.unwrap_or(exact_end)].to_string();
    while preview
        .chars()
        .last()
        .is_some_and(|ch| ch.is_whitespace() || is_boundary_char(ch))
    {
        preview.pop();
    }
    if preview.is_empty() {
        preview = content[..exact_end].to_string();
    }
    preview.push('…');

    PreviewText {
        content: preview,
        truncated: true,
    }
}

fn is_boundary_char(ch: char) -> bool {
    ch.is_whitespace()
        || ch.is_ascii_punctuation()
        || matches!(
            ch,
            '，' | '。'
                | '、'
                | '；'
                | '：'
                | '！'
                | '？'
                | '（'
                | '）'
                | '【'
                | '】'
                | '「'
                | '」'
                | '《'
                | '》'
        )
}

fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x3040..=0x309F
            | 0x30A0..=0x30FF
            | 0xAC00..=0xD7AF
    )
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn test_short_content_not_truncated() {
        let content = "short content";
        let preview = truncate(content, 120);

        assert_eq!(preview.content, content);
        assert!(!preview.truncated);
    }

    #[test]
    fn test_truncation_aligns_to_word_boundary() {
        let preview = truncate("The quick brown fox jumps over the lazy dog", 20);

        assert!(preview.truncated);
        assert!(preview.content.ends_with('…'));
        assert!(preview.content.chars().count() <= 21);
        assert!(!preview.content.contains("fox j"));
    }

    #[test]
    fn test_cjk_truncation_utf8_safe() {
        let preview = truncate(
            "系统决策：采用共享内存同步机制解决状态漂移问题的根本原因是并发安全",
            10,
        );

        assert!(preview.truncated);
        assert!(preview.content.ends_with('…'));
        assert!(preview.content.chars().count() <= 11);
        assert!(std::str::from_utf8(preview.content.as_bytes()).is_ok());
    }
}
