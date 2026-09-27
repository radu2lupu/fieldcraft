use std::collections::HashSet;

use fieldcraft_core::TrError;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::repository::KnowledgeRepository;
use crate::schema::sqlite_error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisReport {
    pub generated_at: String,
    pub window_days: u32,
    pub summary: ReportSummary,
    pub noise_candidates: Vec<LessonFinding>,
    pub stale_workhorses: Vec<LessonFinding>,
    pub contradicted: Vec<LessonFinding>,
    pub ghosts: Vec<LessonFinding>,
    pub per_project: Vec<ProjectStats>,
    pub suggested_actions: Vec<SuggestedAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReportSummary {
    pub total_lessons: i64,
    pub total_outcome_events: i64,
    pub confirmed_total: i64,
    pub contradicted_total: i64,
    pub ignored_total: i64,
    pub records_with_dynconf: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LessonFinding {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub project_id: String,
    pub confidence: f32,
    pub dynamic_confidence: Option<f32>,
    pub metric: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectStats {
    pub project_id: String,
    pub lessons: i64,
    pub recent_writes: i64,
    pub recent_retrievals: i64,
    pub recent_confirmed: i64,
    pub recent_contradicted: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestedAction {
    pub kind: String,
    pub lesson_id: String,
    pub title: String,
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleReport {
    pub generated_at: String,
    pub min_age_days: i64,
    pub candidates: Vec<LifecycleCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleCandidate {
    pub id: String,
    pub category: String,
    pub reason: String,
}

pub fn analyze_report(
    repo: &KnowledgeRepository,
    days: u32,
    limit: usize,
) -> Result<AnalysisReport, TrError> {
    let conn = repo.connection();
    let window_clause = format!("datetime('now', '-{} days')", days);

    let summary = ReportSummary {
        total_lessons: scalar_i64(conn, "SELECT COUNT(*) FROM knowledge")?,
        total_outcome_events: scalar_i64(conn, "SELECT COUNT(*) FROM lesson_outcome_events")?,
        confirmed_total: scalar_i64(
            conn,
            "SELECT COUNT(*) FROM lesson_outcome_events WHERE outcome='confirmed'",
        )?,
        contradicted_total: scalar_i64(
            conn,
            "SELECT COUNT(*) FROM lesson_outcome_events WHERE outcome='contradicted'",
        )?,
        ignored_total: scalar_i64(
            conn,
            "SELECT COUNT(*) FROM lesson_outcome_events WHERE outcome='ignored'",
        )?,
        records_with_dynconf: scalar_i64(
            conn,
            "SELECT COUNT(*) FROM knowledge WHERE dynamic_confidence IS NOT NULL",
        )?,
    };

    let noise_candidates = query_lesson_findings(
        conn,
        &format!(
            "SELECT k.id, k.title, k.kind, k.project_id, k.confidence, k.dynamic_confidence,
                    SUM(CASE WHEN e.outcome='confirmed' THEN 1 ELSE 0 END) AS confirmed,
                    COUNT(e.id) AS total
             FROM knowledge k
             JOIN lesson_outcome_events e ON e.knowledge_id = k.id
             WHERE e.created_at >= {window_clause}
             GROUP BY k.id
             HAVING total >= 5 AND CAST(confirmed AS REAL) / total < 0.2
             ORDER BY total DESC LIMIT ?1"
        ),
        limit,
        |confirmed, total| format!("{confirmed}/{total} confirmed in last {days}d"),
    )?;

    let stale_workhorses = query_stale_workhorses(conn, &window_clause, limit)?;

    let contradicted = query_lesson_findings(
        conn,
        &format!(
            "SELECT k.id, k.title, k.kind, k.project_id, k.confidence, k.dynamic_confidence,
                    COUNT(e.id) AS contradictions, 0 AS unused
             FROM knowledge k
             JOIN lesson_outcome_events e ON e.knowledge_id = k.id
             WHERE e.outcome='contradicted' AND e.created_at >= {window_clause}
             GROUP BY k.id
             ORDER BY contradictions DESC LIMIT ?1"
        ),
        limit,
        |contradictions, _| format!("{contradictions} contradictions in last {days}d"),
    )?;

    let ghosts = query_ghosts(conn, limit)?;

    let mut per_project = query_per_project(conn, &window_clause)?;
    per_project.retain(|p| {
        p.recent_retrievals > 0
            || p.recent_writes > 0
            || p.recent_confirmed > 0
            || p.recent_contradicted > 0
    });
    per_project.truncate(limit.saturating_mul(2));

    let mut suggested_actions = Vec::new();
    for f in &noise_candidates {
        suggested_actions.push(SuggestedAction {
            kind: "archive".to_string(),
            lesson_id: f.id.clone(),
            title: f.title.clone(),
            rationale: format!("noise candidate: {}", f.metric),
        });
    }
    for f in &contradicted {
        suggested_actions.push(SuggestedAction {
            kind: "investigate".to_string(),
            lesson_id: f.id.clone(),
            title: f.title.clone(),
            rationale: format!(
                "contradicted: {}. Read the lesson_outcome_events.note to learn why and decide whether to supersede or archive.",
                f.metric
            ),
        });
    }
    for f in &stale_workhorses {
        suggested_actions.push(SuggestedAction {
            kind: "rewrite_cues".to_string(),
            lesson_id: f.id.clone(),
            title: f.title.clone(),
            rationale: format!(
                "stale workhorse: {} — likely a cue-extraction failure causing over-retrieval without uptake. Inspect the lesson's cue_bundle.",
                f.metric
            ),
        });
    }
    for f in &ghosts {
        suggested_actions.push(SuggestedAction {
            kind: "prune".to_string(),
            lesson_id: f.id.clone(),
            title: f.title.clone(),
            rationale: format!(
                "ghost: {}. Nobody has ever retrieved this; safe to archive unless intentionally cold-stored.",
                f.metric
            ),
        });
    }

    Ok(AnalysisReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        window_days: days,
        summary,
        noise_candidates,
        stale_workhorses,
        contradicted,
        ghosts,
        per_project,
        suggested_actions,
    })
}

pub fn lifecycle_report(
    repo: &KnowledgeRepository,
    min_age_days: i64,
) -> Result<LifecycleReport, TrError> {
    let mut candidates = repo.archive_candidates(min_age_days)?;
    let mut seen: HashSet<String> = candidates.iter().map(|c| c.id.clone()).collect();
    for c in repo.rif_demotion_candidates(3)? {
        if seen.insert(c.id.clone()) {
            candidates.push(c);
        }
    }
    Ok(LifecycleReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        min_age_days,
        candidates: candidates
            .into_iter()
            .map(|candidate| LifecycleCandidate {
                id: candidate.id,
                category: candidate.category.to_string(),
                reason: candidate.reason,
            })
            .collect(),
    })
}

pub fn render_analysis_report_stdout(report: &AnalysisReport) -> String {
    let mut out = String::new();
    pushln(&mut out, "=== Fieldcraft Audit Report ===");
    pushln(&mut out, format!("Generated: {}", report.generated_at));
    pushln(
        &mut out,
        format!("Window:    last {} days", report.window_days),
    );
    pushln(&mut out, "");
    pushln(&mut out, "== Summary ==");
    pushln(
        &mut out,
        format!(
            "  total lessons:           {}",
            report.summary.total_lessons
        ),
    );
    pushln(
        &mut out,
        format!(
            "  total outcome events:    {}",
            report.summary.total_outcome_events
        ),
    );
    pushln(
        &mut out,
        format!(
            "    confirmed:             {}",
            report.summary.confirmed_total
        ),
    );
    pushln(
        &mut out,
        format!(
            "    contradicted:          {}",
            report.summary.contradicted_total
        ),
    );
    pushln(
        &mut out,
        format!(
            "    ignored:               {}",
            report.summary.ignored_total
        ),
    );
    pushln(
        &mut out,
        format!(
            "  records with dyn_conf:   {}",
            report.summary.records_with_dynconf
        ),
    );
    pushln(&mut out, "");

    render_findings(
        &mut out,
        "Noise candidates (propose: archive)",
        &report.noise_candidates,
    );
    render_findings(
        &mut out,
        "Stale workhorses (propose: rewrite_cues)",
        &report.stale_workhorses,
    );
    render_findings(
        &mut out,
        "Contradicted (propose: investigate)",
        &report.contradicted,
    );
    render_findings(&mut out, "Ghosts >30d (propose: prune)", &report.ghosts);

    pushln(&mut out, "== Per-project (window) ==");
    for p in &report.per_project {
        pushln(
            &mut out,
            format!(
                "  {:<24} lessons={:<5} writes={:<4} retr={:<5} conf={:<3} contr={}",
                p.project_id,
                p.lessons,
                p.recent_writes,
                p.recent_retrievals,
                p.recent_confirmed,
                p.recent_contradicted
            ),
        );
    }
    pushln(&mut out, "");
    pushln(
        &mut out,
        format!(
            "== Suggested actions: {} ==",
            report.suggested_actions.len()
        ),
    );
    out
}

pub fn render_lifecycle_report_stdout(
    report: &LifecycleReport,
    mode: &str,
    apply_summary: Option<(usize, usize)>,
) -> String {
    let mut out = String::new();
    if report.candidates.is_empty() {
        pushln(
            &mut out,
            format!(
                "lifecycle: no archive candidates (min_age_days={}).",
                report.min_age_days
            ),
        );
        return out;
    }

    pushln(
        &mut out,
        format!(
            "lifecycle [{mode}]: {} archive candidate(s) (min_age_days={})",
            report.candidates.len(),
            report.min_age_days
        ),
    );
    for c in &report.candidates {
        pushln(
            &mut out,
            format!("  [{}] {}: {}", c.category, c.id, c.reason),
        );
    }
    if let Some((archived, skipped)) = apply_summary {
        pushln(
            &mut out,
            format!(
                "lifecycle: archived {archived}, skipped {skipped}. Every archive is reversible \
                 (tier=archive + audit row); restore with restore_record."
            ),
        );
    } else {
        pushln(
            &mut out,
            "lifecycle: dry run only, nothing written. Re-run with --apply to archive.",
        );
    }
    out
}

fn scalar_i64(conn: &Connection, sql: &str) -> Result<i64, TrError> {
    conn.query_row(sql, [], |row| row.get(0))
        .map_err(sqlite_error)
}

fn query_lesson_findings(
    conn: &Connection,
    sql: &str,
    limit: usize,
    metric_fmt: impl Fn(i64, i64) -> String,
) -> Result<Vec<LessonFinding>, TrError> {
    let mut stmt = conn.prepare(sql).map_err(sqlite_error)?;
    let rows = stmt
        .query_map(rusqlite::params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, f64>(4)? as f32,
                row.get::<_, Option<f64>>(5)?.map(|v| v as f32),
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })
        .map_err(sqlite_error)?;
    let mut out = Vec::new();
    for row in rows {
        let (id, title, kind, project_id, confidence, dyn_conf, n1, n2) =
            row.map_err(sqlite_error)?;
        out.push(LessonFinding {
            id,
            title,
            kind,
            project_id,
            confidence,
            dynamic_confidence: dyn_conf,
            metric: metric_fmt(n1, n2),
        });
    }
    Ok(out)
}

fn query_stale_workhorses(
    conn: &Connection,
    window_clause: &str,
    limit: usize,
) -> Result<Vec<LessonFinding>, TrError> {
    let sql = format!(
        "WITH retrieval_counts AS (
           SELECT je.value AS id, COUNT(*) AS retrievals
           FROM retrieval_log, json_each(retrieval_log.result_ids) je
           WHERE retrieval_log.created_at >= {window_clause}
           GROUP BY je.value
         ),
         recent_confirms AS (
           SELECT knowledge_id, COUNT(*) AS confirms
           FROM lesson_outcome_events
           WHERE outcome='confirmed' AND created_at >= {window_clause}
           GROUP BY knowledge_id
         )
         SELECT k.id, k.title, k.kind, k.project_id, k.confidence, k.dynamic_confidence,
                rc.retrievals, COALESCE(c.confirms, 0) AS recent_confirms
         FROM knowledge k
         JOIN retrieval_counts rc ON rc.id = k.id
         LEFT JOIN recent_confirms c ON c.knowledge_id = k.id
         WHERE rc.retrievals >= 10 AND COALESCE(c.confirms, 0) = 0
         ORDER BY rc.retrievals DESC LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql).map_err(sqlite_error)?;
    let rows = stmt
        .query_map(rusqlite::params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, f64>(4)? as f32,
                row.get::<_, Option<f64>>(5)?.map(|v| v as f32),
                row.get::<_, i64>(6)?,
            ))
        })
        .map_err(sqlite_error)?;
    let mut out = Vec::new();
    for row in rows {
        let (id, title, kind, project_id, confidence, dyn_conf, retrievals) =
            row.map_err(sqlite_error)?;
        out.push(LessonFinding {
            id,
            title,
            kind,
            project_id,
            confidence,
            dynamic_confidence: dyn_conf,
            metric: format!("{retrievals} retrievals, 0 confirms"),
        });
    }
    Ok(out)
}

fn query_ghosts(conn: &Connection, limit: usize) -> Result<Vec<LessonFinding>, TrError> {
    let mut stmt = conn
        .prepare(
            "SELECT k.id, k.title, k.kind, k.project_id, k.confidence, k.dynamic_confidence,
                k.created_at
         FROM knowledge k
         WHERE k.id NOT IN (
           SELECT DISTINCT je.value FROM retrieval_log, json_each(retrieval_log.result_ids) je
         )
         AND k.created_at < datetime('now', '-30 days')
         ORDER BY k.created_at ASC LIMIT ?1",
        )
        .map_err(sqlite_error)?;
    let rows = stmt
        .query_map(rusqlite::params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, f64>(4)? as f32,
                row.get::<_, Option<f64>>(5)?.map(|v| v as f32),
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(sqlite_error)?;
    let mut out = Vec::new();
    for row in rows {
        let (id, title, kind, project_id, confidence, dyn_conf, created_at) =
            row.map_err(sqlite_error)?;
        let age_date = created_at
            .split('T')
            .next()
            .unwrap_or(&created_at)
            .to_string();
        out.push(LessonFinding {
            id,
            title,
            kind,
            project_id,
            confidence,
            dynamic_confidence: dyn_conf,
            metric: format!("never retrieved; created {age_date}"),
        });
    }
    Ok(out)
}

fn query_per_project(conn: &Connection, window_clause: &str) -> Result<Vec<ProjectStats>, TrError> {
    let sql = format!(
        "SELECT
            k.project_id,
            COUNT(DISTINCT k.id) AS lessons,
            SUM(CASE WHEN k.created_at >= {window_clause} THEN 1 ELSE 0 END) AS recent_writes,
            (SELECT COUNT(*) FROM retrieval_log r
             WHERE r.project_id = k.project_id AND r.created_at >= {window_clause}) AS recent_retrievals,
            (SELECT COUNT(*) FROM lesson_outcome_events e
             JOIN knowledge k2 ON k2.id = e.knowledge_id
             WHERE k2.project_id = k.project_id AND e.outcome='confirmed' AND e.created_at >= {window_clause}) AS recent_confirmed,
            (SELECT COUNT(*) FROM lesson_outcome_events e
             JOIN knowledge k2 ON k2.id = e.knowledge_id
             WHERE k2.project_id = k.project_id AND e.outcome='contradicted' AND e.created_at >= {window_clause}) AS recent_contradicted
         FROM knowledge k
         GROUP BY k.project_id
         ORDER BY recent_retrievals DESC, lessons DESC"
    );
    let mut stmt = conn.prepare(&sql).map_err(sqlite_error)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ProjectStats {
                project_id: row.get::<_, String>(0)?,
                lessons: row.get::<_, i64>(1)?,
                recent_writes: row.get::<_, i64>(2)?,
                recent_retrievals: row.get::<_, i64>(3)?,
                recent_confirmed: row.get::<_, i64>(4)?,
                recent_contradicted: row.get::<_, i64>(5)?,
            })
        })
        .map_err(sqlite_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
}

fn render_findings(out: &mut String, label: &str, findings: &[LessonFinding]) {
    pushln(out, format!("== {label}: {} ==", findings.len()));
    if findings.is_empty() {
        pushln(out, "  (none)");
    } else {
        for f in findings {
            let dyn_str = match f.dynamic_confidence {
                Some(d) => format!("{d:.2}"),
                None => "—".to_string(),
            };
            pushln(
                out,
                format!(
                    "  [{kind:<10}] {title} ({metric})",
                    kind = f.kind,
                    title = truncate(&f.title, 60),
                    metric = f.metric
                ),
            );
            pushln(
                out,
                format!(
                    "      id={} project={} conf={:.2}/{dyn_str}",
                    &f.id[..12.min(f.id.len())],
                    f.project_id,
                    f.confidence
                ),
            );
        }
    }
    pushln(out, "");
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

fn pushln(out: &mut String, line: impl AsRef<str>) {
    out.push_str(line.as_ref());
    out.push('\n');
}
