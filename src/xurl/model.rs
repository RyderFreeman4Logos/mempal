#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tool {
    Cc,
    Codex,
    Hermes,
}

impl Tool {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tool::Cc => "cc",
            Tool::Codex => "codex",
            Tool::Hermes => "hermes",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// Turn provenance — who generated the turn content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    Human,
    Agent,
    System,
}

impl Provenance {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provenance::Human => "human",
            Provenance::Agent => "agent",
            Provenance::System => "system",
        }
    }
}

pub struct RawTurn {
    pub session_id: String,
    pub tool: Tool,
    pub role: Role,
    pub content: String,
    pub timestamp_epoch: f64,
    pub project_path: Option<String>,
    pub git_branch: Option<String>,
    pub is_csa_delegated: bool,
    pub provenance: Provenance,
    /// 0-based monotonic index within the session (counting only kept turns).
    pub turn_index: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_display_round_trip() {
        assert_eq!(Tool::Cc.as_str(), "cc");
        assert_eq!(Tool::Codex.as_str(), "codex");
        assert_eq!(Tool::Hermes.as_str(), "hermes");
    }

    #[test]
    fn role_display_round_trip() {
        assert_eq!(Role::User.as_str(), "user");
        assert_eq!(Role::Assistant.as_str(), "assistant");
    }
}
