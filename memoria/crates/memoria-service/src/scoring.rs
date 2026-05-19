//! Scoring plugin system for extensible retrieval ranking.
//!
//! This module provides:
//! - `ScoringPlugin` trait for custom scoring logic
//! - `ScoringStore` trait for storage operations needed by scoring
//! - `DefaultScoringPlugin` with feedback-based score adjustment
//! - `AdaptiveTuner` for auto-tuning user parameters from feedback history

use async_trait::async_trait;
use memoria_core::MemoriaError;
use memoria_storage::{MemoryFeedback, SqlMemoryStore, UserRetrievalParams};

/// Storage operations needed by scoring plugins.
#[async_trait]
pub trait ScoringStore: Send + Sync {
    /// Get user's retrieval parameters.
    async fn get_user_params(&self, user_id: &str) -> Result<UserRetrievalParams, MemoriaError>;

    /// Update user's retrieval parameters.
    async fn set_user_params(&self, params: &UserRetrievalParams) -> Result<(), MemoriaError>;

    /// Get total feedback counts for a user.
    async fn get_feedback_totals(&self, user_id: &str) -> Result<FeedbackTotals, MemoriaError>;
}

#[async_trait]
impl ScoringStore for SqlMemoryStore {
    async fn get_user_params(&self, user_id: &str) -> Result<UserRetrievalParams, MemoriaError> {
        self.get_user_retrieval_params(user_id).await
    }

    async fn set_user_params(&self, params: &UserRetrievalParams) -> Result<(), MemoriaError> {
        self.set_user_retrieval_params(params).await
    }

    async fn get_feedback_totals(&self, user_id: &str) -> Result<FeedbackTotals, MemoriaError> {
        let stats = self.get_feedback_stats(user_id).await?;
        Ok(FeedbackTotals {
            useful: stats.useful,
            irrelevant: stats.irrelevant,
            outdated: stats.outdated,
            wrong: stats.wrong,
        })
    }
}

/// Aggregated feedback totals for a user.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FeedbackTotals {
    pub useful: i64,
    pub irrelevant: i64,
    pub outdated: i64,
    pub wrong: i64,
}

/// Scoring plugin trait for extensible retrieval ranking.
#[async_trait]
pub trait ScoringPlugin: Send + Sync {
    /// Plugin identifier.
    fn plugin_key(&self) -> &str;

    /// Adjust a base score using feedback and user parameters.
    fn adjust_score(
        &self,
        base_score: f64,
        feedback: &MemoryFeedback,
        params: &UserRetrievalParams,
    ) -> f64;

    /// Auto-tune user parameters based on feedback history.
    /// Returns updated params if tuning was performed, None if insufficient data.
    async fn tune_params(
        &self,
        store: &dyn ScoringStore,
        user_id: &str,
    ) -> Result<Option<UserRetrievalParams>, MemoriaError>;
}

/// Default scoring plugin with feedback-based adjustment.
#[derive(Debug, Default)]
pub struct DefaultScoringPlugin;

impl DefaultScoringPlugin {
    /// Minimum feedback count before auto-tuning kicks in.
    const MIN_FEEDBACK_FOR_TUNING: i64 = 10;
}

#[async_trait]
impl ScoringPlugin for DefaultScoringPlugin {
    fn plugin_key(&self) -> &str {
        "scoring:default:v1"
    }

    fn adjust_score(
        &self,
        base_score: f64,
        feedback: &MemoryFeedback,
        params: &UserRetrievalParams,
    ) -> f64 {
        let positive = feedback.useful as f64;
        let negative = (feedback.irrelevant + feedback.outdated + feedback.wrong) as f64;
        let feedback_delta = positive - 0.5 * negative;

        if feedback_delta.abs() > 0.01 {
            base_score * (1.0 + params.feedback_weight * feedback_delta).clamp(0.5, 2.0)
        } else {
            base_score
        }
    }

    async fn tune_params(
        &self,
        store: &dyn ScoringStore,
        user_id: &str,
    ) -> Result<Option<UserRetrievalParams>, MemoriaError> {
        let totals = store.get_feedback_totals(user_id).await?;
        let total = totals.useful + totals.irrelevant + totals.outdated + totals.wrong;

        if total < Self::MIN_FEEDBACK_FOR_TUNING {
            return Ok(None);
        }

        let mut params = store.get_user_params(user_id).await?;

        // Adaptive tuning logic:
        // - High useful ratio → increase feedback_weight (trust feedback more)
        // - High negative ratio → decrease feedback_weight (be more conservative)
        let useful_ratio = totals.useful as f64 / total as f64;
        let negative_ratio = (totals.irrelevant + totals.wrong) as f64 / total as f64;

        // Adjust feedback_weight: range [0.05, 0.2]
        if useful_ratio > 0.7 {
            // User gives mostly positive feedback → trust it more
            params.feedback_weight = (params.feedback_weight * 1.1).min(0.2);
        } else if negative_ratio > 0.5 {
            // User gives mostly negative feedback → be more conservative
            params.feedback_weight = (params.feedback_weight * 0.9).max(0.05);
        }

        // Round to 3 decimal places for cleaner storage
        params.feedback_weight = (params.feedback_weight * 1000.0).round() / 1000.0;

        store.set_user_params(&params).await?;
        Ok(Some(params))
    }
}

/// Result of an auto-tuning run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TuningResult {
    pub user_id: String,
    pub tuned: bool,
    pub old_params: Option<UserRetrievalParams>,
    pub new_params: Option<UserRetrievalParams>,
    pub feedback_count: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_scoring_adjustment() {
        let plugin = DefaultScoringPlugin;
        let params = UserRetrievalParams::default();

        // No feedback → no change
        let fb_none = MemoryFeedback::default();
        assert!((plugin.adjust_score(1.0, &fb_none, &params) - 1.0).abs() < 0.001);

        // 2 useful → 1.2x boost (0.1 * 2 = 0.2)
        let fb_useful = MemoryFeedback { useful: 2, ..Default::default() };
        assert!((plugin.adjust_score(1.0, &fb_useful, &params) - 1.2).abs() < 0.001);

        // 2 wrong → 0.9x penalty (0.1 * -1 = -0.1)
        let fb_wrong = MemoryFeedback { wrong: 2, ..Default::default() };
        assert!((plugin.adjust_score(1.0, &fb_wrong, &params) - 0.9).abs() < 0.001);

        // 2 useful, 3 wrong → 1.05x (2 - 1.5 = 0.5, 0.1 * 0.5 = 0.05)
        let fb_mixed = MemoryFeedback { useful: 2, wrong: 3, ..Default::default() };
        assert!((plugin.adjust_score(1.0, &fb_mixed, &params) - 1.05).abs() < 0.001);
    }

    #[test]
    fn test_scoring_clamp() {
        let plugin = DefaultScoringPlugin;
        let params = UserRetrievalParams::default();

        // Extreme positive → clamped to 2.0
        let fb_extreme = MemoryFeedback { useful: 100, ..Default::default() };
        assert!((plugin.adjust_score(1.0, &fb_extreme, &params) - 2.0).abs() < 0.001);

        // Extreme negative → clamped to 0.5
        let fb_extreme_neg = MemoryFeedback { wrong: 100, ..Default::default() };
        assert!((plugin.adjust_score(1.0, &fb_extreme_neg, &params) - 0.5).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_tune_params_negative_feedback_decreases_weight() {
        // Mock ScoringStore with mostly negative feedback (negative_ratio > 0.5)
        struct MockStore {
            totals: FeedbackTotals,
            params: std::sync::Mutex<UserRetrievalParams>,
        }
        #[async_trait::async_trait]
        impl ScoringStore for MockStore {
            async fn get_user_params(&self, _: &str) -> Result<UserRetrievalParams, MemoriaError> {
                Ok(self.params.lock().unwrap().clone())
            }
            async fn set_user_params(&self, p: &UserRetrievalParams) -> Result<(), MemoriaError> {
                *self.params.lock().unwrap() = p.clone();
                Ok(())
            }
            async fn get_feedback_totals(&self, _: &str) -> Result<FeedbackTotals, MemoriaError> {
                Ok(self.totals.clone())
            }
        }

        let plugin = DefaultScoringPlugin;

        // Case 1: negative_ratio > 0.5 → weight decreases
        let store = MockStore {
            totals: FeedbackTotals { useful: 2, irrelevant: 5, outdated: 0, wrong: 5 },
            params: std::sync::Mutex::new(UserRetrievalParams::default()),
        };
        let result = plugin.tune_params(&store, "u1").await.unwrap().unwrap();
        assert!(
            (result.feedback_weight - 0.09).abs() < 0.001,
            "negative feedback should decrease weight: got {:.4}", result.feedback_weight
        );

        // Case 2: neutral zone (neither branch) → weight unchanged
        let store2 = MockStore {
            totals: FeedbackTotals { useful: 5, irrelevant: 3, outdated: 2, wrong: 2 },
            params: std::sync::Mutex::new(UserRetrievalParams::default()),
        };
        let result2 = plugin.tune_params(&store2, "u2").await.unwrap().unwrap();
        assert!(
            (result2.feedback_weight - 0.1).abs() < 0.001,
            "neutral feedback should not change weight: got {:.4}", result2.feedback_weight
        );

        // Case 3: weight clamped at max 0.2
        let store3 = MockStore {
            totals: FeedbackTotals { useful: 10, irrelevant: 0, outdated: 0, wrong: 0 },
            params: std::sync::Mutex::new(UserRetrievalParams { feedback_weight: 0.19, ..Default::default() }),
        };
        let result3 = plugin.tune_params(&store3, "u3").await.unwrap().unwrap();
        assert!(
            (result3.feedback_weight - 0.2).abs() < 0.001,
            "weight should be clamped at 0.2: got {:.4}", result3.feedback_weight
        );

        // Case 4: weight clamped at min 0.05
        let store4 = MockStore {
            totals: FeedbackTotals { useful: 0, irrelevant: 6, outdated: 0, wrong: 6 },
            params: std::sync::Mutex::new(UserRetrievalParams { feedback_weight: 0.051, ..Default::default() }),
        };
        let result4 = plugin.tune_params(&store4, "u4").await.unwrap().unwrap();
        assert!(
            (result4.feedback_weight - 0.05).abs() < 0.001,
            "weight should be clamped at 0.05: got {:.4}", result4.feedback_weight
        );
    }

    /// Verify that different feedback_weight values produce measurably different
    /// ranking outcomes for the same feedback signals.
    #[test]
    fn test_feedback_weight_affects_ranking_spread() {
        let plugin = DefaultScoringPlugin;
        let fb = MemoryFeedback { useful: 3, wrong: 1, ..Default::default() };
        // net delta = 3 - 0.5*1 = 2.5

        let weights = [0.05, 0.1, 0.15, 0.2];
        let scores: Vec<f64> = weights
            .iter()
            .map(|&w| {
                let p = UserRetrievalParams { feedback_weight: w, ..Default::default() };
                plugin.adjust_score(1.0, &fb, &p)
            })
            .collect();

        // Each higher weight must produce a strictly higher score
        for i in 1..scores.len() {
            assert!(
                scores[i] > scores[i - 1],
                "weight {:.2} score {:.4} should > weight {:.2} score {:.4}",
                weights[i], scores[i], weights[i - 1], scores[i - 1]
            );
        }

        // Verify exact values: multiplier = 1 + weight * 2.5
        assert!((scores[0] - 1.125).abs() < 0.001, "w=0.05: {}", scores[0]);
        assert!((scores[1] - 1.25).abs() < 0.001, "w=0.10: {}", scores[1]);
        assert!((scores[2] - 1.375).abs() < 0.001, "w=0.15: {}", scores[2]);
        assert!((scores[3] - 1.5).abs() < 0.001, "w=0.20: {}", scores[3]);
    }

    /// Verify that two memories with identical base scores but different feedback
    /// produce the correct relative ordering.
    #[test]
    fn test_feedback_ranking_order_with_equal_base_scores() {
        let plugin = DefaultScoringPlugin;
        let params = UserRetrievalParams::default(); // weight=0.1

        let base = 0.85;
        let fb_positive = MemoryFeedback { useful: 4, ..Default::default() };
        let fb_neutral = MemoryFeedback::default();
        let fb_negative = MemoryFeedback { irrelevant: 2, wrong: 2, ..Default::default() };

        let s_pos = plugin.adjust_score(base, &fb_positive, &params);
        let s_neu = plugin.adjust_score(base, &fb_neutral, &params);
        let s_neg = plugin.adjust_score(base, &fb_negative, &params);

        assert!(s_pos > s_neu, "positive {s_pos} > neutral {s_neu}");
        assert!(s_neu > s_neg, "neutral {s_neu} > negative {s_neg}");

        // Exact: pos = 0.85 * 1.4 = 1.19, neu = 0.85, neg = 0.85 * 0.8 = 0.68
        assert!((s_pos - 1.19).abs() < 0.001);
        assert!((s_neu - 0.85).abs() < 0.001);
        assert!((s_neg - 0.68).abs() < 0.001);
    }
}
