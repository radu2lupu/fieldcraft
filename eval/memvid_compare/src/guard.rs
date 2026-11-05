//! Time-leakage guard: a query only sees lessons created at or before the query.
//! Archived lessons stay in; only `created_at` decides visibility.

use chrono::{DateTime, Utc};

pub fn parse_ts(s: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| format!("bad timestamp {s:?}: {e}"))
}

/// True when a lesson created at `lesson_at` may be returned for a query issued at `query_at`.
pub fn visible(lesson_at: DateTime<Utc>, query_at: DateTime<Utc>) -> bool {
    lesson_at <= query_at
}

/// Number of lessons visible at `query_at` when `created` is sorted ascending.
pub fn visible_prefix(created: &[DateTime<Utc>], query_at: DateTime<Utc>) -> usize {
    created.partition_point(|c| visible(*c, query_at))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        parse_ts(s).unwrap()
    }

    #[test]
    fn boundary_is_inclusive_and_future_is_hidden() {
        let q = t("2026-08-01T12:00:00.000000+00:00");
        assert!(visible(t("2026-08-01T12:00:00+00:00"), q));
        assert!(visible(t("2026-07-31T23:59:59.999999+00:00"), q));
        assert!(!visible(t("2026-08-01T12:00:00.000001+00:00"), q));
    }

    #[test]
    fn offsets_are_compared_as_instants() {
        let q = t("2026-08-01T12:00:00+00:00");
        assert!(visible(t("2026-08-01T13:00:00+02:00"), q));
        assert!(!visible(t("2026-08-01T12:30:00+00:00"), q));
    }

    #[test]
    fn prefix_counts_only_past_lessons() {
        let created = vec![
            t("2026-01-01T00:00:00+00:00"),
            t("2026-02-01T00:00:00+00:00"),
            t("2026-03-01T00:00:00+00:00"),
        ];
        assert_eq!(visible_prefix(&created, t("2025-12-31T00:00:00+00:00")), 0);
        assert_eq!(visible_prefix(&created, t("2026-02-01T00:00:00+00:00")), 2);
        assert_eq!(visible_prefix(&created, t("2027-01-01T00:00:00+00:00")), 3);
    }
}
