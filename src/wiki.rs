use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::core::config::scrub_export_sensitive_text;
use crate::core::db::Database;
use crate::core::decay::parse_temporal_timestamp_secs;
use crate::core::project::ProjectSearchScope;

const FORMAT_VERSION: &str = "knowledge_wiki_v1";
const CANONICAL_SOURCE: &str = "sqlite";
const WIKI_SEMANTICS: &str = "generated_read_only";
const MANIFEST_FILE: &str = ".mempal-wiki.toml";
const README_FILE: &str = "README.md";
const INDEX_FILE: &str = "index.md";

#[derive(Debug, Clone)]
pub struct WikiBuildOptions {
    pub output_dir: PathBuf,
    pub scope: ProjectSearchScope,
    pub now_secs: i64,
    pub redact: bool,
}

#[derive(Debug, Clone)]
pub struct WikiVerifyOptions {
    pub output_dir: PathBuf,
    pub now_secs: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiBuildReport {
    pub output_dir: PathBuf,
    pub page_count: usize,
    pub citation_count: usize,
    pub canonical_source: &'static str,
    pub wiki_semantics: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiVerifyReport {
    pub output_dir: PathBuf,
    pub checked_refs: usize,
    pub stale_refs: Vec<WikiStaleRef>,
}

impl WikiVerifyReport {
    pub fn is_clean(&self) -> bool {
        self.stale_refs.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WikiStaleRef {
    pub page: String,
    pub ref_kind: String,
    pub ref_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct WikiManifest {
    mempal_format: String,
    canonical_source: String,
    wiki_semantics: String,
    generated_files: Vec<String>,
    pages: Vec<WikiManifestPage>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct WikiManifestPage {
    path: String,
    title: String,
    kind: String,
    drawer_refs: Vec<WikiDrawerRef>,
    triple_refs: Vec<WikiTripleRef>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct WikiDrawerRef {
    drawer_id: String,
    role: String,
    require_active: bool,
    source_file: Option<String>,
    source_file_hash: Option<String>,
    content_hash: Option<String>,
    updated_at: Option<String>,
    valid_until: Option<String>,
    deleted_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct WikiTripleRef {
    triple_id: String,
    role: String,
    require_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim_hash: Option<String>,
    valid_to: Option<String>,
    source_drawer: Option<String>,
}

#[derive(Debug, Clone)]
struct WikiPage {
    path: String,
    title: String,
    kind: &'static str,
    markdown: String,
    drawer_refs: Vec<WikiDrawerRef>,
    triple_refs: Vec<WikiTripleRef>,
}

#[derive(Debug, Clone)]
struct WikiDecision {
    id: String,
    content: String,
    project_id: Option<String>,
    statement: Option<String>,
    status: Option<String>,
    supporting_refs: Vec<String>,
    counterexample_refs: Vec<String>,
    verification_refs: Vec<String>,
    supersedes: Option<String>,
    valid_until: Option<String>,
}

#[derive(Debug, Clone)]
struct WikiTriple {
    id: String,
    subject: String,
    predicate: String,
    object: String,
    valid_from: Option<String>,
    valid_to: Option<String>,
    source_drawer: Option<String>,
    source_project_id: Option<String>,
}

#[derive(Debug)]
enum ActiveDrawerRef {
    Active(WikiDrawerRef),
    Missing,
    Inactive(&'static str),
}

#[derive(Debug)]
struct OmittedTripleClaim<'a> {
    triple: &'a WikiTriple,
    reason: &'static str,
}

pub fn current_wiki_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub fn build_wiki(db: &Database, options: &WikiBuildOptions) -> Result<WikiBuildReport> {
    prepare_output_dir(&options.output_dir)?;
    let previous_manifest = load_manifest(&options.output_dir)?;
    let previous_paths = manifest_generated_paths(&previous_manifest)?;

    let pages = assemble_pages(db, options)?;
    let mut generated_paths = BTreeSet::from([README_FILE.to_string(), INDEX_FILE.to_string()]);
    for page in &pages {
        generated_paths.insert(page.path.clone());
    }
    verify_no_unmanaged_collisions(&options.output_dir, &generated_paths, &previous_paths)?;

    for page in &pages {
        let relative = validate_manifest_relative_path(&page.path)?;
        write_generated_file(
            &options.output_dir,
            &relative,
            &page.path,
            page.markdown.as_bytes(),
            &previous_paths,
        )?;
    }
    write_generated_file(
        &options.output_dir,
        Path::new(README_FILE),
        README_FILE,
        render_readme(pages.len()).as_bytes(),
        &previous_paths,
    )?;
    write_generated_file(
        &options.output_dir,
        Path::new(INDEX_FILE),
        INDEX_FILE,
        render_index(&pages).as_bytes(),
        &previous_paths,
    )?;
    remove_stale_generated_files(&options.output_dir, &previous_paths, &generated_paths)?;
    write_manifest(
        &options.output_dir,
        &generated_paths,
        &pages,
        previous_manifest_exists(&options.output_dir)?,
    )?;

    let citation_count = pages
        .iter()
        .map(|page| page.drawer_refs.len() + page.triple_refs.len())
        .sum();
    Ok(WikiBuildReport {
        output_dir: options.output_dir.clone(),
        page_count: pages.len(),
        citation_count,
        canonical_source: CANONICAL_SOURCE,
        wiki_semantics: WIKI_SEMANTICS,
    })
}

pub fn verify_wiki(db: &Database, options: &WikiVerifyOptions) -> Result<WikiVerifyReport> {
    let manifest = load_existing_manifest(&options.output_dir)?;
    let mut stale_refs = Vec::new();
    let mut checked_refs = 0usize;

    for page in &manifest.pages {
        for expected in &page.drawer_refs {
            checked_refs += 1;
            match load_drawer_ref(
                db,
                &expected.drawer_id,
                &expected.role,
                expected.require_active,
            )? {
                Some(current) => compare_drawer_ref(
                    &page.path,
                    expected,
                    &current,
                    options.now_secs,
                    &mut stale_refs,
                ),
                None => stale_refs.push(WikiStaleRef {
                    page: page.path.clone(),
                    ref_kind: "drawer".to_string(),
                    ref_id: expected.drawer_id.clone(),
                    reason: "missing drawer ref".to_string(),
                }),
            }
        }
        for expected in &page.triple_refs {
            checked_refs += 1;
            match load_triple_ref(
                db,
                &expected.triple_id,
                &expected.role,
                expected.require_active,
            )? {
                Some(current) => compare_triple_ref(
                    &page.path,
                    expected,
                    &current,
                    options.now_secs,
                    &mut stale_refs,
                ),
                None => stale_refs.push(WikiStaleRef {
                    page: page.path.clone(),
                    ref_kind: "triple".to_string(),
                    ref_id: expected.triple_id.clone(),
                    reason: "missing triple ref".to_string(),
                }),
            }
        }
    }

    Ok(WikiVerifyReport {
        output_dir: options.output_dir.clone(),
        checked_refs,
        stale_refs,
    })
}

fn assemble_pages(db: &Database, options: &WikiBuildOptions) -> Result<Vec<WikiPage>> {
    let mut pages = Vec::new();
    pages.extend(entity_pages(db, options)?);
    pages.extend(decision_pages(db, options)?);
    pages.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(pages)
}

fn entity_pages(db: &Database, options: &WikiBuildOptions) -> Result<Vec<WikiPage>> {
    let triples = load_triples(db, &options.scope)?;
    let mut by_entity: BTreeMap<String, Vec<WikiTriple>> = BTreeMap::new();
    for triple in triples {
        by_entity
            .entry(triple.subject.clone())
            .or_default()
            .push(triple.clone());
        by_entity
            .entry(triple.object.clone())
            .or_default()
            .push(triple);
    }

    let mut pages = Vec::new();
    for (entity, triples) in by_entity {
        let display_entity = redact_if_needed(&entity, options.redact);
        let path = format!(
            "entities/{}.md",
            path_component_with_hash_display(&display_entity, &entity)
        );
        let page = render_entity_page(
            db,
            &path,
            &entity,
            &display_entity,
            &triples,
            options.now_secs,
            options.redact,
        )?;
        pages.push(page);
    }
    Ok(pages)
}

fn decision_pages(db: &Database, options: &WikiBuildOptions) -> Result<Vec<WikiPage>> {
    let decisions = load_decisions(db, &options.scope)?;
    let mut pages = Vec::new();
    for decision in decisions {
        let title = decision_title(&decision);
        let display_title = redact_if_needed(&title, options.redact);
        let path = format!(
            "decisions/{}.md",
            path_component_with_hash_display(&display_title, &format!("{}-{title}", decision.id))
        );
        pages.push(render_decision_page(
            db,
            &path,
            &decision,
            &display_title,
            options.now_secs,
            options.redact,
        )?);
    }
    Ok(pages)
}

fn render_entity_page(
    db: &Database,
    path: &str,
    entity: &str,
    display_entity: &str,
    triples: &[WikiTriple],
    now_secs: i64,
    redact: bool,
) -> Result<WikiPage> {
    let mut markdown = frontmatter(path, "entity", display_entity);
    markdown.push_str("# ");
    markdown.push_str(&markdown_inline(display_entity));
    markdown.push_str("\n\n");
    markdown.push_str(DERIVED_NOTICE);
    let mut drawer_refs = Vec::new();
    let mut triple_refs = Vec::new();
    let mut omitted_claims = Vec::new();

    markdown.push_str("## Active Claims\n\n");
    let mut active_count = 0usize;
    for triple in triples
        .iter()
        .filter(|triple| triple_is_active(triple, now_secs))
    {
        let source_ref = match active_triple_source_ref(db, triple, now_secs)? {
            ActiveDrawerRef::Active(drawer_ref) => drawer_ref,
            ActiveDrawerRef::Missing => {
                omitted_claims.push(OmittedTripleClaim {
                    triple,
                    reason: "source drawer is missing",
                });
                continue;
            }
            ActiveDrawerRef::Inactive(reason) => {
                omitted_claims.push(OmittedTripleClaim { triple, reason });
                continue;
            }
        };
        let source_ref = output_drawer_ref(source_ref, redact);
        active_count += 1;
        push_triple_claim(&mut markdown, entity, display_entity, triple, redact);
        push_triple_citation(
            &mut markdown,
            triple,
            true,
            Some(source_ref),
            &mut drawer_refs,
            &mut triple_refs,
            redact,
        );
    }
    if active_count == 0 {
        markdown.push_str("No source-backed active claims were found.\n");
    }

    markdown.push_str("\n## Superseded Claims\n\n");
    let mut superseded_count = 0usize;
    for triple in triples
        .iter()
        .filter(|triple| !triple_is_active(triple, now_secs))
    {
        if triple.source_drawer.is_none() {
            omitted_claims.push(OmittedTripleClaim {
                triple,
                reason: "source drawer is missing",
            });
            continue;
        }
        superseded_count += 1;
        push_triple_claim(&mut markdown, entity, display_entity, triple, redact);
        let source_ref = load_triple_source_ref(db, triple, false, redact)?;
        push_triple_citation(
            &mut markdown,
            triple,
            false,
            source_ref,
            &mut drawer_refs,
            &mut triple_refs,
            redact,
        );
    }
    if superseded_count == 0 {
        markdown.push_str("No source-backed superseded claims were found.\n");
    }

    markdown.push_str("\n## Open Questions\n\n");
    markdown.push_str(
        "No source-backed open questions were found by this deterministic wiki builder.\n",
    );
    push_omitted_section(&mut markdown, &omitted_claims, redact);

    Ok(WikiPage {
        path: path.to_string(),
        title: display_entity.to_string(),
        kind: "entity",
        markdown,
        drawer_refs,
        triple_refs,
    })
}

fn render_decision_page(
    db: &Database,
    path: &str,
    decision: &WikiDecision,
    display_title: &str,
    now_secs: i64,
    redact: bool,
) -> Result<WikiPage> {
    let mut markdown = frontmatter(path, "decision", display_title);
    markdown.push_str("# ");
    markdown.push_str(&markdown_inline(display_title));
    markdown.push_str("\n\n");
    markdown.push_str(DERIVED_NOTICE);
    let mut drawer_refs = Vec::new();
    let triple_refs = Vec::new();
    let decision_active = decision_is_active(decision, now_secs);

    markdown.push_str("## Active Claims\n\n");
    if decision_active {
        markdown.push_str("- ");
        markdown.push_str(&markdown_inline(display_title));
        markdown.push('\n');
        push_decision_citation(&mut markdown, db, decision, true, &mut drawer_refs, redact)?;
        push_decision_supporting_refs(
            &mut markdown,
            db,
            decision,
            &mut drawer_refs,
            now_secs,
            redact,
        )?;
    } else {
        markdown.push_str("No source-backed active claims were found.\n");
    }

    markdown.push_str("\n## Superseded Claims\n\n");
    let mut superseded_count = 0usize;
    if !decision_active {
        superseded_count += 1;
        markdown.push_str("- ");
        markdown.push_str(&markdown_inline(display_title));
        markdown.push('\n');
        push_decision_citation(&mut markdown, db, decision, false, &mut drawer_refs, redact)?;
    }
    if let Some(old_id) = decision.supersedes.as_deref()
        && let Some(old_ref) = load_drawer_ref(db, old_id, "superseded_claim", false)?
    {
        superseded_count += 1;
        markdown.push_str("- Supersedes `");
        markdown.push_str(&markdown_inline(&redact_if_needed(old_id, redact)));
        markdown.push_str("`\n");
        let old_ref = output_drawer_ref(old_ref, redact);
        push_drawer_ref_line(&mut markdown, &old_ref, redact);
        drawer_refs.push(old_ref);
    }
    if superseded_count == 0 {
        markdown.push_str("No source-backed superseded claims were found.\n");
    }

    markdown.push_str("\n## Open Questions\n\n");
    if matches!(
        decision.status.as_deref(),
        Some("candidate" | "pending_review")
    ) {
        markdown.push_str("- Decision is not active yet; current lifecycle status is `");
        markdown.push_str(&markdown_inline(&redact_if_needed(
            decision.status.as_deref().unwrap_or("unknown"),
            redact,
        )));
        markdown.push_str("`.\n");
    } else {
        markdown.push_str(
            "No source-backed open questions were found by this deterministic wiki builder.\n",
        );
    }

    markdown.push_str("\n## Rationale Excerpt\n\n");
    push_excerpt(&mut markdown, &decision.content, redact);

    Ok(WikiPage {
        path: path.to_string(),
        title: display_title.to_string(),
        kind: "decision",
        markdown,
        drawer_refs,
        triple_refs,
    })
}

const DERIVED_NOTICE: &str = "> Generated from canonical SQLite memory. This wiki is a derived, read-only view, not an authoritative store.\n\n";

fn frontmatter(path: &str, kind: &str, title: &str) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    push_yaml_str(&mut out, "mempal_format", FORMAT_VERSION);
    push_yaml_str(&mut out, "canonical_source", CANONICAL_SOURCE);
    push_yaml_str(&mut out, "wiki_semantics", WIKI_SEMANTICS);
    push_yaml_str(&mut out, "page_kind", kind);
    push_yaml_str(&mut out, "page_path", path);
    push_yaml_str(&mut out, "title", title);
    out.push_str("---\n\n");
    out
}

fn push_triple_claim(
    output: &mut String,
    entity: &str,
    display_entity: &str,
    triple: &WikiTriple,
    redact: bool,
) {
    output.push_str("- ");
    if triple.subject == entity {
        output.push('`');
        output.push_str(&markdown_inline(&redact_if_needed(&triple.subject, redact)));
        output.push_str("` --`");
        output.push_str(&markdown_inline(&redact_if_needed(
            &triple.predicate,
            redact,
        )));
        output.push_str("`--> `");
        output.push_str(&markdown_inline(&redact_if_needed(&triple.object, redact)));
        output.push('`');
    } else {
        output.push('`');
        output.push_str(&markdown_inline(display_entity));
        output.push_str("` <--`");
        output.push_str(&markdown_inline(&redact_if_needed(
            &triple.predicate,
            redact,
        )));
        output.push_str("`-- `");
        output.push_str(&markdown_inline(&redact_if_needed(&triple.subject, redact)));
        output.push('`');
    }
    if let Some(valid_from) = triple.valid_from.as_deref() {
        output.push_str(" (valid_from `");
        output.push_str(&markdown_inline(&redact_if_needed(valid_from, redact)));
        output.push_str("`)");
    }
    output.push('\n');
}

fn push_triple_citation(
    output: &mut String,
    triple: &WikiTriple,
    require_active: bool,
    source_ref: Option<WikiDrawerRef>,
    drawer_refs: &mut Vec<WikiDrawerRef>,
    triple_refs: &mut Vec<WikiTripleRef>,
    redact: bool,
) {
    triple_refs.push(WikiTripleRef {
        triple_id: triple.id.clone(),
        role: if require_active {
            "active_claim".to_string()
        } else {
            "superseded_claim".to_string()
        },
        require_active,
        claim_hash: Some(triple_claim_hash(
            &triple.subject,
            &triple.predicate,
            &triple.object,
            triple.valid_from.as_deref(),
        )),
        valid_to: triple.valid_to.clone(),
        source_drawer: triple.source_drawer.clone(),
    });
    output.push_str("  - citation: `triple:");
    output.push_str(&markdown_inline(&redact_if_needed(&triple.id, redact)));
    output.push('`');
    if let Some(drawer_ref) = source_ref {
        output.push_str(", ");
        push_drawer_ref_inline(output, &drawer_ref, redact);
        drawer_refs.push(drawer_ref);
    }
    output.push('\n');
}

fn push_decision_citation(
    output: &mut String,
    db: &Database,
    decision: &WikiDecision,
    require_active: bool,
    drawer_refs: &mut Vec<WikiDrawerRef>,
    redact: bool,
) -> Result<()> {
    let Some(drawer_ref) = load_drawer_ref(db, &decision.id, "decision_claim", require_active)?
    else {
        bail!(
            "decision drawer disappeared while rendering wiki: {}",
            decision.id
        );
    };
    let drawer_ref = output_drawer_ref(drawer_ref, redact);
    output.push_str("  - citation: ");
    push_drawer_ref_inline(output, &drawer_ref, redact);
    output.push('\n');
    drawer_refs.push(drawer_ref);
    Ok(())
}

fn push_decision_supporting_refs(
    output: &mut String,
    db: &Database,
    decision: &WikiDecision,
    drawer_refs: &mut Vec<WikiDrawerRef>,
    now_secs: i64,
    redact: bool,
) -> Result<()> {
    let refs = role_refs(decision);
    if refs.is_empty() {
        output.push_str(
            "  - supporting_refs: none recorded; only the decision drawer itself is cited.\n",
        );
        return Ok(());
    }
    output.push_str("  - supporting_refs:\n");
    for (role, drawer_id) in refs {
        match load_active_drawer_ref(db, &drawer_id, role, now_secs)? {
            ActiveDrawerRef::Active(drawer_ref) => {
                let drawer_ref = output_drawer_ref(drawer_ref, redact);
                output.push_str("    - ");
                push_drawer_ref_inline(output, &drawer_ref, redact);
                output.push('\n');
                drawer_refs.push(drawer_ref);
            }
            ActiveDrawerRef::Missing => {
                output.push_str("    - missing `drawer:");
                output.push_str(&markdown_inline(&redact_if_needed(&drawer_id, redact)));
                output.push_str("` (not cited)\n");
            }
            ActiveDrawerRef::Inactive(reason) => {
                output.push_str("    - stale `drawer:");
                output.push_str(&markdown_inline(&redact_if_needed(&drawer_id, redact)));
                output.push_str("` (not cited; ");
                output.push_str(reason);
                output.push_str(")\n");
            }
        }
    }
    Ok(())
}

fn role_refs(decision: &WikiDecision) -> Vec<(&'static str, String)> {
    let mut refs = Vec::new();
    refs.extend(
        decision
            .supporting_refs
            .iter()
            .cloned()
            .map(|id| ("supporting_ref", id)),
    );
    refs.extend(
        decision
            .verification_refs
            .iter()
            .cloned()
            .map(|id| ("verification_ref", id)),
    );
    refs.extend(
        decision
            .counterexample_refs
            .iter()
            .cloned()
            .map(|id| ("counterexample_ref", id)),
    );
    refs
}

fn push_drawer_ref_line(output: &mut String, drawer_ref: &WikiDrawerRef, redact: bool) {
    output.push_str("  - citation: ");
    push_drawer_ref_inline(output, drawer_ref, redact);
    output.push('\n');
}

fn push_drawer_ref_inline(output: &mut String, drawer_ref: &WikiDrawerRef, redact: bool) {
    output.push_str("`drawer:");
    output.push_str(&markdown_inline(&redact_if_needed(
        &drawer_ref.drawer_id,
        redact,
    )));
    output.push('`');
    if let Some(source_file) = drawer_ref.source_file.as_deref() {
        output.push_str(" (`");
        output.push_str(&markdown_inline(source_file));
        output.push_str("`)");
    } else {
        output.push_str(" (no source_file recorded)");
    }
}

fn push_excerpt(output: &mut String, content: &str, redact: bool) {
    let redacted = redact_if_needed(content, redact);
    let excerpt = redacted.trim();
    if excerpt.is_empty() {
        output.push_str("No rationale content recorded.\n");
        return;
    }
    let mut taken = String::new();
    for ch in excerpt.chars().take(600) {
        taken.push(ch);
    }
    output.push_str("```text\n");
    output.push_str(&taken);
    if excerpt.chars().count() > taken.chars().count() {
        output.push_str("\n[TRUNCATED]\n");
    } else if !taken.ends_with('\n') {
        output.push('\n');
    }
    output.push_str("```\n");
}

fn push_omitted_section(
    output: &mut String,
    omitted_claims: &[OmittedTripleClaim<'_>],
    redact: bool,
) {
    if omitted_claims.is_empty() {
        return;
    }
    output.push_str("\n## Uncited Claims Omitted\n\n");
    output.push_str(
        "The following KG triples were not rendered as active/source-backed claims because their source drawer support is unavailable or inactive:\n",
    );
    for omitted in omitted_claims {
        output.push_str("- `triple:");
        output.push_str(&markdown_inline(&redact_if_needed(
            &omitted.triple.id,
            redact,
        )));
        output.push_str("` (");
        output.push_str(omitted.reason);
        output.push_str(")\n");
    }
}

fn active_triple_source_ref(
    db: &Database,
    triple: &WikiTriple,
    now_secs: i64,
) -> Result<ActiveDrawerRef> {
    let Some(source_drawer) = triple.source_drawer.as_deref() else {
        return Ok(ActiveDrawerRef::Missing);
    };
    load_active_drawer_ref(db, source_drawer, "triple_source", now_secs)
}

fn load_triple_source_ref(
    db: &Database,
    triple: &WikiTriple,
    require_active: bool,
    redact: bool,
) -> Result<Option<WikiDrawerRef>> {
    let Some(source_drawer) = triple.source_drawer.as_deref() else {
        return Ok(None);
    };
    Ok(
        load_drawer_ref(db, source_drawer, "triple_source", require_active)?
            .map(|drawer_ref| output_drawer_ref(drawer_ref, redact)),
    )
}

fn load_active_drawer_ref(
    db: &Database,
    drawer_id: &str,
    role: &str,
    now_secs: i64,
) -> Result<ActiveDrawerRef> {
    let Some(drawer_ref) = load_drawer_ref(db, drawer_id, role, true)? else {
        return Ok(ActiveDrawerRef::Missing);
    };
    if let Some(reason) = drawer_inactive_reason(&drawer_ref, now_secs) {
        return Ok(ActiveDrawerRef::Inactive(reason));
    }
    Ok(ActiveDrawerRef::Active(drawer_ref))
}

fn drawer_inactive_reason(drawer_ref: &WikiDrawerRef, now_secs: i64) -> Option<&'static str> {
    if drawer_ref.deleted_at.is_some() {
        return Some("source drawer is deleted");
    }
    if timestamp_expired(drawer_ref.valid_until.as_deref(), now_secs) {
        return Some("source drawer validity expired");
    }
    None
}

fn output_drawer_ref(mut drawer_ref: WikiDrawerRef, redact: bool) -> WikiDrawerRef {
    if let Some(source_file) = drawer_ref.source_file.as_deref() {
        drawer_ref.source_file_hash = Some(hash_text(source_file));
        drawer_ref.source_file = Some(redact_if_needed(source_file, redact));
    }
    drawer_ref
}

fn hash_text(value: &str) -> String {
    blake3::hash(value.as_bytes()).to_hex().to_string()
}

fn redact_if_needed(value: &str, redact: bool) -> String {
    if !redact {
        return value.to_string();
    }
    scrub_export_sensitive_text(value)
}

fn render_readme(page_count: usize) -> String {
    format!(
        "# mempal Knowledge Wiki\n\n\
         This directory was generated by `mempal wiki build` from canonical SQLite memory.\n\n\
         - `canonical_source: sqlite` means SQLite wins by default.\n\
         - `wiki_semantics: generated_read_only` means these Markdown pages are derived views, not editable source data.\n\
         - Secret-like values in generated Markdown are redacted by default; use `--no-redact` only for trusted local exports.\n\
         - `.mempal-wiki.toml` records generated files and source fingerprints for `mempal wiki verify`.\n\
         - Wiki import/update sync is intentionally not implemented here. A future import flow must be explicit and conflict-safe.\n\
         - Generated pages: {page_count}.\n\n\
         Re-run `mempal wiki build <dir>` to refresh derived pages from SQLite.\n"
    )
}

fn render_index(pages: &[WikiPage]) -> String {
    let mut output = String::new();
    output.push_str("# mempal Knowledge Wiki Index\n\n");
    output.push_str("SQLite is canonical. Wiki pages are generated read-only views.\n\n");
    output.push_str("## Entities\n\n");
    for page in pages.iter().filter(|page| page.kind == "entity") {
        output.push_str("- [");
        output.push_str(&markdown_inline(&page.title));
        output.push_str("](");
        output.push_str(&page.path.replace(' ', "%20"));
        output.push_str(")\n");
    }
    output.push_str("\n## Decisions\n\n");
    for page in pages.iter().filter(|page| page.kind == "decision") {
        output.push_str("- [");
        output.push_str(&markdown_inline(&page.title));
        output.push_str("](");
        output.push_str(&page.path.replace(' ', "%20"));
        output.push_str(")\n");
    }
    output
}

fn load_triples(db: &Database, scope: &ProjectSearchScope) -> Result<Vec<WikiTriple>> {
    let mut statement = db
        .conn()
        .prepare(
            r#"
            SELECT
                t.id,
                t.subject,
                t.predicate,
                t.object,
                t.valid_from,
                t.valid_to,
                t.source_drawer,
                d.project_id
            FROM triples t
            LEFT JOIN drawers d ON d.id = t.source_drawer
            ORDER BY t.subject ASC, t.predicate ASC, t.object ASC, t.id ASC
            "#,
        )
        .context("failed to prepare wiki triple query")?;
    let rows = statement
        .query_map([], |row| {
            Ok(WikiTriple {
                id: row.get(0)?,
                subject: row.get(1)?,
                predicate: row.get(2)?,
                object: row.get(3)?,
                valid_from: row.get(4)?,
                valid_to: row.get(5)?,
                source_drawer: row.get(6)?,
                source_project_id: row.get(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to decode wiki triples")?;
    Ok(rows
        .into_iter()
        .filter(|triple| scope.allows_row(triple.source_project_id.as_deref()))
        .collect())
}

fn load_decisions(db: &Database, scope: &ProjectSearchScope) -> Result<Vec<WikiDecision>> {
    let mut statement = db
        .conn()
        .prepare(
            r#"
            SELECT
                id,
                content,
                project_id,
                statement,
                status,
                supporting_refs,
                counterexample_refs,
                verification_refs,
                supersedes,
                valid_until
            FROM drawers
            WHERE deleted_at IS NULL
              AND memory_kind = 'decision'
            ORDER BY COALESCE(statement, id) ASC, id ASC
            "#,
        )
        .context("failed to prepare wiki decision query")?;
    let rows = statement
        .query_map([], |row| {
            Ok(WikiDecision {
                id: row.get(0)?,
                content: row.get(1)?,
                project_id: row.get(2)?,
                statement: row.get(3)?,
                status: row.get(4)?,
                supporting_refs: decode_refs(row.get::<_, Option<String>>(5)?)?,
                counterexample_refs: decode_refs(row.get::<_, Option<String>>(6)?)?,
                verification_refs: decode_refs(row.get::<_, Option<String>>(7)?)?,
                supersedes: row.get(8)?,
                valid_until: row.get(9)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to decode wiki decisions")?;
    Ok(rows
        .into_iter()
        .filter(|decision| scope.allows_row(decision.project_id.as_deref()))
        .collect())
}

fn load_drawer_ref(
    db: &Database,
    drawer_id: &str,
    role: &str,
    require_active: bool,
) -> Result<Option<WikiDrawerRef>> {
    db.conn()
        .query_row(
            r#"
            SELECT id, source_file, content_hash, updated_at, valid_until, deleted_at
            FROM drawers
            WHERE id = ?1
            "#,
            [drawer_id],
            |row| {
                let source_file: Option<String> = row.get(1)?;
                let source_file_hash = source_file.as_deref().map(hash_text);
                Ok(WikiDrawerRef {
                    drawer_id: row.get(0)?,
                    role: role.to_string(),
                    require_active,
                    source_file,
                    source_file_hash,
                    content_hash: row.get(2)?,
                    updated_at: row.get(3)?,
                    valid_until: row.get(4)?,
                    deleted_at: row.get(5)?,
                })
            },
        )
        .optional()
        .context("failed to load wiki drawer ref")
}

fn load_triple_ref(
    db: &Database,
    triple_id: &str,
    role: &str,
    require_active: bool,
) -> Result<Option<WikiTripleRef>> {
    db.conn()
        .query_row(
            r#"
            SELECT id, subject, predicate, object, valid_from, valid_to, source_drawer
            FROM triples
            WHERE id = ?1
            "#,
            [triple_id],
            |row| {
                let subject: String = row.get(1)?;
                let predicate: String = row.get(2)?;
                let object: String = row.get(3)?;
                let valid_from: Option<String> = row.get(4)?;
                Ok(WikiTripleRef {
                    triple_id: row.get(0)?,
                    role: role.to_string(),
                    require_active,
                    claim_hash: Some(triple_claim_hash(
                        &subject,
                        &predicate,
                        &object,
                        valid_from.as_deref(),
                    )),
                    valid_to: row.get(5)?,
                    source_drawer: row.get(6)?,
                })
            },
        )
        .optional()
        .context("failed to load wiki triple ref")
}

fn compare_drawer_ref(
    page: &str,
    expected: &WikiDrawerRef,
    current: &WikiDrawerRef,
    now_secs: i64,
    stale_refs: &mut Vec<WikiStaleRef>,
) {
    if expected.content_hash != current.content_hash {
        push_stale(
            stale_refs,
            page,
            "drawer",
            &expected.drawer_id,
            "content_hash changed",
        );
    }
    if expected.updated_at != current.updated_at {
        push_stale(
            stale_refs,
            page,
            "drawer",
            &expected.drawer_id,
            "updated_at changed",
        );
    }
    let source_file_changed = match (&expected.source_file_hash, &current.source_file_hash) {
        (Some(expected_hash), Some(current_hash)) => expected_hash != current_hash,
        (Some(_), None) => true,
        (None, Some(_)) | (None, None) => expected.source_file != current.source_file,
    };
    if source_file_changed {
        push_stale(
            stale_refs,
            page,
            "drawer",
            &expected.drawer_id,
            "source_file changed",
        );
    }
    if expected.require_active && current.deleted_at.is_some() {
        push_stale(
            stale_refs,
            page,
            "drawer",
            &expected.drawer_id,
            "drawer was deleted",
        );
    }
    if expected.require_active && timestamp_expired(current.valid_until.as_deref(), now_secs) {
        push_stale(
            stale_refs,
            page,
            "drawer",
            &expected.drawer_id,
            "drawer validity expired",
        );
    }
}

fn compare_triple_ref(
    page: &str,
    expected: &WikiTripleRef,
    current: &WikiTripleRef,
    now_secs: i64,
    stale_refs: &mut Vec<WikiStaleRef>,
) {
    match (&expected.claim_hash, &current.claim_hash) {
        (Some(expected_hash), Some(current_hash)) if expected_hash != current_hash => push_stale(
            stale_refs,
            page,
            "triple",
            &expected.triple_id,
            "claim content changed",
        ),
        (None, _) => push_stale(
            stale_refs,
            page,
            "triple",
            &expected.triple_id,
            "claim content fingerprint missing",
        ),
        _ => {}
    }
    if expected.valid_to != current.valid_to {
        push_stale(
            stale_refs,
            page,
            "triple",
            &expected.triple_id,
            "valid_to changed",
        );
    }
    if expected.source_drawer != current.source_drawer {
        push_stale(
            stale_refs,
            page,
            "triple",
            &expected.triple_id,
            "source_drawer changed",
        );
    }
    if expected.require_active && timestamp_expired(current.valid_to.as_deref(), now_secs) {
        push_stale(
            stale_refs,
            page,
            "triple",
            &expected.triple_id,
            "triple validity expired",
        );
    }
}

fn push_stale(
    stale_refs: &mut Vec<WikiStaleRef>,
    page: &str,
    ref_kind: &str,
    ref_id: &str,
    reason: &str,
) {
    stale_refs.push(WikiStaleRef {
        page: page.to_string(),
        ref_kind: ref_kind.to_string(),
        ref_id: ref_id.to_string(),
        reason: reason.to_string(),
    });
}

fn triple_is_active(triple: &WikiTriple, now_secs: i64) -> bool {
    !timestamp_expired(triple.valid_to.as_deref(), now_secs)
}

fn timestamp_expired(raw: Option<&str>, now_secs: i64) -> bool {
    raw.and_then(parse_temporal_timestamp_secs)
        .is_some_and(|expires| expires <= now_secs)
}

fn triple_claim_hash(
    subject: &str,
    predicate: &str,
    object: &str,
    valid_from: Option<&str>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    for value in [Some(subject), Some(predicate), Some(object), valid_from] {
        match value {
            Some(value) => {
                hasher.update(&[1]);
                hasher.update(value.as_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
        hasher.update(&[0xff]);
    }
    hasher.finalize().to_hex().to_string()
}

fn decision_is_active(decision: &WikiDecision, now_secs: i64) -> bool {
    !matches!(
        decision.status.as_deref(),
        Some("superseded" | "demoted" | "retired" | "pending_review" | "candidate")
    ) && !timestamp_expired(decision.valid_until.as_deref(), now_secs)
}

fn decision_title(decision: &WikiDecision) -> String {
    decision
        .statement
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(decision.id.as_str())
        .to_string()
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

fn prepare_output_dir(output_dir: &Path) -> Result<()> {
    fs::create_dir_all(output_dir).with_context(|| {
        format!(
            "failed to create knowledge wiki directory {}",
            output_dir.display()
        )
    })?;
    ensure_output_dir(output_dir)?;
    if previous_manifest_exists(output_dir)? || output_dir_is_empty(output_dir)? {
        return Ok(());
    }
    bail!(
        "refusing to build into non-empty unmanaged knowledge wiki directory {}; choose an empty directory or an existing mempal knowledge wiki",
        output_dir.display()
    );
}

fn ensure_output_dir(output_dir: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(output_dir).with_context(|| {
        format!(
            "failed to inspect knowledge wiki directory {}",
            output_dir.display()
        )
    })?;
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to build into symlinked knowledge wiki directory {}",
            output_dir.display()
        );
    }
    if !metadata.file_type().is_dir() {
        bail!(
            "knowledge wiki path is not a directory {}",
            output_dir.display()
        );
    }
    Ok(())
}

fn output_dir_is_empty(output_dir: &Path) -> Result<bool> {
    let mut entries = fs::read_dir(output_dir).with_context(|| {
        format!(
            "failed to read knowledge wiki directory {}",
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
                    "refusing to use symlinked knowledge wiki manifest {}",
                    manifest_path.display()
                );
            }
            if !metadata.file_type().is_file() {
                bail!(
                    "knowledge wiki manifest is not a regular file {}",
                    manifest_path.display()
                );
            }
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to inspect knowledge wiki manifest {}",
                manifest_path.display()
            )
        }),
    }
}

fn load_manifest(output_dir: &Path) -> Result<WikiManifest> {
    if !previous_manifest_exists(output_dir)? {
        return Ok(WikiManifest::empty());
    }
    load_existing_manifest(output_dir)
}

fn load_existing_manifest(output_dir: &Path) -> Result<WikiManifest> {
    let manifest_path = output_dir.join(MANIFEST_FILE);
    let raw = fs::read_to_string(&manifest_path).with_context(|| {
        format!(
            "failed to read knowledge wiki manifest {}",
            manifest_path.display()
        )
    })?;
    let manifest: WikiManifest = toml::from_str(&raw).with_context(|| {
        format!(
            "failed to parse knowledge wiki manifest {}",
            manifest_path.display()
        )
    })?;
    if manifest.mempal_format != FORMAT_VERSION
        || manifest.canonical_source != CANONICAL_SOURCE
        || manifest.wiki_semantics != WIKI_SEMANTICS
    {
        bail!(
            "refusing to use incompatible knowledge wiki manifest {}",
            manifest_path.display()
        );
    }
    Ok(manifest)
}

impl WikiManifest {
    fn empty() -> Self {
        Self {
            mempal_format: FORMAT_VERSION.to_string(),
            canonical_source: CANONICAL_SOURCE.to_string(),
            wiki_semantics: WIKI_SEMANTICS.to_string(),
            generated_files: Vec::new(),
            pages: Vec::new(),
        }
    }

    fn from_pages(paths: &BTreeSet<String>, pages: &[WikiPage]) -> Self {
        let pages = pages
            .iter()
            .map(|page| WikiManifestPage {
                path: page.path.clone(),
                title: page.title.clone(),
                kind: page.kind.to_string(),
                drawer_refs: page.drawer_refs.clone(),
                triple_refs: page.triple_refs.clone(),
            })
            .collect();
        Self {
            generated_files: paths.iter().cloned().collect(),
            pages,
            ..Self::empty()
        }
    }
}

fn manifest_generated_paths(manifest: &WikiManifest) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    for raw in &manifest.generated_files {
        let normalized = manifest_relative_path(&validate_manifest_relative_path(raw)?);
        if normalized != *raw {
            bail!("knowledge wiki manifest path is not normalized: {raw}");
        }
        paths.insert(raw.clone());
    }
    Ok(paths)
}

fn validate_manifest_relative_path(raw: &str) -> Result<PathBuf> {
    let raw_path = Path::new(raw);
    if raw_path.is_absolute() {
        bail!("knowledge wiki manifest path must be relative: {raw}");
    }
    let mut relative = PathBuf::new();
    for component in raw_path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            _ => bail!("unsafe knowledge wiki manifest path: {raw}"),
        }
    }
    if relative.as_os_str().is_empty() {
        bail!("empty knowledge wiki manifest path");
    }
    Ok(relative)
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
            Ok(_) => bail!(
                "refusing to overwrite unmanaged knowledge wiki file {}; choose an empty directory or remove the colliding file",
                path.display()
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect knowledge wiki path {}", path.display())
                });
            }
        }
    }
    Ok(())
}

fn ensure_existing_parent_dirs_are_safe(output_dir: &Path, relative_path: &Path) -> Result<()> {
    let Some(parent) = relative_path.parent() else {
        return Ok(());
    };
    let mut current = output_dir.to_path_buf();
    for component in parent.components() {
        let Component::Normal(part) = component else {
            bail!(
                "unsafe knowledge wiki path parent: {}",
                relative_path.display()
            );
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "refusing to use symlinked knowledge wiki directory {}",
                    current.display()
                );
            }
            Ok(metadata) if !metadata.file_type().is_dir() => {
                bail!(
                    "refusing to use non-directory knowledge wiki parent {}",
                    current.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect knowledge wiki directory {}",
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
                "unsafe knowledge wiki path parent: {}",
                relative_path.display()
            );
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "refusing to use symlinked knowledge wiki directory {}",
                    current.display()
                );
            }
            Ok(metadata) if !metadata.file_type().is_dir() => {
                bail!(
                    "refusing to use non-directory knowledge wiki parent {}",
                    current.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {
                fs::create_dir(&current).with_context(|| {
                    format!(
                        "failed to create knowledge wiki directory {}",
                        current.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect knowledge wiki directory {}",
                        current.display()
                    )
                });
            }
        }
    }
    Ok(output_dir.join(relative_path))
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
                        "failed to remove symlinked knowledge wiki {}",
                        path.display()
                    )
                })?;
                return create_generated_file(&path, content);
            }
            Ok(metadata) if metadata.file_type().is_dir() => {
                bail!(
                    "refusing to overwrite knowledge wiki directory {}",
                    path.display()
                );
            }
            Ok(_) => {
                fs::write(&path, content).with_context(|| {
                    format!("failed to write knowledge wiki {}", path.display())
                })?;
                return Ok(());
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return create_generated_file(&path, content);
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect knowledge wiki path {}", path.display())
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
                "failed to create knowledge wiki {}; refusing to overwrite unmanaged files",
                path.display()
            )
        })?;
    file.write_all(content)
        .with_context(|| format!("failed to write knowledge wiki {}", path.display()))
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
                    format!("failed to remove stale knowledge wiki {}", path.display())
                })?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect stale knowledge wiki {}", path.display())
                });
            }
        }
    }
    Ok(())
}

fn write_manifest(
    output_dir: &Path,
    generated_paths: &BTreeSet<String>,
    pages: &[WikiPage],
    had_manifest: bool,
) -> Result<()> {
    let manifest_path = output_dir.join(MANIFEST_FILE);
    let manifest = WikiManifest::from_pages(generated_paths, pages);
    let content =
        toml::to_string_pretty(&manifest).context("failed to serialize knowledge wiki manifest")?;
    if had_manifest {
        match fs::symlink_metadata(&manifest_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "refusing to overwrite symlinked knowledge wiki manifest {}",
                    manifest_path.display()
                );
            }
            Ok(metadata) if !metadata.file_type().is_file() => {
                bail!(
                    "refusing to overwrite non-regular knowledge wiki manifest {}",
                    manifest_path.display()
                );
            }
            Ok(_) => {
                fs::write(&manifest_path, content).with_context(|| {
                    format!(
                        "failed to write knowledge wiki manifest {}",
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
                        "failed to inspect knowledge wiki manifest {}",
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

fn manifest_relative_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
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

fn path_component_with_hash_display(slug_value: &str, hash_value: &str) -> String {
    let slug = path_component(slug_value);
    let digest = blake3::hash(hash_value.as_bytes()).to_hex();
    format!("{slug}-{}", &digest[..8])
}

fn markdown_inline(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

fn push_yaml_str(output: &mut String, key: &str, value: &str) {
    output.push_str(key);
    output.push_str(": \"");
    for ch in value.chars() {
        match ch {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            other => output.push(other),
        }
    }
    output.push_str("\"\n");
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs as unix_fs;

    #[cfg(unix)]
    #[test]
    fn write_manifest_refuses_symlinked_existing_manifest_at_final_write() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let output_dir = tmp.path().join("wiki");
        fs::create_dir(&output_dir).expect("create wiki dir");
        let outside = tmp.path().join("outside.toml");
        fs::write(&outside, "outside").expect("write outside target");
        unix_fs::symlink(&outside, output_dir.join(MANIFEST_FILE)).expect("symlink manifest");

        let mut generated_paths = BTreeSet::new();
        generated_paths.insert(README_FILE.to_string());
        let error = write_manifest(&output_dir, &generated_paths, &[], true)
            .expect_err("symlinked manifest must be refused");

        assert!(
            error
                .to_string()
                .contains("refusing to overwrite symlinked knowledge wiki manifest"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(outside).expect("read outside target"),
            "outside"
        );
    }
}
