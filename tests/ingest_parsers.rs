use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::Command;

use async_trait::async_trait;
use mempal::core::db::Database;
use mempal::embed::{Embedder, Result as EmbedResult};
use mempal::ingest::parsers::{
    ParseContext, ParserError, ParserMode, ParserResourceLimit, ParserResourceLimits,
    parse_document,
};
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
    write_docx_with_method(
        path,
        &[("word/document.xml".to_string(), xml)],
        zip::CompressionMethod::Stored,
    );
}

fn write_docx_with_method(path: &Path, entries: &[(String, &str)], method: zip::CompressionMethod) {
    let file = File::create(path).expect("create docx");
    let mut writer = zip::ZipWriter::new(file);
    for (name, xml) in entries {
        let options = SimpleFileOptions::default().compression_method(method);
        writer.start_file(name, options).expect("start office xml");
        writer.write_all(xml.as_bytes()).expect("write office xml");
    }
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
fn pdf_parser_rejects_raw_input_over_limit_before_pdf_extract() {
    let limits = ParserResourceLimits::default();
    let oversized_pdf = vec![b'%'; limits.max_pdf_input_bytes + 1];
    let error = parse_document(
        Path::new("paper.pdf"),
        &oversized_pdf,
        ParseContext {
            mode: ParserMode::Auto,
            allow_llm: false,
        },
    )
    .expect_err("oversized PDF should fail before pdf_extract");

    assert!(matches!(
        error,
        ParserError::ResourceLimitExceeded {
            limit: ParserResourceLimit::PdfInputBytes,
            ..
        }
    ));
}

#[test]
fn office_parser_rejects_large_compressed_xml_member() {
    let tmp = TempDir::new().expect("tempdir");
    let source = tmp.path().join("large.docx");
    let limits = ParserResourceLimits::default();
    let xml = format!(
        "<w:document><w:body><w:t>{}</w:t></w:body></w:document>",
        "a".repeat(limits.max_ooxml_xml_member_bytes + 1)
    );
    write_docx_with_method(
        &source,
        &[("word/document.xml".to_string(), &xml)],
        zip::CompressionMethod::Deflated,
    );
    let bytes = fs::read(&source).expect("read docx");

    let error = parse_document(
        &source,
        &bytes,
        ParseContext {
            mode: ParserMode::Auto,
            allow_llm: false,
        },
    )
    .expect_err("large XML member should hit the OOXML member limit");

    assert!(matches!(
        error,
        ParserError::ResourceLimitExceeded {
            limit: ParserResourceLimit::OoxmlXmlMemberBytes,
            ..
        }
    ));
}

#[test]
fn office_parser_rejects_too_many_relevant_entries() {
    let tmp = TempDir::new().expect("tempdir");
    let source = tmp.path().join("many-slides.pptx");
    let limits = ParserResourceLimits::default();
    let xml = "<p:sld><a:t>slide</a:t></p:sld>";
    let entries: Vec<_> = (0..=limits.max_ooxml_archive_entries)
        .map(|index| (format!("ppt/slides/slide{index}.xml"), xml))
        .collect();
    write_docx_with_method(&source, &entries, zip::CompressionMethod::Stored);
    let bytes = fs::read(&source).expect("read pptx");

    let error = parse_document(
        &source,
        &bytes,
        ParseContext {
            mode: ParserMode::Auto,
            allow_llm: false,
        },
    )
    .expect_err("too many OOXML entries should fail safely");

    assert!(matches!(
        error,
        ParserError::ResourceLimitExceeded {
            limit: ParserResourceLimit::OoxmlArchiveEntries,
            ..
        }
    ));
}

#[test]
fn office_parser_rejects_extracted_text_over_limit() {
    let tmp = TempDir::new().expect("tempdir");
    let source = tmp.path().join("too-much-text.docx");
    let limits = ParserResourceLimits::default();
    let xml = format!(
        "<w:document><w:body><w:t>{}</w:t></w:body></w:document>",
        "a".repeat(limits.max_extracted_text_bytes + 1)
    );
    write_docx(&source, &xml);
    let bytes = fs::read(&source).expect("read docx");

    let error = parse_document(
        &source,
        &bytes,
        ParseContext {
            mode: ParserMode::Auto,
            allow_llm: false,
        },
    )
    .expect_err("extracted text cap should fail safely");

    assert!(matches!(
        error,
        ParserError::ResourceLimitExceeded {
            limit: ParserResourceLimit::ExtractedTextBytes,
            ..
        }
    ));
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
