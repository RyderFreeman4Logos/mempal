use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::core::{db::Database, types::ReindexSource};
use crate::embed::Embedder;

use super::{
    IngestError, IngestOptions, ingest_file_with_options, normalize::CURRENT_NORMALIZE_VERSION,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReindexMode {
    Stale,
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReindexOptions {
    pub mode: ReindexMode,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReindexReport {
    pub candidate_drawers: u64,
    pub candidate_sources: u64,
    pub processed_sources: u64,
    pub reingested_files: usize,
    pub reingested_chunks: usize,
    pub skipped_existing_chunks: usize,
    pub skipped_missing_sources: u64,
    pub skipped_missing_drawers: u64,
}

#[derive(Debug, Error)]
pub enum ReindexError {
    #[error(transparent)]
    Db(#[from] crate::core::db::DbError),
    #[error(
        "project-scoped source reindex is unsupported for isolation safety: {candidate_drawers} candidate drawers across {candidate_sources} source identities have project_id; use `mempal reindex --from-config --stale` for project-safe embedder/vector reindex"
    )]
    ProjectScopedSourceReindexUnsupported {
        candidate_drawers: u64,
        candidate_sources: u64,
    },
    #[error("failed to reindex source {source_file}")]
    Ingest {
        source_file: String,
        #[source]
        source: IngestError,
    },
}

pub async fn reindex_sources<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    options: ReindexOptions,
) -> Result<ReindexReport, ReindexError> {
    let project_scoped = match options.mode {
        ReindexMode::Stale => db.project_scoped_reindex_sources_stale(CURRENT_NORMALIZE_VERSION)?,
        ReindexMode::Force => db.project_scoped_reindex_sources_force()?,
    };
    if project_scoped.drawer_count > 0 {
        return Err(ReindexError::ProjectScopedSourceReindexUnsupported {
            candidate_drawers: project_scoped.drawer_count,
            candidate_sources: project_scoped.source_count,
        });
    }

    let sources = match options.mode {
        ReindexMode::Stale => db.reindex_sources_stale(CURRENT_NORMALIZE_VERSION)?,
        ReindexMode::Force => db.reindex_sources_force()?,
    };

    let mut report = ReindexReport {
        candidate_drawers: sources.iter().map(|source| source.drawer_count).sum(),
        candidate_sources: sources.len() as u64,
        ..ReindexReport::default()
    };

    if options.dry_run {
        return Ok(report);
    }

    for source in sources {
        let Some(source_file) = source.source_file.as_deref() else {
            report.skipped_missing_sources += 1;
            report.skipped_missing_drawers += source.drawer_count;
            continue;
        };
        let source_path = match source.source_root.as_deref() {
            Some(source_root) => PathBuf::from(source_root).join(source_file),
            None => PathBuf::from(source_file),
        };
        if !source_path.is_file() {
            report.skipped_missing_sources += 1;
            report.skipped_missing_drawers += source.drawer_count;
            continue;
        }

        let stats = reindex_one_source(db, embedder, &source, source_file, source_path).await?;
        report.processed_sources += 1;
        report.reingested_files += stats.files;
        report.reingested_chunks += stats.chunks;
        report.skipped_existing_chunks += stats.skipped;
    }

    Ok(report)
}

async fn reindex_one_source<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    source: &ReindexSource,
    source_file: &str,
    source_path: PathBuf,
) -> Result<super::IngestStats, ReindexError> {
    ingest_file_with_options(
        db,
        embedder,
        &source_path,
        &source.wing,
        IngestOptions {
            room: source.room.as_deref(),
            project_id: source.project_id.as_deref(),
            source_root: source.source_root.as_deref().map(Path::new),
            dry_run: false,
            source_file_override: Some(source_file),
            replace_existing_source: true,
            no_strip_noise: false,
            ..IngestOptions::default()
        },
    )
    .await
    .map_err(|source| ReindexError::Ingest {
        source_file: source_file.to_string(),
        source,
    })
}
