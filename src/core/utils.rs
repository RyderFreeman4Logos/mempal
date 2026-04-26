use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::types::TaxonomyEntry;
use crate::cowork::peek::{format_rfc3339, parse_rfc3339};

pub const DEFAULT_ROOM: &str = "default";

pub fn build_drawer_id(wing: &str, room: Option<&str>, content: &str) -> String {
    build_scoped_drawer_id(wing, room, content, None)
}

pub fn build_scoped_drawer_id(
    wing: &str,
    room: Option<&str>,
    content: &str,
    scope_seed: Option<&str>,
) -> String {
    let room = room.unwrap_or(DEFAULT_ROOM);
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    if let Some(scope_seed) = scope_seed {
        hasher.update([0]);
        hasher.update(scope_seed.as_bytes());
    }
    let digest = format!("{:x}", hasher.finalize());

    format!(
        "drawer_{}_{}_{}",
        sanitize_component(wing),
        sanitize_component(room),
        &digest[..8]
    )
}

pub fn build_triple_id(subject: &str, predicate: &str, object: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(subject.as_bytes());
    hasher.update([0]);
    hasher.update(predicate.as_bytes());
    hasher.update([0]);
    hasher.update(object.as_bytes());
    let digest = format!("{:x}", hasher.finalize());

    format!(
        "triple_{}_{}_{}",
        sanitize_component_prefix(subject, 8),
        sanitize_component_prefix(predicate, 8),
        &digest[..8]
    )
}

pub fn current_timestamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs().to_string(),
        Err(_) => "0".to_string(),
    }
}

/// Return the current UTC time as an RFC 3339 string with second precision
/// (e.g. `"2026-04-26T05:39:49Z"`).  Use this for `added_at` fields in new
/// drawers so the column is uniformly ISO 8601.
pub fn iso_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_rfc3339(UNIX_EPOCH + Duration::from_secs(secs))
}

/// Attempt to normalise a stored `added_at` value to RFC 3339 UTC.
///
/// Returns:
/// - `Some(iso)` — value was a bare Unix-epoch integer string and was
///   converted successfully.
/// - `None` — value is already an ISO 8601 string (idempotent skip) **or**
///   is unrecognised garbage (skipped gracefully; caller should log a
///   warning).
pub fn normalize_added_at(value: &str) -> Option<String> {
    let v = value.trim();
    // Already ISO 8601 — skip (idempotent).
    if parse_rfc3339(v).is_some() {
        return None;
    }
    // Bare Unix-epoch integer — convert.
    if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(secs) = v.parse::<u64>() {
            return Some(format_rfc3339(UNIX_EPOCH + Duration::from_secs(secs)));
        }
    }
    // Unrecognised — caller should log a warning, we return None.
    None
}

pub fn synthetic_source_file(drawer_id: &str) -> String {
    format!("mempal://drawer/{drawer_id}")
}

pub fn source_file_or_synthetic(drawer_id: &str, source_file: Option<&str>) -> String {
    source_file
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| synthetic_source_file(drawer_id))
}

pub fn route_room_from_taxonomy(content: &str, wing: &str, taxonomy: &[TaxonomyEntry]) -> String {
    let normalized_content = content.to_lowercase();
    let content_terms = content_terms(&normalized_content);

    taxonomy
        .iter()
        .filter(|entry| entry.wing == wing)
        .filter_map(|entry| {
            let matched_keywords = matched_keywords(&normalized_content, &content_terms, entry);
            (!matched_keywords.is_empty()).then_some((entry, matched_keywords))
        })
        .max_by(|(left_entry, left_matches), (right_entry, right_matches)| {
            left_matches
                .len()
                .cmp(&right_matches.len())
                .then_with(|| {
                    left_matches
                        .iter()
                        .map(String::len)
                        .sum::<usize>()
                        .cmp(&right_matches.iter().map(String::len).sum::<usize>())
                })
                .then_with(|| left_entry.keywords.len().cmp(&right_entry.keywords.len()))
        })
        .map(|(entry, _)| {
            if entry.room.trim().is_empty() {
                DEFAULT_ROOM.to_string()
            } else {
                entry.room.clone()
            }
        })
        .unwrap_or_else(|| DEFAULT_ROOM.to_string())
}

fn sanitize_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn sanitize_component_prefix(value: &str, max_len: usize) -> String {
    let sanitized = sanitize_component(value);
    let prefix: String = sanitized.chars().take(max_len).collect();
    if prefix.is_empty() {
        "x".to_string()
    } else {
        prefix
    }
}

fn matched_keywords(
    normalized_content: &str,
    content_terms: &BTreeSet<String>,
    entry: &TaxonomyEntry,
) -> Vec<String> {
    entry
        .keywords
        .iter()
        .map(|keyword| keyword.trim().to_lowercase())
        .filter(|keyword| {
            !keyword.is_empty()
                && (content_terms.contains(keyword)
                    || normalized_content.contains(keyword.as_str()))
        })
        .collect()
}

fn content_terms(content: &str) -> BTreeSet<String> {
    content
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_added_at_unix_epoch_to_iso() {
        let ts: u64 = 1_777_169_989;
        let result = normalize_added_at("1777169989");
        assert!(result.is_some(), "expected Some for Unix epoch string");
        let iso = result.unwrap();
        // Verify round-trip: parsing back gives the same epoch seconds.
        let parsed = parse_rfc3339(&iso).expect("converted value must be valid RFC3339");
        assert_eq!(parsed as u64, ts);
        assert!(iso.ends_with('Z'), "output must be UTC");
        assert!(iso.contains('T'), "output must contain date/time separator");
    }

    #[test]
    fn test_normalize_added_at_already_iso_returns_none() {
        assert_eq!(normalize_added_at("2026-04-26T05:39:49Z"), None);
        assert_eq!(normalize_added_at("2026-04-26T05:39:49+08:00"), None);
    }

    #[test]
    fn test_normalize_added_at_garbage_returns_none() {
        assert_eq!(normalize_added_at("not-a-date"), None);
        assert_eq!(normalize_added_at(""), None);
        assert_eq!(normalize_added_at("   "), None);
    }
}
