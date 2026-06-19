use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use regex::Regex;
use rusqlite::{params_from_iter, types::Value as SqlValue};
use serde::{Deserialize, Serialize};

use crate::core::config::scrub_sensitive_text;
use crate::core::db::Database;
use crate::core::project::{ProjectFilterMode, ProjectSearchScope};

const FORMAT_VERSION: &str = "markdown_mirror_v1";
const CANONICAL_SOURCE: &str = "sqlite";
const MIRROR_SEMANTICS: &str = "generated_read_only";
const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024;
const MANIFEST_FILE: &str = ".mempal-markdown-mirror.toml";
const README_FILE: &str = "README.md";
const INDEX_FILE: &str = "index.md";

#[derive(Debug, Clone)]
pub struct MarkdownExportOptions {
    pub output_dir: PathBuf,
    pub scope: ProjectSearchScope,
    pub wing: Option<String>,
    pub room: Option<String>,
    pub max_body_bytes: usize,
    pub redact: bool,
}

impl MarkdownExportOptions {
    pub fn new(output_dir: PathBuf, scope: ProjectSearchScope) -> Self {
        Self {
            output_dir,
            scope,
            wing: None,
            room: None,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            redact: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownExportReport {
    pub output_dir: PathBuf,
    pub exported: usize,
    pub redacted: bool,
    pub canonical_source: &'static str,
    pub mirror_semantics: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
struct ExportDrawer {
    id: String,
    content: String,
    wing: String,
    room: Option<String>,
    source_file: Option<String>,
    source_root: Option<String>,
    source_type: Option<String>,
    confidence: Option<f64>,
    added_at: String,
    chunk_index: Option<i64>,
    normalize_version: Option<i64>,
    importance: Option<i64>,
    project_id: Option<String>,
    content_hash: Option<String>,
    memory_kind: Option<String>,
    domain: Option<String>,
    field: Option<String>,
    anchor_kind: Option<String>,
    anchor_id: Option<String>,
    parent_anchor_id: Option<String>,
    provenance: Option<String>,
    statement: Option<String>,
    tier: Option<String>,
    status: Option<String>,
    supporting_refs: Vec<String>,
    counterexample_refs: Vec<String>,
    teaching_refs: Vec<String>,
    verification_refs: Vec<String>,
    scope_constraints: Option<String>,
    trigger_hints_json: Option<String>,
    is_pinned: bool,
    pin_order: Option<i64>,
    supersedes: Option<String>,
    valid_from: Option<String>,
    valid_until: Option<String>,
    updated_at: Option<String>,
    merge_count: Option<i64>,
    effective_importance: Option<f64>,
    compacted_into: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExportBody {
    content: String,
    original_bytes: usize,
    exported_bytes: usize,
    truncated: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct MarkdownExportManifest {
    mempal_format: String,
    canonical_source: String,
    mirror_semantics: String,
    generated_files: Vec<String>,
}

pub fn default_max_body_bytes() -> usize {
    DEFAULT_MAX_BODY_BYTES
}

pub fn export_markdown(
    db: &Database,
    options: &MarkdownExportOptions,
) -> Result<MarkdownExportReport> {
    prepare_output_dir(&options.output_dir)?;
    let previous_manifest = load_managed_manifest(&options.output_dir)?;
    let previous_paths = manifest_generated_paths(&previous_manifest)?;

    let drawers = load_export_drawers(db, options)?;
    let mut index_entries = Vec::new();
    let mut drawer_outputs = Vec::new();
    let mut generated_paths = BTreeSet::from([README_FILE.to_string(), INDEX_FILE.to_string()]);
    for drawer in &drawers {
        let relative_path = drawer_relative_path(drawer);
        let manifest_path = manifest_relative_path(&relative_path);
        generated_paths.insert(manifest_path.clone());
        index_entries.push((drawer.id.clone(), relative_path));
        drawer_outputs.push((drawer, manifest_path));
    }
    verify_no_unmanaged_collisions(&options.output_dir, &generated_paths, &previous_paths)?;

    for (drawer, manifest_path) in &drawer_outputs {
        let relative_path = validate_manifest_relative_path(manifest_path)?;
        let markdown = render_drawer_markdown(drawer, options)?;
        write_generated_file(
            &options.output_dir,
            &relative_path,
            manifest_path,
            markdown.as_bytes(),
            &previous_paths,
        )?;
    }

    write_generated_file(
        &options.output_dir,
        Path::new(README_FILE),
        README_FILE,
        render_export_readme(options, drawers.len()).as_bytes(),
        &previous_paths,
    )?;
    write_generated_file(
        &options.output_dir,
        Path::new(INDEX_FILE),
        INDEX_FILE,
        render_index(&index_entries, options).as_bytes(),
        &previous_paths,
    )?;
    remove_stale_generated_files(&options.output_dir, &previous_paths, &generated_paths)?;
    write_manifest(
        &options.output_dir,
        &generated_paths,
        previous_manifest_exists(&options.output_dir)?,
    )?;

    Ok(MarkdownExportReport {
        output_dir: options.output_dir.clone(),
        exported: drawers.len(),
        redacted: options.redact,
        canonical_source: CANONICAL_SOURCE,
        mirror_semantics: MIRROR_SEMANTICS,
    })
}

fn prepare_output_dir(output_dir: &Path) -> Result<()> {
    fs::create_dir_all(output_dir).with_context(|| {
        format!(
            "failed to create markdown export directory {}",
            output_dir.display()
        )
    })?;
    ensure_output_dir(output_dir)?;
    if previous_manifest_exists(output_dir)? || output_dir_is_empty(output_dir)? {
        return Ok(());
    }
    bail!(
        "refusing to export into non-empty unmanaged markdown mirror directory {}; choose an empty directory or an existing mempal Markdown mirror",
        output_dir.display()
    );
}

fn ensure_output_dir(output_dir: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(output_dir).with_context(|| {
        format!(
            "failed to inspect markdown export directory {}",
            output_dir.display()
        )
    })?;
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to export into symlinked markdown mirror directory {}",
            output_dir.display()
        );
    }
    if !metadata.file_type().is_dir() {
        bail!(
            "markdown export path is not a directory {}",
            output_dir.display()
        );
    }
    Ok(())
}

fn output_dir_is_empty(output_dir: &Path) -> Result<bool> {
    let mut entries = fs::read_dir(output_dir).with_context(|| {
        format!(
            "failed to read markdown export directory {}",
            output_dir.display()
        )
    })?;
    Ok(entries.next().is_none())
}

fn previous_manifest_exists(output_dir: &Path) -> Result<bool> {
    let manifest_path = output_dir.join(MANIFEST_FILE);
    match fs::symlink_metadata(&manifest_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!(
                    "refusing to use symlinked markdown export manifest {}",
                    manifest_path.display()
                );
            }
            if !metadata.file_type().is_file() {
                bail!(
                    "markdown export manifest is not a regular file {}",
                    manifest_path.display()
                );
            }
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to inspect markdown export manifest {}",
                manifest_path.display()
            )
        }),
    }
}

fn load_managed_manifest(output_dir: &Path) -> Result<MarkdownExportManifest> {
    let manifest_path = output_dir.join(MANIFEST_FILE);
    if !previous_manifest_exists(output_dir)? {
        return Ok(MarkdownExportManifest::empty());
    }
    let raw = fs::read_to_string(&manifest_path).with_context(|| {
        format!(
            "failed to read markdown export manifest {}",
            manifest_path.display()
        )
    })?;
    let manifest: MarkdownExportManifest = toml::from_str(&raw).with_context(|| {
        format!(
            "failed to parse markdown export manifest {}",
            manifest_path.display()
        )
    })?;
    if manifest.mempal_format != FORMAT_VERSION
        || manifest.canonical_source != CANONICAL_SOURCE
        || manifest.mirror_semantics != MIRROR_SEMANTICS
    {
        bail!(
            "refusing to update incompatible markdown export manifest {}",
            manifest_path.display()
        );
    }
    Ok(manifest)
}

impl MarkdownExportManifest {
    fn empty() -> Self {
        Self {
            mempal_format: FORMAT_VERSION.to_string(),
            canonical_source: CANONICAL_SOURCE.to_string(),
            mirror_semantics: MIRROR_SEMANTICS.to_string(),
            generated_files: Vec::new(),
        }
    }

    fn from_paths(paths: &BTreeSet<String>) -> Self {
        Self {
            generated_files: paths.iter().cloned().collect(),
            ..Self::empty()
        }
    }
}

fn manifest_generated_paths(manifest: &MarkdownExportManifest) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    for raw in &manifest.generated_files {
        let normalized = manifest_relative_path(&validate_manifest_relative_path(raw)?);
        if normalized != *raw {
            bail!("markdown export manifest path is not normalized: {raw}");
        }
        paths.insert(raw.clone());
    }
    Ok(paths)
}

fn validate_manifest_relative_path(raw: &str) -> Result<PathBuf> {
    let raw_path = Path::new(raw);
    if raw_path.is_absolute() {
        bail!("markdown export manifest path must be relative: {raw}");
    }
    let mut relative = PathBuf::new();
    for component in raw_path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            _ => bail!("unsafe markdown export manifest path: {raw}"),
        }
    }
    if relative.as_os_str().is_empty() {
        bail!("empty markdown export manifest path");
    }
    Ok(relative)
}

fn ensure_existing_parent_dirs_are_safe(output_dir: &Path, relative_path: &Path) -> Result<()> {
    let Some(parent) = relative_path.parent() else {
        return Ok(());
    };
    let mut current = output_dir.to_path_buf();
    for component in parent.components() {
        let Component::Normal(part) = component else {
            bail!(
                "unsafe markdown export path parent: {}",
                relative_path.display()
            );
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!(
                        "refusing to use symlinked markdown export directory {}",
                        current.display()
                    );
                }
                if !metadata.file_type().is_dir() {
                    bail!(
                        "refusing to use non-directory markdown export parent {}",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect markdown export directory {}",
                        current.display()
                    )
                });
            }
        }
    }
    Ok(())
}

fn create_parent_dirs_safely(output_dir: &Path, relative_path: &Path) -> Result<PathBuf> {
    let Some(parent) = relative_path.parent() else {
        return Ok(output_dir.join(relative_path));
    };
    let mut current = output_dir.to_path_buf();
    for component in parent.components() {
        let Component::Normal(part) = component else {
            bail!(
                "unsafe markdown export path parent: {}",
                relative_path.display()
            );
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!(
                        "refusing to use symlinked markdown export directory {}",
                        current.display()
                    );
                }
                if !metadata.file_type().is_dir() {
                    bail!(
                        "refusing to use non-directory markdown export parent {}",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                fs::create_dir(&current).with_context(|| {
                    format!(
                        "failed to create markdown export directory {}",
                        current.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect markdown export directory {}",
                        current.display()
                    )
                });
            }
        }
    }
    Ok(output_dir.join(relative_path))
}

fn manifest_relative_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn verify_no_unmanaged_collisions(
    output_dir: &Path,
    desired_paths: &BTreeSet<String>,
    previous_paths: &BTreeSet<String>,
) -> Result<()> {
    for raw in desired_paths {
        if previous_paths.contains(raw) {
            continue;
        }
        let relative_path = validate_manifest_relative_path(raw)?;
        ensure_existing_parent_dirs_are_safe(output_dir, &relative_path)?;
        let path = output_dir.join(relative_path);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                bail!(
                    "refusing to overwrite unmanaged markdown export file {}; choose an empty directory or remove the colliding file",
                    path.display()
                );
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect markdown export path {}", path.display())
                });
            }
        }
    }
    Ok(())
}

fn write_generated_file(
    output_dir: &Path,
    relative_path: &Path,
    manifest_path: &str,
    content: &[u8],
    previous_paths: &BTreeSet<String>,
) -> Result<()> {
    let path = create_parent_dirs_safely(output_dir, relative_path)?;
    if previous_paths.contains(manifest_path) {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                fs::remove_file(&path).with_context(|| {
                    format!(
                        "failed to remove symlinked markdown export {}",
                        path.display()
                    )
                })?;
                return create_generated_file(&path, content);
            }
            Ok(metadata) if metadata.file_type().is_dir() => {
                bail!(
                    "refusing to overwrite markdown export directory {}",
                    path.display()
                );
            }
            Ok(_) => {
                fs::write(&path, content).with_context(|| {
                    format!("failed to write markdown export {}", path.display())
                })?;
                return Ok(());
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return create_generated_file(&path, content);
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect markdown export path {}", path.display())
                });
            }
        }
    }
    create_generated_file(&path, content)
}

fn create_generated_file(path: &Path, content: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| {
            format!(
                "failed to create markdown export {}; refusing to overwrite unmanaged files",
                path.display()
            )
        })?;
    file.write_all(content)
        .with_context(|| format!("failed to write markdown export {}", path.display()))
}

fn remove_stale_generated_files(
    output_dir: &Path,
    previous_paths: &BTreeSet<String>,
    desired_paths: &BTreeSet<String>,
) -> Result<()> {
    for raw in previous_paths.difference(desired_paths) {
        let relative_path = validate_manifest_relative_path(raw)?;
        ensure_existing_parent_dirs_are_safe(output_dir, &relative_path)?;
        let path = output_dir.join(relative_path);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
                fs::remove_file(&path).with_context(|| {
                    format!("failed to remove stale markdown export {}", path.display())
                })?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect stale markdown export {}", path.display())
                });
            }
        }
    }
    Ok(())
}

fn write_manifest(
    output_dir: &Path,
    generated_paths: &BTreeSet<String>,
    had_manifest: bool,
) -> Result<()> {
    let manifest_path = output_dir.join(MANIFEST_FILE);
    let manifest = MarkdownExportManifest::from_paths(generated_paths);
    let content = toml::to_string_pretty(&manifest)
        .context("failed to serialize markdown export manifest")?;
    if had_manifest {
        match fs::symlink_metadata(&manifest_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "refusing to overwrite symlinked markdown export manifest {}",
                    manifest_path.display()
                );
            }
            Ok(metadata) if metadata.file_type().is_dir() => {
                bail!(
                    "refusing to overwrite markdown export manifest directory {}",
                    manifest_path.display()
                );
            }
            Ok(_) => {
                fs::write(&manifest_path, content).with_context(|| {
                    format!(
                        "failed to write markdown export manifest {}",
                        manifest_path.display()
                    )
                })?;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                create_generated_file(&manifest_path, content.as_bytes())?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect markdown export manifest {}",
                        manifest_path.display()
                    )
                });
            }
        }
    } else {
        create_generated_file(&manifest_path, content.as_bytes())?;
    }
    Ok(())
}

fn load_export_drawers(
    db: &Database,
    options: &MarkdownExportOptions,
) -> Result<Vec<ExportDrawer>> {
    let mut sql = String::from(
        r#"
        SELECT
            id,
            content,
            wing,
            room,
            source_file,
            source_root,
            source_type,
            confidence,
            added_at,
            chunk_index,
            normalize_version,
            importance,
            project_id,
            content_hash,
            memory_kind,
            domain,
            field,
            anchor_kind,
            anchor_id,
            parent_anchor_id,
            provenance,
            statement,
            tier,
            status,
            supporting_refs,
            counterexample_refs,
            teaching_refs,
            verification_refs,
            scope_constraints,
            trigger_hints,
            is_pinned,
            pin_order,
            supersedes,
            valid_from,
            valid_until,
            updated_at,
            COALESCE(merge_count, 0) AS merge_count,
            effective_importance,
            compacted_into
        FROM drawers
        WHERE deleted_at IS NULL
        "#,
    );
    let mut values = Vec::new();
    append_scope_filter(&mut sql, &mut values, &options.scope);
    if let Some(wing) = options.wing.as_deref() {
        values.push(SqlValue::Text(wing.to_string()));
        sql.push_str(&format!(" AND wing = ?{} ", values.len()));
    }
    if let Some(room) = options.room.as_deref() {
        values.push(SqlValue::Text(room.to_string()));
        sql.push_str(&format!(" AND room = ?{} ", values.len()));
    }
    sql.push_str(
        " ORDER BY wing ASC, COALESCE(room, '') ASC, COALESCE(chunk_index, 0) ASC, id ASC",
    );

    let mut statement = db
        .conn()
        .prepare(&sql)
        .context("failed to prepare markdown export drawer query")?;
    let rows = statement
        .query_map(params_from_iter(values.iter()), |row| {
            Ok(ExportDrawer {
                id: row.get(0)?,
                content: row.get(1)?,
                wing: row.get(2)?,
                room: row.get(3)?,
                source_file: row.get(4)?,
                source_root: row.get(5)?,
                source_type: row.get(6)?,
                confidence: row.get(7)?,
                added_at: row.get(8)?,
                chunk_index: row.get(9)?,
                normalize_version: row.get(10)?,
                importance: row.get(11)?,
                project_id: row.get(12)?,
                content_hash: row.get(13)?,
                memory_kind: row.get(14)?,
                domain: row.get(15)?,
                field: row.get(16)?,
                anchor_kind: row.get(17)?,
                anchor_id: row.get(18)?,
                parent_anchor_id: row.get(19)?,
                provenance: row.get(20)?,
                statement: row.get(21)?,
                tier: row.get(22)?,
                status: row.get(23)?,
                supporting_refs: decode_refs(row.get::<_, Option<String>>(24)?)?,
                counterexample_refs: decode_refs(row.get::<_, Option<String>>(25)?)?,
                teaching_refs: decode_refs(row.get::<_, Option<String>>(26)?)?,
                verification_refs: decode_refs(row.get::<_, Option<String>>(27)?)?,
                scope_constraints: row.get(28)?,
                trigger_hints_json: row.get(29)?,
                is_pinned: row.get(30)?,
                pin_order: row.get(31)?,
                supersedes: row.get(32)?,
                valid_from: row.get(33)?,
                valid_until: row.get(34)?,
                updated_at: row.get(35)?,
                merge_count: row.get(36)?,
                effective_importance: row.get(37)?,
                compacted_into: row.get(38)?,
            })
        })
        .context("failed to query markdown export drawers")?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to decode markdown export drawers")
}

fn append_scope_filter(sql: &mut String, values: &mut Vec<SqlValue>, scope: &ProjectSearchScope) {
    match scope.mode {
        ProjectFilterMode::AllProjects => {}
        ProjectFilterMode::ProjectScoped => {
            values.push(SqlValue::Text(scope.project_id.clone().unwrap_or_default()));
            sql.push_str(&format!(" AND project_id = ?{} ", values.len()));
        }
        ProjectFilterMode::ProjectPlusGlobal => {
            values.push(SqlValue::Text(scope.project_id.clone().unwrap_or_default()));
            sql.push_str(&format!(
                " AND (project_id = ?{} OR project_id IS NULL) ",
                values.len()
            ));
        }
        ProjectFilterMode::NullOnly => {
            sql.push_str(" AND project_id IS NULL ");
        }
    }
}

fn decode_refs(raw: Option<String>) -> rusqlite::Result<Vec<String>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    serde_json::from_str::<Vec<String>>(&raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            raw.len(),
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn render_drawer_markdown(
    drawer: &ExportDrawer,
    options: &MarkdownExportOptions,
) -> Result<String> {
    let body = export_body(&drawer.content, options.max_body_bytes, options.redact);
    let mut output = String::new();
    output.push_str("---\n");
    push_yaml_str(&mut output, "mempal_format", FORMAT_VERSION);
    push_yaml_str(&mut output, "canonical_source", CANONICAL_SOURCE);
    push_yaml_str(&mut output, "mirror_semantics", MIRROR_SEMANTICS);
    push_yaml_str(&mut output, "drawer_id", &drawer.id);
    push_yaml_opt(
        &mut output,
        "project_id",
        drawer.project_id.as_deref(),
        options.redact,
    );
    push_yaml_str_redacted(&mut output, "wing", &drawer.wing, options.redact);
    push_yaml_opt(&mut output, "room", drawer.room.as_deref(), options.redact);
    push_yaml_opt(
        &mut output,
        "memory_kind",
        drawer.memory_kind.as_deref(),
        options.redact,
    );
    push_yaml_opt(
        &mut output,
        "domain",
        drawer.domain.as_deref(),
        options.redact,
    );
    push_yaml_opt(
        &mut output,
        "field",
        drawer.field.as_deref(),
        options.redact,
    );
    push_yaml_opt(
        &mut output,
        "source_type",
        drawer.source_type.as_deref(),
        options.redact,
    );
    push_yaml_f64(&mut output, "confidence", drawer.confidence);
    push_yaml_i64(&mut output, "importance", drawer.importance);
    push_yaml_f64(
        &mut output,
        "effective_importance",
        drawer.effective_importance,
    );
    push_yaml_opt(
        &mut output,
        "source_file",
        drawer.source_file.as_deref(),
        options.redact,
    );
    push_yaml_opt(
        &mut output,
        "source_root",
        drawer.source_root.as_deref(),
        options.redact,
    );
    push_yaml_opt(
        &mut output,
        "content_hash",
        drawer.content_hash.as_deref(),
        options.redact,
    );
    push_yaml_str_redacted(&mut output, "added_at", &drawer.added_at, options.redact);
    push_yaml_opt(
        &mut output,
        "updated_at",
        drawer.updated_at.as_deref(),
        options.redact,
    );
    push_yaml_opt(
        &mut output,
        "valid_from",
        drawer.valid_from.as_deref(),
        options.redact,
    );
    push_yaml_opt(
        &mut output,
        "valid_until",
        drawer.valid_until.as_deref(),
        options.redact,
    );
    push_yaml_i64(&mut output, "chunk_index", drawer.chunk_index);
    push_yaml_i64(&mut output, "normalize_version", drawer.normalize_version);
    push_yaml_opt(
        &mut output,
        "anchor_kind",
        drawer.anchor_kind.as_deref(),
        options.redact,
    );
    push_yaml_opt(
        &mut output,
        "anchor_id",
        drawer.anchor_id.as_deref(),
        options.redact,
    );
    push_yaml_opt(
        &mut output,
        "parent_anchor_id",
        drawer.parent_anchor_id.as_deref(),
        options.redact,
    );
    push_yaml_opt(
        &mut output,
        "provenance",
        drawer.provenance.as_deref(),
        options.redact,
    );
    push_yaml_opt(
        &mut output,
        "statement",
        drawer.statement.as_deref(),
        options.redact,
    );
    push_yaml_opt(&mut output, "tier", drawer.tier.as_deref(), options.redact);
    push_yaml_opt(
        &mut output,
        "status",
        drawer.status.as_deref(),
        options.redact,
    );
    push_yaml_bool(&mut output, "is_pinned", drawer.is_pinned);
    push_yaml_i64(&mut output, "pin_order", drawer.pin_order);
    push_yaml_opt(
        &mut output,
        "supersedes",
        drawer.supersedes.as_deref(),
        options.redact,
    );
    push_yaml_i64(&mut output, "merge_count", drawer.merge_count);
    push_yaml_opt(
        &mut output,
        "compacted_into",
        drawer.compacted_into.as_deref(),
        options.redact,
    );
    push_yaml_refs(
        &mut output,
        "supporting_refs",
        &drawer.supporting_refs,
        options.redact,
    );
    push_yaml_refs(
        &mut output,
        "counterexample_refs",
        &drawer.counterexample_refs,
        options.redact,
    );
    push_yaml_refs(
        &mut output,
        "teaching_refs",
        &drawer.teaching_refs,
        options.redact,
    );
    push_yaml_refs(
        &mut output,
        "verification_refs",
        &drawer.verification_refs,
        options.redact,
    );
    push_yaml_opt(
        &mut output,
        "scope_constraints",
        drawer.scope_constraints.as_deref(),
        options.redact,
    );
    push_yaml_opt(
        &mut output,
        "trigger_hints_json",
        drawer.trigger_hints_json.as_deref(),
        options.redact,
    );
    push_yaml_str(
        &mut output,
        "redaction",
        if options.redact {
            "default"
        } else {
            "disabled"
        },
    );
    push_yaml_bool(&mut output, "body_truncated", body.truncated);
    push_yaml_usize(&mut output, "body_original_bytes", body.original_bytes);
    push_yaml_usize(&mut output, "body_exported_bytes", body.exported_bytes);
    output.push_str("---\n\n");

    let title = drawer
        .statement
        .as_deref()
        .filter(|statement| !statement.trim().is_empty())
        .unwrap_or(drawer.id.as_str());
    output.push_str("# ");
    output.push_str(&markdown_inline(
        redact_if_needed(title, options.redact).as_ref(),
    ));
    output.push_str("\n\n");
    output.push_str("> Generated from canonical SQLite memory. Markdown is a review mirror, not the source of truth.\n\n");
    output.push_str("## Content\n\n");
    output.push_str(&body.content);
    if !body.content.ends_with('\n') {
        output.push('\n');
    }
    if body.truncated {
        output.push_str("\n[TRUNCATED: body exceeded max_body_bytes]\n");
    }

    Ok(output)
}

fn render_export_readme(options: &MarkdownExportOptions, count: usize) -> String {
    format!(
        "# mempal Markdown Mirror\n\n\
         This directory was generated by `mempal export md` from the canonical SQLite store.\n\n\
         - `canonical_source: sqlite` means SQLite wins by default.\n\
         - `mirror_semantics: generated_read_only` means these files are for review, diffing, and Git-friendly export.\n\
         - `.mempal-markdown-mirror.toml` records generated paths. Re-runs only overwrite manifest-owned files and remove stale generated files.\n\
         - Markdown import/watch sync is not active here. Future import/watch behavior must be explicit opt-in and conflict-safe.\n\
         - Redaction is `{}` for this export; raw SQLite drawer content is unchanged.\n\
         - Exported drawers: {count}.\n\n\
         Re-run `mempal export md <dir>` to refresh this mirror from SQLite.\n",
        if options.redact {
            "enabled"
        } else {
            "disabled"
        }
    )
}

fn render_index(entries: &[(String, PathBuf)], options: &MarkdownExportOptions) -> String {
    let mut output = String::new();
    output.push_str("# mempal Markdown Export Index\n\n");
    output.push_str("SQLite is canonical. Markdown files are generated review artifacts.\n\n");
    output.push_str(&format!(
        "- canonical_source: `{CANONICAL_SOURCE}`\n- mirror_semantics: `{MIRROR_SEMANTICS}`\n- redaction: `{}`\n\n",
        if options.redact { "default" } else { "disabled" }
    ));
    for (drawer_id, path) in entries {
        output.push_str("- [");
        output.push_str(&markdown_inline(drawer_id));
        output.push_str("](");
        output.push_str(&path.to_string_lossy().replace(' ', "%20"));
        output.push_str(")\n");
    }
    output
}

fn drawer_relative_path(drawer: &ExportDrawer) -> PathBuf {
    let mut path = PathBuf::new();
    path.push(format!("wing-{}", path_component(&drawer.wing)));
    path.push(format!(
        "room-{}",
        path_component(drawer.room.as_deref().unwrap_or("default"))
    ));
    path.push(format!("{}.md", path_component_with_hash(&drawer.id)));
    path
}

fn path_component(value: &str) -> String {
    let mut component = value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => ch,
            _ => '_',
        })
        .collect::<String>();
    while component.contains("__") {
        component = component.replace("__", "_");
    }
    let trimmed = component.trim_matches(['_', '.']).to_string();
    if trimmed.is_empty() {
        "none".to_string()
    } else {
        trimmed
    }
}

fn path_component_with_hash(value: &str) -> String {
    let slug = path_component(value);
    let digest = blake3::hash(value.as_bytes()).to_hex();
    format!("{slug}-{}", &digest[..8])
}

fn export_body(content: &str, max_bytes: usize, redact: bool) -> ExportBody {
    let redacted = redact_if_needed(content, redact);
    let original_bytes = redacted.len();
    let truncated_content = truncate_utf8(&redacted, max_bytes);
    let truncated = truncated_content.len() < original_bytes;
    ExportBody {
        exported_bytes: truncated_content.len(),
        content: truncated_content,
        original_bytes,
        truncated,
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn redact_if_needed(value: &str, redact: bool) -> String {
    if !redact {
        return value.to_string();
    }
    let mut redacted = scrub_sensitive_text(value);
    for regex in export_redaction_patterns() {
        redacted = regex
            .replace_all(&redacted, "[REDACTED:secret_like]")
            .into_owned();
    }
    redacted
}

fn export_redaction_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r#"(?i)\b(api[_-]?key|access[_-]?token|auth[_-]?token|password|secret)\s*[:=]\s*["']?[^\s"']{8,}"#,
            r#"(?i)\bAuthorization:\s*Basic\s+[A-Za-z0-9+/=]{12,}"#,
        ]
        .into_iter()
        .filter_map(|pattern| Regex::new(pattern).ok())
        .collect()
    })
}

fn push_yaml_str(output: &mut String, key: &str, value: &str) {
    output.push_str(key);
    output.push_str(": \"");
    output.push_str(&yaml_escape(value));
    output.push_str("\"\n");
}

fn push_yaml_str_redacted(output: &mut String, key: &str, value: &str, redact: bool) {
    push_yaml_str(output, key, redact_if_needed(value, redact).as_ref());
}

fn push_yaml_opt(output: &mut String, key: &str, value: Option<&str>, redact: bool) {
    match value {
        Some(value) => push_yaml_str_redacted(output, key, value, redact),
        None => {
            output.push_str(key);
            output.push_str(": null\n");
        }
    }
}

fn push_yaml_refs(output: &mut String, key: &str, values: &[String], redact: bool) {
    output.push_str(key);
    if values.is_empty() {
        output.push_str(": []\n");
        return;
    }
    output.push_str(":\n");
    for value in values {
        output.push_str("  - \"");
        output.push_str(&yaml_escape(&redact_if_needed(value, redact)));
        output.push_str("\"\n");
    }
}

fn push_yaml_bool(output: &mut String, key: &str, value: bool) {
    output.push_str(key);
    output.push_str(": ");
    output.push_str(if value { "true" } else { "false" });
    output.push('\n');
}

fn push_yaml_i64(output: &mut String, key: &str, value: Option<i64>) {
    output.push_str(key);
    output.push_str(": ");
    match value {
        Some(value) => output.push_str(&value.to_string()),
        None => output.push_str("null"),
    }
    output.push('\n');
}

fn push_yaml_usize(output: &mut String, key: &str, value: usize) {
    output.push_str(key);
    output.push_str(": ");
    output.push_str(&value.to_string());
    output.push('\n');
}

fn push_yaml_f64(output: &mut String, key: &str, value: Option<f64>) {
    output.push_str(key);
    output.push_str(": ");
    match value {
        Some(value) => output.push_str(&format!("{value:.6}")),
        None => output.push_str("null"),
    }
    output.push('\n');
}

fn yaml_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| match ch {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect::<Vec<_>>(),
            '\t' => "\\t".chars().collect::<Vec<_>>(),
            other => vec![other],
        })
        .collect()
}

fn markdown_inline(value: &str) -> String {
    value.replace('[', "\\[").replace(']', "\\]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{BootstrapEvidenceArgs, Drawer, SourceType};

    fn insert_drawer(db: &Database, drawer: Drawer, project_id: Option<&str>) {
        db.insert_drawer_with_project(&drawer, project_id)
            .expect("insert drawer");
    }

    fn drawer(id: &str, content: &str) -> Drawer {
        Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
            id: id.to_string(),
            content: content.to_string(),
            wing: "mempal".to_string(),
            room: Some("markdown".to_string()),
            source_file: Some(format!("tests://{id}.md")),
            source_type: SourceType::AgentObservation,
            added_at: "2026-06-19T12:00:00Z".to_string(),
            chunk_index: Some(0),
            importance: 3,
        })
    }

    #[test]
    fn export_preserves_stable_frontmatter_and_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        let mut seeded = drawer("drawer/stable", "remember the interface contract");
        seeded.memory_kind = crate::core::types::MemoryKind::Decision;
        seeded.statement = Some("Use SQLite as canonical memory.".to_string());
        seeded.supporting_refs = vec!["drawer-source".to_string()];
        insert_drawer(&db, seeded, Some("proj-md"));

        let out = tmp.path().join("mirror");
        let mut options = MarkdownExportOptions::new(
            out.clone(),
            ProjectSearchScope::from_request(Some("proj-md".to_string()), false, false, false),
        );
        options.redact = true;

        let report = export_markdown(&db, &options).expect("export markdown");
        assert_eq!(report.exported, 1);

        let expected_path = out.join("wing-mempal").join("room-markdown").join(format!(
            "drawer_stable-{}.md",
            &blake3::hash("drawer/stable".as_bytes()).to_hex()[..8]
        ));
        let exported = fs::read_to_string(expected_path).expect("read exported markdown");
        assert!(exported.contains("mempal_format: \"markdown_mirror_v1\""));
        assert!(exported.contains("canonical_source: \"sqlite\""));
        assert!(exported.contains("mirror_semantics: \"generated_read_only\""));
        assert!(exported.contains("drawer_id: \"drawer/stable\""));
        assert!(exported.contains("project_id: \"proj-md\""));
        assert!(exported.contains("memory_kind: \"decision\""));
        assert!(exported.contains("source_file: \"tests://drawer/stable.md\""));
        assert!(exported.contains("supporting_refs:\n  - \"drawer-source\""));
        assert!(exported.contains("## Content\n\nremember the interface contract"));
    }

    #[test]
    fn export_redacts_secret_like_values_by_default() {
        let secret = "sk-abcdefghijklmnopqrstuvwxyz1234567890";
        let body = export_body(
            &format!("token {secret} and api_key=plainsecretvalue"),
            DEFAULT_MAX_BODY_BYTES,
            true,
        );
        assert!(!body.content.contains(secret));
        assert!(!body.content.contains("plainsecretvalue"));
        assert!(body.content.contains("[REDACTED:openai_key]"));
        assert!(body.content.contains("[REDACTED:secret_like]"));
    }
}
