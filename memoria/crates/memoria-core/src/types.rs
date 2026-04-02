use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Memory type classification.
///
/// The built-in variants cover standard agent memory categories.
/// `Custom(String)` allows downstream applications to define their own
/// domain-specific types (e.g. `brand_theme`, `layout_catalog`) without
/// requiring changes to Memoria itself.
///
/// Custom types are stored as-is in the database `memory_type VARCHAR(64)`
/// column and participate in retrieval filtering just like built-in types.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MemoryType {
    Semantic,
    Working,
    Episodic,
    Profile,
    ToolResult,
    Procedural,
    /// Application-defined memory type. The inner string is stored verbatim.
    Custom(String),
}

impl MemoryType {
    /// Returns `true` for the six built-in variants, `false` for `Custom`.
    pub fn is_builtin(&self) -> bool {
        !matches!(self, MemoryType::Custom(_))
    }
}

impl Serialize for MemoryType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for MemoryType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(s.parse().expect("MemoryType::from_str is infallible"))
    }
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
            MemoryType::Custom(name) => name.as_str(),
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for MemoryType {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "semantic" => MemoryType::Semantic,
            "working" => MemoryType::Working,
            "episodic" => MemoryType::Episodic,
            "profile" => MemoryType::Profile,
            "tool_result" => MemoryType::ToolResult,
            "procedural" => MemoryType::Procedural,
            other => MemoryType::Custom(other.to_string()),
        })
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
    fn test_builtin_types_roundtrip() {
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
            assert!(parsed.is_builtin());
        }
    }

    #[test]
    fn test_custom_type_roundtrip() {
        let parsed: MemoryType = "brand_theme".parse().unwrap();
        assert_eq!(parsed, MemoryType::Custom("brand_theme".to_string()));
        assert_eq!(parsed.to_string(), "brand_theme");
        assert!(!parsed.is_builtin());
    }

    #[test]
    fn test_custom_type_serde_roundtrip() {
        let mt = MemoryType::Custom("layout_catalog".to_string());
        let json = serde_json::to_string(&mt).unwrap();
        assert_eq!(json, "\"layout_catalog\"");
        let back: MemoryType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, mt);
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
