#![warn(clippy::all)]

pub mod chunk;
pub mod detect;
pub mod gating;
pub mod lock;
pub mod normalize;
pub mod novelty;

use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;

use crate::core::{
    config::ConfigHandle,
    config::IngestGatingConfig,
    db::Database,
    types::{Drawer, SourceType},
    utils::{build_drawer_id, iso_timestamp, route_room_from_taxonomy},
};
use crate::embed::{EmbedError, Embedder};
use crate::ingest::gating::{PrototypeClassifier, evaluate_tier1, evaluate_tier2};
use thiserror::Error;

use crate::ingest::{
    chunk::{chunk_conversation_token_aware, chunk_text_token_aware},
    detect::{Format, detect_format},
    normalize::{NormalizeError, normalize_content},
};

/// Max wait for per-source ingest lock before returning LockError::Timeout.
const LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Derive `mempal_home` from the DB path by taking the parent of
/// `palace.db`. Falls back to `./` on unusual layouts.
fn mempal_home_from_db(db: &Database) -> PathBuf {
    db.path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngestStats {
    pub files: usize,
    pub chunks: usize,
    pub skipped: usize,
    pub dropped_by_gate: usize,
    /// Time waited acquiring the per-source ingest lock (P9-B). `None`
    /// when the lock was bypassed (e.g. dry-run) or when no wait was
    /// needed and the path took the fast exit before lock acquisition.
    pub lock_wait_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct IngestOptions<'a> {
    pub room: Option<&'a str>,
    pub source_root: Option<&'a Path>,
    pub dry_run: bool,
    pub project_id: Option<&'a str>,
    pub gating: Option<&'a IngestGatingConfig>,
    pub prototype_classifier: Option<&'a PrototypeClassifier>,
}

pub type Result<T> = std::result::Result<T, IngestError>;

#[derive(Debug, Error)]
pub enum IngestError {
    #[error("failed to read {path}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to normalize {path}")]
    Normalize {
        path: PathBuf,
        #[source]
        source: NormalizeError,
    },
    #[error("failed to load taxonomy for wing {wing}")]
    LoadTaxonomy {
        wing: String,
        #[source]
        source: crate::core::db::DbError,
    },
    #[error("failed to embed chunks from {path}")]
    EmbedChunks {
        path: PathBuf,
        #[source]
        source: EmbedError,
    },
    #[error("failed to check drawer {drawer_id}")]
    CheckDrawer {
        drawer_id: String,
        #[source]
        source: crate::core::db::DbError,
    },
    #[error("failed to insert drawer {drawer_id}")]
    InsertDrawer {
        drawer_id: String,
        #[source]
        source: crate::core::db::DbError,
    },
    #[error("failed to insert vector for {drawer_id}")]
    InsertVector {
        drawer_id: String,
        #[source]
        source: crate::core::db::DbError,
    },
    #[error(
        "embedding dimension mismatch: drawer_vectors uses {current_dim}d but embedder returned {new_dim}d; run `mempal reindex --embedder <name>` before ingesting more content"
    )]
    VectorDimensionMismatch { current_dim: usize, new_dim: usize },
    #[error("failed to acquire ingest lock: {0}")]
    Lock(#[from] lock::LockError),
    #[error("failed to read directory {path}")]
    ReadDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read entry in {path}")]
    ReadDirEntry {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Chunk content for embedding using the token-aware chunker.
///
/// This is the shared entry point for MCP, REST, and any non-file ingest
/// path that receives pre-processed text. Uses `chunk_text_token_aware`
/// (plaintext mode) since the caller has already normalized the content.
///
/// Returns a `Vec<String>` where each element fits within the embedder's
/// `max_input_tokens` limit. For short content that fits in a single chunk,
/// the returned vec has exactly one element.
pub fn prepare_chunks<E: Embedder + ?Sized>(
    content: &str,
    config: &crate::core::config::ChunkerConfig,
    embedder: &E,
) -> Vec<String> {
    chunk_text_token_aware(content, config, embedder, None)
}

pub async fn ingest_file<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    path: &Path,
    wing: &str,
    room: Option<&str>,
) -> Result<IngestStats> {
    ingest_file_with_options(
        db,
        embedder,
        path,
        wing,
        IngestOptions {
            room,
            source_root: path.parent(),
            dry_run: false,
            project_id: None,
            gating: None,
            prototype_classifier: None,
        },
    )
    .await
}

pub async fn ingest_file_with_options<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    path: &Path,
    wing: &str,
    options: IngestOptions<'_>,
) -> Result<IngestStats> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|source| IngestError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
    let content = String::from_utf8_lossy(&bytes).to_string();
    if content.trim().is_empty() {
        return Ok(IngestStats {
            files: 1,
            ..IngestStats::default()
        });
    }

    let format = detect_format(&content);
    let normalized =
        normalize_content(&content, format).map_err(|source| IngestError::Normalize {
            path: path.to_path_buf(),
            source,
        })?;
    let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
    let scrubbed = config.scrub_content_with_compiled(&normalized, compiled_privacy.as_ref());
    let resolved_room = match options.room {
        Some(room) => room.to_string(),
        None => {
            let taxonomy = db
                .taxonomy_entries()
                .map_err(|source| IngestError::LoadTaxonomy {
                    wing: wing.to_string(),
                    source,
                })?;
            route_room_from_taxonomy(&scrubbed, wing, &taxonomy)
        }
    };
    let source_display = path.to_string_lossy();
    let chunker_config = &config.chunker;
    let chunks = match format {
        Format::ClaudeJsonl | Format::ChatGptJson | Format::CodexJsonl | Format::SlackJson => {
            chunk_conversation_token_aware(
                &scrubbed,
                chunker_config,
                embedder,
                Some(&source_display),
            )
        }
        Format::PlainText => {
            chunk_text_token_aware(&scrubbed, chunker_config, embedder, Some(&source_display))
        }
    };
    if chunks.is_empty() {
        return Ok(IngestStats {
            files: 1,
            ..IngestStats::default()
        });
    }

    let mut stats = IngestStats {
        files: 1,
        ..IngestStats::default()
    };
    let source_file = normalize_source_file(path, options.source_root);

    // Per-source ingest lock (P9-B). Guards dedup-check + insert critical
    // section against concurrent Claude↔Codex ingests of the same source.
    // Skip in dry-run — no writes happen there, so race is impossible.
    let _lock_guard = if options.dry_run {
        None
    } else {
        let home = mempal_home_from_db(db);
        let key = lock::source_key(Path::new(&source_file));
        let guard = lock::acquire_source_lock(&home, &key, LOCK_TIMEOUT)?;
        stats.lock_wait_ms = Some(guard.wait_duration().as_millis() as u64);
        Some(guard)
    };

    let mut pending = Vec::new();

    for (chunk_index, chunk) in chunks.iter().enumerate() {
        let (drawer_id, exists) = db
            .resolve_ingest_drawer_id(
                wing,
                Some(resolved_room.as_str()),
                chunk,
                options.project_id,
            )
            .map_err(|source| IngestError::CheckDrawer {
                drawer_id: build_drawer_id(wing, Some(resolved_room.as_str()), chunk),
                source,
            })?;
        if exists {
            stats.skipped += 1;
            continue;
        }

        if options.dry_run {
            stats.chunks += 1;
            continue;
        }

        if let Some(gating) = options.gating {
            let candidate = gating::IngestCandidate {
                content: chunk.to_string(),
                tool_name: None,
                exit_code: None,
            };
            let mut gating_decision = evaluate_tier1(&candidate, gating);
            if gating_decision.is_none()
                && let Some(classifier) = options.prototype_classifier
            {
                let tier2 = evaluate_tier2(
                    &candidate,
                    classifier,
                    embedder,
                    gating.embedding_classifier.threshold,
                )
                .await;
                gating_decision = Some(tier2.decision);
            }
            if let Some(decision) = gating_decision.as_ref() {
                db.record_gating_audit(&drawer_id, decision, options.project_id)
                    .map_err(|source| IngestError::InsertDrawer {
                        drawer_id: drawer_id.clone(),
                        source,
                    })?;
                if decision.is_rejected() {
                    stats.dropped_by_gate += 1;
                    continue;
                }
            }
        }

        pending.push((chunk_index, chunk, drawer_id));
    }

    if options.dry_run || pending.is_empty() {
        return Ok(stats);
    }

    let chunk_refs = pending
        .iter()
        .map(|(_, chunk, _)| chunk.as_ref())
        .collect::<Vec<_>>();
    let vectors = embedder
        .embed(&chunk_refs)
        .await
        .map_err(|source| IngestError::EmbedChunks {
            path: path.to_path_buf(),
            source,
        })?;
    if let Some(first_vector) = vectors.first() {
        let expected_dim = first_vector.len();
        if let Some(actual_dim) = vectors
            .iter()
            .map(Vec::len)
            .find(|dim| *dim != expected_dim)
        {
            return Err(IngestError::VectorDimensionMismatch {
                current_dim: expected_dim,
                new_dim: actual_dim,
            });
        }
        if let Some(current_dim) =
            current_vector_dim(db).map_err(|source| IngestError::InsertVector {
                drawer_id: pending
                    .first()
                    .map(|(_, _, drawer_id)| drawer_id.clone())
                    .unwrap_or_else(|| "unknown".to_string()),
                source,
            })?
            && current_dim != expected_dim
        {
            return Err(IngestError::VectorDimensionMismatch {
                current_dim,
                new_dim: expected_dim,
            });
        }
    }

    for ((chunk_index, chunk, drawer_id), vector) in pending.into_iter().zip(vectors.into_iter()) {
        let drawer = Drawer {
            id: drawer_id.clone(),
            content: chunk.to_string(),
            wing: wing.to_string(),
            room: Some(resolved_room.clone()),
            source_file: Some(source_file.clone()),
            source_type: source_type_for(format),
            added_at: iso_timestamp(),
            chunk_index: Some(chunk_index as i64),
            importance: 0,
        };

        db.insert_drawer_with_project(&drawer, options.project_id)
            .map_err(|source| IngestError::InsertDrawer {
                drawer_id: drawer.id.clone(),
                source,
            })?;
        db.insert_vector_with_project(&drawer_id, &vector, options.project_id)
            .map_err(|source| IngestError::InsertVector {
                drawer_id: drawer.id.clone(),
                source,
            })?;
        stats.chunks += 1;
    }

    Ok(stats)
}

pub async fn ingest_dir<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    dir: &Path,
    wing: &str,
    room: Option<&str>,
) -> Result<IngestStats> {
    ingest_dir_with_options(
        db,
        embedder,
        dir,
        wing,
        IngestOptions {
            room,
            source_root: Some(dir),
            dry_run: false,
            project_id: None,
            gating: None,
            prototype_classifier: None,
        },
    )
    .await
}

pub async fn ingest_dir_with_options<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    dir: &Path,
    wing: &str,
    options: IngestOptions<'_>,
) -> Result<IngestStats> {
    let mut stats = IngestStats::default();
    let mut stack = vec![dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).map_err(|source| IngestError::ReadDir {
            path: current.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| IngestError::ReadDirEntry {
                path: current.clone(),
                source,
            })?;
            let path = entry.path();

            if path.is_dir() {
                if should_skip_dir(&path) {
                    continue;
                }
                stack.push(path);
                continue;
            }

            if path.is_file() {
                let file_stats =
                    ingest_file_with_options(db, embedder, &path, wing, options).await?;
                stats.files += file_stats.files;
                stats.chunks += file_stats.chunks;
                stats.skipped += file_stats.skipped;
                stats.dropped_by_gate += file_stats.dropped_by_gate;
            }
        }
    }

    Ok(stats)
}

fn source_type_for(format: Format) -> SourceType {
    match format {
        Format::ClaudeJsonl | Format::ChatGptJson | Format::CodexJsonl | Format::SlackJson => {
            SourceType::Conversation
        }
        Format::PlainText => SourceType::Project,
    }
}

fn should_skip_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| matches!(name, ".git" | "target" | "node_modules"))
        .unwrap_or(false)
}

fn current_vector_dim(
    db: &Database,
) -> std::result::Result<Option<usize>, crate::core::db::DbError> {
    let exists: bool = db.conn().query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='drawer_vectors')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }

    let dimension = db
        .conn()
        .query_row(
            "SELECT vec_length(embedding) FROM drawer_vectors LIMIT 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .map(|value| value as usize);
    Ok(dimension)
}

fn normalize_source_file(path: &Path, source_root: Option<&Path>) -> String {
    let normalized = source_root
        .and_then(|root| path.strip_prefix(root).ok())
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| path.file_name().map(PathBuf::from))
        .unwrap_or_else(|| path.to_path_buf());

    normalized.to_string_lossy().replace('\\', "/")
}
