use super::config::{TurnStorageMode, TurnsConfig};
use super::db::{Database, DbError};
use super::types::{Drawer, SearchResult};

pub fn is_raw_turn(wing: &str, room: Option<&str>, config: &TurnsConfig) -> bool {
    config
        .raw_turn_wings
        .iter()
        .filter(|prefix| !prefix.is_empty())
        .any(|prefix| wing.starts_with(prefix))
        || room.is_some_and(|room| {
            config
                .raw_turn_rooms
                .iter()
                .filter(|candidate| !candidate.is_empty())
                .any(|candidate| candidate == room)
        })
}

pub fn should_store_raw_turns(mode: &TurnStorageMode) -> bool {
    matches!(mode, TurnStorageMode::RawEvidence)
}

pub fn raw_turn_importance(wing: &str, room: Option<&str>, config: &TurnsConfig) -> Option<i32> {
    is_raw_turn(wing, room, config).then_some(config.default_importance)
}

pub fn is_excluded_raw_turn(
    wing: &str,
    room: Option<&str>,
    importance: i32,
    config: &TurnsConfig,
) -> bool {
    importance == 0 && is_raw_turn(wing, room, config)
}

pub fn is_excluded_raw_turn_result(result: &SearchResult, config: &TurnsConfig) -> bool {
    is_excluded_raw_turn(
        &result.wing,
        result.room.as_deref(),
        result.importance,
        config,
    )
}

pub fn is_excluded_raw_turn_drawer(drawer: &Drawer, config: &TurnsConfig) -> bool {
    is_excluded_raw_turn(
        &drawer.wing,
        drawer.room.as_deref(),
        drawer.importance,
        config,
    )
}

pub fn count_raw_turn_drawers(db: &Database, config: &TurnsConfig) -> Result<i64, DbError> {
    Ok(db
        .scope_counts()?
        .into_iter()
        .filter(|(wing, room, _)| is_raw_turn(wing, room.as_deref(), config))
        .map(|(_, _, count)| count)
        .sum())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_wing_prefix_or_room() {
        let config = TurnsConfig {
            raw_turn_wings: vec!["hooks-raw".to_string(), "hermes-user".to_string()],
            raw_turn_rooms: vec!["turns".to_string(), "turns/raw".to_string()],
            ..TurnsConfig::default()
        };

        assert!(is_raw_turn("hooks-raw", Some("user-prompt"), &config));
        assert!(is_raw_turn("hermes-user/alice", Some("facts"), &config));
        assert!(is_raw_turn("project", Some("turns"), &config));
        assert!(!is_raw_turn("project", Some("decision"), &config));
    }

    #[test]
    fn empty_prefixes_do_not_match_everything() {
        let config = TurnsConfig {
            raw_turn_wings: vec![String::new()],
            raw_turn_rooms: vec![String::new()],
            ..TurnsConfig::default()
        };

        assert!(!is_raw_turn("project", Some("decision"), &config));
    }
}
