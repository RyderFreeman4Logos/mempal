use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::core::config::ChunkerConfig;
use crate::embed::Embedder;

/// Global chunker statistics, updated atomically during ingest.
pub struct ChunkerStats {
    /// Chunks that exceeded `effective_max` and required a hard split
    /// (no natural break point found).
    pub hard_split_count: AtomicU64,
    /// Source file that last triggered a hard split.
    last_hard_split_source: std::sync::Mutex<Option<String>>,
}

impl ChunkerStats {
    fn new() -> Self {
        Self {
            hard_split_count: AtomicU64::new(0),
            last_hard_split_source: std::sync::Mutex::new(None),
        }
    }

    pub fn record_hard_split(&self, source: Option<&str>) {
        self.hard_split_count.fetch_add(1, Ordering::Relaxed);
        if let Some(source) = source {
            if let Ok(mut guard) = self.last_hard_split_source.lock() {
                *guard = Some(source.to_string());
            }
        }
    }

    pub fn snapshot(&self) -> ChunkerStatsSnapshot {
        ChunkerStatsSnapshot {
            hard_split_count: self.hard_split_count.load(Ordering::Relaxed),
            last_hard_split_source: self
                .last_hard_split_source
                .lock()
                .ok()
                .and_then(|guard| guard.clone()),
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ChunkerStatsSnapshot {
    pub hard_split_count: u64,
    pub last_hard_split_source: Option<String>,
}

pub fn global_chunker_stats() -> &'static ChunkerStats {
    static STATS: OnceLock<ChunkerStats> = OnceLock::new();
    STATS.get_or_init(ChunkerStats::new)
}

/// Effective max tokens: `min(config.max_tokens, embedder.max_input_tokens - safety_margin)`.
/// The safety margin (32 tokens) accounts for special tokens the backend may prepend.
const SAFETY_MARGIN: usize = 32;

pub fn effective_max_tokens<E: Embedder + ?Sized>(config: &ChunkerConfig, embedder: &E) -> usize {
    let embedder_limit = embedder
        .max_input_tokens()
        .map(|limit| limit.saturating_sub(SAFETY_MARGIN))
        .unwrap_or(usize::MAX);
    config.max_tokens.min(embedder_limit).max(1)
}

/// Token-aware text chunking. Splits `text` into chunks where each chunk
/// has at most `effective_max` estimated tokens. Prefers splitting at natural
/// break points (newline, space, tab). When no natural break is found, performs
/// a hard character-level split and increments the global hard-split counter.
///
/// The `source` parameter is used for observability (recorded on hard splits).
pub fn chunk_text_token_aware<E: Embedder + ?Sized>(
    text: &str,
    config: &ChunkerConfig,
    embedder: &E,
    source: Option<&str>,
) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let max_tokens = effective_max_tokens(config, embedder);
    let target_tokens = config.target_tokens.min(max_tokens);
    let overlap_tokens = config.overlap_tokens.min(target_tokens.saturating_sub(1));

    let chars: Vec<char> = trimmed.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0usize;

    while start < chars.len() {
        // Find the end position that fits within max_tokens
        let end = find_chunk_end(&chars, start, target_tokens, max_tokens, embedder, source);

        let chunk: String = chars[start..end].iter().collect::<String>();
        let chunk = chunk.trim().to_string();
        if !chunk.is_empty() {
            chunks.push(chunk);
        }

        if end >= chars.len() {
            break;
        }

        // Overlap: step back by overlap_tokens worth of characters
        let overlap_chars = estimate_chars_for_tokens(overlap_tokens);
        let next_start = end.saturating_sub(overlap_chars);
        start = if next_start <= start { end } else { next_start };
    }

    chunks
}

/// Find the end position for a chunk starting at `start` that fits within
/// `max_tokens`. Tries to hit `target_tokens` at a natural break point.
fn find_chunk_end<E: Embedder + ?Sized>(
    chars: &[char],
    start: usize,
    target_tokens: usize,
    max_tokens: usize,
    embedder: &E,
    source: Option<&str>,
) -> usize {
    // Estimate characters for target tokens (generous estimate)
    let target_chars = estimate_chars_for_tokens(target_tokens);

    // Initial end: aim for target_chars but don't exceed remaining
    let tentative_end = (start + target_chars).min(chars.len());

    // Check if the tentative end fits
    let tentative_text: String = chars[start..tentative_end].iter().collect();
    let tentative_token_count = embedder.estimate_tokens(&tentative_text);

    if tentative_end >= chars.len() && tentative_token_count <= max_tokens {
        // Remaining text fits entirely
        return chars.len();
    }

    if tentative_token_count <= max_tokens {
        // Tentative fits; try to extend to a natural break within max_tokens
        return find_natural_break_forward(
            chars,
            start,
            tentative_end,
            max_tokens,
            embedder,
            source,
        );
    }

    // Tentative is too big; binary-search backwards for max_tokens boundary
    find_max_end_binary(chars, start, tentative_end, max_tokens, embedder, source)
}

/// Starting from `current_end`, try extending forward to fill up to `max_tokens`,
/// preferring natural break points.
fn find_natural_break_forward<E: Embedder + ?Sized>(
    chars: &[char],
    start: usize,
    current_end: usize,
    max_tokens: usize,
    embedder: &E,
    source: Option<&str>,
) -> usize {
    let max_chars = estimate_chars_for_tokens(max_tokens);
    let search_limit = (start + max_chars).min(chars.len());

    // Find the rightmost position that still fits within max_tokens
    // Use a stepping approach: extend in larger steps, then refine
    let mut best_end = current_end;

    // Step forward in increments to find the boundary
    let step = ((search_limit - current_end) / 8).max(1);
    let mut probe = current_end;
    while probe <= search_limit {
        let text: String = chars[start..probe].iter().collect();
        let tokens = embedder.estimate_tokens(&text);
        if tokens > max_tokens {
            break;
        }
        best_end = probe;
        if probe == search_limit {
            break;
        }
        probe = (probe + step).min(search_limit);
    }

    // Now look for a natural break in the latter half of [start..best_end]
    if best_end < chars.len() {
        let half = (best_end - start) / 2;
        if let Some(split) = chars[start..best_end]
            .iter()
            .rposition(|ch| matches!(ch, '\n' | ' ' | '\t'))
            && split > half
        {
            return start + split + 1;
        }
    }

    // No natural break found in second half — this is a hard split
    if best_end < chars.len() {
        global_chunker_stats().record_hard_split(source);
        tracing::warn!(
            source = source.unwrap_or("<unknown>"),
            start,
            end = best_end,
            "hard split: no natural break point found in chunk"
        );
    }

    best_end
}

/// Binary search for the maximum end position where the chunk fits in max_tokens.
fn find_max_end_binary<E: Embedder + ?Sized>(
    chars: &[char],
    start: usize,
    upper: usize,
    max_tokens: usize,
    embedder: &E,
    source: Option<&str>,
) -> usize {
    let mut lo = start + 1;
    let mut hi = upper;
    let mut best = start + 1;

    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let text: String = chars[start..mid].iter().collect();
        let tokens = embedder.estimate_tokens(&text);
        if tokens <= max_tokens {
            best = mid;
            lo = mid + 1;
        } else {
            if mid == 0 {
                break;
            }
            hi = mid - 1;
        }
    }

    // Try to find natural break point in latter half
    if best < chars.len() {
        let half = (best - start) / 2;
        if let Some(split) = chars[start..best]
            .iter()
            .rposition(|ch| matches!(ch, '\n' | ' ' | '\t'))
            && split > half
        {
            return start + split + 1;
        }
        // Hard split
        global_chunker_stats().record_hard_split(source);
        tracing::warn!(
            source = source.unwrap_or("<unknown>"),
            start,
            end = best,
            "hard split: no natural break point found in chunk"
        );
    }

    best
}

/// Conservative estimate: how many characters correspond to N tokens.
/// Uses `tokens * 2.5` (inverse of the heuristic `chars / 2.5`).
fn estimate_chars_for_tokens(tokens: usize) -> usize {
    (tokens * 5).div_ceil(2)
}

/// Token-aware conversation chunking. First splits by user turns ("> " marker),
/// then enforces `effective_max` on each turn by further splitting oversized turns.
pub fn chunk_conversation_token_aware<E: Embedder + ?Sized>(
    transcript: &str,
    config: &ChunkerConfig,
    embedder: &E,
    source: Option<&str>,
) -> Vec<String> {
    let raw_turns = chunk_conversation_by_turns(transcript);
    if raw_turns.is_empty() {
        return Vec::new();
    }

    let max_tokens = effective_max_tokens(config, embedder);
    let mut chunks = Vec::new();

    for turn in &raw_turns {
        let token_count = embedder.estimate_tokens(turn);
        if token_count <= max_tokens {
            chunks.push(turn.clone());
        } else {
            // Oversized turn: split it further using token-aware text chunking
            let sub_chunks = chunk_text_token_aware(turn, config, embedder, source);
            chunks.extend(sub_chunks);
        }
    }

    chunks
}

/// Split a transcript into turns at user-turn boundaries (`> ` prefix).
/// This preserves the original conversation-level splitting logic.
fn chunk_conversation_by_turns(transcript: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();

    for line in transcript.lines() {
        let is_user_turn = line.starts_with("> ");
        if is_user_turn && !current.is_empty() {
            chunks.push(current.join("\n"));
            current.clear();
        }

        if !line.trim().is_empty() || !current.is_empty() {
            current.push(line.to_string());
        }
    }

    if !current.is_empty() {
        chunks.push(current.join("\n"));
    }

    chunks
}

// ---- Legacy API (kept for backward compatibility of existing callers) ----

pub fn chunk_text(text: &str, window: usize, overlap: usize) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || window == 0 {
        return Vec::new();
    }

    debug_assert!(
        overlap < window,
        "chunk overlap ({overlap}) must be less than window ({window})"
    );
    let overlap = overlap.min(window.saturating_sub(1));

    let chars = trimmed.chars().collect::<Vec<_>>();
    let mut chunks = Vec::new();
    let mut start = 0usize;

    while start < chars.len() {
        let mut end = usize::min(start + window, chars.len());

        if end < chars.len()
            && let Some(split) = chars[start..end]
                .iter()
                .rposition(|ch| matches!(ch, '\n' | ' ' | '\t'))
            && split > window / 2
        {
            end = start + split + 1;
        }

        let chunk = chars[start..end]
            .iter()
            .collect::<String>()
            .trim()
            .to_string();
        if !chunk.is_empty() {
            chunks.push(chunk);
        }

        if end == chars.len() {
            break;
        }

        let next_start = end.saturating_sub(overlap);
        start = if next_start <= start { end } else { next_start };
    }

    chunks
}

pub fn chunk_conversation(transcript: &str) -> Vec<String> {
    chunk_conversation_by_turns(transcript)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test embedder that uses the default heuristic estimator
    /// and reports a configurable max_input_tokens.
    struct TestEmbedder {
        max_tokens: Option<usize>,
    }

    impl TestEmbedder {
        fn new(max_tokens: Option<usize>) -> Self {
            Self { max_tokens }
        }
    }

    #[async_trait::async_trait]
    impl Embedder for TestEmbedder {
        async fn embed(&self, _texts: &[&str]) -> crate::embed::Result<Vec<Vec<f32>>> {
            Ok(vec![vec![0.0; 4]])
        }
        fn dimensions(&self) -> usize {
            4
        }
        fn name(&self) -> &str {
            "test"
        }
        fn max_input_tokens(&self) -> Option<usize> {
            self.max_tokens
        }
    }

    fn default_config() -> ChunkerConfig {
        ChunkerConfig {
            max_tokens: 1024,
            target_tokens: 512,
            overlap_tokens: 64,
        }
    }

    #[test]
    fn test_legacy_chunk_text_unchanged() {
        let text = "hello world this is a test";
        let chunks = chunk_text(text, 10, 2);
        assert!(!chunks.is_empty());
        // All chars should be covered
        assert!(chunks.iter().all(|c| !c.is_empty()));
    }

    #[test]
    fn test_legacy_chunk_conversation_unchanged() {
        let transcript = "> user: hello\nassistant: hi\n> user: bye\nassistant: goodbye";
        let chunks = chunk_conversation(transcript);
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn test_token_aware_short_text_single_chunk() {
        let embedder = TestEmbedder::new(Some(1024));
        let config = default_config();
        let text = "Hello, world!";
        let chunks = chunk_text_token_aware(text, &config, &embedder, None);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "Hello, world!");
    }

    #[test]
    fn test_token_aware_respects_max_tokens() {
        let embedder = TestEmbedder::new(Some(100));
        let config = ChunkerConfig {
            max_tokens: 100,
            target_tokens: 50,
            overlap_tokens: 10,
        };
        // Generate a long text: ~500 chars = ~200 estimated tokens
        let text = "word ".repeat(100);
        let chunks = chunk_text_token_aware(&text, &config, &embedder, None);
        assert!(chunks.len() > 1, "should split into multiple chunks");
        for chunk in &chunks {
            let tokens = embedder.estimate_tokens(chunk);
            assert!(
                tokens <= 100,
                "chunk has {tokens} tokens, exceeds max 100: {chunk:?}"
            );
        }
    }

    #[test]
    fn test_token_aware_10k_single_line_hard_split() {
        let embedder = TestEmbedder::new(Some(512));
        let config = ChunkerConfig {
            max_tokens: 512,
            target_tokens: 256,
            overlap_tokens: 32,
        };
        // 10K chars of continuous text (no spaces) -> forces hard splits
        let text = "x".repeat(10_000);
        let chunks = chunk_text_token_aware(&text, &config, &embedder, Some("test-10k"));
        assert!(chunks.len() > 1, "should split into multiple chunks");
        for chunk in &chunks {
            let tokens = embedder.estimate_tokens(chunk);
            assert!(tokens <= 512, "chunk has {tokens} tokens, exceeds max 512");
        }
        // Hard splits should have been recorded
        let stats = global_chunker_stats().snapshot();
        assert!(
            stats.hard_split_count > 0,
            "hard split counter should be > 0"
        );
    }

    #[test]
    fn test_token_aware_conversation_oversized_turn() {
        let embedder = TestEmbedder::new(Some(100));
        let config = ChunkerConfig {
            max_tokens: 100,
            target_tokens: 50,
            overlap_tokens: 10,
        };
        // A 50K-token-equivalent assistant turn
        let big_turn = "word ".repeat(500);
        let transcript = format!("> user: hello\n{big_turn}\n> user: bye\nassistant: goodbye");
        let chunks =
            chunk_conversation_token_aware(&transcript, &config, &embedder, Some("test-conv"));
        assert!(
            chunks.len() >= 3,
            "oversized turn should be split: got {} chunks",
            chunks.len()
        );
        for chunk in &chunks {
            let tokens = embedder.estimate_tokens(chunk);
            assert!(tokens <= 100, "chunk has {tokens} tokens, exceeds max 100");
        }
    }

    #[test]
    fn test_conversation_union_covers_original() {
        let embedder = TestEmbedder::new(Some(200));
        let config = ChunkerConfig {
            max_tokens: 200,
            target_tokens: 100,
            overlap_tokens: 20,
        };
        // Build a 50K-token assistant turn (word-separated for natural breaks)
        let big_turn = "word ".repeat(2000);
        let transcript = format!("> user: hello\n{big_turn}\n> user: bye\nassistant: goodbye");

        let chunks = chunk_conversation_token_aware(&transcript, &config, &embedder, None);

        // Concatenate all chunks into one string for coverage check
        let combined = chunks.join(" ");

        // Every word from the original content should appear in the combined output.
        // "word" appears many times. Also check the control tokens.
        assert!(combined.contains("user: hello"), "user: hello not found");
        assert!(combined.contains("user: bye"), "user: bye not found");
        assert!(combined.contains("assistant: goodbye"), "goodbye not found");
        // The "word" content should be preserved (at least the unique words)
        assert!(combined.contains("word"), "word content not found");
        // Total word count should be reasonable (overlap may duplicate, but not lose)
        let word_count = combined.matches("word").count();
        assert!(
            word_count >= 2000,
            "expected at least 2000 'word' occurrences, got {word_count}"
        );
    }

    #[test]
    fn test_effective_max_tokens_clamps_to_embedder() {
        let embedder = TestEmbedder::new(Some(512));
        let config = ChunkerConfig {
            max_tokens: 1024,
            target_tokens: 512,
            overlap_tokens: 64,
        };
        // 512 - 32 (safety margin) = 480
        assert_eq!(effective_max_tokens(&config, &embedder), 480);
    }

    #[test]
    fn test_effective_max_tokens_no_embedder_limit() {
        let embedder = TestEmbedder::new(None);
        let config = ChunkerConfig {
            max_tokens: 1024,
            target_tokens: 512,
            overlap_tokens: 64,
        };
        assert_eq!(effective_max_tokens(&config, &embedder), 1024);
    }

    #[test]
    fn test_effective_max_tokens_config_lower() {
        let embedder = TestEmbedder::new(Some(8192));
        let config = ChunkerConfig {
            max_tokens: 512,
            target_tokens: 256,
            overlap_tokens: 32,
        };
        // config.max_tokens=512 < embedder 8192-32=8160
        assert_eq!(effective_max_tokens(&config, &embedder), 512);
    }

    #[test]
    fn test_cjk_content_respects_token_limit() {
        let embedder = TestEmbedder::new(Some(100));
        let config = ChunkerConfig {
            max_tokens: 100,
            target_tokens: 50,
            overlap_tokens: 10,
        };
        // CJK: each char ~2 tokens in heuristic (chars/2.5 actually, but dense)
        let cjk = "中".repeat(500);
        let chunks = chunk_text_token_aware(&cjk, &config, &embedder, None);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            let tokens = embedder.estimate_tokens(chunk);
            assert!(tokens <= 100, "CJK chunk has {tokens} tokens, exceeds 100");
        }
    }

    #[test]
    fn test_base64_content_respects_token_limit() {
        let embedder = TestEmbedder::new(Some(100));
        let config = ChunkerConfig {
            max_tokens: 100,
            target_tokens: 50,
            overlap_tokens: 10,
        };
        // Dense base64-like content (no spaces)
        let base64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".repeat(20);
        let chunks = chunk_text_token_aware(&base64, &config, &embedder, None);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            let tokens = embedder.estimate_tokens(chunk);
            assert!(
                tokens <= 100,
                "base64 chunk has {tokens} tokens, exceeds 100"
            );
        }
    }

    #[test]
    fn test_empty_text_returns_empty() {
        let embedder = TestEmbedder::new(Some(100));
        let config = default_config();
        assert!(chunk_text_token_aware("", &config, &embedder, None).is_empty());
        assert!(chunk_text_token_aware("   ", &config, &embedder, None).is_empty());
    }

    #[test]
    fn test_estimate_tokens_heuristic() {
        let embedder = TestEmbedder::new(None);
        // 100 ASCII chars → ceil(100/2.5) = 40 tokens
        assert_eq!(embedder.estimate_tokens(&"a".repeat(100)), 40);
        // 5 chars → ceil(5/2.5) = 2
        assert_eq!(embedder.estimate_tokens("hello"), 2);
        // Empty
        assert_eq!(embedder.estimate_tokens(""), 0);
    }
}
