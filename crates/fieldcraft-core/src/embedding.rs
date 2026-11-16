//! Which embedding model makes the dense vectors, and the tag each stored
//! vector carries.
//!
//! This module is the one owner of that fact. Storage keeps every vector under
//! a tag (`provider:model:dims`) and only ever compares vectors that share a
//! tag; retrieval encodes with the provider named here; the server, `status`
//! and `doctor` report it. Vectors from two models never meet, so switching
//! models is: build the new tag's vectors (`fieldcraft reindex --provider`),
//! then switch the provider. The old tag's vectors stay until deleted.
//!
//! Configuration is read through [`crate::secrets`] (process environment
//! first, then `~/.fieldcraft/secrets.env`), so a reinstall that rewrites a
//! launchd plist cannot silently change the model:
//!
//! - `FIELDCRAFT_EMBED_PROVIDER`: `gemma` | `gemini` | `none`.
//! - `FIELDCRAFT_EMBED_GEMMA_MODEL`, `FIELDCRAFT_EMBED_GEMINI_MODEL`: model ids.
//! - `FIELDCRAFT_EMBED_DIM`: output dimensions (default 768).

use std::fmt;

use crate::secrets;

/// Provider used when `FIELDCRAFT_EMBED_PROVIDER` is unset.
pub const DEFAULT_PROVIDER: EmbeddingProvider = EmbeddingProvider::Gemini;
/// llama.cpp model alias the local sidecar serves EmbeddingGemma 2 under.
pub const GEMMA_DEFAULT_MODEL: &str = "embeddinggemma-2-Q8_0";
pub const GEMINI_DEFAULT_MODEL: &str = "models/gemini-embedding-001";
pub const DEFAULT_DIMS: usize = 768;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmbeddingProvider {
    /// EmbeddingGemma 2 on a local llama.cpp server; no text leaves the machine.
    Gemma,
    /// Google Gemini embedding API; lesson and query text are sent to Google.
    Gemini,
    /// No dense vectors: BM25-only retrieval, vector maintenance inert.
    None,
}

impl EmbeddingProvider {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "gemma" | "embeddinggemma" => Ok(Self::Gemma),
            "gemini" => Ok(Self::Gemini),
            "none" | "off" | "bm25" => Ok(Self::None),
            other => Err(format!(
                "FIELDCRAFT_EMBED_PROVIDER={other:?} is not one of gemma, gemini, none"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gemma => "gemma",
            Self::Gemini => "gemini",
            Self::None => "none",
        }
    }
}

impl fmt::Display for EmbeddingProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Cosine thresholds of the vector-based maintenance jobs. They depend on the
/// model: each model spreads similarities differently.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CosineThresholds {
    /// Merge: two lessons are the same lesson (also needs cue overlap).
    pub duplicate_min: f32,
    /// L4 near-twins: related but distinct, `[near_twin_min, duplicate_min)`.
    pub near_twin_min: f32,
    /// L5 dream clusters: `[dream_min, duplicate_min)`.
    pub dream_min: f32,
    /// L3 resurrection: an archived lesson matches a live query.
    pub resurrection_min: f32,
    /// Item search: a passage below this cosine to the query is not a dense
    /// candidate (`GET /search`). Lessons do not use it.
    pub item_dense_min: f32,
}

/// Calibrated on the production store with Gemini vectors (2026-06/07).
pub const GEMINI_THRESHOLDS: CosineThresholds = CosineThresholds {
    duplicate_min: 0.92,
    near_twin_min: 0.85,
    dream_min: 0.80,
    resurrection_min: 0.85,
    // Not calibrated for items: every passage is a dense candidate.
    item_dense_min: 0.0,
};

/// Calibrated 2026-11-01 on a copy of the production store by quantile matching
/// (`fieldcraft-retrieval/examples/calibrate_cosine.rs`, 2,000 sampled records x
/// 26,681 others): each Gemini threshold maps to the Gemma cosine that admits the
/// same share of pairs (0.92 -> 0.936, 89% of pairs shared; 0.85 -> 0.874, 72%;
/// 0.80 -> 0.833, 62%). Merged duplicates measure p10 0.895 / p50 0.952 under
/// Gemma; a record's own text as a query measures p10 0.962 against it, so the
/// resurrection threshold keeps a wide margin.
pub const GEMMA_THRESHOLDS: CosineThresholds = CosineThresholds {
    duplicate_min: 0.935,
    near_twin_min: 0.875,
    dream_min: 0.835,
    resurrection_min: 0.875,
    // I Want Things' calibrated `denseFloor` for the same model and prompts
    // (`EmbeddingRuntime/Resources/models.json`, profile `embeddinggemma2`).
    item_dense_min: 0.6,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingSpec {
    pub provider: EmbeddingProvider,
    pub model: String,
    pub dims: usize,
}

impl EmbeddingSpec {
    /// The configured provider (see module docs).
    pub fn from_env() -> Result<Self, String> {
        let provider = match secrets::lookup("FIELDCRAFT_EMBED_PROVIDER") {
            Some(value) => EmbeddingProvider::parse(&value)?,
            None => DEFAULT_PROVIDER,
        };
        Self::for_provider(provider)
    }

    /// `provider` with its configured model and dimensions.
    pub fn for_provider(provider: EmbeddingProvider) -> Result<Self, String> {
        let model = match provider {
            EmbeddingProvider::Gemma => secrets::lookup("FIELDCRAFT_EMBED_GEMMA_MODEL"),
            EmbeddingProvider::Gemini => secrets::lookup("FIELDCRAFT_EMBED_GEMINI_MODEL"),
            EmbeddingProvider::None => None,
        };
        let dims = secrets::lookup("FIELDCRAFT_EMBED_DIM");
        Self::from_values(provider, model.as_deref(), dims.as_deref())
    }

    pub fn from_values(
        provider: EmbeddingProvider,
        model: Option<&str>,
        dims: Option<&str>,
    ) -> Result<Self, String> {
        let default_model = match provider {
            EmbeddingProvider::Gemma => GEMMA_DEFAULT_MODEL,
            EmbeddingProvider::Gemini => GEMINI_DEFAULT_MODEL,
            EmbeddingProvider::None => "",
        };
        let model = model
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or(default_model)
            .to_string();
        let dims = match dims.map(str::trim).filter(|d| !d.is_empty()) {
            None => DEFAULT_DIMS,
            Some(value) => match value.parse::<usize>() {
                Ok(n) if n > 0 => n,
                _ => return Err("FIELDCRAFT_EMBED_DIM must be a positive integer".into()),
            },
        };
        if provider == EmbeddingProvider::None {
            return Ok(Self {
                provider,
                model: String::new(),
                dims: 0,
            });
        }
        Ok(Self {
            provider,
            model,
            dims,
        })
    }

    /// Tag stored with every vector this spec produces; `None` in BM25-only mode.
    pub fn tag(&self) -> Option<String> {
        match self.provider {
            EmbeddingProvider::None => None,
            provider => Some(format!("{provider}:{}:{}", self.model, self.dims)),
        }
    }

    pub fn is_dense(&self) -> bool {
        self.provider != EmbeddingProvider::None
    }

    pub fn thresholds(&self) -> CosineThresholds {
        thresholds_for_tag(self.tag().as_deref())
    }
}

/// Thresholds for vectors with `tag` (by provider prefix).
pub fn thresholds_for_tag(tag: Option<&str>) -> CosineThresholds {
    match tag.and_then(|t| t.split(':').next()) {
        Some("gemma") => GEMMA_THRESHOLDS,
        _ => GEMINI_THRESHOLDS,
    }
}

/// The tag of the configured provider, or `None` (BM25-only, or invalid
/// configuration, which the server and `doctor` report separately).
pub fn active_tag() -> Option<String> {
    EmbeddingSpec::from_env().ok().and_then(|spec| spec.tag())
}

/// Tag given to vectors that existed before tagging (all made by Gemini at
/// its default model; other lengths are unknown legacy models).
pub fn legacy_tag_for_dims(dims: usize) -> String {
    if dims == DEFAULT_DIMS {
        format!("gemini:{GEMINI_DEFAULT_MODEL}:{dims}")
    } else {
        format!("legacy:unknown:{dims}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_name_provider_model_and_dims() {
        let gemma = EmbeddingSpec::from_values(EmbeddingProvider::Gemma, None, None).unwrap();
        assert_eq!(gemma.tag().unwrap(), "gemma:embeddinggemma-2-Q8_0:768");
        let gemini =
            EmbeddingSpec::from_values(EmbeddingProvider::Gemini, None, Some("256")).unwrap();
        assert_eq!(
            gemini.tag().unwrap(),
            "gemini:models/gemini-embedding-001:256"
        );
        let none = EmbeddingSpec::from_values(EmbeddingProvider::None, None, None).unwrap();
        assert_eq!(none.tag(), None);
        assert!(!none.is_dense());
        assert_eq!(legacy_tag_for_dims(768), gemini_default_tag());
        assert_eq!(legacy_tag_for_dims(384), "legacy:unknown:384");
    }

    fn gemini_default_tag() -> String {
        EmbeddingSpec::from_values(EmbeddingProvider::Gemini, None, None)
            .unwrap()
            .tag()
            .unwrap()
    }

    #[test]
    fn provider_parsing_rejects_unknown_values() {
        assert_eq!(
            EmbeddingProvider::parse(" Gemma ").unwrap(),
            EmbeddingProvider::Gemma
        );
        assert_eq!(
            EmbeddingProvider::parse("none").unwrap(),
            EmbeddingProvider::None
        );
        assert!(EmbeddingProvider::parse("openai").is_err());
        assert!(EmbeddingSpec::from_values(EmbeddingProvider::Gemma, None, Some("0")).is_err());
    }

    #[test]
    fn thresholds_follow_the_tag_provider() {
        assert_eq!(
            thresholds_for_tag(Some("gemini:models/x:768")),
            GEMINI_THRESHOLDS
        );
        assert_eq!(
            thresholds_for_tag(Some("gemma:embeddinggemma-2-Q8_0:768")),
            GEMMA_THRESHOLDS
        );
        const {
            assert!(GEMMA_THRESHOLDS.duplicate_min > GEMINI_THRESHOLDS.duplicate_min);
            assert!(GEMMA_THRESHOLDS.dream_min < GEMMA_THRESHOLDS.near_twin_min);
            assert!(GEMMA_THRESHOLDS.near_twin_min < GEMMA_THRESHOLDS.duplicate_min);
        }
    }
}
