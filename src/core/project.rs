use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::config::Config;

pub const PROJECT_MIGRATION_BATCH_SIZE: usize = 1_000;
pub const PROJECT_MIGRATION_RETRY_DELAYS_MS: [u64; 3] = [500, 1_000, 2_000];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectFilterMode {
    AllProjects,
    ProjectScoped,
    ProjectPlusGlobal,
    NullOnly,
}

impl ProjectFilterMode {
    pub fn as_sql_mode(self) -> &'static str {
        match self {
            Self::AllProjects => "all",
            Self::ProjectScoped => "project",
            Self::ProjectPlusGlobal => "project_plus_global",
            Self::NullOnly => "null_only",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchResultSource {
    Project,
    Global,
    TunnelCrossProject,
}

impl SearchResultSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
            Self::TunnelCrossProject => "tunnel_cross_project",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSearchScope {
    pub project_id: Option<String>,
    pub mode: ProjectFilterMode,
}

impl ProjectSearchScope {
    pub fn all_projects() -> Self {
        Self {
            project_id: None,
            mode: ProjectFilterMode::AllProjects,
        }
    }

    pub fn from_request(
        resolved_project_id: Option<String>,
        include_global: bool,
        all_projects: bool,
        strict_project_isolation: bool,
    ) -> Self {
        if all_projects {
            return Self::all_projects();
        }

        match resolved_project_id {
            Some(project_id) if include_global => Self {
                project_id: Some(project_id),
                mode: ProjectFilterMode::ProjectPlusGlobal,
            },
            Some(project_id) => Self {
                project_id: Some(project_id),
                mode: ProjectFilterMode::ProjectScoped,
            },
            None if strict_project_isolation => Self {
                project_id: None,
                mode: ProjectFilterMode::NullOnly,
            },
            None => Self::all_projects(),
        }
    }

    pub fn mode_param(&self) -> &'static str {
        self.mode.as_sql_mode()
    }

    pub fn allows_row(&self, row_project_id: Option<&str>) -> bool {
        match self.mode {
            ProjectFilterMode::AllProjects => true,
            ProjectFilterMode::ProjectScoped => row_project_id == self.project_id.as_deref(),
            ProjectFilterMode::ProjectPlusGlobal => {
                row_project_id.is_none() || row_project_id == self.project_id.as_deref()
            }
            ProjectFilterMode::NullOnly => row_project_id.is_none(),
        }
    }

    pub fn classify_row(&self, row_project_id: Option<&str>) -> SearchResultSource {
        match row_project_id {
            Some(_) => SearchResultSource::Project,
            None => SearchResultSource::Global,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMigrationProgress {
    pub batch_index: usize,
    pub updated: usize,
    pub remaining: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectMigrationEvent {
    Busy { delay_ms: u64 },
    Progress(ProjectMigrationProgress),
}

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("project id cannot be empty")]
    Empty,
    #[error("project id cannot start or end with whitespace")]
    SurroundingWhitespace,
    #[error("project id cannot contain '/'")]
    Slash,
    #[error("project id cannot contain NUL")]
    Nul,
    #[error("failed to read current directory")]
    CurrentDir(#[source] std::io::Error),
    #[error("failed to run git to infer project root")]
    Git(#[source] std::io::Error),
    #[error("invalid UTF-8 in project path")]
    InvalidUtf8Path,
    #[error("project path has no basename: {0}")]
    MissingBasename(PathBuf),
    #[error("database busy while assigning project ids")]
    DatabaseBusy,
    #[error("failed to run project migration query")]
    Sqlite(#[from] rusqlite::Error),
}

pub fn resolve_project_id(
    explicit: Option<&str>,
    config: &Config,
    cwd: Option<&Path>,
) -> Result<Option<String>, ProjectError> {
    if let Some(explicit) = explicit {
        return Ok(Some(validate_project_id(explicit)?));
    }
    if let Some(configured) = config.project.id.as_deref() {
        return Ok(Some(validate_project_id(configured)?));
    }
    if let Some(from_env) = env::var("MEMPAL_PROJECT_ID").ok().as_deref() {
        return Ok(Some(validate_project_id(from_env)?));
    }
    if let Some(cwd) = cwd {
        return infer_project_id_from_path(cwd);
    }
    Ok(None)
}

pub fn infer_project_id_from_path(path: &Path) -> Result<Option<String>, ProjectError> {
    let candidate_root = git_repo_root(path)?.unwrap_or_else(|| path.to_path_buf());
    match candidate_root.file_name().and_then(|name| name.to_str()) {
        Some(name) => Ok(Some(validate_project_id(name)?)),
        None if candidate_root.as_os_str().is_empty() => Ok(None),
        None => Err(ProjectError::MissingBasename(candidate_root)),
    }
}

fn git_repo_root(path: &Path) -> Result<Option<PathBuf>, ProjectError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .map_err(ProjectError::Git)?;
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| ProjectError::InvalidUtf8Path)?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(trimmed)))
}

pub fn validate_project_id(raw: &str) -> Result<String, ProjectError> {
    if raw.is_empty() {
        return Err(ProjectError::Empty);
    }
    if raw.trim() != raw {
        return Err(ProjectError::SurroundingWhitespace);
    }
    if raw.contains('/') {
        return Err(ProjectError::Slash);
    }
    if raw.contains('\0') {
        return Err(ProjectError::Nul);
    }
    Ok(raw.to_string())
}

pub fn migrate_null_project_ids<F>(
    db_path: &Path,
    project_id: &str,
    wing: Option<&str>,
    mut on_event: F,
) -> Result<(), ProjectError>
where
    F: FnMut(ProjectMigrationEvent),
{
    let project_id = validate_project_id(project_id)?;
    let wing = wing.map(ToOwned::to_owned);
    let mut batch_index = 0usize;

    loop {
        let conn = Connection::open(db_path)?;
        conn.busy_timeout(Duration::from_millis(0))?;
        match conn.execute_batch("BEGIN IMMEDIATE") {
            Ok(()) => {}
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DatabaseBusy =>
            {
                for delay_ms in PROJECT_MIGRATION_RETRY_DELAYS_MS {
                    on_event(ProjectMigrationEvent::Busy { delay_ms });
                    thread::sleep(Duration::from_millis(delay_ms));
                }
                continue;
            }
            Err(error) => return Err(ProjectError::Sqlite(error)),
        }

        let result = (|| {
            let ids = collect_batch_ids(&conn, wing.as_deref())?;
            if ids.is_empty() {
                conn.execute_batch("COMMIT")?;
                return Ok(None);
            }

            update_project_ids(&conn, "drawers", &ids, &project_id)?;
            update_project_ids(&conn, "drawer_vectors", &ids, &project_id)?;
            let remaining = count_remaining(&conn, wing.as_deref())?;
            conn.execute_batch("COMMIT")?;
            Ok(Some((ids.len(), remaining)))
        })();

        match result {
            Ok(Some((updated, remaining))) => {
                batch_index += 1;
                on_event(ProjectMigrationEvent::Progress(ProjectMigrationProgress {
                    batch_index,
                    updated,
                    remaining,
                }));
                thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => return Ok(()),
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(error);
            }
        }
    }
}

fn collect_batch_ids(conn: &Connection, wing: Option<&str>) -> Result<Vec<String>, ProjectError> {
    let mut stmt = conn.prepare(
        r#"
        SELECT id
        FROM drawers
        WHERE project_id IS NULL
          AND (?1 IS NULL OR wing = ?1)
        ORDER BY id
        LIMIT ?2
        "#,
    )?;
    let rows = stmt
        .query_map(
            params![
                wing,
                i64::try_from(PROJECT_MIGRATION_BATCH_SIZE).unwrap_or(1_000)
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn update_project_ids(
    conn: &Connection,
    table: &str,
    ids: &[String],
    project_id: &str,
) -> Result<(), ProjectError> {
    if ids.is_empty() {
        return Ok(());
    }

    let placeholders = (0..ids.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!("UPDATE {table} SET project_id = ?1 WHERE id IN ({placeholders})");
    let mut params = Vec::<&dyn rusqlite::ToSql>::with_capacity(ids.len() + 1);
    params.push(&project_id);
    for id in ids {
        params.push(id);
    }
    conn.execute(&sql, params.as_slice())?;
    Ok(())
}

fn count_remaining(conn: &Connection, wing: Option<&str>) -> Result<usize, ProjectError> {
    let remaining = conn.query_row(
        r#"
        SELECT COUNT(*)
        FROM drawers
        WHERE project_id IS NULL
          AND (?1 IS NULL OR wing = ?1)
        "#,
        [wing],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(remaining as usize)
}
