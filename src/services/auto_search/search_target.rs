//! `SearchTarget` enum + target-set builders.
//!
//! A `SearchTarget` is the per-query granularity the search pipeline
//! works at: either "one entry" (movies, single-episode OVAs, anything
//! AniList marks as one-of) or "episode N" (regular TV). The builders
//! turn a series's on-disk state + monitoring config into the list of
//! targets to search for.

use std::collections::{HashMap, HashSet};

use crate::models::episode_tags::EpisodeQualityTag;
use crate::services::anilist::AnimeDetail;
use crate::services::media;
use crate::services::source::{self, ClassificationResult, Resolution, Source};

use super::resolve_existing_classification;

#[derive(Debug, Clone)]
pub enum SearchTarget {
    Single,
    Episode(i32),
}

impl SearchTarget {
    /// Build a search target for a user-initiated "search this episode"
    /// action. Collapses to `Single` only when the media is genuinely
    /// single-entry; otherwise stays as `Episode(n)`.
    ///
    /// This exists because the per-episode handlers used to pass
    /// `Episode(n)` unconditionally — and for movies, `matches_target`
    /// then rejected every real release on Nyaa (movie filenames don't
    /// carry episode numbers), leaving Phase 1 empty and triggering the
    /// extended-alias fallback with its looser matching. Collapsing to
    /// `Single` for single-entry media keeps the search on the correct
    /// code path and prevents the fallback from firing in the first
    /// place.
    ///
    /// Rules:
    /// - `MOVIE` → always `Single`. Movies are always single-entry; if
    ///   AniList reports something weird like `episodes: None` or
    ///   `Some(2)`, we still trust the format.
    /// - `SPECIAL` / `OVA` / `ONA` with `episodes == Some(1)` → `Single`.
    ///   These formats are single-entry *in the common case*, but
    ///   multi-episode OVAs (Hellsing Ultimate, LOGH) and multi-episode
    ///   ONAs absolutely exist and their releases DO carry episode
    ///   numbers, so only collapse when AniList explicitly confirms a
    ///   single episode.
    /// - Everything else (TV, TV_SHORT, multi-episode OVA/ONA/SPECIAL,
    ///   or unknown episode count) → `Episode(n)`. TV releases carry
    ///   episode numbers, and for ambiguous formats with unknown episode
    ///   count the safe default is to keep `Episode(n)` — the failure
    ///   mode there is "no results" rather than "wrong series grabbed".
    pub fn for_episode(detail: &AnimeDetail, episode_number: i32) -> Self {
        match detail.format.as_str() {
            "MOVIE" => SearchTarget::Single,
            "SPECIAL" | "OVA" | "ONA" if detail.episodes == Some(1) => SearchTarget::Single,
            _ => SearchTarget::Episode(episode_number),
        }
    }
}

pub fn build_missing_targets(detail: &AnimeDetail, existing_episodes: &[i32]) -> Vec<SearchTarget> {
    let total_eps = detail.episodes.unwrap_or(0);

    if total_eps <= 1 || matches!(detail.format.as_str(), "MOVIE" | "SPECIAL" | "OVA" | "ONA") {
        return vec![SearchTarget::Single];
    }

    let existing: HashSet<i32> = existing_episodes.iter().copied().collect();
    let mut targets = Vec::new();
    for ep in 1..=total_eps.max(0) {
        if !existing.contains(&ep) {
            targets.push(SearchTarget::Episode(ep));
        }
    }
    targets
}

pub fn build_monitored_targets(
    detail: &AnimeDetail,
    existing_episodes: &[i32],
    monitored_episodes: &[i32],
) -> Vec<SearchTarget> {
    if detail.episodes.unwrap_or(0) <= 1
        || matches!(detail.format.as_str(), "MOVIE" | "SPECIAL" | "OVA" | "ONA")
    {
        if monitored_episodes.is_empty() || monitored_episodes.contains(&1) {
            return vec![SearchTarget::Single];
        }
        return Vec::new();
    }

    let existing: HashSet<i32> = existing_episodes.iter().copied().collect();
    let mut monitored: Vec<i32> = monitored_episodes.to_vec();
    monitored.sort_unstable();
    monitored.dedup();

    monitored
        .into_iter()
        .filter(|ep| !existing.contains(ep))
        .map(SearchTarget::Episode)
        .collect()
}

/// Build upgrade targets: candidate episodes that exist on disk but are below
/// the quality cutoff. These are candidates for automatic quality upgrades.
///
/// Hydration order for each on-disk episode:
/// 1. Structured classification columns on `episode_quality_tags` (written
///    since Phase 1b).
/// 2. Legacy `release_title` column parsed via filename-only classification
///    (for rows grabbed before Phase 1b landed, where the structured cols
///    are empty).
/// 3. On-disk filename + `quality` string, also via filename-only
///    classification (for episodes that have no grab record at all — e.g.
///    pre-existing library files that Ryokan didn't grab itself).
pub fn build_upgrade_targets(
    disk_files: &[media::EpisodeFile],
    candidate_episodes: &[i32],
    cutoff_source: Source,
    cutoff_resolution: Resolution,
    cutoff_is_remux: bool,
    cutoff_is_bdmv: bool,
    quality_tags: &HashMap<i32, EpisodeQualityTag>,
) -> Vec<(SearchTarget, ClassificationResult)> {
    let candidates: HashSet<i32> = candidate_episodes.iter().copied().collect();
    let cutoff = source::cutoff_classification(
        cutoff_source,
        cutoff_resolution,
        cutoff_is_remux,
        cutoff_is_bdmv,
    );
    let cutoff_rank = cutoff.rank();

    let mut targets = Vec::new();
    for file in disk_files {
        if !candidates.contains(&file.episode_number) {
            continue;
        }
        // manual_override pins must short-circuit before find_best_for_target
        // runs. The downstream SQL guards on record_grab /
        // update_classification drop the tag write, but post-processing has
        // already replaced the on-disk file by the time those guards fire.
        if quality_tags
            .get(&file.episode_number)
            .is_some_and(|t| t.manual_override)
        {
            continue;
        }
        let existing =
            resolve_existing_classification(file, quality_tags.get(&file.episode_number));
        // Skip completely unclassified episodes — we have no way to know
        // whether an incoming release would actually be an upgrade.
        if existing.source == Source::Unknown && existing.resolution == Resolution::Unknown {
            continue;
        }
        if existing.rank() < cutoff_rank {
            targets.push((SearchTarget::Episode(file.episode_number), existing));
        }
    }
    targets.sort_by_key(|(t, _)| match t {
        SearchTarget::Episode(n) => *n,
        SearchTarget::Single => 0,
    });
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail_with(format: &str, episodes: Option<i32>) -> AnimeDetail {
        AnimeDetail {
            is_adult: false,
            id: 1,
            id_mal: None,
            title_romaji: String::new(),
            title_english: String::new(),
            title_native: String::new(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: format.to_string(),
            status: String::new(),
            status_display: String::new(),
            episodes,
            duration: None,
            season: String::new(),
            season_year: None,
            end_year: None,
            description: String::new(),
            genres: Vec::new(),
            average_score: None,
            average_score_display: None,
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: Vec::new(),
            streaming_episodes: Vec::new(),
            relations: Vec::new(),
        }
    }

    // SearchTarget::for_episode collapses to Single for single-entry media
    // so per-episode handlers don't pass Episode(n) for shows that don't
    // have episode numbers in their release filenames.

    #[test]
    fn for_episode_collapses_movie_to_single() {
        let d = detail_with("MOVIE", Some(1));
        assert!(matches!(
            SearchTarget::for_episode(&d, 1),
            SearchTarget::Single
        ));
    }

    #[test]
    fn for_episode_collapses_special_to_single() {
        let d = detail_with("SPECIAL", Some(1));
        assert!(matches!(
            SearchTarget::for_episode(&d, 1),
            SearchTarget::Single
        ));
    }

    #[test]
    fn for_episode_collapses_ova_to_single() {
        let d = detail_with("OVA", Some(1));
        assert!(matches!(
            SearchTarget::for_episode(&d, 1),
            SearchTarget::Single
        ));
    }

    #[test]
    fn for_episode_keeps_episode_for_single_episode_tv() {
        // TV format stays as Episode(n) regardless of episode count — the
        // collapse rule is format-only. A TV release titled "Show - 01" still
        // carries an episode number that Episode(1) can match against.
        let d = detail_with("TV", Some(1));
        assert!(matches!(
            SearchTarget::for_episode(&d, 1),
            SearchTarget::Episode(1)
        ));
    }

    #[test]
    fn for_episode_keeps_episode_for_multi_episode_tv() {
        let d = detail_with("TV", Some(12));
        assert!(matches!(
            SearchTarget::for_episode(&d, 7),
            SearchTarget::Episode(7)
        ));
    }

    #[test]
    fn for_episode_keeps_episode_when_episode_count_unknown() {
        // AniList reports episodes=None for currently-airing shows — the
        // fallback should still be Episode(n) because that's the correct
        // target for an airing weekly release.
        let d = detail_with("TV", None);
        assert!(matches!(
            SearchTarget::for_episode(&d, 3),
            SearchTarget::Episode(3)
        ));
    }

    #[test]
    fn for_episode_collapses_movie_even_when_episode_count_is_none() {
        // MOVIE always collapses regardless of AniList's episode count — a
        // film is single-entry even if AniList has weird/missing data.
        let d = detail_with("MOVIE", None);
        assert!(matches!(
            SearchTarget::for_episode(&d, 1),
            SearchTarget::Single
        ));
    }

    #[test]
    fn for_episode_keeps_episode_for_multi_episode_ova() {
        // Multi-episode OVA series (e.g., long-running OVA franchises with
        // 10+ entries) carry episode numbers in their release filenames,
        // so per-episode search must NOT collapse them to Single — that
        // would return a release for any episode or a full batch when the
        // user specifically asked for episode N.
        let d = detail_with("OVA", Some(10));
        assert!(matches!(
            SearchTarget::for_episode(&d, 5),
            SearchTarget::Episode(5)
        ));
    }

    #[test]
    fn for_episode_keeps_episode_for_multi_episode_ona() {
        let d = detail_with("ONA", Some(24));
        assert!(matches!(
            SearchTarget::for_episode(&d, 12),
            SearchTarget::Episode(12)
        ));
    }

    #[test]
    fn for_episode_keeps_episode_for_multi_episode_special() {
        let d = detail_with("SPECIAL", Some(4));
        assert!(matches!(
            SearchTarget::for_episode(&d, 2),
            SearchTarget::Episode(2)
        ));
    }

    #[test]
    fn for_episode_keeps_episode_for_ova_with_unknown_count() {
        // Ambiguous: we don't know whether it's a 1-episode OVA or a
        // 12-episode OVA. Safe default is Episode(n) — the failure mode
        // is "no results" rather than "grabbed the wrong release".
        let d = detail_with("OVA", None);
        assert!(matches!(
            SearchTarget::for_episode(&d, 1),
            SearchTarget::Episode(1)
        ));
    }
}
