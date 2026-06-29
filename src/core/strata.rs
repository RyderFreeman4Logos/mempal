use super::config::{TurnStorageMode, TurnsConfig};
use super::db::{Database, DbError};
use super::types::{Drawer, MemoryKind, SearchResult};

pub fn is_raw_turn(
    wing: &str,
    room: Option<&str>,
    memory_kind: Option<&MemoryKind>,
    config: &TurnsConfig,
) -> bool {
    if room == Some("facts") || memory_kind.is_some_and(|kind| kind == &MemoryKind::ProfileFact) {
        return false;
    }

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

pub fn raw_turn_importance(
    wing: &str,
    room: Option<&str>,
    memory_kind: Option<&MemoryKind>,
    config: &TurnsConfig,
) -> Option<i32> {
    is_raw_turn(wing, room, memory_kind, config).then_some(config.default_importance)
}

pub fn is_excluded_raw_turn(
    wing: &str,
    room: Option<&str>,
    memory_kind: &MemoryKind,
    importance: i32,
    config: &TurnsConfig,
) -> bool {
    importance == 0 && is_raw_turn(wing, room, Some(memory_kind), config)
}

pub fn is_excluded_raw_turn_result(result: &SearchResult, config: &TurnsConfig) -> bool {
    is_excluded_raw_turn(
        &result.wing,
        result.room.as_deref(),
        &result.memory_kind,
        result.importance,
        config,
    )
}

pub fn is_excluded_raw_turn_drawer(drawer: &Drawer, config: &TurnsConfig) -> bool {
    is_excluded_raw_turn(
        &drawer.wing,
        drawer.room.as_deref(),
        &drawer.memory_kind,
        drawer.importance,
        config,
    )
}

pub fn count_raw_turn_drawers(db: &Database, config: &TurnsConfig) -> Result<i64, DbError> {
    Ok(db
        .raw_turn_classification_rows()?
        .into_iter()
        .filter(|row| {
            is_excluded_raw_turn(
                &row.wing,
                row.room.as_deref(),
                &row.memory_kind,
                row.importance,
                config,
            )
        })
        .count() as i64)
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

        assert!(is_raw_turn("hooks-raw", Some("user-prompt"), None, &config));
        assert!(!is_raw_turn(
            "hermes-user/alice",
            Some("facts"),
            None,
            &config
        ));
        assert!(is_raw_turn("project", Some("turns"), None, &config));
        assert!(!is_raw_turn("project", Some("decision"), None, &config));
        assert!(!is_raw_turn(
            "hermes-user/alice",
            Some("turns"),
            Some(&MemoryKind::ProfileFact),
            &config
        ));
    }

    #[test]
    fn empty_prefixes_do_not_match_everything() {
        let config = TurnsConfig {
            raw_turn_wings: vec![String::new()],
            raw_turn_rooms: vec![String::new()],
            ..TurnsConfig::default()
        };

        assert!(!is_raw_turn("project", Some("decision"), None, &config));
    }
}
