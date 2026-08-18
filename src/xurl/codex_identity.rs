use sha2::{Digest, Sha256};

use crate::xurl::model::{RawTurn, Tool};

pub(crate) fn stable_turn_id(turn: &RawTurn) -> Option<String> {
    let message_id = codex_message_id(turn)?;
    Some(format!(
        "turn_{:x}",
        Sha256::digest(format!("codex\0{}\0{message_id}", turn.session_id).as_bytes())
    ))
}

pub(crate) fn stable_turn_index(turn: &RawTurn) -> Option<u32> {
    let message_id = codex_message_id(turn)?;
    let digest = Sha256::digest(format!("{}\0{message_id}", turn.session_id).as_bytes());
    Some(u32::from_le_bytes([
        digest[0], digest[1], digest[2], digest[3],
    ]))
}

fn codex_message_id(turn: &RawTurn) -> Option<&str> {
    (turn.tool == Tool::Codex).then_some(turn.metadata.message_id.as_deref())?
}
