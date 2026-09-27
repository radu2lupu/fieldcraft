//! fieldcraft-server entry point.
//!
//! Binds to 0.0.0.0:7777 (or FIELDCRAFT_SERVER_PORT). Opens three sqlite catalogs:
//!   - memory.sqlite3      (KnowledgeRepository + MemoryRepository)
//!   - .tr-source-sessions.sqlite3 (source sessions)
//!   - tokens.sqlite3      (auth tokens)
//!
//! Optional: --scan-local-sessions scans ~/.codex and ~/.claude at startup
//! and every 30 minutes, inserting new sessions into source_sessions.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use fieldcraft_server::{
    AppState, ProxyState,
    auth::{EnrollPolicy, TokenStore},
    lifecycle_loop, merge_queue_loop, proxy_router, router,
};

#[derive(Parser, Debug)]
#[command(name = "fieldcraft-server", about = "fieldcraft HTTP server")]
struct Args {
    /// Directory for all sqlite databases. Defaults to ~/.fieldcraft
    #[arg(long, env = "FIELDCRAFT_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Port to listen on. Defaults to 7777.
    #[arg(long, env = "FIELDCRAFT_SERVER_PORT", default_value = "7777")]
    port: u16,

    /// Scan ~/.codex and ~/.claude at startup and every 30 minutes.
    #[arg(long, default_value = "false")]
    scan_local_sessions: bool,

    /// Upstream fieldcraft-server URL. When set, this process runs as a local
    /// gateway/proxy and does not open SQLite catalogs.
    #[arg(long, env = "FIELDCRAFT_UPSTREAM_URL")]
    upstream_url: Option<String>,

    /// Bearer token for the upstream server. Optional: if omitted, incoming
    /// Authorization headers are forwarded unchanged.
    #[arg(long, env = "FIELDCRAFT_UPSTREAM_AUTH_TOKEN")]
    upstream_auth_token: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialise structured logging (respects RUST_LOG env var).
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    if let Some(upstream_url) = args.upstream_url.as_deref() {
        let state = ProxyState {
            upstream_url: upstream_url.trim_end_matches('/').to_string(),
            upstream_auth_token: args.upstream_auth_token.clone(),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(60))
                .build()?,
        };
        let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
        info!(%addr, upstream_url = %state.upstream_url, "fieldcraft gateway listening");
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, proxy_router(state).into_make_service()).await?;
        return Ok(());
    }

    let data_dir = args.data_dir.unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".fieldcraft")
    });

    std::fs::create_dir_all(&data_dir)?;

    // Secrets live next to the data, not in the launchd plist: installers
    // rewrite plists and once dropped the embedding key for weeks unnoticed.
    let secrets_path = data_dir.join(fieldcraft_core::secrets::SECRETS_FILE_NAME);
    fieldcraft_core::secrets::set_file_path(secrets_path.clone());
    match fieldcraft_core::secrets::lookup_any(&["GEMINI_API_KEY", "GOOGLE_API_KEY"]) {
        Some((_, source)) => info!(?source, "embeddings: gemini api key configured"),
        None => tracing::warn!(
            path = %secrets_path.display(),
            "NO EMBEDDING KEY: set GEMINI_API_KEY in the environment or in the secrets file; \
             retrieval degrades to BM25 and merge, resurrection and dream stay inert"
        ),
    }

    // Open three sqlite catalogs.
    let knowledge_path = data_dir.join("memory.sqlite3");
    let tokens_path = data_dir.join("tokens.sqlite3");

    let repo = Arc::new(Mutex::new(fieldcraft_storage::KnowledgeRepository::open(
        &knowledge_path,
    )?));
    info!(path = %knowledge_path.display(), "knowledge catalog opened");
    let memories = Arc::new(Mutex::new(fieldcraft_storage::MemoryRepository::open(
        &knowledge_path,
    )?));
    info!(path = %knowledge_path.display(), "memories catalog opened");

    let sessions_conn = fieldcraft_storage::open_source_sessions_catalog(&data_dir)?;
    info!(path = %data_dir.display(), "source sessions catalog opened");

    let tokens = Arc::new(TokenStore::open(&tokens_path)?);
    info!(path = %tokens_path.display(), "token store opened");

    let state = AppState {
        repo,
        memories,
        source_sessions: Arc::new(Mutex::new(sessions_conn)),
        tokens,
        db_path: knowledge_path,
        enroll_policy: EnrollPolicy::from_env(),
    };

    // Optional background scan of local session directories.
    if args.scan_local_sessions {
        let scan_state = state.clone();
        tokio::spawn(async move {
            scan_local_sessions_loop(scan_state).await;
        });
    }

    let merge_state = state.clone();
    tokio::spawn(async move {
        merge_queue_loop(merge_state).await;
    });

    // Memory Lifecycle maintenance (L2/L4 eviction + RIF). Applies only when
    // FIELDCRAFT_LIFECYCLE_APPLY=1; otherwise logs candidate counts.
    let lifecycle_state = state.clone();
    tokio::spawn(async move {
        lifecycle_loop(lifecycle_state).await;
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    info!(%addr, "fieldcraft-server listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    // `into_make_service_with_connect_info::<SocketAddr>` exposes peer IPs to
    // tower_governor's PeerIpKeyExtractor for per-IP rate limiting on /enroll.
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

/// Walk ~/.codex and ~/.claude every 30 minutes, inserting new sessions.
///
/// INVARIANT: Sessions inserted here are raw source material.
/// Any future distillation pipeline that promotes these sessions into
/// `knowledge` records MUST set `tier = Archive` on the resulting records.
/// This is deliberate: source sessions are never auto-injected into
/// coding context without an explicit distillation step.
async fn scan_local_sessions_loop(state: AppState) {
    use std::time::Duration;
    loop {
        scan_local_sessions_once(&state);
        tokio::time::sleep(Duration::from_secs(30 * 60)).await;
    }
}

fn scan_local_sessions_once(state: &AppState) {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let scan_dirs = vec![
        PathBuf::from(&home).join(".codex"),
        PathBuf::from(&home).join(".claude"),
    ];

    let conn = match state.source_sessions.lock() {
        Ok(c) => c,
        Err(_) => {
            tracing::warn!("source_sessions mutex poisoned, skipping scan");
            return;
        }
    };

    for dir in &scan_dirs {
        if !dir.exists() {
            continue;
        }
        // Walk looking for session JSON files.
        for entry in walkdir::WalkDir::new(dir)
            .max_depth(5)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|x| x == "json" || x == "jsonl")
                    .unwrap_or(false)
            })
        {
            let path = entry.path();
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Compute fingerprint from path + content.
            let fingerprint = {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(path.to_string_lossy().as_bytes());
                h.update(content.as_bytes());
                format!("{:x}", h.finalize())
            };

            // Check if already present via fingerprint.
            let already: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM source_sessions WHERE source_fingerprint = ?1",
                    rusqlite::params![fingerprint],
                    |r| r.get(0),
                )
                .unwrap_or(0);

            if already > 0 {
                continue;
            }

            // Derive source_id.
            let source_id = {
                use sha2::{Digest, Sha256};
                let tool = if path.to_string_lossy().contains(".codex") {
                    "codex"
                } else {
                    "claude"
                };
                let mut h = Sha256::new();
                h.update(format!("{}|{}", tool, fingerprint).as_bytes());
                format!("{:x}", h.finalize())[..24].to_string()
            };

            let tool = if path.to_string_lossy().contains(".codex") {
                "codex"
            } else {
                "claude"
            };

            let now = chrono::Utc::now().to_rfc3339();
            let path_str = path.to_string_lossy().to_string();

            let _ = conn.execute(
                "INSERT OR IGNORE INTO source_sessions
                 (source_id, source_tool, source_path, session_id, project, machine, cwd,
                  title, started_at, updated_at, discovered_at, source_fingerprint,
                  transcript_json, status, attempts, extractor_version, last_error, last_extracted_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                rusqlite::params![
                    source_id,
                    tool,
                    path_str,
                    "",          // session_id — filled by extractor
                    "unknown",   // project
                    "",          // machine
                    None::<String>, // cwd
                    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    None::<String>, // started_at
                    None::<String>, // updated_at
                    now,
                    fingerprint,
                    "[]",        // transcript_json — raw content not yet parsed
                    "pending",
                    0i64,
                    "fieldcraft-server-scanner-v1",
                    None::<String>,
                    None::<String>,
                ],
            );
        }
    }
}
