//! In-memory record index for the query path.
//!
//! Dense search used to read every active row (text, payload and vector) from
//! SQLite on each query, and identity admission read every record without an
//! identity cue (with its text): together ~250 ms per query on the 2026-11
//! production store, under the server's single mutex. The cache keeps, per
//! active or provisional record, only what those scans decide on (id, tier,
//! state, origin, project, identity-cue flag, vector), in the order the old
//! scans produced (state, then rowid), so results are identical. Freshness
//! comes from `knowledge_vector_changes`, a log that triggers append to on
//! every write touching those fields (see `fieldcraft_storage::record_index`);
//! each query applies the entries logged since the last one it saw.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use fieldcraft_core::{KnowledgeOrigin, TrError};
use fieldcraft_storage::{KnowledgeRepository, RecordIndexRow};

use crate::internal;

/// Position in the old scan order: state (`active` < `provisional`), rowid.
type ScanKey = (u8, i64);

#[derive(Debug, Clone)]
pub(crate) struct CachedRecord {
    pub id: String,
    pub rowid: i64,
    pub tier: Option<String>,
    pub origin: KnowledgeOrigin,
    pub superseded: bool,
    pub project_id: String,
    pub has_identity_cue: bool,
    pub vector: Option<Vec<f32>>,
}

#[derive(Debug, Default)]
pub(crate) struct RecordCache {
    entries: BTreeMap<ScanKey, CachedRecord>,
    key_of: HashMap<String, ScanKey>,
    last_seq: i64,
    loaded: bool,
}

fn state_rank(state: &str) -> u8 {
    match state {
        "active" => 0,
        _ => 1,
    }
}

fn origin_of(source_json: &str) -> KnowledgeOrigin {
    KnowledgeOrigin::from_source(&crate::sqlite_search::parse_source(source_json))
}

impl RecordCache {
    fn insert_row(&mut self, row: RecordIndexRow) {
        let key = (state_rank(&row.state), row.rowid);
        self.remove(&row.id);
        self.key_of.insert(row.id.clone(), key);
        self.entries.insert(
            key,
            CachedRecord {
                origin: origin_of(&row.source_json),
                superseded: !row.superseded_by_is_null,
                id: row.id,
                rowid: row.rowid,
                tier: row.tier,
                project_id: row.project_id,
                has_identity_cue: row.has_identity_cue,
                vector: row.vector,
            },
        );
    }

    fn remove(&mut self, id: &str) {
        if let Some(key) = self.key_of.remove(id) {
            self.entries.remove(&key);
        }
    }

    fn rebuild(&mut self, repo: &KnowledgeRepository) -> Result<(), TrError> {
        // Read the sequence first: anything committed after it is replayed on
        // the next refresh, and replaying an already-applied change is harmless.
        let seq = repo
            .max_record_change_seq()
            .map_err(|e| internal(format!("record cache seq: {e}")))?;
        let rows = repo
            .load_record_index_rows()
            .map_err(|e| internal(format!("record cache load: {e}")))?;
        self.entries.clear();
        self.key_of.clear();
        for row in rows {
            self.insert_row(row);
        }
        self.last_seq = seq;
        self.loaded = true;
        Ok(())
    }

    fn apply_changes(&mut self, repo: &KnowledgeRepository) -> Result<(), TrError> {
        let min_seq = repo
            .min_record_change_seq()
            .map_err(|e| internal(format!("record cache min seq: {e}")))?;
        if min_seq > self.last_seq + 1 {
            // The log was pruned past us; we cannot know what we missed.
            return self.rebuild(repo);
        }
        let (ids, max_seq) = repo
            .record_changes_since(self.last_seq)
            .map_err(|e| internal(format!("record cache changes: {e}")))?;
        if ids.is_empty() {
            self.last_seq = max_seq;
            return Ok(());
        }
        for id in &ids {
            self.remove(id);
        }
        let rows = repo
            .load_record_index_rows_for(&ids)
            .map_err(|e| internal(format!("record cache reload: {e}")))?;
        for row in rows {
            self.insert_row(row);
        }
        self.last_seq = max_seq;
        Ok(())
    }

    fn is_current(&self, max_seq: i64) -> bool {
        self.loaded && self.last_seq == max_seq
    }

    /// Entries in the old scan order.
    pub fn iter(&self) -> impl Iterator<Item = &CachedRecord> {
        self.entries.values()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

type SharedCache = Arc<RwLock<RecordCache>>;
/// (catalog file, vector tag)
type CacheKey = (PathBuf, Option<String>);

/// One cache per (catalog file, vector tag), shared by every connection in the
/// process that reads that tag.
fn registry() -> &'static Mutex<HashMap<CacheKey, SharedCache>> {
    static REGISTRY: OnceLock<Mutex<HashMap<CacheKey, SharedCache>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_for(repo: &KnowledgeRepository) -> SharedCache {
    match repo.connection().path().filter(|p| !p.is_empty()) {
        Some(path) => {
            let key = (PathBuf::from(path), repo.vector_tag().map(str::to_string));
            let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
            map.entry(key).or_default().clone()
        }
        // In-memory catalogs cannot be shared; use a throwaway cache.
        None => SharedCache::default(),
    }
}

/// Bring the catalog's cache up to date through `repo`, then run `f` on it
/// under a read lock.
pub(crate) fn with_record_cache<R>(
    repo: &KnowledgeRepository,
    f: impl FnOnce(&RecordCache) -> R,
) -> Result<R, TrError> {
    let shared = cache_for(repo);
    let max_seq = repo
        .max_record_change_seq()
        .map_err(|e| internal(format!("record cache seq: {e}")))?;
    let current = shared
        .read()
        .map(|cache| cache.is_current(max_seq))
        .unwrap_or(false);
    if !current {
        let mut cache = shared.write().unwrap_or_else(|e| e.into_inner());
        if !cache.is_current(max_seq) {
            if cache.loaded {
                cache.apply_changes(repo)?;
            } else {
                cache.rebuild(repo)?;
            }
        }
    }
    let cache = shared.read().unwrap_or_else(|e| e.into_inner());
    Ok(f(&cache))
}

/// Records without any identity cue, as `(id, project_id)` in scan order,
/// for legacy identity admission (see `identity_cue_search`).
pub(crate) fn legacy_identity_candidates(
    repo: &KnowledgeRepository,
    tier: &str,
) -> Result<Vec<(String, String)>, TrError> {
    with_record_cache(repo, |cache| {
        cache
            .iter()
            .filter(|entry| !entry.has_identity_cue)
            .filter(|entry| entry.tier.as_deref().is_none_or(|t| t == tier))
            .map(|entry| (entry.id.clone(), entry.project_id.clone()))
            .collect()
    })
}

/// Archived (plain, not superseded) records whose vector is at least
/// `min_cosine` from `query_dense`, best first. Drives L3 resurrection; reads
/// the cache instead of decoding every archive vector from SQLite.
pub fn archive_matches(
    repo: &KnowledgeRepository,
    query_dense: &[f32],
    min_cosine: f32,
) -> Result<Vec<(String, f32)>, TrError> {
    with_record_cache(repo, |cache| {
        let mut hits: Vec<(i64, String, f32)> = cache
            .iter()
            .filter(|entry| entry.tier.as_deref() == Some("archive") && !entry.superseded)
            .filter_map(|entry| {
                let vector = entry.vector.as_ref()?;
                fieldcraft_storage::repository::cosine_similarity(query_dense, vector)
                    .filter(|sim| *sim >= min_cosine)
                    .map(|sim| (entry.rowid, entry.id.clone(), sim))
            })
            .collect();
        hits.sort_by(|a, b| {
            b.2.partial_cmp(&a.2)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        hits.into_iter().map(|(_, id, sim)| (id, sim)).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fieldcraft_core::{
        AgentTool, KnowledgeKind, KnowledgeRecord, KnowledgeSource, KnowledgeState, KnowledgeTier,
    };
    use tempfile::tempdir;

    fn record(id: &str) -> KnowledgeRecord {
        KnowledgeRecord {
            id: id.to_string(),
            project_id: "p".to_string(),
            kind: KnowledgeKind::Claim,
            state: KnowledgeState::Active,
            title: format!("title {id}"),
            payload_json: r#"{"statement":"s"}"#.to_string(),
            confidence: 0.5,
            source: KnowledgeSource::Session {
                session_id: "s".into(),
                tool: AgentTool::ClaudeCode,
            },
            superseded_by: None,
            project_specific: false,
            created_at: chrono::Utc::now(),
            valid_until: None,
            retrieval_text: format!("text {id}"),
            schema_version: 4,
            phase: None,
            tier: KnowledgeTier::Active,
            owner: None,
            session_date: None,
            source_refs: vec![],
            protected: false,
        }
    }

    fn ids(repo: &KnowledgeRepository) -> Vec<String> {
        with_record_cache(repo, |c| {
            c.iter()
                .filter(|e| e.vector.is_some())
                .map(|e| e.id.clone())
                .collect()
        })
        .unwrap()
    }

    #[test]
    fn cache_sees_writes_from_another_connection() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let writer = KnowledgeRepository::open(&path).unwrap();
        let reader = KnowledgeRepository::open_reader(&path).unwrap();
        writer.insert_knowledge(&record("a")).unwrap();
        writer.store_dense_vector("a", &[1.0, 0.0]).unwrap();
        assert_eq!(ids(&reader), vec!["a"]);

        writer.insert_knowledge(&record("b")).unwrap();
        assert_eq!(ids(&reader), vec!["a"], "no vector yet");
        writer.store_dense_vector("b", &[0.0, 1.0]).unwrap();
        assert_eq!(ids(&reader), vec!["a", "b"]);

        writer.archive_record("a", "test").unwrap();
        let tiers: Vec<Option<String>> =
            with_record_cache(&reader, |c| c.iter().map(|e| e.tier.clone()).collect()).unwrap();
        assert_eq!(tiers, vec![Some("archive".into()), Some("active".into())]);
        assert_eq!(
            archive_matches(&reader, &[1.0, 0.0], 0.5).unwrap(),
            vec![("a".to_string(), 1.0)]
        );

        writer
            .connection()
            .execute(
                "UPDATE knowledge SET state = 'superseded' WHERE id = 'b'",
                [],
            )
            .unwrap();
        assert_eq!(ids(&reader), vec!["a"]);
        writer.delete_knowledge("a").unwrap();
        assert!(ids(&reader).is_empty());
    }

    #[test]
    fn pruned_log_forces_rebuild() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let repo = KnowledgeRepository::open(&path).unwrap();
        repo.insert_knowledge(&record("a")).unwrap();
        repo.store_dense_vector("a", &[1.0]).unwrap();
        assert_eq!(ids(&repo).len(), 1);
        for id in ["b", "c", "d"] {
            repo.insert_knowledge(&record(id)).unwrap();
            repo.store_dense_vector(id, &[1.0]).unwrap();
        }
        repo.prune_record_changes(1).unwrap();
        let shared = cache_for(&repo);
        assert_eq!(ids(&repo).len(), 4);
        assert_eq!(shared.read().unwrap().len(), 4);
    }
    #[test]
    fn archive_matches_finds_similar_archived_record_only() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        repo.insert_knowledge(&record("arch-1")).unwrap();
        repo.store_dense_vector("arch-1", &[1.0, 0.0, 0.0]).unwrap();
        // An ACTIVE record with an identical vector must NOT be returned.
        repo.insert_knowledge(&record("active-1")).unwrap();
        repo.store_dense_vector("active-1", &[1.0, 0.0, 0.0])
            .unwrap();
        // A SUPERSEDED archive is a provenance tombstone, not a candidate.
        repo.insert_knowledge(&record("sup-1")).unwrap();
        repo.store_dense_vector("sup-1", &[1.0, 0.0, 0.0]).unwrap();
        repo.connection()
            .execute(
                "UPDATE knowledge
                 SET state = 'superseded', tier = 'archive', superseded_by = 'active-1'
                 WHERE id = 'sup-1'",
                [],
            )
            .unwrap();
        repo.archive_record("arch-1", "test archive").unwrap();

        let hits = archive_matches(&repo, &[0.99, 0.14, 0.0], 0.93).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "arch-1");
        assert!(
            archive_matches(&repo, &[0.0, 0.0, 1.0], 0.93)
                .unwrap()
                .is_empty()
        );
    }
}
