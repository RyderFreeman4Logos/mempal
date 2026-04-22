use crate::core::project::ProjectFilterMode;

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
    use super::{build_fts_search_sql, build_vector_search_sql};
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
}
