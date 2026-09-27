//! Promote high-confirm lessons into a local skill package or AGENTS.md directive.
//!
//! This is a deliberate, human/agent-gated path. Default is a read-only dry run.
//! `--apply` (via [`PromoteRequest::apply`]) is required to write files or
//! provenance. Nothing here is invoked from hooks, cron, or lifecycle loops.
//!
//! Eligibility aligns with `fieldcraft stats` workhorses (`confirmed >= 3` and
//! `confirm_rate >= 0.5`) and additionally prefers `procedure` / `directive`,
//! excludes ghosts/noise/contradicted clusters, and skips already-promoted
//! lessons. The original lesson is never deleted.

use chrono::Utc;
use fieldcraft_core::{
    ClaimPayload, ContextPayload, DirectivePayload, EpisodePayload, KnowledgeKind, KnowledgeRecord,
    KnowledgeState, KnowledgeTier, ProcedurePayload, TrError,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::repository::KnowledgeRepository;
use crate::schema::sqlite_error;
use crate::slugify;

/// `knowledge_events.event_type` written when a lesson is promoted.
/// Distinct from `promoted_active` (provisional → active after a confirm).
pub const PROMOTE_EVENT_TYPE: &str = "lesson_promoted";

pub const DEFAULT_MIN_CONFIRMED: i64 = 3;
pub const DEFAULT_MIN_CONFIRM_RATE: f64 = 0.5;
pub const DEFAULT_MAX_CONTRADICTED: i64 = 0;
pub const DEFAULT_PROMOTE_KINDS: &[&str] = &["procedure", "directive"];

/// Default directory name under the catalog parent for skill packages.
pub const PROMOTED_SKILLS_DIR: &str = "promoted-skills";

const NOISE_MIN_TOTAL: i64 = 5;
const NOISE_MAX_CONFIRM_RATE: f64 = 0.2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromoteTarget {
    Skill,
    AgentsMd,
}

impl PromoteTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::AgentsMd => "agents-md",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "skill" | "skill-md" | "skills" => Some(Self::Skill),
            "agents-md" | "agents_md" | "agentsmd" | "directive" | "agents" => Some(Self::AgentsMd),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromoteThresholds {
    pub min_confirmed: i64,
    pub min_confirm_rate: f64,
    pub max_contradicted: i64,
    pub kinds: Vec<String>,
}

impl Default for PromoteThresholds {
    fn default() -> Self {
        Self {
            min_confirmed: DEFAULT_MIN_CONFIRMED,
            min_confirm_rate: DEFAULT_MIN_CONFIRM_RATE,
            max_contradicted: DEFAULT_MAX_CONTRADICTED,
            kinds: DEFAULT_PROMOTE_KINDS
                .iter()
                .map(|k| (*k).to_string())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OutcomeCounts {
    pub confirmed: i64,
    pub ignored: i64,
    pub contradicted: i64,
    pub total: i64,
}

impl OutcomeCounts {
    pub fn confirm_rate(&self) -> f64 {
        if self.total <= 0 {
            0.0
        } else {
            self.confirmed as f64 / self.total as f64
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromoteCandidate {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub project_id: String,
    pub state: String,
    pub tier: String,
    pub counts: OutcomeCounts,
    pub already_promoted: bool,
    pub ineligible: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PromoteRequest {
    /// When false (default), preview only — no files and no audit rows.
    pub apply: bool,
    /// Promote this lesson. Required for `--apply`. Omit to list candidates.
    pub lesson_id: Option<String>,
    pub target: PromoteTarget,
    /// Optional override: skill package root, or AGENTS.md file/dir.
    pub dest: Option<PathBuf>,
    /// Catalog parent used as the default skill-package root.
    pub output_root: PathBuf,
    /// Working directory used as the default AGENTS.md location.
    pub cwd: PathBuf,
    pub thresholds: PromoteThresholds,
    pub include_promoted: bool,
    /// Bypass eligibility and overwrite an existing destination.
    pub force: bool,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromotePreview {
    pub lesson_id: String,
    pub target: PromoteTarget,
    pub destination: PathBuf,
    pub artifact: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromoteProvenance {
    pub source_lesson_id: String,
    pub target: String,
    pub destination: String,
    pub confirmed: i64,
    pub ignored: i64,
    pub contradicted: i64,
    pub total: i64,
    pub confirm_rate: f64,
    pub promoted_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromoteApplyResult {
    pub lesson_id: String,
    pub target: PromoteTarget,
    pub destination: PathBuf,
    pub provenance: PromoteProvenance,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromoteReport {
    pub mode: &'static str,
    pub candidates: Vec<PromoteCandidate>,
    pub preview: Option<PromotePreview>,
    pub applied: Option<PromoteApplyResult>,
    pub message: String,
}

/// Pure eligibility used by listing and by `--id` promote.
pub fn eligibility_reason(
    kind: &str,
    state: &str,
    tier: &str,
    counts: &OutcomeCounts,
    already_promoted: bool,
    thresholds: &PromoteThresholds,
) -> Option<String> {
    let kind = kind.trim().to_ascii_lowercase();
    if !thresholds.kinds.iter().any(|k| k == &kind) {
        return Some(format!(
            "kind {kind} is not in promote set ({})",
            thresholds.kinds.join(",")
        ));
    }
    if matches!(state, "superseded" | "retired") {
        return Some(format!("state {state} is not promotable"));
    }
    if tier == KnowledgeTier::Archive.as_str() {
        return Some("tier archive is excluded".to_string());
    }
    if already_promoted {
        return Some("already promoted".to_string());
    }
    if counts.total == 0 {
        return Some("ghost: no outcome events".to_string());
    }
    if counts.total >= NOISE_MIN_TOTAL && counts.confirm_rate() < NOISE_MAX_CONFIRM_RATE {
        return Some(format!(
            "noise: {confirmed}/{total} confirmed (rate {:.2} < {NOISE_MAX_CONFIRM_RATE})",
            counts.confirm_rate(),
            confirmed = counts.confirmed,
            total = counts.total,
        ));
    }
    if counts.confirmed < thresholds.min_confirmed {
        return Some(format!(
            "confirmed {} < min_confirmed {}",
            counts.confirmed, thresholds.min_confirmed
        ));
    }
    if counts.confirm_rate() + f64::EPSILON < thresholds.min_confirm_rate {
        return Some(format!(
            "confirm_rate {:.2} < min_confirm_rate {:.2}",
            counts.confirm_rate(),
            thresholds.min_confirm_rate
        ));
    }
    if counts.contradicted > thresholds.max_contradicted {
        return Some(format!(
            "contradicted {} exceeds max_contradicted {}",
            counts.contradicted, thresholds.max_contradicted
        ));
    }
    None
}

pub fn list_promote_candidates(
    repo: &KnowledgeRepository,
    thresholds: &PromoteThresholds,
    include_promoted: bool,
    limit: usize,
) -> Result<Vec<PromoteCandidate>, TrError> {
    let mut candidates = load_kind_candidates(repo, &thresholds.kinds)?;
    candidates.retain(|c| {
        if c.already_promoted && !include_promoted {
            return false;
        }
        eligibility_reason(
            &c.kind,
            &c.state,
            &c.tier,
            &c.counts,
            c.already_promoted,
            thresholds,
        )
        .is_none()
    });
    candidates.sort_by(|a, b| {
        b.counts
            .confirmed
            .cmp(&a.counts.confirmed)
            .then_with(|| {
                b.counts
                    .confirm_rate()
                    .partial_cmp(&a.counts.confirm_rate())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.id.cmp(&b.id))
    });
    if limit > 0 {
        candidates.truncate(limit);
    }
    Ok(candidates)
}

pub fn load_promote_candidate(
    repo: &KnowledgeRepository,
    lesson_id: &str,
) -> Result<PromoteCandidate, TrError> {
    let record = repo
        .get_knowledge(lesson_id)?
        .ok_or_else(|| TrError::NotFound {
            message: format!("knowledge record {lesson_id}"),
        })?;
    let counts = outcome_counts(repo, &record.id)?;
    let already_promoted = is_already_promoted(repo, &record.id)?;
    Ok(candidate_from_record(&record, counts, already_promoted))
}

pub fn run_promote(
    repo: &KnowledgeRepository,
    request: &PromoteRequest,
) -> Result<PromoteReport, TrError> {
    let mode = if request.apply { "APPLY" } else { "DRY-RUN" };

    let Some(lesson_id) = request.lesson_id.as_deref() else {
        if request.apply {
            return Err(TrError::InvalidRequest {
                message:
                    "refusing to bulk-apply promotions; pass --id <lesson> to promote one lesson"
                        .to_string(),
            });
        }
        let candidates = list_promote_candidates(
            repo,
            &request.thresholds,
            request.include_promoted,
            request.limit,
        )?;
        let message = if candidates.is_empty() {
            format!(
                "promote [{mode}]: no eligible candidates (confirmed ≥ {}, confirm_rate ≥ {:.2}, contradicted ≤ {}, kinds {}).",
                request.thresholds.min_confirmed,
                request.thresholds.min_confirm_rate,
                request.thresholds.max_contradicted,
                request.thresholds.kinds.join(",")
            )
        } else {
            format!(
                "promote [{mode}]: {} candidate(s). Dry run only, nothing written. Re-run with --id <lesson> --target <skill|agents-md> --apply to write.",
                candidates.len()
            )
        };
        return Ok(PromoteReport {
            mode,
            candidates,
            preview: None,
            applied: None,
            message,
        });
    };

    let candidate = load_promote_candidate(repo, lesson_id)?;
    if let Some(reason) = eligibility_reason(
        &candidate.kind,
        &candidate.state,
        &candidate.tier,
        &candidate.counts,
        candidate.already_promoted,
        &request.thresholds,
    ) && !request.force
    {
        return Err(TrError::InvalidRequest {
            message: format!("lesson {lesson_id} is not eligible: {reason}"),
        });
    }

    let record = repo
        .get_knowledge(lesson_id)?
        .ok_or_else(|| TrError::NotFound {
            message: format!("knowledge record {lesson_id}"),
        })?;
    let destination = resolve_destination(request, &record);
    let artifact = match request.target {
        PromoteTarget::Skill => render_skill_markdown(&record, &candidate.counts),
        PromoteTarget::AgentsMd => render_agents_md_snippet(&record, &candidate.counts),
    };
    let preview = PromotePreview {
        lesson_id: record.id.clone(),
        target: request.target,
        destination: destination.clone(),
        artifact,
    };

    if !request.apply {
        return Ok(PromoteReport {
            mode,
            candidates: vec![candidate],
            preview: Some(preview.clone()),
            applied: None,
            message: format!(
                "promote [{mode}]: would write {} for {}. Destination: {}. Re-run with --apply to write.",
                request.target.as_str(),
                record.id,
                destination.display()
            ),
        });
    }

    let applied = apply_promotion(repo, request, &record, &candidate, &preview)?;
    Ok(PromoteReport {
        mode,
        candidates: vec![candidate],
        preview: Some(preview),
        applied: Some(applied),
        message: format!(
            "promote [{mode}]: wrote {} for {} → {}",
            request.target.as_str(),
            record.id,
            destination.display()
        ),
    })
}

pub fn render_promote_report(report: &PromoteReport) -> String {
    let mut out = String::new();
    out.push_str(&report.message);
    out.push('\n');
    if report.preview.is_none() {
        for c in &report.candidates {
            let promoted = if c.already_promoted { " promoted" } else { "" };
            out.push_str(&format!(
                "  [{kind}] {id}  {title}  confirmed={confirmed}/{total} rate={rate:.2} contradicted={contradicted}  project={project}{promoted}\n",
                kind = c.kind,
                id = c.id,
                title = c.title,
                confirmed = c.counts.confirmed,
                total = c.counts.total,
                rate = c.counts.confirm_rate(),
                contradicted = c.counts.contradicted,
                project = c.project_id,
            ));
        }
    }
    if let Some(preview) = &report.preview {
        out.push_str("\n--- artifact preview ---\n");
        out.push_str(&preview.artifact);
        if !preview.artifact.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("--- end preview ---\n");
    }
    if let Some(applied) = &report.applied {
        out.push_str(&format!(
            "provenance: event_type={PROMOTE_EVENT_TYPE} destination={}\n",
            applied.destination.display()
        ));
    }
    out
}

pub fn render_skill_markdown(record: &KnowledgeRecord, counts: &OutcomeCounts) -> String {
    let name = skill_name(record);
    let description = skill_description(record);
    let extracted = extract_payload(record);
    let mut body = String::new();
    body.push_str("---\n");
    body.push_str(&format!("name: {}\n", yaml_quote(&name)));
    body.push_str(&format!("description: {}\n", yaml_quote(&description)));
    body.push_str("---\n\n");
    body.push_str(&format!("# {}\n\n", record.title));
    body.push_str("## Provenance\n\n");
    body.push_str(&format!("- Source lesson: `{}`\n", record.id));
    body.push_str(&format!("- Kind: {}\n", knowledge_kind_label(record.kind)));
    body.push_str(&format!("- Project: {}\n", record.project_id));
    body.push_str(&format!(
        "- Feedback at promotion: confirmed {} / {} (rate {:.2}), ignored {}, contradicted {}\n",
        counts.confirmed,
        counts.total,
        counts.confirm_rate(),
        counts.ignored,
        counts.contradicted
    ));
    body.push_str(
        "- The original lesson stays in Fieldcraft retrieval; this file is a formal copy.\n\n",
    );
    if let Some(trigger) = extracted.trigger.as_deref().filter(|s| !s.is_empty()) {
        body.push_str("## When to use\n\n");
        body.push_str(&format!("- {trigger}\n\n"));
    }
    if !extracted.retrieval_queries.is_empty() {
        if extracted.trigger.is_none() {
            body.push_str("## When to use\n\n");
        }
        for q in &extracted.retrieval_queries {
            body.push_str(&format!("- {q}\n"));
        }
        body.push('\n');
    }
    if !extracted.goal.is_empty() {
        body.push_str("## Goal\n\n");
        body.push_str(&extracted.goal);
        body.push_str("\n\n");
    }
    if !extracted.steps.is_empty() {
        body.push_str("## Procedure\n\n");
        for (i, step) in extracted.steps.iter().enumerate() {
            body.push_str(&format!("{}. {step}\n", i + 1));
        }
        body.push('\n');
    }
    if !extracted.preconditions.is_empty() {
        body.push_str("## Preconditions\n\n");
        for item in &extracted.preconditions {
            body.push_str(&format!("- {item}\n"));
        }
        body.push('\n');
    }
    if !extracted.failure_modes.is_empty() {
        body.push_str("## Failure modes\n\n");
        for item in &extracted.failure_modes {
            body.push_str(&format!("- {item}\n"));
        }
        body.push('\n');
    }
    if !extracted.avoid.is_empty() {
        body.push_str("## Avoid\n\n");
        for item in &extracted.avoid {
            body.push_str(&format!("- {item}\n"));
        }
        body.push('\n');
    }
    if let Some(verify) = extracted.verify.as_deref().filter(|s| !s.is_empty()) {
        body.push_str("## Verification\n\n");
        body.push_str(&format!("- {verify}\n\n"));
    }
    if let Some(invalid_if) = extracted.invalid_if.as_deref().filter(|s| !s.is_empty()) {
        body.push_str("## Invalid if\n\n");
        body.push_str(&format!("- {invalid_if}\n\n"));
    }
    if !extracted.files.is_empty() {
        body.push_str("## Files\n\n");
        for file in &extracted.files {
            body.push_str(&format!("- `{file}`\n"));
        }
        body.push('\n');
    }
    if body.ends_with("\n\n") {
        body.pop();
    }
    body
}

pub fn render_agents_md_snippet(record: &KnowledgeRecord, counts: &OutcomeCounts) -> String {
    let extracted = extract_payload(record);
    let mut snippet = String::new();
    snippet.push_str(&format!(
        "<!-- fieldcraft-promote:start lesson:{} -->\n",
        record.id
    ));
    snippet.push_str(&format!("## {}\n\n", record.title));
    if let Some(trigger) = extracted.trigger.as_deref().filter(|s| !s.is_empty()) {
        snippet.push_str(&format!("**When:** {trigger}\n\n"));
    }
    if !extracted.goal.is_empty() {
        snippet.push_str(&extracted.goal);
        snippet.push_str("\n\n");
    }
    if !extracted.steps.is_empty() {
        for (i, step) in extracted.steps.iter().enumerate() {
            snippet.push_str(&format!("{}. {step}\n", i + 1));
        }
        snippet.push('\n');
    }
    if !extracted.avoid.is_empty() {
        snippet.push_str("**Avoid:** ");
        snippet.push_str(&extracted.avoid.join("; "));
        snippet.push_str("\n\n");
    }
    if let Some(verify) = extracted.verify.as_deref().filter(|s| !s.is_empty()) {
        snippet.push_str(&format!("**Verify:** {verify}\n\n"));
    }
    snippet.push_str(&format!(
        "**Source:** Fieldcraft lesson `{}` · confirmed {}/{} ({:.0}%) · contradicted {}\n",
        record.id,
        counts.confirmed,
        counts.total,
        counts.confirm_rate() * 100.0,
        counts.contradicted
    ));
    snippet.push_str(&format!(
        "<!-- fieldcraft-promote:end lesson:{} -->\n",
        record.id
    ));
    snippet
}

pub fn resolve_destination(request: &PromoteRequest, record: &KnowledgeRecord) -> PathBuf {
    match request.target {
        PromoteTarget::Skill => {
            resolve_skill_destination(request.dest.as_deref(), &request.output_root, record)
        }
        PromoteTarget::AgentsMd => {
            resolve_agents_md_destination(request.dest.as_deref(), &request.cwd)
        }
    }
}

fn resolve_skill_destination(
    dest: Option<&Path>,
    output_root: &Path,
    record: &KnowledgeRecord,
) -> PathBuf {
    let name = skill_name(record);
    let project = slugify(&record.project_id);
    if let Some(dest) = dest {
        if dest
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("SKILL.md"))
        {
            return dest.to_path_buf();
        }
        return dest.join(&project).join(&name).join("SKILL.md");
    }
    output_root
        .join(PROMOTED_SKILLS_DIR)
        .join(project)
        .join(name)
        .join("SKILL.md")
}

fn resolve_agents_md_destination(dest: Option<&Path>, cwd: &Path) -> PathBuf {
    if let Some(dest) = dest {
        if dest
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("AGENTS.md"))
            || dest.extension().and_then(|e| e.to_str()) == Some("md")
        {
            return dest.to_path_buf();
        }
        return dest.join("AGENTS.md");
    }
    cwd.join("AGENTS.md")
}

fn apply_promotion(
    repo: &KnowledgeRepository,
    request: &PromoteRequest,
    record: &KnowledgeRecord,
    candidate: &PromoteCandidate,
    preview: &PromotePreview,
) -> Result<PromoteApplyResult, TrError> {
    let destination = preview.destination.clone();
    if destination.exists() && !request.force && request.target == PromoteTarget::Skill {
        return Err(TrError::Conflict {
            message: format!(
                "destination already exists: {} (pass --force to overwrite)",
                destination.display()
            ),
        });
    }
    match request.target {
        PromoteTarget::Skill => write_skill_package(&destination, &preview.artifact)?,
        PromoteTarget::AgentsMd => {
            upsert_agents_md_block(&destination, &record.id, &preview.artifact)?
        }
    }
    let now = Utc::now().to_rfc3339();
    let provenance = PromoteProvenance {
        source_lesson_id: record.id.clone(),
        target: request.target.as_str().to_string(),
        destination: destination.display().to_string(),
        confirmed: candidate.counts.confirmed,
        ignored: candidate.counts.ignored,
        contradicted: candidate.counts.contradicted,
        total: candidate.counts.total,
        confirm_rate: candidate.counts.confirm_rate(),
        promoted_at: now.clone(),
    };
    let note = serde_json::to_string(&provenance).map_err(|e| TrError::Storage {
        message: format!("serialize promote provenance: {e}"),
    })?;
    repo.connection()
        .execute(
            "INSERT INTO knowledge_events
             (knowledge_id, event_type, source_knowledge_id, source_refs_json, note, created_at)
             VALUES (?1, ?2, NULL, '[]', ?3, ?4)",
            params![record.id, PROMOTE_EVENT_TYPE, note, now],
        )
        .map_err(sqlite_error)?;
    Ok(PromoteApplyResult {
        lesson_id: record.id.clone(),
        target: request.target,
        destination,
        provenance,
    })
}

fn write_skill_package(path: &Path, artifact: &str) -> Result<(), TrError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| TrError::Storage {
            message: format!("create skill package dir {}: {e}", parent.display()),
        })?;
    }
    fs::write(path, artifact).map_err(|e| TrError::Storage {
        message: format!("write {}: {e}", path.display()),
    })
}

fn upsert_agents_md_block(path: &Path, lesson_id: &str, snippet: &str) -> Result<(), TrError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| TrError::Storage {
            message: format!("create AGENTS.md dir {}: {e}", parent.display()),
        })?;
    }
    let start = format!("<!-- fieldcraft-promote:start lesson:{lesson_id} -->");
    let end = format!("<!-- fieldcraft-promote:end lesson:{lesson_id} -->");
    let existing = if path.exists() {
        fs::read_to_string(path).map_err(|e| TrError::Storage {
            message: format!("read {}: {e}", path.display()),
        })?
    } else {
        "# AGENTS.md\n\nDirectives promoted from Fieldcraft lessons. Review before relying on them.\n"
            .to_string()
    };
    let next =
        if let (Some(start_idx), Some(end_idx)) = (existing.find(&start), existing.find(&end)) {
            let end_idx = end_idx + end.len();
            let mut next = String::new();
            next.push_str(&existing[..start_idx]);
            next.push_str(snippet.trim_end());
            next.push('\n');
            next.push_str(&existing[end_idx..]);
            next
        } else {
            let mut next = existing;
            if !next.ends_with('\n') {
                next.push('\n');
            }
            next.push('\n');
            next.push_str(snippet.trim_end());
            next.push('\n');
            next
        };
    fs::write(path, next).map_err(|e| TrError::Storage {
        message: format!("write {}: {e}", path.display()),
    })
}

fn load_kind_candidates(
    repo: &KnowledgeRepository,
    kinds: &[String],
) -> Result<Vec<PromoteCandidate>, TrError> {
    if kinds.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = kinds
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT k.id, k.title, k.kind, k.project_id, k.state, k.tier,
                SUM(CASE WHEN e.outcome = 'confirmed' THEN 1 ELSE 0 END) AS confirmed,
                SUM(CASE WHEN e.outcome = 'ignored' THEN 1 ELSE 0 END) AS ignored,
                SUM(CASE WHEN e.outcome = 'contradicted' THEN 1 ELSE 0 END) AS contradicted,
                COUNT(e.id) AS total,
                EXISTS(
                    SELECT 1 FROM knowledge_events ev
                    WHERE ev.knowledge_id = k.id AND ev.event_type = '{PROMOTE_EVENT_TYPE}'
                ) AS already_promoted
         FROM knowledge k
         LEFT JOIN lesson_outcome_events e ON e.knowledge_id = k.id
         WHERE k.kind IN ({placeholders})
         GROUP BY k.id, k.title, k.kind, k.project_id, k.state, k.tier"
    );
    let mut stmt = repo.connection().prepare(&sql).map_err(sqlite_error)?;
    let params_vec: Vec<&dyn rusqlite::ToSql> =
        kinds.iter().map(|k| k as &dyn rusqlite::ToSql).collect();
    let rows = stmt
        .query_map(params_vec.as_slice(), |row| {
            Ok(PromoteCandidate {
                id: row.get(0)?,
                title: row.get(1)?,
                kind: row.get(2)?,
                project_id: row.get(3)?,
                state: row.get(4)?,
                tier: row.get(5)?,
                counts: OutcomeCounts {
                    confirmed: row.get::<_, i64>(6)?,
                    ignored: row.get::<_, i64>(7)?,
                    contradicted: row.get::<_, i64>(8)?,
                    total: row.get::<_, i64>(9)?,
                },
                already_promoted: row.get::<_, i64>(10)? != 0,
                ineligible: None,
            })
        })
        .map_err(sqlite_error)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(sqlite_error)?);
    }
    Ok(out)
}

fn outcome_counts(repo: &KnowledgeRepository, lesson_id: &str) -> Result<OutcomeCounts, TrError> {
    repo.connection()
        .query_row(
            "SELECT
                SUM(CASE WHEN outcome = 'confirmed' THEN 1 ELSE 0 END),
                SUM(CASE WHEN outcome = 'ignored' THEN 1 ELSE 0 END),
                SUM(CASE WHEN outcome = 'contradicted' THEN 1 ELSE 0 END),
                COUNT(*)
             FROM lesson_outcome_events WHERE knowledge_id = ?1",
            [lesson_id],
            |row| {
                Ok(OutcomeCounts {
                    confirmed: row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    ignored: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    contradicted: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    total: row.get(3)?,
                })
            },
        )
        .map_err(sqlite_error)
}

fn is_already_promoted(repo: &KnowledgeRepository, lesson_id: &str) -> Result<bool, TrError> {
    let n: i64 = repo
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM knowledge_events
             WHERE knowledge_id = ?1 AND event_type = ?2",
            params![lesson_id, PROMOTE_EVENT_TYPE],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    Ok(n > 0)
}

fn candidate_from_record(
    record: &KnowledgeRecord,
    counts: OutcomeCounts,
    already_promoted: bool,
) -> PromoteCandidate {
    PromoteCandidate {
        id: record.id.clone(),
        title: record.title.clone(),
        kind: knowledge_kind_label(record.kind).to_string(),
        project_id: record.project_id.clone(),
        state: knowledge_state_label(record.state).to_string(),
        tier: record.tier.as_str().to_string(),
        counts,
        already_promoted,
        ineligible: None,
    }
}

fn knowledge_kind_label(kind: KnowledgeKind) -> &'static str {
    match kind {
        KnowledgeKind::Claim => "claim",
        KnowledgeKind::Procedure => "procedure",
        KnowledgeKind::Episode => "episode",
        KnowledgeKind::Directive => "directive",
        KnowledgeKind::Context => "context",
    }
}

fn knowledge_state_label(state: KnowledgeState) -> &'static str {
    match state {
        KnowledgeState::Active => "active",
        KnowledgeState::Provisional => "provisional",
        KnowledgeState::Superseded => "superseded",
        KnowledgeState::Retired => "retired",
    }
}

#[derive(Default)]
struct ExtractedPayload {
    goal: String,
    trigger: Option<String>,
    steps: Vec<String>,
    preconditions: Vec<String>,
    failure_modes: Vec<String>,
    avoid: Vec<String>,
    verify: Option<String>,
    invalid_if: Option<String>,
    files: Vec<String>,
    retrieval_queries: Vec<String>,
}

fn extract_payload(record: &KnowledgeRecord) -> ExtractedPayload {
    match record.kind {
        KnowledgeKind::Procedure => serde_json::from_str::<ProcedurePayload>(&record.payload_json)
            .map(|p| ExtractedPayload {
                goal: p.goal,
                trigger: p.trigger,
                steps: p.steps,
                preconditions: p.preconditions,
                failure_modes: p.failure_modes,
                invalid_if: p.invalid_if,
                files: p.affected_files,
                retrieval_queries: p.retrieval_questions,
                ..ExtractedPayload::default()
            })
            .unwrap_or_else(|_| fallback_payload(record)),
        KnowledgeKind::Directive => serde_json::from_str::<DirectivePayload>(&record.payload_json)
            .map(|p| ExtractedPayload {
                goal: if p.reason.is_empty() {
                    p.action.clone()
                } else {
                    format!("{}\n\n{}", p.action, p.reason)
                },
                trigger: Some(p.trigger).filter(|s| !s.is_empty()),
                avoid: p.avoid,
                verify: p.verify,
                invalid_if: p.invalid_if,
                files: p.files,
                retrieval_queries: p.retrieval_queries,
                ..ExtractedPayload::default()
            })
            .unwrap_or_else(|_| fallback_payload(record)),
        KnowledgeKind::Claim => serde_json::from_str::<ClaimPayload>(&record.payload_json)
            .map(|p| ExtractedPayload {
                goal: p.statement,
                trigger: p.trigger,
                invalid_if: p.invalid_if,
                files: p.affected_files,
                retrieval_queries: p.retrieval_questions,
                ..ExtractedPayload::default()
            })
            .unwrap_or_else(|_| fallback_payload(record)),
        KnowledgeKind::Context => serde_json::from_str::<ContextPayload>(&record.payload_json)
            .map(|p| ExtractedPayload {
                goal: p.statement,
                invalid_if: p.invalid_if,
                files: p.files,
                retrieval_queries: p.retrieval_queries,
                ..ExtractedPayload::default()
            })
            .unwrap_or_else(|_| fallback_payload(record)),
        KnowledgeKind::Episode => serde_json::from_str::<EpisodePayload>(&record.payload_json)
            .map(|p| ExtractedPayload {
                goal: p.summary,
                trigger: p.trigger,
                files: p.affected_files,
                ..ExtractedPayload::default()
            })
            .unwrap_or_else(|_| fallback_payload(record)),
    }
}

fn fallback_payload(record: &KnowledgeRecord) -> ExtractedPayload {
    ExtractedPayload {
        goal: if record.retrieval_text.trim().is_empty() {
            record.title.clone()
        } else {
            record.retrieval_text.clone()
        },
        ..ExtractedPayload::default()
    }
}

fn skill_name(record: &KnowledgeRecord) -> String {
    let slug = slugify(&record.title);
    if slug == "lesson" {
        let short = if record.id.len() >= 12 {
            &record.id[..12]
        } else {
            &record.id
        };
        format!("lesson-{short}")
    } else {
        slug
    }
}

fn skill_description(record: &KnowledgeRecord) -> String {
    let extracted = extract_payload(record);
    let raw = if !extracted.goal.is_empty() {
        extracted.goal
    } else {
        record.title.clone()
    };
    let one_line: String = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(220)
        .collect();
    if one_line.is_empty() {
        record.title.clone()
    } else {
        one_line
    }
}

fn yaml_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', " ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FeedbackOutcome;
    use fieldcraft_core::{AgentTool, KnowledgeSource};
    use tempfile::tempdir;

    fn make_record(id: &str, kind: KnowledgeKind, title: &str, payload: &str) -> KnowledgeRecord {
        KnowledgeRecord {
            id: id.to_string(),
            project_id: "fieldcraft".to_string(),
            kind,
            state: KnowledgeState::Active,
            title: title.to_string(),
            payload_json: payload.to_string(),
            confidence: 0.6,
            source: KnowledgeSource::Session {
                session_id: "s1".to_string(),
                tool: AgentTool::Cursor,
            },
            superseded_by: None,
            project_specific: true,
            created_at: Utc::now(),
            valid_until: None,
            retrieval_text: title.to_string(),
            schema_version: 3,
            phase: None,
            tier: KnowledgeTier::Active,
            owner: None,
            session_date: None,
            source_refs: vec![],
            protected: false,
        }
    }

    fn procedure_payload() -> String {
        serde_json::to_string(&ProcedurePayload {
            goal: "Verify Rust changes before push".into(),
            preconditions: vec!["workspace compiles".into()],
            steps: vec![
                "cargo test --workspace".into(),
                "cargo clippy --workspace -- -D warnings".into(),
            ],
            failure_modes: vec!["ignored failing tests".into()],
            affected_files: vec!["Cargo.toml".into()],
            retrieval_questions: vec!["how do I verify rust changes".into()],
            trigger: Some("before pushing rust changes".into()),
            invalid_if: Some("non-rust repo".into()),
        })
        .unwrap()
    }

    fn directive_payload() -> String {
        serde_json::to_string(&DirectivePayload {
            action: "Never auto-promote lessons from hooks or cron".into(),
            reason: "Promotion is a human decision and must stay dry-run by default".into(),
            files: vec!["AGENTS.md".into()],
            trigger: "adding scheduled Fieldcraft jobs".into(),
            avoid: vec!["silent --apply from a hook".into()],
            verify: Some("job command has no --apply".into()),
            invalid_if: None,
            retrieval_queries: vec!["auto promote lesson".into()],
        })
        .unwrap()
    }

    fn confirm(repo: &KnowledgeRepository, id: &str, n: usize) {
        for i in 0..n {
            repo.apply_feedback_outcome(
                id,
                FeedbackOutcome::Confirmed,
                Some(&format!("sess-ok-{i}")),
                "test",
                None,
            )
            .unwrap();
        }
    }

    fn ignore(repo: &KnowledgeRepository, id: &str, n: usize) {
        for i in 0..n {
            repo.apply_feedback_outcome(
                id,
                FeedbackOutcome::Ignored,
                Some(&format!("sess-ig-{i}")),
                "test",
                None,
            )
            .unwrap();
        }
    }

    fn contradict(repo: &KnowledgeRepository, id: &str, n: usize) {
        for i in 0..n {
            repo.apply_feedback_outcome(
                id,
                FeedbackOutcome::Contradicted,
                Some(&format!("sess-no-{i}")),
                "test",
                None,
            )
            .unwrap();
        }
    }

    fn seed_workhorse(repo: &KnowledgeRepository) -> KnowledgeRecord {
        let rec = make_record(
            "proc-workhorse",
            KnowledgeKind::Procedure,
            "Verify cargo before push",
            &procedure_payload(),
        );
        repo.insert_knowledge(&rec).unwrap();
        confirm(repo, &rec.id, 3);
        rec
    }

    fn request(dir: &Path, apply: bool, id: Option<&str>, target: PromoteTarget) -> PromoteRequest {
        PromoteRequest {
            apply,
            lesson_id: id.map(ToOwned::to_owned),
            target,
            dest: None,
            output_root: dir.to_path_buf(),
            cwd: dir.to_path_buf(),
            thresholds: PromoteThresholds::default(),
            include_promoted: false,
            force: false,
            limit: 20,
        }
    }

    #[test]
    fn eligibility_accepts_workhorse_procedure() {
        let counts = OutcomeCounts {
            confirmed: 3,
            ignored: 1,
            contradicted: 0,
            total: 4,
        };
        assert!(
            eligibility_reason(
                "procedure",
                "active",
                "active",
                &counts,
                false,
                &PromoteThresholds::default()
            )
            .is_none()
        );
    }

    #[test]
    fn eligibility_rejects_ghosts_noise_claims_and_contradictions() {
        let thresholds = PromoteThresholds::default();
        assert_eq!(
            eligibility_reason(
                "procedure",
                "active",
                "active",
                &OutcomeCounts::default(),
                false,
                &thresholds
            )
            .as_deref(),
            Some("ghost: no outcome events")
        );
        assert!(
            eligibility_reason(
                "procedure",
                "active",
                "active",
                &OutcomeCounts {
                    confirmed: 0,
                    ignored: 5,
                    contradicted: 0,
                    total: 5,
                },
                false,
                &thresholds
            )
            .unwrap()
            .starts_with("noise:")
        );
        assert!(
            eligibility_reason(
                "claim",
                "active",
                "active",
                &OutcomeCounts {
                    confirmed: 5,
                    ignored: 0,
                    contradicted: 0,
                    total: 5,
                },
                false,
                &thresholds
            )
            .unwrap()
            .contains("not in promote set")
        );
        assert!(
            eligibility_reason(
                "procedure",
                "active",
                "active",
                &OutcomeCounts {
                    confirmed: 4,
                    ignored: 0,
                    contradicted: 1,
                    total: 5,
                },
                false,
                &thresholds
            )
            .unwrap()
            .contains("contradicted")
        );
        assert_eq!(
            eligibility_reason(
                "procedure",
                "active",
                "archive",
                &OutcomeCounts {
                    confirmed: 4,
                    ignored: 0,
                    contradicted: 0,
                    total: 4,
                },
                false,
                &thresholds
            )
            .as_deref(),
            Some("tier archive is excluded")
        );
    }

    #[test]
    fn list_candidates_keeps_workhorse_excludes_noise_ghost_claim() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        seed_workhorse(&repo);

        let mut ghost = make_record(
            "ghost-1",
            KnowledgeKind::Procedure,
            "Never retrieved procedure",
            &procedure_payload(),
        );
        ghost.created_at = Utc::now() - chrono::Duration::days(40);
        repo.insert_knowledge(&ghost).unwrap();

        let noise = make_record(
            "noise-1",
            KnowledgeKind::Procedure,
            "Noisy procedure",
            &procedure_payload(),
        );
        repo.insert_knowledge(&noise).unwrap();
        ignore(&repo, &noise.id, 5);

        let claim = make_record(
            "claim-1",
            KnowledgeKind::Claim,
            "Strong claim",
            r#"{"statement":"claims stay retrieval-only"}"#,
        );
        repo.insert_knowledge(&claim).unwrap();
        confirm(&repo, &claim.id, 4);

        let contradicted = make_record(
            "contra-1",
            KnowledgeKind::Directive,
            "Contradicted directive",
            &directive_payload(),
        );
        repo.insert_knowledge(&contradicted).unwrap();
        confirm(&repo, &contradicted.id, 3);
        contradict(&repo, &contradicted.id, 1);

        let candidates =
            list_promote_candidates(&repo, &PromoteThresholds::default(), false, 20).unwrap();
        let ids: Vec<&str> = candidates.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["proc-workhorse"]);
    }

    #[test]
    fn dry_run_lists_and_does_not_write() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        seed_workhorse(&repo);
        let req = request(dir.path(), false, None, PromoteTarget::Skill);
        let report = run_promote(&repo, &req).unwrap();
        assert_eq!(report.mode, "DRY-RUN");
        assert_eq!(report.candidates.len(), 1);
        assert!(report.applied.is_none());
        assert!(!dir.path().join(PROMOTED_SKILLS_DIR).exists());
        let events: i64 = repo
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM knowledge_events WHERE event_type = ?1",
                [PROMOTE_EVENT_TYPE],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(events, 0);
    }

    #[test]
    fn apply_without_id_is_rejected() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        seed_workhorse(&repo);
        let err = run_promote(
            &repo,
            &request(dir.path(), true, None, PromoteTarget::Skill),
        )
        .unwrap_err();
        match err {
            TrError::InvalidRequest { message } => {
                assert!(message.contains("refusing to bulk-apply"), "{message}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
        assert!(!dir.path().join(PROMOTED_SKILLS_DIR).exists());
    }

    #[test]
    fn dry_run_preview_for_id_does_not_write() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let rec = seed_workhorse(&repo);
        let report = run_promote(
            &repo,
            &request(dir.path(), false, Some(&rec.id), PromoteTarget::Skill),
        )
        .unwrap();
        assert_eq!(report.mode, "DRY-RUN");
        let preview = report.preview.expect("preview");
        assert!(
            preview
                .artifact
                .contains("name: \"verify-cargo-before-push\"")
        );
        assert!(preview.artifact.contains("Source lesson: `proc-workhorse`"));
        assert!(preview.destination.ends_with("SKILL.md"));
        assert!(!preview.destination.exists());
        let events: i64 = repo
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM knowledge_events WHERE knowledge_id = ?1 AND event_type = ?2",
                params![rec.id, PROMOTE_EVENT_TYPE],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(events, 0);
        assert_eq!(
            repo.get_knowledge(&rec.id).unwrap().unwrap().state,
            KnowledgeState::Active
        );
    }

    #[test]
    fn apply_skill_writes_package_and_provenance_without_deleting_lesson() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let rec = seed_workhorse(&repo);
        let report = run_promote(
            &repo,
            &request(dir.path(), true, Some(&rec.id), PromoteTarget::Skill),
        )
        .unwrap();
        assert_eq!(report.mode, "APPLY");
        let applied = report.applied.expect("applied");
        let body = fs::read_to_string(&applied.destination).unwrap();
        assert!(body.contains("Verify Rust changes before push"));
        assert!(body.contains("cargo test --workspace"));
        assert!(body.contains("proc-workhorse"));
        assert_eq!(applied.provenance.source_lesson_id, rec.id);
        assert_eq!(applied.provenance.confirmed, 3);
        assert_eq!(applied.provenance.target, "skill");

        let note: String = repo
            .connection()
            .query_row(
                "SELECT note FROM knowledge_events WHERE knowledge_id = ?1 AND event_type = ?2",
                params![rec.id, PROMOTE_EVENT_TYPE],
                |row| row.get(0),
            )
            .unwrap();
        let parsed: PromoteProvenance = serde_json::from_str(&note).unwrap();
        assert_eq!(
            parsed.destination,
            applied.destination.display().to_string()
        );
        assert_eq!(parsed.confirmed, 3);

        let still = repo.get_knowledge(&rec.id).unwrap().unwrap();
        assert_eq!(still.state, KnowledgeState::Active);
        assert_eq!(still.tier, KnowledgeTier::Active);

        let listed =
            list_promote_candidates(&repo, &PromoteThresholds::default(), false, 20).unwrap();
        assert!(
            listed.is_empty(),
            "promoted lesson should drop from default list"
        );
    }

    #[test]
    fn apply_agents_md_appends_snippet() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let rec = make_record(
            "dir-workhorse",
            KnowledgeKind::Directive,
            "No silent auto-promote",
            &directive_payload(),
        );
        repo.insert_knowledge(&rec).unwrap();
        confirm(&repo, &rec.id, 3);

        let report = run_promote(
            &repo,
            &request(dir.path(), true, Some(&rec.id), PromoteTarget::AgentsMd),
        )
        .unwrap();
        let dest = report.applied.expect("applied").destination;
        assert_eq!(dest, dir.path().join("AGENTS.md"));
        let body = fs::read_to_string(&dest).unwrap();
        assert!(body.contains("<!-- fieldcraft-promote:start lesson:dir-workhorse -->"));
        assert!(body.contains("Never auto-promote lessons from hooks or cron"));
        assert!(body.contains("Source:** Fieldcraft lesson `dir-workhorse`"));

        // Re-apply with force replaces the same block instead of duplicating.
        let mut again = request(dir.path(), true, Some(&rec.id), PromoteTarget::AgentsMd);
        again.force = true;
        run_promote(&repo, &again).unwrap();
        let body = fs::read_to_string(&dest).unwrap();
        assert_eq!(
            body.matches("<!-- fieldcraft-promote:start lesson:dir-workhorse -->")
                .count(),
            1
        );
    }

    #[test]
    fn skill_markdown_is_ingestible_by_skill_scanner() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let rec = seed_workhorse(&repo);
        let report = run_promote(
            &repo,
            &request(dir.path(), true, Some(&rec.id), PromoteTarget::Skill),
        )
        .unwrap();
        let dest = report.applied.expect("applied").destination;
        let package_dir = dest.parent().unwrap();
        let scan = crate::skills::SkillRepository::scan_path(package_dir).unwrap();
        assert_eq!(scan.package.name, "verify-cargo-before-push");
        assert!(!scan.package.description.is_empty());
        assert_eq!(scan.package.scan_status, "eligible");
    }
}
