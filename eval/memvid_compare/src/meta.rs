//! Per-lesson metadata the navigation tree needs, aligned index-for-index with `Corpus`.

use crate::data::Corpus;
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct LessonMeta {
    pub project: String,
    pub kind: String,
    pub title: String,
    /// `project_specific = 0`: the lesson applies beyond its own project.
    pub global: bool,
    /// First `domain` cue (lowest cue_index rowid), if any.
    pub domain: Option<String>,
    pub created: DateTime<Utc>,
}

/// Metadata for every lesson, in corpus order (so `meta[i]` describes `corpus.ids[i]`).
pub fn load_meta(conn: &Connection, corpus: &Corpus) -> Result<Vec<LessonMeta>, String> {
    let index_of = corpus.index_of();
    let mut meta: Vec<Option<LessonMeta>> = vec![None; corpus.len()];
    let mut stmt = conn
        .prepare("SELECT id, project_id, COALESCE(kind, ''), COALESCE(title, ''), project_specific FROM knowledge")
        .map_err(|e| e.to_string())?;
    let mut it = stmt.query([]).map_err(|e| e.to_string())?;
    while let Some(r) = it.next().map_err(|e| e.to_string())? {
        let id: String = r.get(0).map_err(|e| e.to_string())?;
        let Some(&i) = index_of.get(id.as_str()) else { continue };
        meta[i] = Some(LessonMeta {
            project: r.get(1).map_err(|e| e.to_string())?,
            kind: r.get(2).map_err(|e| e.to_string())?,
            title: r.get(3).map_err(|e| e.to_string())?,
            global: r.get::<_, i64>(4).map_err(|e| e.to_string())? == 0,
            domain: None,
            created: corpus.created[i],
        });
    }
    let mut domains: HashMap<usize, String> = HashMap::new();
    let mut stmt = conn
        .prepare("SELECT record_id, dim_value FROM cue_index WHERE dim_name = 'domain' ORDER BY rowid")
        .map_err(|e| e.to_string())?;
    let mut it = stmt.query([]).map_err(|e| e.to_string())?;
    while let Some(r) = it.next().map_err(|e| e.to_string())? {
        let id: String = r.get(0).map_err(|e| e.to_string())?;
        if let Some(&i) = index_of.get(id.as_str()) {
            domains.entry(i).or_insert(r.get(1).map_err(|e| e.to_string())?);
        }
    }
    meta.into_iter()
        .enumerate()
        .map(|(i, m)| {
            let mut m = m.ok_or_else(|| format!("lesson {} has no metadata row", corpus.ids[i]))?;
            m.domain = domains.remove(&i).filter(|d| !d.trim().is_empty());
            Ok(m)
        })
        .collect()
}
