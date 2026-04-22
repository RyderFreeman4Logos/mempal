use anyhow::{Result, bail};

use super::{
    INTEGRATIONS_SPEC_PATH, IntegrationContext, IntegrationProfile, ToolActionReport,
    ToolIntegration, ToolStatusReport,
};

pub(crate) struct CodexIntegration;

impl ToolIntegration for CodexIntegration {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn config_paths(&self, _context: &IntegrationContext) -> Vec<std::path::PathBuf> {
        Vec::new()
    }

    fn install(
        &self,
        _context: &IntegrationContext,
        _profile: IntegrationProfile,
    ) -> Result<ToolActionReport> {
        bail!(
            "not_yet_implemented: codex integration is spec-reserved, see {}",
            INTEGRATIONS_SPEC_PATH
        )
    }

    fn uninstall(&self, _context: &IntegrationContext) -> Result<ToolActionReport> {
        bail!(
            "not_yet_implemented: codex integration is spec-reserved, see {}",
            INTEGRATIONS_SPEC_PATH
        )
    }

    fn status(&self, _context: &IntegrationContext) -> Result<ToolStatusReport> {
        Ok(ToolStatusReport {
            name: self.name(),
            installed: false,
            detail: "reserved stub".to_string(),
        })
    }
}
