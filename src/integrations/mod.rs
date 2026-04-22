use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

mod claude_code;
mod codex;
mod csa;

use self::claude_code::ClaudeCodeIntegration;
use self::codex::CodexIntegration;
use self::csa::CsaIntegration;

pub(crate) const INTEGRATIONS_SPEC_PATH: &str = "specs/fork-ext/p11-integrations-layer.spec.md";
pub(crate) const SETTINGS_SNIPPET_PLACEHOLDER: &str = "__MEMPAL_SESSION_START_COMMAND__";
const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Subcommand)]
pub enum IntegrationCommands {
    Bootstrap,
    Install {
        #[arg(long, value_enum)]
        tool: IntegrationTool,
        #[arg(long, value_enum, default_value_t = IntegrationProfile::User)]
        profile: IntegrationProfile,
    },
    Uninstall {
        #[arg(long, value_enum)]
        tool: IntegrationTool,
    },
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum IntegrationTool {
    #[value(name = "claude-code")]
    ClaudeCode,
    #[value(name = "codex")]
    Codex,
    #[value(name = "csa")]
    Csa,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum IntegrationProfile {
    #[value(name = "user", alias = "global")]
    User,
    #[value(name = "project")]
    Project,
}

#[derive(Debug, Clone)]
pub struct ToolActionReport {
    pub changed: bool,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ToolStatusReport {
    pub name: &'static str,
    pub installed: bool,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub(crate) struct IntegrationContext {
    pub home: PathBuf,
    pub root: PathBuf,
}

impl IntegrationContext {
    fn new(home: PathBuf) -> Self {
        let root = home.join(".mempal").join("integrations");
        Self { home, root }
    }

    pub(crate) fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.toml")
    }
}

pub(crate) trait ToolIntegration {
    fn name(&self) -> &'static str;
    fn config_paths(&self, context: &IntegrationContext) -> Vec<PathBuf>;
    fn install(
        &self,
        context: &IntegrationContext,
        profile: IntegrationProfile,
    ) -> Result<ToolActionReport>;
    fn uninstall(&self, context: &IntegrationContext) -> Result<ToolActionReport>;
    fn status(&self, context: &IntegrationContext) -> Result<ToolStatusReport>;
}

#[derive(Debug, Clone)]
struct BundledAsset {
    relative_path: PathBuf,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AssetManifest {
    version: u32,
    assets: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
struct BootstrapReport {
    changed_files: usize,
    manifest_changed: bool,
}

#[derive(Debug, Clone)]
struct StatusReport {
    lines: Vec<String>,
    has_drift: bool,
}

#[derive(Debug)]
struct FileLockGuard {
    file: File,
}

impl FileLockGuard {
    fn acquire(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("failed to create integrations root {}", root.display()))?;
        let lock_path = root.join(".lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
        lock_file_exclusive(&file, &lock_path)?;
        Ok(Self { file })
    }
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        let _ = unlock_file(&self.file);
    }
}

pub fn run_command(command: IntegrationCommands) -> Result<()> {
    ensure_supported_platform()?;
    let home = user_home_dir()?;
    let context = IntegrationContext::new(home);

    match command {
        IntegrationCommands::Bootstrap => {
            let _lock = FileLockGuard::acquire(&context.root)?;
            let report = bootstrap_assets(&context)?;
            if report.changed_files == 0 && !report.manifest_changed {
                println!("integrations assets are up-to-date");
            } else {
                println!(
                    "bootstrapped integrations assets: {} file(s) updated{}",
                    report.changed_files,
                    if report.manifest_changed {
                        " + manifest"
                    } else {
                        ""
                    }
                );
            }
        }
        IntegrationCommands::Install { tool, profile } => {
            let _lock = FileLockGuard::acquire(&context.root)?;
            bootstrap_assets(&context)?;
            let report = tool_impl(tool).install(&context, profile)?;
            println!("{}", report.message);
        }
        IntegrationCommands::Uninstall { tool } => {
            let _lock = FileLockGuard::acquire(&context.root)?;
            let report = tool_impl(tool).uninstall(&context)?;
            println!("{}", report.message);
        }
        IntegrationCommands::Status => {
            let report = status(&context)?;
            for line in &report.lines {
                println!("{line}");
            }
            if report.has_drift {
                bail!("integrations status detected drift");
            }
        }
    }

    Ok(())
}

fn tool_impl(tool: IntegrationTool) -> Box<dyn ToolIntegration> {
    match tool {
        IntegrationTool::ClaudeCode => Box::new(ClaudeCodeIntegration),
        IntegrationTool::Codex => Box::new(CodexIntegration),
        IntegrationTool::Csa => Box::new(CsaIntegration),
    }
}

fn bootstrap_assets(context: &IntegrationContext) -> Result<BootstrapReport> {
    let assets = bundled_assets();
    let mut changed_files = 0usize;

    for asset in &assets {
        let destination = context.root.join(&asset.relative_path);
        if write_bytes_if_changed(
            &destination,
            &asset.bytes,
            is_executable_asset(&asset.relative_path),
        )? {
            changed_files += 1;
        }
    }

    let manifest = AssetManifest {
        version: MANIFEST_VERSION,
        assets: assets
            .iter()
            .map(|asset| {
                (
                    asset.relative_path.to_string_lossy().to_string(),
                    blake3_hash(&asset.bytes),
                )
            })
            .collect(),
    };
    let manifest_bytes = toml::to_string_pretty(&manifest)
        .context("failed to serialize integrations manifest")?
        .into_bytes();
    let manifest_changed =
        write_bytes_if_changed(&context.manifest_path(), &manifest_bytes, false)?;

    Ok(BootstrapReport {
        changed_files,
        manifest_changed,
    })
}

fn bundled_assets() -> Vec<BundledAsset> {
    let mut assets = vec![
        bundled_asset(
            "claude-code/hooks/session-start.sh",
            include_bytes!("../../assets/integrations/claude-code/hooks/session-start.sh"),
        ),
        bundled_asset(
            "claude-code/settings-snippet.json",
            include_bytes!("../../assets/integrations/claude-code/settings-snippet.json"),
        ),
        bundled_asset(
            "claude-code/skills/README.md",
            include_bytes!("../../assets/integrations/claude-code/skills/README.md"),
        ),
        bundled_asset(
            "codex/README.md",
            include_bytes!("../../assets/integrations/codex/README.md"),
        ),
        bundled_asset(
            "codex/config-snippet.toml",
            include_bytes!("../../assets/integrations/codex/config-snippet.toml"),
        ),
        bundled_asset(
            "csa/README.md",
            include_bytes!("../../assets/integrations/csa/README.md"),
        ),
    ];
    assets.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    assets
}

fn bundled_asset(relative_path: &str, bytes: &'static [u8]) -> BundledAsset {
    BundledAsset {
        relative_path: PathBuf::from(relative_path),
        bytes: bytes.to_vec(),
    }
}

fn status(context: &IntegrationContext) -> Result<StatusReport> {
    let mut lines = Vec::new();
    let mut has_drift = false;

    let manifest_path = context.manifest_path();
    let manifest: AssetManifest = if manifest_path.exists() {
        toml::from_str(
            &fs::read_to_string(&manifest_path)
                .with_context(|| format!("failed to read {}", manifest_path.display()))?,
        )
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?
    } else {
        has_drift = true;
        lines.push(format!(
            "assets manifest missing at {}",
            manifest_path.display()
        ));
        AssetManifest {
            version: MANIFEST_VERSION,
            assets: BTreeMap::new(),
        }
    };

    for (relative_path, expected_hash) in &manifest.assets {
        let path = context.root.join(relative_path);
        match fs::read(&path) {
            Ok(bytes) => {
                let current_hash = blake3_hash(&bytes);
                if current_hash == *expected_hash {
                    lines.push(format!("asset {relative_path}: ok"));
                } else {
                    has_drift = true;
                    lines.push(format!("asset {relative_path}: drifted"));
                }
            }
            Err(_) => {
                has_drift = true;
                lines.push(format!("asset {relative_path}: drifted"));
            }
        }
    }

    for tool in [
        IntegrationTool::ClaudeCode,
        IntegrationTool::Codex,
        IntegrationTool::Csa,
    ] {
        let tool_status = tool_impl(tool).status(context)?;
        lines.push(format!(
            "tool {}: {} ({})",
            tool_status.name,
            if tool_status.installed {
                "installed"
            } else {
                "not-installed"
            },
            tool_status.detail
        ));
    }

    Ok(StatusReport { lines, has_drift })
}

pub(crate) fn ensure_supported_platform() -> Result<()> {
    ensure_supported_platform_for(std::env::consts::OS)
}

fn ensure_supported_platform_for(os_name: &str) -> Result<()> {
    if os_name.eq_ignore_ascii_case("windows") {
        bail!("integrations layer is POSIX-only; Windows not supported");
    }
    Ok(())
}

pub(crate) fn user_home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot resolve $HOME"))
}

pub(crate) fn read_json_object_or_default(path: &Path, default: &str) -> Result<String> {
    if !path.exists() {
        return Ok(default.to_string());
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let parsed: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("invalid JSON in {}", path.display()))?;
    if !parsed.is_object() {
        bail!(
            "refusing to overwrite {}: top-level JSON must be an object",
            path.display()
        );
    }
    Ok(content)
}

pub(crate) fn write_text_if_changed(path: &Path, content: &str) -> Result<bool> {
    write_bytes_if_changed(path, content.as_bytes(), false)
}

pub(crate) fn shell_escape_path(path: &Path) -> String {
    let rendered = path.to_string_lossy();
    if !rendered.contains([' ', '\t', '\n', '\'', '"']) {
        return rendered.into_owned();
    }
    format!("'{}'", rendered.replace('\'', r"'\''"))
}

fn blake3_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn is_executable_asset(relative_path: &Path) -> bool {
    relative_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext == "sh")
}

fn write_bytes_if_changed(path: &Path, bytes: &[u8], executable: bool) -> Result<bool> {
    if fs::read(path).ok().as_deref() == Some(bytes) {
        return Ok(false);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent {}", parent.display()))?;
    }

    if path.exists() {
        backup_existing_file(path)?;
    }

    let tmp_path = temp_write_path(path);
    fs::write(&tmp_path, bytes)
        .with_context(|| format!("failed to write temporary file {}", tmp_path.display()))?;
    if executable {
        make_executable(&tmp_path)?;
    }
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "failed to move temporary file {} into {}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(true)
}

fn backup_existing_file(path: &Path) -> Result<()> {
    let backup_name = format!(
        "{}.bak.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("backup"),
        unix_timestamp_secs()
    );
    let backup_path = path.with_file_name(backup_name);
    fs::copy(path, &backup_path).with_context(|| {
        format!(
            "failed to create backup {} from {}",
            backup_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

fn temp_write_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("tmp"),
        unix_timestamp_secs()
    ))
}

fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to chmod {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn lock_file_exclusive(file: &File, lock_path: &Path) -> Result<()> {
    // SAFETY: `file` is a live `std::fs::File`; `as_raw_fd()` yields a valid
    // descriptor for the duration of this call, and `flock` does not retain it.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to lock {}", lock_path.display()))
    }
}

#[cfg(not(unix))]
fn lock_file_exclusive(_file: &File, _lock_path: &Path) -> Result<()> {
    bail!("integrations layer is POSIX-only; Windows not supported");
}

#[cfg(unix)]
fn unlock_file(file: &File) -> Result<()> {
    // SAFETY: `file` is the same live descriptor previously locked via
    // `flock`; unlocking is scoped to this process and does not outlive the fd.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).context("failed to unlock integrations lock file")
    }
}

#[cfg(not(unix))]
fn unlock_file(_file: &File) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_windows_detection_errors_early() {
        let error = super::ensure_supported_platform_for("windows").expect_err("must fail");
        assert_eq!(
            error.to_string(),
            "integrations layer is POSIX-only; Windows not supported"
        );
    }
}
