use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use mempal::core::db::Database;
use mempal::core::types::{
    BootstrapEvidenceArgs, Drawer, KnowledgeStatus, MemoryKind, SourceType, Triple,
};
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn setup_home() -> (TempDir, Database, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let mempal_dir = home.join(".mempal");
    fs::create_dir_all(&mempal_dir).expect("create .mempal");
    let db_path = mempal_dir.join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    (tmp, db, home)
}

fn run_mempal(home: &Path, args: &[&str]) -> Output {
    Command::new(mempal_bin())
        .env("HOME", home)
        .args(args)
        .output()
        .expect("run mempal")
}

fn wiki_drawer(id: &str, content: &str, source_file: &str) -> Drawer {
    Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: id.to_string(),
        content: content.to_string(),
        wing: "mempal".to_string(),
        room: Some("wiki".to_string()),
        source_file: Some(source_file.to_string()),
        source_type: SourceType::AgentObservation,
        added_at: "2026-06-19T12:00:00Z".to_string(),
        chunk_index: Some(0),
        importance: 4,
    })
}

fn decision_drawer(id: &str, statement: &str, content: &str) -> Drawer {
    let mut drawer = wiki_drawer(id, content, &format!("tests://wiki/{id}.md"));
    drawer.memory_kind = MemoryKind::Decision;
    drawer.statement = Some(statement.to_string());
    drawer.status = Some(KnowledgeStatus::Active);
    drawer
}

fn seed_wiki_fixture(db: &Database) {
    let source = wiki_drawer(
        "drawer_entity_source",
        "Alice chose SQLite for the canonical memory store.",
        "tests://wiki/entity-source.md",
    );
    db.insert_drawer_with_project(&source, Some("proj-wiki"))
        .expect("insert source");

    db.insert_triple(&Triple {
        id: "triple_alice_sqlite".to_string(),
        subject: "Alice".to_string(),
        predicate: "chose".to_string(),
        object: "SQLite".to_string(),
        valid_from: Some("2026-06-19T12:00:00Z".to_string()),
        valid_to: None,
        confidence: 0.9,
        source_drawer: Some("drawer_entity_source".to_string()),
    })
    .expect("insert triple");

    let mut old = decision_drawer(
        "drawer_old_decision",
        "Use Markdown as canonical memory.",
        "Old decision kept for superseded citation.",
    );
    old.status = Some(KnowledgeStatus::Superseded);
    db.insert_drawer_with_project_validity(&old, Some("proj-wiki"), None, None, Some("1"))
        .expect("insert old decision");
    db.soft_delete_drawer("drawer_old_decision")
        .expect("soft delete old decision");

    let mut decision = decision_drawer(
        "drawer_decision",
        "Keep SQLite canonical and generate wiki pages as derived views.",
        "The wiki is an index view. It cites drawer refs and is never imported back as truth.",
    );
    decision.supporting_refs = vec!["drawer_entity_source".to_string()];
    decision.supersedes = Some("drawer_old_decision".to_string());
    db.insert_drawer_with_project(&decision, Some("proj-wiki"))
        .expect("insert decision");
}

fn slug_with_hash(value: &str) -> String {
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
    let trimmed = component.trim_matches(['_', '.']);
    let slug = if trimmed.is_empty() { "none" } else { trimmed };
    format!("{slug}-{}", &blake3::hash(value.as_bytes()).to_hex()[..8])
}

fn read_decision_page(out_dir: &Path) -> String {
    let decisions_dir = out_dir.join("decisions");
    let entries = fs::read_dir(decisions_dir).expect("read decisions dir");
    for entry in entries {
        let path = entry.expect("entry").path();
        let markdown = fs::read_to_string(&path).expect("read decision page");
        if markdown.contains("Keep SQLite canonical") {
            return markdown;
        }
    }
    panic!("decision page not found");
}

fn markdown_section<'a>(markdown: &'a str, heading: &str) -> &'a str {
    let start = markdown.find(heading).expect("section heading");
    let rest = &markdown[start + heading.len()..];
    let end = rest.find("\n## ").unwrap_or(rest.len());
    &rest[..end]
}

fn read_all_generated_text(out_dir: &Path) -> String {
    let mut output = String::new();
    for relative in ["README.md", "index.md", ".mempal-wiki.toml"] {
        output.push_str(&fs::read_to_string(out_dir.join(relative)).expect("read generated file"));
        output.push('\n');
    }
    for directory in ["entities", "decisions"] {
        for entry in fs::read_dir(out_dir.join(directory)).expect("read generated dir") {
            output.push_str(
                &fs::read_to_string(entry.expect("entry").path()).expect("read generated page"),
            );
            output.push('\n');
        }
    }
    output
}

#[test]
fn wiki_build_writes_source_backed_entity_and_decision_pages() {
    let (_tmp, db, home) = setup_home();
    seed_wiki_fixture(&db);
    drop(db);

    let out_dir = home.join("wiki");
    let output = run_mempal(
        &home,
        &[
            "wiki",
            "build",
            out_dir.to_str().expect("utf8 path"),
            "--project",
            "proj-wiki",
            "--now",
            "2000000000",
        ],
    );
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("wiki_semantics: generated_read_only"),
        "{stdout}"
    );
    assert!(
        stdout.contains("SQLite remains canonical; wiki pages are generated read-only views"),
        "{stdout}"
    );

    let readme = fs::read_to_string(out_dir.join("README.md")).expect("read wiki readme");
    assert!(readme.contains("derived views, not editable source data"));
    assert!(readme.contains("Wiki import/update sync is intentionally not implemented"));

    let alice_page = out_dir
        .join("entities")
        .join(format!("{}.md", slug_with_hash("Alice")));
    let alice = fs::read_to_string(alice_page).expect("read Alice page");
    assert!(alice.contains("page_kind: \"entity\""));
    assert!(alice.contains("## Active Claims"));
    assert!(alice.contains("`Alice` --`chose`--> `SQLite`"), "{alice}");
    assert!(alice.contains("`triple:triple_alice_sqlite`"), "{alice}");
    assert!(alice.contains("`drawer:drawer_entity_source`"), "{alice}");
    assert!(alice.contains("tests://wiki/entity-source.md"), "{alice}");
    assert!(alice.contains("## Superseded Claims"));
    assert!(alice.contains("## Open Questions"));

    let decision = read_decision_page(&out_dir);
    assert!(decision.contains("page_kind: \"decision\""));
    assert!(decision.contains("## Active Claims"));
    assert!(decision.contains("`drawer:drawer_decision`"), "{decision}");
    assert!(decision.contains("supporting_refs:"), "{decision}");
    assert!(
        decision.contains("`drawer:drawer_entity_source`"),
        "{decision}"
    );
    assert!(decision.contains("## Superseded Claims"));
    assert!(
        decision.contains("`drawer:drawer_old_decision`"),
        "{decision}"
    );
    assert!(decision.contains("No source-backed open questions were found"));

    let manifest = fs::read_to_string(out_dir.join(".mempal-wiki.toml")).expect("read manifest");
    assert!(manifest.contains("mempal_format = \"knowledge_wiki_v1\""));
    assert!(manifest.contains("wiki_semantics = \"generated_read_only\""));
    assert!(manifest.contains("drawer_entity_source"));

    let verify = run_mempal(
        &home,
        &[
            "wiki",
            "verify",
            out_dir.to_str().expect("utf8 path"),
            "--now",
            "2000000000",
        ],
    );
    assert!(
        verify.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr)
    );
    assert!(
        String::from_utf8_lossy(&verify.stdout).contains("wiki verification clean"),
        "{}",
        String::from_utf8_lossy(&verify.stdout)
    );
}

#[test]
fn wiki_build_omits_soft_deleted_supporting_drawers_from_active_refs() {
    let (_tmp, db, home) = setup_home();
    seed_wiki_fixture(&db);
    assert!(
        db.soft_delete_drawer("drawer_entity_source")
            .expect("soft delete source")
    );
    drop(db);

    let out_dir = home.join("wiki");
    let build = run_mempal(
        &home,
        &[
            "wiki",
            "build",
            out_dir.to_str().expect("utf8 path"),
            "--project",
            "proj-wiki",
            "--now",
            "2000000000",
        ],
    );
    assert!(
        build.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );

    let alice_page = out_dir
        .join("entities")
        .join(format!("{}.md", slug_with_hash("Alice")));
    let alice = fs::read_to_string(alice_page).expect("read Alice page");
    assert!(
        alice.contains("No source-backed active claims were found"),
        "{alice}"
    );
    assert!(
        alice.contains("source drawer is deleted"),
        "soft-deleted source should be marked omitted: {alice}"
    );
    assert!(
        !alice.contains("`Alice` --`chose`--> `SQLite`"),
        "soft-deleted source must not back an active claim: {alice}"
    );
    assert!(
        !alice.contains("`drawer:drawer_entity_source`"),
        "soft-deleted source must not be emitted as an active citation: {alice}"
    );

    let decision = read_decision_page(&out_dir);
    assert!(
        decision
            .contains("stale `drawer:drawer_entity_source` (not cited; source drawer is deleted)"),
        "{decision}"
    );

    let manifest = fs::read_to_string(out_dir.join(".mempal-wiki.toml")).expect("read manifest");
    assert!(
        !manifest.contains("drawer_entity_source"),
        "soft-deleted supporting drawer must not be an active manifest ref: {manifest}"
    );

    let verify = run_mempal(
        &home,
        &[
            "wiki",
            "verify",
            out_dir.to_str().expect("utf8 path"),
            "--now",
            "2000000000",
        ],
    );
    assert!(
        verify.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr)
    );
}

#[test]
fn wiki_verify_flags_changed_and_expired_supporting_refs() {
    let (_tmp, db, home) = setup_home();
    seed_wiki_fixture(&db);
    drop(db);

    let out_dir = home.join("wiki");
    let build = run_mempal(
        &home,
        &[
            "wiki",
            "build",
            out_dir.to_str().expect("utf8 path"),
            "--project",
            "proj-wiki",
            "--now",
            "2",
        ],
    );
    assert!(
        build.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );

    let db = Database::open(&home.join(".mempal").join("palace.db")).expect("reopen db");
    db.conn()
        .execute(
            "UPDATE drawers SET updated_at = ?2, valid_until = ?3 WHERE id = ?1",
            ("drawer_entity_source", "2026-06-20T12:00:00Z", "1"),
        )
        .expect("expire supporting drawer");
    db.conn()
        .execute(
            "UPDATE triples SET valid_to = ?2 WHERE id = ?1",
            ("triple_alice_sqlite", "1"),
        )
        .expect("expire triple");
    assert!(
        db.soft_delete_drawer("drawer_entity_source")
            .expect("soft delete supporting drawer")
    );
    drop(db);

    let verify = run_mempal(
        &home,
        &[
            "wiki",
            "verify",
            out_dir.to_str().expect("utf8 path"),
            "--now",
            "2",
        ],
    );
    assert!(!verify.status.success());
    let stdout = String::from_utf8_lossy(&verify.stdout);
    let stderr = String::from_utf8_lossy(&verify.stderr);
    assert!(
        stdout.contains("stale_refs:"),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(stdout.contains("drawer_entity_source"), "{stdout}");
    assert!(stdout.contains("updated_at changed"), "{stdout}");
    assert!(stdout.contains("drawer validity expired"), "{stdout}");
    assert!(stdout.contains("drawer was deleted"), "{stdout}");
    assert!(stdout.contains("triple_alice_sqlite"), "{stdout}");
    assert!(stdout.contains("valid_to changed"), "{stdout}");
    assert!(
        stderr.contains("knowledge wiki verification failed"),
        "stdout={stdout}\nstderr={stderr}"
    );
}

#[test]
fn wiki_verify_flags_changed_triple_claim_text() {
    let (_tmp, db, home) = setup_home();
    seed_wiki_fixture(&db);
    drop(db);

    let out_dir = home.join("wiki");
    let build = run_mempal(
        &home,
        &[
            "wiki",
            "build",
            out_dir.to_str().expect("utf8 path"),
            "--project",
            "proj-wiki",
            "--now",
            "2000000000",
        ],
    );
    assert!(
        build.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );

    let db = Database::open(&home.join(".mempal").join("palace.db")).expect("reopen db");
    db.conn()
        .execute(
            "UPDATE triples SET object = ?2 WHERE id = ?1",
            ("triple_alice_sqlite", "Postgres"),
        )
        .expect("change triple object");
    drop(db);

    let verify = run_mempal(
        &home,
        &[
            "wiki",
            "verify",
            out_dir.to_str().expect("utf8 path"),
            "--now",
            "2000000000",
        ],
    );
    assert!(!verify.status.success());
    let stdout = String::from_utf8_lossy(&verify.stdout);
    let stderr = String::from_utf8_lossy(&verify.stderr);
    assert!(
        stdout.contains("triple_alice_sqlite") && stdout.contains("claim content changed"),
        "stdout={stdout}\nstderr={stderr}"
    );
}

#[test]
fn wiki_build_classifies_expired_decision_drawer_as_superseded() {
    let (_tmp, db, home) = setup_home();
    seed_wiki_fixture(&db);
    db.conn()
        .execute(
            "UPDATE drawers SET valid_until = ?2 WHERE id = ?1",
            ("drawer_decision", "1"),
        )
        .expect("expire decision drawer");
    drop(db);

    let out_dir = home.join("wiki");
    let build = run_mempal(
        &home,
        &[
            "wiki",
            "build",
            out_dir.to_str().expect("utf8 path"),
            "--project",
            "proj-wiki",
            "--now",
            "2",
        ],
    );
    assert!(
        build.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );

    let decision = read_decision_page(&out_dir);
    let active = markdown_section(&decision, "## Active Claims");
    let superseded = markdown_section(&decision, "## Superseded Claims");
    assert!(
        active.contains("No source-backed active claims were found"),
        "{decision}"
    );
    assert!(
        !active.contains("`drawer:drawer_decision`"),
        "expired decision must not be cited as active: {decision}"
    );
    assert!(
        superseded.contains("`drawer:drawer_decision`"),
        "expired decision should remain visible as a superseded claim: {decision}"
    );
}

#[test]
fn wiki_build_redacts_secret_like_values_by_default() {
    let (_tmp, db, home) = setup_home();
    let secret = "sk-abcdefghijklmnopqrstuvwxyz1234567890";
    let source = wiki_drawer(
        "drawer_secret_source",
        &format!("Alice used api_key={secret} for a local test."),
        &format!("tests://wiki/source?auth_token={secret}.md"),
    );
    db.insert_drawer_with_project(&source, Some("proj-wiki"))
        .expect("insert source");
    db.insert_triple(&Triple {
        id: "triple_secret_source".to_string(),
        subject: format!("Alice api_key={secret}"),
        predicate: "stores".to_string(),
        object: "SQLite".to_string(),
        valid_from: Some("2026-06-19T12:00:00Z".to_string()),
        valid_to: None,
        confidence: 0.9,
        source_drawer: Some("drawer_secret_source".to_string()),
    })
    .expect("insert triple");

    let mut decision = decision_drawer(
        "drawer_secret_decision",
        &format!("Keep secret={secret} out of the generated wiki."),
        &format!("Authorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==\nsecret={secret}"),
    );
    decision.supporting_refs = vec!["drawer_secret_source".to_string()];
    db.insert_drawer_with_project(&decision, Some("proj-wiki"))
        .expect("insert decision");
    drop(db);

    let out_dir = home.join("wiki");
    let build = run_mempal(
        &home,
        &[
            "wiki",
            "build",
            out_dir.to_str().expect("utf8 path"),
            "--project",
            "proj-wiki",
            "--now",
            "2000000000",
        ],
    );
    assert!(
        build.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    let stdout = String::from_utf8_lossy(&build.stdout);
    assert!(stdout.contains("redaction: enabled"), "{stdout}");

    let generated = read_all_generated_text(&out_dir);
    assert!(
        !generated.contains(secret),
        "generated wiki leaked secret: {generated}"
    );
    assert!(
        !generated.contains("Authorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="),
        "generated wiki leaked basic auth: {generated}"
    );
    assert!(
        !generated.contains("auth_token="),
        "generated wiki leaked source_file token key: {generated}"
    );
    assert!(
        generated.contains("[REDACTED:"),
        "generated wiki should show redaction markers: {generated}"
    );
}

#[test]
fn wiki_help_explains_derived_noncanonical_semantics() {
    let (_tmp, _db, home) = setup_home();
    let output = run_mempal(&home, &["wiki", "build", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("derived read-only view"), "{stdout}");
    assert!(stdout.contains("not a second source of truth"), "{stdout}");
    assert!(stdout.contains("drawer/triple citations"), "{stdout}");
    assert!(
        stdout.contains("redact secret-like values by default"),
        "{stdout}"
    );
    assert!(stdout.contains("--no-redact"), "{stdout}");
}
