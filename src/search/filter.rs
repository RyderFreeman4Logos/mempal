use crate::core::project::ProjectFilterMode;

#[derive(Debug, Clone, Copy)]
pub struct RetrievalFilterParamIndexes {
    pub wing: usize,
    pub room: usize,
    pub project_mode: usize,
    pub project_id: usize,
    pub memory_kind: usize,
    pub domain: usize,
    pub field: usize,
    pub tier: usize,
    pub status: usize,
    pub anchor_kind: usize,
}

pub fn build_filter_clause(
    alias: &str,
    wing_param: usize,
    room_param: usize,
    project_mode_param: usize,
    project_id_param: usize,
) -> String {
    let prefix = if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    };

    format!(
        "WHERE {prefix}deleted_at IS NULL \
         AND (?{wing_param} IS NULL OR {prefix}wing = ?{wing_param}) \
         AND (?{room_param} IS NULL OR {prefix}room = ?{room_param}) \
         AND (\
             ?{project_mode_param} = 'all' \
             OR (?{project_mode_param} = 'project' AND {prefix}project_id = ?{project_id_param}) \
             OR (?{project_mode_param} = 'project_plus_global' AND ({prefix}project_id = ?{project_id_param} OR {prefix}project_id IS NULL)) \
             OR (?{project_mode_param} = 'null_only' AND {prefix}project_id IS NULL)\
         )"
    )
}

pub fn build_retrieval_filter_clause(alias: &str, params: RetrievalFilterParamIndexes) -> String {
    let prefix = alias_prefix(alias);
    format!(
        "{} \
         AND (?{} IS NULL OR {prefix}memory_kind = ?{}) \
         AND (?{} IS NULL OR {prefix}domain = ?{}) \
         AND (?{} IS NULL OR {prefix}field = ?{}) \
         AND (?{} IS NULL OR {prefix}tier = ?{}) \
         AND (?{} IS NULL OR {prefix}status = ?{}) \
         AND (?{} IS NULL OR {prefix}anchor_kind = ?{})",
        build_filter_clause(
            alias,
            params.wing,
            params.room,
            params.project_mode,
            params.project_id
        ),
        params.memory_kind,
        params.memory_kind,
        params.domain,
        params.domain,
        params.field,
        params.field,
        params.tier,
        params.tier,
        params.status,
        params.status,
        params.anchor_kind,
        params.anchor_kind
    )
}

fn alias_prefix(alias: &str) -> String {
    if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    }
}

pub fn build_vector_search_sql(_mode: ProjectFilterMode) -> String {
    format!(
        r#"
        WITH matches AS (
            SELECT id, distance
            FROM drawer_vectors v
            WHERE embedding MATCH vec_f32(?1)
              AND k = ?2
              AND (
                  ?3 = 'all'
                  OR (?3 = 'project' AND v.project_id = ?4)
                  OR (?3 = 'project_plus_global' AND (v.project_id = ?4 OR v.project_id IS NULL))
                  OR (?3 = 'null_only' AND v.project_id IS NULL)
              )
        )
        SELECT d.id, d.content, d.wing, d.room, d.source_file, d.project_id, matches.distance
        FROM matches
        JOIN drawers d ON d.id = matches.id
        {}
        ORDER BY matches.distance ASC
        LIMIT ?7
        "#,
        build_filter_clause("d", 5, 6, 3, 4)
    )
}

pub fn build_fts_runtime_sql() -> String {
    r#"
        SELECT d.id, fts.rank
        FROM drawers_fts fts
        JOIN drawers d ON d.rowid = fts.rowid
        WHERE drawers_fts MATCH ?1
          AND (?2 IS NULL OR d.wing = ?2)
          AND (?3 IS NULL OR d.room = ?3)
          AND d.deleted_at IS NULL
          AND (
              ?4 = 'all'
              OR (?4 = 'project' AND d.project_id = ?5)
              OR (?4 = 'project_plus_global' AND (d.project_id = ?5 OR d.project_id IS NULL))
              OR (?4 = 'null_only' AND d.project_id IS NULL)
          )
          AND (?7 IS NULL OR d.memory_kind = ?7)
          AND (?8 IS NULL OR d.domain = ?8)
          AND (?9 IS NULL OR d.field = ?9)
          AND (?10 IS NULL OR d.tier = ?10)
          AND (?11 IS NULL OR d.status = ?11)
          AND (?12 IS NULL OR d.anchor_kind = ?12)
        ORDER BY fts.rank
        LIMIT ?6
        "#
    .to_string()
}

pub fn build_fts_search_sql(mode: ProjectFilterMode) -> String {
    build_fts_runtime_sql().replace("?4", &format!("'{}'", mode.as_sql_mode()))
}

#[cfg(test)]
mod tests {
    use super::{
        RetrievalFilterParamIndexes, build_fts_search_sql, build_retrieval_filter_clause,
        build_vector_search_sql,
    };
    use crate::core::project::ProjectFilterMode;

    #[test]
    fn test_vector_recall_project_filter_pushed_to_sql() {
        let sql = build_vector_search_sql(ProjectFilterMode::ProjectScoped);
        assert!(
            sql.contains("v.project_id = ?4"),
            "vector SQL must push project_id filter into the vector CTE: {sql}"
        );
    }

    #[test]
    fn test_fts5_recall_project_filter_pushed_to_sql() {
        let sql = build_fts_search_sql(ProjectFilterMode::ProjectScoped);
        assert!(
            sql.contains("d.project_id = ?5") || sql.contains("d.project_id = 'project'"),
            "fts SQL must reference project_id in SQL: {sql}"
        );
    }

    #[test]
    fn test_fts5_recall_typed_scope_filters_pushed_to_sql() {
        let sql = build_fts_search_sql(ProjectFilterMode::AllProjects);
        for fragment in [
            "d.memory_kind = ?7",
            "d.domain = ?8",
            "d.field = ?9",
            "d.status = ?11",
        ] {
            assert!(
                sql.contains(fragment),
                "fts SQL must push typed scope fragment `{fragment}`: {sql}"
            );
        }
    }

    #[test]
    fn test_retrieval_filter_clause_includes_typed_scope() {
        let sql = build_retrieval_filter_clause(
            "d",
            RetrievalFilterParamIndexes {
                wing: 1,
                room: 2,
                project_mode: 3,
                project_id: 4,
                memory_kind: 5,
                domain: 6,
                field: 7,
                tier: 8,
                status: 9,
                anchor_kind: 10,
            },
        );
        for fragment in [
            "d.memory_kind = ?5",
            "d.domain = ?6",
            "d.field = ?7",
            "d.status = ?9",
        ] {
            assert!(
                sql.contains(fragment),
                "retrieval filter SQL must include `{fragment}`: {sql}"
            );
        }
    }
}
