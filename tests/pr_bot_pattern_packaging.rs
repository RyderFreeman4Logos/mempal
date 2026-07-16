//! #759 — pr-bot PATTERN must ship as tracked packaging, not a gitignored local file.

use std::path::PathBuf;

fn pattern_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/pr-bot/PATTERN.md")
}

#[test]
fn pr_bot_pattern_is_tracked_and_resolvable_from_fresh_tree() {
    let path = pattern_path();
    assert!(
        path.is_file(),
        "missing tracked packaging path {}; executor must not require .claude/PATTERN.md",
        path.display()
    );

    let body = std::fs::read_to_string(&path).expect("read PATTERN.md");
    assert!(
        body.contains("pr-bot"),
        "tracked PATTERN must identify pr-bot workflow"
    );
    assert!(
        body.contains("REVIEW_COMPLETED"),
        "tracked PATTERN must include review precondition marker"
    );
    assert!(
        body.contains("Canonical **tracked**") || body.contains("canonical"),
        "tracked PATTERN must document its own canonical path role"
    );
    // Must explicitly discourage treating gitignored agent-local copies as packaging.
    assert!(
        body.contains(".claude/") && body.contains("not"),
        "tracked PATTERN must warn that .claude copies are not packaging"
    );
}

#[test]
fn pr_bot_readme_documents_canonical_path() {
    let readme = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/pr-bot/README.md");
    assert!(readme.is_file(), "missing {}", readme.display());
    let body = std::fs::read_to_string(&readme).expect("read README");
    assert!(body.contains("assets/pr-bot/PATTERN.md"));
    assert!(body.contains("gitignored") || body.contains("not a deliverable"));
}
