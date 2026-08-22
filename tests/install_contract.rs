use std::fs;

fn repo_file(path: &str) -> String {
    fs::read_to_string(format!("{}/{}", env!("CARGO_MANIFEST_DIR"), path))
        .unwrap_or_else(|error| panic!("read {path}: {error}"))
}

#[test]
fn live_install_contract_keeps_rest_and_recycles_daemon() {
    let justfile = repo_file("justfile");
    let source_installer = repo_file("scripts/install-from-source.sh");
    let post_merge = repo_file("scripts/hooks/post-merge.sh");

    for docs_path in ["README.md", "docs/usage.md"] {
        let docs = repo_file(docs_path);
        assert!(
            docs.contains("cargo install --path . --locked --features rest"),
            "{docs_path} must document a REST-enabled source install"
        );
        assert!(
            !docs.contains("cargo install --path . --locked\n"),
            "{docs_path} must not document a default-feature source install"
        );
    }

    assert!(
        justfile.contains("install --path . --locked --features rest --force --root"),
        "the live install recipe must build REST support"
    );
    assert!(
        source_installer.contains("install --path . --force --locked --features rest --root"),
        "the source installer must build REST support"
    );
    assert!(
        post_merge.contains("just install"),
        "post-merge must use the canonical install recipe"
    );
    assert!(
        post_merge.contains("daemon restart"),
        "post-merge must recycle the daemon after replacing its binary"
    );
    assert!(
        !post_merge.contains("mempal.service"),
        "the contract must not introduce a second systemd unit"
    );
}
