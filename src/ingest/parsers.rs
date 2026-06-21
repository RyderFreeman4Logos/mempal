//! Plugin boundary for deterministic document parsing before normalization.

use std::fmt;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::{Reader, escape};
use thiserror::Error;
use zip::ZipArchive;

use super::detect::{Format, detect_format};

const ZIP_EOCD_SIGNATURE: u32 = 0x0605_4b50;
const ZIP64_EOCD_SIGNATURE: u32 = 0x0606_4b50;
const ZIP64_EOCD_LOCATOR_SIGNATURE: u32 = 0x0706_4b50;
const ZIP_EOCD_MIN_LEN: usize = 22;
const ZIP64_EOCD_MIN_LEN: usize = 56;
const ZIP64_EOCD_LOCATOR_LEN: usize = 20;
const ZIP_MAX_COMMENT_LEN: usize = u16::MAX as usize;
const ZIP16_ENTRY_COUNT_SENTINEL: u16 = u16::MAX;
const ZIP32_SIZE_SENTINEL: u32 = u32::MAX;

/// Resource limits for deterministic in-process parsers.
///
/// These defaults keep plugin parsing bounded while still allowing ordinary
/// notes, source files, and Office documents to ingest without config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParserResourceLimits {
    pub max_deterministic_input_bytes: usize,
    pub max_pdf_input_bytes: usize,
    pub max_ooxml_archive_entries: usize,
    pub max_ooxml_central_directory_bytes: usize,
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
            max_ooxml_central_directory_bytes: 4 * 1024 * 1024,
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
    OoxmlCentralDirectoryBytes,
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
            Self::OoxmlCentralDirectoryBytes => "ooxml_central_directory_bytes",
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
    #[error("failed to preflight Office archive {path}: {reason}")]
    OfficeZipPreflight { path: PathBuf, reason: &'static str },
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
    #[error("failed to parse Office XML attributes from {path}")]
    OfficeXmlAttribute {
        path: PathBuf,
        #[source]
        source: quick_xml::events::attributes::AttrError,
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
    preflight_ooxml_zip(path, bytes, &limits)?;
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
        let Some(xml_kind) = office_text_xml_kind(&name) else {
            continue;
        };

        let uncompressed_size = file.size();
        budget.ensure_next_xml_member(path, uncompressed_size, &limits)?;
        let xml = read_ooxml_member(path, &mut file, uncompressed_size, &limits)?;
        budget.add_xml_bytes(path, xml.len() as u64, &limits)?;
        extract_xml_text(path, &xml, xml_kind, &limits, &mut budget, &mut content)?;
    }

    let content = content.into_string();
    if content.trim().is_empty() {
        return Err(ParserError::EmptyOfficeText {
            path: path.to_path_buf(),
        });
    }
    Ok(content)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OoxmlZipMetadata {
    entries: u64,
    central_directory_size: u64,
    central_directory_offset: u64,
}

pub(crate) fn preflight_ooxml_zip(
    path: &Path,
    bytes: &[u8],
    limits: &ParserResourceLimits,
) -> Result<OoxmlZipMetadata, ParserError> {
    let metadata = read_zip_metadata(path, bytes)?;
    ensure_resource_limit(
        path,
        metadata.entries,
        ParserResourceLimit::OoxmlArchiveEntries,
        limits.max_ooxml_archive_entries as u64,
    )?;
    ensure_resource_limit(
        path,
        metadata.central_directory_size,
        ParserResourceLimit::OoxmlCentralDirectoryBytes,
        limits.max_ooxml_central_directory_bytes as u64,
    )?;
    let archive_len = bytes.len() as u64;
    let central_directory_end = metadata
        .central_directory_offset
        .checked_add(metadata.central_directory_size)
        .ok_or_else(|| ooxml_preflight_error(path, "central directory bounds overflow"))?;
    if central_directory_end > archive_len {
        return Err(ooxml_preflight_error(
            path,
            "central directory extends beyond archive bytes",
        ));
    }
    Ok(metadata)
}

fn read_zip_metadata(path: &Path, bytes: &[u8]) -> Result<OoxmlZipMetadata, ParserError> {
    let eocd_offset = find_zip_eocd(bytes)
        .ok_or_else(|| ooxml_preflight_error(path, "end-of-central-directory record not found"))?;
    let entries16 = read_u16_le(bytes, eocd_offset + 10)
        .ok_or_else(|| ooxml_preflight_error(path, "truncated end-of-central-directory record"))?;
    let central_directory_size32 = read_u32_le(bytes, eocd_offset + 12)
        .ok_or_else(|| ooxml_preflight_error(path, "truncated end-of-central-directory record"))?;
    let central_directory_offset32 = read_u32_le(bytes, eocd_offset + 16)
        .ok_or_else(|| ooxml_preflight_error(path, "truncated end-of-central-directory record"))?;

    if entries16 == ZIP16_ENTRY_COUNT_SENTINEL
        || central_directory_size32 == ZIP32_SIZE_SENTINEL
        || central_directory_offset32 == ZIP32_SIZE_SENTINEL
    {
        return read_zip64_metadata(path, bytes, eocd_offset);
    }

    Ok(OoxmlZipMetadata {
        entries: u64::from(entries16),
        central_directory_size: u64::from(central_directory_size32),
        central_directory_offset: u64::from(central_directory_offset32),
    })
}

fn read_zip64_metadata(
    path: &Path,
    bytes: &[u8],
    eocd_offset: usize,
) -> Result<OoxmlZipMetadata, ParserError> {
    let locator_offset = eocd_offset
        .checked_sub(ZIP64_EOCD_LOCATOR_LEN)
        .ok_or_else(|| ooxml_preflight_error(path, "missing Zip64 locator"))?;
    let locator_signature = read_u32_le(bytes, locator_offset)
        .ok_or_else(|| ooxml_preflight_error(path, "truncated Zip64 locator"))?;
    if locator_signature != ZIP64_EOCD_LOCATOR_SIGNATURE {
        return Err(ooxml_preflight_error(path, "missing Zip64 locator"));
    }
    let zip64_eocd_offset = read_u64_le(bytes, locator_offset + 8)
        .ok_or_else(|| ooxml_preflight_error(path, "truncated Zip64 locator"))?;
    let zip64_eocd_offset = usize::try_from(zip64_eocd_offset)
        .map_err(|_| ooxml_preflight_error(path, "Zip64 record offset is too large"))?;
    let zip64_signature = read_u32_le(bytes, zip64_eocd_offset)
        .ok_or_else(|| ooxml_preflight_error(path, "truncated Zip64 record"))?;
    if zip64_signature != ZIP64_EOCD_SIGNATURE {
        return Err(ooxml_preflight_error(path, "missing Zip64 record"));
    }
    let zip64_record_size = read_u64_le(bytes, zip64_eocd_offset + 4)
        .ok_or_else(|| ooxml_preflight_error(path, "truncated Zip64 record"))?;
    if zip64_record_size < 44 {
        return Err(ooxml_preflight_error(path, "invalid Zip64 record size"));
    }
    let zip64_min_end = zip64_eocd_offset
        .checked_add(ZIP64_EOCD_MIN_LEN)
        .ok_or_else(|| ooxml_preflight_error(path, "Zip64 record bounds overflow"))?;
    if zip64_min_end > bytes.len() {
        return Err(ooxml_preflight_error(path, "truncated Zip64 record"));
    }

    Ok(OoxmlZipMetadata {
        entries: read_u64_le(bytes, zip64_eocd_offset + 32)
            .ok_or_else(|| ooxml_preflight_error(path, "truncated Zip64 record"))?,
        central_directory_size: read_u64_le(bytes, zip64_eocd_offset + 40)
            .ok_or_else(|| ooxml_preflight_error(path, "truncated Zip64 record"))?,
        central_directory_offset: read_u64_le(bytes, zip64_eocd_offset + 48)
            .ok_or_else(|| ooxml_preflight_error(path, "truncated Zip64 record"))?,
    })
}

fn find_zip_eocd(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < ZIP_EOCD_MIN_LEN {
        return None;
    }
    let search_start = bytes
        .len()
        .saturating_sub(ZIP_EOCD_MIN_LEN + ZIP_MAX_COMMENT_LEN);
    let latest_start = bytes.len() - ZIP_EOCD_MIN_LEN;
    for offset in (search_start..=latest_start).rev() {
        if read_u32_le(bytes, offset) != Some(ZIP_EOCD_SIGNATURE) {
            continue;
        }
        let comment_len = read_u16_le(bytes, offset + 20)? as usize;
        if offset + ZIP_EOCD_MIN_LEN + comment_len == bytes.len() {
            return Some(offset);
        }
    }
    None
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u64_le(bytes: &[u8], offset: usize) -> Option<u64> {
    let slice = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

fn ooxml_preflight_error(path: &Path, reason: &'static str) -> ParserError {
    ParserError::OfficeZipPreflight {
        path: path.to_path_buf(),
        reason,
    }
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
    xml_kind: OfficeXmlKind,
    limits: &ParserResourceLimits,
    budget: &mut OoxmlDocumentBudget,
    output: &mut BoundedText,
) -> Result<(), ParserError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut member_has_text = false;
    let mut worksheet_state = WorksheetTextState::default();

    loop {
        budget.add_xml_node(path, limits)?;
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                if xml_kind == OfficeXmlKind::SpreadsheetWorksheet {
                    worksheet_state.enter_start(path, &element)?;
                }
            }
            Ok(Event::End(element)) => {
                if xml_kind == OfficeXmlKind::SpreadsheetWorksheet {
                    worksheet_state.exit_end(&element);
                }
            }
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
                    push_xml_fragment(
                        path,
                        value,
                        &worksheet_state,
                        output,
                        &mut member_has_text,
                        limits,
                    )?;
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
                    push_xml_fragment(
                        path,
                        value,
                        &worksheet_state,
                        output,
                        &mut member_has_text,
                        limits,
                    )?;
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                if let Some(value) = resolve_general_ref(path, &reference)? {
                    push_xml_fragment(
                        path,
                        &value,
                        &worksheet_state,
                        output,
                        &mut member_has_text,
                        limits,
                    )?;
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

fn push_xml_fragment(
    path: &Path,
    value: &str,
    worksheet_state: &WorksheetTextState,
    output: &mut BoundedText,
    member_has_text: &mut bool,
    limits: &ParserResourceLimits,
) -> Result<(), ParserError> {
    if worksheet_state.should_skip_text() {
        return Ok(());
    }
    output.push_fragment(path, value, member_has_text, limits)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OfficeXmlKind {
    PlainText,
    SpreadsheetWorksheet,
}

#[derive(Debug, Default)]
struct WorksheetTextState {
    cell_depth: u32,
    shared_string_cell: bool,
    shared_string_value_depth: u32,
}

impl WorksheetTextState {
    fn enter_start(
        &mut self,
        path: &Path,
        element: &quick_xml::events::BytesStart<'_>,
    ) -> Result<(), ParserError> {
        let name = element.name();
        if xml_local_name_eq(name.as_ref(), b"c") {
            self.cell_depth = self.cell_depth.saturating_add(1);
            if self.cell_depth == 1 {
                self.shared_string_cell = element_has_raw_attribute(path, element, b"t", b"s")?;
            }
            return Ok(());
        }
        if self.cell_depth > 0 && self.shared_string_cell && xml_local_name_eq(name.as_ref(), b"v")
        {
            self.shared_string_value_depth = self.shared_string_value_depth.saturating_add(1);
        }
        Ok(())
    }

    fn exit_end(&mut self, element: &quick_xml::events::BytesEnd<'_>) {
        let name = element.name();
        if self.shared_string_value_depth > 0 && xml_local_name_eq(name.as_ref(), b"v") {
            self.shared_string_value_depth -= 1;
            return;
        }
        if self.cell_depth > 0 && xml_local_name_eq(name.as_ref(), b"c") {
            self.cell_depth -= 1;
            if self.cell_depth == 0 {
                self.shared_string_cell = false;
                self.shared_string_value_depth = 0;
            }
        }
    }

    fn should_skip_text(&self) -> bool {
        self.shared_string_value_depth > 0
    }
}

fn element_has_raw_attribute(
    path: &Path,
    element: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
    value: &[u8],
) -> Result<bool, ParserError> {
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|source| ParserError::OfficeXmlAttribute {
            path: path.to_path_buf(),
            source,
        })?;
        if xml_local_name_eq(attribute.key.as_ref(), key) && attribute.value.as_ref() == value {
            return Ok(true);
        }
    }
    Ok(false)
}

fn xml_local_name_eq(name: &[u8], expected: &[u8]) -> bool {
    name == expected
        || name
            .rsplit(|byte| *byte == b':')
            .next()
            .is_some_and(|local_name| local_name == expected)
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

fn office_text_xml_kind(name: &str) -> Option<OfficeXmlKind> {
    if name == "word/document.xml"
        || name == "xl/sharedStrings.xml"
        || (name.starts_with("ppt/slides/slide") && name.ends_with(".xml"))
    {
        return Some(OfficeXmlKind::PlainText);
    }
    if name.starts_with("xl/worksheets/sheet") && name.ends_with(".xml") {
        return Some(OfficeXmlKind::SpreadsheetWorksheet);
    }
    None
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

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::write::SimpleFileOptions;

    use super::*;

    fn zip_with_empty_entries(entry_count: usize) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        for index in 0..entry_count {
            writer
                .start_file(
                    format!("ppt/slides/slide{index}.xml"),
                    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
                )
                .expect("start zip member");
            writer.write_all(b"<p:sld/>").expect("write zip member");
        }
        writer.finish().expect("finish zip").into_inner()
    }

    #[test]
    fn ooxml_zip_preflight_rejects_entry_count_before_archive_open() {
        let limits = ParserResourceLimits::default();
        let archive = zip_with_empty_entries(limits.max_ooxml_archive_entries + 1);

        let error = preflight_ooxml_zip(Path::new("many-slides.pptx"), &archive, &limits)
            .expect_err("preflight should reject too many entries directly");

        assert!(matches!(
            error,
            ParserError::ResourceLimitExceeded {
                limit: ParserResourceLimit::OoxmlArchiveEntries,
                ..
            }
        ));
    }

    #[test]
    fn ooxml_zip_preflight_rejects_central_directory_bytes() {
        let limits = ParserResourceLimits {
            max_ooxml_central_directory_bytes: 32,
            ..ParserResourceLimits::default()
        };
        let archive = zip_with_empty_entries(2);

        let error = preflight_ooxml_zip(Path::new("wide-directory.docx"), &archive, &limits)
            .expect_err("preflight should cap the central directory directly");

        assert!(matches!(
            error,
            ParserError::ResourceLimitExceeded {
                limit: ParserResourceLimit::OoxmlCentralDirectoryBytes,
                ..
            }
        ));
    }

    #[test]
    fn ooxml_zip_preflight_reads_zip64_metadata() {
        let limits = ParserResourceLimits::default();
        let mut archive = Vec::new();
        archive.extend_from_slice(&ZIP64_EOCD_SIGNATURE.to_le_bytes());
        archive.extend_from_slice(&44u64.to_le_bytes());
        archive.extend_from_slice(&45u16.to_le_bytes());
        archive.extend_from_slice(&45u16.to_le_bytes());
        archive.extend_from_slice(&0u32.to_le_bytes());
        archive.extend_from_slice(&0u32.to_le_bytes());
        archive.extend_from_slice(&0u64.to_le_bytes());
        archive.extend_from_slice(&((limits.max_ooxml_archive_entries + 1) as u64).to_le_bytes());
        archive.extend_from_slice(&0u64.to_le_bytes());
        archive.extend_from_slice(&0u64.to_le_bytes());
        archive.extend_from_slice(&ZIP64_EOCD_LOCATOR_SIGNATURE.to_le_bytes());
        archive.extend_from_slice(&0u32.to_le_bytes());
        archive.extend_from_slice(&0u64.to_le_bytes());
        archive.extend_from_slice(&1u32.to_le_bytes());
        archive.extend_from_slice(&ZIP_EOCD_SIGNATURE.to_le_bytes());
        archive.extend_from_slice(&0u16.to_le_bytes());
        archive.extend_from_slice(&0u16.to_le_bytes());
        archive.extend_from_slice(&ZIP16_ENTRY_COUNT_SENTINEL.to_le_bytes());
        archive.extend_from_slice(&ZIP16_ENTRY_COUNT_SENTINEL.to_le_bytes());
        archive.extend_from_slice(&0u32.to_le_bytes());
        archive.extend_from_slice(&0u32.to_le_bytes());
        archive.extend_from_slice(&0u16.to_le_bytes());

        let error = preflight_ooxml_zip(Path::new("zip64.docx"), &archive, &limits)
            .expect_err("preflight should use Zip64 entry metadata");

        assert!(matches!(
            error,
            ParserError::ResourceLimitExceeded {
                limit: ParserResourceLimit::OoxmlArchiveEntries,
                ..
            }
        ));
    }
}
