use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum};
use fieldcraft_core::{
    ClaimPayload, ContextPayload, KnowledgeKind, KnowledgeRecord, KnowledgeSource, KnowledgeState,
    KnowledgeTier, LearningSourceRecord, LearningSourceSeed, LearningTraversalPolicy,
    ProcedurePayload, SourceFilter, SourceReference,
};
use fieldcraft_retrieval::{RetrievalRequest, index_knowledge_sqlite, retrieve};
use fieldcraft_storage::{
    KnowledgeRepository, PromoteRequest, PromoteTarget, PromoteThresholds, analyze_report,
    fieldcraft_db_path, lifecycle_report, render_analysis_report_stdout,
    render_lifecycle_report_stdout, render_promote_report, reset_source_session_to_pending,
    run_promote,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::{LazyLock, Mutex};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

mod onboarding;
mod prompt_hook;

#[derive(Parser)]
#[command(
    name = "fieldcraft",
    about = "Local learning layer for AI coding sessions",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum HookCommand {
    /// Claude Code UserPromptSubmit hook: gate with Jev, inject up to 3 new lessons
    PromptSubmit,
}

#[derive(Subcommand)]
enum Command {
    /// Search lessons for a project
    Query {
        /// What to search for
        query_text: String,
        /// Project ID (default: derived from current working directory)
        #[arg(long)]
        project: Option<String>,
        /// Optional session id to tag this retrieval for closed-loop feedback
        #[arg(long)]
        session_id: Option<String>,
        /// Maximum number of results to return
        #[arg(long, default_value_t = 5)]
        limit: usize,
        /// Enable "inner voice" LLM sub-agent that reasons about which
        /// lessons are actually relevant and filters out misleading ones.
        /// Requires OPENAI_API_KEY. Based on SWE-ContextBench (arXiv:2602.08316)
        /// finding that oracle selection beats free selection by +12pp.
        #[arg(long)]
        inner_voice: bool,
        /// Eval/debug source-origin filter: any | learned-only | external-only
        #[arg(long, value_enum, default_value_t = SourceFilterArg::Any)]
        source_filter: SourceFilterArg,
        /// Retrieval tier: active | archive
        #[arg(long, value_enum, default_value_t = TierArg::Active)]
        tier: TierArg,
    },

    /// Write a lesson manually
    Write {
        /// Summary text to store
        summary: String,
        /// Project ID (default: derived from current working directory)
        #[arg(long)]
        project: Option<String>,
        /// Lesson kind
        #[arg(long, value_enum, default_value_t = KindArg::Claim)]
        kind: KindArg,
        /// Subtask phase this lesson applies to (analyze/reproduce/edit/verify).
        /// Used for category-filtered retrieval (arXiv:2602.21611).
        #[arg(long)]
        phase: Option<String>,
    },

    /// Discover and extract knowledge from coding session transcripts
    Ingest {
        /// Project ID (default: derived from current working directory)
        #[arg(long)]
        project: Option<String>,
        /// Re-extract all sessions, ignoring previous results
        #[arg(long, default_value_t = false)]
        rebuild: bool,
        /// Path to Claude home directory (default: ~/.claude)
        #[arg(long)]
        claude_home: Option<PathBuf>,
        /// Additional session source directories to scan
        #[arg(long)]
        source: Vec<PathBuf>,
        /// Skip Codex session discovery
        #[arg(long)]
        skip_codex: bool,
    },

    /// Show extraction stats for a project
    Status {
        /// Project ID (default: derived from current working directory)
        #[arg(long)]
        project: Option<String>,
        /// Print a single-line summary suitable for startup hooks / status UIs
        #[arg(long, default_value_t = false)]
        compact: bool,
    },

    /// Batch import v1 markdown knowledge files into SQLite
    Import {
        /// Project ID to import
        #[arg(long)]
        project: String,
        /// Path to v1 knowledge directory (default: ~/.ai-memory/knowledge/{project})
        #[arg(long)]
        source: Option<PathBuf>,
    },

    /// Rebuild dense vector index for all records missing embeddings
    Reindex {
        /// Project ID (default: all projects)
        #[arg(long)]
        project: Option<String>,
        /// Batch size for embedding computation
        #[arg(long, default_value_t = 32)]
        batch_size: usize,
        /// Re-embed every selected record, overwriting existing vectors
        #[arg(long, default_value_t = false)]
        all: bool,
    },

    /// Re-extract old lessons using the new directive/context format
    ReExtract {
        /// Only re-extract records with schema_version below this value
        #[arg(long, default_value_t = 3)]
        target_version: u32,
        /// Print what would be done without making changes
        #[arg(long)]
        dry_run: bool,
    },

    /// Record feedback on a previously retrieved lesson.
    /// Outcome must be one of: confirmed | ignored | contradicted.
    Feedback {
        /// Lesson id (full hex, as returned by `fieldcraft query`)
        id: String,
        /// Outcome: confirmed | ignored | contradicted
        #[arg(long)]
        outcome: String,
        /// Optional free-text note describing why
        #[arg(long)]
        note: Option<String>,
        /// Optional session id to correlate the feedback with
        #[arg(long)]
        session_id: Option<String>,
    },

    /// Scan a Claude/Codex session transcript and emit closed-loop feedback
    /// events: Confirmed for inline `lesson:<id>` citations, Contradicted for
    /// explicit refutations ("lesson:<id> is wrong/outdated"), and Ignored for
    /// retrieved-but-not-cited lessons whose cue match factor exceeded the
    /// threshold (server-computed via POST /sessions/trace-analyze).
    ///
    /// Designed to run from the Stop hook as a detached process; idempotent
    /// across reruns on the same (lesson, session, source) triple.
    TraceAnalyze {
        /// Session id (used for idempotency + audit trail in lesson_outcome_events).
        #[arg(long = "session")]
        session_id: String,
        /// Path to the session transcript (JSONL).
        #[arg(long)]
        transcript: PathBuf,
        /// Working directory the session ran in. Used to derive the project id
        /// for retrieval correlation. Defaults to the current process CWD.
        #[arg(long)]
        cwd: Option<PathBuf>,
    },

    /// Cheap local friction score for a session transcript (no LLM).
    ///
    /// Used by the Stop hook to skip quiet-session distillation. Prints one
    /// machine-grepable line:
    /// `decision=skip score=1 threshold=3 reasons=tool_failure:1`
    DistillGate {
        /// Path to the session transcript (JSONL).
        #[arg(long)]
        transcript: PathBuf,
        /// Minimum score to spawn the LLM distiller. 0 disables the gate.
        #[arg(long, env = "FIELDCRAFT_DISTILL_MIN_FRICTION", default_value_t = 3)]
        min_friction: u32,
        /// Bypass the score check and always decide run.
        #[arg(long)]
        force: bool,
        /// Print JSON instead of the one-line log format.
        #[arg(long)]
        json: bool,
    },

    /// Import raw sessions to a remote fieldcraft-server via POST /sessions/import.
    /// Requires FIELDCRAFT_SERVER_URL to be set. Falls back to local ingest when unset.
    ImportSessions {
        /// Additional session source directories to scan (default: ~/.codex ~/.claude)
        #[arg(long)]
        source: Vec<PathBuf>,
    },

    /// Submit pending sessions to the OpenAI Batch API for async extraction.
    /// Default: --batch-api --no-wait. Requires OPENAI_API_KEY env var.
    ExtractBatch {
        /// Maximum number of sessions to claim per run
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Abort if estimated cost exceeds this threshold (USD)
        #[arg(long, default_value_t = 10.0)]
        max_spend_usd: f64,
        /// Filter to a specific project
        #[arg(long)]
        project: Option<String>,
        /// Use the OpenAI Batch API (default)
        #[arg(long, default_value_t = true)]
        batch_api: bool,
        /// Use sync extraction (redirects to `fieldcraft ingest`)
        #[arg(long, conflicts_with = "batch_api")]
        sync: bool,
        /// Wait for batch completion before exiting (polls every 10 min)
        #[arg(long)]
        wait: bool,
        /// Exit immediately after submission (default)
        #[arg(long, conflicts_with = "wait")]
        no_wait: bool,
    },

    /// Poll OpenAI Batch API for status and reconcile completed batches.
    ExtractBatchPoll,

    /// Show status of recent extraction batches.
    ExtractBatchStatus {
        /// Number of recent batches to show
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },

    /// Queue and claim external source-learning work for worker agents
    Learn {
        #[command(subcommand)]
        command: LearnCommand,
    },

    /// Backfill `session_date` on knowledge records from source session `started_at`.
    BackfillSessionDates,

    /// Closed-loop feedback stats: workhorses, noise candidates, ghosts, session lift
    Stats {
        /// Maximum rows per category
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Time window in days for outcome event proof counts. Default 7.
        #[arg(long, default_value_t = 7)]
        days: u32,
    },

    /// Show retrieval log + feedback outcomes for a single session
    Session {
        /// Session identifier (matches lesson_outcome_events.session_id)
        session_id: String,
    },

    /// Show source-session provenance for one lesson
    Provenance {
        /// Lesson id
        id: String,
    },

    /// Analyze the closed-loop catalog and propose maintenance actions
    /// (archive noise, investigate contradicted clusters, prune ghosts, etc.).
    /// Designed to run on a daily schedule against the server's catalog.
    Analyze {
        /// Time window in days for "recent" analysis. Default 14.
        #[arg(long, default_value_t = 14)]
        days: u32,
        /// Output format. `stdout` (default) prints a human-readable report;
        /// `json` emits a machine-parseable AnalysisReport for downstream tools.
        #[arg(long, value_enum, default_value_t = AnalyzeFormat::Stdout)]
        format: AnalyzeFormat,
        /// Override the data directory (defaults to FIELDCRAFT_HOME / ~/.fieldcraft).
        /// On the server box, point at the server's catalog (e.g. ~/.dam).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Maximum rows per finding category.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },

    /// Memory Lifecycle eviction (L2): propose, or with --apply perform,
    /// reversible archival of old/never-served/unconfirmed (ghost) or
    /// repeatedly-contradicted records. Always non-destructive (archive, not
    /// delete) and audited; default is a read-only dry run.
    Lifecycle {
        /// Apply the archival. Without this flag the command is a read-only dry
        /// run that prints candidates and writes nothing.
        #[arg(long)]
        apply: bool,
        /// Minimum age in days for a ghost candidate.
        #[arg(long, default_value_t = 60)]
        min_age_days: i64,
        /// Override the data directory (defaults to FIELDCRAFT_HOME / ~/.fieldcraft).
        /// On the server box, point at the server's catalog (e.g. ~/.dam).
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// Promote a high-confirm procedure/directive lesson into a local
    /// SKILL.md package or an AGENTS.md directive snippet.
    ///
    /// Default is a read-only dry run that lists eligible workhorses
    /// (confirmed ≥ 3, confirm_rate ≥ 0.5, contradicted = 0). Nothing is
    /// written unless `--apply` is passed with `--id`. Never scheduled;
    /// hooks and cron must not call `--apply`. See docs/promote-checklist.md.
    Promote {
        /// Write the artifact and record provenance. Without this flag the
        /// command only previews candidates / the destination file.
        #[arg(long, conflicts_with = "dry_run")]
        apply: bool,
        /// Explicit dry-run. This is the default when `--apply` is omitted;
        /// the flag exists so help documents both modes.
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        /// Lesson id to promote. Required with `--apply` (no bulk write).
        #[arg(long)]
        id: Option<String>,
        /// Destination kind: `skill` writes a SKILL.md package;
        /// `agents-md` appends a directive snippet to AGENTS.md.
        #[arg(long, value_enum, default_value_t = PromoteTargetArg::Skill)]
        target: PromoteTargetArg,
        /// Skill package root, or AGENTS.md file/directory. Defaults:
        /// `<catalog>/promoted-skills/{project}/{slug}/SKILL.md` or
        /// `./AGENTS.md`.
        #[arg(long)]
        dest: Option<PathBuf>,
        /// Minimum confirmed outcomes (default 3; env FIELDCRAFT_PROMOTE_MIN_CONFIRMED)
        #[arg(long, env = "FIELDCRAFT_PROMOTE_MIN_CONFIRMED", default_value_t = 3)]
        min_confirmed: i64,
        /// Minimum confirm / total rate (default 0.5; env FIELDCRAFT_PROMOTE_MIN_CONFIRM_RATE)
        #[arg(
            long,
            env = "FIELDCRAFT_PROMOTE_MIN_CONFIRM_RATE",
            default_value_t = 0.5
        )]
        min_confirm_rate: f64,
        /// Maximum contradicted outcomes (default 0; env FIELDCRAFT_PROMOTE_MAX_CONTRADICTED)
        #[arg(long, env = "FIELDCRAFT_PROMOTE_MAX_CONTRADICTED", default_value_t = 0)]
        max_contradicted: i64,
        /// Comma-separated lesson kinds eligible for promotion
        #[arg(long, default_value = "procedure,directive")]
        kinds: String,
        /// Include lessons that already have a lesson_promoted audit event
        #[arg(long, default_value_t = false)]
        include_promoted: bool,
        /// Bypass eligibility and overwrite an existing destination
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Maximum candidates to list
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Override the data directory (defaults to FIELDCRAFT_HOME / ~/.fieldcraft)
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// Dream-tier generative consolidation (L5): cluster related confirmed
    /// lessons and abstract each cluster into ONE derived Claim via an LLM.
    /// Guardrails: a derived claim must cite >= 3 source lessons and lands
    /// `provisional` (activated only by a real confirmed outcome). Sources are
    /// retained at synthesis time and demoted only after confirmed promotion.
    /// Default is a read-only dry run that prints the clusters.
    Dream {
        /// Actually abstract and write derived claims. Without this flag the
        /// command only prints the clusters it would dream over.
        #[arg(long)]
        apply: bool,
        /// Maximum clusters to process per run.
        #[arg(long, default_value_t = 3)]
        max_clusters: usize,
        /// LLM program invoked once per cluster with the abstraction prompt as
        /// its single trailing argument; it must print a JSON object with
        /// "title" and "statement". Default: `codex exec --sandbox read-only`.
        #[arg(long)]
        llm_cmd: Option<String>,
        /// Override the data directory (defaults to FIELDCRAFT_HOME / ~/.fieldcraft).
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// Interactive first-time setup: store mode, embedding key, distiller, agent hooks, then a health check
    Setup {
        /// Path to the Fieldcraft git checkout (default: $FIELDCRAFT_REPO or ~/Code/fieldcraft)
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Accept every default without prompting
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Agent hook entry points (hook JSON on stdin; never fail the agent)
    Hook {
        #[command(subcommand)]
        which: HookCommand,
    },
    /// Check the install: config, secrets, server, agent hooks, instructions, distiller, and a query round trip
    Doctor {
        /// Print the checks as JSON
        #[arg(long)]
        json: bool,
        /// Skip network checks (embedding call, server, query round trip)
        #[arg(long)]
        offline: bool,
    },
    /// Pull the Fieldcraft repo and reinstall the local CLI binaries/config
    Update {
        /// Path to the Fieldcraft git checkout (default: ~/Code/fieldcraft)
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Install in local mode, removing remote client routing
        #[arg(long, conflicts_with = "server_url")]
        local: bool,
        /// Remote fieldcraft-server URL for client mode
        #[arg(long)]
        server_url: Option<String>,
        /// Remote fieldcraft-server auth token
        #[arg(long)]
        auth_token: Option<String>,
        /// Client enrollment label passed to install.sh when enrolling
        #[arg(long)]
        label: Option<String>,
    },

    /// Run evaluation suites
    Eval {
        #[command(subcommand)]
        command: EvalCommand,
    },
}

#[derive(Subcommand)]
enum EvalCommand {
    /// Compare whole vs split external-derived KnowledgeRecord retrieval
    ExternalSkills {
        /// JSONL corpus with skill_name, description, body, optional triggers
        #[arg(long)]
        corpus: PathBuf,
        /// JSONL queries with id, query, expected[], optional acceptable[], should_abstain
        #[arg(long)]
        queries: PathBuf,
        /// Optional Markdown report path
        #[arg(long)]
        report: Option<PathBuf>,
        /// Parent data directory for isolated eval catalogs. Defaults to a temp directory.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Maximum records retrieved per query, capped at 3
        #[arg(long, default_value_t = 3)]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum LearnCommand {
    /// Queue URLs/files/directories from JSONL or plain-line manifest
    Queue {
        /// Manifest path. JSONL lines may be LearningSourceSeed objects; plain lines are treated as URIs.
        #[arg(long)]
        manifest: PathBuf,
        /// Default project for plain-line manifest entries
        #[arg(long)]
        project: Option<String>,
        /// Default scope hint for plain-line manifest entries
        #[arg(long)]
        scope_hint: Option<String>,
    },
    /// Atomically claim pending source-learning work for this worker
    Claim {
        /// Maximum records to claim
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Optional project filter
        #[arg(long)]
        project: Option<String>,
        /// Worker identifier stored on claimed rows
        #[arg(long)]
        worker_id: Option<String>,
    },
    /// Claim sources and derive external KnowledgeRecords
    Run {
        /// Maximum sources to claim and process
        #[arg(long, default_value_t = 5)]
        limit: usize,
        /// Optional project filter
        #[arg(long)]
        project: Option<String>,
        /// Worker identifier stored on claimed rows
        #[arg(long)]
        worker_id: Option<String>,
        /// Maximum records to derive per source
        #[arg(long, default_value_t = 12)]
        max_records_per_source: usize,
    },
    /// Mark a claimed source as succeeded after derived records are written
    Complete {
        /// Learning source id
        source_id: String,
        /// Hash of the traversed/normalized source content, if available
        #[arg(long)]
        source_hash: Option<String>,
        /// Number of KnowledgeRecords written from this source
        #[arg(long, default_value_t = 0)]
        derived_record_count: u32,
    },
    /// Mark a claimed source as failed
    Fail {
        /// Learning source id
        source_id: String,
        /// Error visible in the server catalog
        #[arg(long)]
        error: String,
    },
}

#[derive(Clone, ValueEnum)]
enum KindArg {
    Claim,
    Procedure,
    Episode,
    Directive,
    Context,
}

#[derive(Clone, Copy, ValueEnum)]
enum SourceFilterArg {
    Any,
    LearnedOnly,
    ExternalOnly,
}

#[derive(Clone, Copy, ValueEnum)]
enum TierArg {
    Active,
    Archive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AtomizeMode {
    Whole,
    Split,
}

impl From<SourceFilterArg> for SourceFilter {
    fn from(value: SourceFilterArg) -> Self {
        match value {
            SourceFilterArg::Any => SourceFilter::Any,
            SourceFilterArg::LearnedOnly => SourceFilter::LearnedOnly,
            SourceFilterArg::ExternalOnly => SourceFilter::ExternalOnly,
        }
    }
}

impl From<TierArg> for KnowledgeTier {
    fn from(value: TierArg) -> Self {
        match value {
            TierArg::Active => KnowledgeTier::Active,
            TierArg::Archive => KnowledgeTier::Archive,
        }
    }
}

#[derive(Clone, ValueEnum)]
enum AnalyzeFormat {
    Stdout,
    Json,
}

#[derive(Clone, Copy, ValueEnum)]
enum PromoteTargetArg {
    /// SKILL.md-style package under promoted-skills/
    Skill,
    /// AGENTS.md directive snippet
    AgentsMd,
}

impl From<PromoteTargetArg> for PromoteTarget {
    fn from(value: PromoteTargetArg) -> Self {
        match value {
            PromoteTargetArg::Skill => PromoteTarget::Skill,
            PromoteTargetArg::AgentsMd => PromoteTarget::AgentsMd,
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    rt.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Query {
            query_text,
            project,
            session_id,
            limit,
            inner_voice,
            source_filter,
            tier,
        } => {
            let project_id = resolve_project_id(project);
            cmd_query(
                &project_id,
                &query_text,
                session_id.as_deref(),
                limit,
                inner_voice,
                source_filter.into(),
                tier.into(),
            )
            .await
        }

        Command::Write {
            summary,
            project,
            kind,
            phase,
        } => {
            let project_id = resolve_project_id(project);
            cmd_write(&project_id, &summary, kind, phase.as_deref()).await
        }

        Command::Ingest {
            project,
            rebuild,
            claude_home,
            source,
            skip_codex,
        } => {
            let project_id = resolve_project_id(project);
            cmd_ingest(&project_id, rebuild, claude_home, &source, skip_codex).await
        }

        Command::Status { project, compact } => {
            let project_id = resolve_project_id(project);
            cmd_status(&project_id, compact).await
        }

        Command::Setup { repo, yes } => onboarding::run_setup(repo, yes).await,

        Command::Hook {
            which: HookCommand::PromptSubmit,
        } => {
            prompt_hook::run_prompt_submit().await;
            Ok(())
        }

        Command::Doctor { json, offline } => onboarding::cmd_doctor(json, offline).await,

        Command::Stats { limit, days } => cmd_stats(limit, days).await,

        Command::Session { session_id } => cmd_session(&session_id),

        Command::Provenance { id } => cmd_provenance(&id),

        Command::Analyze {
            days,
            format,
            data_dir,
            limit,
        } => cmd_analyze(days, format, data_dir.as_deref(), limit).await,

        Command::Lifecycle {
            apply,
            min_age_days,
            data_dir,
        } => cmd_lifecycle(apply, min_age_days, data_dir.as_deref()).await,

        Command::Promote {
            apply,
            dry_run: _,
            id,
            target,
            dest,
            min_confirmed,
            min_confirm_rate,
            max_contradicted,
            kinds,
            include_promoted,
            force,
            limit,
            data_dir,
        } => cmd_promote(PromoteCliArgs {
            apply,
            lesson_id: id,
            target,
            dest,
            min_confirmed,
            min_confirm_rate,
            max_contradicted,
            kinds,
            include_promoted,
            force,
            limit,
            data_dir,
        }),

        Command::Dream {
            apply,
            max_clusters,
            llm_cmd,
            data_dir,
        } => cmd_dream(apply, max_clusters, llm_cmd.as_deref(), data_dir.as_deref()).await,

        Command::Update {
            repo,
            local,
            server_url,
            auth_token,
            label,
        } => cmd_update(repo, local, server_url, auth_token, label),

        Command::Import { project, source } => cmd_import(&project, source).await,

        Command::Reindex {
            project,
            batch_size,
            all,
        } => cmd_reindex(project.as_deref(), batch_size, all).await,

        Command::ReExtract {
            target_version,
            dry_run,
        } => {
            cmd_reextract(target_version, dry_run)?;
            Ok(())
        }

        Command::Feedback {
            id,
            outcome,
            note,
            session_id,
        } => cmd_feedback(&id, &outcome, note.as_deref(), session_id.as_deref()).await,

        Command::TraceAnalyze {
            session_id,
            transcript,
            cwd,
        } => cmd_trace_analyze(&session_id, &transcript, cwd.as_deref()).await,

        Command::DistillGate {
            transcript,
            min_friction,
            force,
            json,
        } => cmd_distill_gate(&transcript, min_friction, force, json),

        Command::ImportSessions { source } => cmd_import_sessions(&source).await,

        Command::ExtractBatch {
            limit,
            max_spend_usd,
            project,
            sync,
            wait,
            ..
        } => {
            if sync {
                println!(
                    "Sync extraction is handled by `fieldcraft ingest`. \
                     Please use that command for immediate extraction."
                );
                return Ok(());
            }
            let project_filter = project.as_deref();
            let shared_root = {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                PathBuf::from(home).join(".fieldcraft")
            };
            let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
                anyhow::anyhow!(
                    "OPENAI_API_KEY environment variable not set. \
                     Export it before running extract-batch."
                )
            })?;
            let client = fieldcraft_extract::openai_batch::OpenAiBatchClient::new(api_key);
            fieldcraft_cli::cmd_extract_batch_inner(
                &shared_root,
                project_filter,
                limit,
                max_spend_usd,
                &client,
            )
            .await?;
            if wait {
                println!("--wait: polling for completion (10 min intervals)...");
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                    let done =
                        fieldcraft_cli::cmd_extract_batch_poll_inner(&shared_root, &client).await?;
                    if done {
                        break;
                    }
                }
            }
            Ok(())
        }

        Command::ExtractBatchPoll => {
            let shared_root = {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                PathBuf::from(home).join(".fieldcraft")
            };
            let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
                anyhow::anyhow!(
                    "OPENAI_API_KEY environment variable not set. \
                     Export it before running extract-batch-poll."
                )
            })?;
            let client = fieldcraft_extract::openai_batch::OpenAiBatchClient::new(api_key);
            fieldcraft_cli::cmd_extract_batch_poll_inner(&shared_root, &client).await?;
            Ok(())
        }

        Command::ExtractBatchStatus { limit } => {
            let shared_root = {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                PathBuf::from(home).join(".fieldcraft")
            };
            cmd_extract_batch_status_inner(&shared_root, limit)
        }

        Command::Learn { command } => match command {
            LearnCommand::Queue {
                manifest,
                project,
                scope_hint,
            } => cmd_learn_queue(&manifest, project.as_deref(), scope_hint.as_deref()).await,
            LearnCommand::Claim {
                limit,
                project,
                worker_id,
            } => cmd_learn_claim(limit, project.as_deref(), worker_id.as_deref()).await,
            LearnCommand::Run {
                limit,
                project,
                worker_id,
                max_records_per_source,
            } => {
                cmd_learn_run(
                    limit,
                    project.as_deref(),
                    worker_id.as_deref(),
                    max_records_per_source,
                )
                .await
            }
            LearnCommand::Complete {
                source_id,
                source_hash,
                derived_record_count,
            } => cmd_learn_complete(&source_id, source_hash.as_deref(), derived_record_count).await,
            LearnCommand::Fail { source_id, error } => cmd_learn_fail(&source_id, &error).await,
        },

        Command::BackfillSessionDates => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            let data_dir = PathBuf::from(&home).join(".fieldcraft");
            let db_path = fieldcraft_db_path_for_command("backfill-session-dates")?;
            let conn = fieldcraft_storage::open_catalog(&db_path)
                .context("open lesson sqlite database")?;
            let sessions_path = data_dir.join(".tr-source-sessions.sqlite3");
            let updated = fieldcraft_storage::backfill_session_dates(&conn, &sessions_path)
                .context("backfill session dates")?;
            println!("Backfilled {updated} records with session_date");
            Ok(())
        }

        Command::Eval { command } => match command {
            EvalCommand::ExternalSkills {
                corpus,
                queries,
                report,
                data_dir,
                limit,
            } => {
                cmd_eval_external_skills(
                    &corpus,
                    &queries,
                    report.as_deref(),
                    data_dir.as_deref(),
                    limit,
                )
                .await
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Project ID derivation
// ---------------------------------------------------------------------------

fn project_basename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "default".to_string())
}

fn git_toplevel(path: &Path) -> Option<PathBuf> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let toplevel = stdout.trim();
    if toplevel.is_empty() {
        None
    } else {
        Some(PathBuf::from(toplevel))
    }
}

fn natural_project_id_from_cwd(cwd: &Path) -> String {
    git_toplevel(cwd)
        .as_deref()
        .map(project_basename)
        .unwrap_or_else(|| project_basename(cwd))
}

fn canonicalize_project_id_for_client(project_id: &str) -> String {
    let db_path = fieldcraft_storage::fieldcraft_db_path();
    if local_catalog_allowed()
        && db_path.exists()
        && let Ok(repo) = KnowledgeRepository::open(&db_path)
        && let Ok(canonical) = repo.canonical_project_id(project_id)
    {
        return canonical;
    }
    fieldcraft_storage::canonical_project_id_from_existing(project_id, std::iter::empty::<&str>())
}

fn derive_project_id() -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    canonicalize_project_id_for_client(&natural_project_id_from_cwd(&cwd))
}

fn derive_project_id_from_cwd_path(cwd: &Path) -> String {
    canonicalize_project_id_for_client(&natural_project_id_from_cwd(cwd))
}

fn resolve_project_id(project: Option<String>) -> String {
    project
        .as_deref()
        .map(canonicalize_project_id_for_client)
        .unwrap_or_else(derive_project_id)
}

fn fieldcraft_state_path(file_name: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".fieldcraft")
        .join("state")
        .join(file_name)
}

fn comparable_cwd(path: &str) -> String {
    Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path))
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string()
}

fn resolve_session_id(explicit: Option<&str>) -> Option<String> {
    if let Some(id) = explicit.map(str::trim).filter(|id| !id.is_empty()) {
        return Some(id.to_string());
    }
    if let Ok(id) = std::env::var("FIELDCRAFT_SESSION_ID") {
        let trimmed = id.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    let path = fieldcraft_state_path("current-session.json");
    let raw = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let session_id = value.get("session_id")?.as_str()?.trim();
    if session_id.is_empty() {
        return None;
    }

    let recorded_cwd = value.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
    let current_cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    if !recorded_cwd.is_empty() && comparable_cwd(recorded_cwd) != comparable_cwd(&current_cwd) {
        return None;
    }

    Some(session_id.to_string())
}

// ---------------------------------------------------------------------------
// HTTP client mode helpers
// ---------------------------------------------------------------------------

static EVAL_DATA_DIR_OVERRIDE: LazyLock<Mutex<Option<PathBuf>>> =
    LazyLock::new(|| Mutex::new(None));

struct EvalIsolationGuard {
    previous: Option<PathBuf>,
}

impl EvalIsolationGuard {
    fn new(data_dir: PathBuf) -> Self {
        let mut guard = EVAL_DATA_DIR_OVERRIDE
            .lock()
            .expect("eval data dir mutex poisoned");
        let previous = guard.replace(data_dir);
        Self { previous }
    }
}

impl Drop for EvalIsolationGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = EVAL_DATA_DIR_OVERRIDE.lock() {
            *guard = self.previous.take();
        }
    }
}

fn eval_data_dir_override() -> Option<PathBuf> {
    EVAL_DATA_DIR_OVERRIDE
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
}

/// Keychain service name and user key for the auto-enrolled bearer token.
const KEYCHAIN_SERVICE: &str = "fieldcraft";
const KEYCHAIN_USER: &str = "token";

/// Retrieve or auto-enroll a bearer token for the given server URL.
///
/// Priority: FIELDCRAFT_AUTH_TOKEN env var → ~/.fieldcraft/env → macOS Keychain → POST /enroll.
async fn get_or_enroll_token(server_url: &str) -> Result<String> {
    if let Some(token) = fieldcraft_auth_token() {
        return Ok(token);
    }

    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER)
        .context("failed to open keychain entry")?;

    // 2. Try to get existing token from Keychain.
    match entry.get_password() {
        Ok(token) if !token.is_empty() => return Ok(token),
        _ => {}
    }

    // No token found — auto-enroll.
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{server_url}/enroll"))
        .json(&serde_json::json!({"label": "fieldcraft-cli"}))
        .send()
        .await
        .context("POST /enroll failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("POST /enroll returned {status}: {body}");
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .context("failed to parse enroll response")?;
    let token = body["token"]
        .as_str()
        .context("enroll response missing 'token' field")?
        .to_string();

    entry
        .set_password(&token)
        .context("failed to store token in keychain")?;

    eprintln!("[fieldcraft] enrolled new token (stored in keychain)");
    Ok(token)
}

fn fieldcraft_server_url() -> Option<String> {
    if eval_data_dir_override().is_some() {
        return None;
    }
    fieldcraft_config_value("FIELDCRAFT_SERVER_URL")
        .map(|url| url.trim_end_matches('/').to_string())
}

fn local_catalog_allowed() -> bool {
    fieldcraft_server_url()
        .as_deref()
        .is_none_or(is_loopback_fieldcraft_url)
}

fn fieldcraft_db_path_for_command(command: &str) -> Result<PathBuf> {
    if let Some(data_dir) = eval_data_dir_override() {
        return Ok(data_dir.join("memory.sqlite3"));
    }
    if let Some(server_url) = fieldcraft_server_url()
        && !is_loopback_fieldcraft_url(&server_url)
    {
        anyhow::bail!(
            "{command} refuses to use the local catalog because FIELDCRAFT_SERVER_URL is non-local ({server_url}). Use the server-backed path or run this command on the server host."
        );
    }
    Ok(fieldcraft_db_path())
}

fn fieldcraft_db_path_for_command_or_data_dir(
    command: &str,
    data_dir: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(data_dir) = data_dir {
        return Ok(data_dir.join("memory.sqlite3"));
    }
    if let Some(data_dir) = eval_data_dir_override() {
        return Ok(data_dir.join("memory.sqlite3"));
    }
    if let Some(server_url) = fieldcraft_server_url()
        && !is_loopback_fieldcraft_url(&server_url)
    {
        anyhow::bail!(
            "{command} refuses to use a local catalog because FIELDCRAFT_SERVER_URL is non-local ({server_url}). Use the server-backed path or run this command on the server host."
        );
    }
    Ok(fieldcraft_db_path())
}

fn fieldcraft_auth_token() -> Option<String> {
    fieldcraft_config_value("FIELDCRAFT_AUTH_TOKEN")
}

fn fieldcraft_config_value(key: &str) -> Option<String> {
    let file_value = read_env_file_value(&default_fieldcraft_env_file(), key)
        .ok()
        .flatten();
    let env_value = std::env::var(key).ok();
    select_fieldcraft_config_value(key, file_value, env_value)
}

fn select_fieldcraft_config_value(
    key: &str,
    file_value: Option<String>,
    env_value: Option<String>,
) -> Option<String> {
    if key == "FIELDCRAFT_SERVER_URL" {
        nonempty_config_value(file_value).or_else(|| nonempty_config_value(env_value))
    } else {
        nonempty_config_value(env_value).or_else(|| nonempty_config_value(file_value))
    }
}

fn nonempty_config_value(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

// ---------------------------------------------------------------------------
// Command: learn
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct LocalLearningImportSummary {
    imported: usize,
    skipped_existing: usize,
    failed: usize,
}

async fn cmd_learn_queue(
    manifest: &Path,
    default_project: Option<&str>,
    default_scope_hint: Option<&str>,
) -> Result<()> {
    let seeds = read_learning_manifest(manifest, default_project, default_scope_hint)?;
    anyhow::ensure!(!seeds.is_empty(), "learning source manifest is empty");

    if let Some(server_url) = fieldcraft_server_url() {
        let server_url = server_url.trim_end_matches('/').to_string();
        let token = get_or_enroll_token(&server_url).await?;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{server_url}/learning/sources"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&seeds)
            .send()
            .await
            .context("POST /learning/sources failed")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("POST /learning/sources returned {status}: {body}");
        }
        println!("{body}");
        return Ok(());
    }

    let data_dir = default_fieldcraft_data_dir();
    let conn = fieldcraft_storage::open_source_sessions_catalog(&data_dir)
        .context("failed to open source-learning catalog")?;
    let mut imported = 0usize;
    let mut skipped_existing = 0usize;
    let mut failed = 0usize;
    for seed in &seeds {
        match fieldcraft_storage::enqueue_learning_source(&conn, seed) {
            Ok((_, true)) => imported += 1,
            Ok((_, false)) => skipped_existing += 1,
            Err(e) => {
                failed += 1;
                eprintln!("failed to queue {}: {e}", seed.uri);
            }
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&LocalLearningImportSummary {
            imported,
            skipped_existing,
            failed,
        })?
    );
    Ok(())
}

async fn cmd_learn_claim(
    limit: usize,
    project: Option<&str>,
    worker_id: Option<&str>,
) -> Result<()> {
    let worker_id = worker_id.unwrap_or("fieldcraft-worker");
    if let Some(server_url) = fieldcraft_server_url() {
        let server_url = server_url.trim_end_matches('/').to_string();
        let token = get_or_enroll_token(&server_url).await?;
        let client = reqwest::Client::new();
        let mut req = client
            .get(format!("{server_url}/learning/sources/claim"))
            .header("Authorization", format!("Bearer {token}"))
            .query(&[
                ("limit", limit.to_string()),
                ("worker_id", worker_id.to_string()),
            ]);
        if let Some(project) = project {
            req = req.query(&[("project", project)]);
        }
        let resp = req
            .send()
            .await
            .context("GET /learning/sources/claim failed")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("GET /learning/sources/claim returned {status}: {body}");
        }
        println!("{body}");
        return Ok(());
    }

    let data_dir = default_fieldcraft_data_dir();
    let conn = fieldcraft_storage::open_source_sessions_catalog(&data_dir)
        .context("failed to open source-learning catalog")?;
    let records = fieldcraft_storage::claim_learning_sources(&conn, worker_id, limit, project)
        .context("failed to claim learning sources")?;
    println!("{}", serde_json::to_string_pretty(&records)?);
    Ok(())
}

async fn cmd_learn_run(
    limit: usize,
    project: Option<&str>,
    worker_id: Option<&str>,
    max_records_per_source: usize,
) -> Result<()> {
    anyhow::ensure!(
        max_records_per_source > 0,
        "max_records_per_source must be greater than zero"
    );
    let worker_id = worker_id.unwrap_or("fieldcraft-worker");
    let claimed = claim_learning_sources_for_worker(limit, project, worker_id).await?;
    if claimed.is_empty() {
        println!("No pending learning sources.");
        return Ok(());
    }

    let mut succeeded = 0usize;
    let mut failed = 0usize;
    for source in claimed {
        match process_learning_source(&source, max_records_per_source).await {
            Ok((source_hash, count)) => {
                complete_learning_source_for_worker(
                    &source.source_id,
                    Some(&source_hash),
                    count as u32,
                )
                .await?;
                println!("learned {} -> {} record(s)", source.source_id, count);
                succeeded += 1;
            }
            Err(e) => {
                fail_learning_source_for_worker(&source.source_id, &e.to_string()).await?;
                eprintln!("failed {}: {e}", source.source_id);
                failed += 1;
            }
        }
    }
    println!("learn run complete: {succeeded} succeeded, {failed} failed");
    Ok(())
}

async fn claim_learning_sources_for_worker(
    limit: usize,
    project: Option<&str>,
    worker_id: &str,
) -> Result<Vec<LearningSourceRecord>> {
    if let Some(server_url) = fieldcraft_server_url() {
        let server_url = server_url.trim_end_matches('/').to_string();
        let token = get_or_enroll_token(&server_url).await?;
        let client = reqwest::Client::new();
        let mut req = client
            .get(format!("{server_url}/learning/sources/claim"))
            .header("Authorization", format!("Bearer {token}"))
            .query(&[
                ("limit", limit.to_string()),
                ("worker_id", worker_id.to_string()),
            ]);
        if let Some(project) = project {
            req = req.query(&[("project", project)]);
        }
        let resp = req
            .send()
            .await
            .context("GET /learning/sources/claim failed")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("GET /learning/sources/claim returned {status}: {body}");
        }
        return serde_json::from_str(&body).context("parse learning source claim response");
    }

    let data_dir = default_fieldcraft_data_dir();
    let conn = fieldcraft_storage::open_source_sessions_catalog(&data_dir)
        .context("failed to open source-learning catalog")?;
    fieldcraft_storage::claim_learning_sources(&conn, worker_id, limit, project)
        .context("failed to claim learning sources")
}

async fn process_learning_source(
    source: &LearningSourceRecord,
    max_records_per_source: usize,
) -> Result<(String, usize)> {
    let normalized = load_learning_source_text(source).await?;
    let source_hash = fieldcraft_storage::compute_checksum(&normalized);
    let sections = learning_sections(&normalized, &source.title);
    let repo = if fieldcraft_server_url().is_none() {
        Some(
            KnowledgeRepository::open(&fieldcraft_db_path_for_command("learn")?)
                .context("open knowledge repo")?,
        )
    } else {
        None
    };

    let mut written = 0usize;
    for (idx, section) in sections.into_iter().enumerate() {
        if written >= max_records_per_source {
            break;
        }
        let salient_content = salient_learning_content(&section.content);
        let learning_content = salient_content.trim();
        if learning_content.is_empty() {
            continue;
        }
        if learning_content
            .chars()
            .filter(|c| !c.is_whitespace())
            .count()
            < 180
        {
            continue;
        }
        let record = learning_section_record(source, &source_hash, idx, &section)?;
        write_learning_record(record, repo.as_ref()).await?;
        written += 1;
    }
    Ok((source_hash, written))
}

async fn load_learning_source_text(source: &LearningSourceRecord) -> Result<String> {
    if source.uri.starts_with("http://") || source.uri.starts_with("https://") {
        let body = reqwest::get(&source.uri)
            .await
            .with_context(|| format!("fetch {}", source.uri))?
            .error_for_status()
            .with_context(|| format!("fetch {}", source.uri))?
            .text()
            .await
            .with_context(|| format!("read response body {}", source.uri))?;
        return Ok(normalize_learning_text(&body));
    }

    let path = Path::new(&source.uri);
    if path.is_dir() {
        let mut combined = String::new();
        let mut total_bytes = 0u64;
        for entry in walkdir::WalkDir::new(path)
            .max_depth(source.traversal_policy.max_depth.max(1) as usize)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_file())
        {
            let ext = entry
                .path()
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("")
                .to_lowercase();
            if !matches!(ext.as_str(), "md" | "mdx" | "txt" | "html" | "htm") {
                continue;
            }
            let bytes = fs::read(entry.path())
                .with_context(|| format!("read {}", entry.path().display()))?;
            total_bytes += bytes.len() as u64;
            if total_bytes > source.traversal_policy.max_bytes {
                break;
            }
            let text = String::from_utf8_lossy(&bytes);
            combined.push_str(&format!("\n# {}\n\n", entry.path().display()));
            combined.push_str(&normalize_learning_text(&text));
            combined.push('\n');
        }
        anyhow::ensure!(
            !combined.trim().is_empty(),
            "directory contained no readable learning text"
        );
        return Ok(combined);
    }

    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() as u64 <= source.traversal_policy.max_bytes,
        "source exceeds max_bytes policy"
    );
    Ok(normalize_learning_text(&String::from_utf8_lossy(&bytes)))
}

#[derive(Debug, Clone)]
struct LearningSection {
    heading: String,
    content: String,
}

fn learning_sections(text: &str, fallback_title: &str) -> Vec<LearningSection> {
    let mut sections = Vec::new();
    let mut heading = fallback_title.to_string();
    let mut lines = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let md_heading = trimmed
            .strip_prefix("## ")
            .or_else(|| trimmed.strip_prefix("# "));
        if let Some(next_heading) = md_heading {
            if !lines.is_empty() {
                sections.push(LearningSection {
                    heading: heading.clone(),
                    content: lines.join("\n").trim().to_string(),
                });
                lines.clear();
            }
            heading = next_heading.trim().to_string();
        } else {
            lines.push(line.to_string());
        }
    }
    if !lines.is_empty() {
        sections.push(LearningSection {
            heading,
            content: lines.join("\n").trim().to_string(),
        });
    }
    if sections.is_empty() && !text.trim().is_empty() {
        sections.push(LearningSection {
            heading: fallback_title.to_string(),
            content: text.trim().to_string(),
        });
    }
    sections
}

fn learning_section_record(
    source: &LearningSourceRecord,
    source_hash: &str,
    chunk_index: usize,
    section: &LearningSection,
) -> Result<KnowledgeRecord> {
    let salient_content = salient_learning_content(&section.content);
    let record_content = if salient_content.trim().is_empty() {
        section.content.clone()
    } else {
        salient_content
    };
    let is_procedure = looks_like_procedure(&record_content);
    let title = format!(
        "{}: {}",
        truncate(&source.title, 42),
        truncate(&section.heading, 72)
    );
    let retrieval_questions = learning_retrieval_questions(source, section);
    let (kind, payload_json) = if is_procedure {
        let payload = ProcedurePayload {
            goal: section.heading.clone(),
            preconditions: vec![],
            steps: section_steps(&record_content),
            failure_modes: section_failure_modes(&record_content),
            affected_files: vec![],
            retrieval_questions: retrieval_questions.clone(),
            trigger: Some(section_trigger(source, section)),
            invalid_if: Some(format!("source hash changes from {source_hash}")),
        };
        (KnowledgeKind::Procedure, serde_json::to_string(&payload)?)
    } else {
        let payload = ContextPayload {
            statement: first_content_sentences(&record_content, 2),
            scope: source
                .scope_hint
                .clone()
                .unwrap_or_else(|| source.uri.clone()),
            files: vec![],
            implications: section_implications(&record_content),
            invalid_if: Some(format!("source hash changes from {source_hash}")),
            retrieval_queries: retrieval_questions.clone(),
        };
        (KnowledgeKind::Context, serde_json::to_string(&payload)?)
    };
    let retrieval_text = format!(
        "title: {}\nsection: {}\n{}\n\nsource: {}\nscope_hint: {}\nsource_context:\n{}",
        source.title,
        section.heading,
        record_content,
        source.uri,
        source.scope_hint.clone().unwrap_or_default(),
        section.content
    );
    let mut hasher = Sha256::new();
    hasher.update(b"learning-source-record|");
    hasher.update(source.source_id.as_bytes());
    hasher.update(b"|");
    hasher.update(chunk_index.to_string().as_bytes());
    hasher.update(b"|");
    hasher.update(source_hash.as_bytes());
    let id = format!("{:x}", hasher.finalize());
    Ok(KnowledgeRecord {
        protected: false,
        id,
        project_id: source.project.clone(),
        kind,
        title,
        payload_json,
        state: KnowledgeState::Active,
        confidence: 0.55,
        source: KnowledgeSource::External {
            uri: source.uri.clone(),
            title: source.title.clone(),
            source_hash: Some(source_hash.to_string()),
        },
        superseded_by: None,
        project_specific: source.project != "global",
        created_at: Utc::now(),
        valid_until: None,
        retrieval_text,
        schema_version: 4,
        phase: None,
        tier: KnowledgeTier::Active,
        owner: None,
        session_date: None,
        source_refs: vec![SourceReference {
            source_id: source.source_id.clone(),
            chunk_index: Some(chunk_index),
        }],
    })
}

async fn write_learning_record(
    mut record: KnowledgeRecord,
    local_repo: Option<&KnowledgeRepository>,
) -> Result<()> {
    if let Some(server_url) = fieldcraft_server_url() {
        let server_url = server_url.trim_end_matches('/').to_string();
        let token = get_or_enroll_token(&server_url).await?;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{server_url}/lessons"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&record)
            .send()
            .await
            .context("POST /lessons failed")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("POST /lessons returned {status}: {body}");
        }
        return Ok(());
    }

    let repo = local_repo.context("local knowledge repo missing")?;
    record.owner = Some("learn-worker".to_string());
    repo.insert_knowledge(&record)?;
    index_knowledge_sqlite(repo, &record)?;
    Ok(())
}

async fn complete_learning_source_for_worker(
    source_id: &str,
    source_hash: Option<&str>,
    derived_record_count: u32,
) -> Result<()> {
    if fieldcraft_server_url().is_some() {
        cmd_learn_complete(source_id, source_hash, derived_record_count).await
    } else {
        let data_dir = default_fieldcraft_data_dir();
        let conn = fieldcraft_storage::open_source_sessions_catalog(&data_dir)?;
        fieldcraft_storage::complete_learning_source(
            &conn,
            source_id,
            source_hash,
            derived_record_count,
        )?;
        Ok(())
    }
}

async fn fail_learning_source_for_worker(source_id: &str, error: &str) -> Result<()> {
    if fieldcraft_server_url().is_some() {
        cmd_learn_fail(source_id, error).await
    } else {
        let data_dir = default_fieldcraft_data_dir();
        let conn = fieldcraft_storage::open_source_sessions_catalog(&data_dir)?;
        fieldcraft_storage::fail_learning_source(&conn, source_id, error)?;
        Ok(())
    }
}

fn normalize_learning_text(text: &str) -> String {
    let mut out = text.to_string();
    for pattern in [
        r"(?is)<script[^>]*>.*?</script>",
        r"(?is)<style[^>]*>.*?</style>",
        r"(?is)<nav[^>]*>.*?</nav>",
        r"(?is)<footer[^>]*>.*?</footer>",
    ] {
        let re = regex::Regex::new(pattern).expect("static regex compiles");
        out = re.replace_all(&out, "\n").to_string();
    }
    let heading_re = regex::Regex::new(r"(?i)</?h([12])[^>]*>").expect("static regex compiles");
    out = heading_re.replace_all(&out, "\n## ").to_string();
    let tag_re = regex::Regex::new(r"(?is)<[^>]+>").expect("static regex compiles");
    out = tag_re.replace_all(&out, " ").to_string();
    decode_basic_html_entities(&out)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn decode_basic_html_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn salient_learning_content(content: &str) -> String {
    let mut lines = Vec::new();
    let mut previous_blank = false;
    for raw_line in content.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            if !lines.is_empty() && !previous_blank {
                lines.push(String::new());
                previous_blank = true;
            }
            continue;
        }

        let lower = trimmed.to_lowercase();
        let is_browser_provenance = lower.starts_with("source:")
            || lower.starts_with("source url:")
            || lower.starts_with("source site:")
            || lower.starts_with("source frame:")
            || lower.starts_with("browser action:")
            || lower.starts_with("action source:")
            || lower.starts_with("confidence:")
            || lower.starts_with("captured:")
            || lower.starts_with("captured at:")
            || lower.starts_with("target:")
            || lower.starts_with("author:")
            || lower.starts_with("image:")
            || lower.starts_with("image url:")
            || lower.starts_with("image evidence:")
            || lower.starts_with("context: this was saved from");
        if is_browser_provenance {
            continue;
        }

        lines.push(trimmed.to_string());
        previous_blank = false;
    }

    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn looks_like_procedure(content: &str) -> bool {
    let lower = content.to_lowercase();
    let numbered = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.chars().next().is_some_and(|c| c.is_ascii_digit())
                && (trimmed.contains(". ") || trimmed.contains(") "))
        })
        .count();
    numbered >= 3
        || (lower.contains("first ") && lower.contains("then ") && lower.contains("finally "))
        || lower.contains("step ")
        || lower.contains("verify ")
}

fn section_steps(content: &str) -> Vec<String> {
    let steps: Vec<String> = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            line.chars().next().is_some_and(|c| c.is_ascii_digit())
                || line.starts_with("- ")
                || line.starts_with("* ")
        })
        .map(|line| {
            line.trim_start_matches(|c: char| {
                c.is_ascii_digit() || matches!(c, '.' | ')' | '-' | '*' | ' ')
            })
            .trim()
            .to_string()
        })
        .filter(|line| !line.is_empty())
        .take(8)
        .collect();
    if steps.is_empty() {
        vec![first_content_sentences(content, 3)]
    } else {
        steps
    }
}

fn section_failure_modes(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| {
            let lower = line.to_lowercase();
            lower.contains("avoid")
                || lower.contains("fail")
                || lower.contains("risk")
                || lower.contains("pitfall")
        })
        .map(|line| line.chars().take(220).collect())
        .take(5)
        .collect()
}

fn section_implications(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| {
            let lower = line.to_lowercase();
            lower.contains("because")
                || lower.contains("therefore")
                || lower.contains("means that")
                || lower.contains("important")
        })
        .map(|line| line.chars().take(220).collect())
        .take(5)
        .collect()
}

fn learning_retrieval_questions(
    source: &LearningSourceRecord,
    section: &LearningSection,
) -> Vec<String> {
    let mut questions = vec![
        format!("When should I use {}", section.heading),
        format!("How does {} apply?", section.heading),
    ];
    if let Some(scope) = &source.scope_hint {
        questions.push(format!("{scope}: {}", section.heading));
    }
    questions
}

fn section_trigger(source: &LearningSourceRecord, section: &LearningSection) -> String {
    match &source.scope_hint {
        Some(scope) => format!("When working on {scope} and needing {}", section.heading),
        None => format!("When needing guidance from {}", section.heading),
    }
}

fn first_content_sentences(content: &str, max_sentences: usize) -> String {
    let mut out = String::new();
    let mut sentences = 0usize;
    for part in content.split_inclusive(['.', '!', '?']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(part);
        sentences += 1;
        if sentences >= max_sentences || out.len() >= 600 {
            break;
        }
    }
    if out.is_empty() {
        content.chars().take(600).collect()
    } else {
        out
    }
}

async fn cmd_learn_complete(
    source_id: &str,
    source_hash: Option<&str>,
    derived_record_count: u32,
) -> Result<()> {
    if let Some(server_url) = fieldcraft_server_url() {
        let server_url = server_url.trim_end_matches('/').to_string();
        let token = get_or_enroll_token(&server_url).await?;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!(
                "{server_url}/learning/sources/{source_id}/complete"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({
                "source_hash": source_hash,
                "derived_record_count": derived_record_count,
            }))
            .send()
            .await
            .context("POST /learning/sources/{id}/complete failed")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("POST /learning/sources/{source_id}/complete returned {status}: {body}");
        }
        println!("{body}");
        return Ok(());
    }

    let data_dir = default_fieldcraft_data_dir();
    let conn = fieldcraft_storage::open_source_sessions_catalog(&data_dir)
        .context("failed to open source-learning catalog")?;
    fieldcraft_storage::complete_learning_source(
        &conn,
        source_id,
        source_hash,
        derived_record_count,
    )
    .context("failed to complete learning source")?;
    println!(
        "{}",
        serde_json::json!({ "source_id": source_id, "status": "succeeded" })
    );
    Ok(())
}

async fn cmd_learn_fail(source_id: &str, error: &str) -> Result<()> {
    anyhow::ensure!(!error.trim().is_empty(), "error must not be empty");
    if let Some(server_url) = fieldcraft_server_url() {
        let server_url = server_url.trim_end_matches('/').to_string();
        let token = get_or_enroll_token(&server_url).await?;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{server_url}/learning/sources/{source_id}/fail"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({ "error": error }))
            .send()
            .await
            .context("POST /learning/sources/{id}/fail failed")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("POST /learning/sources/{source_id}/fail returned {status}: {body}");
        }
        println!("{body}");
        return Ok(());
    }

    let data_dir = default_fieldcraft_data_dir();
    let conn = fieldcraft_storage::open_source_sessions_catalog(&data_dir)
        .context("failed to open source-learning catalog")?;
    fieldcraft_storage::fail_learning_source(&conn, source_id, error)
        .context("failed to fail learning source")?;
    println!(
        "{}",
        serde_json::json!({ "source_id": source_id, "status": "failed" })
    );
    Ok(())
}

fn read_learning_manifest(
    manifest: &Path,
    default_project: Option<&str>,
    default_scope_hint: Option<&str>,
) -> Result<Vec<LearningSourceSeed>> {
    let text = fs::read_to_string(manifest)
        .with_context(|| format!("read learning manifest {}", manifest.display()))?;
    let trimmed = text.trim();
    if trimmed.starts_with('[') {
        return serde_json::from_str(trimmed)
            .with_context(|| format!("parse learning manifest {}", manifest.display()));
    }

    let mut seeds = Vec::new();
    for (line_no, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut seed = if line.starts_with('{') {
            serde_json::from_str::<LearningSourceSeed>(line).with_context(|| {
                format!(
                    "parse learning manifest {} line {}",
                    manifest.display(),
                    line_no + 1
                )
            })?
        } else {
            LearningSourceSeed {
                uri: line.to_string(),
                kind: None,
                project: default_project.map(ToString::to_string),
                title: None,
                scope_hint: default_scope_hint.map(ToString::to_string),
                traversal_policy: LearningTraversalPolicy::default(),
            }
        };
        if seed.project.is_none() {
            seed.project = default_project.map(ToString::to_string);
        }
        if seed.scope_hint.is_none() {
            seed.scope_hint = default_scope_hint.map(ToString::to_string);
        }
        seeds.push(seed);
    }
    Ok(seeds)
}

fn default_fieldcraft_data_dir() -> PathBuf {
    if let Some(dir) = eval_data_dir_override() {
        return dir;
    }
    if let Ok(dir) = std::env::var("FIELDCRAFT_HOME") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".fieldcraft")
}

// ---------------------------------------------------------------------------
// Command: eval skills
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct SkillEvalCorpusRecord {
    skill_name: String,
    description: String,
    body: String,
    #[serde(default)]
    triggers: Vec<String>,
    #[serde(default)]
    source_uri: Option<String>,
    #[serde(default)]
    category: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct SkillEvalQueryRecord {
    id: String,
    query: String,
    #[serde(default)]
    expected: Vec<String>,
    #[serde(default)]
    acceptable: Vec<String>,
    #[serde(default)]
    should_abstain: bool,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    label_source: Option<String>,
    #[serde(default)]
    review_status: Option<String>,
}

#[derive(Debug)]
struct SkillEvalResultRow {
    id: String,
    query: String,
    expected: Vec<String>,
    returned: Vec<String>,
    hit_at_1: bool,
    hit_at_3: bool,
    abstain_correct: Option<bool>,
    label_source: String,
}

#[derive(Debug)]
struct ExternalSkillEvalSummary {
    mode: AtomizeMode,
    corpus_records: usize,
    knowledge_records: usize,
    blocked_packages: usize,
    rows: Vec<SkillEvalResultRow>,
    average_returned: f32,
}

async fn cmd_eval_external_skills(
    corpus_path: &Path,
    queries_path: &Path,
    report_path: Option<&Path>,
    data_dir: Option<&Path>,
    limit: usize,
) -> Result<()> {
    let temp_data_dir;
    let eval_data_dir = match data_dir {
        Some(path) => {
            fs::create_dir_all(path)
                .with_context(|| format!("create eval data dir {}", path.display()))?;
            path.to_path_buf()
        }
        None => {
            temp_data_dir = tempfile::tempdir().context("create isolated eval data dir")?;
            temp_data_dir.path().to_path_buf()
        }
    };
    let _eval_isolation = EvalIsolationGuard::new(eval_data_dir.clone());

    let corpus: Vec<SkillEvalCorpusRecord> = read_jsonl(corpus_path)
        .with_context(|| format!("failed to read corpus {}", corpus_path.display()))?;
    let queries: Vec<SkillEvalQueryRecord> = read_jsonl(queries_path)
        .with_context(|| format!("failed to read queries {}", queries_path.display()))?;
    anyhow::ensure!(!corpus.is_empty(), "external skill eval corpus is empty");
    anyhow::ensure!(
        !queries.is_empty(),
        "external skill eval query set is empty"
    );

    let whole =
        run_external_skill_eval(&corpus, &queries, AtomizeMode::Whole, &eval_data_dir, limit)
            .await?;
    let split =
        run_external_skill_eval(&corpus, &queries, AtomizeMode::Split, &eval_data_dir, limit)
            .await?;
    let report = render_external_skill_eval_report(&whole, &split);
    if let Some(path) = report_path {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create report directory {}", parent.display()))?;
        }
        fs::write(path, &report).with_context(|| format!("write report {}", path.display()))?;
        println!("Wrote {}", path.display());
    } else {
        print!("{report}");
    }
    Ok(())
}

async fn run_external_skill_eval(
    corpus: &[SkillEvalCorpusRecord],
    queries: &[SkillEvalQueryRecord],
    mode: AtomizeMode,
    data_dir: &Path,
    limit: usize,
) -> Result<ExternalSkillEvalSummary> {
    fs::create_dir_all(data_dir)
        .with_context(|| format!("create external skill eval data dir {}", data_dir.display()))?;
    let db_dir = tempfile::tempdir_in(data_dir).context("create external skill eval db tempdir")?;
    let skill_root = tempfile::tempdir().context("create external skill eval skill tempdir")?;
    let db_path = db_dir.path().join("memory.sqlite3");
    let skill_repo =
        fieldcraft_storage::SkillRepository::open(&db_path).context("open eval skill repo")?;
    let knowledge_repo = KnowledgeRepository::open(&db_path).context("open eval knowledge repo")?;

    let mut blocked_packages = 0usize;
    let mut knowledge_records = 0usize;
    for record in corpus {
        let skill_dir = write_eval_skill_fixture(skill_root.path(), record)?;
        let report = skill_repo
            .ingest_path(&skill_dir)
            .with_context(|| format!("ingest skill fixture {}", record.skill_name))?;
        let Some(card) = &report.card else {
            blocked_packages += 1;
            continue;
        };
        let specs = external_skill_record_specs(&report.package, card, &skill_dir, mode)?;
        for spec in &specs {
            let knowledge = external_skill_record(&report.package, spec)?;
            knowledge_repo.insert_knowledge(&knowledge)?;
            index_knowledge_sqlite(&knowledge_repo, &knowledge)?;
            knowledge_records += 1;
        }
    }

    let mut rows = Vec::new();
    let mut returned_total = 0usize;
    for query in queries {
        let response = retrieve(
            RetrievalRequest {
                project_id: "eval".to_string(),
                context: query.query.clone(),
                files: vec![],
                task: Some(query.query.clone()),
                preferred_types: vec![],
                tier: KnowledgeTier::Archive,
                source_filter: SourceFilter::ExternalOnly,
            },
            &db_path,
        )
        .await
        .with_context(|| format!("retrieve external skill query {}", query.id))?;
        let returned_lessons: Vec<_> = response
            .lessons
            .into_iter()
            .take(limit.clamp(1, 3))
            .collect();
        returned_total += returned_lessons.len();
        let mut accepted = query.expected.clone();
        accepted.extend(query.acceptable.clone());
        accepted.sort();
        accepted.dedup();
        let hit_at_1 = returned_lessons
            .first()
            .map(|lesson| {
                accepted
                    .iter()
                    .any(|expected| external_record_matches(expected, &lesson.id, &lesson.title))
            })
            .unwrap_or(false);
        let hit_at_3 = returned_lessons.iter().take(3).any(|lesson| {
            accepted
                .iter()
                .any(|expected| external_record_matches(expected, &lesson.id, &lesson.title))
        });
        let abstain_correct = if query.should_abstain {
            Some(returned_lessons.is_empty())
        } else {
            None
        };
        let returned = returned_lessons
            .into_iter()
            .map(|lesson| format!("{} [{}] {}", lesson.id, lesson.source_origin, lesson.title))
            .collect();
        rows.push(SkillEvalResultRow {
            id: query.id.clone(),
            query: query.query.clone(),
            expected: accepted,
            returned,
            hit_at_1,
            hit_at_3,
            abstain_correct,
            label_source: query
                .label_source
                .clone()
                .or_else(|| query.source.clone())
                .unwrap_or_else(|| "unknown".to_string()),
        });
    }
    Ok(ExternalSkillEvalSummary {
        mode,
        corpus_records: corpus.len(),
        knowledge_records,
        blocked_packages,
        average_returned: ratio(returned_total, queries.len()),
        rows,
    })
}

fn write_eval_skill_fixture(root: &Path, record: &SkillEvalCorpusRecord) -> Result<PathBuf> {
    let skill_dir = root.join(sanitize_path_segment(&record.skill_name));
    fs::create_dir_all(&skill_dir)
        .with_context(|| format!("create skill fixture {}", skill_dir.display()))?;
    let trigger_section = if record.triggers.is_empty() {
        String::new()
    } else {
        format!(
            "\n## When to use\n{}\n",
            record
                .triggers
                .iter()
                .map(|trigger| format!("- {trigger}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let body = format!(
        "---\nname: {}\ndescription: {}\n---\n# {}\n{}\n{}\n",
        record.skill_name, record.description, record.skill_name, trigger_section, record.body
    );
    fs::write(skill_dir.join("SKILL.md"), body)
        .with_context(|| format!("write skill fixture {}", record.skill_name))?;
    Ok(skill_dir)
}

fn external_record_matches(expected: &str, record_id: &str, title: &str) -> bool {
    let expected = normalize_external_eval_label(expected);
    expected == normalize_external_eval_label(record_id)
        || expected == normalize_external_eval_label(title)
        || expected == normalize_external_eval_label(title.trim_start_matches("Use "))
}

fn normalize_external_eval_label(value: &str) -> String {
    value.trim().to_lowercase()
}

fn render_external_skill_eval_report(
    whole: &ExternalSkillEvalSummary,
    split: &ExternalSkillEvalSummary,
) -> String {
    let mut out = String::new();
    out.push_str("# External Skill Whole-vs-Split Eval\n\n");
    out.push_str("| mode | corpus | knowledge records | blocked | top-1 | top-3 | abstain accuracy | avg returned |\n");
    out.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for summary in [whole, split] {
        let positives: Vec<&SkillEvalResultRow> = summary
            .rows
            .iter()
            .filter(|row| !row.expected.is_empty())
            .collect();
        let abstains: Vec<&SkillEvalResultRow> = summary
            .rows
            .iter()
            .filter(|row| row.abstain_correct.is_some())
            .collect();
        let top1 = positives.iter().filter(|row| row.hit_at_1).count();
        let top3 = positives.iter().filter(|row| row.hit_at_3).count();
        let abstain_ok = abstains
            .iter()
            .filter(|row| row.abstain_correct == Some(true))
            .count();
        out.push_str(&format!(
            "| {:?} | {} | {} | {} | {:.3} ({}/{}) | {:.3} ({}/{}) | {:.3} ({}/{}) | {:.2} |\n",
            summary.mode,
            summary.corpus_records,
            summary.knowledge_records,
            summary.blocked_packages,
            ratio(top1, positives.len()),
            top1,
            positives.len(),
            ratio(top3, positives.len()),
            top3,
            positives.len(),
            ratio(abstain_ok, abstains.len()),
            abstain_ok,
            abstains.len(),
            summary.average_returned,
        ));
    }
    out.push_str("\nRecord labels are matched against the exact derived lesson id or title, with only the display prefix `Use ` ignored.\n");
    out.push_str("Interpretation: split wins only if individual split-derived lessons improve top-k relevance or lower average returned context without reducing abstain accuracy.\n");
    for summary in [whole, split] {
        let failures: Vec<&SkillEvalResultRow> = summary
            .rows
            .iter()
            .filter(|row| {
                (!row.expected.is_empty() && (!row.hit_at_1 || !row.hit_at_3))
                    || row.abstain_correct == Some(false)
            })
            .take(20)
            .collect();
        if failures.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "\n## {:?} Record-Level Failures\n\n",
            summary.mode
        ));
        for failure in failures {
            out.push_str(&format!(
                "- `{}` [{}] expected {:?}, returned {:?}\n  query: {}\n",
                failure.id, failure.label_source, failure.expected, failure.returned, failure.query
            ));
        }
    }
    out
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    let file = fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut out = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        out.push(
            serde_json::from_str(trimmed)
                .with_context(|| format!("parse JSONL line {} in {}", idx + 1, path.display()))?,
        );
    }
    Ok(out)
}

fn ratio(numerator: usize, denominator: usize) -> f32 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f32 / denominator as f32
    }
}

fn sanitize_path_segment(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "skill".to_string()
    } else {
        trimmed.to_string()
    }
}

#[derive(Debug, Clone)]
struct ExternalSkillRecordSpec {
    atomization_status: String,
    atomization_key: String,
    title: String,
    description: String,
    activation_cues: Vec<String>,
    negative_cues: Vec<String>,
    procedure: String,
    required_capabilities: Vec<String>,
    verification_steps: Vec<String>,
    retrieval_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillSection {
    heading: String,
    content: String,
}

fn external_skill_record_specs(
    package: &fieldcraft_storage::SkillPackageRecord,
    card: &fieldcraft_storage::SkillCardRecord,
    source: &Path,
    atomize: AtomizeMode,
) -> Result<Vec<ExternalSkillRecordSpec>> {
    if atomize == AtomizeMode::Whole {
        return Ok(vec![whole_skill_spec(card, "kept_whole")]);
    }
    let split_specs = split_skill_specs(package, card, source)?;
    match atomize {
        AtomizeMode::Split if split_specs.is_empty() => {
            Ok(vec![whole_skill_spec(card, "kept_whole")])
        }
        AtomizeMode::Split => Ok(split_specs),
        AtomizeMode::Whole => Ok(vec![whole_skill_spec(card, "kept_whole")]),
    }
}

fn external_skill_record(
    package: &fieldcraft_storage::SkillPackageRecord,
    spec: &ExternalSkillRecordSpec,
) -> Result<KnowledgeRecord> {
    let mut steps = skill_procedure_steps(&spec.procedure, &spec.description);
    steps.extend(spec.verification_steps.iter().cloned());
    let payload = ProcedurePayload {
        goal: spec.description.clone(),
        preconditions: spec.required_capabilities.clone(),
        steps,
        failure_modes: spec.negative_cues.clone(),
        affected_files: vec![],
        retrieval_questions: spec.activation_cues.clone(),
        trigger: spec.activation_cues.first().cloned(),
        invalid_if: spec.negative_cues.first().cloned(),
    };
    let payload_json =
        serde_json::to_string(&payload).context("failed to serialize skill procedure payload")?;
    Ok(KnowledgeRecord {
        protected: false,
        id: external_skill_record_id(package, spec),
        project_id: "global".to_string(),
        kind: KnowledgeKind::Procedure,
        title: spec.title.clone(),
        payload_json,
        state: KnowledgeState::Active,
        confidence: 0.65,
        source: KnowledgeSource::External {
            uri: package.source_uri.clone(),
            title: package.name.clone(),
            source_hash: Some(package.package_hash.clone()),
        },
        superseded_by: None,
        project_specific: false,
        created_at: Utc::now(),
        valid_until: None,
        retrieval_text: spec.retrieval_text.clone(),
        schema_version: 4,
        phase: None,
        tier: KnowledgeTier::Archive,
        owner: None,
        session_date: None,
        source_refs: vec![],
    })
}

fn whole_skill_spec(
    card: &fieldcraft_storage::SkillCardRecord,
    atomization_status: &str,
) -> ExternalSkillRecordSpec {
    let mut negative_cues = card.negative_cues.clone();
    negative_cues.extend(card.constraints.clone());
    ExternalSkillRecordSpec {
        atomization_status: atomization_status.to_string(),
        atomization_key: "whole".to_string(),
        title: card.title.clone(),
        description: card.description.clone(),
        activation_cues: card.activation_cues.clone(),
        negative_cues,
        procedure: card.procedure.clone(),
        required_capabilities: card.required_capabilities.clone(),
        verification_steps: card.verification_steps.clone(),
        retrieval_text: format!(
            "atomization_status: {atomization_status}\nskill: {}\n{}",
            card.skill_name, card.retrieval_text
        ),
    }
}

fn split_skill_specs(
    package: &fieldcraft_storage::SkillPackageRecord,
    card: &fieldcraft_storage::SkillCardRecord,
    source: &Path,
) -> Result<Vec<ExternalSkillRecordSpec>> {
    let skill_md = source.join("SKILL.md");
    let text = fs::read_to_string(&skill_md)
        .with_context(|| format!("read skill source {}", skill_md.display()))?;
    let body = skill_body_without_frontmatter(&text);
    let sections = split_skill_sections(&body);
    let mut specs = Vec::new();
    for section in sections {
        if !section_is_atomizable(&section) {
            continue;
        }
        let mut cues = vec![
            package.name.clone(),
            card.description.clone(),
            section.heading.clone(),
        ];
        cues.extend(section_cues(&section.content));
        cues.sort();
        cues.dedup();
        let mut verify = section_verification_steps(&section.content);
        if verify.is_empty() {
            verify = card.verification_steps.clone();
        }
        let key = sanitize_path_segment(&section.heading);
        let retrieval_text = format!(
            "atomization_status: split\nskill: {}\nsection: {}\n{}\n{}\n{}",
            package.name,
            section.heading,
            card.description,
            cues.join("\n"),
            section.content
        );
        specs.push(ExternalSkillRecordSpec {
            atomization_status: "split".to_string(),
            atomization_key: key,
            title: format!("Use {}: {}", package.name, section.heading),
            description: format!("{} ({})", card.description, section.heading),
            activation_cues: cues.into_iter().take(10).collect(),
            negative_cues: card.negative_cues.clone(),
            procedure: section.content.chars().take(1400).collect(),
            required_capabilities: card.required_capabilities.clone(),
            verification_steps: verify.into_iter().take(6).collect(),
            retrieval_text,
        });
    }
    Ok(specs)
}

fn skill_body_without_frontmatter(text: &str) -> String {
    if let Some(rest) = text.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---")
    {
        return rest[end + 4..].trim_start_matches('\n').to_string();
    }
    text.to_string()
}

fn split_skill_sections(body: &str) -> Vec<SkillSection> {
    let mut sections = Vec::new();
    let mut current_heading: Option<String> = None;
    let mut current_lines: Vec<String> = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix("## ")
            && !heading.starts_with('#')
        {
            if let Some(prev) = current_heading.take() {
                let content = current_lines.join("\n").trim().to_string();
                if !content.is_empty() {
                    sections.push(SkillSection {
                        heading: prev,
                        content,
                    });
                }
            }
            current_heading = Some(heading.trim().to_string());
            current_lines.clear();
        } else if current_heading.is_some() {
            current_lines.push(line.to_string());
        }
    }
    if let Some(prev) = current_heading {
        let content = current_lines.join("\n").trim().to_string();
        if !content.is_empty() {
            sections.push(SkillSection {
                heading: prev,
                content,
            });
        }
    }
    sections
}

fn section_is_atomizable(section: &SkillSection) -> bool {
    let heading = section.heading.to_lowercase();
    if [
        "overview",
        "purpose",
        "introduction",
        "requirements",
        "prerequisites",
        "when not to use",
    ]
    .iter()
    .any(|blocked| heading.contains(blocked))
    {
        return false;
    }
    section
        .content
        .chars()
        .filter(|c| !c.is_whitespace())
        .count()
        >= 120
}

fn section_cues(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| {
            let lower = line.to_lowercase();
            lower.contains("use when") || lower.contains("trigger") || lower.contains("when ")
        })
        .map(|line| {
            line.trim_start_matches('-')
                .trim()
                .chars()
                .take(220)
                .collect()
        })
        .collect()
}

fn section_verification_steps(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| {
            let lower = line.to_lowercase();
            lower.contains("verify") || lower.contains("test") || lower.contains("check")
        })
        .map(|line| {
            line.trim_start_matches('-')
                .trim()
                .chars()
                .take(220)
                .collect()
        })
        .collect()
}

fn skill_procedure_steps(procedure: &str, fallback: &str) -> Vec<String> {
    let steps: Vec<String> = procedure
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.trim_start_matches(|c: char| {
                c.is_ascii_digit() || matches!(c, '.' | ')' | '-' | '*' | ' ')
            })
            .trim()
            .to_string()
        })
        .filter(|line| !line.is_empty())
        .collect();
    if steps.is_empty() {
        vec![fallback.to_string()]
    } else {
        steps
    }
}

fn external_skill_record_id(
    package: &fieldcraft_storage::SkillPackageRecord,
    spec: &ExternalSkillRecordSpec,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"external-skill-record|");
    hasher.update(package.source_uri.as_bytes());
    hasher.update(b"|");
    hasher.update(package.package_hash.as_bytes());
    hasher.update(b"|");
    hasher.update(spec.atomization_status.as_bytes());
    hasher.update(b"|");
    hasher.update(spec.atomization_key.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Command: query
// ---------------------------------------------------------------------------

/// One lesson returned by a query, local or remote.
struct QueryHit {
    id: String,
    title: String,
    content: String,
}

async fn cmd_query(
    project_id: &str,
    query_text: &str,
    session_id: Option<&str>,
    limit: usize,
    inner_voice: bool,
    source_filter: SourceFilter,
    tier: KnowledgeTier,
) -> Result<()> {
    let hits = query_lessons(
        project_id,
        query_text,
        session_id,
        limit,
        inner_voice,
        source_filter,
        tier,
    )
    .await?;
    if hits.is_empty() {
        println!("No lessons found for project '{project_id}'.");
        return Ok(());
    }
    // Output clean lesson content — no display noise, agent-friendly
    for hit in &hits {
        println!("{}", hit.content);
        println!();
    }
    Ok(())
}

/// The retrieval path shared by `fieldcraft query` and the prompt-submit hook:
/// remote proxying via FIELDCRAFT_SERVER_URL, local retrieval, and the
/// retrieval_log row tagged with the session id.
async fn query_lessons(
    project_id: &str,
    query_text: &str,
    session_id: Option<&str>,
    limit: usize,
    inner_voice: bool,
    source_filter: SourceFilter,
    tier: KnowledgeTier,
) -> Result<Vec<QueryHit>> {
    let resolved_session_id = resolve_session_id(session_id);

    // HTTP mode: when FIELDCRAFT_SERVER_URL is set, route through the remote server.
    if let Some(server_url) = fieldcraft_server_url() {
        let server_url = server_url.trim_end_matches('/').to_string();
        let token = get_or_enroll_token(&server_url).await?;
        let client = reqwest::Client::new();
        let limit_param = limit.to_string();
        let tier_param = tier.as_str();
        let source_filter_param = source_filter.as_str();
        let mut request = client
            .get(format!("{server_url}/lessons"))
            .query(&[
                ("q", query_text),
                ("project", project_id),
                ("limit", &limit_param),
                ("tier", tier_param),
                ("source_filter", source_filter_param),
            ])
            .header("Authorization", format!("Bearer {token}"));
        if let Some(session_id) = resolved_session_id.as_deref() {
            request = request.query(&[("session_id", session_id)]);
        }
        let resp = request.send().await.context("GET /lessons failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GET /lessons returned {status}: {body}");
        }
        let hits: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse lessons response")?;
        let out = hits
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|hit| {
                        Some(QueryHit {
                            id: hit["id"].as_str().unwrap_or_default().to_string(),
                            title: hit["title"].as_str().unwrap_or_default().to_string(),
                            content: hit["content"].as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        return Ok(out);
    }

    let start = std::time::Instant::now();

    let request = RetrievalRequest {
        project_id: project_id.to_string(),
        context: query_text.to_string(),
        files: vec![],
        task: Some(query_text.to_string()),
        preferred_types: vec![],
        tier,
        source_filter,
    };
    let request_context = request.context.clone();
    let request_files = request.files.clone();

    let db_path = fieldcraft_db_path_for_command("query")?;
    let response = retrieve(request, &db_path)
        .await
        .context("retrieval failed")?;
    let query_cue_bundle_json = response.query_cue_bundle_json.clone();
    let raw_candidate_ids = response.raw_candidate_ids.clone();
    let raw_candidate_scores = response.raw_candidate_scores.clone();
    let raw_candidate_titles = response.raw_candidate_titles.clone();
    let raw_candidate_origins = response.raw_candidate_origins.clone();

    let latency_ms = start.elapsed().as_millis() as u64;
    // Keep a larger pool for the inner voice to choose from. Without inner voice,
    // respect the limit directly.
    let pool_size = if inner_voice {
        limit.clamp(10, 20)
    } else {
        limit
    };
    let mut shown: Vec<_> = response.lessons.into_iter().take(pool_size).collect();

    // Inner voice: LLM sub-agent reasons about which lessons are actually relevant
    // and filters out misleading ones. Literature basis:
    //   SWE-ContextBench (arXiv:2602.08316): oracle selection beats free selection +12pp
    //   MSU/Harvard (arXiv:2505.16067): strict selective lesson beats add-all by 15pp
    if inner_voice && !shown.is_empty() {
        shown = apply_inner_voice(&shown, query_text, limit).await;
    }

    // Log retrieval event for feedback loop (fire-and-forget, don't block on errors)
    if let Ok(db_path) = fieldcraft_db_path_for_command("query")
        && let Ok(repo) = fieldcraft_storage::KnowledgeRepository::open(&db_path)
    {
        let ids: Vec<String> = shown.iter().map(|m| m.id.clone()).collect();
        let scores: Vec<f32> = shown.iter().map(|m| m.score).collect();
        let origins: Vec<String> = shown.iter().map(|m| m.source_origin.clone()).collect();
        let session_id = resolved_session_id.unwrap_or_default();
        let _ = repo.log_retrieval_entry(&fieldcraft_storage::RetrievalLogEntry {
            project_id,
            query_text,
            query_context: &request_context,
            query_files: &request_files,
            result_ids: &ids,
            result_scores: &scores,
            latency_ms,
            raw_candidate_ids: &raw_candidate_ids,
            raw_candidate_scores: &raw_candidate_scores,
            raw_candidate_titles: &raw_candidate_titles,
            result_origins: &origins,
            raw_candidate_origins: &raw_candidate_origins,
            query_cue_bundle: &query_cue_bundle_json,
            session_id: &session_id,
            ..Default::default()
        });
    }

    Ok(shown
        .into_iter()
        .map(|m| QueryHit {
            id: m.id,
            title: m.title,
            content: m.content,
        })
        .collect())
}

/// Inner voice sub-agent: LLM reasons about which candidate lessons are
/// actually relevant to the current task, filtering out misleading ones.
///
/// Returns the filtered lesson list. On any failure (no API key, network
/// error, bad JSON), returns the original list unchanged — the inner voice
/// is strictly additive.
async fn apply_inner_voice(
    candidates: &[fieldcraft_retrieval::RetrievedLesson],
    query_text: &str,
    limit: usize,
) -> Vec<fieldcraft_retrieval::RetrievedLesson> {
    let api_key = match std::env::var("OPENAI_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => return candidates.iter().take(limit).cloned().collect(),
    };

    // Build a numbered list of candidates for the LLM
    let mut candidate_block = String::new();
    for (i, m) in candidates.iter().enumerate() {
        candidate_block.push_str(&format!(
            "[{}] id={}\n{}\n\n",
            i + 1,
            m.id,
            m.content.lines().take(8).collect::<Vec<_>>().join("\n")
        ));
    }

    let system = "You are a lesson curator for a coding agent. Pick lessons that \
        are likely to help the task, while rejecting clearly misleading ones. \
        The literature shows that bad selection hurts (SWE-ContextBench: -4pp) but \
        so does over-rejection — be willing to include a lesson when it's plausibly \
        relevant, even if not a perfect match. Return STRICT JSON and nothing else.";

    let prompt = format!(
        "The agent is about to work on this task:\n\n\
         === TASK ===\n{}\n\n\
         === CANDIDATE MEMORIES ===\n{}\n\n\
         Return STRICT JSON:\n\
         {{\"selected\": [\"<id string>\", ...], \"reasoning\": \"<one sentence>\"}}\n\n\
         HOW TO SELECT (be practical, not paranoid):\n\
         - INCLUDE lessons that touch the SAME module, class, or code pattern.\n\
         - INCLUDE lessons that describe a known gotcha in this area of the codebase.\n\
         - INCLUDE lessons whose function/file overlaps with what the task will modify.\n\
         - REJECT lessons that describe code the task explicitly needs to REMOVE or REPLACE \
         (predecessor conflict).\n\
         - REJECT lessons from a clearly DIFFERENT backend/framework \
         (e.g. PostgreSQL lesson for a MySQL task, React lesson for a Vue task).\n\
         - REJECT lessons for completely unrelated subsystems.\n\
         - If ONE lesson is plausibly relevant, include it. Don't demand perfection.\n\
         - Pick at MOST {} lessons.\n\n\
         Return only the JSON object — no prose, no markdown.",
        query_text, candidate_block, limit
    );

    let client = fieldcraft_extract::anthropic::LlmClient::openai(api_key, None);
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());

    let raw = match client.message(&model, system, &prompt, 600).await {
        Ok(r) => r,
        Err(_) => return candidates.iter().take(limit).cloned().collect(),
    };

    // Strip markdown fences if present and extract the JSON object.
    let json_text = {
        let trimmed = raw.trim();
        if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
            if end >= start {
                trimmed[start..=end].to_string()
            } else {
                trimmed.to_string()
            }
        } else {
            trimmed.to_string()
        }
    };

    let curator: serde_json::Value = match serde_json::from_str(&json_text) {
        Ok(v) => v,
        Err(_) => return candidates.iter().take(limit).cloned().collect(),
    };

    let selected_ids: Vec<String> = curator
        .get("selected")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // If the curator returned an empty list, the inner voice said "no lesson helps".
    // Respect that — return empty.
    if selected_ids.is_empty() {
        return Vec::new();
    }

    // Filter candidates to only selected ones, preserving selection order.
    let mut result = Vec::new();
    for id in &selected_ids {
        if let Some(m) = candidates.iter().find(|c| &c.id == id) {
            result.push(m.clone());
        }
        if result.len() >= limit {
            break;
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Command: write
// ---------------------------------------------------------------------------

async fn cmd_write(
    project_id: &str,
    summary: &str,
    kind: KindArg,
    phase_str: Option<&str>,
) -> Result<()> {
    let knowledge_kind = match kind {
        KindArg::Claim => KnowledgeKind::Claim,
        KindArg::Procedure => KnowledgeKind::Procedure,
        KindArg::Episode => KnowledgeKind::Episode,
        KindArg::Directive => KnowledgeKind::Directive,
        KindArg::Context => KnowledgeKind::Context,
    };
    let phase = phase_str.and_then(fieldcraft_core::KnowledgePhase::parse);

    // Build a minimal payload and retrieval text from the summary.
    let (payload_json, retrieval_text) = match knowledge_kind {
        KnowledgeKind::Claim => {
            let payload = ClaimPayload {
                statement: summary.to_string(),
                mechanism: None,
                scope: None,
                exceptions: vec![],
                affected_files: vec![],
                retrieval_questions: vec![],
                trigger: None,
                invalid_if: None,
            };
            let json =
                serde_json::to_string(&payload).context("failed to serialize claim payload")?;
            let text = summary.to_string();
            (json, text)
        }
        KnowledgeKind::Procedure => {
            let payload = ProcedurePayload {
                goal: summary.to_string(),
                preconditions: vec![],
                steps: vec![],
                failure_modes: vec![],
                affected_files: vec![],
                retrieval_questions: vec![],
                trigger: None,
                invalid_if: None,
            };
            let json =
                serde_json::to_string(&payload).context("failed to serialize procedure payload")?;
            let text = summary.to_string();
            (json, text)
        }
        KnowledgeKind::Episode => {
            let payload = fieldcraft_core::EpisodePayload {
                summary: summary.to_string(),
                ..Default::default()
            };
            let json =
                serde_json::to_string(&payload).context("failed to serialize episode payload")?;
            (json, summary.to_string())
        }
        KnowledgeKind::Directive => {
            let payload = fieldcraft_core::DirectivePayload {
                action: summary.to_string(),
                ..Default::default()
            };
            let json = serde_json::to_string(&payload)
                .context("failed to serialize directive payload")?;
            (json, summary.to_string())
        }
        KnowledgeKind::Context => {
            let payload = ContextPayload {
                statement: summary.to_string(),
                ..Default::default()
            };
            let json =
                serde_json::to_string(&payload).context("failed to serialize context payload")?;
            (json, summary.to_string())
        }
    };

    // Generate stable SHA256-based ID.
    let id = {
        let mut hasher = Sha256::new();
        hasher.update(project_id.as_bytes());
        hasher.update(b"|");
        hasher.update(summary.as_bytes());
        format!("{:x}", hasher.finalize())
    };

    let record = KnowledgeRecord {
        protected: false,
        id: id.clone(),
        project_id: project_id.to_string(),
        kind: knowledge_kind,
        title: summary.chars().take(120).collect(),
        payload_json,
        state: KnowledgeState::Active,
        confidence: 0.8,
        source: KnowledgeSource::Manual,
        superseded_by: None,
        project_specific: true,
        created_at: Utc::now(),
        valid_until: None,
        retrieval_text,
        schema_version: if phase.is_some() { 4 } else { 2 },
        phase,
        tier: fieldcraft_core::KnowledgeTier::Active,
        owner: None,
        session_date: None,
        source_refs: vec![],
    };

    let kind_label = match knowledge_kind {
        KnowledgeKind::Claim => "claim",
        KnowledgeKind::Procedure => "procedure",
        KnowledgeKind::Episode => "episode",
        KnowledgeKind::Directive => "directive",
        KnowledgeKind::Context => "context",
    };

    // HTTP mode: when FIELDCRAFT_SERVER_URL is set, route through the remote server.
    if let Some(server_url) = fieldcraft_server_url() {
        let server_url = server_url.trim_end_matches('/').to_string();
        let token = get_or_enroll_token(&server_url).await?;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{server_url}/lessons"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&record)
            .send()
            .await
            .context("POST /lessons failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("POST /lessons returned {status}: {body}");
        }
        eprintln!("[fieldcraft] stored {kind_label} {id} for project '{project_id}' (remote)");
        return Ok(());
    }

    // Store in SQLite (local mode).
    let db_path = fieldcraft_db_path_for_command("import")?;
    let repo =
        KnowledgeRepository::open(&db_path).context("failed to open knowledge repository")?;
    repo.insert_knowledge(&record)
        .context("failed to store knowledge record")?;

    // Index in the appropriate backend (non-fatal if unavailable).
    index_record(&record).await;

    eprintln!("[fieldcraft] stored {kind_label} {id} for project '{project_id}'");

    // Correlate with recent retrievals for implicit feedback signal
    correlate_write_with_retrievals(project_id);

    Ok(())
}

/// If a retrieval happened in the last 10 minutes for this project,
/// record co_session feedback for all returned lessons.
fn correlate_write_with_retrievals(project_id: &str) {
    let db_path = match fieldcraft_db_path_for_command("write") {
        Ok(path) => path,
        Err(_) => return,
    };
    let repo = match fieldcraft_storage::KnowledgeRepository::open(&db_path) {
        Ok(r) => r,
        Err(_) => return,
    };

    if let Some((retrieval_id, ids_json)) = repo.recent_retrieval(project_id, 10) {
        // ids_json is a JSON array like ["id1","id2"]
        if let Ok(ids) = serde_json::from_str::<Vec<String>>(&ids_json) {
            for kid in &ids {
                let _ = repo.record_feedback(kid, "co_session", Some(retrieval_id), project_id);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// US-008: extract-batch-status
// ---------------------------------------------------------------------------

fn cmd_extract_batch_status_inner(
    shared_root: &std::path::Path,
    limit: usize,
) -> anyhow::Result<()> {
    use fieldcraft_storage::list_extraction_batches_recent;

    let batches = list_extraction_batches_recent(shared_root, limit)?;

    if batches.is_empty() {
        println!("No extraction batches found.");
        return Ok(());
    }

    println!(
        "{:<28} {:<14} {:>8} {:>11} {:<22} {:<22}",
        "batch_id", "status", "sessions", "total_req", "created_at", "last_polled_at"
    );
    println!("{}", "-".repeat(110));

    for batch in &batches {
        let batch_id_short = if batch.batch_id.len() > 26 {
            format!("{}…", &batch.batch_id[..26])
        } else {
            batch.batch_id.clone()
        };
        let created = &batch.created_at[..batch.created_at.len().min(22)];
        let polled = batch
            .last_polled_at
            .as_deref()
            .map(|s| &s[..s.len().min(22)])
            .unwrap_or("-");
        println!(
            "{:<28} {:<14} {:>8} {:>11} {:<22} {:<22}",
            batch_id_short,
            batch.status,
            batch.session_ids.len(),
            batch.total_requests,
            created,
            polled
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Command: ingest
// ---------------------------------------------------------------------------

async fn cmd_ingest(
    project_id: &str,
    rebuild: bool,
    claude_home: Option<PathBuf>,
    extra_sources: &[PathBuf],
    skip_codex: bool,
) -> Result<()> {
    use fieldcraft_extract::{ContentBlock, TranscriptMessage};
    use fieldcraft_storage::{discover_local_sessions, list_source_sessions};

    let db_path = fieldcraft_db_path_for_command("ingest")?;

    let claude_home = claude_home.unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".claude")
    });

    // Use a temp dir for the shared_root (source sessions tracking DB).
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let shared_root = PathBuf::from(&home).join(".fieldcraft").join("sessions");

    // Codex home (may not exist — that's fine). Skip if --skip-codex.
    let codex_home = if skip_codex {
        PathBuf::from("/nonexistent-skip-codex")
    } else {
        PathBuf::from(&home).join(".codex")
    };

    println!("Discovering sessions in {}", claude_home.display());

    let report = discover_local_sessions(
        &shared_root,
        Some(project_id),
        &codex_home,
        &claude_home,
        extra_sources,
    )
    .context("failed to discover local sessions")?;

    println!(
        "Scanned {} files, discovered {} sessions ({} new, {} unchanged)",
        report.scanned, report.discovered, report.queued, report.unchanged
    );

    if report.discovered == 0 {
        println!("Nothing to ingest.");
        return Ok(());
    }

    // Choose LLM backend: prefer API keys if set, otherwise use Claude Code CLI.
    let backend = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) if !key.trim().is_empty() => {
            println!("Using Anthropic API for extraction.");
            fieldcraft_extract::extract::LlmBackend::Api(key)
        }
        _ => match std::env::var("OPENAI_API_KEY") {
            Ok(key) if !key.trim().is_empty() => {
                let base_url = std::env::var("OPENAI_BASE_URL").ok();
                let model = std::env::var("OPENAI_MODEL").ok();
                println!(
                    "Using OpenAI API for extraction (model: {}).",
                    model.as_deref().unwrap_or("gpt-4o")
                );
                fieldcraft_extract::extract::LlmBackend::OpenAi {
                    api_key: key,
                    base_url,
                    model,
                }
            }
            _ => {
                println!("Using Claude Code CLI for extraction.");
                fieldcraft_extract::extract::LlmBackend::ClaudeCode
            }
        },
    };

    // Load sessions that need processing.
    let sessions = list_source_sessions(&shared_root, Some(project_id))
        .context("failed to list source sessions")?;

    let to_process: Vec<_> = sessions
        .into_iter()
        .filter(|s| {
            use fieldcraft_core::SourceSessionStatus;
            if rebuild {
                true
            } else {
                matches!(
                    s.status,
                    SourceSessionStatus::Pending | SourceSessionStatus::Stale
                )
            }
        })
        .collect();

    if to_process.is_empty() {
        println!("All sessions already processed.");
        return Ok(());
    }

    println!("Extracting knowledge from {} sessions…", to_process.len());

    let repo =
        KnowledgeRepository::open(&db_path).context("failed to open knowledge repository")?;

    let pipeline = fieldcraft_extract::extract::ExtractionPipeline {
        backend,
        ..Default::default()
    };

    let mut total_extracted = 0usize;

    for session in &to_process {
        // Build a CompressedTranscript from transcript entries.
        let messages: Vec<TranscriptMessage> = session
            .transcript
            .iter()
            .map(|entry| TranscriptMessage {
                role: entry.role.clone(),
                content: vec![ContentBlock::Text(entry.content.clone())],
                timestamp: entry.timestamp.clone(),
            })
            .collect();

        let compressor = fieldcraft_extract::TranscriptCompressor::default();
        let compressed = compressor.compress(&messages);

        match pipeline
            .extract(
                project_id,
                &session.source_id,
                session.started_at.as_deref(),
                &compressed,
            )
            .await
        {
            Ok(result) => {
                let count = result.records.len();
                for record in &result.records {
                    if let Err(e) = repo.insert_knowledge(record) {
                        eprintln!("  failed to store record {}: {e}", record.id);
                    } else {
                        index_record(record).await;
                    }
                }
                total_extracted += count;
                println!(
                    "  session {} → {} records extracted",
                    &session.source_id[..session.source_id.len().min(8)],
                    count
                );
            }
            Err(e) => {
                eprintln!("  session {} extraction failed: {e}", session.source_id);
            }
        }
    }

    println!(
        "\nIngest complete: {} records extracted from {} sessions.",
        total_extracted,
        to_process.len()
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Command: status
// ---------------------------------------------------------------------------

fn format_compact_status_line(
    total_records: usize,
    kinds: &std::collections::BTreeMap<String, i64>,
    import_count: i64,
) -> String {
    let kind_summary = if kinds.is_empty() {
        "no typed records".to_string()
    } else {
        kinds
            .iter()
            .map(|(kind, count)| format!("{kind} {count}"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    format!("{total_records} records ({kind_summary}) | imports queued {import_count}")
}

#[derive(Debug, Deserialize)]
struct RemoteStatusRow {
    kind: String,
    state: String,
    count: i64,
}

#[derive(Debug, Deserialize)]
struct RemoteStatus {
    project_id: String,
    total_records: usize,
    imports_queued: i64,
    rows: Vec<RemoteStatusRow>,
}

async fn cmd_status(project_id: &str, compact: bool) -> Result<()> {
    if let Some(server_url) = fieldcraft_server_url() {
        let server_url = server_url.trim_end_matches('/').to_string();
        let token = get_or_enroll_token(&server_url).await?;
        let resp = reqwest::Client::new()
            .get(format!("{server_url}/status"))
            .query(&[("project", project_id)])
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .context("GET /status failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GET /status returned {status}: {body}");
        }
        let remote: RemoteStatus = resp
            .json()
            .await
            .context("failed to parse status response")?;
        let mut kinds = std::collections::BTreeMap::<String, i64>::new();
        for row in &remote.rows {
            *kinds.entry(row.kind.clone()).or_insert(0) += row.count;
        }
        if compact {
            println!(
                "{}",
                format_compact_status_line(remote.total_records, &kinds, remote.imports_queued)
            );
            return Ok(());
        }

        println!("Project: {} (remote)", remote.project_id);
        println!("{server_url}");
        print_status_table(&remote.rows, remote.imports_queued, remote.total_records);
        return Ok(());
    }

    let db_path = fieldcraft_db_path_for_command("status")?;

    if !db_path.exists() {
        if compact {
            println!("0 records (no database) | imports queued 0");
        } else {
            println!("No knowledge database found at {}.", db_path.display());
            println!("Run `fieldcraft ingest` to get started.");
        }
        return Ok(());
    }

    let repo =
        KnowledgeRepository::open(&db_path).context("failed to open knowledge repository")?;
    let conn = repo.connection();

    // Count by kind and state.
    let mut stmt = conn
        .prepare(
            "SELECT kind, state, COUNT(*) as n
             FROM knowledge
             WHERE project_id = ?1
             GROUP BY kind, state
             ORDER BY kind, state",
        )
        .context("failed to prepare status query")?;

    let rows = stmt
        .query_map(rusqlite::params![project_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .context("failed to query status")?;

    let data: Vec<_> = rows.collect::<std::result::Result<_, _>>()?;

    let mut kinds = std::collections::BTreeMap::<String, i64>::new();
    for (kind, _state, count) in &data {
        *kinds.entry(kind.clone()).or_insert(0) += *count;
    }

    let import_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM imports WHERE project_id = ?1",
            rusqlite::params![project_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let total = repo
        .count_by_project(project_id)
        .context("failed to count knowledge records")?;

    if compact {
        println!(
            "{}",
            format_compact_status_line(total, &kinds, import_count)
        );
        return Ok(());
    }

    println!("Project: {project_id}");
    println!("{}", db_path.display());
    let rows: Vec<RemoteStatusRow> = data
        .into_iter()
        .map(|(kind, state, count)| RemoteStatusRow { kind, state, count })
        .collect();
    print_status_table(&rows, import_count, total);

    Ok(())
}

fn print_status_table(rows: &[RemoteStatusRow], import_count: i64, total: usize) {
    println!();

    if rows.is_empty() {
        println!("No records found.");
    } else {
        println!("{:<12} {:<14} {:>6}", "Kind", "State", "Count");
        println!("{}", "-".repeat(34));
        for row in rows {
            println!("{:<12} {:<14} {:>6}", row.kind, row.state, row.count);
        }
    }

    println!();
    println!("Imports queued: {import_count}");
    println!("Total records:  {total}");
}

#[cfg(test)]
mod tests {
    #[test]
    fn write_accepts_every_documented_lesson_kind() {
        use clap::Parser;
        // The installed agent instructions document all five kinds.
        for kind in ["claim", "procedure", "episode", "directive", "context"] {
            let parsed = super::Cli::try_parse_from(["fieldcraft", "write", "--kind", kind, "x"]);
            assert!(parsed.is_ok(), "write --kind {kind} must parse");
        }
    }

    use super::format_compact_status_line;
    use super::{
        AtomizeMode, ContextPayload, EvalIsolationGuard, LearningSection, derive_project_from_path,
        derive_project_id_from_cwd_path, eval_data_dir_override, external_record_matches,
        external_skill_record_specs, fieldcraft_db_path_for_command, fieldcraft_server_url,
        is_loopback_fieldcraft_url, is_session_path, learning_section_record,
        natural_project_id_from_cwd, parse_transcript_signals, read_env_file_value,
        salient_learning_content, select_fieldcraft_config_value, split_skill_sections,
    };
    use fieldcraft_core::{
        KnowledgeKind, KnowledgeTier, LearningSourceKind, LearningSourceRecord,
        LearningSourceStatus, LearningTraversalPolicy,
    };
    use fieldcraft_storage::{SkillCardRecord, SkillPackageRecord};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::process::Command as TestCommand;

    fn test_package() -> SkillPackageRecord {
        SkillPackageRecord {
            id: "pkg_test-skill".to_string(),
            source_uri: "file:///tmp/test-skill".to_string(),
            package_hash: "abc123".to_string(),
            name: "test-skill".to_string(),
            description: "A test skill".to_string(),
            trust_status: "trusted".to_string(),
            scan_status: "accepted".to_string(),
            file_count: 1,
            total_bytes: 100,
        }
    }

    fn test_card() -> SkillCardRecord {
        SkillCardRecord {
            id: "card_test-skill".to_string(),
            package_id: "pkg_test-skill".to_string(),
            title: "Use test-skill".to_string(),
            skill_name: "test-skill".to_string(),
            description: "Apply the test skill when structured steps are needed.".to_string(),
            activation_cues: vec!["test skill cue".to_string()],
            negative_cues: vec!["do not use for unrelated work".to_string()],
            procedure: "Follow the source skill instructions.".to_string(),
            constraints: vec![],
            required_capabilities: vec!["local files".to_string()],
            verification_steps: vec!["Verify the result matches the request.".to_string()],
            risk_status: "low".to_string(),
            risk_summary: "test only".to_string(),
            retrieval_text: "Use when structured steps are needed.".to_string(),
            dense_vector: None,
        }
    }

    fn long_section(name: &str) -> String {
        format!(
            "Use when {name} applies to a concrete user request.\n\
             1. Inspect the requested inputs and classify the target domain before acting.\n\
             2. Apply the {name} procedure with the exact source constraints preserved.\n\
             3. Verify the result with an explicit check that would fail if the guidance was ignored."
        )
    }

    fn browser_learning_source() -> LearningSourceRecord {
        LearningSourceRecord {
            source_id: "source-browser-x-post".to_string(),
            kind: LearningSourceKind::File,
            uri: "/tmp/browser-x-post.md".to_string(),
            project: "iwantthings-browser-learning-qa".to_string(),
            title: "Saved X Post: Reduce backdrop blur during fast scroll".to_string(),
            scope_hint: Some(
                "ui performance swiftui appkit blur scrolling browser saved x post".to_string(),
            ),
            traversal_policy: LearningTraversalPolicy::default(),
            status: LearningSourceStatus::Running,
            attempts: 1,
            created_at: "2026-07-04T00:00:00Z".to_string(),
            updated_at: "2026-07-04T00:00:00Z".to_string(),
            claimed_at: None,
            finished_at: None,
            source_hash: None,
            derived_record_count: 0,
            worker_id: Some("test-worker".to_string()),
            last_error: None,
        }
    }

    #[test]
    fn browser_learning_content_drops_provenance_before_statement() {
        let content = "Source URL: https://x.com/example/status/1\n\
Source site: x.com\n\
Source frame: https://x.com/i/bookmarks\n\
Browser action: bookmark\n\
Action source: adapter\n\
Confidence: 0.9\n\
Author: @example\n\
Captured at: 2026-07-04T00:00:00Z\n\
Target: Bookmark\n\
Image evidence: https://pbs.twimg.com/media/example.jpg\n\n\
Design Engineering Tip: Reduce backdrop blur while the user scrolls fast. Heavy blur during motion destroys perceived smoothness.\n\n\
Context: This was saved from X/Twitter as a browser memory candidate.";

        let salient = salient_learning_content(content);

        assert!(salient.starts_with("Design Engineering Tip"));
        assert!(salient.contains("Heavy blur during motion"));
        assert!(!salient.contains("Source URL:"));
        assert!(!salient.contains("Browser action:"));
        assert!(!salient.contains("Captured at:"));
    }

    #[test]
    fn browser_learning_content_returns_empty_for_metadata_only_sections() {
        let content = "Source URL: https://x.com/example/status/1\n\
Source site: x.com\n\
Source frame: https://x.com/i/bookmarks\n\
Browser action: bookmark\n\
Action source: adapter\n\
Confidence: 0.9\n\
Captured at: 2026-07-04T00:00:00Z\n\
Target: Bookmark\n\
Author: @example\n\
Image evidence: https://pbs.twimg.com/media/example.jpg";

        assert!(salient_learning_content(content).trim().is_empty());
    }

    #[test]
    fn learning_source_record_uses_project_and_salient_browser_statement() {
        let source = browser_learning_source();
        let section = LearningSection {
            heading: "Saved X Post: Reduce backdrop blur during fast scroll".to_string(),
            content: "Source: https://x.com/gabriell_lab/status/2054901923476607068\n\
Browser action: bookmark\n\
Author: @gabriell_lab\n\
Captured: 2026-06-18T09:37:42Z\n\
Image evidence: https://pbs.twimg.com/amplify_video_thumb/example.jpg\n\n\
Design Engineering Tip: Reduce backdrop blur while the user scrolls fast. Heavy blur during motion destroys perceived smoothness and increases GPU workload. Lowering blur dynamically keeps interfaces crisp while also improving performance.\n\n\
Context: This was saved from X/Twitter as a browser memory candidate."
                .to_string(),
        };

        let record = learning_section_record(&source, "hash", 0, &section).unwrap();
        let payload: ContextPayload = serde_json::from_str(&record.payload_json).unwrap();

        assert_eq!(record.project_id, "iwantthings-browser-learning-qa");
        assert!(record.project_specific);
        assert_eq!(record.kind, KnowledgeKind::Context);
        assert_eq!(record.tier, KnowledgeTier::Active);
        assert!(payload.statement.starts_with("Design Engineering Tip"));
        assert!(payload.statement.contains("Heavy blur during motion"));
        assert!(!payload.statement.contains("Source:"));
        assert!(!payload.statement.contains("Browser action:"));
        assert!(record.retrieval_text.contains("Design Engineering Tip"));
        assert!(record.retrieval_text.contains("source_context:"));
    }

    #[test]
    fn skill_section_splitter_uses_h2_boundaries() {
        let sections = split_skill_sections(&format!(
            "# Test Skill\n\nIntro text.\n\n## Planning\n{}\n\n## Verification\n{}",
            long_section("planning"),
            long_section("verification")
        ));

        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].heading, "Planning");
        assert!(sections[0].content.contains("classify the target domain"));
        assert_eq!(sections[1].heading, "Verification");
    }

    #[test]
    fn external_skill_split_mode_splits_multiple_atomizable_sections() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("SKILL.md"),
            format!(
                "---\nname: test-skill\ndescription: A test skill\n---\n# Test Skill\n\
                 ## Overview\nshort context\n\
                 ## Planning\n{}\n\
                 ## Verification\n{}",
                long_section("planning"),
                long_section("verification")
            ),
        )
        .unwrap();

        let specs = external_skill_record_specs(
            &test_package(),
            &test_card(),
            dir.path(),
            AtomizeMode::Split,
        )
        .unwrap();

        assert_eq!(specs.len(), 2);
        assert!(specs.iter().all(|spec| spec.atomization_status == "split"));
        assert!(
            specs
                .iter()
                .any(|spec| spec.title == "Use test-skill: Planning")
        );
        assert!(
            specs
                .iter()
                .any(|spec| spec.retrieval_text.contains("section: Verification"))
        );
    }

    #[test]
    fn external_skill_whole_mode_keeps_one_record() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("SKILL.md"),
            format!(
                "# Test Skill\n## Planning\n{}\n## Verification\n{}",
                long_section("planning"),
                long_section("verification")
            ),
        )
        .unwrap();

        let specs = external_skill_record_specs(
            &test_package(),
            &test_card(),
            dir.path(),
            AtomizeMode::Whole,
        )
        .unwrap();

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].atomization_status, "kept_whole");
        assert_eq!(specs[0].title, "Use test-skill");
    }

    #[test]
    fn external_record_match_accepts_exact_split_record_labels() {
        assert!(external_record_matches(
            "test-skill: Verification",
            "record-1",
            "Use test-skill: Verification"
        ));
        assert!(external_record_matches(
            "record-1",
            "record-1",
            "Use test-skill: Verification"
        ));
        assert!(!external_record_matches(
            "other-skill: Verification",
            "record-1",
            "Use test-skill: Verification"
        ));
        assert!(!external_record_matches(
            "test-skill: Planning",
            "record-1",
            "Use test-skill: Verification"
        ));
    }

    #[test]
    fn compact_status_formats_kind_breakdown() {
        let mut kinds = BTreeMap::new();
        kinds.insert("claim".to_string(), 9);
        kinds.insert("procedure".to_string(), 3);

        let line = format_compact_status_line(12, &kinds, 5);
        assert_eq!(line, "12 records (claim 9, procedure 3) | imports queued 5");
    }

    #[test]
    fn compact_status_handles_empty_kinds() {
        let line = format_compact_status_line(0, &BTreeMap::new(), 0);
        assert_eq!(line, "0 records (no typed records) | imports queued 0");
    }

    #[test]
    fn update_env_reader_preserves_install_sh_exported_client_config() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("env");
        fs::write(
            &env_file,
            r#"
# install.sh writes this format; update must preserve client mode.
export FIELDCRAFT_SERVER_URL="https://fieldcraft.example"
export FIELDCRAFT_UPSTREAM_URL="https://fieldcraft-upstream.example"
export FIELDCRAFT_AUTH_TOKEN='secret-token'
FIELDCRAFT_DISTILLER=codex
"#,
        )
        .unwrap();

        assert_eq!(
            read_env_file_value(&env_file, "FIELDCRAFT_SERVER_URL").unwrap(),
            Some("https://fieldcraft.example".to_string())
        );
        assert_eq!(
            read_env_file_value(&env_file, "FIELDCRAFT_UPSTREAM_URL").unwrap(),
            Some("https://fieldcraft-upstream.example".to_string())
        );
        assert_eq!(
            read_env_file_value(&env_file, "FIELDCRAFT_AUTH_TOKEN").unwrap(),
            Some("secret-token".to_string())
        );
        assert_eq!(
            read_env_file_value(&env_file, "FIELDCRAFT_DISTILLER").unwrap(),
            Some("codex".to_string())
        );
    }

    #[test]
    fn persisted_config_beats_stale_inherited_process_env() {
        assert_eq!(
            select_fieldcraft_config_value(
                "FIELDCRAFT_SERVER_URL",
                Some("http://fieldcraft-server.example.ts.net:7777".to_string()),
                Some("http://127.0.0.1:7777".to_string()),
            ),
            Some("http://fieldcraft-server.example.ts.net:7777".to_string())
        );
        assert_eq!(
            select_fieldcraft_config_value(
                "FIELDCRAFT_SERVER_URL",
                Some("  ".to_string()),
                Some("fallback".to_string()),
            ),
            Some("fallback".to_string())
        );
        assert_eq!(
            select_fieldcraft_config_value(
                "FIELDCRAFT_AUTH_TOKEN",
                Some("file-token".to_string()),
                Some("env-token".to_string()),
            ),
            Some("env-token".to_string())
        );
    }

    #[test]
    fn update_filters_local_gateway_url_as_persisted_upstream() {
        assert!(is_loopback_fieldcraft_url("http://127.0.0.1:7777"));
        assert!(is_loopback_fieldcraft_url("http://localhost:7777/"));
        assert!(is_loopback_fieldcraft_url("http://127.0.0.1:8888"));
        assert!(is_loopback_fieldcraft_url("http://[::1]:7777"));
        assert!(!is_loopback_fieldcraft_url(
            "http://fieldcraft-server.example.ts.net:7777"
        ));
    }

    #[test]
    fn natural_project_id_uses_git_toplevel_basename() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("Field_Notes");
        let nested = repo.join("crates/server/src");
        fs::create_dir_all(&nested).unwrap();
        let status = TestCommand::new("git")
            .arg("init")
            .arg(&repo)
            .status()
            .unwrap();
        assert!(status.success());

        assert_eq!(natural_project_id_from_cwd(&nested), "Field_Notes");
    }

    #[test]
    fn natural_project_id_falls_back_to_cwd_basename_outside_git() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("subdir");
        fs::create_dir_all(&cwd).unwrap();

        assert_eq!(natural_project_id_from_cwd(&cwd), "subdir");
    }

    #[test]
    fn eval_isolation_suppresses_remote_and_overrides_local_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _guard = EvalIsolationGuard::new(dir.path().to_path_buf());
            assert_eq!(eval_data_dir_override().as_deref(), Some(dir.path()));
            assert_eq!(
                fieldcraft_db_path_for_command("eval").unwrap(),
                dir.path().join("memory.sqlite3")
            );
            assert!(fieldcraft_server_url().is_none());
        }
        assert!(eval_data_dir_override().is_none());
    }

    // --- derive_project_from_path tests ---

    #[test]
    fn derive_project_from_path_claude_multiword_documents() {
        // FIX B: multi-word project name under Documents/ must be preserved
        let path = Path::new(
            "/Users/alice/.claude/projects/-Users-alice-Documents-flower-fields/abc123/session.jsonl",
        );
        assert_eq!(derive_project_from_path(path), "flower-fields");
    }

    #[test]
    fn derive_project_from_path_claude_multiword_code() {
        // FIX B: multi-word project under Code/
        let path =
            Path::new("/Users/radu/.claude/projects/-Users-radu-Code-fieldcraft/sess001.jsonl");
        assert_eq!(derive_project_from_path(path), "fieldcraft");
    }

    #[test]
    fn derive_project_from_path_claude_multiword_projects() {
        // FIX B: multi-word project under Projects/
        let path =
            Path::new("/Users/bar/.claude/projects/-Users-bar-Projects-chief-of-staff/sess.jsonl");
        assert_eq!(derive_project_from_path(path), "chief-of-staff");
    }

    #[test]
    fn derive_project_from_path_claude_fallback_no_known_root() {
        // FIX B: fallback when no known root — skip Users+username, take the rest
        let path = Path::new("/Users/bar/.claude/projects/-Users-bar-some-weird-path/sess.jsonl");
        assert_eq!(derive_project_from_path(path), "some-weird-path");
    }

    #[test]
    fn derive_project_from_path_codex_sessions() {
        let path = Path::new("/Users/radu/.codex/sessions/abc/session.jsonl");
        assert_eq!(derive_project_from_path(path), "codex");
    }

    #[test]
    fn derive_project_from_path_codex_archived() {
        let path = Path::new("/Users/radu/.codex/archived_sessions/old.jsonl");
        assert_eq!(derive_project_from_path(path), "codex");
    }

    #[test]
    fn derive_project_from_path_unknown_for_other_paths() {
        let path = Path::new("/tmp/some/random/file.jsonl");
        assert_eq!(derive_project_from_path(path), "unknown");
    }

    // --- codex cwd-based project derivation tests (FIX C) ---

    /// Helper: extract project name from a synthetic codex JSONL content string,
    /// replicating the logic in cmd_import_sessions.
    fn codex_project_from_jsonl(content: &str) -> String {
        // Replicate the cwd-override logic from cmd_import_sessions
        if let Some(cwd_val) = content.lines().find_map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| {
                    // top-level cwd OR payload.cwd
                    v["cwd"]
                        .as_str()
                        .map(|s| s.to_string())
                        .or_else(|| v["payload"]["cwd"].as_str().map(|s| s.to_string()))
                })
        }) {
            let cwd_path = std::path::Path::new(&cwd_val);
            return derive_project_id_from_cwd_path(cwd_path);
        }
        "codex".to_string()
    }

    #[test]
    fn codex_cwd_top_level_documents_flower_fields() {
        // FIX C: top-level cwd field → basename of the path
        let content = r#"{"type":"start","cwd":"/Users/alice/Documents/flower-fields"}
{"type":"message","role":"user","content":"hello"}"#;
        assert_eq!(codex_project_from_jsonl(content), "flower-fields");
    }

    #[test]
    fn codex_cwd_top_level_code_fieldcraft() {
        let content = r#"{"type":"start","cwd":"/Users/alice/Code/fieldcraft"}
{"type":"message","role":"user","content":"hello"}"#;
        assert_eq!(codex_project_from_jsonl(content), "fieldcraft");
    }

    #[test]
    fn codex_cwd_payload_nested() {
        // FIX C: cwd under payload sub-object
        let content = r#"{"type":"session","payload":{"cwd":"/Users/foo/Projects/chief-of-staff","id":"abc"}}
{"type":"message","content":"hi"}"#;
        assert_eq!(codex_project_from_jsonl(content), "chief-of-staff");
    }

    #[test]
    fn codex_cwd_not_found_falls_back() {
        // FIX C: no cwd in first 50 lines → "codex"
        let content = r#"{"type":"message","role":"user","content":"hello"}
{"type":"message","role":"assistant","content":"world"}"#;
        assert_eq!(codex_project_from_jsonl(content), "codex");
    }

    // --- is_session_path tests ---

    #[test]
    fn is_session_path_accepts_codex_sessions() {
        let home = "/Users/radu";
        let path = Path::new("/Users/radu/.codex/sessions/abc/session.jsonl");
        assert!(is_session_path(path, home));
    }

    #[test]
    fn is_session_path_accepts_codex_archived() {
        let home = "/Users/radu";
        let path = Path::new("/Users/radu/.codex/archived_sessions/old.jsonl");
        assert!(is_session_path(path, home));
    }

    #[test]
    fn is_session_path_accepts_claude_projects() {
        let home = "/Users/radu";
        let path = Path::new("/Users/radu/.claude/projects/-Users-radu-Code-foo/abc/session.jsonl");
        assert!(is_session_path(path, home));
    }

    #[test]
    fn is_session_path_rejects_index_file() {
        let home = "/Users/radu";
        let path =
            Path::new("/Users/radu/.claude/projects/-Users-radu-Code-foo/sessions-index.jsonl");
        assert!(!is_session_path(path, home));
    }

    #[test]
    fn is_session_path_rejects_json_config() {
        let home = "/Users/radu";
        let path = Path::new("/Users/radu/.claude/mcp-needs-auth-cache.json");
        assert!(!is_session_path(path, home));
    }

    #[test]
    fn is_session_path_rejects_version_json() {
        let home = "/Users/radu";
        let path = Path::new("/Users/radu/.codex/version.json");
        assert!(!is_session_path(path, home));
    }

    #[test]
    fn parse_transcript_signals_reads_codex_response_item_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex.jsonl");
        fs::write(
            &path,
            r#"{"timestamp":"2026-05-21T06:14:08.137Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Using lesson:8a277adf0cd0bb33a912fcea06a7fad984c5cec8924ed54de3a662500352389b for verification."}]}}
{"timestamp":"2026-05-21T06:14:09.137Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"lesson:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa should not count"}]}}"#,
        )
        .unwrap();

        let signals = parse_transcript_signals(&path).unwrap();
        assert!(
            signals
                .citations
                .contains("8a277adf0cd0bb33a912fcea06a7fad984c5cec8924ed54de3a662500352389b")
        );
        assert!(
            !signals
                .citations
                .contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn parse_transcript_signals_reads_codex_agent_messages_and_contradictions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex-agent-message.jsonl");
        fs::write(
            &path,
            r#"{"timestamp":"2026-05-21T06:49:23.913Z","type":"event_msg","payload":{"type":"agent_message","message":"lesson:569000ff59f22dc622062c35b9d90069e4ddbd85704cf97fd43e9d2807fcf9ca is outdated for this path."}}"#,
        )
        .unwrap();

        let signals = parse_transcript_signals(&path).unwrap();
        assert!(signals.citations.is_empty());
        assert!(
            signals
                .contradicted
                .contains_key("569000ff59f22dc622062c35b9d90069e4ddbd85704cf97fd43e9d2807fcf9ca")
        );
    }
}

// ---------------------------------------------------------------------------
// Command: import
// ---------------------------------------------------------------------------

async fn cmd_import(project_id: &str, source: Option<PathBuf>) -> Result<()> {
    let source_dir = source.unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home)
            .join(".ai-memory")
            .join("knowledge")
            .join(project_id)
    });

    if !source_dir.exists() {
        anyhow::bail!("source directory not found: {}", source_dir.display());
    }

    let db_path = fieldcraft_db_path_for_command("import")?;
    let repo =
        KnowledgeRepository::open(&db_path).context("failed to open knowledge repository")?;

    let subdirs = ["lessons", "bugs", "decisions", "patterns", "sessions"];
    let mut imported = 0usize;
    let mut skipped = 0usize;

    for subdir in &subdirs {
        let sd = source_dir.join(subdir);
        if !sd.exists() {
            continue;
        }

        let md_files: Vec<_> = walkdir::WalkDir::new(&sd)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
            .collect();

        if md_files.is_empty() {
            continue;
        }

        for entry in &md_files {
            let text = match std::fs::read_to_string(entry.path()) {
                Ok(t) => t,
                Err(_) => continue,
            };

            let (meta, body) = parse_frontmatter(&text);
            if body.trim().is_empty() {
                continue;
            }

            let kind = match *subdir {
                "patterns" => KnowledgeKind::Procedure,
                "sessions" => KnowledgeKind::Episode,
                _ => KnowledgeKind::Claim,
            };

            // Override if frontmatter says session
            let kind = if meta.get("memory_kind").is_some_and(|v| v == "session") {
                KnowledgeKind::Episode
            } else {
                kind
            };

            let title = extract_md_title(&body, &entry.file_name().to_string_lossy());

            let rel_path = entry
                .path()
                .strip_prefix(&source_dir)
                .unwrap_or(entry.path())
                .to_string_lossy();
            let id = {
                let mut hasher = Sha256::new();
                hasher.update(project_id.as_bytes());
                hasher.update(b"|");
                hasher.update(rel_path.as_bytes());
                format!("{:x}", hasher.finalize())
            };

            // Check if already exists
            if repo.get_knowledge(&id).ok().flatten().is_some() {
                skipped += 1;
                continue;
            }

            let retrieval_text = format!("{} {}", title, &body[..body.len().min(2000)]);

            let payload_json = match kind {
                KnowledgeKind::Claim => {
                    let payload = ClaimPayload {
                        statement: title.clone(),
                        mechanism: None,
                        scope: None,
                        exceptions: vec![],
                        affected_files: vec![],
                        retrieval_questions: vec![],
                        trigger: None,
                        invalid_if: None,
                    };
                    serde_json::to_string(&payload).unwrap_or_default()
                }
                KnowledgeKind::Procedure => {
                    let payload = ProcedurePayload {
                        goal: title.clone(),
                        preconditions: vec![],
                        steps: vec![],
                        failure_modes: vec![],
                        affected_files: vec![],
                        retrieval_questions: vec![],
                        trigger: None,
                        invalid_if: None,
                    };
                    serde_json::to_string(&payload).unwrap_or_default()
                }
                KnowledgeKind::Episode => {
                    format!(
                        r#"{{"summary":"{}","outcome":null,"participants":[]}}"#,
                        title.replace('"', "\\\"")
                    )
                }
                KnowledgeKind::Directive | KnowledgeKind::Context => {
                    // Legacy import path doesn't produce new types
                    let payload = ClaimPayload {
                        statement: title.clone(),
                        ..Default::default()
                    };
                    serde_json::to_string(&payload).unwrap_or_default()
                }
            };

            let created_at = meta
                .get("date")
                .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
                .map(|d| d.and_hms_opt(0, 0, 0).unwrap())
                .map(|dt| chrono::DateTime::<Utc>::from_naive_utc_and_offset(dt, Utc))
                .unwrap_or_else(Utc::now);

            let record = KnowledgeRecord {
                protected: false,
                id: id.clone(),
                project_id: project_id.to_string(),
                kind,
                title: title.chars().take(200).collect(),
                payload_json,
                state: KnowledgeState::Active,
                confidence: 0.7,
                source: KnowledgeSource::Manual,
                superseded_by: None,
                project_specific: true,
                created_at,
                valid_until: None,
                retrieval_text,
                schema_version: 2,
                phase: None,
                tier: fieldcraft_core::KnowledgeTier::Active,
                owner: None,
                session_date: None,
                source_refs: vec![],
            };

            match repo.insert_knowledge(&record) {
                Ok(_) => imported += 1,
                Err(e) => eprintln!("  failed to insert {}: {e}", rel_path),
            }
        }

        println!("  {}/{subdir}: {} files", project_id, md_files.len());
    }

    println!("\nImported {imported} records ({skipped} skipped as duplicates).");

    // Auto-index embeddings for newly imported records
    if imported > 0 {
        println!("Indexing embeddings...");
        cmd_reindex(Some(project_id), 32, false).await?;
    }

    Ok(())
}

fn parse_frontmatter(text: &str) -> (std::collections::HashMap<String, String>, String) {
    let mut meta = std::collections::HashMap::new();
    if !text.starts_with("---") {
        return (meta, text.to_string());
    }
    let end = match text[3..].find("---") {
        Some(pos) => pos + 3,
        None => return (meta, text.to_string()),
    };
    let fm = &text[3..end];
    let body = text[end + 3..].trim().to_string();
    for line in fm.lines() {
        if let Some((key, val)) = line.split_once(':') {
            meta.insert(key.trim().to_string(), val.trim().to_string());
        }
    }
    (meta, body)
}

fn extract_md_title(body: &str, filename: &str) -> String {
    for line in body.lines() {
        let line = line.trim();
        if let Some(title) = line.strip_prefix("# ") {
            let title = title.strip_prefix("Session: ").unwrap_or(title);
            return title.chars().take(200).collect();
        }
    }
    filename
        .replace(".md", "")
        .replace('-', " ")
        .chars()
        .take(200)
        .collect()
}

// ---------------------------------------------------------------------------
// Command: reindex
// ---------------------------------------------------------------------------

async fn cmd_reindex(project: Option<&str>, batch_size: usize, all: bool) -> Result<()> {
    let db_path = fieldcraft_db_path_for_command("reindex")?;
    let repo =
        KnowledgeRepository::open(&db_path).context("failed to open knowledge repository")?;
    let conn = repo.connection();

    let stmt = if let Some(pid) = project {
        let sql = if all {
            "SELECT id, retrieval_text FROM knowledge WHERE project_id = ?1"
        } else {
            "SELECT id, retrieval_text FROM knowledge WHERE project_id = ?1 AND dense_vector IS NULL"
        };
        let mut s = conn.prepare(sql).context("prepare")?;
        let rows: Vec<(String, String)> = s
            .query_map(rusqlite::params![pid], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("query")?
            .filter_map(|r| r.ok())
            .collect();
        rows
    } else {
        let sql = if all {
            "SELECT id, retrieval_text FROM knowledge"
        } else {
            "SELECT id, retrieval_text FROM knowledge WHERE dense_vector IS NULL"
        };
        let mut s = conn.prepare(sql).context("prepare")?;
        let rows: Vec<(String, String)> = s
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("query")?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    let total = stmt.len();
    if total == 0 {
        if all {
            println!("No records found to reindex.");
        } else {
            println!("All records already have embeddings.");
        }
        return Ok(());
    }

    let mode = if all {
        "all records"
    } else {
        "missing embeddings"
    };
    println!("Indexing {total} records ({mode}, batch_size={batch_size})...");

    let mut indexed = 0;
    for chunk in stmt.chunks(batch_size) {
        let texts: Vec<String> = chunk.iter().map(|(_, t)| t.clone()).collect();
        let ids: Vec<&str> = chunk.iter().map(|(id, _)| id.as_str()).collect();

        match fieldcraft_retrieval::encode_dense(&texts) {
            Ok(vectors) => {
                for (id, vector) in ids.iter().zip(vectors.iter()) {
                    if let Err(e) = repo.store_dense_vector(id, vector) {
                        eprintln!("  failed to store vector for {id}: {e}");
                    }
                }
                indexed += ids.len();
                eprint!("\r  {indexed}/{total}");
            }
            Err(e) => {
                eprintln!("\n  embedding batch failed: {e}");
            }
        }
    }

    eprintln!();
    println!("Reindex complete: {indexed}/{total} records indexed.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Command: serve
// ---------------------------------------------------------------------------

fn cmd_reextract(target_version: u32, dry_run: bool) -> Result<()> {
    let db_path = fieldcraft_db_path_for_command("re-extract")?;
    let repo =
        KnowledgeRepository::open(&db_path).context("failed to open knowledge repository")?;

    let outdated = repo
        .find_outdated(target_version)
        .context("failed to find outdated records")?;

    if outdated.is_empty() {
        println!("No records with schema_version < {target_version} found. Nothing to do.");
        return Ok(());
    }

    // Extract unique source session IDs
    let session_ids: std::collections::BTreeSet<String> = outdated
        .iter()
        .filter_map(|r| {
            if let KnowledgeSource::Session { ref session_id, .. } = r.source {
                Some(session_id.clone())
            } else {
                None
            }
        })
        .collect();

    if dry_run {
        println!(
            "Dry run — would re-extract {} records from {} sessions:",
            outdated.len(),
            session_ids.len()
        );
        for sid in &session_ids {
            let count = outdated.iter().filter(|r| {
                matches!(&r.source, KnowledgeSource::Session { session_id, .. } if session_id == sid)
            }).count();
            println!("  {sid}: {count} records");
        }
        return Ok(());
    }

    // Delete outdated records and reset sessions
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let shared_root = std::path::PathBuf::from(&home).join(".fieldcraft");
    let mut deleted = 0usize;
    for sid in &session_ids {
        deleted += repo
            .delete_by_source_session(sid)
            .context("failed to delete records")?;
        let _ = reset_source_session_to_pending(&shared_root, sid);
    }

    println!(
        "Deleted {deleted} outdated records from {} sessions.",
        session_ids.len()
    );
    println!("Sessions reset to pending. Run `fieldcraft ingest` to re-extract with new format.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Command: import-sessions (US-013)
// ---------------------------------------------------------------------------

/// Walk source directories, fingerprint session files, and send unprocessed
/// ones to POST /sessions/import on the remote server (when FIELDCRAFT_SERVER_URL set)
/// or insert directly into the local source_sessions catalog (when local).
///
/// INVARIANT: Sessions imported here are raw source material. Any future
/// distillation pipeline that promotes them into knowledge records MUST set
/// tier = Archive on the resulting records.
/// Parse a JSONL session file (codex OR claude format) into transcript entries.
/// Handles both shapes by reading each line as a JSON object and extracting
/// role/content/timestamp from whichever fields are present.
fn parse_session_jsonl(content: &str) -> Vec<fieldcraft_core::SourceTranscriptEntry> {
    use serde_json::Value;

    let mut entries = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        // role: try multiple locations
        let role = obj
            .get("role")
            .and_then(|v| v.as_str())
            .or_else(|| obj.pointer("/message/role").and_then(|v| v.as_str()))
            .or_else(|| obj.pointer("/payload/role").and_then(|v| v.as_str()))
            .or_else(|| obj.get("type").and_then(|v| v.as_str()))
            .unwrap_or("unknown")
            .to_string();

        // content: handle string or array-of-blocks (claude rich content)
        let content_text = {
            let raw = obj
                .get("content")
                .or_else(|| obj.pointer("/message/content"))
                .or_else(|| obj.pointer("/payload/content"))
                .or_else(|| obj.get("text"));
            match raw {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(blocks)) => blocks
                    .iter()
                    .filter_map(|b| {
                        b.get("text")
                            .and_then(|v| v.as_str())
                            .or_else(|| b.as_str())
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                Some(other) => other.to_string(),
                None => String::new(),
            }
        };

        if content_text.is_empty() {
            continue;
        }

        let timestamp = obj
            .get("timestamp")
            .and_then(|v| v.as_str())
            .or_else(|| obj.get("created_at").and_then(|v| v.as_str()))
            .or_else(|| obj.get("time").and_then(|v| v.as_str()))
            .map(|s| s.to_string());

        let kind = obj
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("message")
            .to_string();

        entries.push(fieldcraft_core::SourceTranscriptEntry {
            timestamp,
            role,
            kind,
            content: content_text,
        });
    }
    entries
}

/// Derive a project identifier from the path to a session file.
///
/// Rules:
/// - `~/.codex/sessions/**/*.jsonl`  → "codex" (cwd-override happens in caller)
/// - `~/.codex/archived_sessions/*.jsonl` → "codex"
/// - `~/.claude/projects/-Users-foo-Documents-flower-fields/**/*.jsonl`
///   → strip leading `-`, find a known root segment (Documents, Code, Projects,
///   Developer, work, src), take everything after it joined with `-`.
///   e.g. "-Users-foo-Documents-flower-fields" → "flower-fields"
///   e.g. "-Users-foo-Code-fieldcraft"        → "fieldcraft"
///   Fallback when no known root: skip the first three dash-tokens (Users, USER, ?)
///   and take the rest joined with `-`.
/// - Anything else → "unknown"
pub fn derive_project_from_path(path: &std::path::Path) -> String {
    let path_str = path.to_string_lossy();

    // Claude project sessions: ~/.claude/projects/<encoded-dir>/...
    if let Some(idx) = path_str.find("/.claude/projects/") {
        let after = &path_str[idx + "/.claude/projects/".len()..];
        // The next path component is the encoded directory name
        let encoded_dir = after.split('/').next().unwrap_or("");
        if !encoded_dir.is_empty() {
            // encoded_dir looks like "-Users-foo-Documents-flower-fields"
            // Strip leading '-', then split on '-'
            let stripped = encoded_dir.trim_start_matches('-');
            let tokens: Vec<&str> = stripped.split('-').filter(|s| !s.is_empty()).collect();

            // Known root segments that mark where the real project path begins
            const KNOWN_ROOTS: &[&str] =
                &["Documents", "Code", "Projects", "Developer", "work", "src"];

            if let Some(root_pos) = tokens
                .iter()
                .position(|t| KNOWN_ROOTS.iter().any(|r| r.eq_ignore_ascii_case(t)))
            {
                let project_tokens = &tokens[root_pos + 1..];
                if !project_tokens.is_empty() {
                    return project_tokens.join("-").to_lowercase();
                }
            }

            // Fallback: skip "Users" + username (first two tokens) and join the rest
            if tokens.len() > 2 {
                return tokens[2..].join("-").to_lowercase();
            }

            // Last-resort: take the last non-empty token
            if let Some(last) = tokens.last() {
                return last.to_lowercase();
            }
        }
    }

    // Codex sessions: ~/.codex/sessions/... or ~/.codex/archived_sessions/...
    if path_str.contains("/.codex/") {
        return "codex".to_string();
    }

    "unknown".to_string()
}

/// Returns true if this path is a session JSONL we should import.
/// Restricts to known session paths and excludes config/index files.
fn is_session_path(path: &std::path::Path, home: &str) -> bool {
    let path_str = path.to_string_lossy();

    // Must be a .jsonl file
    if path.extension().map(|x| x != "jsonl").unwrap_or(true) {
        return false;
    }

    // Skip index files
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if file_name == "sessions-index.jsonl" || file_name.ends_with("-index.jsonl") {
        return false;
    }

    // Allow codex session paths
    let codex_sessions = format!("{}/.codex/sessions/", home);
    let codex_archived = format!("{}/.codex/archived_sessions/", home);
    if path_str.starts_with(&codex_sessions) || path_str.starts_with(&codex_archived) {
        return true;
    }

    // Allow claude project session paths (but not config files)
    let claude_projects = format!("{}/.claude/projects/", home);
    if path_str.starts_with(&claude_projects) {
        return true;
    }

    false
}

async fn cmd_import_sessions(extra_sources: &[PathBuf]) -> Result<()> {
    use sha2::{Digest, Sha256};

    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let mut scan_dirs = vec![
        PathBuf::from(&home).join(".codex"),
        PathBuf::from(&home).join(".claude"),
    ];
    scan_dirs.extend_from_slice(extra_sources);

    // Collect session files with their fingerprints.
    let mut sessions: Vec<fieldcraft_core::SourceSessionRecord> = Vec::new();
    let now = chrono::Utc::now().to_rfc3339();

    for dir in &scan_dirs {
        if !dir.exists() {
            continue;
        }

        // Determine if this is an extra source (not a standard home dir)
        let is_extra = !dir.starts_with(PathBuf::from(&home).join(".codex"))
            && !dir.starts_with(PathBuf::from(&home).join(".claude"));

        for entry in walkdir::WalkDir::new(dir)
            .max_depth(8)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();

            // For standard dirs, apply session path filter; extra sources accept any .jsonl
            if !is_extra && !is_session_path(path, &home) {
                continue;
            }
            // Extra sources: still require .jsonl
            if is_extra && path.extension().map(|x| x != "jsonl").unwrap_or(true) {
                continue;
            }

            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let fingerprint = {
                let mut h = Sha256::new();
                h.update(path.to_string_lossy().as_bytes());
                h.update(content.as_bytes());
                format!("{:x}", h.finalize())
            };

            let tool = if path.to_string_lossy().contains("/.codex/") {
                fieldcraft_core::AgentTool::Codex
            } else {
                fieldcraft_core::AgentTool::ClaudeCode
            };
            let tool_str = match tool {
                fieldcraft_core::AgentTool::Codex => "codex",
                _ => "claude",
            };

            let source_id = {
                let mut h = Sha256::new();
                h.update(format!("{}|{}", tool_str, fingerprint).as_bytes());
                format!("{:x}", h.finalize())[..24].to_string()
            };

            let title = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            // Derive project from path; for codex sessions try to get cwd from content
            let project = {
                let mut proj = derive_project_from_path(path);
                if proj == "codex" {
                    // Look for a cwd field in any JSONL line
                    if let Some(cwd_val) = content.lines().find_map(|line| {
                        serde_json::from_str::<serde_json::Value>(line)
                            .ok()
                            .and_then(|v| v["cwd"].as_str().map(|s| s.to_string()))
                    }) {
                        let cwd_path = std::path::Path::new(&cwd_val);
                        let derived = derive_project_id_from_cwd_path(cwd_path);
                        if !derived.is_empty() {
                            proj = derived;
                        }
                    }
                }
                proj
            };

            sessions.push(fieldcraft_core::SourceSessionRecord {
                source_id,
                source_tool: tool,
                source_path: path.to_string_lossy().to_string(),
                session_id: String::new(),
                project,
                machine: String::new(),
                cwd: None,
                title,
                started_at: None,
                updated_at: None,
                discovered_at: now.clone(),
                source_fingerprint: fingerprint,
                transcript: parse_session_jsonl(&content),
                status: fieldcraft_core::SourceSessionStatus::Pending,
                attempts: 0,
                extractor_version: "fieldcraft-cli-import-v1".to_string(),
                last_error: None,
                last_extracted_at: None,
                extraction_batch_id: None,
            });
        }
    }

    if sessions.is_empty() {
        eprintln!("[fieldcraft] no session files found to import");
        return Ok(());
    }

    // HTTP mode: send to remote server.
    if let Some(server_url) = fieldcraft_server_url() {
        let server_url = server_url.trim_end_matches('/').to_string();
        let token = get_or_enroll_token(&server_url).await?;
        let client = reqwest::Client::new();

        // Send in small batches (full transcripts can be huge — some codex
        // sessions are 5-50 MB raw). Server limit is 256 MB; batch of 10 keeps
        // us well under even with worst-case sessions, and gives steady progress.
        let total = sessions.len();
        let mut imported = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;
        let mut sent_so_far = 0usize;

        for chunk in sessions.chunks(10) {
            let resp = client
                .post(format!("{server_url}/sessions/import"))
                .header("Authorization", format!("Bearer {token}"))
                .json(chunk)
                .send()
                .await
                .context("POST /sessions/import failed")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                eprintln!(
                    "[fieldcraft] batch failed at offset {sent_so_far}: {status}: {}",
                    body.chars().take(200).collect::<String>()
                );
                failed += chunk.len();
                sent_so_far += chunk.len();
                continue;
            }

            let body: serde_json::Value = resp
                .json()
                .await
                .context("failed to parse import response")?;
            if let Some(summary) = body["summary"].as_object() {
                imported += summary["imported"].as_u64().unwrap_or(0) as usize;
                skipped += summary["skipped_existing"].as_u64().unwrap_or(0) as usize;
                failed += summary["failed"].as_u64().unwrap_or(0) as usize;
            }
            sent_so_far += chunk.len();
            if sent_so_far.is_multiple_of(100) || sent_so_far == total {
                eprintln!(
                    "[fieldcraft] import progress: {sent_so_far}/{total} ({imported} imported, {skipped} skipped, {failed} failed)"
                );
            }
        }

        eprintln!(
            "[fieldcraft] import-sessions: {total} scanned, {imported} imported, {skipped} skipped, {failed} failed (remote)"
        );
        return Ok(());
    }

    // Local mode: insert directly into source_sessions catalog.
    let data_dir = {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".fieldcraft")
    };
    let conn = fieldcraft_storage::open_source_sessions_catalog(&data_dir)
        .context("failed to open source sessions catalog")?;

    let mut imported = 0usize;
    let mut skipped = 0usize;

    for record in &sessions {
        let tool_str = match record.source_tool {
            fieldcraft_core::AgentTool::Codex => "codex",
            fieldcraft_core::AgentTool::ClaudeCode => "claude",
            fieldcraft_core::AgentTool::Cursor => "cursor",
            fieldcraft_core::AgentTool::Unknown => "unknown",
        };
        let transcript_json =
            serde_json::to_string(&record.transcript).unwrap_or_else(|_| "[]".to_string());

        conn.execute_batch("BEGIN IMMEDIATE").ok();
        let rows = conn.execute(
            "INSERT OR IGNORE INTO source_sessions
             (source_id, source_tool, source_path, session_id, project, machine, cwd,
              title, started_at, updated_at, discovered_at, source_fingerprint,
              transcript_json, status, attempts, extractor_version, last_error, last_extracted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            rusqlite::params![
                record.source_id,
                tool_str,
                record.source_path,
                record.session_id,
                record.project,
                record.machine,
                record.cwd,
                record.title,
                record.started_at,
                record.updated_at,
                record.discovered_at,
                record.source_fingerprint,
                transcript_json,
                "pending",
                record.attempts,
                record.extractor_version,
                record.last_error,
                record.last_extracted_at,
            ],
        );
        conn.execute_batch("COMMIT").ok();

        match rows {
            Ok(n) if n > 0 => imported += 1,
            _ => skipped += 1,
        }
    }

    let total = sessions.len();
    eprintln!(
        "[fieldcraft] import-sessions: {total} scanned, {imported} imported, {skipped} skipped (local)"
    );
    Ok(())
}

async fn cmd_feedback(
    id: &str,
    outcome: &str,
    note: Option<&str>,
    session_id: Option<&str>,
) -> Result<()> {
    let normalized = outcome.trim().to_lowercase();
    if !matches!(
        normalized.as_str(),
        "confirmed" | "ignored" | "contradicted"
    ) {
        anyhow::bail!("invalid outcome '{outcome}': must be confirmed | ignored | contradicted");
    }

    let server_url =
        fieldcraft_server_url().context("FIELDCRAFT_SERVER_URL must be set to record feedback")?;
    let server_url = server_url.trim_end_matches('/').to_string();
    let token = get_or_enroll_token(&server_url).await?;

    let body = serde_json::json!({
        "outcome": normalized,
        "session_id": session_id,
        "note": note,
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{server_url}/lessons/{id}/feedback"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .context("POST /lessons/{id}/feedback failed")?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("feedback returned {status}: {text}");
    }

    let result: serde_json::Value = resp
        .json()
        .await
        .context("failed to parse feedback response")?;
    let before = result["confidence_before"].as_f64().unwrap_or(0.0);
    let after = result["confidence_after"].as_f64().unwrap_or(0.0);
    eprintln!("[fieldcraft] feedback {normalized} for {id}: {before:.3} -> {after:.3}");
    Ok(())
}

// ---------------------------------------------------------------------------
// distill-gate: cheap local friction score. No LLM, no network. The Stop hook
// uses this to skip quiet-session distillation while still always writing the
// session-end marker and running trace-analyze.
// ---------------------------------------------------------------------------

fn cmd_distill_gate(transcript: &Path, min_friction: u32, force: bool, json: bool) -> Result<()> {
    if !transcript.exists() {
        anyhow::bail!("transcript not found: {}", transcript.display());
    }
    let force = force || fieldcraft_extract::env_force_enabled();
    let report = fieldcraft_extract::score_transcript_path(transcript)
        .with_context(|| format!("reading transcript {}", transcript.display()))?;
    let line = fieldcraft_extract::format_gate_line(&report, min_friction, force);
    if json {
        let decision = fieldcraft_extract::distill_decision(report.score, min_friction, force);
        let payload = serde_json::json!({
            "decision": decision.as_str(),
            "score": if force { serde_json::Value::String("forced".into()) } else { report.score.into() },
            "threshold": min_friction,
            "reasons": if force {
                "FIELDCRAFT_DISTILL_FORCE".to_string()
            } else {
                report.counts.reasons_compact()
            },
            "counts": {
                "interrupt": report.counts.interrupt,
                "rejection": report.counts.rejection,
                "user_correction": report.counts.user_correction,
                "tool_failure": report.counts.tool_failure,
                "retry": report.counts.retry,
            },
        });
        println!("{}", serde_json::to_string(&payload)?);
    } else {
        println!("[fieldcraft-distill-gate] {line}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// trace-analyze: scan a session transcript and emit closed-loop feedback events
// via POST /sessions/trace-analyze. The server does the heavy lifting (cue
// match factor + Ignored event emission); the client just extracts signals.
//
// Three event kinds emitted:
//   - Confirmed     for every `lesson:<id>` citation in assistant text
//   - Contradicted  for every explicit refutation pattern near a citation
//   - Ignored       for retrieved-but-not-cited lessons whose cue match against
//                   the query was >= threshold (server-side computed)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct TranscriptSignals {
    citations: std::collections::BTreeSet<String>,
    contradicted: std::collections::BTreeMap<String, String>,
    first_ts: Option<String>,
    last_ts: Option<String>,
}

/// Parse a session transcript JSONL file, collecting citations + explicit
/// contradictions + the time bounds of the session. Only assistant turns are
/// scanned — user input and tool output are deliberately ignored.
fn is_assistant_transcript_turn(v: &serde_json::Value) -> bool {
    v.get("type").and_then(|s| s.as_str()) == Some("assistant")
        || v.get("role").and_then(|s| s.as_str()) == Some("assistant")
        || v.pointer("/message/role").and_then(|s| s.as_str()) == Some("assistant")
        || v.pointer("/payload/role").and_then(|s| s.as_str()) == Some("assistant")
        || v.pointer("/payload/message/role").and_then(|s| s.as_str()) == Some("assistant")
        || v.pointer("/payload/type").and_then(|s| s.as_str()) == Some("agent_message")
}

fn append_transcript_text(v: &serde_json::Value, out: &mut String) {
    for pointer in [
        "/message/content",
        "/content",
        "/payload/message/content",
        "/payload/content",
    ] {
        if let Some(content) = v.pointer(pointer).and_then(|c| c.as_array()) {
            for block in content {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    out.push_str(text);
                    out.push('\n');
                }
            }
        }
    }

    for pointer in [
        "/text",
        "/message/text",
        "/payload/text",
        "/payload/message",
        "/payload/message/text",
    ] {
        if let Some(text) = v.pointer(pointer).and_then(|t| t.as_str()) {
            out.push_str(text);
            out.push('\n');
        }
    }
}

fn parse_transcript_signals(transcript: &Path) -> Result<TranscriptSignals> {
    let citation_re = regex::Regex::new(r"lesson:([a-f0-9]{16,})").expect("static regex compiles");
    // Explicit refutation patterns. Conservative — only catch unambiguous
    // factual claims, not "this didn't help me" (which is just Ignored).
    let contradict_re = regex::Regex::new(
        r"(?i)(?:\blesson:([a-f0-9]{16,})\s+(?:is|was|seems|appears\s+to\s+be)\s+(?:wrong|incorrect|outdated|stale|invalid|obsolete)\b|\b(?:contradict(?:s|ing|ed)|refut(?:es|ing|ed))\s+lesson:([a-f0-9]{16,})\b|\blesson:([a-f0-9]{16,})\s+(?:doesn'?t|does\s+not)\s+(?:work\s+anymore|hold\b|apply\s+anymore))",
    )
    .expect("static regex compiles");

    let file = std::fs::File::open(transcript)
        .with_context(|| format!("opening transcript {}", transcript.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut citations: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut contradicted: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let mut first_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Track timestamps from every line (not just assistant) so the
        // session window covers the full transcript.
        if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
            if first_ts.is_none() {
                first_ts = Some(ts.to_string());
            }
            last_ts = Some(ts.to_string());
        }

        if !is_assistant_transcript_turn(&v) {
            continue;
        }

        let mut text_buf = String::new();
        append_transcript_text(&v, &mut text_buf);

        for cap in citation_re.captures_iter(&text_buf) {
            if let Some(m) = cap.get(1) {
                citations.insert(m.as_str().to_string());
            }
        }

        // Contradicted regex has THREE capture groups (one per branch); pick
        // whichever matched. Store the surrounding snippet as the note.
        for cap in contradict_re.captures_iter(&text_buf) {
            let id = cap
                .get(1)
                .or_else(|| cap.get(2))
                .or_else(|| cap.get(3))
                .map(|m| m.as_str().to_string());
            if let Some(id) = id {
                let snippet = cap
                    .get(0)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
                contradicted.insert(id, snippet);
            }
        }
    }

    // If the transcript had no timestamps, fall back to file mtime / "now".
    if first_ts.is_none() || last_ts.is_none() {
        let now = chrono::Utc::now().to_rfc3339();
        let one_day_ago = (chrono::Utc::now() - chrono::Duration::hours(24)).to_rfc3339();
        first_ts.get_or_insert(one_day_ago);
        last_ts.get_or_insert(now);
    }

    // Citations that are also contradicted should NOT count as confirmed.
    for id in contradicted.keys() {
        citations.remove(id);
    }

    Ok(TranscriptSignals {
        citations,
        contradicted,
        first_ts,
        last_ts,
    })
}

fn changed_files_in_cwd(cwd: Option<&Path>) -> Result<Vec<String>> {
    let current_dir;
    let dir = match cwd {
        Some(path) => path,
        None => {
            current_dir = std::env::current_dir().context("current_dir")?;
            current_dir.as_path()
        }
    };

    let mut files = std::collections::BTreeSet::new();
    for staged in [false, true] {
        let mut command = ProcessCommand::new("git");
        command
            .current_dir(dir)
            .arg("diff")
            .arg("--name-only")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if staged {
            command.arg("--staged");
        }
        let output = match command.output() {
            Ok(output) if output.status.success() => output,
            _ => continue,
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                files.insert(trimmed.to_string());
            }
        }
    }
    Ok(files.into_iter().collect())
}

async fn cmd_trace_analyze(session_id: &str, transcript: &Path, cwd: Option<&Path>) -> Result<()> {
    if !transcript.exists() {
        anyhow::bail!("transcript not found: {}", transcript.display());
    }

    let server_url =
        fieldcraft_server_url().context("FIELDCRAFT_SERVER_URL must be set to record feedback")?;
    let server_url = server_url.trim_end_matches('/').to_string();
    let token = get_or_enroll_token(&server_url).await?;

    let project_id = match cwd {
        Some(p) => derive_project_id_from_cwd_path(p),
        None => derive_project_id(),
    };

    let signals = parse_transcript_signals(transcript)?;
    let changed_files = changed_files_in_cwd(cwd)?;

    if signals.citations.is_empty() && signals.contradicted.is_empty() {
        eprintln!(
            "[fieldcraft] trace-analyze: no citations or contradictions in session {session_id} \
             (no Confirmed/Contradicted to emit; Ignored events would only land if cue match \
             surfaced something — sending request anyway so the server can compute them)"
        );
    } else {
        eprintln!(
            "[fieldcraft] trace-analyze: session {session_id}: {} citation(s), {} contradiction(s)",
            signals.citations.len(),
            signals.contradicted.len()
        );
    }

    let contradicted_payload: Vec<serde_json::Value> = signals
        .contradicted
        .iter()
        .map(|(id, ctx)| serde_json::json!({ "id": id, "context": ctx }))
        .collect();
    let cited_ids: Vec<String> = signals.citations.iter().cloned().collect();

    let body = serde_json::json!({
        "session_id": session_id,
        "project_id": project_id,
        "since": signals.first_ts,
        "until": signals.last_ts,
        "cited_ids": cited_ids,
        "contradicted": contradicted_payload,
        "changed_files": changed_files,
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{server_url}/sessions/trace-analyze"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .context("POST /sessions/trace-analyze failed")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("trace-analyze returned {status}: {text}");
    }
    let result: serde_json::Value = resp.json().await.context("parsing response")?;
    eprintln!(
        "[fieldcraft] trace-analyze: confirmed={} contradicted={} ignored={} idempotent={}",
        result["confirmed_emitted"].as_u64().unwrap_or(0),
        result["contradicted_emitted"].as_u64().unwrap_or(0),
        result["ignored_emitted"].as_u64().unwrap_or(0),
        result["idempotent"].as_u64().unwrap_or(0)
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Indexing — SQLite only
// ---------------------------------------------------------------------------

/// Index a knowledge record in SQLite (non-fatal).
async fn index_record(record: &KnowledgeRecord) {
    let _ = try_index_sqlite(record);
}

fn try_index_sqlite(record: &KnowledgeRecord) -> Result<(), String> {
    if !local_catalog_allowed() {
        return Err(
            "remote FIELDCRAFT_SERVER_URL configured; local sqlite indexing skipped".into(),
        );
    }
    let db_path = fieldcraft_db_path_for_command("index").map_err(|e| e.to_string())?;
    let repo = KnowledgeRepository::open(&db_path).map_err(|e| format!("sqlite open: {e}"))?;
    fieldcraft_retrieval::index_knowledge_sqlite(&repo, record)
        .map_err(|e| format!("sqlite index: {e}"))
}

// ---------------------------------------------------------------------------
// Closed-loop feedback stats + session inspector (Phase 7 of cue-bundle plan)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ClosedLoopStatsResponse {
    window_days: u32,
    confirmed_recent: i64,
    ignored_recent: i64,
    contradicted_recent: i64,
    dynamic_confidence_non_null: i64,
    retrieval_log_with_query_cue_bundle: i64,
    retrieval_log_without_query_cue_bundle: i64,
}

async fn cmd_stats(limit: usize, days: u32) -> Result<()> {
    if let Some(server_url) = fieldcraft_server_url() {
        let server_url = server_url.trim_end_matches('/').to_string();
        let token = get_or_enroll_token(&server_url).await?;
        let days_param = days.to_string();
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{server_url}/stats"))
            .query(&[("days", days_param.as_str())])
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .context("GET /stats failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GET /stats returned {status}: {body}");
        }
        let stats: ClosedLoopStatsResponse = resp
            .json()
            .await
            .context("failed to parse stats response")?;
        print_authoritative_stats(&stats);
        return Ok(());
    }

    let path = fieldcraft_db_path_for_command("stats")?;
    let conn = fieldcraft_storage::open_catalog(&path).context("open catalog")?;

    println!("# fieldcraft stats — closed-loop feedback view\n");
    println!("## Proof counts");
    println!("- outcome events in last {days} days:");
    for outcome in ["confirmed", "ignored", "contradicted"] {
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM lesson_outcome_events \
                     WHERE outcome = ?1 AND created_at >= datetime('now', '-{days} days')"
                ),
                [outcome],
                |row| row.get(0),
            )
            .unwrap_or(0);
        println!("  - {outcome}: {count}");
    }
    let dyn_non_null: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM knowledge WHERE dynamic_confidence IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let dyn_differs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM knowledge \
             WHERE dynamic_confidence IS NOT NULL \
               AND ABS(dynamic_confidence - confidence) > 0.000001",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    println!("- knowledge rows with dynamic_confidence: {dyn_non_null}");
    println!("- knowledge rows where dynamic_confidence differs from confidence: {dyn_differs}");

    let qcb_with: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM retrieval_log \
             WHERE query_cue_bundle IS NOT NULL AND TRIM(query_cue_bundle) <> ''",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let qcb_without: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM retrieval_log \
             WHERE query_cue_bundle IS NULL OR TRIM(query_cue_bundle) = ''",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    println!("- retrieval_log rows with query_cue_bundle: {qcb_with}");
    println!("- retrieval_log rows without query_cue_bundle: {qcb_without}\n");

    // ---- North Star: session lift -------------------------------------------------
    let with_confirms: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT session_id) FROM lesson_outcome_events \
             WHERE outcome = 'confirmed' AND session_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let total_sessions: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT session_id) FROM lesson_outcome_events \
             WHERE session_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let lift = if total_sessions > 0 {
        100.0 * with_confirms as f64 / total_sessions as f64
    } else {
        0.0
    };
    println!("## North Star — Session lift");
    println!(
        "- {with_confirms}/{total_sessions} sessions had ≥1 confirmed retrieval ({lift:.1}%)\n"
    );

    // ---- Feedback event totals ----------------------------------------------------
    let total_events: i64 = conn
        .query_row("SELECT COUNT(*) FROM lesson_outcome_events", [], |row| {
            row.get(0)
        })
        .unwrap_or(0);
    println!("## Feedback events");
    println!("- {total_events} total outcome events recorded");
    let mut stmt = conn.prepare(
        "SELECT outcome, COUNT(*) FROM lesson_outcome_events GROUP BY outcome ORDER BY outcome",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (kind, count) = row?;
        println!("  - {kind}: {count}");
    }
    println!();

    // ---- Workhorses ----------------------------------------------------------------
    println!("## Workhorses (confirmed ≥3, confirm rate ≥0.5)");
    print_outcome_grouped(
        &conn,
        "SELECT k.id, k.title,
                SUM(CASE WHEN e.outcome = 'confirmed' THEN 1 ELSE 0 END) AS confirmed,
                COUNT(e.id) AS total
         FROM knowledge k
         JOIN lesson_outcome_events e ON e.knowledge_id = k.id
         GROUP BY k.id, k.title
         HAVING confirmed >= 3 AND CAST(confirmed AS REAL) / total >= 0.5
         ORDER BY confirmed DESC LIMIT ?1",
        limit,
        "(none yet — needs more retrieval feedback)",
    )?;

    // ---- Noise candidates ----------------------------------------------------------
    println!("## Noise candidates (retrievals ≥5, confirm rate <0.2)");
    print_outcome_grouped(
        &conn,
        "SELECT k.id, k.title,
                SUM(CASE WHEN e.outcome = 'confirmed' THEN 1 ELSE 0 END) AS confirmed,
                COUNT(e.id) AS total
         FROM knowledge k
         JOIN lesson_outcome_events e ON e.knowledge_id = k.id
         GROUP BY k.id, k.title
         HAVING total >= 5 AND CAST(confirmed AS REAL) / total < 0.2
         ORDER BY total DESC LIMIT ?1",
        limit,
        "(none — no records hit the noise threshold)",
    )?;

    // ---- Ghosts -------------------------------------------------------------------
    println!("## Ghosts (no feedback events, age >30d)");
    let mut stmt = conn.prepare(
        "SELECT k.id, k.title, k.created_at
         FROM knowledge k
         WHERE k.id NOT IN (SELECT DISTINCT knowledge_id FROM lesson_outcome_events)
           AND k.created_at < datetime('now', '-30 days')
         ORDER BY k.created_at ASC LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut ghost_count = 0;
    for row in rows {
        let (id, title, created_at) = row?;
        let date = created_at.split('T').next().unwrap_or(&created_at);
        println!("- [{date}] {title}  ({id})");
        ghost_count += 1;
    }
    if ghost_count == 0 {
        println!("  (none — all aged records have feedback)");
    }
    println!();

    Ok(())
}

fn print_authoritative_stats(stats: &ClosedLoopStatsResponse) {
    println!("# fieldcraft stats — closed-loop feedback view\n");
    println!("## Proof counts");
    println!("- outcome events in last {} days:", stats.window_days);
    println!("  - confirmed: {}", stats.confirmed_recent);
    println!("  - ignored: {}", stats.ignored_recent);
    println!("  - contradicted: {}", stats.contradicted_recent);
    println!(
        "- knowledge rows with dynamic_confidence: {}",
        stats.dynamic_confidence_non_null
    );
    println!(
        "- retrieval_log rows with query_cue_bundle: {}",
        stats.retrieval_log_with_query_cue_bundle
    );
    println!(
        "- retrieval_log rows without query_cue_bundle: {}",
        stats.retrieval_log_without_query_cue_bundle
    );
}

/// Shared formatter for the workhorses / noise queries: each row is
/// `(id, title, confirmed, total)`.
fn print_outcome_grouped(
    conn: &rusqlite::Connection,
    sql: &str,
    limit: usize,
    empty_msg: &str,
) -> Result<()> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([limit as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    let mut count = 0;
    for row in rows {
        let (id, title, confirmed, total) = row?;
        let rate = 100.0 * confirmed as f64 / total.max(1) as f64;
        println!("- [{confirmed}/{total}, {rate:.0}%] {title}  ({id})");
        count += 1;
    }
    if count == 0 {
        println!("  {empty_msg}");
    }
    println!();
    Ok(())
}

fn cmd_session(session_id: &str) -> Result<()> {
    let path = fieldcraft_db_path_for_command("session")?;
    let conn = fieldcraft_storage::open_catalog(&path).context("open catalog")?;

    println!("# Session {session_id}\n");

    let event_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM lesson_outcome_events WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if event_count == 0 {
        println!("No feedback events recorded for this session.");
        return Ok(());
    }

    println!("## Summary");
    println!("- {event_count} outcome events");
    let mut stmt = conn.prepare(
        "SELECT outcome, COUNT(*) FROM lesson_outcome_events \
         WHERE session_id = ?1 GROUP BY outcome ORDER BY outcome",
    )?;
    let rows = stmt.query_map([session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (kind, count) = row?;
        println!("  - {kind}: {count}");
    }
    println!();

    println!("## Events");
    let mut stmt = conn.prepare(
        "SELECT e.knowledge_id, k.title, k.kind, e.outcome, \
                e.confidence_before, e.confidence_after, e.source, e.created_at, e.note
         FROM lesson_outcome_events e
         LEFT JOIN knowledge k ON k.id = e.knowledge_id
         WHERE e.session_id = ?1
         ORDER BY e.created_at",
    )?;
    let rows = stmt.query_map([session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<f64>>(4)?,
            row.get::<_, Option<f64>>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, Option<String>>(8)?,
        ))
    })?;
    for row in rows {
        let (kid, title, kind, outcome, before, after, source, created_at, note) = row?;
        let title = title.unwrap_or_else(|| "<deleted>".to_string());
        let kind = kind.unwrap_or_else(|| "?".to_string());
        let ts = created_at
            .split('T')
            .nth(1)
            .map(|t| t.split('.').next().unwrap_or(t).to_string())
            .unwrap_or_default();
        println!("- {ts} [{outcome:>13}] {kind:<10} {title}");
        println!("    id: {kid}  source: {source}");
        if let (Some(b), Some(a)) = (before, after) {
            println!("    confidence: {b:.3} → {a:.3}");
        }
        if let Some(n) = note {
            println!("    note: {n}");
        }
    }

    Ok(())
}

fn cmd_provenance(id: &str) -> Result<()> {
    let repo = KnowledgeRepository::open(&fieldcraft_db_path_for_command("provenance")?)
        .context("open catalog")?;
    let Some(record) = repo.get_knowledge(id).context("load lesson")? else {
        anyhow::bail!("lesson not found: {id}");
    };

    println!("# Provenance for {}", record.id);
    println!("- title: {}", record.title);
    println!("- kind: {:?}", record.kind);

    let mut refs = record.source_refs.clone();
    if refs.is_empty()
        && let KnowledgeSource::Session { session_id, .. } = &record.source
    {
        refs.push(fieldcraft_core::SourceReference {
            source_id: session_id.clone(),
            chunk_index: None,
        });
    }

    if refs.is_empty() {
        println!("\nNo source-session references recorded for this lesson.");
        return Ok(());
    }

    let roots = source_session_roots();
    for source_ref in refs {
        println!("\n## Source {}", source_ref.source_id);
        let session = roots.iter().find_map(|root| {
            fieldcraft_storage::get_source_session(root, &source_ref.source_id)
                .ok()
                .flatten()
                .map(|session| (root, session))
        });

        let Some((root, session)) = session else {
            println!("- source session not found in local catalogs");
            continue;
        };

        println!("- catalog: {}", root.display());
        println!("- external session: {}", session.session_id);
        println!("- tool: {:?}", session.source_tool);
        println!("- path: {}", session.source_path);
        if let Some(started_at) = &session.started_at {
            println!("- started: {started_at}");
        }

        if let Some(chunk_index) = source_ref.chunk_index {
            use fieldcraft_extract::{ContentBlock, TranscriptCompressor, TranscriptMessage};
            let messages: Vec<TranscriptMessage> = session
                .transcript
                .iter()
                .map(|entry| TranscriptMessage {
                    role: entry.role.clone(),
                    content: vec![ContentBlock::Text(entry.content.clone())],
                    timestamp: entry.timestamp.clone(),
                })
                .collect();
            let compressed = TranscriptCompressor::default().compress(&messages);
            let chunks = fieldcraft_extract::chunker::chunk_transcript(&compressed);
            if let Some(chunk) = chunks.get(chunk_index) {
                println!("\n### Extraction chunk {chunk_index}\n");
                println!("{}", chunk.text);
            } else {
                println!("- extraction chunk {chunk_index} no longer reconstructs");
            }
        }
    }

    Ok(())
}

fn source_session_roots() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let mut roots = Vec::new();
    if let Ok(fieldcraft_home) = std::env::var("FIELDCRAFT_HOME") {
        roots.push(PathBuf::from(fieldcraft_home));
    }
    roots.push(PathBuf::from(&home).join(".fieldcraft"));
    roots.push(PathBuf::from(&home).join(".fieldcraft").join("sessions"));
    roots
}

async fn cmd_lifecycle(apply: bool, min_age_days: i64, data_dir: Option<&Path>) -> Result<()> {
    if data_dir.is_none()
        && let Some(server_url) = fieldcraft_server_url()
        && !is_loopback_fieldcraft_url(&server_url)
    {
        if apply {
            anyhow::bail!(
                "lifecycle refuses to use a local catalog because FIELDCRAFT_SERVER_URL is non-local ({server_url}). Use the server-backed path or run this command on the server host."
            );
        }
        return cmd_lifecycle_remote(min_age_days, &server_url).await;
    }

    let db_path = fieldcraft_db_path_for_command_or_data_dir("lifecycle", data_dir)?;
    if !db_path.exists() {
        anyhow::bail!("catalog not found at {}", db_path.display());
    }
    let repo = KnowledgeRepository::open(&db_path)
        .with_context(|| format!("opening knowledge repo at {}", db_path.display()))?;
    let report = lifecycle_report(&repo, min_age_days)?;

    if report.candidates.is_empty() {
        print!(
            "{}",
            render_lifecycle_report_stdout(&report, if apply { "APPLY" } else { "DRY-RUN" }, None)
        );
        return Ok(());
    }

    let mut archived = 0usize;
    let mut skipped = 0usize;
    for c in &report.candidates {
        if apply {
            // A per-record failure (e.g. NotFound from a concurrent change)
            // must skip that record, not abort the whole batch.
            match repo.archive_record(&c.id, &c.reason) {
                Ok(()) => archived += 1,
                Err(e) => {
                    eprintln!("    skip {}: {e}", c.id);
                    skipped += 1;
                }
            }
        }
    }
    print!(
        "{}",
        render_lifecycle_report_stdout(
            &report,
            if apply { "APPLY" } else { "DRY-RUN" },
            apply.then_some((archived, skipped)),
        )
    );
    Ok(())
}

async fn cmd_lifecycle_remote(min_age_days: i64, server_url: &str) -> Result<()> {
    let token = get_or_enroll_token(server_url).await?;
    let report: fieldcraft_storage::LifecycleReport = reqwest::Client::new()
        .get(format!("{server_url}/lifecycle"))
        .query(&[("min_age_days", min_age_days.to_string())])
        .bearer_auth(&token)
        .send()
        .await
        .context("GET /lifecycle failed")?
        .error_for_status()
        .context("GET /lifecycle returned an error")?
        .json()
        .await
        .context("failed to parse lifecycle response")?;
    print!(
        "{}",
        render_lifecycle_report_stdout(&report, "DRY-RUN", None)
    );
    Ok(())
}

struct PromoteCliArgs {
    apply: bool,
    lesson_id: Option<String>,
    target: PromoteTargetArg,
    dest: Option<PathBuf>,
    min_confirmed: i64,
    min_confirm_rate: f64,
    max_contradicted: i64,
    kinds: String,
    include_promoted: bool,
    force: bool,
    limit: usize,
    data_dir: Option<PathBuf>,
}

fn parse_promote_kinds(kinds: &str) -> Result<Vec<String>> {
    let allowed = ["claim", "procedure", "episode", "directive", "context"];
    let parsed: Vec<String> = kinds
        .split(',')
        .map(|k| k.trim().to_ascii_lowercase())
        .filter(|k| !k.is_empty())
        .collect();
    if parsed.is_empty() {
        anyhow::bail!("--kinds must list at least one knowledge kind");
    }
    for kind in &parsed {
        if !allowed.contains(&kind.as_str()) {
            anyhow::bail!(
                "unknown promote kind '{kind}' (expected one of {})",
                allowed.join(", ")
            );
        }
    }
    Ok(parsed)
}

fn cmd_promote(args: PromoteCliArgs) -> Result<()> {
    let db_path = fieldcraft_db_path_for_command_or_data_dir("promote", args.data_dir.as_deref())?;
    if !db_path.exists() {
        anyhow::bail!("catalog not found at {}", db_path.display());
    }
    let repo = KnowledgeRepository::open(&db_path)
        .with_context(|| format!("opening knowledge repo at {}", db_path.display()))?;
    let output_root = db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let request = PromoteRequest {
        apply: args.apply,
        lesson_id: args.lesson_id,
        target: args.target.into(),
        dest: args.dest,
        output_root,
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        thresholds: PromoteThresholds {
            min_confirmed: args.min_confirmed,
            min_confirm_rate: args.min_confirm_rate,
            max_contradicted: args.max_contradicted,
            kinds: parse_promote_kinds(&args.kinds)?,
        },
        include_promoted: args.include_promoted,
        force: args.force,
        limit: args.limit,
    };
    let report = run_promote(&repo, &request)?;
    print!("{}", render_promote_report(&report));
    Ok(())
}

/// L5 dream-tier: cluster related confirmed lessons and (with --apply)
/// abstract each cluster into one provenance-gated, provisional derived Claim.
#[derive(Debug, Deserialize)]
struct DreamResponse {
    clusters: Vec<DreamClusterResponse>,
}

#[derive(Debug, Deserialize)]
struct DreamClusterResponse {
    project_id: String,
    already_dreamed: bool,
    members: Vec<DreamMemberResponse>,
}

#[derive(Debug, Deserialize)]
struct DreamMemberResponse {
    id: String,
    title: String,
    retrieval_text: String,
}

async fn cmd_dream(
    apply: bool,
    max_clusters: usize,
    llm_cmd: Option<&str>,
    data_dir: Option<&Path>,
) -> Result<()> {
    if data_dir.is_none()
        && let Some(server_url) = fieldcraft_server_url()
        && !is_loopback_fieldcraft_url(&server_url)
    {
        return cmd_dream_remote(apply, max_clusters, llm_cmd, &server_url).await;
    }

    let db_path = fieldcraft_db_path_for_command_or_data_dir("dream", data_dir)?;
    if !db_path.exists() {
        anyhow::bail!("catalog not found at {}", db_path.display());
    }
    let repo = KnowledgeRepository::open(&db_path)
        .with_context(|| format!("opening knowledge repo at {}", db_path.display()))?;
    let clusters = repo.dream_clusters(3, 2, max_clusters)?;
    if clusters.is_empty() {
        println!(
            "dream: no clusters with enough confirmed signal (need >=3 members, >=2 confirms)."
        );
        return Ok(());
    }
    let mode = if apply { "APPLY" } else { "DRY-RUN" };
    println!("dream [{mode}]: {} cluster(s)", clusters.len());

    for (ci, ids) in clusters.iter().enumerate() {
        let mut members = Vec::new();
        for id in ids {
            if let Ok(Some(rec)) = repo.get_knowledge(id) {
                members.push(rec);
            }
        }
        if members.len() < 3 {
            continue;
        }
        let project_id = members[0].project_id.clone();
        // Over-cap clusters must not abort the run: skip them so the
        // remaining clusters still get synthesized. insert_derived_claim
        // would refuse them anyway (seen live: a 90-member cluster).
        if members.len() > fieldcraft_storage::DERIVED_CLAIM_MAX_SOURCES {
            println!(
                "cluster {} ({} members, project {}): over the {}-source cap, skipped.",
                ci + 1,
                members.len(),
                project_id,
                fieldcraft_storage::DERIVED_CLAIM_MAX_SOURCES
            );
            continue;
        }
        let source_ids: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
        if repo.dream_cluster_already_dreamed(&source_ids)? {
            println!(
                "cluster {} ({} members, project {}): already dreamed, skipped.",
                ci + 1,
                members.len(),
                project_id
            );
            continue;
        }
        println!(
            "cluster {} ({} members, project {}):",
            ci + 1,
            members.len(),
            project_id
        );
        for m in &members {
            println!("  - {}: {}", &m.id[..12.min(m.id.len())], m.title);
        }
        if !apply {
            continue;
        }

        // Build the abstraction prompt: the LLM sees the member lessons and
        // must produce ONE general claim supported by ALL of them, as JSON.
        let mut prompt = String::from(
            "You are abstracting a durable engineering lesson from several related, \
             individually-confirmed lessons in the same project. Produce ONE general claim \
             that ALL of the lessons below genuinely support. Do NOT generalize beyond what \
             they establish; if they do not support a single common claim, output \
             {\"title\":\"\",\"statement\":\"\"}. Output ONLY one JSON object on one line: \
             {\"title\":\"<short title>\",\"statement\":\"<2-4 sentence claim>\"}.\n\nLessons:\n",
        );
        for m in &members {
            prompt.push_str(&format!("- {}\n  {}\n", m.title, m.retrieval_text));
        }

        let output = {
            let mut cmd = match llm_cmd {
                Some(custom) => {
                    let mut parts = custom.split_whitespace();
                    let program = parts.next().context("empty --llm-cmd")?;
                    let mut c = std::process::Command::new(program);
                    c.args(parts);
                    c
                }
                None => {
                    let mut c = std::process::Command::new("codex");
                    // --skip-git-repo-check: the dream pass runs from the data
                    // dir (often not a git repo) and codex exec hard-refuses
                    // untrusted non-repo cwds otherwise.
                    c.args(["exec", "--sandbox", "read-only", "--skip-git-repo-check"]);
                    c
                }
            };
            cmd.arg(&prompt);
            // No TTY: codex exec otherwise blocks reading "additional input"
            // from stdin.
            cmd.stdin(std::process::Stdio::null());
            let out = cmd.output().context("running dream LLM command")?;
            if !out.status.success() {
                eprintln!(
                    "  cluster {}: LLM command failed ({}): {}",
                    ci + 1,
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                        .lines()
                        .last()
                        .unwrap_or("")
                );
            }
            out
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Take the LAST parseable {"title","statement"} object in stdout so
        // LLM narration before the final answer is ignored.
        let parsed: Option<(String, String)> = stdout
            .lines()
            .rev()
            .filter_map(|line| {
                let line = line.trim();
                let start = line.find('{')?;
                serde_json::from_str::<serde_json::Value>(&line[start..]).ok()
            })
            .find_map(|v| {
                let title = v.get("title")?.as_str()?.trim().to_string();
                let statement = v.get("statement")?.as_str()?.trim().to_string();
                if title.is_empty() || statement.is_empty() {
                    None
                } else {
                    Some((title, statement))
                }
            });
        let Some((title, statement)) = parsed else {
            eprintln!(
                "  cluster {}: LLM produced no usable claim, skipped (guardrail).",
                ci + 1
            );
            continue;
        };
        let new_id = repo.insert_derived_claim(&source_ids, &project_id, &title, &statement)?;
        if let Ok(Some(rec)) = repo.get_knowledge(&new_id) {
            index_record(&rec).await;
        }
        println!(
            "  -> derived claim {} (provisional, cites {} sources): {}",
            &new_id[..12.min(new_id.len())],
            source_ids.len(),
            title
        );
    }
    if !apply {
        println!("dream: dry run only, nothing written. Re-run with --apply to abstract.");
    }
    Ok(())
}

async fn cmd_dream_remote(
    apply: bool,
    max_clusters: usize,
    llm_cmd: Option<&str>,
    server_url: &str,
) -> Result<()> {
    let token = get_or_enroll_token(server_url).await?;
    let client = reqwest::Client::new();
    let response: DreamResponse = client
        .get(format!("{server_url}/dream"))
        .query(&[("max_clusters", max_clusters.to_string())])
        .bearer_auth(&token)
        .send()
        .await
        .context("GET /dream failed")?
        .error_for_status()
        .context("GET /dream returned an error")?
        .json()
        .await
        .context("failed to parse dream response")?;

    if response.clusters.is_empty() {
        println!(
            "dream: no clusters with enough confirmed signal (need >=3 members, >=2 confirms)."
        );
        return Ok(());
    }
    let mode = if apply { "APPLY" } else { "DRY-RUN" };
    println!(
        "dream [{mode} remote {server_url}]: {} cluster(s)",
        response.clusters.len()
    );

    for (ci, cluster) in response.clusters.iter().enumerate() {
        if cluster.members.len() < 3 {
            continue;
        }
        if cluster.already_dreamed {
            println!(
                "cluster {} ({} members, project {}): already dreamed, skipped.",
                ci + 1,
                cluster.members.len(),
                cluster.project_id
            );
            continue;
        }
        println!(
            "cluster {} ({} members, project {}):",
            ci + 1,
            cluster.members.len(),
            cluster.project_id
        );
        for member in &cluster.members {
            println!(
                "  - {}: {}",
                &member.id[..12.min(member.id.len())],
                member.title
            );
        }
        if !apply {
            continue;
        }

        let mut prompt = String::from(
            "You are abstracting a durable engineering lesson from several related, \
             individually-confirmed lessons in the same project. Produce ONE general claim \
             that ALL of the lessons below genuinely support. Do NOT generalize beyond what \
             they establish; if they do not support a single common claim, output \
             {\"title\":\"\",\"statement\":\"\"}. Output ONLY one JSON object on one line: \
             {\"title\":\"<short title>\",\"statement\":\"<2-4 sentence claim>\"}.\n\nLessons:\n",
        );
        for member in &cluster.members {
            prompt.push_str(&format!(
                "- {}\n  {}\n",
                member.title, member.retrieval_text
            ));
        }

        let output = {
            let mut cmd = match llm_cmd {
                Some(custom) => {
                    let mut parts = custom.split_whitespace();
                    let program = parts.next().context("empty --llm-cmd")?;
                    let mut c = std::process::Command::new(program);
                    c.args(parts);
                    c
                }
                None => {
                    let mut c = std::process::Command::new("codex");
                    c.args(["exec", "--sandbox", "read-only", "--skip-git-repo-check"]);
                    c
                }
            };
            cmd.arg(&prompt);
            cmd.stdin(std::process::Stdio::null());
            let out = cmd.output().context("running dream LLM command")?;
            if !out.status.success() {
                eprintln!(
                    "  cluster {}: LLM command failed ({}): {}",
                    ci + 1,
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                        .lines()
                        .last()
                        .unwrap_or("")
                );
            }
            out
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: Option<(String, String)> = stdout
            .lines()
            .rev()
            .filter_map(|line| {
                let line = line.trim();
                let start = line.find('{')?;
                serde_json::from_str::<serde_json::Value>(&line[start..]).ok()
            })
            .find_map(|v| {
                let title = v.get("title")?.as_str()?.trim().to_string();
                let statement = v.get("statement")?.as_str()?.trim().to_string();
                if title.is_empty() || statement.is_empty() {
                    None
                } else {
                    Some((title, statement))
                }
            });
        let Some((title, statement)) = parsed else {
            eprintln!(
                "  cluster {}: LLM produced no usable claim, skipped (guardrail).",
                ci + 1
            );
            continue;
        };

        let source_ids: Vec<String> = cluster
            .members
            .iter()
            .map(|member| member.id.clone())
            .collect();
        let result: serde_json::Value = client
            .post(format!("{server_url}/dream"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "source_ids": source_ids,
                "project_id": cluster.project_id,
                "title": title,
                "statement": statement,
            }))
            .send()
            .await
            .context("POST /dream failed")?
            .error_for_status()
            .context("POST /dream returned an error")?
            .json()
            .await
            .context("failed to parse derived claim response")?;
        let new_id = result["id"].as_str().unwrap_or("<missing id>");
        println!(
            "  -> derived claim {} (provisional, cites {} sources): {}",
            &new_id[..12.min(new_id.len())],
            source_ids.len(),
            title
        );
    }
    if !apply {
        println!("dream: dry run only, nothing written. Re-run with --apply to abstract.");
    }
    Ok(())
}

async fn cmd_analyze(
    days: u32,
    format: AnalyzeFormat,
    data_dir: Option<&Path>,
    limit: usize,
) -> Result<()> {
    if data_dir.is_none()
        && let Some(server_url) = fieldcraft_server_url()
        && !is_loopback_fieldcraft_url(&server_url)
    {
        return cmd_analyze_remote(days, format, limit, &server_url).await;
    }

    let db_path = fieldcraft_db_path_for_command_or_data_dir("analyze", data_dir)?;
    if !db_path.exists() {
        anyhow::bail!("catalog not found at {}", db_path.display());
    }
    let repo = KnowledgeRepository::open(&db_path)
        .with_context(|| format!("opening knowledge repo at {}", db_path.display()))?;
    let report = analyze_report(&repo, days, limit)?;

    match format {
        AnalyzeFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        AnalyzeFormat::Stdout => print!("{}", render_analysis_report_stdout(&report)),
    }

    Ok(())
}

async fn cmd_analyze_remote(
    days: u32,
    format: AnalyzeFormat,
    limit: usize,
    server_url: &str,
) -> Result<()> {
    let token = get_or_enroll_token(server_url).await?;
    let report: fieldcraft_storage::AnalysisReport = reqwest::Client::new()
        .get(format!("{server_url}/analyze"))
        .query(&[("days", days.to_string()), ("limit", limit.to_string())])
        .bearer_auth(&token)
        .send()
        .await
        .context("GET /analyze failed")?
        .error_for_status()
        .context("GET /analyze returned an error")?
        .json()
        .await
        .context("failed to parse analyze response")?;
    match format {
        AnalyzeFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        AnalyzeFormat::Stdout => print!("{}", render_analysis_report_stdout(&report)),
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

fn cmd_update(
    repo: Option<PathBuf>,
    local: bool,
    server_url: Option<String>,
    auth_token: Option<String>,
    label: Option<String>,
) -> Result<()> {
    let repo = repo.unwrap_or_else(default_fieldcraft_repo);
    let install_script = repo.join("install.sh");
    if !install_script.exists() {
        anyhow::bail!(
            "install.sh not found at {}. Pass --repo PATH for the Fieldcraft checkout.",
            install_script.display()
        );
    }

    println!("Updating Fieldcraft checkout at {}", repo.display());
    run_checked(
        ProcessCommand::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("pull")
            .arg("--ff-only"),
    )
    .context("git pull --ff-only failed")?;

    let env_file = default_fieldcraft_env_file();
    let persisted_upstream_url = read_env_file_value(&env_file, "FIELDCRAFT_UPSTREAM_URL")?
        .filter(|url| !is_loopback_fieldcraft_url(url));
    let persisted_server_url = read_env_file_value(&env_file, "FIELDCRAFT_SERVER_URL")?
        .filter(|url| !is_loopback_fieldcraft_url(url));
    let persisted_auth_token = read_env_file_value(&env_file, "FIELDCRAFT_AUTH_TOKEN")?;

    let selected_server_url = if local {
        None
    } else {
        server_url
            .or(persisted_upstream_url)
            .or(persisted_server_url)
    };
    let selected_auth_token = auth_token.or(persisted_auth_token);

    println!("Reinstalling Fieldcraft binaries and integration config");
    let mut install = ProcessCommand::new("bash");
    install.arg(&install_script).arg("--yes");
    if let Some(url) = selected_server_url {
        install.arg("--server-url").arg(url);
        if let Some(token) = selected_auth_token {
            install.arg("--auth-token").arg(token);
        }
        if let Some(label) = label {
            install.arg("--label").arg(label);
        }
    }
    run_checked(&mut install).context("install.sh failed")?;

    println!();
    println!("Update complete.");
    println!("Restart Codex/Claude so new MCP subprocesses use the updated binary.");
    println!(
        "If the shared HTTP fieldcraft-server runs on another Mac, update and restart that server there too."
    );
    Ok(())
}

fn default_fieldcraft_repo() -> PathBuf {
    if let Ok(repo) = std::env::var("FIELDCRAFT_REPO") {
        return PathBuf::from(repo);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join("Code").join("fieldcraft")
}

fn default_fieldcraft_env_file() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".fieldcraft").join("env")
}

fn is_loopback_fieldcraft_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url.trim()) else {
        return false;
    };
    parsed
        .host_str()
        .map(|host| host.trim_matches(&['[', ']'][..]))
        .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "::1"))
}

fn read_env_file_value(path: &Path, key: &str) -> Result<Option<String>> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(None);
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return Ok(Some(strip_shell_quotes(v.trim()).to_string()));
        }
    }
    Ok(None)
}

fn strip_shell_quotes(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value)
}

fn run_checked(cmd: &mut ProcessCommand) -> Result<()> {
    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to start {:?}", cmd))?;
    if !status.success() {
        anyhow::bail!("command exited with {status}: {:?}", cmd);
    }
    Ok(())
}
