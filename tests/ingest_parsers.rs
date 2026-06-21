use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::Command;

use async_trait::async_trait;
use mempal::core::db::Database;
use mempal::embed::{Embedder, Result as EmbedResult};
use mempal::ingest::parsers::{ParseContext, ParserError, ParserMode, parse_document};
use mempal::ingest::{IngestOptions, ingest_file_with_options};
use tempfile::TempDir;
use zip::write::SimpleFileOptions;

struct StubEmbedder;

#[async_trait]
impl Embedder for StubEmbedder {
    async fn embed(&self, texts: &[&str]) -> EmbedResult<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
    }

    fn dimensions(&self) -> usize {
        3
    }

    fn name(&self) -> &str {
        "stub"
    }
}

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn setup_home() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    Database::open(&mempal_home.join("palace.db")).expect("open db");
    tmp
}

fn write_docx(path: &Path, xml: &str) {
    let file = File::create(path).expect("create docx");
    let mut writer = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    writer
        .start_file("word/document.xml", options)
        .expect("start document xml");
    writer
        .write_all(xml.as_bytes())
        .expect("write document xml");
    writer.finish().expect("finish docx");
}

fn only_active_content(db: &Database) -> String {
    db.conn()
        .query_row(
            "SELECT content FROM drawers WHERE deleted_at IS NULL LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("read drawer content")
}

#[test]
fn auto_parser_selects_markdown_and_jsonl_without_llm() {
    let markdown = parse_document(
        Path::new("note.md"),
        b"# Title\nbody",
        ParseContext {
            mode: ParserMode::Auto,
            allow_llm: false,
        },
    )
    .expect("parse markdown");
    assert_eq!(markdown.parser_id, "builtin:markdown");

    let jsonl = parse_document(
        Path::new("session.jsonl"),
        br#"{"type":"assistant","message":"hello"}"#,
        ParseContext {
            mode: ParserMode::Auto,
            allow_llm: false,
        },
    )
    .expect("parse jsonl");
    assert_eq!(jsonl.parser_id, "builtin:jsonl");
}

#[test]
fn no_llm_policy_rejects_multimodal_auto_parser() {
    let error = parse_document(
        Path::new("screenshot.png"),
        b"\x89PNG\r\n",
        ParseContext {
            mode: ParserMode::Auto,
            allow_llm: false,
        },
    )
    .expect_err("image auto parser must require opt-in");

    assert!(matches!(
        error,
        ParserError::LlmParserRequiresOptIn {
            parser: ParserMode::Vlm,
            ..
        }
    ));
}

#[test]
fn auto_parser_dispatches_pdf_to_text_extractor() {
    let error = parse_document(
        Path::new("paper.pdf"),
        b"not a real pdf",
        ParseContext {
            mode: ParserMode::Auto,
            allow_llm: false,
        },
    )
    .expect_err("invalid PDF should still dispatch to PDF extractor");

    assert!(matches!(error, ParserError::PdfText { .. }));
}

#[test]
fn explicit_llm_parser_requires_then_checks_provider() {
    let path = Path::new("scan.pdf");
    let denied = parse_document(
        path,
        b"not used",
        ParseContext {
            mode: ParserMode::Ocr,
            allow_llm: false,
        },
    )
    .expect_err("OCR parser must require opt-in");
    assert!(matches!(
        denied,
        ParserError::LlmParserRequiresOptIn {
            parser: ParserMode::Ocr,
            ..
        }
    ));

    let unavailable = parse_document(
        path,
        b"not used",
        ParseContext {
            mode: ParserMode::Ocr,
            allow_llm: true,
        },
    )
    .expect_err("OCR parser has no configured provider");
    assert!(matches!(
        unavailable,
        ParserError::LlmParserUnavailable {
            parser: ParserMode::Ocr,
            ..
        }
    ));
}

#[tokio::test]
async fn auto_parser_ingests_office_docx_text() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    let source = tmp.path().join("design.docx");
    write_docx(
        &source,
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Office &amp; document text</w:t></w:r></w:p></w:body></w:document>"#,
    );

    let stats = ingest_file_with_options(
        &db,
        &StubEmbedder,
        &source,
        "docs",
        IngestOptions {
            room: Some("documents"),
            source_root: source.parent(),
            parser: ParserMode::Auto,
            allow_llm_parsers: false,
            ..IngestOptions::default()
        },
    )
    .await
    .expect("ingest docx");

    assert_eq!(stats.files, 1);
    assert_eq!(stats.chunks, 1);
    assert_eq!(only_active_content(&db), "Office & document text");
}

#[test]
fn cli_documents_auto_parser_accepts_no_llm_example() {
    let home = setup_home();
    let source_dir = home.path().join("docs");
    fs::create_dir_all(&source_dir).expect("create docs dir");
    fs::write(
        source_dir.join("note.md"),
        "# Note\nsome deterministic text",
    )
    .expect("write note");

    let output = Command::new(mempal_bin())
        .args([
            "ingest",
            source_dir.to_str().expect("utf8 path"),
            "--wing",
            "docs",
            "--parser",
            "auto",
            "--no-llm",
            "--dry-run",
        ])
        .env("HOME", home.path())
        .output()
        .expect("run mempal ingest");

    assert!(
        output.status.success(),
        "ingest example failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("dry_run=true"), "stdout: {stdout}");
    assert!(stdout.contains("files=1"), "stdout: {stdout}");
    assert!(stdout.contains("chunks=1"), "stdout: {stdout}");
}
