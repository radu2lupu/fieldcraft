use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub mod cue_extract;
pub mod secrets;

// ---------------------------------------------------------------------------
// Knowledge tier (Active = auto-injected into coding context, Archive = not)
// ---------------------------------------------------------------------------

/// Controls whether a lesson is automatically surfaced in retrieval.
/// Active records are injected into coding context; Archive records are stored
/// but never auto-retrieved (they can only be fetched via explicit ?tier=archive).
///
/// INVARIANT: Any future distillation pipeline that promotes `source_sessions`
/// content into `knowledge` records MUST set `tier = Archive` on the resulting
/// records. Active promotion requires an explicit human decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeTier {
    #[default]
    Active,
    Archive,
}

impl KnowledgeTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archive => "archive",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "archive" => Self::Archive,
            _ => Self::Active,
        }
    }
}

// ---------------------------------------------------------------------------
// Knowledge taxonomy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeKind {
    Claim,
    Procedure,
    Episode,
    Directive,
    Context,
}

/// Subtask phase this lesson applies to. Based on Shen et al. 2026
/// (arXiv:2602.21611) "Structurally Aligned Subtask-Level Lesson for
/// Software Engineering Agents". Category isolation — filtering retrieval
/// by phase — eliminates cross-stage noise and produces +3.9pp on
/// SWE-bench Verified with Claude 3.7 Sonnet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgePhase {
    /// Understanding code, invariants, module structure. Answers "what/why".
    Analyze,
    /// Reproducing a bug, setting up failing tests, triggering an issue.
    Reproduce,
    /// Making changes — the fix itself, the patch, the edit.
    Edit,
    /// Confirming a fix works — tests to run, checks, regression guards.
    Verify,
}

impl KnowledgePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Analyze => "analyze",
            Self::Reproduce => "reproduce",
            Self::Edit => "edit",
            Self::Verify => "verify",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "analyze" | "analyse" => Some(Self::Analyze),
            "reproduce" | "repro" => Some(Self::Reproduce),
            "edit" | "fix" | "change" => Some(Self::Edit),
            "verify" | "test" | "validate" => Some(Self::Verify),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Cue bundle (Phase 1 of cue-bundle retrieval — see docs/plans/cue-bundle-retrieval.md)
// ---------------------------------------------------------------------------

/// User / agent cognitive state at write or query time.
/// LLM-classified (Haiku); None for legacy records and write paths that
/// skip classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum UserState {
    Committed,
    Exploratory,
    Skeptical,
    Blocked,
    Decisive,
}

impl UserState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Exploratory => "exploratory",
            Self::Skeptical => "skeptical",
            Self::Blocked => "blocked",
            Self::Decisive => "decisive",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "committed" => Some(Self::Committed),
            "exploratory" => Some(Self::Exploratory),
            "skeptical" => Some(Self::Skeptical),
            "blocked" => Some(Self::Blocked),
            "decisive" => Some(Self::Decisive),
            _ => None,
        }
    }
}

/// Position within a session, bucketed by turn count.
/// `Early` ≤ 5 turns, `Mid` 6-20, `Late` 20+.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SessionArc {
    Early,
    Mid,
    Late,
}

impl SessionArc {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Early => "early",
            Self::Mid => "mid",
            Self::Late => "late",
        }
    }

    pub fn from_turn_count(turns: u32) -> Self {
        match turns {
            0..=5 => Self::Early,
            6..=20 => Self::Mid,
            _ => Self::Late,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "early" => Some(Self::Early),
            "mid" => Some(Self::Mid),
            "late" => Some(Self::Late),
            _ => None,
        }
    }
}

/// High-level cue dimension. These are stable retrieval axes, not hard
/// filters. Values inside each dimension stay flexible so project/app aliases
/// and domain concepts can evolve without schema churn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CueDimension {
    Identity,
    Task,
    Domain,
    Technical,
    File,
    Entity,
    Symbol,
    Temporal,
    Outcome,
}

impl CueDimension {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Task => "task",
            Self::Domain => "domain",
            Self::Technical => "technical",
            Self::File => "file",
            Self::Entity => "entity",
            Self::Symbol => "symbol",
            Self::Temporal => "temporal",
            Self::Outcome => "outcome",
        }
    }
}

/// Flexible cue payload. `raw` preserves what the agent/user called the thing;
/// `canonical` is the normalized matching key; `aliases` capture known
/// alternate spellings; `kind` names the value type within the dimension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CueValue {
    pub raw: String,
    pub canonical: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

impl CueValue {
    pub fn new(raw: impl Into<String>, canonical: impl Into<String>) -> Self {
        Self {
            raw: raw.into(),
            canonical: canonical.into(),
            aliases: Vec::new(),
            kind: None,
        }
    }

    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    pub fn with_aliases(mut self, aliases: Vec<String>) -> Self {
        self.aliases = aliases;
        self
    }
}

/// First-class retrieval cue. Cues are soft evidence: they admit, expand, and
/// rank candidates, but no single cue dimension is a storage partition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cue {
    pub dimension: CueDimension,
    pub value: CueValue,
    pub confidence: f32,
    pub source: String,
}

impl Cue {
    pub fn new(
        dimension: CueDimension,
        value: CueValue,
        confidence: f32,
        source: impl Into<String>,
    ) -> Self {
        Self {
            dimension,
            value,
            confidence,
            source: source.into(),
        }
    }
}

/// Situational features captured at write time and extracted at retrieval time.
/// Scored conjunctively against the current session's bundle to determine
/// whether a record's encoding context matches the current retrieval context.
///
/// Grounding: Tulving & Thomson (1973) encoding specificity — recall succeeds
/// to the degree that retrieval cues match cues present at encoding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CueBundle {
    /// First-class typed cues used for cue-first retrieval. This supersedes the
    /// older fixed fields below for identity/entity matching while preserving
    /// them for compatibility and coarse scoring.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cues: Vec<Cue>,
    /// Subtask phase. Mirrors `KnowledgeRecord.phase` but lives in the bundle
    /// for retrieval-time matching alongside the other situational cues.
    pub phase: Option<KnowledgePhase>,
    /// Coarse domain inferred from active file paths or content
    /// (e.g. "fieldcraft_retrieval", "system_infra").
    pub domain: Option<String>,
    /// Names of tools used or referenced (e.g. "cargo", "git", "Read", "Bash").
    pub tools_involved: Vec<String>,
    /// Active file paths at encoding / retrieval time.
    pub active_files: Vec<String>,
    /// Top distinctive terms from the surrounding text — rare, high-signal tokens.
    pub distinctive_terms: Vec<String>,
    /// Cognitive state of user / agent (LLM-classified, optional).
    pub user_state: Option<UserState>,
    /// Where in the session the bundle was formed.
    pub session_arc: Option<SessionArc>,
}

impl CueBundle {
    /// True when every dimension is empty/None. Used during cue-aware retrieval
    /// to skip records that have not yet been backfilled with a bundle.
    pub fn is_empty(&self) -> bool {
        self.phase.is_none()
            && self.domain.is_none()
            && self.tools_involved.is_empty()
            && self.active_files.is_empty()
            && self.distinctive_terms.is_empty()
            && self.user_state.is_none()
            && self.session_arc.is_none()
            && self.cues.is_empty()
    }

    /// Flatten into `(dim_name, dim_value)` tuples for the `cue_index` inverted
    /// table. Set-valued dimensions produce one row per element.
    pub fn to_index_rows(&self) -> Vec<(&'static str, String)> {
        let mut rows = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for cue in &self.cues {
            if seen.insert((cue.dimension.as_str(), cue.value.canonical.clone())) {
                rows.push((cue.dimension.as_str(), cue.value.canonical.clone()));
            }
            for alias in &cue.value.aliases {
                if seen.insert((cue.dimension.as_str(), alias.clone())) {
                    rows.push((cue.dimension.as_str(), alias.clone()));
                }
            }
        }
        if let Some(phase) = self.phase {
            let value = phase.as_str().to_string();
            if seen.insert(("phase", value.clone())) {
                rows.push(("phase", value));
            }
        }
        if let Some(domain) = &self.domain
            && seen.insert(("domain", domain.clone()))
        {
            rows.push(("domain", domain.clone()));
        }
        for tool in &self.tools_involved {
            if seen.insert(("tools_involved", tool.clone())) {
                rows.push(("tools_involved", tool.clone()));
            }
        }
        for path in &self.active_files {
            if seen.insert(("active_files", path.clone())) {
                rows.push(("active_files", path.clone()));
            }
        }
        for term in &self.distinctive_terms {
            if seen.insert(("distinctive_terms", term.clone())) {
                rows.push(("distinctive_terms", term.clone()));
            }
        }
        if let Some(user_state) = self.user_state {
            let value = user_state.as_str().to_string();
            if seen.insert(("user_state", value.clone())) {
                rows.push(("user_state", value));
            }
        }
        if let Some(arc) = self.session_arc {
            let value = arc.as_str().to_string();
            if seen.insert(("session_arc", value.clone())) {
                rows.push(("session_arc", value));
            }
        }
        rows
    }
}

// ---------------------------------------------------------------------------
// Payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ClaimPayload {
    pub statement: String,
    pub mechanism: Option<String>,
    pub scope: Option<String>,
    pub exceptions: Vec<String>,
    pub affected_files: Vec<String>,
    pub retrieval_questions: Vec<String>,
    pub trigger: Option<String>,
    pub invalid_if: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProcedurePayload {
    pub goal: String,
    pub preconditions: Vec<String>,
    pub steps: Vec<String>,
    pub failure_modes: Vec<String>,
    pub affected_files: Vec<String>,
    pub retrieval_questions: Vec<String>,
    pub trigger: Option<String>,
    pub invalid_if: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EpisodePayload {
    pub summary: String,
    pub trigger: Option<String>,
    pub outcome: Option<String>,
    pub observations: Vec<String>,
    pub affected_files: Vec<String>,
    pub derived_claim_ids: Vec<String>,
    pub derived_procedure_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DirectivePayload {
    pub action: String,
    pub reason: String,
    pub files: Vec<String>,
    pub trigger: String,
    pub avoid: Vec<String>,
    pub verify: Option<String>,
    pub invalid_if: Option<String>,
    pub retrieval_queries: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ContextPayload {
    pub statement: String,
    pub scope: String,
    pub files: Vec<String>,
    pub implications: Vec<String>,
    pub invalid_if: Option<String>,
    pub retrieval_queries: Vec<String>,
}

// ---------------------------------------------------------------------------
// Lifecycle / state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeState {
    /// Retrievable. Default after passing self-critique.
    Active,
    /// New, not yet validated. Retrievable with lower weight.
    Provisional,
    /// Replaced by a newer record. Not retrieved.
    Superseded,
    /// No longer relevant. Not retrieved.
    Retired,
}

// ---------------------------------------------------------------------------
// Source provenance
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTool {
    ClaudeCode,
    Codex,
    Cursor,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum KnowledgeSource {
    Session {
        session_id: String,
        tool: AgentTool,
    },
    External {
        uri: String,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_hash: Option<String>,
    },
    Imported {
        source_path: String,
    },
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeOrigin {
    Learned,
    ExternalDerived,
    ManualImported,
}

impl KnowledgeOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Learned => "learned",
            Self::ExternalDerived => "external_derived",
            Self::ManualImported => "manual_imported",
        }
    }

    pub fn from_source(source: &KnowledgeSource) -> Self {
        match source {
            KnowledgeSource::Session { .. } => Self::Learned,
            KnowledgeSource::External { .. } => Self::ExternalDerived,
            KnowledgeSource::Imported { .. } | KnowledgeSource::Manual => Self::ManualImported,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourceFilter {
    #[default]
    Any,
    LearnedOnly,
    ExternalOnly,
}

impl SourceFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::LearnedOnly => "learned_only",
            Self::ExternalOnly => "external_only",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "learned" | "learned_only" | "learned-only" => Self::LearnedOnly,
            "external" | "external_only" | "external-only" | "external_derived" => {
                Self::ExternalOnly
            }
            _ => Self::Any,
        }
    }

    pub fn matches(self, origin: KnowledgeOrigin) -> bool {
        match self {
            Self::Any => true,
            Self::LearnedOnly => origin == KnowledgeOrigin::Learned,
            Self::ExternalOnly => origin == KnowledgeOrigin::ExternalDerived,
        }
    }
}

/// Concrete source locations that support a knowledge record.
///
/// `source_id` joins to `source_sessions.source_id`; `chunk_index` identifies
/// the deterministic extraction chunk used to create the lesson.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceReference {
    pub source_id: String,
    #[serde(default)]
    pub chunk_index: Option<usize>,
}

// ---------------------------------------------------------------------------
// Relations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    /// This record replaces that record.
    Supersedes,
    /// This claim/procedure was derived from that episode.
    DerivedFrom,
}

// ---------------------------------------------------------------------------
// Core record envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeRecord {
    /// SHA256-based stable ID.
    pub id: String,
    /// Which project this belongs to.
    pub project_id: String,
    pub kind: KnowledgeKind,
    /// Short summary for display.
    pub title: String,
    /// Serialized ClaimPayload / ProcedurePayload / EpisodePayload.
    pub payload_json: String,
    pub state: KnowledgeState,
    /// 0.0–1.0. Starts 0.5 for auto-extracted, 0.8 for manual.
    pub confidence: f32,
    pub source: KnowledgeSource,
    pub superseded_by: Option<String>,
    /// True if references project-internal entities.
    pub project_specific: bool,
    pub created_at: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
    /// Pre-computed embedding text.
    pub retrieval_text: String,
    /// Schema version: 2 = legacy (claim/procedure/episode), 3 = directive/context,
    /// 4 = directive/context + subtask phase tagging (Analyze/Reproduce/Edit/Verify).
    pub schema_version: u32,
    /// Subtask phase this lesson applies to. None = legacy records without
    /// phase classification. Category-filtered retrieval routes queries to
    /// lessons matching the inferred query phase.
    pub phase: Option<KnowledgePhase>,
    /// Active = auto-injected into coding context. Archive = stored but never
    /// auto-retrieved. Defaults to Active for all records written via MCP.
    /// HTTP imports land as Archive (pending explicit distillation).
    #[serde(default)]
    pub tier: KnowledgeTier,
    /// User who wrote this lesson (token label at write time). None for legacy records.
    #[serde(default)]
    pub owner: Option<String>,
    /// When the source session occurred. None for manual writes and legacy records.
    #[serde(default)]
    pub session_date: Option<DateTime<Utc>>,
    /// Concrete source-session references for provenance reconstruction.
    #[serde(default)]
    pub source_refs: Vec<SourceReference>,
    /// Explicit retention pin (Memory Lifecycle L1). When true, this record
    /// is never auto-archived regardless of usage/outcome signals. This is
    /// the *explicit* override only; `kind in {directive, context}` and
    /// static `confidence >= 0.9` are additional *implicit* protection
    /// rules applied at read time (see `KnowledgeRepository::is_protected`),
    /// not reflected in this field.
    #[serde(default)]
    pub protected: bool,
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Ingest,
    Promote,
    Consolidate,
    Refine,
    Rebuild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobState {
    pub id: String,
    pub project: String,
    pub kind: JobKind,
    pub status: JobStatus,
    pub queued_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Source session ingestion
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceSessionStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceTranscriptEntry {
    pub timestamp: Option<String>,
    pub role: String,
    pub kind: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSessionRecord {
    pub source_id: String,
    pub source_tool: AgentTool,
    pub source_path: String,
    pub session_id: String,
    pub project: String,
    pub machine: String,
    pub cwd: Option<String>,
    pub title: String,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    pub discovered_at: String,
    pub source_fingerprint: String,
    pub transcript: Vec<SourceTranscriptEntry>,
    pub status: SourceSessionStatus,
    pub attempts: u32,
    pub extractor_version: String,
    pub last_error: Option<String>,
    pub last_extracted_at: Option<String>,
    /// Set when this session is being processed by an extraction batch.
    /// NULL/None when session is Pending, Succeeded, or Failed.
    #[serde(default)]
    pub extraction_batch_id: Option<String>,
}

// ---------------------------------------------------------------------------
// External source learning
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningSourceKind {
    Url,
    File,
    Directory,
    Text,
}

impl LearningSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Url => "url",
            Self::File => "file",
            Self::Directory => "directory",
            Self::Text => "text",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.to_lowercase().as_str() {
            "file" => Self::File,
            "directory" | "dir" => Self::Directory,
            "text" => Self::Text,
            _ => Self::Url,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningSourceStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Stale,
}

impl LearningSourceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Stale => "stale",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.to_lowercase().as_str() {
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "stale" => Self::Stale,
            _ => Self::Pending,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningTraversalPolicy {
    #[serde(default = "LearningTraversalPolicy::default_max_depth")]
    pub max_depth: u32,
    #[serde(default = "LearningTraversalPolicy::default_max_bytes")]
    pub max_bytes: u64,
    #[serde(default)]
    pub allow_external_domains: bool,
}

impl LearningTraversalPolicy {
    fn default_max_depth() -> u32 {
        1
    }

    fn default_max_bytes() -> u64 {
        5 * 1024 * 1024
    }
}

impl Default for LearningTraversalPolicy {
    fn default() -> Self {
        Self {
            max_depth: Self::default_max_depth(),
            max_bytes: Self::default_max_bytes(),
            allow_external_domains: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningSourceSeed {
    pub uri: String,
    #[serde(default)]
    pub kind: Option<LearningSourceKind>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub scope_hint: Option<String>,
    #[serde(default)]
    pub traversal_policy: LearningTraversalPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningSourceRecord {
    pub source_id: String,
    pub kind: LearningSourceKind,
    pub uri: String,
    pub project: String,
    pub title: String,
    pub scope_hint: Option<String>,
    pub traversal_policy: LearningTraversalPolicy,
    pub status: LearningSourceStatus,
    pub attempts: u32,
    pub created_at: String,
    pub updated_at: String,
    pub claimed_at: Option<String>,
    pub finished_at: Option<String>,
    pub source_hash: Option<String>,
    pub derived_record_count: u32,
    pub worker_id: Option<String>,
    pub last_error: Option<String>,
}

// ---------------------------------------------------------------------------
// Extraction batch tracking (US-004)
// ---------------------------------------------------------------------------

/// Tracks an OpenAI Batch API submission for Stage B extraction.
/// `session_ids` maps to `session_ids_json` (JSON array) in the DB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionBatchRecord {
    pub batch_id: String,
    pub created_at: String,
    /// submitted | in_progress | completed | failed | reconciled
    pub status: String,
    /// Serialized as JSON array in the DB column `session_ids_json`.
    pub session_ids: Vec<String>,
    pub input_file_id: Option<String>,
    pub output_file_id: Option<String>,
    pub error_file_id: Option<String>,
    pub total_requests: u32,
    pub reconciled_at: Option<String>,
    pub extractor_version: String,
    pub last_polled_at: Option<String>,
    pub poll_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportStatusSummary {
    pub pending: usize,
    pub running: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub stale: usize,
}

// ---------------------------------------------------------------------------
// API error type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiErrorBody {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
    pub details: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// Domain error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum TrError {
    #[error("not found: {message}")]
    NotFound { message: String },
    #[error("invalid request: {message}")]
    InvalidRequest { message: String },
    #[error("conflict: {message}")]
    Conflict { message: String },
    #[error("unauthorized")]
    Unauthorized,
    #[error("storage failure: {message}")]
    Storage { message: String },
    #[error("parse failure: {message}")]
    Parse { message: String },
    #[error("internal failure: {message}")]
    Internal { message: String },
}

impl TrError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "not_found",
            Self::InvalidRequest { .. } => "invalid_request",
            Self::Conflict { .. } => "conflict",
            Self::Unauthorized => "unauthorized",
            Self::Storage { .. } => "storage_error",
            Self::Parse { .. } => "parse_error",
            Self::Internal { .. } => "internal_error",
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(self, Self::Storage { .. } | Self::Internal { .. })
    }

    pub fn user_message(&self) -> String {
        self.to_string()
    }

    pub fn to_api_body(&self) -> ApiErrorBody {
        ApiErrorBody {
            code: self.code(),
            message: self.user_message(),
            retryable: self.retryable(),
            details: BTreeMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_contract_has_stable_codes() {
        let error = TrError::InvalidRequest {
            message: "missing project".into(),
        };
        let body = error.to_api_body();
        assert_eq!(body.code, "invalid_request");
        assert!(!body.retryable);
    }

    #[test]
    fn knowledge_kind_serializes_to_snake_case() {
        let claim = serde_json::to_string(&KnowledgeKind::Claim).unwrap();
        let procedure = serde_json::to_string(&KnowledgeKind::Procedure).unwrap();
        let episode = serde_json::to_string(&KnowledgeKind::Episode).unwrap();
        let directive = serde_json::to_string(&KnowledgeKind::Directive).unwrap();
        let context = serde_json::to_string(&KnowledgeKind::Context).unwrap();
        assert_eq!(claim, r#""claim""#);
        assert_eq!(procedure, r#""procedure""#);
        assert_eq!(episode, r#""episode""#);
        assert_eq!(directive, r#""directive""#);
        assert_eq!(context, r#""context""#);
    }

    #[test]
    fn knowledge_state_has_four_variants() {
        // Ensure all four variants round-trip through serde
        for state in [
            KnowledgeState::Active,
            KnowledgeState::Provisional,
            KnowledgeState::Superseded,
            KnowledgeState::Retired,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let round_tripped: KnowledgeState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, round_tripped);
        }
    }

    #[test]
    fn relation_kind_has_two_variants() {
        let supersedes = serde_json::to_string(&RelationKind::Supersedes).unwrap();
        let derived_from = serde_json::to_string(&RelationKind::DerivedFrom).unwrap();
        assert_eq!(supersedes, r#""supersedes""#);
        assert_eq!(derived_from, r#""derived_from""#);
    }

    #[test]
    fn agent_tool_serializes_correctly() {
        let cc = serde_json::to_string(&AgentTool::ClaudeCode).unwrap();
        assert_eq!(cc, r#""claude_code""#);
    }

    #[test]
    fn knowledge_tier_defaults_to_active() {
        let tier: KnowledgeTier = Default::default();
        assert_eq!(tier, KnowledgeTier::Active);
    }

    #[test]
    fn knowledge_tier_round_trip_serde() {
        for (tier, expected_json) in [
            (KnowledgeTier::Active, r#""active""#),
            (KnowledgeTier::Archive, r#""archive""#),
        ] {
            let json = serde_json::to_string(&tier).unwrap();
            assert_eq!(json, expected_json);
            let rt: KnowledgeTier = serde_json::from_str(&json).unwrap();
            assert_eq!(rt, tier);
        }
    }

    #[test]
    fn knowledge_record_without_tier_deserializes_as_active() {
        // Simulate a v3 JSON record without the `tier` field
        let json = r#"{
            "id": "rec-1",
            "project_id": "proj",
            "kind": "claim",
            "title": "Test",
            "payload_json": "{}",
            "state": "active",
            "confidence": 0.8,
            "source": {"type": "manual"},
            "superseded_by": null,
            "project_specific": true,
            "created_at": "2026-05-01T00:00:00Z",
            "valid_until": null,
            "retrieval_text": "test",
            "schema_version": 2,
            "phase": null
        }"#;
        let record: KnowledgeRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.tier, KnowledgeTier::Active);
    }

    #[test]
    fn knowledge_record_with_tier_archive_round_trips() {
        use chrono::Utc;
        let record = KnowledgeRecord {
            protected: false,
            id: "rec-2".to_string(),
            project_id: "proj".to_string(),
            kind: KnowledgeKind::Claim,
            title: "Archive record".to_string(),
            payload_json: "{}".to_string(),
            state: KnowledgeState::Active,
            confidence: 0.5,
            source: KnowledgeSource::Manual,
            superseded_by: None,
            project_specific: false,
            created_at: Utc::now(),
            valid_until: None,
            retrieval_text: "archive test".to_string(),
            schema_version: 3,
            phase: None,
            tier: KnowledgeTier::Archive,
            owner: None,
            session_date: None,
            source_refs: vec![],
        };
        let json = serde_json::to_string(&record).unwrap();
        let rt: KnowledgeRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.tier, KnowledgeTier::Archive);
    }

    // -------------------------------------------------------------------------
    // US-002: SourceSessionRecord.extraction_batch_id field
    // -------------------------------------------------------------------------

    fn make_source_session_json_without_batch_id() -> &'static str {
        r#"{
            "source_id": "src-1",
            "source_tool": "codex",
            "source_path": "/path/to/session.jsonl",
            "session_id": "sess-1",
            "project": "my-project",
            "machine": "mac-mini",
            "cwd": null,
            "title": "Fix the bug",
            "started_at": null,
            "updated_at": null,
            "discovered_at": "2026-01-01T00:00:00Z",
            "source_fingerprint": "abc123",
            "transcript": [],
            "status": "pending",
            "attempts": 0,
            "extractor_version": "v1",
            "last_error": null,
            "last_extracted_at": null
        }"#
    }

    #[test]
    fn source_session_record_without_batch_id_deserializes_as_none() {
        // US-002: existing JSON without the field should deserialize successfully.
        let record: SourceSessionRecord =
            serde_json::from_str(make_source_session_json_without_batch_id()).unwrap();
        assert_eq!(record.extraction_batch_id, None);
    }

    #[test]
    fn source_session_record_extraction_batch_id_round_trip() {
        // US-002: serialize with Some("abc"), deserialize, assert equal.
        let mut record: SourceSessionRecord =
            serde_json::from_str(make_source_session_json_without_batch_id()).unwrap();
        record.extraction_batch_id = Some("batch-abc-123".to_string());
        let json = serde_json::to_string(&record).unwrap();
        let rt: SourceSessionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.extraction_batch_id, Some("batch-abc-123".to_string()));
    }

    // -------------------------------------------------------------------------
    // CueBundle (Phase 1 of cue-bundle retrieval)
    // -------------------------------------------------------------------------

    #[test]
    fn cue_bundle_default_is_empty() {
        let b: CueBundle = Default::default();
        assert!(b.is_empty());
        assert!(b.to_index_rows().is_empty());
    }

    #[test]
    fn cue_bundle_round_trip_serde() {
        let b = CueBundle {
            cues: vec![],
            phase: Some(KnowledgePhase::Edit),
            domain: Some("fieldcraft_retrieval".to_string()),
            tools_involved: vec!["cargo".to_string(), "git".to_string()],
            active_files: vec!["crates/fieldcraft-retrieval/src/retrieval.rs".to_string()],
            distinctive_terms: vec!["P0_WEIGHTS".to_string(), "RRF".to_string()],
            user_state: Some(UserState::Committed),
            session_arc: Some(SessionArc::Mid),
        };
        let json = serde_json::to_string(&b).unwrap();
        let rt: CueBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(b, rt);
    }

    #[test]
    fn cue_bundle_to_index_rows_flattens_sets() {
        let b = CueBundle {
            cues: vec![],
            phase: Some(KnowledgePhase::Analyze),
            domain: Some("infra".to_string()),
            tools_involved: vec!["cargo".to_string(), "git".to_string()],
            active_files: vec![],
            distinctive_terms: vec!["RRF".to_string()],
            user_state: None,
            session_arc: Some(SessionArc::Early),
        };
        let rows = b.to_index_rows();
        // phase + domain + 2 tools + 1 term + session_arc = 6
        assert_eq!(rows.len(), 6);
        assert!(rows.contains(&("phase", "analyze".to_string())));
        assert!(rows.contains(&("domain", "infra".to_string())));
        assert!(rows.contains(&("tools_involved", "cargo".to_string())));
        assert!(rows.contains(&("tools_involved", "git".to_string())));
        assert!(rows.contains(&("distinctive_terms", "RRF".to_string())));
        assert!(rows.contains(&("session_arc", "early".to_string())));
    }

    #[test]
    fn session_arc_buckets_by_turn_count() {
        assert_eq!(SessionArc::from_turn_count(1), SessionArc::Early);
        assert_eq!(SessionArc::from_turn_count(5), SessionArc::Early);
        assert_eq!(SessionArc::from_turn_count(6), SessionArc::Mid);
        assert_eq!(SessionArc::from_turn_count(20), SessionArc::Mid);
        assert_eq!(SessionArc::from_turn_count(21), SessionArc::Late);
    }

    #[test]
    fn user_state_round_trip_serde() {
        for s in [
            UserState::Committed,
            UserState::Exploratory,
            UserState::Skeptical,
            UserState::Blocked,
            UserState::Decisive,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let rt: UserState = serde_json::from_str(&json).unwrap();
            assert_eq!(s, rt);
        }
    }

    #[test]
    fn session_arc_round_trip_serde() {
        for s in [SessionArc::Early, SessionArc::Mid, SessionArc::Late] {
            let json = serde_json::to_string(&s).unwrap();
            let rt: SessionArc = serde_json::from_str(&json).unwrap();
            assert_eq!(s, rt);
        }
    }
}
