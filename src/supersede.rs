use std::collections::BTreeSet;

use crate::context::{ContextError, ContextPack};
use crate::core::db::{Database, DbError};

pub fn superseded_drawer_ids(db: &Database, hit_ids: &[&str]) -> Result<BTreeSet<String>, DbError> {
    let mut superseded = BTreeSet::new();
    let mut stack = hit_ids
        .iter()
        .map(|id| (*id).to_string())
        .collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let Some(drawer) = db.get_drawer(&id)? else {
            continue;
        };
        if let Some(old_id) = drawer.supersedes {
            superseded.insert(old_id.clone());
            stack.push(old_id);
        }
    }
    Ok(superseded)
}

pub fn strip_superseded_from_pack(
    db: &Database,
    pack: &mut ContextPack,
) -> Result<(), ContextError> {
    let mut hit_ids = pack
        .sections
        .iter()
        .flat_map(|section| section.items.iter().map(|item| item.drawer_id.clone()))
        .collect::<Vec<_>>();
    if let Some(tiered) = &pack.tiered {
        hit_ids.extend(
            tiered
                .t1_items
                .iter()
                .chain(&tiered.t2_items)
                .chain(&tiered.t3_items)
                .map(|item| item.drawer_id.clone()),
        );
    }
    let refs = hit_ids.iter().map(String::as_str).collect::<Vec<_>>();
    let superseded = superseded_drawer_ids(db, &refs).map_err(ContextError::LoadDrawer)?;
    if superseded.is_empty() {
        return Ok(());
    }
    for section in &mut pack.sections {
        section
            .items
            .retain(|item| !superseded.contains(&item.drawer_id));
    }
    pack.sections.retain(|section| !section.items.is_empty());
    if let Some(tiered) = pack.tiered.as_mut() {
        tiered
            .t1_items
            .retain(|item| !superseded.contains(&item.drawer_id));
        tiered
            .t2_items
            .retain(|item| !superseded.contains(&item.drawer_id));
        tiered
            .t3_items
            .retain(|item| !superseded.contains(&item.drawer_id));
    }
    Ok(())
}

pub fn assemble_with_vector(
    db: &Database,
    request: crate::context::ContextRequest,
    query_vector: &[f32],
) -> Result<ContextPack, ContextError> {
    let cfg = request
        .context_cfg_override
        .clone()
        .unwrap_or_else(|| crate::core::config::ConfigHandle::current().context.clone());
    let mut pack = if cfg.tiered_retrieval_enabled {
        crate::context::assemble_tiered(db, request, query_vector, &cfg)?
    } else {
        crate::context::assemble_flat(db, request, query_vector)?
    };
    strip_superseded_from_pack(db, &mut pack)?;
    Ok(pack)
}
