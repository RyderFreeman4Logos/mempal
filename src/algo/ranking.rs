#![warn(clippy::all)]

//! Stateless ranking operators for memory retrieval.
//!
//! The APIs in this module consume ranked identifiers or caller-owned memory
//! items. They deliberately know nothing about SQLite rows, embedders, daemon
//! handles, or transport layers.

use std::cmp::Ordering;
use std::collections::HashMap;

/// A memory-like item that can be ranked without store access.
pub trait RankedMemoryItem {
    /// Stable memory identifier used for deterministic tie-breaking.
    fn memory_id(&self) -> &str;

    /// Primary retrieval score, where larger is better.
    fn similarity_score(&self) -> f32;

    /// Secondary importance score, where larger is better.
    fn effective_importance(&self) -> f64;
}

/// One fused memory reference emitted by reciprocal rank fusion.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedRank {
    pub id: String,
    pub score: f64,
}

/// Stateless Reciprocal Rank Fusion operator.
///
/// The operator accepts any number of ranked identifier lists and returns one
/// deterministic fused list. It does not load missing records; callers decide
/// how to hydrate the returned IDs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReciprocalRankFusion {
    rank_constant: f64,
}

impl ReciprocalRankFusion {
    /// Create an RRF operator with an explicit rank constant.
    pub fn new(rank_constant: f64) -> Self {
        Self { rank_constant }
    }

    /// Fuse ranked lists into descending RRF score order.
    ///
    /// Ties resolve by ascending memory id so equal-score merges are stable
    /// across hash seeds, platforms, and process runs.
    pub fn fuse<'a, Lists, List>(self, ranked_lists: Lists) -> Vec<FusedRank>
    where
        Lists: IntoIterator<Item = List>,
        List: IntoIterator<Item = &'a str>,
    {
        let mut scores = HashMap::<String, f64>::new();
        for list in ranked_lists {
            for (rank, id) in list.into_iter().enumerate() {
                let score = 1.0 / (self.rank_constant + rank as f64 + 1.0);
                *scores.entry(id.to_string()).or_default() += score;
            }
        }

        let mut fused = scores
            .into_iter()
            .map(|(id, score)| FusedRank { id, score })
            .collect::<Vec<_>>();
        fused.sort_by(|left, right| {
            compare_score_desc_then_id(left.score, &left.id, right.score, &right.id)
        });
        fused
    }
}

impl Default for ReciprocalRankFusion {
    fn default() -> Self {
        Self::new(60.0)
    }
}

/// Sort by descending similarity and ascending id.
pub fn sort_by_similarity_desc_then_id<T: RankedMemoryItem>(items: &mut [T]) {
    items.sort_by(|left, right| {
        compare_score_desc_then_id(
            f64::from(left.similarity_score()),
            left.memory_id(),
            f64::from(right.similarity_score()),
            right.memory_id(),
        )
    });
}

/// Stable secondary rerank by descending effective importance.
///
/// Equal or non-comparable importance values preserve the incoming order. This
/// lets callers keep the primary retrieval order from RRF or vector search.
pub fn rerank_by_effective_importance<T: RankedMemoryItem>(items: &mut [T]) {
    items.sort_by(|left, right| {
        right
            .effective_importance()
            .partial_cmp(&left.effective_importance())
            .unwrap_or(Ordering::Equal)
    });
}

fn compare_score_desc_then_id(
    left_score: f64,
    left_id: &str,
    right_score: f64,
    right_id: &str,
) -> Ordering {
    right_score
        .partial_cmp(&left_score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| left_id.cmp(right_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct Item {
        id: &'static str,
        similarity: f32,
        importance: f64,
    }

    impl RankedMemoryItem for Item {
        fn memory_id(&self) -> &str {
            self.id
        }

        fn similarity_score(&self) -> f32 {
            self.similarity
        }

        fn effective_importance(&self) -> f64 {
            self.importance
        }
    }

    fn ids(items: &[Item]) -> Vec<&str> {
        items.iter().map(|item| item.id).collect()
    }

    #[test]
    fn rrf_fuses_ranked_lists_and_tie_breaks_by_id() {
        let fused = ReciprocalRankFusion::default().fuse([
            vec!["tie_b", "tie_a", "vector_only"],
            vec!["tie_a", "tie_b", "bm25_only"],
        ]);

        let ids = fused
            .iter()
            .map(|rank| rank.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["tie_a", "tie_b", "bm25_only", "vector_only"]);
        assert_eq!(fused[0].score, fused[1].score);
    }

    #[test]
    fn similarity_sort_orders_by_score_then_id() {
        let mut items = vec![
            Item {
                id: "b",
                similarity: 0.7,
                importance: 0.0,
            },
            Item {
                id: "a",
                similarity: 0.7,
                importance: 0.0,
            },
            Item {
                id: "c",
                similarity: 0.9,
                importance: 0.0,
            },
        ];

        sort_by_similarity_desc_then_id(&mut items);

        assert_eq!(ids(&items), vec!["c", "a", "b"]);
    }

    #[test]
    fn importance_rerank_is_stable_for_equal_scores() {
        let mut items = vec![
            Item {
                id: "rrf-first",
                similarity: 0.3,
                importance: 2.0,
            },
            Item {
                id: "rrf-second",
                similarity: 0.9,
                importance: 2.0,
            },
            Item {
                id: "important",
                similarity: 0.1,
                importance: 5.0,
            },
        ];

        rerank_by_effective_importance(&mut items);

        assert_eq!(ids(&items), vec!["important", "rrf-first", "rrf-second"]);
    }
}
