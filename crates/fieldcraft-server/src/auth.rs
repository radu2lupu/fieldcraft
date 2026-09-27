//! Bearer token auth for fieldcraft HTTP server.
//!
//! Security invariants:
//! - Plaintext tokens are NEVER stored or logged.
//! - Only sha256(plaintext) is persisted in tokens.sqlite3.
//! - Logs reference only the first 8 chars of the sha256 hash.
//! - Max 64 active tokens; LRU eviction by last_used_at.

use chrono::Utc;
use rand::RngCore;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Mutex;
use tracing::{debug, info};

use crate::error::AppError;

const MAX_TOKENS: i64 = 64;

/// Thread-safe token store backed by a SQLite database.
///
/// `rusqlite::Connection` is `Send` but not `Sync` (it wraps `RefCell`
/// internally), so we protect it with a `Mutex`. This makes `TokenStore`
/// both `Send` and `Sync`, which is required for use inside `Arc` as axum
/// state.
pub struct TokenStore {
    conn: Mutex<Connection>,
}

impl TokenStore {
    /// Open (or create) the tokens catalog at the given path.
    /// Applies WAL + busy_timeout pragmas and creates the schema.
    pub fn open(path: &Path) -> Result<Self, AppError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AppError::Storage(e.to_string()))?;
        }
        let conn = Connection::open(path).map_err(|e| AppError::Storage(e.to_string()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS tr_tokens (
                 token_hash TEXT PRIMARY KEY,
                 label TEXT,
                 created_at TEXT NOT NULL,
                 last_used_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS tr_tokens_last_used_idx ON tr_tokens(last_used_at);",
        )
        .map_err(|e| AppError::Storage(e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Mint a new token. Returns the plaintext hex token (the ONLY time it is returned).
    /// Stores only the sha256 hash in the database.
    /// When token count would exceed MAX_TOKENS after insert, evicts the LRU token first.
    pub fn mint(&self, label: Option<&str>) -> Result<String, AppError> {
        let mut raw = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut raw);
        let plaintext = hex::encode(raw);

        let hash = sha256_hex(&plaintext);
        let now = Utc::now().to_rfc3339();

        // Log only the first 8 chars of the hash — never the plaintext.
        let hash_prefix = &hash[..8];
        debug!(token_hash_prefix = %hash_prefix, "minting new token");

        let conn = self
            .conn
            .lock()
            .map_err(|_| AppError::Storage("token store mutex poisoned".into()))?;

        // LRU eviction: if we're at the limit, delete the oldest token.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tr_tokens", [], |r| r.get(0))
            .map_err(|e| AppError::Storage(e.to_string()))?;
        if count >= MAX_TOKENS {
            conn.execute(
                "DELETE FROM tr_tokens WHERE token_hash = (
                     SELECT token_hash FROM tr_tokens ORDER BY last_used_at ASC LIMIT 1
                 )",
                [],
            )
            .map_err(|e| AppError::Storage(e.to_string()))?;
            info!("evicted LRU token (count was {})", count);
        }

        conn.execute(
            "INSERT INTO tr_tokens (token_hash, label, created_at, last_used_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![hash, label, now, now],
        )
        .map_err(|e| AppError::Storage(e.to_string()))?;

        info!(token_hash_prefix = %hash_prefix, "token minted");
        Ok(plaintext)
    }

    /// Verify a bearer token and return its label if valid.
    /// Returns `Some(label)` if valid (label may be empty string if no label was set),
    /// `None` if the token is not found. Updates last_used_at on success.
    /// NEVER logs the plaintext bearer.
    pub fn verify_with_label(&self, bearer: &str) -> Result<Option<String>, AppError> {
        let hash = sha256_hex(bearer);
        let conn = self
            .conn
            .lock()
            .map_err(|_| AppError::Storage("token store mutex poisoned".into()))?;
        // Read the label as nullable: a token minted without a label stores SQL NULL,
        // and reading that into a non-Option String errors ("Invalid column type Null"),
        // which previously made every authenticated request 500.
        let result: Option<Option<String>> = conn
            .query_row(
                "SELECT label FROM tr_tokens WHERE token_hash = ?1",
                params![hash],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional_()
            .map_err(|e| AppError::Storage(e.to_string()))?;
        // Also update last_used_at if found.
        if result.is_some() {
            let now = Utc::now().to_rfc3339();
            let _ = conn.execute(
                "UPDATE tr_tokens SET last_used_at = ?1 WHERE token_hash = ?2",
                params![now, hash],
            );
        }
        // Token found but with a NULL label is still valid → empty-string label.
        Ok(result.map(|label| label.unwrap_or_default()))
    }

    /// Verify a bearer token. Returns true if valid, and updates last_used_at.
    /// NEVER logs the plaintext bearer.
    pub fn verify(&self, bearer: &str) -> Result<bool, AppError> {
        let hash = sha256_hex(bearer);
        let hash_prefix = &hash[..8];

        let conn = self
            .conn
            .lock()
            .map_err(|_| AppError::Storage("token store mutex poisoned".into()))?;

        let found: Option<String> = conn
            .query_row(
                "SELECT token_hash FROM tr_tokens WHERE token_hash = ?1",
                params![hash],
                |r| r.get(0),
            )
            .optional_()
            .map_err(|e| AppError::Storage(e.to_string()))?;

        if found.is_some() {
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE tr_tokens SET last_used_at = ?1 WHERE token_hash = ?2",
                params![now, hash],
            )
            .map_err(|e| AppError::Storage(e.to_string()))?;
            debug!(token_hash_prefix = %hash_prefix, "token verified ok");
            Ok(true)
        } else {
            debug!(token_hash_prefix = %hash_prefix, "token not found");
            Ok(false)
        }
    }
}

/// Compute sha256 of a string and return lowercase hex.
pub(crate) fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Extension trait for rusqlite to handle `optional` returns without polluting callers.
trait OptionalExt<T> {
    fn optional_(self) -> rusqlite::Result<Option<T>>;
}

impl<T> OptionalExt<T> for rusqlite::Result<T> {
    fn optional_(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Extract a valid bearer token from an Authorization header value.
/// Accepts "Bearer <token>" (case-insensitive prefix).
pub fn extract_bearer(header_value: &str) -> Option<&str> {
    let stripped = header_value.trim();
    if stripped.len() > 7 && stripped[..7].eq_ignore_ascii_case("bearer ") {
        Some(stripped[7..].trim())
    } else {
        None
    }
}

/// Header carrying the shared enrollment secret on `POST /enroll`.
pub const ENROLL_SECRET_HEADER: &str = "x-fieldcraft-enroll-secret";

/// Who may mint a token via `POST /enroll`.
///
/// The server binds `0.0.0.0`, so an open `/enroll` would let anyone on the
/// same network mint a token (and, via LRU eviction, push out real ones).
/// - `FIELDCRAFT_ENROLL_OPEN=1`: anyone may enroll (fully trusted network only).
/// - `FIELDCRAFT_ENROLL_SECRET` set: every caller must present it, including
///   loopback, because a reverse proxy makes remote callers look local.
/// - Neither set: only loopback callers may enroll (local installs).
#[derive(Clone, Debug, Default)]
pub struct EnrollPolicy {
    pub secret: Option<String>,
    pub open: bool,
}

impl EnrollPolicy {
    pub fn from_env() -> Self {
        // Environment first, then ~/.fieldcraft/secrets.env (one place for all secrets).
        let secret = fieldcraft_core::secrets::lookup("FIELDCRAFT_ENROLL_SECRET");
        let open = std::env::var("FIELDCRAFT_ENROLL_OPEN")
            .is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes"));
        Self { secret, open }
    }

    pub fn allows(&self, peer: std::net::IpAddr, presented_secret: Option<&str>) -> bool {
        if self.open {
            return true;
        }
        match self.secret.as_deref() {
            // Compare digests so the check does not short-circuit on the secret's bytes.
            Some(expected) => presented_secret
                .is_some_and(|given| sha256_hex(given.trim()) == sha256_hex(expected)),
            None => peer.to_canonical().is_loopback(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn open_test_store() -> (TokenStore, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let store = TokenStore::open(&dir.path().join("tokens.sqlite3")).unwrap();
        (store, dir)
    }

    #[test]
    fn mint_verify_roundtrip() {
        let (store, _dir) = open_test_store();
        let token = store.mint(Some("test")).unwrap();
        assert!(
            store.verify(&token).unwrap(),
            "freshly minted token should verify"
        );
    }

    #[test]
    fn verify_rejects_unknown_token() {
        let (store, _dir) = open_test_store();
        assert!(!store.verify("not-a-real-token").unwrap());
    }

    #[test]
    fn verify_with_label_handles_null_label() {
        // Regression: a token minted without a label stores SQL NULL. Reading it must
        // not error — previously verify_with_label 500'd on "Invalid column type Null",
        // breaking every authenticated request for label-less tokens (plain /enroll).
        let (store, _dir) = open_test_store();
        let token = store.mint(None).unwrap();
        assert!(
            store.verify(&token).unwrap(),
            "label-less token should verify"
        );
        assert_eq!(
            store.verify_with_label(&token).unwrap(),
            Some(String::new()),
            "NULL label must resolve to empty string, not error"
        );
    }

    #[test]
    fn lru_eviction_at_65th_token() {
        let dir = tempdir().unwrap();
        let store = TokenStore::open(&dir.path().join("tokens.sqlite3")).unwrap();
        // Mint 64 tokens without touching any
        let first_64: Vec<String> = (0..64)
            .map(|i| store.mint(Some(&format!("s{i}"))).unwrap())
            .collect();

        {
            let conn = store.conn.lock().unwrap();
            let count_before: i64 = conn
                .query_row("SELECT COUNT(*) FROM tr_tokens", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count_before, 64);
        }

        // Mint 65th — should evict the oldest (first minted)
        let _new_token = store.mint(Some("new")).unwrap();

        let first_hash = sha256_hex(&first_64[0]);
        let conn = store.conn.lock().unwrap();
        let count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM tr_tokens", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count_after, 64,
            "count should stay at 64 after LRU eviction"
        );

        let still_present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tr_tokens WHERE token_hash = ?1",
                params![first_hash],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still_present, 0, "LRU token should have been evicted");
    }

    #[test]
    fn extract_bearer_valid() {
        assert_eq!(extract_bearer("Bearer abc123"), Some("abc123"));
        assert_eq!(extract_bearer("bearer abc123"), Some("abc123"));
        assert_eq!(extract_bearer("BEARER abc123"), Some("abc123"));
    }

    #[test]
    fn extract_bearer_invalid() {
        assert_eq!(extract_bearer("Basic abc"), None);
        assert_eq!(extract_bearer(""), None);
    }

    #[test]
    fn enroll_policy_default_is_loopback_only() {
        let policy = EnrollPolicy::default();
        assert!(policy.allows("127.0.0.1".parse().unwrap(), None));
        assert!(policy.allows("::1".parse().unwrap(), None));
        assert!(policy.allows("::ffff:127.0.0.1".parse().unwrap(), None));
        assert!(!policy.allows("100.64.0.7".parse().unwrap(), None));
        assert!(!policy.allows("192.168.1.20".parse().unwrap(), Some("guess")));
    }

    #[test]
    fn enroll_policy_secret_and_open_modes() {
        let with_secret = EnrollPolicy {
            secret: Some("s3cret".into()),
            open: false,
        };
        assert!(with_secret.allows("100.64.0.7".parse().unwrap(), Some("s3cret")));
        assert!(!with_secret.allows("100.64.0.7".parse().unwrap(), Some("wrong")));
        assert!(!with_secret.allows("127.0.0.1".parse().unwrap(), None));

        let open = EnrollPolicy {
            secret: None,
            open: true,
        };
        assert!(open.allows("203.0.113.9".parse().unwrap(), None));
    }
}
