use sqlx::{Row, SqlitePool};

use crate::services::auto_search::{MatchProvenance, history_summary};
use crate::services::source::ClassificationResult;

/// One entry in the cross-series "needs review" list. Carries just enough
/// to render a row: series identity for the link, episode number, and the
/// current (uncertain) classification for context. Produced by
/// [`get_needs_review`].
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct NeedsReviewEntry {
    pub series_id: i64,
    pub series_anilist_id: i64,
    /// Default-language title (English with romaji fallback). Kept for
    /// callers that don't honor the user's title-language preference.
    /// The Needs Review UI uses the three variants below to drive the
    /// `.title-switcher` CSS pattern; this field stays as a no-JS
    /// fallback.
    pub series_title: String,
    /// Title variants for the language-preference switcher. Empty
    /// strings when the source row didn't carry that variant; the
    /// template's `{% if !empty %}…{% else %}fallback{% endif %}`
    /// chain mirrors what library-card titles do.
    pub series_title_english: String,
    pub series_title_romaji: String,
    pub series_title_native: String,
    pub cover_url: String,
    pub episode_number: i32,
    pub quality_tag: String,
    pub release_title: String,
    pub release_group: String,
    pub source: String,
    pub resolution: String,
    /// Sonarr-parity BD variant flags. Surfaced on Needs Review so the
    /// inline-override dropdown can pre-fill `bluray_remux` / `bluray_bdmv`
    /// when the original verdict was the more specific variant — without
    /// these the pre-fill collapsed to plain `bluray` and the user lost
    /// the variant on every re-pick.
    pub is_remux: bool,
    pub is_bdmv: bool,
    /// Web sub-tier (`WEBDL` / `WEBRip`, or empty for Unknown). Same role
    /// as `is_remux` / `is_bdmv`: lets the inline pre-fill resolve
    /// `webrip` instead of plain `web` when that's what the classifier
    /// actually produced.
    pub web_kind: String,
    pub classification_confidence: f32,
    /// Serialized `Vec<SourceEvidence>` captured at classification time.
    /// Rendered inline by the Needs-Review UI so the user can see *why*
    /// the row was flagged without running a fresh classify. Empty string
    /// for legacy rows grabbed before the column existed.
    pub classification_evidence: String,
}

/// Return every episode currently flagged `needs_review = true` across the
/// entire library, joined with its series info. Used by the Phase 4
/// "Needs review" list view. Excludes rows the user has already manually
/// overridden (manual_override = 1 clears `needs_review` too, but we
/// filter defensively in case an older row has both set).
pub async fn get_needs_review(db: &SqlitePool) -> Result<Vec<NeedsReviewEntry>, sqlx::Error> {
    sqlx::query_as::<_, NeedsReviewEntry>(
        "SELECT t.series_id, t.episode_number, t.quality_tag, t.release_title, t.release_group,
                t.source, t.resolution,
                t.is_remux,
                COALESCE(t.is_bdmv, 0) AS is_bdmv,
                COALESCE(t.web_kind, '') AS web_kind,
                t.classification_confidence,
                COALESCE(t.classification_evidence, '') AS classification_evidence,
                s.anilist_id AS series_anilist_id,
                COALESCE(NULLIF(s.title_english, ''), NULLIF(s.title_romaji, ''), s.title) AS series_title,
                COALESCE(s.title_english, '') AS series_title_english,
                COALESCE(s.title_romaji, '') AS series_title_romaji,
                COALESCE(s.title_native, '') AS series_title_native,
                s.cover_url
         FROM episode_quality_tags t
         JOIN series s ON s.id = t.series_id
         WHERE t.needs_review = 1
           AND COALESCE(t.manual_override, 0) = 0
         ORDER BY s.title_english, t.episode_number",
    )
    .fetch_all(db)
    .await
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema, sqlx::FromRow)]
pub struct GrabHistoryEntry {
    pub id: i64,
    pub quality_tag: String,
    pub release_title: String,
    pub release_group: String,
    /// Post-processed on-disk file name for this episode. Seeded with
    /// the Nyaa release title at grab time, then overwritten with the
    /// Sonarr-style renamed file once post-processing imports the
    /// episode (e.g. `Jujutsu Kaisen - S01E06 - Hidden Inventory.mkv`).
    /// This is distinct from `release_title`, which carries the batch
    /// torrent's title unchanged even for per-episode rows of a pack.
    pub file_name: String,
    /// Size reported at grab time. For batch grabs this is the **whole
    /// torrent** total (same value replicated across every episode row
    /// of the batch — the episode detail modal reads it as "this came
    /// from an X GiB batch"). For single-episode grabs it is refined to
    /// the imported file's true size at post-process time. Zero only
    /// for pre-migration rows or cases where the size was never known.
    pub size_bytes: i64,
    /// True when the originating grab was a batch/pack. Used by the UI
    /// to decide whether `size_bytes` represents a whole-torrent total
    /// or a single-file size.
    pub is_batch: bool,
    pub grabbed_at: String,
    pub state: String,
    /// Client-side path of the completed torrent
    /// (`grabbed_torrents.client_content_path`). Empty until the
    /// post-processing sweep observes the torrent as complete. Sourced
    /// via a correlated subquery on this row's `release_title` +
    /// `series_id` so each grab history row shows the client path of
    /// *its* torrent, not the most recent one. Sonarr-parity dual-path
    /// tracking: this is the "DownloadClientItem.OutputPath" side;
    /// the `file_name` column above is the library path.
    ///
    /// Wire-level field name matches the DB column post-#63. The series
    /// modal reads `current.client_content_path` to populate the
    /// "output path" row when post-processing has stamped a result.
    #[serde(default)]
    #[sqlx(default)]
    pub client_content_path: String,
    /// Misgrab guardrails: how the release title matched the series at
    /// grab time. Empty strings and 0 for rows written before the
    /// columns existed and for paths that carry no provenance (RSS,
    /// autobrr, the picker).
    #[serde(default)]
    #[sqlx(default)]
    pub match_kind: String,
    #[serde(default)]
    #[sqlx(default)]
    pub match_phase: String,
    #[serde(default)]
    #[sqlx(default)]
    pub matched_alias: String,
    #[serde(default)]
    #[sqlx(default)]
    pub match_ratio: f64,
    /// Readable sentence built from the four columns; empty for legacy
    /// rows. The episode modal shows it under the release title.
    #[serde(default)]
    #[sqlx(default)]
    pub grab_match_summary: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(dead_code)]
pub struct EpisodeQualityTag {
    /// Convenience for callers that hold a `Vec<EpisodeQualityTag>` and
    /// want to know which episode a row belongs to without rebuilding
    /// a HashMap. `get_for_series` still returns a HashMap keyed by
    /// this value for the in-memory lookup hot path.
    pub episode_number: i32,
    pub quality_tag: String,
    pub release_title: String,
    pub release_group: String,
    pub state: String,
    /// Structured source label ("BluRay", "Web", "DVD", "HDTV", "TV",
    /// "Unknown", or empty for rows grabbed before Phase 1b).
    pub source: String,
    /// Structured resolution label ("1080p", "720p", …, or empty).
    pub resolution: String,
    pub is_remux: bool,
    /// Sonarr-parity: true when the release is a raw BDMV / BD-Raw
    /// disc-structure release (distinct from `is_remux`). Mutually
    /// exclusive with `is_remux` at the label level.
    pub is_bdmv: bool,
    /// Sonarr-parity: WEB-DL vs WEBRip sub-classification when the
    /// filename was specific enough to tell. Empty string for legacy
    /// bare-WEB rows or non-Web sources.
    pub web_kind: String,
    pub classification_confidence: f32,
    pub needs_review: bool,
    /// True when the user has pinned this classification via the manual
    /// override picker. Prevents `update_classification` from overwriting
    /// on subsequent post-download re-classifies.
    pub manual_override: bool,
    /// Serialized `Vec<SourceEvidence>` captured at classification time.
    /// Empty string for legacy rows and for manually-overridden rows.
    /// Consumers (the Needs-Review UI) `serde_json::from_str` to rehydrate.
    pub classification_evidence: String,
    /// ISO 8601 timestamp of the most recent post-download (full-pipeline)
    /// classification attempt. Set by `update_classification` and by the
    /// post-classify `record_grab` paths (post-processing import + the
    /// library scan), left NULL by grab-time `record_grab` writes.
    /// Issue #53: the library sweep uses `is_some()` here on
    /// empty/"unknown" source rows as the "already tried, leave alone"
    /// guard, so a file the classifier can't decide on doesn't get
    /// re-ffprobed every six hours forever.
    pub classification_attempted_at: Option<String>,
}

/// Record a new grab for an episode — inserts into history and upserts the
/// current tag.
///
/// The legacy `quality_tag` column is populated from `classification.label()`
/// so existing read paths continue to work; the new structured columns
/// (source, resolution, is_remux, classification_confidence, needs_review)
/// are written alongside it on `episode_quality_tags`. `manual_override` is
/// intentionally preserved across re-grabs — a user-set override should
/// stick even when a newer grab comes in.
#[allow(clippy::too_many_arguments)]
pub async fn record_grab(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
    classification: &ClassificationResult,
    release_title: &str,
    release_group: &str,
    size_bytes: i64,
    is_batch: bool,
) -> Result<i64, sqlx::Error> {
    record_grab_with_match(
        db,
        series_id,
        episode_number,
        classification,
        release_title,
        release_group,
        size_bytes,
        is_batch,
        None,
    )
    .await
}

/// `record_grab` plus the match provenance the search pipeline stamped
/// on the winning candidate (misgrab guardrails). The auto-search,
/// upgrade, and interactive grab paths use this; everything else goes
/// through `record_grab` and leaves the provenance columns empty.
#[allow(clippy::too_many_arguments)]
pub async fn record_grab_with_match(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
    classification: &ClassificationResult,
    release_title: &str,
    release_group: &str,
    size_bytes: i64,
    is_batch: bool,
    provenance: Option<&MatchProvenance>,
) -> Result<i64, sqlx::Error> {
    let (match_kind, match_phase, matched_alias, match_ratio) = match provenance {
        Some(p) => (
            p.kind.as_str(),
            p.phase.as_str(),
            p.alias.as_str(),
            f64::from(p.ratio),
        ),
        None => ("", "", "", 0.0),
    };
    let quality_tag = classification.label();
    let source_str = classification.source.as_str();
    let resolution_str = classification.resolution.as_str();
    let is_remux = if classification.is_remux {
        1_i64
    } else {
        0_i64
    };
    let is_bdmv = if classification.is_bdmv { 1_i64 } else { 0_i64 };
    let web_kind_str = classification.web_kind.as_str();
    let confidence = classification.confidence as f64;
    let needs_review = if classification.needs_review {
        1_i64
    } else {
        0_i64
    };
    // Serialize the full evidence trail so the Needs-Review UI can audit
    // *why* the row was flagged without re-running classification. Empty
    // string on serialize failure — the row is still valid, we just lose
    // the trail on that particular write.
    let evidence_json = serde_json::to_string(&classification.evidence).unwrap_or_default();

    // Seed `file_name` with the Nyaa release title. For non-batch grabs
    // post-processing later overwrites it with the Sonarr-style renamed
    // on-disk file. For batch grabs each per-episode row gets its own
    // on-disk file name too, populated from the landed filename rather
    // than the batch torrent's title. `size_bytes` is whatever Nyaa
    // reported for the whole torrent — for a batch it stays as the
    // pack total (every episode row of the batch has the same value);
    // for a single-episode it gets refined to the per-file size at
    // import time.
    let is_batch_i: i64 = if is_batch { 1 } else { 0 };

    // Wrap both writes in a single transaction so we never end up with a
    // history row but no quality_tags row. Pre-transaction, an error
    // between the two INSERTs would commit the history side and leave
    // the tag side empty — the OP ep 1157 case in production. The
    // tag-overflow rendering path (build_episodes' Pass 2) iterates
    // `quality_tags`, so a missing tag silently de-renders the row's
    // grabbed state and the user can't open grab history for the
    // episode. Rolling back both on failure keeps the two tables in
    // lockstep.
    let mut tx = db.begin().await?;

    let history_id: i64 = sqlx::query_scalar(
        "INSERT INTO episode_grab_history (series_id, episode_number, quality_tag, release_title, release_group, file_name, size_bytes, is_batch, state, match_kind, match_phase, matched_alias, match_ratio)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'grabbed', ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(series_id)
    .bind(episode_number)
    .bind(&quality_tag)
    .bind(release_title)
    .bind(release_group)
    .bind(release_title)
    .bind(size_bytes)
    .bind(is_batch_i)
    .bind(match_kind)
    .bind(match_phase)
    .bind(matched_alias)
    .bind(match_ratio)
    .fetch_one(&mut *tx)
    .await?;

    // The `WHERE COALESCE(manual_override, 0) = 0` guard mirrors
    // `update_classification`: if the user has pinned a classification on
    // this episode, re-grabs must not silently overwrite it. Without the
    // guard the row would end up internally inconsistent — manual_override
    // still flipped on, but the columns it's supposed to protect replaced
    // by the automatic classifier's verdict. An upgrade re-grab on a
    // pinned row is a no-op on the tag row; the grab history row still
    // records the event unconditionally above.
    sqlx::query(
        "INSERT INTO episode_quality_tags (
             series_id, episode_number, quality_tag, release_title, release_group, state,
             source, resolution, is_remux, is_bdmv, web_kind,
             classification_confidence, needs_review, classification_evidence
         )
         VALUES (?, ?, ?, ?, ?, 'grabbed', ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(series_id, episode_number) DO UPDATE SET
             quality_tag = excluded.quality_tag,
             release_title = excluded.release_title,
             release_group = excluded.release_group,
             state = 'grabbed',
             source = excluded.source,
             resolution = excluded.resolution,
             is_remux = excluded.is_remux,
             is_bdmv = excluded.is_bdmv,
             web_kind = excluded.web_kind,
             classification_confidence = excluded.classification_confidence,
             needs_review = excluded.needs_review,
             classification_evidence = excluded.classification_evidence,
             updated_at = CURRENT_TIMESTAMP
         WHERE COALESCE(episode_quality_tags.manual_override, 0) = 0",
    )
    .bind(series_id)
    .bind(episode_number)
    .bind(&quality_tag)
    .bind(release_title)
    .bind(release_group)
    .bind(source_str)
    .bind(resolution_str)
    .bind(is_remux)
    .bind(is_bdmv)
    .bind(web_kind_str)
    .bind(confidence)
    .bind(needs_review)
    .bind(&evidence_json)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(history_id)
}

/// Get the current quality tag map for a series.
pub async fn get_for_series(
    db: &SqlitePool,
    series_id: i64,
) -> Result<std::collections::HashMap<i32, EpisodeQualityTag>, sqlx::Error> {
    let rows: Vec<EpisodeQualityTag> = sqlx::query_as(
        "SELECT episode_number, quality_tag, release_title, release_group, state,
                source, resolution, is_remux,
                COALESCE(is_bdmv, 0) AS is_bdmv,
                COALESCE(web_kind, '') AS web_kind,
                classification_confidence, needs_review,
                COALESCE(manual_override, 0) AS manual_override,
                COALESCE(classification_evidence, '') AS classification_evidence,
                classification_attempted_at
         FROM episode_quality_tags WHERE series_id = ?",
    )
    .bind(series_id)
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(|t| (t.episode_number, t)).collect())
}

/// Library-wide slice of active tag states for the index page's
/// per-card completeness bars: every (series_id, episode_number,
/// state) row whose state is 'completed' (counts toward downloaded,
/// unioned with on-disk files) or 'grabbed' (flips the card into the
/// downloading state). Failed tags are irrelevant to the bar — a
/// failed grab leaves the episode missing — so they're filtered
/// server-side to keep the row count proportional to the library.
pub async fn active_states_all_series(
    db: &SqlitePool,
) -> Result<Vec<(i64, i32, String)>, sqlx::Error> {
    sqlx::query_as::<_, (i64, i32, String)>(
        "SELECT series_id, episode_number, state FROM episode_quality_tags
         WHERE state IN ('completed', 'grabbed')",
    )
    .fetch_all(db)
    .await
}

/// Get grab history for a specific episode, newest first. No LIMIT — the
/// modal UI scrolls past the first 10 entries, and there are no known
/// series with enough grabs for an unbounded SELECT to be a problem.
pub async fn get_grab_history(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
) -> Result<Vec<GrabHistoryEntry>, sqlx::Error> {
    let mut rows = sqlx::query_as::<_, GrabHistoryEntry>(
        "SELECT egh.id,
                egh.quality_tag,
                egh.release_title,
                egh.release_group,
                COALESCE(egh.file_name, '') AS file_name,
                COALESCE(egh.size_bytes, 0) AS size_bytes,
                COALESCE(egh.is_batch, 0) AS is_batch,
                egh.grabbed_at,
                egh.state,
                COALESCE(egh.match_kind, '') AS match_kind,
                COALESCE(egh.match_phase, '') AS match_phase,
                COALESCE(egh.matched_alias, '') AS matched_alias,
                COALESCE(egh.match_ratio, 0) AS match_ratio,
                COALESCE((
                    SELECT gt.client_content_path
                      FROM grabbed_torrents gt
                     WHERE gt.series_id = egh.series_id
                       AND gt.torrent_name = egh.release_title
                       AND COALESCE(gt.client_content_path, '') <> ''
                     ORDER BY gt.grabbed_at DESC
                     LIMIT 1
                ), '') AS client_content_path
         FROM episode_grab_history egh
         WHERE egh.series_id = ? AND egh.episode_number = ?
         ORDER BY egh.grabbed_at DESC",
    )
    .bind(series_id)
    .bind(episode_number)
    .fetch_all(db)
    .await?;
    for row in &mut rows {
        row.grab_match_summary = history_summary(
            &row.match_kind,
            &row.match_phase,
            &row.matched_alias,
            row.match_ratio,
        );
    }
    Ok(rows)
}

/// Overwrite the structured classification columns on an existing tag row.
/// Called after post-download classification (Layer 5 + Layer 6) produces a
/// verdict that may differ from the pre-download one. Preserves
/// `release_title`, `release_group`, `state`, and — crucially — any
/// `manual_override` the user has set. Rows with `manual_override = 1` are
/// left entirely alone: the user's explicit tag wins over the classifier.
///
/// Also refreshes the legacy `quality_tag` string from the new classification
/// so any UI that still reads that column picks up the post-download update.
pub async fn update_classification(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
    classification: &ClassificationResult,
) -> Result<(), sqlx::Error> {
    let quality_tag = classification.label();
    let source_str = classification.source.as_str();
    let resolution_str = classification.resolution.as_str();
    let is_remux = if classification.is_remux {
        1_i64
    } else {
        0_i64
    };
    let is_bdmv = if classification.is_bdmv { 1_i64 } else { 0_i64 };
    let web_kind_str = classification.web_kind.as_str();
    let confidence = classification.confidence as f64;
    let needs_review = if classification.needs_review {
        1_i64
    } else {
        0_i64
    };
    let evidence_json = serde_json::to_string(&classification.evidence).unwrap_or_default();

    sqlx::query(
        "UPDATE episode_quality_tags SET
             quality_tag = ?,
             source = ?,
             resolution = ?,
             is_remux = ?,
             is_bdmv = ?,
             web_kind = ?,
             classification_confidence = ?,
             needs_review = ?,
             classification_evidence = ?,
             classification_attempted_at = CURRENT_TIMESTAMP,
             updated_at = CURRENT_TIMESTAMP
         WHERE series_id = ?
           AND episode_number = ?
           AND COALESCE(manual_override, 0) = 0",
    )
    .bind(&quality_tag)
    .bind(source_str)
    .bind(resolution_str)
    .bind(is_remux)
    .bind(is_bdmv)
    .bind(web_kind_str)
    .bind(confidence)
    .bind(needs_review)
    .bind(&evidence_json)
    .bind(series_id)
    .bind(episode_number)
    .execute(db)
    .await?;

    Ok(())
}

/// Stamp `classification_attempted_at = CURRENT_TIMESTAMP` on a row.
/// Issue #53: called from the post-classify `record_grab` paths in
/// `services/post_processing.rs` so the library scan can tell "the
/// full-pipeline classifier saw this file" apart from "we've never
/// tried with the file in hand."
///
/// Skipped on `manual_override = 1` rows for symmetry with the rest of
/// the classification helpers — a user-pinned row's "attempt" timestamp
/// would just be misleading.
pub async fn stamp_classification_attempted(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE episode_quality_tags SET classification_attempted_at = CURRENT_TIMESTAMP
         WHERE series_id = ?
           AND episode_number = ?
           AND COALESCE(manual_override, 0) = 0",
    )
    .bind(series_id)
    .bind(episode_number)
    .execute(db)
    .await?;
    Ok(())
}

/// Apply a user's manual classification override for an episode. The row is
/// upserted (inserted if it doesn't exist yet, e.g. for externally-imported
/// files the classifier hasn't seen) with `manual_override = 1`, which
/// prevents `update_classification` from overwriting it on re-classify.
/// `needs_review` is cleared because the user has explicitly resolved it.
///
/// If `source` is empty, the override is removed: the row is updated with
/// `manual_override = 0` and kept otherwise intact, so the next
/// post-download classify pass is free to overwrite it.
#[allow(clippy::too_many_arguments)]
pub async fn set_manual_override(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
    source: &str,
    resolution: &str,
    is_remux: bool,
    is_bdmv: bool,
    web_kind: &str,
) -> Result<(), sqlx::Error> {
    if source.is_empty() {
        // Clear override. Semantics chosen 2026-04-15: read as
        // unclassified/missing unless a pre-existing grab already
        // classified this episode. If a live grab exists (grabbed or
        // completed state), fall back to its classification; otherwise
        // the row is deleted so the UI shows the episode as missing.
        //
        // Re-derivation uses `classify_release_sync` over the stored
        // release title — the grab_history row doesn't carry the
        // structured `(source, resolution, is_remux, …)` columns, so
        // we re-parse from the release title. This is weaker than a
        // full post-download classify (no group/ffprobe/dir layers)
        // but the result is persisted once and does NOT self-heal:
        // `scan_library_for_unclassified` skips rows with non-empty
        // `source` (see `services/post_processing.rs` around the
        // "Skip when ... a tag exists with a non-empty source" guard),
        // so the 6h sweep leaves these rows alone. A stronger
        // reclassification requires a new grab (which goes through
        // `record_grab`'s full pipeline) or a manual re-override.
        let existing = sqlx::query(
            "SELECT release_title FROM episode_grab_history
             WHERE series_id = ? AND episode_number = ?
               AND state IN ('grabbed', 'completed')
             ORDER BY grabbed_at DESC LIMIT 1",
        )
        .bind(series_id)
        .bind(episode_number)
        .fetch_optional(db)
        .await?;

        match existing {
            None => {
                // No live grab — delete the tag row entirely so the
                // episode reads as missing/unclassified.
                sqlx::query(
                    "DELETE FROM episode_quality_tags
                     WHERE series_id = ? AND episode_number = ?",
                )
                .bind(series_id)
                .bind(episode_number)
                .execute(db)
                .await?;
            }
            Some(row) => {
                let release_title: String = row.get("release_title");
                let fallback = crate::services::source::classify_release_sync(&release_title, None);
                let derived_tag = fallback.label();
                let src_str = fallback.source.as_str();
                let res_str = fallback.resolution.as_str();
                let remux_i = if fallback.is_remux { 1_i64 } else { 0_i64 };
                let bdmv_i = if fallback.is_bdmv { 1_i64 } else { 0_i64 };
                let wk_str = fallback.web_kind.as_str();
                let conf = fallback.confidence as f64;
                let nr_i = if fallback.needs_review { 1_i64 } else { 0_i64 };
                sqlx::query(
                    "UPDATE episode_quality_tags SET
                         manual_override = 0,
                         quality_tag = ?,
                         source = ?,
                         resolution = ?,
                         is_remux = ?,
                         is_bdmv = ?,
                         web_kind = ?,
                         classification_confidence = ?,
                         needs_review = ?,
                         classification_evidence = '',
                         updated_at = CURRENT_TIMESTAMP
                     WHERE series_id = ? AND episode_number = ?",
                )
                .bind(&derived_tag)
                .bind(src_str)
                .bind(res_str)
                .bind(remux_i)
                .bind(bdmv_i)
                .bind(wk_str)
                .bind(conf)
                .bind(nr_i)
                .bind(series_id)
                .bind(episode_number)
                .execute(db)
                .await?;
            }
        }
        return Ok(());
    }

    // Build a `ClassificationResult` from the manual fields and reuse its
    // `label()` — keeps the rendering rules in exactly one place so the
    // BDMV/Remux/WebKind precedence can't drift between automatic and
    // manual paths.
    let parsed_source = crate::services::source::Source::from_str(source);
    let parsed_resolution = crate::services::source::Resolution::from_str(resolution);
    let parsed_web_kind = crate::services::source::WebKind::from_str(web_kind);
    let synthetic = crate::services::source::ClassificationResult {
        source: parsed_source,
        resolution: parsed_resolution,
        is_remux,
        is_bdmv,
        web_kind: parsed_web_kind,
        confidence: 1.0,
        needs_review: false,
        evidence: Vec::new(),
        decision_rule: crate::services::source::DecisionRule::Empty,
    };
    let label = synthetic.label();
    let is_remux_i = if is_remux { 1_i64 } else { 0_i64 };
    let is_bdmv_i = if is_bdmv { 1_i64 } else { 0_i64 };
    let web_kind_str = parsed_web_kind.as_str();

    // Upsert: if the row doesn't exist yet (user tagging a file that the
    // classifier never saw), insert a fresh row with empty release metadata.
    // If it exists, flip to manual_override and overwrite the classification
    // columns with the user's choice. We explicitly blank the stored
    // evidence trail — the user's pin isn't backed by layer evidence, so
    // keeping a stale trail from a previous automatic classify would be
    // misleading.
    sqlx::query(
        "INSERT INTO episode_quality_tags (
             series_id, episode_number, quality_tag, release_title, release_group, state,
             source, resolution, is_remux, is_bdmv, web_kind,
             classification_confidence, needs_review, manual_override, classification_evidence
         )
         VALUES (?, ?, ?, '', '', 'completed', ?, ?, ?, ?, ?, 1.0, 0, 1, '')
         ON CONFLICT(series_id, episode_number) DO UPDATE SET
             quality_tag = excluded.quality_tag,
             source = excluded.source,
             resolution = excluded.resolution,
             is_remux = excluded.is_remux,
             is_bdmv = excluded.is_bdmv,
             web_kind = excluded.web_kind,
             classification_confidence = 1.0,
             needs_review = 0,
             manual_override = 1,
             classification_evidence = '',
             updated_at = CURRENT_TIMESTAMP",
    )
    .bind(series_id)
    .bind(episode_number)
    .bind(&label)
    // Bind the *parsed* enum's canonical .as_str() instead of the raw
    // input — defense in depth alongside the handler validation, so a
    // future caller that bypasses the handler can't write a non-
    // canonical string to the DB and confuse downstream column-vs-enum
    // comparisons.
    .bind(parsed_source.as_str())
    .bind(parsed_resolution.as_str())
    .bind(is_remux_i)
    .bind(is_bdmv_i)
    .bind(web_kind_str)
    .execute(db)
    .await?;
    Ok(())
}

/// Mark episode quality tags as "completed" for the given episodes of a series.
/// Called by post-processing after a torrent is successfully imported.
///
/// Single UPDATE with `episode_number IN (?, ?, ...)` instead of one
/// query per episode — a batch import of a 24-episode BD pack used to
/// fire 24 round-trips here, all of which serialise behind SQLite's
/// single-writer lock.
pub async fn mark_completed(
    db: &SqlitePool,
    series_id: i64,
    episode_numbers: &[i32],
) -> Result<(), sqlx::Error> {
    if episode_numbers.is_empty() {
        return Ok(());
    }
    // Build the IN-list placeholders at runtime; episode numbers are
    // i32s from trusted upstream parsing, no injection surface. sqlx
    // doesn't expand slice bindings on SQLite, so we splice the
    // placeholder count into the SQL and bind each value.
    let placeholders = std::iter::repeat_n("?", episode_numbers.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "UPDATE episode_quality_tags
         SET state = 'completed', updated_at = CURRENT_TIMESTAMP
         WHERE series_id = ?
           AND state = 'grabbed'
           AND episode_number IN ({})",
        placeholders
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(series_id);
    for &ep in episode_numbers {
        q = q.bind(ep);
    }
    q.execute(db).await?;
    Ok(())
}

/// Flip the newest 'grabbed' `episode_grab_history` row for an episode to
/// 'completed', stamping in the Sonarr-style post-processed on-disk file
/// name and (for non-batch rows only) refining `size_bytes` to the
/// imported file's true size. Called by post-processing once per imported
/// file, immediately before / alongside `mark_completed`.
///
/// Only the latest grabbed row for that episode is touched — older rows
/// from previous grabs stay as-is (they'll be 'grabbed' forever or were
/// already marked 'failed'/'removed' by the upgrade/removal path).
///
/// `file_name` is the on-disk basename after post-processing's rename
/// step (e.g. `Jujutsu Kaisen - S01E06 - Hidden Inventory.mkv`).
/// `file_size_bytes` is the single imported file's size.
///
/// The `size_bytes` CASE guard enforces the invariant that batch rows
/// continue to hold the whole-torrent total (not the per-episode file
/// size), so the episode detail modal can surface "this episode came
/// from an X GiB batch" without losing the pack total on import.
pub async fn mark_grab_history_completed(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
    file_name: &str,
    file_size_bytes: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE episode_grab_history
         SET state = 'completed',
             file_name = ?,
             size_bytes = CASE WHEN COALESCE(is_batch, 0) = 1 THEN size_bytes ELSE ? END
         WHERE id = (
             SELECT id FROM episode_grab_history
             WHERE series_id = ? AND episode_number = ? AND state = 'grabbed'
             ORDER BY grabbed_at DESC
             LIMIT 1
         )",
    )
    .bind(file_name)
    .bind(file_size_bytes)
    .bind(series_id)
    .bind(episode_number)
    .execute(db)
    .await?;
    Ok(())
}

/// Clear the current quality tag for an episode (e.g. after file deletion).
///
/// Split behavior by `manual_override`:
/// - Non-pinned rows: DELETE the row entirely. No pin to preserve.
/// - Pinned rows: UPDATE `state = ''` but keep the classification
///   columns (source, resolution, is_remux, is_bdmv, web_kind,
///   manual_override). The pin protects the user's *classification*
///   assertion; the file being gone is a separate fact that must
///   reflect in `state` so the series page's `downloaded` flag
///   (on_disk OR state == 'completed') stops rendering a checkmark
///   for a file that no longer exists.
pub async fn clear_episode_tag(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM episode_quality_tags
         WHERE series_id = ? AND episode_number = ?
           AND COALESCE(manual_override, 0) = 0",
    )
    .bind(series_id)
    .bind(episode_number)
    .execute(db)
    .await?;
    sqlx::query(
        "UPDATE episode_quality_tags SET state = '', updated_at = CURRENT_TIMESTAMP
         WHERE series_id = ? AND episode_number = ?
           AND COALESCE(manual_override, 0) = 1",
    )
    .bind(series_id)
    .bind(episode_number)
    .execute(db)
    .await?;
    Ok(())
}

/// Flip the latest `completed` grab_history row for (series_id, ep) to
/// `replaced` — the per-episode counterpart of
/// `grabbed_torrents::mark_replaced`. Called by post-processing when an
/// upgrade lands on an episode that already had an import, so the
/// episode detail modal can distinguish "the old grab was superseded"
/// from "the old grab was user-cancelled" (which stays `removed`).
///
/// Only touches the most recent `completed` row — older entries from
/// prior grab cycles stay as-is (they've already gone through their
/// own lifecycle). If no `completed` row exists (orphan upgrade case
/// covered by post_processing's disk-as-truth path), this is a no-op.
pub async fn mark_grab_history_replaced(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE episode_grab_history
         SET state = 'replaced'
         WHERE id = (
             SELECT id FROM episode_grab_history
             WHERE series_id = ? AND episode_number = ? AND state = 'completed'
             ORDER BY grabbed_at DESC
             LIMIT 1
         )",
    )
    .bind(series_id)
    .bind(episode_number)
    .execute(db)
    .await?;
    Ok(())
}

/// Flip the latest `completed` (or in-flight `grabbed`) grab_history
/// row for (series_id, ep) to `removed` — the per-episode counterpart
/// of `grabbed_torrents::mark_removed`. Called by `delete_episode_file`
/// (user removed the file from disk via the modal) and by the bulk
/// removal sweep further down. Mirrors `mark_grab_history_replaced`'s
/// shape; the `state IN (...)` filter is the difference: replaced
/// only fires from a successful upgrade-import (so the prior row is
/// always `completed`), while removal can land on either an in-flight
/// `grabbed` row OR an already-`completed` row depending on whether
/// post-processing has moved the file yet.
///
/// Older entries from prior grab cycles stay as-is — they've already
/// gone through their own lifecycle (`replaced`, `failed`, etc.).
pub async fn mark_grab_history_removed(
    db: &SqlitePool,
    series_id: i64,
    episode_number: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE episode_grab_history
         SET state = 'removed'
         WHERE id = (
             SELECT id FROM episode_grab_history
             WHERE series_id = ? AND episode_number = ?
               AND state IN ('grabbed', 'completed')
             ORDER BY grabbed_at DESC
             LIMIT 1
         )",
    )
    .bind(series_id)
    .bind(episode_number)
    .execute(db)
    .await?;
    Ok(())
}

/// Clear episode quality tags and mark grab history as "removed" for all episodes
/// associated with a grabbed torrent (identified by series_id + episode_numbers).
///
/// Rows with `manual_override = 1` are kept — blocklisting a release must
/// not silently destroy a pinned tag. Unlike [`clear_episode_tag`] this
/// does NOT clear `state` on pinned rows: the torrent going away doesn't
/// imply the file left disk (post-processing may have already moved it).
/// The grab_history state flip to 'removed' still runs unconditionally;
/// that history is about the release, not the episode's ground truth.
pub async fn clear_tags_for_removal(
    db: &SqlitePool,
    series_id: i64,
    episode_numbers: &[i32],
) -> Result<(), sqlx::Error> {
    for &ep in episode_numbers {
        // Delete the current quality tag so the episode no longer shows as grabbed.
        sqlx::query(
            "DELETE FROM episode_quality_tags
             WHERE series_id = ? AND episode_number = ?
               AND COALESCE(manual_override, 0) = 0",
        )
        .bind(series_id)
        .bind(ep)
        .execute(db)
        .await?;

        // Mark any in-flight `grabbed` OR already-imported `completed`
        // history entries for this episode as `removed`. Earlier this
        // filter only matched `state = 'grabbed'`, so a user delete
        // landing AFTER post-processing had advanced the row to
        // `completed` left the history showing a stale "completed"
        // forever — visible in the episode-detail Grab History modal
        // even though the file was gone from disk.
        sqlx::query(
            "UPDATE episode_grab_history SET state = 'removed'
             WHERE series_id = ? AND episode_number = ?
               AND state IN ('grabbed', 'completed')",
        )
        .bind(series_id)
        .bind(ep)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// Mark a grab history entry as failed, and update the current tag state if it matches.
pub async fn mark_grab_failed(
    db: &SqlitePool,
    history_id: i64,
) -> Result<(i64, i32, String), sqlx::Error> {
    // Fetch series_id, episode_number, release_title before marking failed.
    let row = sqlx::query(
        "SELECT series_id, episode_number, release_title FROM episode_grab_history WHERE id = ?",
    )
    .bind(history_id)
    .fetch_one(db)
    .await?;
    let series_id: i64 = row.get("series_id");
    let episode_number: i32 = row.get("episode_number");
    let release_title: String = row.get("release_title");

    sqlx::query("UPDATE episode_grab_history SET state = 'failed' WHERE id = ?")
        .bind(history_id)
        .execute(db)
        .await?;

    // Update the current tag to 'failed' so the UI reflects the state.
    sqlx::query(
        "UPDATE episode_quality_tags SET state = 'failed', updated_at = CURRENT_TIMESTAMP
         WHERE series_id = ? AND episode_number = ?",
    )
    .bind(series_id)
    .bind(episode_number)
    .execute(db)
    .await?;

    Ok((series_id, episode_number, release_title))
}

/// Misgrab guardrails: fail every `grabbed` history row this release
/// wrote for the series (the grab's own episodes plus any auto-expand
/// backfill) and the matching quality tags, keyed by release title
/// rather than history id because the sweep holds the grab row, not
/// the history ids.
pub async fn mark_grab_failed_for_release(
    db: &SqlitePool,
    series_id: i64,
    release_title: &str,
) -> Result<u64, sqlx::Error> {
    let episodes: Vec<i32> = sqlx::query_scalar(
        "SELECT episode_number FROM episode_grab_history \
         WHERE series_id = ? AND release_title = ? AND state = 'grabbed'",
    )
    .bind(series_id)
    .bind(release_title)
    .fetch_all(db)
    .await?;
    let result = sqlx::query(
        "UPDATE episode_grab_history SET state = 'failed' \
         WHERE series_id = ? AND release_title = ? AND state = 'grabbed'",
    )
    .bind(series_id)
    .bind(release_title)
    .execute(db)
    .await?;
    for ep in episodes {
        sqlx::query(
            "UPDATE episode_quality_tags SET state = 'failed', updated_at = CURRENT_TIMESTAMP \
             WHERE series_id = ? AND episode_number = ? AND state = 'grabbed'",
        )
        .bind(series_id)
        .bind(ep)
        .execute(db)
        .await?;
    }
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::series;
    use crate::services::source::{Resolution, Source};

    async fn seed_series(db: &SqlitePool) -> i64 {
        let (id, _) = series::upsert(
            db,
            series::SeriesCore {
                anilist_id: 1,
                mal_id: None,
                title: "Test Series",
                title_romaji: "Test Series",
                title_english: "Test Series",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(12),
                season_year: Some(2020),
                end_year: Some(2020),
            },
        )
        .await
        .expect("series upsert");
        id
    }

    fn synthetic_classification(source: Source, resolution: Resolution) -> ClassificationResult {
        ClassificationResult {
            source,
            resolution,
            is_remux: false,
            is_bdmv: false,
            web_kind: crate::services::source::WebKind::Unknown,
            confidence: 1.0,
            needs_review: false,
            evidence: Vec::new(),
            decision_rule: crate::services::source::DecisionRule::Empty,
        }
    }

    /// Regression: `mark_grab_history_removed` must flip the latest
    /// `completed` row to `removed`. Earlier the deletion path only
    /// ran an UPDATE filtered by `state = 'grabbed'`, which left the
    /// post-processed `completed` row stuck in `completed` state in
    /// the Grab History modal forever after the user removed the file.
    /// Reproduced against `data/ryokan.db`'s ep 12 of Mob Psycho III.
    #[tokio::test]
    async fn mark_grab_history_removed_flips_latest_completed_row() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let sid = seed_series(&db).await;

        // Seed two history rows for episode 5: an older `completed`
        // (from a re-grab cycle) plus the latest `completed`. The
        // helper should only flip the latest, leaving the older
        // historical entry untouched.
        sqlx::query(
            "INSERT INTO episode_grab_history
                (series_id, episode_number, quality_tag, release_title, state, grabbed_at)
             VALUES (?, ?, ?, ?, 'completed', '2026-01-01 00:00:00')",
        )
        .bind(sid)
        .bind(5)
        .bind("WEB-1080p")
        .bind("[Group] Old Release")
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO episode_grab_history
                (series_id, episode_number, quality_tag, release_title, state, grabbed_at)
             VALUES (?, ?, ?, ?, 'completed', '2026-05-01 00:00:00')",
        )
        .bind(sid)
        .bind(5)
        .bind("BD-1080p")
        .bind("[Group] Latest Release")
        .execute(&db)
        .await
        .unwrap();

        mark_grab_history_removed(&db, sid, 5).await.unwrap();

        let states: Vec<(String, String)> = sqlx::query_as(
            "SELECT release_title, state FROM episode_grab_history
             WHERE series_id = ? AND episode_number = ?
             ORDER BY grabbed_at ASC",
        )
        .bind(sid)
        .bind(5)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(states.len(), 2);
        assert_eq!(states[0].1, "completed", "older row stays completed");
        assert_eq!(states[1].1, "removed", "latest row flips to removed");
    }

    /// Regression: an in-flight grab must surface a non-empty `state`
    /// through `get_for_series`. `record_grab` upserts the current tag as
    /// `state = 'grabbed'`; the series-page handler clones that into the
    /// episode's `quality_state` (the grab-tag branch in
    /// `handlers/library/pages`), and `series.js`'s poll loop clears the
    /// progress bar whenever `quality_state` comes back empty. If the
    /// loader's SELECT ever dropped the `state` column or mis-keyed the
    /// row by episode, an actively-downloading episode would flash to
    /// "Missing" mid-download. See the PR #170 review + fix 863a0b8.
    #[tokio::test]
    async fn grabbed_episode_has_nonempty_state_via_loader() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let sid = seed_series(&db).await;

        let cls = synthetic_classification(Source::Web, Resolution::R1080p);
        record_grab(
            &db,
            sid,
            3,
            &cls,
            "synthetic-release-token",
            "synthetic-group",
            1_000_000,
            false,
        )
        .await
        .expect("record_grab");

        let tags = get_for_series(&db, sid).await.expect("get_for_series");
        let tag = tags.get(&3).expect("episode 3 tag present after grab");
        assert_eq!(
            tag.state, "grabbed",
            "in-flight grab must round-trip with a non-empty state; an empty \
             quality_state flashes the series-page progress bar to Missing"
        );
    }

    /// Companion to `mark_grab_history_removed_flips_latest_completed_row`
    /// — pins the same `state IN ('grabbed', 'completed')` filter
    /// broadening on the bulk-removal path. Earlier the WHERE clause
    /// was `state = 'grabbed'` only, so a `clear_tags_for_removal`
    /// call landing AFTER post-processing had advanced a history row
    /// to `completed` left the row stuck at `completed` in the
    /// Grab History modal.
    #[tokio::test]
    async fn clear_tags_for_removal_flips_completed_history_to_removed() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let sid = seed_series(&db).await;

        // Pre-seed an `episode_grab_history` row in `state = 'completed'`
        // — the pre-fix WHERE clause would skip this; the broadened
        // clause must flip it.
        sqlx::query(
            "INSERT INTO episode_grab_history
                (series_id, episode_number, quality_tag, release_title, state, grabbed_at)
             VALUES (?, ?, ?, ?, 'completed', '2026-04-25 00:00:00')",
        )
        .bind(sid)
        .bind(7)
        .bind("BD-1080p")
        .bind("[Group] Pre-completed Release")
        .execute(&db)
        .await
        .unwrap();

        clear_tags_for_removal(&db, sid, &[7])
            .await
            .expect("clear tags for removal");

        let states: Vec<String> = sqlx::query_scalar(
            "SELECT state FROM episode_grab_history
             WHERE series_id = ? AND episode_number = ?",
        )
        .bind(sid)
        .bind(7)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            states,
            vec!["removed".to_string()],
            "completed history row must flip to 'removed' under the broadened filter"
        );
    }

    /// Bug A: clear_tags_for_removal previously deleted every row
    /// regardless of manual_override. Blocklisting a release would
    /// silently destroy a user's pinned classification.
    #[tokio::test]
    async fn clear_tags_for_removal_preserves_manual_override() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let sid = seed_series(&db).await;

        set_manual_override(&db, sid, 1, "BluRay", "1080p", false, false, "")
            .await
            .expect("set manual override");

        clear_tags_for_removal(&db, sid, &[1])
            .await
            .expect("clear tags for removal");

        let tags = get_for_series(&db, sid).await.expect("get for series");
        let tag = tags.get(&1).expect("tag row must survive");
        assert!(tag.manual_override, "manual_override row was wiped");
        assert_eq!(tag.source, "BluRay");
        assert_eq!(tag.resolution, "1080p");
    }

    /// Bug A: clear_episode_tag previously deleted every row
    /// regardless of manual_override. Deleting a file on disk would
    /// silently destroy a pinned classification.
    #[tokio::test]
    async fn clear_episode_tag_preserves_manual_override() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let sid = seed_series(&db).await;

        set_manual_override(&db, sid, 1, "BluRay", "1080p", false, false, "")
            .await
            .expect("set manual override");

        clear_episode_tag(&db, sid, 1).await.expect("clear tag");

        let tags = get_for_series(&db, sid).await.expect("get for series");
        assert!(tags.contains_key(&1), "manual_override row was wiped");
    }

    /// PR #35 review: pin an episode's classification, then delete the
    /// file on disk. The pin must survive (classification columns
    /// intact, manual_override stays 1) but `state` must clear so the
    /// series-page `downloaded` flag (on_disk OR state == 'completed')
    /// stops rendering a checkmark for a file that no longer exists.
    #[tokio::test]
    async fn clear_episode_tag_resets_state_on_pinned_row() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let sid = seed_series(&db).await;

        // Grab + complete, then pin. record_grab writes state='grabbed'
        // and the full classification; mark_completed flips to completed.
        let cls = synthetic_classification(Source::Web, Resolution::R1080p);
        record_grab(
            &db,
            sid,
            1,
            &cls,
            "[SubsPlease] Test - 01 (1080p).mkv",
            "SubsPlease",
            1_000_000,
            false,
        )
        .await
        .expect("record grab");
        mark_completed(&db, sid, &[1])
            .await
            .expect("mark completed");
        set_manual_override(&db, sid, 1, "BluRay", "1080p", false, false, "")
            .await
            .expect("pin override");

        // Sanity: state should be 'completed' (set_manual_override
        // upserts the row with state='completed' via its INSERT branch).
        let before = get_for_series(&db, sid).await.expect("get");
        assert!(before[&1].manual_override);
        assert_eq!(before[&1].state, "completed");

        // User deletes the file on disk — the handler calls clear_episode_tag.
        clear_episode_tag(&db, sid, 1).await.expect("clear tag");

        let after = get_for_series(&db, sid).await.expect("get");
        let row = after.get(&1).expect("pinned row must survive");
        assert!(row.manual_override, "pin must survive");
        assert_eq!(row.source, "BluRay", "classification must survive");
        assert_eq!(row.resolution, "1080p", "classification must survive");
        assert_eq!(
            row.state, "",
            "state must clear so downloaded-flag stops rendering checkmark"
        );
    }

    /// Bug B: clearing an override on an episode with no live grab
    /// deletes the row entirely so the episode reads as missing.
    #[tokio::test]
    async fn clear_manual_override_without_grab_deletes_row() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let sid = seed_series(&db).await;

        set_manual_override(&db, sid, 1, "BluRay", "1080p", false, false, "")
            .await
            .expect("set manual override");

        // No grab history — clearing must delete the row.
        set_manual_override(&db, sid, 1, "", "", false, false, "")
            .await
            .expect("clear manual override");

        let tags = get_for_series(&db, sid).await.expect("get for series");
        assert!(
            !tags.contains_key(&1),
            "row should have been deleted when no live grab exists"
        );
    }

    /// Bug B: clearing an override on an episode *with* a prior grab
    /// reverts to the automatic classification rather than leaving
    /// the pinned values in place with `manual_override = 0`.
    #[tokio::test]
    async fn clear_manual_override_with_grab_reverts_to_classification() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let sid = seed_series(&db).await;

        // Record a WEB grab first so episode_grab_history has a live row.
        let web_cls = synthetic_classification(Source::Web, Resolution::R1080p);
        record_grab(
            &db,
            sid,
            1,
            &web_cls,
            "[SubsPlease] Test - 01 (1080p) [abcd].mkv",
            "SubsPlease",
            1_000_000,
            false,
        )
        .await
        .expect("record grab");

        // User pins it as BluRay.
        set_manual_override(&db, sid, 1, "BluRay", "1080p", false, false, "")
            .await
            .expect("set manual override");

        // User clears the pin. Should fall back to a WEB-style
        // classification derived from the release title, not stay as BluRay.
        set_manual_override(&db, sid, 1, "", "", false, false, "")
            .await
            .expect("clear manual override");

        let tags = get_for_series(&db, sid).await.expect("get for series");
        let tag = tags.get(&1).expect("row must survive with live grab");
        assert!(!tag.manual_override, "override flag must clear");
        assert_ne!(
            tag.source, "BluRay",
            "cleared override must not retain the pinned BluRay source"
        );
    }

    /// After bug B fix: a fresh record_grab after a set_manual_override
    /// still respects the pin (the existing `WHERE manual_override = 0`
    /// guard in record_grab). Defensive — the fix to Bug B's clearing
    /// branch must not affect the pinned-vs-re-grab interaction.
    #[tokio::test]
    async fn manual_override_still_wins_over_later_grab() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let sid = seed_series(&db).await;

        set_manual_override(&db, sid, 1, "BluRay", "1080p", false, false, "")
            .await
            .expect("set manual override");

        let web_cls = synthetic_classification(Source::Web, Resolution::R720p);
        record_grab(
            &db,
            sid,
            1,
            &web_cls,
            "[SubsPlease] Test - 01 (720p).mkv",
            "SubsPlease",
            500_000,
            false,
        )
        .await
        .expect("record grab");

        let tags = get_for_series(&db, sid).await.expect("get for series");
        let tag = tags.get(&1).expect("tag row");
        assert!(tag.manual_override, "override must survive re-grab");
        assert_eq!(
            tag.source, "BluRay",
            "pinned source must not be overwritten"
        );
        assert_eq!(
            tag.resolution, "1080p",
            "pinned resolution must not be overwritten"
        );
    }

    /// Regression for the post-#63 field rename: the JSON field name
    /// returned by `get_grab_history` must be `client_content_path`,
    /// not the legacy `qbit_content_path`. The series modal's
    /// `renderGrabHistory` reads `current.client_content_path` to
    /// populate the "Output path" row, and a silent rename here
    /// would hide the whole row with no UI signal.
    #[test]
    fn grab_history_entry_serializes_client_content_path() {
        let entry = GrabHistoryEntry {
            id: 1,
            quality_tag: "WEB 1080p".to_string(),
            release_title: "[Group] Show - 01 [WEB][1080p].mkv".to_string(),
            release_group: "Group".to_string(),
            file_name: "Show - S01E01.mkv".to_string(),
            size_bytes: 1_234_567,
            is_batch: false,
            grabbed_at: "2026-04-24T00:00:00Z".to_string(),
            state: "grabbed".to_string(),
            client_content_path: "/downloads/Show - 01.mkv".to_string(),
            match_kind: String::new(),
            match_phase: String::new(),
            matched_alias: String::new(),
            match_ratio: 0.0,
            grab_match_summary: String::new(),
        };
        let v = serde_json::to_value(&entry).unwrap();
        assert_eq!(v["client_content_path"], "/downloads/Show - 01.mkv");
        // The legacy field name must not re-appear as a shadow alias —
        // there is only one wire key.
        assert!(v.get("qbit_content_path").is_none());
    }
}

#[cfg(test)]
mod grab_history_state_tests {
    use super::*;
    use crate::services::source;
    use crate::test_support::{in_memory_pool, seed_series};

    async fn states(db: &SqlitePool, series_id: i64) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT state FROM episode_grab_history WHERE series_id = ? AND episode_number = 1 ORDER BY id",
        )
        .bind(series_id)
        .fetch_all(db)
        .await
        .unwrap()
    }

    /// Regression: `episode_grab_history` has no `updated_at` column,
    /// and the UPDATE used to set it, so the flip failed with "no such
    /// column" and every caller's `let _ =` swallowed it. The upgrade
    /// path's "replaced" chain never showed in the episode modal.
    #[tokio::test]
    async fn mark_grab_history_replaced_flips_the_completed_row() {
        let db = in_memory_pool().await;
        let sid = seed_series(&db, 1, "Show").await;
        let c = source::classify_release_sync("Show - 01 [WEB 720p].mkv", None);
        record_grab(&db, sid, 1, &c, "Show - 01 [WEB 720p].mkv", "", 1, false)
            .await
            .unwrap();
        mark_grab_history_completed(&db, sid, 1, "Show - 01 [WEB 720p].mkv", 1)
            .await
            .unwrap();
        assert_eq!(states(&db, sid).await, vec!["completed"]);

        mark_grab_history_replaced(&db, sid, 1)
            .await
            .expect("the UPDATE must not error");
        assert_eq!(states(&db, sid).await, vec!["replaced"]);

        // A second grab lands, completes, and leaves the old row alone.
        record_grab(&db, sid, 1, &c, "Show - 01 [BD 1080p].mkv", "", 1, false)
            .await
            .unwrap();
        mark_grab_history_completed(&db, sid, 1, "Show - 01 [BD 1080p].mkv", 1)
            .await
            .unwrap();
        assert_eq!(states(&db, sid).await, vec!["replaced", "completed"]);
    }

    #[tokio::test]
    async fn record_grab_with_match_round_trips_provenance_into_history() {
        use crate::services::auto_search::{MatchKind, MatchPhase};
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let sid: i64 = sqlx::query_scalar(
            "INSERT INTO series (anilist_id, title) VALUES (21521, 'Kowaremono') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let provenance = MatchProvenance {
            phase: MatchPhase::Extended,
            kind: MatchKind::Fuzzy,
            alias: "Risa THE ANIMATION".to_string(),
            ratio: 0.67,
        };
        record_grab_with_match(
            &db,
            sid,
            1,
            &ClassificationResult::unknown(),
            "[Xonline] Grisaia",
            "Xonline",
            0,
            false,
            Some(&provenance),
        )
        .await
        .unwrap();
        let rows = get_grab_history(&db, sid, 1).await.unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.match_kind, "fuzzy");
        assert_eq!(row.match_phase, "extended");
        assert_eq!(row.matched_alias, "Risa THE ANIMATION");
        assert!((row.match_ratio - 0.67).abs() < 1e-6, "{}", row.match_ratio);
        assert_eq!(
            row.grab_match_summary,
            "Fuzzy alias match: \"Risa THE ANIMATION\" at 67% (extended alias pass)"
        );
    }

    #[tokio::test]
    async fn record_grab_without_match_leaves_provenance_columns_empty() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let sid: i64 = sqlx::query_scalar(
            "INSERT INTO series (anilist_id, title) VALUES (1, 'Bebop') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        record_grab(
            &db,
            sid,
            3,
            &ClassificationResult::unknown(),
            "[G] Bebop - 03",
            "G",
            0,
            false,
        )
        .await
        .unwrap();
        let rows = get_grab_history(&db, sid, 3).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].match_kind.is_empty());
        assert!(rows[0].grab_match_summary.is_empty());
        assert_eq!(rows[0].match_ratio, 0.0);
    }

    #[tokio::test]
    async fn mark_grab_failed_for_release_fails_history_and_tags_for_the_release() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::models::migrate(&db).await.unwrap();
        let sid: i64 = sqlx::query_scalar(
            "INSERT INTO series (anilist_id, title) VALUES (5, 'Show') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let cls = ClassificationResult::unknown();
        record_grab(&db, sid, 1, &cls, "[G] Show 01-02", "G", 0, true)
            .await
            .unwrap();
        record_grab(&db, sid, 2, &cls, "[G] Show 01-02", "G", 0, true)
            .await
            .unwrap();
        record_grab(&db, sid, 3, &cls, "[G] Show - 03", "G", 0, false)
            .await
            .unwrap();
        let n = mark_grab_failed_for_release(&db, sid, "[G] Show 01-02")
            .await
            .unwrap();
        assert_eq!(n, 2);
        let states: Vec<(i32, String)> = sqlx::query_as(
            "SELECT episode_number, state FROM episode_grab_history WHERE series_id = ? ORDER BY episode_number",
        )
        .bind(sid)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            states,
            vec![
                (1, "failed".into()),
                (2, "failed".into()),
                (3, "grabbed".into())
            ]
        );
        let tag_states: Vec<(i32, String)> = sqlx::query_as(
            "SELECT episode_number, state FROM episode_quality_tags WHERE series_id = ? ORDER BY episode_number",
        )
        .bind(sid)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            tag_states,
            vec![
                (1, "failed".into()),
                (2, "failed".into()),
                (3, "grabbed".into())
            ]
        );
    }
}
