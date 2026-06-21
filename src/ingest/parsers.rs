//! Plugin boundary for deterministic document parsing before normalization.

use std::fmt;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::{Reader, escape};
use thiserror::Error;
use zip::ZipArchive;

use super::detect::{Format, detect_format};

/// Resource limits for deterministic in-process parsers.
///
/// These defaults keep plugin parsing bounded while still allowing ordinary
/// notes, source files, and Office documents to ingest without config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParserResourceLimits {
    pub max_deterministic_input_bytes: usize,
    pub max_pdf_input_bytes: usize,
    pub max_ooxml_archive_entries: usize,
    pub max_ooxml_xml_member_bytes: usize,
    pub max_ooxml_xml_document_bytes: usize,
    pub max_extracted_text_bytes: usize,
    pub max_extracted_text_fragments: usize,
    pub max_xml_nodes: usize,
}

impl Default for ParserResourceLimits {
    fn default() -> Self {
        Self {
            max_deterministic_input_bytes: 64 * 1024 * 1024,
            max_pdf_input_bytes: 8 * 1024 * 1024,
            max_ooxml_archive_entries: 1024,
            max_ooxml_xml_member_bytes: 2 * 1024 * 1024,
            max_ooxml_xml_document_bytes: 8 * 1024 * 1024,
            max_extracted_text_bytes: 1024 * 1024,
            max_extracted_text_fragments: 16_384,
            max_xml_nodes: 131_072,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserResourceLimit {
    DeterministicInputBytes,
    PdfInputBytes,
    OoxmlArchiveEntries,
    OoxmlXmlMemberBytes,
    OoxmlXmlDocumentBytes,
    ExtractedTextBytes,
    ExtractedTextFragments,
    XmlNodeCount,
}

impl fmt::Display for ParserResourceLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DeterministicInputBytes => "deterministic_input_bytes",
            Self::PdfInputBytes => "pdf_input_bytes",
            Self::OoxmlArchiveEntries => "ooxml_archive_entries",
            Self::OoxmlXmlMemberBytes => "ooxml_xml_member_bytes",
            Self::OoxmlXmlDocumentBytes => "ooxml_xml_document_bytes",
            Self::ExtractedTextBytes => "extracted_text_bytes",
            Self::ExtractedTextFragments => "extracted_text_fragments",
            Self::XmlNodeCount => "xml_node_count",
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ParserMode {
    #[default]
    Auto,
    Text,
    Markdown,
    Code,
    Jsonl,
    Pdf,
    Office,
    Ocr,
    Vlm,
    MmLlm,
}

impl ParserMode {
    pub fn is_auto(self) -> bool {
        self == Self::Auto
    }

    pub fn requires_llm(self) -> bool {
        matches!(self, Self::Ocr | Self::Vlm | Self::MmLlm)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Text => "text",
            Self::Markdown => "markdown",
            Self::Code => "code",
            Self::Jsonl => "jsonl",
            Self::Pdf => "pdf",
            Self::Office => "office",
            Self::Ocr => "ocr",
            Self::Vlm => "vlm",
            Self::MmLlm => "mm-llm",
        }
    }
}

impl fmt::Display for ParserMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseContext {
    pub mode: ParserMode,
    pub allow_llm: bool,
}

impl Default for ParseContext {
    fn default() -> Self {
        Self {
            mode: ParserMode::Auto,
            allow_llm: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDocument {
    pub content: String,
    pub format: Format,
    pub parser_id: &'static str,
}

#[derive(Debug, Error)]
pub enum ParserError {
    #[error("parser `{parser}` requires explicit LLM/VLM opt-in for {path}")]
    LlmParserRequiresOptIn { parser: ParserMode, path: PathBuf },
    #[error("parser `{parser}` is not configured for {path}")]
    LlmParserUnavailable { parser: ParserMode, path: PathBuf },
    #[error("unsupported parser `{parser}` for {path}")]
    UnsupportedParser { parser: ParserMode, path: PathBuf },
    #[error("deterministic parser `{parser}` is disabled for {path}: {reason}")]
    UnsafeDeterministicParser {
        parser: ParserMode,
        path: PathBuf,
        reason: &'static str,
    },
    #[error("parser resource limit `{limit}` exceeded for {path}: actual={actual}, max={max}")]
    ResourceLimitExceeded {
        path: PathBuf,
        limit: ParserResourceLimit,
        actual: u64,
        max: u64,
    },
    #[error("failed to read Office archive {path}")]
    OfficeZip {
        path: PathBuf,
        #[source]
        source: zip::result::ZipError,
    },
    #[error("failed to read Office XML member from {path}")]
    OfficeIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse Office XML from {path}")]
    OfficeXml {
        path: PathBuf,
        #[source]
        source: quick_xml::Error,
    },
    #[error("failed to decode Office XML text from {path}")]
    OfficeXmlDecode {
        path: PathBuf,
        #[source]
        source: quick_xml::encoding::EncodingError,
    },
    #[error("failed to unescape Office XML text from {path}")]
    OfficeXmlEscape {
        path: PathBuf,
        #[source]
        source: escape::EscapeError,
    },
    #[error("Office document {path} contains no extractable text")]
    EmptyOfficeText { path: PathBuf },
}

pub trait DocumentParser: Sync {
    fn id(&self) -> &'static str;
    fn mode(&self) -> ParserMode;
    fn supports_path(&self, path: &Path) -> bool;
    fn parse(&self, input: ParserInput<'_>) -> Result<ParsedDocument, ParserError>;
}

#[derive(Debug, Clone, Copy)]
pub struct ParserInput<'a> {
    pub path: &'a Path,
    pub bytes: &'a [u8],
}

pub struct ParserRegistry {
    parsers: &'static [&'static dyn DocumentParser],
}

impl ParserRegistry {
    pub const fn new(parsers: &'static [&'static dyn DocumentParser]) -> Self {
        Self { parsers }
    }

    pub fn builtin() -> Self {
        Self::new(BUILTIN_PARSERS)
    }

    pub fn parse(
        &self,
        path: &Path,
        bytes: &[u8],
        context: ParseContext,
    ) -> Result<ParsedDocument, ParserError> {
        if let Some(parser) = llm_parser_for_path(path, context.mode) {
            return reject_or_defer_llm_parser(parser, path, context.allow_llm);
        }

        let Some(parser) = self.select(path, context.mode) else {
            return Err(ParserError::UnsupportedParser {
                parser: context.mode,
                path: path.to_path_buf(),
            });
        };
        parser.parse(ParserInput { path, bytes })
    }

    fn select(&self, path: &Path, mode: ParserMode) -> Option<&'static dyn DocumentParser> {
        if mode.is_auto() {
            return self
                .parsers
                .iter()
                .copied()
                .find(|parser| parser.mode() != ParserMode::Text && parser.supports_path(path))
                .or_else(|| {
                    self.parsers
                        .iter()
                        .copied()
                        .find(|parser| parser.mode() == ParserMode::Text)
                });
        }

        self.parsers
            .iter()
            .copied()
            .find(|parser| parser.mode() == mode)
    }
}

pub fn parse_document(
    path: &Path,
    bytes: &[u8],
    context: ParseContext,
) -> Result<ParsedDocument, ParserError> {
    ParserRegistry::builtin().parse(path, bytes, context)
}

struct TextParser;
struct MarkdownParser;
struct CodeParser;
struct JsonlParser;
struct PdfParser;
struct OfficeParser;

static TEXT_PARSER: TextParser = TextParser;
static MARKDOWN_PARSER: MarkdownParser = MarkdownParser;
static CODE_PARSER: CodeParser = CodeParser;
static JSONL_PARSER: JsonlParser = JsonlParser;
static PDF_PARSER: PdfParser = PdfParser;
static OFFICE_PARSER: OfficeParser = OfficeParser;

static BUILTIN_PARSERS: &[&dyn DocumentParser] = &[
    &MARKDOWN_PARSER,
    &CODE_PARSER,
    &JSONL_PARSER,
    &PDF_PARSER,
    &OFFICE_PARSER,
    &TEXT_PARSER,
];

impl DocumentParser for TextParser {
    fn id(&self) -> &'static str {
        "builtin:text"
    }

    fn mode(&self) -> ParserMode {
        ParserMode::Text
    }

    fn supports_path(&self, _path: &Path) -> bool {
        true
    }

    fn parse(&self, input: ParserInput<'_>) -> Result<ParsedDocument, ParserError> {
        ensure_input_within_limit(
            input.path,
            input.bytes.len(),
            ParserResourceLimit::DeterministicInputBytes,
            ParserResourceLimits::default().max_deterministic_input_bytes,
        )?;
        let content = String::from_utf8_lossy(input.bytes).to_string();
        Ok(ParsedDocument {
            format: detect_format(&content),
            content,
            parser_id: self.id(),
        })
    }
}

impl DocumentParser for MarkdownParser {
    fn id(&self) -> &'static str {
        "builtin:markdown"
    }

    fn mode(&self) -> ParserMode {
        ParserMode::Markdown
    }

    fn supports_path(&self, path: &Path) -> bool {
        matches_extension(path, &["md", "markdown", "mdown", "mkd"])
    }

    fn parse(&self, input: ParserInput<'_>) -> Result<ParsedDocument, ParserError> {
        ensure_input_within_limit(
            input.path,
            input.bytes.len(),
            ParserResourceLimit::DeterministicInputBytes,
            ParserResourceLimits::default().max_deterministic_input_bytes,
        )?;
        let content = String::from_utf8_lossy(input.bytes).to_string();
        Ok(ParsedDocument {
            content,
            format: Format::PlainText,
            parser_id: self.id(),
        })
    }
}

impl DocumentParser for CodeParser {
    fn id(&self) -> &'static str {
        "builtin:code"
    }

    fn mode(&self) -> ParserMode {
        ParserMode::Code
    }

    fn supports_path(&self, path: &Path) -> bool {
        matches_extension(
            path,
            &[
                "rs", "go", "py", "ts", "tsx", "js", "jsx", "java", "kt", "kts", "c", "h", "cpp",
                "hpp", "cc", "cs", "rb", "php", "swift", "scala", "sh", "bash", "zsh", "fish",
                "toml", "yaml", "yml", "json", "proto", "sql", "html", "css",
            ],
        )
    }

    fn parse(&self, input: ParserInput<'_>) -> Result<ParsedDocument, ParserError> {
        ensure_input_within_limit(
            input.path,
            input.bytes.len(),
            ParserResourceLimit::DeterministicInputBytes,
            ParserResourceLimits::default().max_deterministic_input_bytes,
        )?;
        let content = String::from_utf8_lossy(input.bytes).to_string();
        let format = detect_format(&content);
        Ok(ParsedDocument {
            content,
            format,
            parser_id: self.id(),
        })
    }
}

impl DocumentParser for JsonlParser {
    fn id(&self) -> &'static str {
        "builtin:jsonl"
    }

    fn mode(&self) -> ParserMode {
        ParserMode::Jsonl
    }

    fn supports_path(&self, path: &Path) -> bool {
        matches_extension(path, &["jsonl"])
    }

    fn parse(&self, input: ParserInput<'_>) -> Result<ParsedDocument, ParserError> {
        ensure_input_within_limit(
            input.path,
            input.bytes.len(),
            ParserResourceLimit::DeterministicInputBytes,
            ParserResourceLimits::default().max_deterministic_input_bytes,
        )?;
        let content = String::from_utf8_lossy(input.bytes).to_string();
        Ok(ParsedDocument {
            format: detect_format(&content),
            content,
            parser_id: self.id(),
        })
    }
}

impl DocumentParser for PdfParser {
    fn id(&self) -> &'static str {
        "builtin:pdf-text"
    }

    fn mode(&self) -> ParserMode {
        ParserMode::Pdf
    }

    fn supports_path(&self, path: &Path) -> bool {
        matches_extension(path, &["pdf"])
    }

    fn parse(&self, input: ParserInput<'_>) -> Result<ParsedDocument, ParserError> {
        ensure_input_within_limit(
            input.path,
            input.bytes.len(),
            ParserResourceLimit::PdfInputBytes,
            ParserResourceLimits::default().max_pdf_input_bytes,
        )?;
        Err(ParserError::UnsafeDeterministicParser {
            parser: ParserMode::Pdf,
            path: input.path.to_path_buf(),
            reason: "no bounded in-process PDF extractor is available; use an explicit LLM/OCR parser when configured",
        })
    }
}

impl DocumentParser for OfficeParser {
    fn id(&self) -> &'static str {
        "builtin:office-ooxml"
    }

    fn mode(&self) -> ParserMode {
        ParserMode::Office
    }

    fn supports_path(&self, path: &Path) -> bool {
        matches_extension(path, &["docx", "pptx", "xlsx"])
    }

    fn parse(&self, input: ParserInput<'_>) -> Result<ParsedDocument, ParserError> {
        ensure_input_within_limit(
            input.path,
            input.bytes.len(),
            ParserResourceLimit::DeterministicInputBytes,
            ParserResourceLimits::default().max_deterministic_input_bytes,
        )?;
        let content = extract_office_text(input.path, input.bytes)?;
        Ok(ParsedDocument {
            content,
            format: Format::PlainText,
            parser_id: self.id(),
        })
    }
}

fn extract_office_text(path: &Path, bytes: &[u8]) -> Result<String, ParserError> {
    let limits = ParserResourceLimits::default();
    let cursor = Cursor::new(bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|source| ParserError::OfficeZip {
        path: path.to_path_buf(),
        source,
    })?;
    ensure_input_within_limit(
        path,
        archive.len(),
        ParserResourceLimit::OoxmlArchiveEntries,
        limits.max_ooxml_archive_entries,
    )?;
    let mut content = BoundedText::default();
    let mut budget = OoxmlDocumentBudget::default();

    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|source| ParserError::OfficeZip {
                path: path.to_path_buf(),
                source,
            })?;
        let name = file.name().to_string();
        if !is_office_text_xml(&name) {
            continue;
        }

        let uncompressed_size = file.size();
        budget.ensure_next_xml_member(path, uncompressed_size, &limits)?;
        let xml = read_ooxml_member(path, &mut file, uncompressed_size, &limits)?;
        budget.add_xml_bytes(path, xml.len() as u64, &limits)?;
        extract_xml_text(path, &xml, &limits, &mut budget, &mut content)?;
    }

    let content = content.into_string();
    if content.trim().is_empty() {
        return Err(ParserError::EmptyOfficeText {
            path: path.to_path_buf(),
        });
    }
    Ok(content)
}

fn read_ooxml_member<R: Read>(
    path: &Path,
    reader: &mut R,
    uncompressed_size: u64,
    limits: &ParserResourceLimits,
) -> Result<String, ParserError> {
    ensure_resource_limit(
        path,
        uncompressed_size,
        ParserResourceLimit::OoxmlXmlMemberBytes,
        limits.max_ooxml_xml_member_bytes as u64,
    )?;

    let mut buffer = Vec::new();
    reader
        .take((limits.max_ooxml_xml_member_bytes as u64).saturating_add(1))
        .read_to_end(&mut buffer)
        .map_err(|source| ParserError::OfficeIo {
            path: path.to_path_buf(),
            source,
        })?;
    ensure_input_within_limit(
        path,
        buffer.len(),
        ParserResourceLimit::OoxmlXmlMemberBytes,
        limits.max_ooxml_xml_member_bytes,
    )?;

    String::from_utf8(buffer).map_err(|source| ParserError::OfficeIo {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
    })
}

fn extract_xml_text(
    path: &Path,
    xml: &str,
    limits: &ParserResourceLimits,
    budget: &mut OoxmlDocumentBudget,
    output: &mut BoundedText,
) -> Result<(), ParserError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut member_has_text = false;

    loop {
        budget.add_xml_node(path, limits)?;
        match reader.read_event() {
            Ok(Event::Text(text)) => {
                let decoded = text
                    .decode()
                    .map_err(|source| ParserError::OfficeXmlDecode {
                        path: path.to_path_buf(),
                        source,
                    })?;
                let unescaped =
                    escape::unescape(&decoded).map_err(|source| ParserError::OfficeXmlEscape {
                        path: path.to_path_buf(),
                        source,
                    })?;
                let value = unescaped.trim();
                if !value.is_empty() {
                    output.push_fragment(path, value, &mut member_has_text, limits)?;
                }
            }
            Ok(Event::CData(text)) => {
                let decoded = text
                    .decode()
                    .map_err(|source| ParserError::OfficeXmlDecode {
                        path: path.to_path_buf(),
                        source,
                    })?;
                let value = decoded.trim();
                if !value.is_empty() {
                    output.push_fragment(path, value, &mut member_has_text, limits)?;
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                if let Some(value) = resolve_general_ref(path, &reference)? {
                    output.push_fragment(path, &value, &mut member_has_text, limits)?;
                }
            }
            Ok(Event::Eof) => break,
            Err(source) => {
                return Err(ParserError::OfficeXml {
                    path: path.to_path_buf(),
                    source,
                });
            }
            _ => {}
        }
    }

    Ok(())
}

#[derive(Debug, Default)]
struct OoxmlDocumentBudget {
    xml_bytes: u64,
    xml_nodes: u64,
}

impl OoxmlDocumentBudget {
    fn ensure_next_xml_member(
        &self,
        path: &Path,
        uncompressed_size: u64,
        limits: &ParserResourceLimits,
    ) -> Result<(), ParserError> {
        ensure_resource_limit(
            path,
            self.xml_bytes.saturating_add(uncompressed_size),
            ParserResourceLimit::OoxmlXmlDocumentBytes,
            limits.max_ooxml_xml_document_bytes as u64,
        )
    }

    fn add_xml_bytes(
        &mut self,
        path: &Path,
        bytes: u64,
        limits: &ParserResourceLimits,
    ) -> Result<(), ParserError> {
        self.xml_bytes = self.xml_bytes.saturating_add(bytes);
        ensure_resource_limit(
            path,
            self.xml_bytes,
            ParserResourceLimit::OoxmlXmlDocumentBytes,
            limits.max_ooxml_xml_document_bytes as u64,
        )
    }

    fn add_xml_node(
        &mut self,
        path: &Path,
        limits: &ParserResourceLimits,
    ) -> Result<(), ParserError> {
        self.xml_nodes = self.xml_nodes.saturating_add(1);
        ensure_resource_limit(
            path,
            self.xml_nodes,
            ParserResourceLimit::XmlNodeCount,
            limits.max_xml_nodes as u64,
        )
    }
}

#[derive(Debug, Default)]
struct BoundedText {
    content: String,
    fragments: usize,
}

impl BoundedText {
    fn push_fragment(
        &mut self,
        path: &Path,
        value: &str,
        member_has_text: &mut bool,
        limits: &ParserResourceLimits,
    ) -> Result<(), ParserError> {
        let separator = if self.content.is_empty() {
            ""
        } else if *member_has_text {
            " "
        } else {
            "\n\n"
        };
        ensure_input_within_limit(
            path,
            self.fragments.saturating_add(1),
            ParserResourceLimit::ExtractedTextFragments,
            limits.max_extracted_text_fragments,
        )?;
        let next_bytes = self
            .content
            .len()
            .saturating_add(separator.len())
            .saturating_add(value.len());
        ensure_input_within_limit(
            path,
            next_bytes,
            ParserResourceLimit::ExtractedTextBytes,
            limits.max_extracted_text_bytes,
        )?;

        self.content.push_str(separator);
        self.content.push_str(value);
        self.fragments = self.fragments.saturating_add(1);
        *member_has_text = true;
        Ok(())
    }

    fn into_string(self) -> String {
        self.content
    }
}

fn ensure_input_within_limit(
    path: &Path,
    actual: usize,
    limit: ParserResourceLimit,
    max: usize,
) -> Result<(), ParserError> {
    ensure_resource_limit(path, actual as u64, limit, max as u64)
}

fn ensure_resource_limit(
    path: &Path,
    actual: u64,
    limit: ParserResourceLimit,
    max: u64,
) -> Result<(), ParserError> {
    if actual <= max {
        return Ok(());
    }
    Err(ParserError::ResourceLimitExceeded {
        path: path.to_path_buf(),
        limit,
        actual,
        max,
    })
}

fn resolve_general_ref(
    path: &Path,
    reference: &quick_xml::events::BytesRef<'_>,
) -> Result<Option<String>, ParserError> {
    if let Some(character) =
        reference
            .resolve_char_ref()
            .map_err(|source| ParserError::OfficeXml {
                path: path.to_path_buf(),
                source,
            })?
    {
        return Ok(Some(character.to_string()));
    }

    let decoded = reference
        .decode()
        .map_err(|source| ParserError::OfficeXmlDecode {
            path: path.to_path_buf(),
            source,
        })?;
    let value = match decoded.as_ref() {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        _ => return Ok(None),
    };
    Ok(Some(value.to_string()))
}

fn is_office_text_xml(name: &str) -> bool {
    name == "word/document.xml"
        || name == "xl/sharedStrings.xml"
        || (name.starts_with("ppt/slides/slide") && name.ends_with(".xml"))
        || (name.starts_with("xl/worksheets/sheet") && name.ends_with(".xml"))
}

fn llm_parser_for_path(path: &Path, mode: ParserMode) -> Option<ParserMode> {
    if mode.requires_llm() {
        return Some(mode);
    }
    if !mode.is_auto() {
        return None;
    }

    if matches_extension(
        path,
        &[
            "png", "jpg", "jpeg", "gif", "webp", "tiff", "tif", "bmp", "heic",
        ],
    ) {
        return Some(ParserMode::Vlm);
    }
    if matches_extension(
        path,
        &["mp3", "wav", "m4a", "flac", "ogg", "mp4", "mov", "webm"],
    ) {
        return Some(ParserMode::MmLlm);
    }
    None
}

fn reject_or_defer_llm_parser(
    parser: ParserMode,
    path: &Path,
    allow_llm: bool,
) -> Result<ParsedDocument, ParserError> {
    if !allow_llm {
        return Err(ParserError::LlmParserRequiresOptIn {
            parser,
            path: path.to_path_buf(),
        });
    }
    Err(ParserError::LlmParserUnavailable {
        parser,
        path: path.to_path_buf(),
    })
}

fn matches_extension(path: &Path, candidates: &[&str]) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            candidates
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
        .unwrap_or(false)
}
