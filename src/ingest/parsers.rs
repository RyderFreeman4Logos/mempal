//! Plugin boundary for deterministic document parsing before normalization.

use std::fmt;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::{Reader, escape};
use thiserror::Error;
use zip::ZipArchive;

use super::detect::{Format, detect_format};

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
    #[error("failed to extract text from PDF {path}")]
    PdfText {
        path: PathBuf,
        #[source]
        source: pdf_extract::OutputError,
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
        let content = pdf_extract::extract_text_from_mem(input.bytes).map_err(|source| {
            ParserError::PdfText {
                path: input.path.to_path_buf(),
                source,
            }
        })?;
        Ok(ParsedDocument {
            content,
            format: Format::PlainText,
            parser_id: self.id(),
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
        let content = extract_office_text(input.path, input.bytes)?;
        Ok(ParsedDocument {
            content,
            format: Format::PlainText,
            parser_id: self.id(),
        })
    }
}

fn extract_office_text(path: &Path, bytes: &[u8]) -> Result<String, ParserError> {
    let cursor = Cursor::new(bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|source| ParserError::OfficeZip {
        path: path.to_path_buf(),
        source,
    })?;
    let mut parts = Vec::new();

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

        let mut xml = String::new();
        file.read_to_string(&mut xml)
            .map_err(|source| ParserError::OfficeIo {
                path: path.to_path_buf(),
                source,
            })?;
        let text = extract_xml_text(path, &xml)?;
        if !text.trim().is_empty() {
            parts.push(text);
        }
    }

    let content = parts.join("\n\n");
    if content.trim().is_empty() {
        return Err(ParserError::EmptyOfficeText {
            path: path.to_path_buf(),
        });
    }
    Ok(content)
}

fn extract_xml_text(path: &Path, xml: &str) -> Result<String, ParserError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut parts = Vec::new();

    loop {
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
                    parts.push(value.to_string());
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
                    parts.push(value.to_string());
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                if let Some(value) = resolve_general_ref(path, &reference)? {
                    parts.push(value);
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

    Ok(parts.join(" "))
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
