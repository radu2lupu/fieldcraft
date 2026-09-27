//! Stores memories as `knowledge` rows with `kind = 'memory'`; the module name
//! is API/domain terminology, not a separate SQL table.

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::schema::{open_catalog, sqlite_error};
use crate::slugify;

static MEMORY_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct MemoryRepository {
    conn: Connection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub text: String,
    pub audio_ref: Option<Value>,
    pub source: String,
    pub extracted: Value,
    pub archived: bool,
    pub created_at: String,
    pub updated_at: String,
    pub revision: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemoryInput {
    pub text: String,
    pub audio_ref: Option<Value>,
    pub source: String,
    pub extracted: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemoryPatch {
    pub text: Option<String>,
    pub extracted: Option<Value>,
    pub archived: Option<bool>,
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("not found")]
    NotFound,
    #[error("revision mismatch: expected {expected}, found {actual}")]
    RevisionConflict { expected: i64, actual: i64 },
    #[error("storage: {0}")]
    Storage(String),
    #[error("invalid: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct RebuildSummary {
    pub memories_processed: usize,
    pub mentions: usize,
    pub actions: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Person {
    pub canonical_id: String,
    pub display_name: String,
    pub aliases: Value,
    pub mention_count: i64,
    pub last_seen_at: Option<String>,
    pub is_device_owner: bool,
    pub created_at: String,
    pub updated_at: String,
    pub revision: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub canonical_id: String,
    pub slug: String,
    pub display_name: String,
    pub aliases: Value,
    pub repo_root: Option<String>,
    pub mention_count: i64,
    pub last_seen_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub revision: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Shape {
    pub id: String,
    pub name: String,
    pub field_signature: Value,
    pub description: String,
    pub extraction_hints: String,
    pub query_synonyms: Value,
    pub render_hint: Option<String>,
    pub user_annotations: Option<String>,
    pub occurrences: i64,
    pub example_memory_ids: Value,
    pub discovered_at: String,
    pub updated_at: String,
    pub revision: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionRow {
    pub memory_id: String,
    pub action_index: i64,
    pub text: String,
    pub assignee_canonical_id: Option<String>,
    pub due_at: Option<String>,
    pub phase: Option<String>,
    pub waiting_on: Option<String>,
    pub snoozed_until: Option<String>,
    pub priority: Option<String>,
    pub effort: Option<String>,
    pub tags: Value,
    pub raw_status: Value,
    pub shape_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ActionFilter {
    pub assignee: Option<String>,
    pub phase: Option<String>,
    pub shape: Option<String>,
    pub limit: Option<usize>,
}

impl MemoryRepository {
    pub fn open(path: &Path) -> Result<Self, MemoryError> {
        let conn = open_catalog(path).map_err(memory_storage_error)?;
        Ok(Self { conn })
    }

    pub fn insert(&self, input: MemoryInput) -> Result<Memory, MemoryError> {
        validate_input(&input)?;
        let now = Utc::now().to_rfc3339();
        let id = new_memory_id(&input.text, &now);
        let audio_ref_json = optional_json_string(&input.audio_ref)?;
        let extracted_json = serde_json::to_string(&input.extracted)
            .map_err(|e| MemoryError::Invalid(e.to_string()))?;
        let source_json = json!({ "source": input.source.as_str() }).to_string();

        self.conn
            .execute(
                "INSERT INTO knowledge
                 (id, text, audio_ref_json, source, extracted_json, archived, created_at, updated_at, revision,
                  project_id, kind, state, title, payload_json, confidence, source_json, retrieval_text)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?6, 1,
                         'field-notes', 'memory', 'active', ?2, '{}', 0.5, ?7, ?2)",
                params![
                    id,
                    input.text,
                    audio_ref_json,
                    input.source,
                    extracted_json,
                    now,
                    source_json,
                ],
            )
            .map_err(storage)?;

        let memory = self.get(&id)?;
        self.refresh_derived_for_memory(&memory)?;
        Ok(memory)
    }

    pub fn get(&self, id: &str) -> Result<Memory, MemoryError> {
        self.conn
            .query_row(
                "SELECT id, text, audio_ref_json, source, extracted_json, archived, created_at, updated_at, revision
                 FROM knowledge WHERE id = ?1 AND kind = 'memory'",
                [id],
                memory_from_row,
            )
            .optional()
            .map_err(storage)?
            .ok_or(MemoryError::NotFound)
    }

    pub fn list(
        &self,
        cursor: Option<&str>,
        limit: usize,
        include_archived: bool,
    ) -> Result<(Vec<Memory>, Option<String>), MemoryError> {
        let limit = limit.clamp(1, 200);
        let decoded = cursor.map(decode_cursor).transpose()?;
        let (cursor_updated, cursor_id) = decoded.as_ref().map_or((None, None), |(updated, id)| {
            (Some(updated.as_str()), Some(id.as_str()))
        });
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, text, audio_ref_json, source, extracted_json, archived, created_at, updated_at, revision
                 FROM knowledge
                 WHERE kind = 'memory'
                   AND (?1 = 1 OR archived = 0)
                   AND (
                     ?2 IS NULL
                     OR updated_at < ?2
                     OR (updated_at = ?2 AND id < ?3)
                   )
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?4",
            )
            .map_err(storage)?;
        let rows = stmt
            .query_map(
                params![
                    if include_archived { 1i64 } else { 0i64 },
                    cursor_updated,
                    cursor_id,
                    (limit + 1) as i64,
                ],
                memory_from_row,
            )
            .map_err(storage)?;
        let mut memories = rows.collect::<Result<Vec<_>, _>>().map_err(storage)?;
        let next_cursor = if memories.len() > limit {
            memories.truncate(limit);
            memories.last().map(|m| encode_cursor(&m.updated_at, &m.id))
        } else {
            None
        };
        Ok((memories, next_cursor))
    }

    pub fn patch(
        &self,
        id: &str,
        expected_revision: i64,
        patch: MemoryPatch,
    ) -> Result<Memory, MemoryError> {
        let text = patch.text.as_deref();
        let extracted_json = patch
            .extracted
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| MemoryError::Invalid(e.to_string()))?;
        let archived = patch.archived.map(|value| if value { 1i64 } else { 0i64 });
        let now = Utc::now().to_rfc3339();
        let changed = self
            .conn
            .execute(
                "UPDATE knowledge
                 SET text = COALESCE(?1, text),
                     extracted_json = COALESCE(?2, extracted_json),
                     archived = COALESCE(?3, archived),
                     retrieval_text = COALESCE(?1, retrieval_text),
                     title = COALESCE(?1, title),
                     updated_at = ?4,
                     revision = revision + 1
                 WHERE id = ?5 AND revision = ?6 AND kind = 'memory'",
                params![text, extracted_json, archived, now, id, expected_revision],
            )
            .map_err(storage)?;

        if changed == 0 {
            return Err(self.revision_error(id, expected_revision)?);
        }

        let memory = self.get(id)?;
        self.refresh_derived_for_memory(&memory)?;
        Ok(memory)
    }

    pub fn archive(&self, id: &str, expected_revision: i64) -> Result<Memory, MemoryError> {
        let now = Utc::now().to_rfc3339();
        let changed = self
            .conn
            .execute(
                "UPDATE knowledge
                 SET archived = 1, updated_at = ?1, revision = revision + 1
                 WHERE id = ?2 AND revision = ?3 AND kind = 'memory'",
                params![now, id, expected_revision],
            )
            .map_err(storage)?;

        if changed == 0 {
            return Err(self.revision_error(id, expected_revision)?);
        }

        let memory = self.get(id)?;
        self.refresh_derived_for_memory(&memory)?;
        Ok(memory)
    }

    fn refresh_derived_for_memory(&self, m: &Memory) -> Result<(), MemoryError> {
        self.conn
            .execute("DELETE FROM mentions WHERE memory_id = ?1", [&m.id])
            .map_err(storage)?;
        self.conn
            .execute("DELETE FROM actions WHERE memory_id = ?1", [&m.id])
            .map_err(storage)?;

        if !m.archived {
            self.write_mentions(m)?;
            self.write_actions(m)?;
        }
        self.recompute_entity_counts()?;
        self.recompute_shape_counts()?;
        Ok(())
    }

    pub fn rebuild_views(&self) -> Result<RebuildSummary, MemoryError> {
        self.conn
            .execute_batch(
                "DELETE FROM mentions;
                 DELETE FROM actions;
                 UPDATE people SET mention_count = 0, last_seen_at = NULL;
                 UPDATE projects SET mention_count = 0, last_seen_at = NULL;
                 UPDATE shapes SET occurrences = 0, example_memory_ids = '[]';",
            )
            .map_err(storage)?;

        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, text, audio_ref_json, source, extracted_json, archived, created_at, updated_at, revision
                 FROM knowledge WHERE kind='memory' AND archived = 0 ORDER BY created_at ASC",
            )
            .map_err(storage)?;
        let memories = stmt
            .query_map([], memory_from_row)
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        drop(stmt);

        for memory in &memories {
            self.refresh_derived_for_memory(memory)?;
        }
        self.conn
            .execute(
                "DELETE FROM people WHERE mention_count = 0 AND is_device_owner = 0",
                [],
            )
            .map_err(storage)?;
        self.conn
            .execute("DELETE FROM projects WHERE mention_count = 0", [])
            .map_err(storage)?;

        let mentions = count_table(&self.conn, "mentions")?;
        let actions = count_table(&self.conn, "actions")?;
        Ok(RebuildSummary {
            memories_processed: memories.len(),
            mentions,
            actions,
        })
    }

    pub fn list_people(&self) -> Result<Vec<Person>, MemoryError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT canonical_id, display_name, aliases_json, mention_count, last_seen_at,
                        is_device_owner, created_at, updated_at, revision
                 FROM people ORDER BY last_seen_at DESC, display_name ASC",
            )
            .map_err(storage)?;
        stmt.query_map([], person_from_row)
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)
    }

    pub fn list_projects(&self) -> Result<Vec<Project>, MemoryError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT canonical_id, slug, display_name, aliases_json, repo_root, mention_count,
                        last_seen_at, created_at, updated_at, revision
                 FROM projects ORDER BY last_seen_at DESC, display_name ASC",
            )
            .map_err(storage)?;
        stmt.query_map([], project_from_row)
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)
    }

    pub fn list_shapes(&self) -> Result<Vec<Shape>, MemoryError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, name, field_signature, description, extraction_hints, query_synonyms,
                        render_hint, user_annotations, occurrences, example_memory_ids,
                        discovered_at, updated_at, revision
                 FROM shapes ORDER BY occurrences DESC, name ASC",
            )
            .map_err(storage)?;
        stmt.query_map([], shape_from_row)
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)
    }

    pub fn view_today(&self, now: &str) -> Result<Vec<ActionRow>, MemoryError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT a.memory_id, a.action_index, a.text, a.assignee_canonical_id, a.due_at,
                        a.phase, a.waiting_on, a.snoozed_until, a.priority, a.effort,
                        a.tags_json, a.raw_status_json, a.shape_id
                 FROM actions a
                 JOIN knowledge m ON m.id = a.memory_id
                 WHERE m.kind='memory'
                   AND m.archived = 0
                   AND a.phase IN ('open', 'in_progress', 'waiting')
                   AND (a.due_at IS NULL OR a.due_at <= ?1)
                   AND (a.snoozed_until IS NULL OR a.snoozed_until <= ?1)
                 ORDER BY COALESCE(a.due_at, m.created_at) ASC, a.action_index ASC",
            )
            .map_err(storage)?;
        stmt.query_map([now], action_from_row)
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)
    }

    pub fn view_inbox(&self, limit: usize) -> Result<Vec<Memory>, MemoryError> {
        let limit = limit.clamp(1, 200);
        let (memories, _) = self.list(None, 200, false)?;
        Ok(memories
            .into_iter()
            .filter(memory_needs_inbox)
            .take(limit)
            .collect())
    }

    pub fn view_project(
        &self,
        canonical_id: &str,
        limit: usize,
    ) -> Result<Vec<Memory>, MemoryError> {
        self.view_by_mention("project", canonical_id, limit)
    }

    pub fn view_person(
        &self,
        canonical_id: &str,
        limit: usize,
    ) -> Result<Vec<Memory>, MemoryError> {
        self.view_by_mention("person", canonical_id, limit)
    }

    pub fn view_actions(&self, filter: ActionFilter) -> Result<Vec<ActionRow>, MemoryError> {
        let limit = filter.limit.unwrap_or(50).clamp(1, 200);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT a.memory_id, a.action_index, a.text, a.assignee_canonical_id, a.due_at,
                        a.phase, a.waiting_on, a.snoozed_until, a.priority, a.effort,
                        a.tags_json, a.raw_status_json, a.shape_id
                 FROM actions a
                 JOIN knowledge m ON m.id = a.memory_id
                 WHERE m.kind='memory'
                   AND m.archived = 0
                   AND (?1 IS NULL OR a.assignee_canonical_id = ?1)
                   AND (?2 IS NULL OR a.phase = ?2)
                   AND (?3 IS NULL OR a.shape_id = ?3)
                 ORDER BY COALESCE(a.due_at, m.created_at) ASC, a.action_index ASC
                 LIMIT ?4",
            )
            .map_err(storage)?;
        stmt.query_map(
            params![
                filter.assignee.as_deref(),
                filter.phase.as_deref(),
                filter.shape.as_deref(),
                limit as i64,
            ],
            action_from_row,
        )
        .map_err(storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)
    }

    fn revision_error(&self, id: &str, expected: i64) -> Result<MemoryError, MemoryError> {
        let actual = self
            .conn
            .query_row(
                "SELECT revision FROM knowledge WHERE id = ?1 AND kind='memory'",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        Ok(match actual {
            Some(actual) => MemoryError::RevisionConflict { expected, actual },
            None => MemoryError::NotFound,
        })
    }

    fn view_by_mention(
        &self,
        entity_kind: &str,
        canonical_id: &str,
        limit: usize,
    ) -> Result<Vec<Memory>, MemoryError> {
        let limit = limit.clamp(1, 200);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT DISTINCT m.id, m.text, m.audio_ref_json, m.source, m.extracted_json,
                        m.archived, m.created_at, m.updated_at, m.revision
                 FROM knowledge m
                 JOIN mentions mn ON mn.memory_id = m.id
                 WHERE m.kind='memory' AND m.archived = 0 AND mn.entity_kind = ?1 AND mn.canonical_id = ?2
                 ORDER BY m.updated_at DESC, m.id DESC
                 LIMIT ?3",
            )
            .map_err(storage)?;
        stmt.query_map(
            params![entity_kind, canonical_id, limit as i64],
            memory_from_row,
        )
        .map_err(storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)
    }

    fn write_mentions(&self, m: &Memory) -> Result<(), MemoryError> {
        if let Some(people) = m.extracted.get("people").and_then(Value::as_array) {
            for person in people {
                let Some((canonical_id, raw_text, span_start, span_end)) =
                    mention_parts(person, "person")
                else {
                    continue;
                };
                self.insert_mention(m, "person", &canonical_id, &raw_text, span_start, span_end)?;
                self.upsert_person(&canonical_id, &raw_text, &m.created_at)?;
            }
        }

        if let Some(project) = m.extracted.get("project")
            && !project.is_null()
            && let Some((canonical_id, raw_text, span_start, span_end)) =
                mention_parts(project, "project")
        {
            let slug = project
                .get("slug")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| slugify(&raw_text));
            self.insert_mention(m, "project", &canonical_id, &raw_text, span_start, span_end)?;
            self.upsert_project(&canonical_id, &slug, &raw_text, &m.created_at)?;
        }
        Ok(())
    }

    fn insert_mention(
        &self,
        m: &Memory,
        entity_kind: &str,
        canonical_id: &str,
        raw_text: &str,
        span_start: Option<i64>,
        span_end: Option<i64>,
    ) -> Result<(), MemoryError> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO mentions
                 (memory_id, entity_kind, canonical_id, raw_text, span_start, span_end)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    m.id,
                    entity_kind,
                    canonical_id,
                    raw_text,
                    span_start,
                    span_end
                ],
            )
            .map_err(storage)?;
        Ok(())
    }

    fn upsert_person(
        &self,
        canonical_id: &str,
        raw_text: &str,
        seen_at: &str,
    ) -> Result<(), MemoryError> {
        let now = Utc::now().to_rfc3339();
        match self.aliases_for("people", "canonical_id", canonical_id)? {
            Some(aliases) => {
                self.conn
                    .execute(
                        "UPDATE people
                         SET aliases_json = ?1, updated_at = ?2, last_seen_at = MAX(COALESCE(last_seen_at, ''), ?3)
                         WHERE canonical_id = ?4",
                        params![append_alias(aliases, raw_text), now, seen_at, canonical_id],
                    )
                    .map_err(storage)?;
            }
            None => {
                self.conn
                    .execute(
                        "INSERT INTO people
                         (canonical_id, display_name, aliases_json, mention_count, last_seen_at,
                          is_device_owner, created_at, updated_at, revision)
                         VALUES (?1, ?2, ?3, 0, ?4, 0, ?5, ?5, 1)",
                        params![
                            canonical_id,
                            raw_text,
                            json_array_string(raw_text),
                            seen_at,
                            now,
                        ],
                    )
                    .map_err(storage)?;
            }
        }
        Ok(())
    }

    fn upsert_project(
        &self,
        canonical_id: &str,
        slug: &str,
        raw_text: &str,
        seen_at: &str,
    ) -> Result<(), MemoryError> {
        let now = Utc::now().to_rfc3339();
        match self.aliases_for("projects", "canonical_id", canonical_id)? {
            Some(aliases) => {
                self.conn
                    .execute(
                        "UPDATE projects
                         SET aliases_json = ?1, updated_at = ?2, last_seen_at = MAX(COALESCE(last_seen_at, ''), ?3)
                         WHERE canonical_id = ?4",
                        params![append_alias(aliases, raw_text), now, seen_at, canonical_id],
                    )
                    .map_err(storage)?;
            }
            None => {
                self.conn
                    .execute(
                        "INSERT INTO projects
                         (canonical_id, slug, display_name, aliases_json, repo_root, mention_count,
                          last_seen_at, created_at, updated_at, revision)
                         VALUES (?1, ?2, ?3, ?4, NULL, 0, ?5, ?6, ?6, 1)",
                        params![
                            canonical_id,
                            slug,
                            raw_text,
                            json_array_string(raw_text),
                            seen_at,
                            now,
                        ],
                    )
                    .map_err(storage)?;
            }
        }
        Ok(())
    }

    fn aliases_for(
        &self,
        table: &str,
        column: &str,
        value: &str,
    ) -> Result<Option<String>, MemoryError> {
        let sql = format!("SELECT aliases_json FROM {table} WHERE {column} = ?1");
        self.conn
            .query_row(&sql, [value], |row| row.get(0))
            .optional()
            .map_err(storage)
    }

    fn write_actions(&self, m: &Memory) -> Result<(), MemoryError> {
        let Some(actions) = m.extracted.get("actions").and_then(Value::as_array) else {
            return Ok(());
        };

        for (index, action) in actions.iter().enumerate() {
            let text = first_string(action, &["text", "title", "description"])
                .unwrap_or_else(|| action.to_string());
            let status = action.get("status").filter(|value| value.is_object());
            let phase = first_string(action, &["phase"])
                .or_else(|| status.and_then(|s| first_string(s, &["phase", "state"])));
            let tags_json = action
                .get("tags")
                .filter(|value| value.is_array())
                .cloned()
                .unwrap_or_else(|| json!([]));
            let raw_status_json = status.cloned().unwrap_or_else(|| json!({}));
            self.conn
                .execute(
                    "INSERT INTO actions
                     (memory_id, action_index, text, assignee_canonical_id, due_at, phase,
                      waiting_on, snoozed_until, priority, effort, tags_json, raw_status_json, shape_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![
                        m.id,
                        index as i64,
                        text,
                        first_string(action, &["assignee_canonical_id", "assignee"]),
                        first_string(action, &["due_at", "due"]),
                        phase,
                        first_string(action, &["waiting_on"]),
                        first_string(action, &["snoozed_until"]),
                        first_string(action, &["priority"]),
                        first_string(action, &["effort"]),
                        tags_json.to_string(),
                        raw_status_json.to_string(),
                        first_string(action, &["shape_id"]),
                    ],
                )
                .map_err(storage)?;
        }
        Ok(())
    }

    fn recompute_entity_counts(&self) -> Result<(), MemoryError> {
        self.conn
            .execute(
                "UPDATE people
                 SET mention_count = (
                       SELECT COUNT(*) FROM mentions mn
                       JOIN knowledge m ON m.id = mn.memory_id
                       WHERE m.archived = 0 AND m.kind='memory'
                         AND mn.entity_kind = 'person'
                         AND mn.canonical_id = people.canonical_id
                     ),
                     last_seen_at = (
                       SELECT MAX(m.created_at) FROM mentions mn
                       JOIN knowledge m ON m.id = mn.memory_id
                       WHERE m.archived = 0 AND m.kind='memory'
                         AND mn.entity_kind = 'person'
                         AND mn.canonical_id = people.canonical_id
                     )",
                [],
            )
            .map_err(storage)?;
        self.conn
            .execute(
                "UPDATE projects
                 SET mention_count = (
                       SELECT COUNT(*) FROM mentions mn
                       JOIN knowledge m ON m.id = mn.memory_id
                       WHERE m.archived = 0 AND m.kind='memory'
                         AND mn.entity_kind = 'project'
                         AND mn.canonical_id = projects.canonical_id
                     ),
                     last_seen_at = (
                       SELECT MAX(m.created_at) FROM mentions mn
                       JOIN knowledge m ON m.id = mn.memory_id
                       WHERE m.archived = 0 AND m.kind='memory'
                         AND mn.entity_kind = 'project'
                         AND mn.canonical_id = projects.canonical_id
                     )",
                [],
            )
            .map_err(storage)?;
        Ok(())
    }

    fn recompute_shape_counts(&self) -> Result<(), MemoryError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM shapes")
            .map_err(storage)?;
        let shape_ids = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        drop(stmt);

        for shape_id in shape_ids {
            let occurrences: i64 = self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM actions a
                     JOIN knowledge m ON m.id = a.memory_id
                     WHERE m.archived = 0 AND m.kind='memory' AND a.shape_id = ?1",
                    [&shape_id],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            let mut examples_stmt = self
                .conn
                .prepare(
                    "SELECT DISTINCT a.memory_id FROM actions a
                     JOIN knowledge m ON m.id = a.memory_id
                     WHERE m.archived = 0 AND m.kind='memory' AND a.shape_id = ?1
                     ORDER BY m.created_at DESC
                     LIMIT 10",
                )
                .map_err(storage)?;
            let examples = examples_stmt
                .query_map([&shape_id], |row| row.get::<_, String>(0))
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)?;
            self.conn
                .execute(
                    "UPDATE shapes SET occurrences = ?1, example_memory_ids = ?2 WHERE id = ?3",
                    params![
                        occurrences,
                        serde_json::to_string(&examples).map_err(|e| {
                            MemoryError::Storage(format!("serialize shape examples: {e}"))
                        })?,
                        shape_id,
                    ],
                )
                .map_err(storage)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Entity hygiene: rename + merge
    // -----------------------------------------------------------------------

    /// Rename a person's display name (the human-facing label), bumping revision.
    /// Does not touch `canonical_id`, mentions, or actions.
    pub fn rename_person(
        &self,
        canonical_id: &str,
        display_name: &str,
    ) -> Result<Person, MemoryError> {
        self.rename_entity("people", canonical_id, display_name)?;
        self.get_person(canonical_id)
    }

    /// Rename a project's display name, bumping revision.
    pub fn rename_project(
        &self,
        canonical_id: &str,
        display_name: &str,
    ) -> Result<Project, MemoryError> {
        self.rename_entity("projects", canonical_id, display_name)?;
        self.get_project(canonical_id)
    }

    fn rename_entity(
        &self,
        table: &str,
        canonical_id: &str,
        display_name: &str,
    ) -> Result<(), MemoryError> {
        let display_name = display_name.trim();
        if display_name.is_empty() {
            return Err(MemoryError::Invalid("display_name is required".into()));
        }
        let now = Utc::now().to_rfc3339();
        let sql = format!(
            "UPDATE {table}
             SET display_name = ?1, updated_at = ?2, revision = revision + 1
             WHERE canonical_id = ?3"
        );
        let changed = self
            .conn
            .execute(&sql, params![display_name, now, canonical_id])
            .map_err(storage)?;
        if changed == 0 {
            return Err(MemoryError::NotFound);
        }
        Ok(())
    }

    /// Merge one or more people into `into`.
    ///
    /// For each `from` id: its display_name + aliases are unioned into `into`'s
    /// aliases, `mentions` (entity_kind='person') and `actions.assignee_canonical_id`
    /// are repointed to `into`, the affected memories' `extracted_json` people
    /// entries are rewritten so `canonical_id` becomes `into` (durability against
    /// `rebuild_views`, which re-derives entities from the blobs), and the
    /// merged-away `people` row is deleted. Counts/last_seen are recomputed and
    /// `into`'s revision is bumped. Refuses if `into` ∈ `from`.
    pub fn merge_people(&self, into: &str, from: &[String]) -> Result<Person, MemoryError> {
        self.merge_entity(EntityKind::Person, into, from)?;
        self.get_person(into)
    }

    /// Merge one or more projects into `into`. Analogous to [`merge_people`],
    /// repointing `mentions` with entity_kind='project'.
    pub fn merge_projects(&self, into: &str, from: &[String]) -> Result<Project, MemoryError> {
        self.merge_entity(EntityKind::Project, into, from)?;
        self.get_project(into)
    }

    fn merge_entity(
        &self,
        kind: EntityKind,
        into: &str,
        from: &[String],
    ) -> Result<(), MemoryError> {
        let table = kind.table();
        let mention_kind = kind.mention_kind();

        // Validate: `into` must exist; refuse self-merge; dedup/clean `from`.
        let into_aliases = self
            .aliases_for(table, "canonical_id", into)?
            .ok_or(MemoryError::NotFound)?;
        let mut froms: Vec<String> = Vec::new();
        for f in from {
            let f = f.trim();
            if f.is_empty() {
                continue;
            }
            if f == into {
                return Err(MemoryError::Invalid(
                    "cannot merge an entity into itself".into(),
                ));
            }
            if !froms.iter().any(|existing| existing == f) {
                froms.push(f.to_string());
            }
        }
        if froms.is_empty() {
            return Err(MemoryError::Invalid("merge_from is empty".into()));
        }

        let now = Utc::now().to_rfc3339();

        // 1. Union aliases (display_name + aliases of each merged-away into `into`).
        let mut merged_aliases =
            serde_json::from_str::<Vec<String>>(&into_aliases).unwrap_or_default();
        for f in &froms {
            let Some((display_name, aliases_json)) = self.entity_label(table, f)? else {
                return Err(MemoryError::NotFound);
            };
            push_unique(&mut merged_aliases, &display_name);
            for alias in serde_json::from_str::<Vec<String>>(&aliases_json).unwrap_or_default() {
                push_unique(&mut merged_aliases, &alias);
            }
        }
        let merged_aliases_json =
            serde_json::to_string(&merged_aliases).unwrap_or_else(|_| "[]".into());

        // Steps 2–6 mutate several tables and must be all-or-nothing: a failure
        // partway through (e.g. blobs rewritten but the old entity row not
        // deleted) would leave the catalog inconsistent. Wrap them in a single
        // transaction and ROLLBACK on any error.
        self.conn.execute_batch("BEGIN").map_err(storage)?;
        let result = self.merge_entity_mutations(
            kind,
            mention_kind,
            table,
            into,
            &froms,
            &merged_aliases_json,
            &now,
        );
        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT").map_err(storage)?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// The mutating half of [`merge_entity`], run inside a transaction so the
    /// whole merge commits atomically or rolls back.
    #[allow(clippy::too_many_arguments)]
    fn merge_entity_mutations(
        &self,
        kind: EntityKind,
        mention_kind: &str,
        table: &str,
        into: &str,
        froms: &[String],
        merged_aliases_json: &str,
        now: &str,
    ) -> Result<(), MemoryError> {
        // 2. Rewrite extracted_json in affected memories so re-derivation maps to
        //    `into` (durability against rebuild_views). Do this BEFORE repointing
        //    mentions so we can find affected memories via the still-`from` rows.
        for f in froms {
            self.rewrite_extracted_canonical_id(kind, f, into)?;
        }

        // 3. Repoint derived tables from each `from` to `into`.
        for f in froms {
            self.conn
                .execute(
                    "UPDATE OR IGNORE mentions SET canonical_id = ?1
                     WHERE entity_kind = ?2 AND canonical_id = ?3",
                    params![into, mention_kind, f],
                )
                .map_err(storage)?;
            // Any rows that collided with an existing (memory, kind, into, raw_text)
            // primary key are left behind by UPDATE OR IGNORE; drop them.
            self.conn
                .execute(
                    "DELETE FROM mentions WHERE entity_kind = ?1 AND canonical_id = ?2",
                    params![mention_kind, f],
                )
                .map_err(storage)?;
            if matches!(kind, EntityKind::Person) {
                self.conn
                    .execute(
                        "UPDATE actions SET assignee_canonical_id = ?1
                         WHERE assignee_canonical_id = ?2",
                        params![into, f],
                    )
                    .map_err(storage)?;
            }
        }

        // 4. Delete the merged-away entity rows.
        let delete_sql = format!("DELETE FROM {table} WHERE canonical_id = ?1");
        for f in froms {
            self.conn.execute(&delete_sql, [f]).map_err(storage)?;
        }

        // 5. Persist unioned aliases + bump revision on the survivor.
        let update_sql = format!(
            "UPDATE {table}
             SET aliases_json = ?1, updated_at = ?2, revision = revision + 1
             WHERE canonical_id = ?3"
        );
        self.conn
            .execute(&update_sql, params![merged_aliases_json, now, into])
            .map_err(storage)?;

        // 6. Recompute mention_count / last_seen_at from the repointed mentions.
        self.recompute_entity_counts()?;
        Ok(())
    }

    /// Rewrite `knowledge.extracted_json` so every people/project entry whose
    /// derived canonical_id is `from` is rewritten to carry `canonical_id =
    /// into`. Mirrors the derivation in [`mention_parts`]: an entry contributes
    /// `from` either via an explicit canonical_id/id/slug field or via
    /// `slugify(raw_text)`. We set an explicit `canonical_id` so future
    /// re-derivation is unambiguous.
    fn rewrite_extracted_canonical_id(
        &self,
        kind: EntityKind,
        from: &str,
        into: &str,
    ) -> Result<(), MemoryError> {
        // Collect candidate memories via the current (pre-repoint) mentions.
        let mut stmt = self
            .conn
            .prepare(
                "SELECT DISTINCT memory_id FROM mentions
                 WHERE entity_kind = ?1 AND canonical_id = ?2",
            )
            .map_err(storage)?;
        let memory_ids = stmt
            .query_map(params![kind.mention_kind(), from], |row| {
                row.get::<_, String>(0)
            })
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        drop(stmt);

        for memory_id in memory_ids {
            let mut memory = self.get(&memory_id)?;
            if rewrite_extracted_value(&mut memory.extracted, kind, from, into) {
                let extracted_json = serde_json::to_string(&memory.extracted)
                    .map_err(|e| MemoryError::Invalid(e.to_string()))?;
                self.conn
                    .execute(
                        "UPDATE knowledge SET extracted_json = ?1 WHERE id = ?2",
                        params![extracted_json, memory_id],
                    )
                    .map_err(storage)?;
            }
        }
        Ok(())
    }

    fn get_person(&self, canonical_id: &str) -> Result<Person, MemoryError> {
        self.conn
            .query_row(
                "SELECT canonical_id, display_name, aliases_json, mention_count, last_seen_at,
                        is_device_owner, created_at, updated_at, revision
                 FROM people WHERE canonical_id = ?1",
                [canonical_id],
                person_from_row,
            )
            .optional()
            .map_err(storage)?
            .ok_or(MemoryError::NotFound)
    }

    fn get_project(&self, canonical_id: &str) -> Result<Project, MemoryError> {
        self.conn
            .query_row(
                "SELECT canonical_id, slug, display_name, aliases_json, repo_root, mention_count,
                        last_seen_at, created_at, updated_at, revision
                 FROM projects WHERE canonical_id = ?1",
                [canonical_id],
                project_from_row,
            )
            .optional()
            .map_err(storage)?
            .ok_or(MemoryError::NotFound)
    }

    fn entity_label(
        &self,
        table: &str,
        canonical_id: &str,
    ) -> Result<Option<(String, String)>, MemoryError> {
        let sql = format!("SELECT display_name, aliases_json FROM {table} WHERE canonical_id = ?1");
        self.conn
            .query_row(&sql, [canonical_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .optional()
            .map_err(storage)
    }
}

#[derive(Clone, Copy)]
enum EntityKind {
    Person,
    Project,
}

impl EntityKind {
    fn table(self) -> &'static str {
        match self {
            EntityKind::Person => "people",
            EntityKind::Project => "projects",
        }
    }

    fn mention_kind(self) -> &'static str {
        match self {
            EntityKind::Person => "person",
            EntityKind::Project => "project",
        }
    }

    /// Key in `extracted_json` holding the entity entries.
    fn extracted_key(self) -> &'static str {
        match self {
            EntityKind::Person => "people",
            EntityKind::Project => "project",
        }
    }
}

/// Rewrite a single memory's extracted blob in place. Returns true if changed.
/// `people` is an array of entries; `project` is a single entry (or null).
fn rewrite_extracted_value(
    extracted: &mut Value,
    kind: EntityKind,
    from: &str,
    into: &str,
) -> bool {
    let key = kind.extracted_key();
    let Some(node) = extracted.get_mut(key) else {
        return false;
    };
    match kind {
        EntityKind::Person => {
            let Some(arr) = node.as_array_mut() else {
                return false;
            };
            let mut changed = false;
            for entry in arr.iter_mut() {
                changed |= rewrite_entry(entry, kind, from, into);
            }
            changed
        }
        EntityKind::Project => {
            if node.is_null() {
                return false;
            }
            rewrite_entry(node, kind, from, into)
        }
    }
}

/// Rewrite one entity entry if its derived canonical_id matches `from`.
fn rewrite_entry(entry: &mut Value, kind: EntityKind, from: &str, into: &str) -> bool {
    let Some((derived_id, _, _, _)) = mention_parts(entry, kind.mention_kind()) else {
        return false;
    };
    if derived_id != from {
        return false;
    }
    match entry {
        // Bare string entry (e.g. "Delma"): promote to an object so we can
        // attach an explicit canonical_id that survives re-derivation.
        Value::String(raw) => {
            *entry = json!({ "raw_text": raw, "canonical_id": into });
            true
        }
        Value::Object(map) => {
            // canonical_id is consulted first in mention_parts, so this alone
            // is what makes re-derivation map to `into`. Also overwrite `id`
            // (a fallback key) to keep the blob internally consistent. We leave
            // `slug` untouched: it is only read by upsert_project when creating
            // a brand-new row, which never happens for the pre-existing `into`.
            map.insert("canonical_id".into(), Value::String(into.to_string()));
            map.insert("id".into(), Value::String(into.to_string()));
            true
        }
        _ => false,
    }
}

fn push_unique(list: &mut Vec<String>, value: &str) {
    if !value.trim().is_empty() && !list.iter().any(|existing| existing == value) {
        list.push(value.to_string());
    }
}

fn validate_input(input: &MemoryInput) -> Result<(), MemoryError> {
    if input.text.trim().is_empty() {
        return Err(MemoryError::Invalid("text is required".into()));
    }
    match input.source.as_str() {
        "voice" | "dictation" | "typed" | "imported" => Ok(()),
        other => Err(MemoryError::Invalid(format!("unsupported source: {other}"))),
    }
}

fn new_memory_id(text: &str, now: &str) -> String {
    let counter = MEMORY_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(now.as_bytes());
    hasher.update(counter.to_le_bytes());
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    format!("mem_{}", hex_prefix(&digest, 16))
}

fn hex_prefix(bytes: &[u8], len: usize) -> String {
    bytes
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .take(len)
        .map(|nibble| char::from_digit(nibble as u32, 16).unwrap_or('0'))
        .collect()
}

fn optional_json_string(value: &Option<Value>) -> Result<Option<String>, MemoryError> {
    value
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| MemoryError::Invalid(e.to_string()))
}

fn memory_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    // text/source/extracted_json are columns on `knowledge` that are nullable at
    // the SQL level (they only carry values for kind='memory' rows). Read them
    // null-safely so a stray NULL can't crash a /memories read.
    let audio_ref_json: Option<String> = row.get(2)?;
    let extracted_json: Option<String> = row.get(4)?;
    let archived: i64 = row.get(5)?;
    Ok(Memory {
        id: row.get(0)?,
        text: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        audio_ref: parse_optional_json(audio_ref_json),
        source: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
        extracted: extracted_json
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({})),
        archived: archived != 0,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        revision: row.get(8)?,
    })
}

fn person_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Person> {
    let aliases_json: String = row.get(2)?;
    let is_device_owner: i64 = row.get(5)?;
    Ok(Person {
        canonical_id: row.get(0)?,
        display_name: row.get(1)?,
        aliases: serde_json::from_str(&aliases_json).unwrap_or_else(|_| json!([])),
        mention_count: row.get(3)?,
        last_seen_at: row.get(4)?,
        is_device_owner: is_device_owner != 0,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        revision: row.get(8)?,
    })
}

fn project_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    let aliases_json: String = row.get(3)?;
    Ok(Project {
        canonical_id: row.get(0)?,
        slug: row.get(1)?,
        display_name: row.get(2)?,
        aliases: serde_json::from_str(&aliases_json).unwrap_or_else(|_| json!([])),
        repo_root: row.get(4)?,
        mention_count: row.get(5)?,
        last_seen_at: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        revision: row.get(9)?,
    })
}

fn shape_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Shape> {
    let field_signature: String = row.get(2)?;
    let query_synonyms: String = row.get(5)?;
    let example_memory_ids: String = row.get(9)?;
    Ok(Shape {
        id: row.get(0)?,
        name: row.get(1)?,
        field_signature: serde_json::from_str(&field_signature).unwrap_or_else(|_| json!([])),
        description: row.get(3)?,
        extraction_hints: row.get(4)?,
        query_synonyms: serde_json::from_str(&query_synonyms).unwrap_or_else(|_| json!([])),
        render_hint: row.get(6)?,
        user_annotations: row.get(7)?,
        occurrences: row.get(8)?,
        example_memory_ids: serde_json::from_str(&example_memory_ids).unwrap_or_else(|_| json!([])),
        discovered_at: row.get(10)?,
        updated_at: row.get(11)?,
        revision: row.get(12)?,
    })
}

fn action_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ActionRow> {
    let tags_json: String = row.get(10)?;
    let raw_status_json: String = row.get(11)?;
    Ok(ActionRow {
        memory_id: row.get(0)?,
        action_index: row.get(1)?,
        text: row.get(2)?,
        assignee_canonical_id: row.get(3)?,
        due_at: row.get(4)?,
        phase: row.get(5)?,
        waiting_on: row.get(6)?,
        snoozed_until: row.get(7)?,
        priority: row.get(8)?,
        effort: row.get(9)?,
        tags: serde_json::from_str(&tags_json).unwrap_or_else(|_| json!([])),
        raw_status: serde_json::from_str(&raw_status_json).unwrap_or_else(|_| json!({})),
        shape_id: row.get(12)?,
    })
}

fn parse_optional_json(value: Option<String>) -> Option<Value> {
    value.and_then(|raw| serde_json::from_str(&raw).ok())
}

// `entity_kind` is retained for call-site symmetry (person vs project) but the
// canonical-id derivation is identical for both, so it is currently unused.
fn mention_parts(
    value: &Value,
    _entity_kind: &str,
) -> Option<(String, String, Option<i64>, Option<i64>)> {
    if let Some(raw) = value.as_str() {
        return Some((slugify(raw), raw.to_string(), None, None));
    }
    let raw_text = first_string(value, &["raw_text", "text", "display_name", "name"])?;
    let canonical_id =
        first_string(value, &["canonical_id", "id", "slug"]).unwrap_or_else(|| slugify(&raw_text));
    let span_start = value.get("span_start").and_then(Value::as_i64);
    let span_end = value.get("span_end").and_then(Value::as_i64);
    Some((canonical_id, raw_text, span_start, span_end))
}

fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
    })
}

fn append_alias(aliases_json: String, alias: &str) -> String {
    let mut aliases = serde_json::from_str::<Vec<String>>(&aliases_json).unwrap_or_default();
    if !aliases.iter().any(|existing| existing == alias) {
        aliases.push(alias.to_string());
    }
    serde_json::to_string(&aliases).unwrap_or_else(|_| "[]".into())
}

fn json_array_string(value: &str) -> String {
    serde_json::to_string(&vec![value]).unwrap_or_else(|_| "[]".into())
}

fn count_table(conn: &Connection, table: &str) -> Result<usize, MemoryError> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let count: i64 = conn
        .query_row(&sql, [], |row| row.get(0))
        .map_err(storage)?;
    Ok(count as usize)
}

fn memory_needs_inbox(memory: &Memory) -> bool {
    let Some(project) = memory.extracted.get("project") else {
        return true;
    };
    if project.is_null() {
        return true;
    }
    project
        .get("confidence")
        .and_then(Value::as_f64)
        .is_some_and(|confidence| confidence < 0.7)
}

fn encode_cursor(updated_at: &str, id: &str) -> String {
    base64_encode(format!("{updated_at}|{id}").as_bytes())
}

fn decode_cursor(cursor: &str) -> Result<(String, String), MemoryError> {
    let bytes = base64_decode(cursor)?;
    let decoded =
        String::from_utf8(bytes).map_err(|_| MemoryError::Invalid("invalid cursor".into()))?;
    let Some((updated_at, id)) = decoded.split_once('|') else {
        return Err(MemoryError::Invalid("invalid cursor".into()));
    };
    Ok((updated_at.to_string(), id.to_string()))
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn base64_decode(input: &str) -> Result<Vec<u8>, MemoryError> {
    let clean = input.trim();
    if !clean.len().is_multiple_of(4) {
        return Err(MemoryError::Invalid("invalid cursor".into()));
    }
    let mut out = Vec::new();
    for chunk in clean.as_bytes().chunks(4) {
        let vals = [
            base64_value(chunk[0])?,
            base64_value(chunk[1])?,
            if chunk[2] == b'=' {
                64
            } else {
                base64_value(chunk[2])?
            },
            if chunk[3] == b'=' {
                64
            } else {
                base64_value(chunk[3])?
            },
        ];
        out.push((vals[0] << 2) | (vals[1] >> 4));
        if vals[2] != 64 {
            out.push((vals[1] << 4) | (vals[2] >> 2));
        }
        if vals[3] != 64 {
            out.push((vals[2] << 6) | vals[3]);
        }
    }
    Ok(out)
}

fn base64_value(byte: u8) -> Result<u8, MemoryError> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(MemoryError::Invalid("invalid cursor".into())),
    }
}

fn memory_storage_error(error: fieldcraft_core::TrError) -> MemoryError {
    MemoryError::Storage(error.to_string())
}

fn storage(error: rusqlite::Error) -> MemoryError {
    let err = sqlite_error(error);
    MemoryError::Storage(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn repo() -> MemoryRepository {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        MemoryRepository::open(&path).unwrap()
    }

    fn input(extracted: Value) -> MemoryInput {
        MemoryInput {
            text: "Talk to Ada about Fieldcraft".into(),
            audio_ref: None,
            source: "typed".into(),
            extracted,
        }
    }

    #[test]
    fn schema_v4_applies_idempotently() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        MemoryRepository::open(&path).unwrap();
        MemoryRepository::open(&path).unwrap();
    }

    #[test]
    fn insert_and_get_roundtrip() {
        let repo = repo();
        let inserted = repo.insert(input(json!({}))).unwrap();
        let fetched = repo.get(&inserted.id).unwrap();
        assert_eq!(fetched.text, "Talk to Ada about Fieldcraft");
        assert_eq!(fetched.source, "typed");
        assert_eq!(fetched.revision, 1);
    }

    #[test]
    fn insert_accepts_dictation_source() {
        let repo = repo();
        let mut inp = input(json!({}));
        inp.source = "dictation".into();
        let inserted = repo.insert(inp).unwrap();
        assert_eq!(inserted.source, "dictation");
    }

    #[test]
    fn patch_updates_revision_and_returns_memory() {
        let repo = repo();
        let inserted = repo.insert(input(json!({}))).unwrap();
        let patched = repo
            .patch(
                &inserted.id,
                1,
                MemoryPatch {
                    text: Some("Updated memory".into()),
                    extracted: None,
                    archived: None,
                },
            )
            .unwrap();
        assert_eq!(patched.text, "Updated memory");
        assert_eq!(patched.revision, 2);
    }

    #[test]
    fn patch_with_wrong_revision_returns_conflict() {
        let repo = repo();
        let inserted = repo.insert(input(json!({}))).unwrap();
        let error = repo
            .patch(
                &inserted.id,
                99,
                MemoryPatch {
                    text: Some("Nope".into()),
                    extracted: None,
                    archived: None,
                },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            MemoryError::RevisionConflict { actual: 1, .. }
        ));
    }

    #[test]
    fn patch_without_revision_match_does_not_mutate() {
        let repo = repo();
        let inserted = repo.insert(input(json!({}))).unwrap();
        let _ = repo.patch(
            &inserted.id,
            99,
            MemoryPatch {
                text: Some("Nope".into()),
                extracted: None,
                archived: None,
            },
        );
        let fetched = repo.get(&inserted.id).unwrap();
        assert_eq!(fetched.text, inserted.text);
        assert_eq!(fetched.revision, 1);
    }

    #[test]
    fn archive_sets_archived_and_bumps_revision() {
        let repo = repo();
        let inserted = repo.insert(input(json!({}))).unwrap();
        let archived = repo.archive(&inserted.id, 1).unwrap();
        assert!(archived.archived);
        assert_eq!(archived.revision, 2);
    }

    #[test]
    fn list_excludes_archived_by_default() {
        let repo = repo();
        let inserted = repo.insert(input(json!({}))).unwrap();
        repo.archive(&inserted.id, 1).unwrap();
        let (visible, _) = repo.list(None, 50, false).unwrap();
        let (all, _) = repo.list(None, 50, true).unwrap();
        assert!(visible.is_empty());
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn insert_populates_mentions_and_people_when_extracted_has_people() {
        let repo = repo();
        repo.insert(input(json!({
            "people": [{"canonical_id": "person_ada", "raw_text": "Ada"}]
        })))
        .unwrap();
        let people = repo.list_people().unwrap();
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].canonical_id, "person_ada");
        assert_eq!(people[0].mention_count, 1);
    }

    #[test]
    fn insert_populates_actions_with_well_known_fields_and_preserves_unknowns_in_raw_status_json() {
        let repo = repo();
        repo.insert(input(json!({
            "actions": [{
                "text": "Send demo",
                "assignee_canonical_id": "person_ada",
                "due_at": "2026-05-01T00:00:00Z",
                "status": {"phase": "open", "custom": "kept"},
                "tags": ["demo"],
                "shape_id": "followup"
            }]
        })))
        .unwrap();
        let actions = repo.view_actions(ActionFilter::default()).unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].text, "Send demo");
        assert_eq!(actions[0].phase.as_deref(), Some("open"));
        assert_eq!(actions[0].raw_status["custom"], "kept");
    }

    #[test]
    fn rebuild_views_is_idempotent_and_recovers_from_truncated_caches() {
        let repo = repo();
        repo.insert(input(json!({
            "people": [{"canonical_id": "person_ada", "raw_text": "Ada"}],
            "actions": [{"text": "Call Ada", "phase": "open"}]
        })))
        .unwrap();
        repo.conn.execute("DELETE FROM mentions", []).unwrap();
        repo.conn.execute("DELETE FROM actions", []).unwrap();
        let first = repo.rebuild_views().unwrap();
        let second = repo.rebuild_views().unwrap();
        assert_eq!(first.mentions, 1);
        assert_eq!(first.actions, 1);
        assert_eq!(second.mentions, 1);
        assert_eq!(second.actions, 1);
    }

    #[test]
    fn view_today_returns_open_actions_with_no_due_or_due_in_past() {
        let repo = repo();
        repo.insert(input(json!({
            "actions": [
                {"text": "No due", "phase": "open"},
                {"text": "Past due", "phase": "in_progress", "due_at": "2026-05-01T00:00:00Z"},
                {"text": "Done", "phase": "done"}
            ]
        })))
        .unwrap();
        let rows = repo.view_today("2026-05-28T00:00:00Z").unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn view_today_excludes_snoozed_future_actions() {
        let repo = repo();
        repo.insert(input(json!({
            "actions": [
                {"text": "Later", "phase": "open", "snoozed_until": "2026-06-01T00:00:00Z"}
            ]
        })))
        .unwrap();
        let rows = repo.view_today("2026-05-28T00:00:00Z").unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn view_project_joins_through_mentions() {
        let repo = repo();
        let inserted = repo
            .insert(input(json!({
                "project": {"canonical_id": "project_fieldcraft", "slug": "fieldcraft", "raw_text": "Fieldcraft"}
            })))
            .unwrap();
        let rows = repo.view_project("project_fieldcraft", 50).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, inserted.id);
    }

    // -----------------------------------------------------------------------
    // Entity hygiene tests
    // -----------------------------------------------------------------------

    fn person_input(raw: &str) -> MemoryInput {
        MemoryInput {
            text: format!("Talk to {raw}"),
            audio_ref: None,
            source: "typed".into(),
            extracted: json!({ "people": [raw] }),
        }
    }

    fn find_person<'a>(people: &'a [Person], id: &str) -> Option<&'a Person> {
        people.iter().find(|p| p.canonical_id == id)
    }

    #[test]
    fn rename_person_updates_display_name_and_bumps_revision() {
        let repo = repo();
        repo.insert(person_input("Dalma")).unwrap();
        let before = find_person(&repo.list_people().unwrap(), "dalma")
            .unwrap()
            .clone();
        let renamed = repo.rename_person("dalma", "Dalma Kovacs").unwrap();
        assert_eq!(renamed.display_name, "Dalma Kovacs");
        assert_eq!(renamed.revision, before.revision + 1);
        // canonical_id and mentions untouched.
        assert_eq!(renamed.canonical_id, "dalma");
        assert_eq!(renamed.mention_count, 1);
    }

    #[test]
    fn rename_person_missing_returns_not_found() {
        let repo = repo();
        let err = repo.rename_person("nobody", "Nobody").unwrap_err();
        assert!(matches!(err, MemoryError::NotFound));
    }

    #[test]
    fn merge_people_unions_aliases_reattributes_mentions_and_actions() {
        let repo = repo();
        // "Dalma" mentioned twice; misheard "Delma" once with an action assigned.
        repo.insert(person_input("Dalma")).unwrap();
        repo.insert(person_input("Dalma")).unwrap();
        repo.insert(MemoryInput {
            text: "Delma owes a reply".into(),
            audio_ref: None,
            source: "typed".into(),
            extracted: json!({
                "people": ["Delma"],
                "actions": [{"text": "Ping back", "phase": "open", "assignee_canonical_id": "delma"}]
            }),
        })
        .unwrap();

        // Sanity: split exists.
        let people = repo.list_people().unwrap();
        assert_eq!(people.len(), 2);
        assert_eq!(find_person(&people, "dalma").unwrap().mention_count, 2);
        assert_eq!(find_person(&people, "delma").unwrap().mention_count, 1);

        let merged = repo.merge_people("dalma", &["delma".to_string()]).unwrap();

        // Survivor has all 3 mentions; merged-away row gone.
        let people = repo.list_people().unwrap();
        assert_eq!(people.len(), 1);
        assert_eq!(merged.canonical_id, "dalma");
        assert_eq!(merged.mention_count, 3);
        assert!(find_person(&people, "delma").is_none());

        // Aliases unioned (display name "Delma" folded in).
        let aliases: Vec<String> = serde_json::from_value(merged.aliases.clone()).unwrap();
        assert!(aliases.iter().any(|a| a == "Dalma"));
        assert!(aliases.iter().any(|a| a == "Delma"));

        // Action reattributed to survivor.
        let actions = repo
            .view_actions(ActionFilter {
                assignee: Some("dalma".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].text, "Ping back");
        // None remain attributed to the merged-away id.
        let orphaned = repo
            .view_actions(ActionFilter {
                assignee: Some("delma".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(orphaned.is_empty());
    }

    #[test]
    fn merge_people_survives_rebuild_views() {
        let repo = repo();
        repo.insert(person_input("Dalma")).unwrap();
        repo.insert(person_input("Delma")).unwrap();
        repo.merge_people("dalma", &["delma".to_string()]).unwrap();
        assert_eq!(repo.list_people().unwrap().len(), 1);

        // The whole point: re-derivation from blobs must NOT resurrect "delma".
        repo.rebuild_views().unwrap();
        let people = repo.list_people().unwrap();
        assert_eq!(people.len(), 1, "rebuild must not resurrect merged entity");
        assert_eq!(people[0].canonical_id, "dalma");
        assert_eq!(people[0].mention_count, 2);
    }

    #[test]
    fn merge_people_refuses_self_merge() {
        let repo = repo();
        repo.insert(person_input("Dalma")).unwrap();
        let err = repo
            .merge_people("dalma", &["dalma".to_string()])
            .unwrap_err();
        assert!(matches!(err, MemoryError::Invalid(_)));
    }

    #[test]
    fn merge_people_missing_into_returns_not_found() {
        let repo = repo();
        repo.insert(person_input("Delma")).unwrap();
        let err = repo
            .merge_people("dalma", &["delma".to_string()])
            .unwrap_err();
        assert!(matches!(err, MemoryError::NotFound));
    }

    #[test]
    fn rename_project_updates_display_name_and_bumps_revision() {
        let repo = repo();
        repo.insert(input(json!({
            "project": {"canonical_id": "fieldcraft", "slug": "fieldcraft", "raw_text": "Fieldcraft"}
        })))
        .unwrap();
        let renamed = repo.rename_project("fieldcraft", "Field Craft").unwrap();
        assert_eq!(renamed.display_name, "Field Craft");
        assert_eq!(renamed.revision, 2);
        assert_eq!(renamed.slug, "fieldcraft");
    }

    #[test]
    fn merge_projects_unions_aliases_and_survives_rebuild() {
        let repo = repo();
        repo.insert(input(json!({
            "project": {"canonical_id": "fieldcraft", "slug": "fieldcraft", "raw_text": "Fieldcraft"}
        })))
        .unwrap();
        repo.insert(input(json!({
            "project": {"canonical_id": "feeldcraft", "slug": "feeldcraft", "raw_text": "Feeldcraft"}
        })))
        .unwrap();
        assert_eq!(repo.list_projects().unwrap().len(), 2);

        let merged = repo
            .merge_projects("fieldcraft", &["feeldcraft".to_string()])
            .unwrap();
        assert_eq!(merged.canonical_id, "fieldcraft");
        assert_eq!(merged.mention_count, 2);
        let aliases: Vec<String> = serde_json::from_value(merged.aliases.clone()).unwrap();
        assert!(aliases.iter().any(|a| a == "Feeldcraft"));

        repo.rebuild_views().unwrap();
        let projects = repo.list_projects().unwrap();
        assert_eq!(
            projects.len(),
            1,
            "rebuild must not resurrect merged project"
        );
        assert_eq!(projects[0].canonical_id, "fieldcraft");
        assert_eq!(projects[0].mention_count, 2);
    }

    #[test]
    fn merge_projects_refuses_self_merge() {
        let repo = repo();
        repo.insert(input(json!({
            "project": {"canonical_id": "fieldcraft", "slug": "fieldcraft", "raw_text": "Fieldcraft"}
        })))
        .unwrap();
        let err = repo
            .merge_projects("fieldcraft", &["fieldcraft".to_string()])
            .unwrap_err();
        assert!(matches!(err, MemoryError::Invalid(_)));
    }
}
