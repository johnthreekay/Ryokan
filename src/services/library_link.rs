//! Manual-search → grab library-linkage resolver.
//!
//! When the user clicks "Grab" on the manual search page, the handler
//! sends the URL to the download client. This module decides whether
//! the grabbed release should also be linked to a series in the
//! library — and if no library row exists yet, optionally auto-adds
//! it via AniList.
//!
//! ## Resolution chain
//!
//! 1. **Cheap fuzzy match** — the existing RSS-style title matcher
//!    (`services::rss::match_library_title`). Catches the case where
//!    the user already has the series and the release title parses
//!    cleanly. No API calls.
//!
//! 2. **anitomy + AL search** — the fallback when (1) misses. anitomy
//!    parses the release title into a structured `anime_title`, then
//!    `services::anilist::search_anime` looks it up. AL search has
//!    its own AL→Jikan fallback + caching, so we don't need to handle
//!    rate-limit / outage logic here.
//!
//! 3. **AL-ID lookup** — query `series::get_by_anilist_id` against
//!    the top AL hit's id. If a row exists, link the grab to it. This
//!    is the case-(b) reliability fix from the user's report:
//!    title-fuzzy matching is too brittle for "user has the series
//!    but the release title's wording differs from the canonical
//!    title slot."
//!
//! 4. **Auto-add** — the case-(a) feature: when no library row
//!    matches, fetch the full AL detail and `series::upsert` it.
//!    Gated by `config.manual_search_auto_add` (default ON) and a
//!    safety check that the parsed title shares a substantive token
//!    with the AL hit's title (catches AL search returning a totally
//!    unrelated #1 result for very short or anitomy-mangled queries).
//!
//! ## Episode-number resolution
//!
//! Each branch of [`LibraryLinkOutcome`] also returns the episode
//! numbers the grab covers, derived via
//! [`auto_search::parse_release_numbers`] with a batch fallback to
//! the series's known episode count (matching
//! `interactive::search_batch_releases::batch_episode_numbers`).
//! Callers use these to populate `episode_quality_tags` /
//! `grabbed_torrents.episode_numbers` immediately so the series page
//! shows progress before post-processing finishes.

use std::collections::HashSet;
use std::sync::LazyLock;

use anitomy::ElementCategory;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, series};
use crate::services::{anilist, auto_search, logger, metadata_sync, rss};

/// Result of [`resolve_or_add_series_for_grab`]. Each variant
/// carries enough context for the grab handler to (a) write the
/// linkage rows and (b) render a meaningful toast on the search
/// page.
#[derive(Debug, Clone)]
pub enum LibraryLinkOutcome {
    /// Existing series matched via the RSS-style fuzzy title matcher
    /// (no API call). The cheapest path — `series` was found by
    /// alias / family-key matching against the release title.
    LinkedExisting {
        series: series::Series,
        episode_numbers: Vec<i32>,
    },
    /// Existing series matched via anitomy → AL search → AL-ID
    /// lookup. Fixes the case where the release title's wording
    /// doesn't match any of the local series's title slots but the
    /// AL hit pins it down via id. Distinct from `LinkedExisting`
    /// for telemetry and for the "the matcher found it" toast.
    LinkedByAnilist {
        series: series::Series,
        episode_numbers: Vec<i32>,
    },
    /// No library row matched, AL search returned a confident hit,
    /// and `manual_search_auto_add` was ON, so the series was added
    /// to the library on the fly. The handler should then fire the
    /// metadata-sync hydration sidecar (same shape as the regular
    /// `add_series` add path).
    AutoAdded {
        series: series::Series,
        episode_numbers: Vec<i32>,
    },
    /// AL search returned a hit but the parsed title and the AL hit
    /// don't share a substantive (≥3 char) alphanumeric token.
    /// Refused to auto-add to avoid polluting the library with a
    /// wrong match. Carries the rejected pair for logging / UI.
    AmbiguousMatch {
        parsed_title: String,
        al_title: String,
    },
    /// AL search found a viable candidate but the user has
    /// `manual_search_auto_add` disabled. Carries the candidate so
    /// the toast can suggest "found <Title> on AL — flip the toggle
    /// to add automatically."
    AutoAddDisabled { al_id: i64, al_title: String },
    /// AL search returned a viable candidate. Both the safety check
    /// and the auto-add toggle passed, but the second-stage
    /// `get_anime_detail` fetch failed (transient AL outage between
    /// the two requests). The grab succeeded in the download
    /// client; a follow-up sync pass will retry the link when AL
    /// is reachable. Distinct from `NoMatch` because AL *did*
    /// match: confusing the user into thinking their show isn't on
    /// AL would push them toward "fix the metadata" workflows that
    /// don't apply here.
    DetailFetchFailed { al_id: i64, al_title: String },
    /// No fuzzy match, anitomy gave up or returned a useless title,
    /// and AL search came up empty. The grab succeeds in the
    /// download client but no library bookkeeping happens.
    NoMatch { parsed_title: Option<String> },
}

impl LibraryLinkOutcome {
    /// Stable string tag used as the JSON `link_status` field on the
    /// grab response. The frontend keys toast wording off this; new
    /// variants must add a new tag rather than reusing an existing
    /// one (toast copy in `static/js/search.js` matches by tag).
    pub fn tag(&self) -> &'static str {
        match self {
            Self::LinkedExisting { .. } => "linked",
            Self::LinkedByAnilist { .. } => "linked",
            Self::AutoAdded { .. } => "added",
            Self::AmbiguousMatch { .. } => "ambiguous",
            Self::AutoAddDisabled { .. } => "auto_add_disabled",
            Self::DetailFetchFailed { .. } => "detail_fetch_failed",
            Self::NoMatch { .. } => "no_match",
        }
    }

    // No `series_title()` helper. Callers that need a display title
    // for the toast / log line must derive it from the user's
    // current `config.title_language` via [`pick_title`] over the
    // series's per-language slots (or use the AL-derived `al_title`
    // for the no-link branches, which the resolver already picked
    // with the current pref). The `series.title` column is frozen
    // in whatever language was active at series-add time, so
    // returning it as the canonical display would silently regress
    // the toast for users who later change their title preference.
}

/// anitomy-extract the `AnimeTitle` token from a release title. anitomy
/// never panics on non-NUL input but returns `Err(elements)` when it
/// couldn't confidently identify a title field. Either way, we just
/// peek at `AnimeTitle`. Returns `None` when anitomy didn't emit one
/// at all or the result trimmed to empty.
pub fn extract_anime_title(release_title: &str) -> Option<String> {
    if release_title.trim().is_empty() {
        return None;
    }
    // Defensive NUL strip — anitomy rejects NUL bytes; production
    // titles shouldn't contain them but a corrupted feed byte
    // shouldn't panic the resolver.
    let elements = crate::services::media::anitomy_parse(release_title);
    elements
        .get(ElementCategory::AnimeTitle)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 3-char tokens that are never show identity — release-form markers,
/// codecs/containers, source labels, common English fillers, quality
/// fields. Without this set, `share_substantive_token` accepted
/// pairings like `[Group] Show OVA - 01` ↔ `OVA Anthology` because
/// `ova` (4 chars) cleared the length filter and was the only shared
/// token. Same shape for `amv`/`raw`/`bdmv`/`web`/`1080p` and the
/// English fillers below.
static SUBSTANTIVE_STOPWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        // Anime-form markers.
        "ova", "ona", "oad", "oav", "amv", "ncop", "nced", "raw", "dub", "sub", "subs",
        // Source / encode / container.
        "web", "bdmv", "bluray", "dvdrip", "hdtv", "webrip", "bdrip", "hdrip", "mkv", "mp4", "m4v",
        "avc", "hevc", "h264", "h265", "x264", "x265", "aac", "flac", "opus", "ass", "srt",
        // Resolution / quality fields.
        "1080", "1080p", "2160", "2160p", "720p", "480p",
        // English fillers that survive the 3-char filter.
        "the", "and", "for", "with", "from", "but", "you", "all", "any", "are", "was", "has",
        "have", "had", "her", "his", "him", "she", "they", "them", "our", "out", "who", "not",
        "its",
    ])
});

/// True when `parsed` and `candidate` share at least one substantive
/// alphanumeric token (≥3 chars, lowercase, not in the stop-word set).
/// Catches AL search returning a totally unrelated #1 result for very
/// short or anitomy-mangled parsed titles. The 3-char threshold drops
/// articles ("the", "of", "a"); the stop-word set drops 3+-char tokens
/// that would produce spurious matches between unrelated shows
/// ("ova", "amv", "raw", "1080p", and common English fillers). Numeric
/// tokens like "100" or "2nd" still count — they're show-identity
/// signals when 3+ chars.
pub fn share_substantive_token(parsed: &str, candidate: &str) -> bool {
    fn tokens(s: &str) -> Vec<String> {
        s.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.chars().count() >= 3 && !SUBSTANTIVE_STOPWORDS.contains(t))
            .map(|t| t.to_string())
            .collect()
    }
    let parsed_tokens = tokens(parsed);
    if parsed_tokens.is_empty() {
        return false;
    }
    let candidate_tokens = tokens(candidate);
    parsed_tokens.iter().any(|t| candidate_tokens.contains(t))
}

/// True when `parsed` shares a substantive token with any of the AL
/// entry's three title slots. Necessary because release groups vary
/// in which language they prefix the title with — matching only
/// against `title_romaji` would reject English-prefixed releases of
/// shows that AL has under romaji canonical names.
fn al_entry_shares_token(parsed: &str, entry: &anilist::AnimeEntry) -> bool {
    [
        &entry.title_romaji,
        &entry.title_english,
        &entry.title_native,
    ]
    .iter()
    .any(|slot| !slot.is_empty() && share_substantive_token(parsed, slot))
}

/// Pick the user-preferred title slot from a candidate's three
/// title strings, falling back through the chain when the requested
/// slot is empty (AL entries can omit any slot — most commonly
/// `title_english` for unlicensed series). The fallback chain
/// preserves user intent: a user who picked "english" but grabs an
/// untranslated doujin still gets a usable display title (romaji →
/// native), not an empty one.
///
/// `pref` matches `config.title_language` ("english" / "romaji" /
/// "native"); any other value is treated as "english" — same
/// coercion `handlers/settings/mod.rs::settings_submit` does on save.
///
/// `pub` so the grab handler can derive a fresh display title for
/// the toast: `series.title` is the persisted column captured at
/// series-add time and doesn't update when the user later changes
/// `config.title_language`, so picking from the per-language slots
/// at render time is the only way to keep the toast in sync with
/// the current preference.
pub fn pick_title<'a>(
    pref: &str,
    title_english: &'a str,
    title_romaji: &'a str,
    title_native: &'a str,
) -> &'a str {
    let chain: [&str; 3] = match pref {
        "romaji" => [title_romaji, title_english, title_native],
        "native" => [title_native, title_romaji, title_english],
        _ => [title_english, title_romaji, title_native],
    };
    chain.into_iter().find(|s| !s.is_empty()).unwrap_or("")
}

/// Pick the user-preferred display title from an `AnimeEntry`. Used
/// for toast / log rendering on the no-link branches of the resolver
/// (Ambiguous / AutoAddDisabled) so the user sees the AL hit's title
/// in their preferred language, not a hardcoded English fallback.
fn entry_display_title<'a>(pref: &str, entry: &'a anilist::AnimeEntry) -> &'a str {
    pick_title(
        pref,
        &entry.title_english,
        &entry.title_romaji,
        &entry.title_native,
    )
}

/// Pick the user-preferred display title from an `AnimeDetail`. Used
/// for the auto-add path's `series.title` (persisted column) so the
/// library card / detail page show the user-preferred title rather
/// than always English.
fn detail_display_title<'a>(pref: &str, detail: &'a anilist::AnimeDetail) -> &'a str {
    pick_title(
        pref,
        &detail.title_english,
        &detail.title_romaji,
        &detail.title_native,
    )
}

/// Read `config.title_language` once. Used by the resolver to pick
/// the right slot from AL responses for both the persisted
/// `series.title` (auto-add path) and the toast text (all paths).
/// `pub` so the grab handler can read the same setting before
/// rendering toast copy.
pub async fn title_language(db: &sqlx::SqlitePool) -> String {
    config::get_config(db)
        .await
        .ok()
        .flatten()
        .map(|c| c.title_language)
        .unwrap_or_else(|| "english".to_string())
}

/// Derive episode numbers for a grab the same way
/// `interactive::batch_episode_numbers` does: parse explicit numbers
/// out of the title; when batch and parse came up empty, fall back
/// to `1..=episodes` so a no-suffix complete-series release gets
/// every episode tagged. Capped at 1000 — a garbage AniList record
/// reporting tens of thousands of episodes shouldn't spawn millions
/// of `episode_quality_tags` rows. Pre-fix the cap zeroed the result
/// (silently dropped the fallback), which left grabs with no
/// per-episode tag rows and no breadcrumb pointing at why; truncate
/// to the cap instead so the protective intent fires without losing
/// the fallback's purpose entirely.
fn parse_grab_episodes(
    title: &str,
    is_batch: bool,
    fallback_episode_count: Option<i32>,
) -> Vec<i32> {
    const FALLBACK_CAP: i32 = 1000;
    let mut ep_nums: Vec<i32> = auto_search::parse_release_numbers(title)
        .into_iter()
        .collect();
    if is_batch
        && ep_nums.is_empty()
        && let Some(total) = fallback_episode_count
        && total > 0
    {
        let bounded = total.min(FALLBACK_CAP);
        ep_nums = (1..=bounded).collect();
    }
    ep_nums.sort_unstable();
    ep_nums
}

/// Orchestrator. See module docs for the resolution chain.
pub async fn resolve_or_add_series_for_grab(
    state: &AppState,
    title: &str,
    is_batch: bool,
) -> LibraryLinkOutcome {
    // 1. Cheap fuzzy match (no API).
    if let Some((s, eps)) = rss::match_library_title(&state.db, title, is_batch).await {
        return LibraryLinkOutcome::LinkedExisting {
            series: s,
            episode_numbers: eps,
        };
    }

    // 2. anitomy parse.
    let Some(parsed_title) = extract_anime_title(title) else {
        return LibraryLinkOutcome::NoMatch { parsed_title: None };
    };

    // Read the user's preferred title language once — used both for
    // the persisted `series.title` on the auto-add branch and for
    // the surfaced AL-title text on the no-link branches (Ambiguous
    // / AutoAddDisabled). Keeping a single read at the top so the
    // toast and DB row always agree on which slot to render.
    let title_pref = title_language(&state.db).await;

    // 3. AL search. `search_anime` already covers AL→Jikan fallback +
    //    caching internally, so a transient AL outage doesn't kill
    //    this path.
    let hits = anilist::search_anime(&parsed_title)
        .await
        .unwrap_or_default();
    let Some(top) = hits.into_iter().next() else {
        return LibraryLinkOutcome::NoMatch {
            parsed_title: Some(parsed_title),
        };
    };

    // 4. AL-ID lookup against existing library — the case-(b) fix.
    if let Ok(Some(existing)) = series::get_by_anilist_id(&state.db, top.id).await {
        let eps = parse_grab_episodes(title, is_batch, existing.episodes);
        return LibraryLinkOutcome::LinkedByAnilist {
            series: existing,
            episode_numbers: eps,
        };
    }

    // 5. Auto-add safety check — refuse if parsed and AL hit don't
    //    share a substantive token. Skips false-positive auto-adds
    //    when AL search returns a spurious top result.
    if !al_entry_shares_token(&parsed_title, &top) {
        return LibraryLinkOutcome::AmbiguousMatch {
            parsed_title,
            al_title: entry_display_title(&title_pref, &top).to_string(),
        };
    }

    // 6. Auto-add toggle. Read fresh; we don't pass a Config in
    //    because the toggle is rare-write/rare-read and reading on
    //    each grab is cheap (single config row, cached by SQLite).
    let auto_add = config::get_config(&state.db)
        .await
        .ok()
        .flatten()
        .map(|c| c.manual_search_auto_add)
        .unwrap_or(true);
    if !auto_add {
        return LibraryLinkOutcome::AutoAddDisabled {
            al_id: top.id,
            al_title: entry_display_title(&title_pref, &top).to_string(),
        };
    }

    // 7. Fetch full detail + upsert. Detail-fetch failure here is
    //    distinct from "no AL match" — search did match, the second
    //    stage just couldn't reach AL. Surfacing as
    //    `DetailFetchFailed` (rather than `NoMatch`) keeps the toast
    //    honest and avoids pushing users toward
    //    "fix-the-metadata" workflows that don't apply when the
    //    show *is* on AL but the resolver hit a transient outage.
    let detail = match anilist::get_anime_detail(top.id).await {
        Ok(d) => d,
        Err(e) => {
            logger::warn(
                &state.db,
                LogCategory::Library,
                &format!(
                    "Manual grab auto-add: AL detail fetch failed for id={}",
                    top.id
                ),
                &e,
            )
            .await;
            return LibraryLinkOutcome::DetailFetchFailed {
                al_id: top.id,
                al_title: entry_display_title(&title_pref, &top).to_string(),
            };
        }
    };

    let display_title = detail_display_title(&title_pref, &detail).to_string();
    let core = series::SeriesCore {
        anilist_id: detail.id,
        mal_id: detail.id_mal,
        title: &display_title,
        title_romaji: &detail.title_romaji,
        title_english: &detail.title_english,
        title_native: &detail.title_native,
        cover_url: &detail.cover_url,
        format: &detail.format,
        status: &detail.status,
        episodes: detail.episodes,
        season_year: detail.season_year,
        end_year: detail.end_year,
    };
    let id = match series::upsert(&state.db, core).await {
        Ok((id, _created)) => id,
        Err(e) => {
            logger::warn(
                &state.db,
                LogCategory::Library,
                "Manual grab auto-add: series upsert failed",
                &e.to_string(),
            )
            .await;
            // Same reasoning as the detail-fetch branch above —
            // the AL match was correct; only persistence failed,
            // so the toast should point at the resolver problem
            // rather than imply AL didn't match.
            return LibraryLinkOutcome::DetailFetchFailed {
                al_id: top.id,
                al_title: entry_display_title(&title_pref, &top).to_string(),
            };
        }
    };
    let series_row = match series::get_by_id(&state.db, id).await {
        Ok(Some(s)) => s,
        _ => {
            return LibraryLinkOutcome::DetailFetchFailed {
                al_id: top.id,
                al_title: entry_display_title(&title_pref, &top).to_string(),
            };
        }
    };

    // Same metadata-hydration sidecar `add_series` fires post-upsert,
    // so banner/description/relations/episode-cache fill in without
    // the user needing to wait. Detached spawn — by the time the
    // background task runs, the grab response has already been
    // returned to the frontend.
    let db_clone = state.db.clone();
    let series_clone = series_row.clone();
    tokio::spawn(async move {
        if let Err(e) =
            metadata_sync::refresh_series_metadata(&db_clone, &series_clone, false).await
        {
            logger::warn(
                &db_clone,
                LogCategory::AniList,
                &format!(
                    "Failed to hydrate metadata for auto-added series {}",
                    series_clone.title
                ),
                &e,
            )
            .await;
        }
    });

    let eps = parse_grab_episodes(title, is_batch, detail.episodes);
    LibraryLinkOutcome::AutoAdded {
        series: series_row,
        episode_numbers: eps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_anime_title_handles_grouped_release() {
        // Standard `[Group] Title - 01 [1080p].mkv` shape.
        let parsed = extract_anime_title("[SubsPlease] Mob Psycho 100 - 12 [1080p].mkv");
        assert_eq!(parsed.as_deref(), Some("Mob Psycho 100"));
    }

    #[test]
    fn extract_anime_title_handles_batch_with_range() {
        let parsed =
            extract_anime_title("[Vodes] Ousama Ranking S01 (BDRip 1080p HEVC FLAC) [Dual-Audio]");
        // anitomy may capture the whole "Ousama Ranking" or include
        // the season — either is fine, test just confirms a usable
        // title comes out.
        let parsed = parsed.expect("anitomy should pull a title from a clean BD batch");
        assert!(
            parsed.to_lowercase().contains("ousama") || parsed.to_lowercase().contains("ranking"),
            "expected the parsed title to contain a substantive token from the release title; got {:?}",
            parsed
        );
    }

    #[test]
    fn extract_anime_title_returns_none_for_empty() {
        assert_eq!(extract_anime_title(""), None);
        assert_eq!(extract_anime_title("    "), None);
    }

    #[test]
    fn extract_anime_title_strips_nul_bytes_without_panic() {
        let parsed = extract_anime_title("[Group] Tit\0le - 01");
        assert!(parsed.is_some(), "NUL strip should leave a parsable title");
    }

    #[test]
    fn share_substantive_token_matches_overlapping_words() {
        assert!(share_substantive_token(
            "Mob Psycho 100",
            "Mob Psycho 100 III"
        ));
        assert!(share_substantive_token(
            "Ousama Ranking",
            "Ranking of Kings"
        ));
    }

    #[test]
    fn share_substantive_token_rejects_unrelated_pairs() {
        assert!(!share_substantive_token("Mob Psycho 100", "Spy x Family"));
        // No shared 3+-char token even though both have "of".
        assert!(!share_substantive_token("Land of the Lustrous", "Of Time"));
    }

    #[test]
    fn share_substantive_token_drops_short_tokens() {
        // "OP" and "ED" are 2 chars — should not produce a spurious
        // match between unrelated openings.
        assert!(!share_substantive_token("OP NCOP", "ED NCED"));
    }

    #[test]
    fn share_substantive_token_rejects_stopword_only_overlap() {
        // The function gets parsed (anitomy-extracted) titles, not raw
        // filenames — so realistic inputs are short title strings, not
        // full release lines.
        //
        // Anime-form-marker overlap. Pre-stopword, "ova" cleared the
        // 3-char length filter and was the only shared token; stop-word
        // filter rejects it.
        assert!(!share_substantive_token("Show OVA", "OVA Anthology"));
        // English-filler overlap. Pre-stopword, "the" cleared the
        // length filter and matched between completely unrelated shows.
        assert!(!share_substantive_token(
            "The Eminence in Shadow",
            "The Quintessential Quintuplets"
        ));
        // Sanity: a real shared identity token still matches even when
        // surrounded by stop-words.
        assert!(share_substantive_token(
            "The Mob Psycho 100 OVA",
            "Mob Psycho 100 III"
        ));
    }

    #[test]
    fn pick_title_returns_user_preferred_slot_when_filled() {
        assert_eq!(
            pick_title("english", "Eng", "Rom", "Nat"),
            "Eng",
            "english pref → english slot"
        );
        assert_eq!(
            pick_title("romaji", "Eng", "Rom", "Nat"),
            "Rom",
            "romaji pref → romaji slot"
        );
        assert_eq!(
            pick_title("native", "Eng", "Rom", "Nat"),
            "Nat",
            "native pref → native slot"
        );
    }

    #[test]
    fn pick_title_falls_back_through_chain_when_preferred_slot_empty() {
        // English-pref user grabbing an unlicensed series with no
        // English title falls back to romaji, not an empty string.
        assert_eq!(
            pick_title("english", "", "Romaji Title", "ロマジ"),
            "Romaji Title"
        );
        // Romaji-pref user with a series AL only carries native for.
        // Romaji empty → falls back through english (also empty in
        // this test case) → native.
        assert_eq!(pick_title("romaji", "", "", "ネイティブ"), "ネイティブ");
        // Unknown pref string coerces to english (settings handler
        // does the same coercion on save).
        assert_eq!(pick_title("klingon", "Eng", "Rom", "Nat"), "Eng");
    }

    #[test]
    fn pick_title_returns_empty_when_all_slots_empty() {
        // No usable title at all — caller is responsible for
        // checking `.is_empty()` before persisting / rendering.
        assert_eq!(pick_title("english", "", "", ""), "");
    }

    #[test]
    fn parse_grab_episodes_uses_fallback_for_batch_with_no_range_in_title() {
        // No range in title, batch, fallback=12 → 1..=12.
        let eps = parse_grab_episodes("[Vodes] Some Series (BDRip)", true, Some(12));
        assert_eq!(eps, (1..=12).collect::<Vec<i32>>());
    }

    #[test]
    fn parse_grab_episodes_truncates_oversized_fallback_to_cap() {
        // A garbage AniList record reporting 100k episodes shouldn't
        // spawn a million tag rows — truncate to the 1000-row cap
        // rather than zero out the fallback. Pre-fix this returned
        // empty, leaving the grab with no per-episode tags and no
        // breadcrumb pointing at why.
        let eps = parse_grab_episodes("[Vodes] Some Series (BDRip)", true, Some(100_000));
        assert_eq!(
            eps,
            (1..=1000).collect::<Vec<i32>>(),
            "fallback >1000 should truncate to the 1000-cap, not zero out"
        );
    }

    #[test]
    fn parse_grab_episodes_ignores_fallback_when_not_batch() {
        // Non-batch with no parseable episode numbers and a fallback
        // count should still return empty — fallback is batch-only.
        let eps = parse_grab_episodes("[Vodes] Some Movie (BDRip)", false, Some(12));
        assert!(eps.is_empty());
    }
}
