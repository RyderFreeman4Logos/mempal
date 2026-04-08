#![warn(clippy::all)]

pub mod chunk;
pub mod detect;
pub mod normalize;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use mempal_core::{
    db::Database,
    types::{Drawer, SourceType},
};
use mempal_embed::Embedder;
use sha2::{Digest, Sha256};

use crate::{
    chunk::{chunk_conversation, chunk_text},
    detect::{Format, detect_format},
    normalize::normalize_content,
};

const CHUNK_WINDOW: usize = 800;
const CHUNK_OVERLAP: usize = 100;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngestStats {
    pub files: usize,
    pub chunks: usize,
    pub skipped: usize,
}

pub async fn ingest_file(
    db: &Database,
    embedder: &impl Embedder,
    path: &Path,
    wing: &str,
    room: Option<&str>,
) -> Result<IngestStats> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    let content = String::from_utf8_lossy(&bytes).to_string();
    if content.trim().is_empty() {
        return Ok(IngestStats {
            files: 1,
            ..IngestStats::default()
        });
    }

    let format = detect_format(&content);
    let normalized = normalize_content(&content, format)
        .with_context(|| format!("failed to normalize {}", path.display()))?;
    let chunks = match format {
        Format::ClaudeJsonl | Format::ChatGptJson => chunk_conversation(&normalized),
        Format::PlainText => chunk_text(&normalized, CHUNK_WINDOW, CHUNK_OVERLAP),
    };
    if chunks.is_empty() {
        return Ok(IngestStats {
            files: 1,
            ..IngestStats::default()
        });
    }

    let chunk_refs = chunks.iter().map(String::as_str).collect::<Vec<_>>();
    let vectors = embedder
        .embed(&chunk_refs)
        .await
        .with_context(|| format!("failed to embed chunks from {}", path.display()))?;

    let mut stats = IngestStats {
        files: 1,
        ..IngestStats::default()
    };

    for (chunk_index, (chunk, vector)) in chunks.iter().zip(vectors.iter()).enumerate() {
        let drawer_id = build_drawer_id(wing, room, chunk);
        if drawer_exists(db, &drawer_id)? {
            stats.skipped += 1;
            continue;
        }

        let drawer = Drawer {
            id: drawer_id.clone(),
            content: chunk.clone(),
            wing: wing.to_string(),
            room: room.map(ToOwned::to_owned),
            source_file: Some(path.to_string_lossy().to_string()),
            source_type: source_type_for(format),
            added_at: current_timestamp(),
            chunk_index: Some(chunk_index as i64),
        };

        db.insert_drawer(&drawer)
            .with_context(|| format!("failed to insert drawer {}", drawer.id))?;
        insert_vector(db, &drawer_id, vector)
            .with_context(|| format!("failed to insert vector for {}", drawer.id))?;
        stats.chunks += 1;
    }

    Ok(stats)
}

pub async fn ingest_dir(
    db: &Database,
    embedder: &impl Embedder,
    dir: &Path,
    wing: &str,
    room: Option<&str>,
) -> Result<IngestStats> {
    let mut stats = IngestStats::default();
    let mut stack = vec![dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)
            .with_context(|| format!("failed to read directory {}", current.display()))?
        {
            let entry =
                entry.with_context(|| format!("failed to read entry in {}", current.display()))?;
            let path = entry.path();

            if path.is_dir() {
                if should_skip_dir(&path) {
                    continue;
                }
                stack.push(path);
                continue;
            }

            if path.is_file() {
                let file_stats = ingest_file(db, embedder, &path, wing, room).await?;
                stats.files += file_stats.files;
                stats.chunks += file_stats.chunks;
                stats.skipped += file_stats.skipped;
            }
        }
    }

    Ok(stats)
}

fn drawer_exists(db: &Database, drawer_id: &str) -> Result<bool> {
    let exists = db
        .conn()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM drawers WHERE id = ?1)",
            [drawer_id],
            |row| row.get::<_, i64>(0),
        )
        .context("failed to query for existing drawer")?;

    Ok(exists == 1)
}

fn insert_vector(db: &Database, drawer_id: &str, vector: &[f32]) -> Result<()> {
    let vector_json = serde_json::to_string(vector).context("failed to serialize vector")?;
    db.conn()
        .execute(
            "INSERT INTO drawer_vectors (id, embedding) VALUES (?1, vec_f32(?2))",
            (drawer_id, vector_json.as_str()),
        )
        .context("failed to insert vector row")?;
    Ok(())
}

fn build_drawer_id(wing: &str, room: Option<&str>, content: &str) -> String {
    let room = room.unwrap_or("default");
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let digest = format!("{:x}", hasher.finalize());

    format!(
        "drawer_{}_{}_{}",
        sanitize_component(wing),
        sanitize_component(room),
        &digest[..8]
    )
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

fn source_type_for(format: Format) -> SourceType {
    match format {
        Format::ClaudeJsonl | Format::ChatGptJson => SourceType::Conversation,
        Format::PlainText => SourceType::Project,
    }
}

fn current_timestamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs().to_string(),
        Err(_) => "0".to_string(),
    }
}

fn should_skip_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| matches!(name, ".git" | "target" | "node_modules"))
        .unwrap_or(false)
}
