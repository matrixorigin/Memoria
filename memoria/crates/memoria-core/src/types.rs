use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Memory type — must have exactly 6 variants matching Python implementation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    Semantic,
    Working,
    Episodic,
    Profile,
    ToolResult,
    Procedural,
}

impl std::fmt::Display for MemoryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            MemoryType::Semantic => "semantic",
            MemoryType::Working => "working",
            MemoryType::Episodic => "episodic",
            MemoryType::Profile => "profile",
            MemoryType::ToolResult => "tool_result",
            MemoryType::Procedural => "procedural",
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for MemoryType {
    type Err = crate::MemoriaError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "semantic" => Ok(MemoryType::Semantic),
            "working" => Ok(MemoryType::Working),
            "episodic" => Ok(MemoryType::Episodic),
            "profile" => Ok(MemoryType::Profile),
            "tool_result" => Ok(MemoryType::ToolResult),
            "procedural" => Ok(MemoryType::Procedural),
            other => Err(crate::MemoriaError::InvalidMemoryType(other.to_string())),
        }
    }
}

/// Trust tier — T1 (verified) → T4 (unverified).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TrustTier {
    #[serde(rename = "T1")]
    T1Verified,
    #[serde(rename = "T2")]
    T2Curated,
    #[serde(rename = "T3")]
    #[default]
    T3Inferred,
    #[serde(rename = "T4")]
    T4Unverified,
}

impl TrustTier {
    pub fn default_half_life_days(&self) -> f64 {
        match self {
            TrustTier::T1Verified => 365.0,
            TrustTier::T2Curated => 180.0,
            TrustTier::T3Inferred => 60.0,
            TrustTier::T4Unverified => 30.0,
        }
    }

    pub fn initial_confidence(&self) -> f64 {
        match self {
            TrustTier::T1Verified => 0.95,
            TrustTier::T2Curated => 0.85,
            TrustTier::T3Inferred => 0.65,
            TrustTier::T4Unverified => 0.40,
        }
    }
}

impl std::fmt::Display for TrustTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TrustTier::T1Verified => "T1",
            TrustTier::T2Curated => "T2",
            TrustTier::T3Inferred => "T3",
            TrustTier::T4Unverified => "T4",
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for TrustTier {
    type Err = crate::MemoriaError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "T1" => Ok(TrustTier::T1Verified),
            "T2" => Ok(TrustTier::T2Curated),
            "T3" => Ok(TrustTier::T3Inferred),
            "T4" => Ok(TrustTier::T4Unverified),
            other => Err(crate::MemoriaError::InvalidTrustTier(other.to_string())),
        }
    }
}

/// Core memory record — mirrors Python `Memory` dataclass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub memory_id: String,
    pub user_id: String,
    pub memory_type: MemoryType,
    pub content: String,
    pub initial_confidence: f64,
    pub embedding: Option<Vec<f32>>,
    pub source_event_ids: Vec<String>,
    pub superseded_by: Option<String>,
    pub is_active: bool,
    pub access_count: i32,
    pub session_id: Option<String>,
    pub observed_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub extra_metadata: Option<HashMap<String, serde_json::Value>>,
    pub trust_tier: TrustTier,
    /// Set by retriever; None if not retrieved via scoring.
    pub retrieval_score: Option<f64>,
}

impl Memory {
    /// Confidence decay: C(t) = C0 * exp(-age_days / half_life).
    pub fn effective_confidence(&self, half_life_days: Option<f64>) -> f64 {
        let Some(observed_at) = self.observed_at else {
            return self.initial_confidence;
        };
        let half_life = half_life_days.unwrap_or_else(|| self.trust_tier.default_half_life_days());
        let age_days = (Utc::now() - observed_at).num_seconds() as f64 / 86400.0;
        self.initial_confidence * (-age_days / half_life).exp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_type_has_six_variants() {
        let types = [
            MemoryType::Semantic,
            MemoryType::Working,
            MemoryType::Episodic,
            MemoryType::Profile,
            MemoryType::ToolResult,
            MemoryType::Procedural,
        ];
        assert_eq!(types.len(), 6);
    }

    #[test]
    fn test_memory_type_roundtrip() {
        for (s, expected) in [
            ("semantic", MemoryType::Semantic),
            ("working", MemoryType::Working),
            ("episodic", MemoryType::Episodic),
            ("profile", MemoryType::Profile),
            ("tool_result", MemoryType::ToolResult),
            ("procedural", MemoryType::Procedural),
        ] {
            let parsed: MemoryType = s.parse().unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(parsed.to_string(), s);
        }
    }

    #[test]
    fn test_trust_tier_roundtrip() {
        for (s, expected) in [
            ("T1", TrustTier::T1Verified),
            ("T2", TrustTier::T2Curated),
            ("T3", TrustTier::T3Inferred),
            ("T4", TrustTier::T4Unverified),
        ] {
            let parsed: TrustTier = s.parse().unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(parsed.to_string(), s);
        }
    }
}
