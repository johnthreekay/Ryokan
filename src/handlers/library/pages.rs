//! HTML page handlers for the library section.
//!
//! Split out of `handlers::library::mod`: `index` (home library list),
//! `needs_review_page` (cross-library needs-review feed), and
//! `series_detail` (the per-series page) plus their shared builders.
//! `build_episodes` is in here because it's called by both `series_detail`
//! and the `series_episodes_json` polling endpoint in the `episodes`
//! submodule.

use std::collections::HashMap;

use askama::Template;
use axum::{
    extract::{Path, State},
    response::Html,
};
use sqlx::SqlitePool;

use crate::AppState;
use crate::models::log::LogCategory;
use crate::models::{config, episode_tags, local_metadata, monitoring, series};
use crate::services::{
    anilist, artwork, jikan, kitsu, logger, media, monitoring as monitoring_service,
};

use super::reconcile::{
    force_kitsu_fallback_enabled, populate_series_cover_urls, resolve_series_context,
};
use super::{
    Episode, ErrorTemplate, IndexTemplate, NeedsReviewTemplate, RelationCard, RelationGroup,
    SeriesTemplate,
};

pub async fn index(State(state): State<AppState>) -> Html<String> {
    // Fetch the library list and config concurrently — they're independent
    // and each was previously serialized on the other. `get_all` is the
    // larger query of the two so this shaves the smaller query's RTT off
    // the critical path.
    let (library_res, cfg_res) =
        tokio::join!(series::get_all(&state.db), config::get_config(&state.db),);
    let mut library = library_res.unwrap_or_default();
    let cfg = cfg_res.ok().flatten();

    populate_series_cover_urls(
        &state.db,
        &mut library,
        |item| item.id,
        |item, url| item.cover_url = url,
    )
    .await;

    let template = IndexTemplate {
        page: "library".to_string(),
        library,
        title_language: cfg
            .map(|c| c.title_language)
            .unwrap_or_else(|| "english".to_string()),
    };
    Html(template.render().unwrap_or_default())
}

/// Phase 4 cross-library "needs review" page. Lists every episode the
/// classifier couldn't land a confident verdict on, with a deep link back
/// to the series detail page so the user can open the override modal.
pub async fn needs_review_page(State(state): State<AppState>) -> Html<String> {
    let mut entries = episode_tags::get_needs_review(&state.db)
        .await
        .unwrap_or_default();

    populate_series_cover_urls(
        &state.db,
        &mut entries,
        |e| e.series_id,
        |entry, url| entry.cover_url = url,
    )
    .await;

    let template = NeedsReviewTemplate {
        page: "library".to_string(),
        entries,
    };
    Html(template.render().unwrap_or_default())
}

pub async fn series_detail(
    State(state): State<AppState>,
    Path(request_id): Path<i64>,
) -> Html<String> {
    let (db_series, provider_id, mut detail) = match resolve_series_context(&state.db, request_id)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            logger::error(
                &state.db,
                LogCategory::AniList,
                &format!("Failed to fetch detail for {}", request_id),
                &e,
            )
            .await;
            let (title, message, tech_detail) = if e.contains("403") {
                (
                    "Metadata Provider Unavailable".to_string(),
                    "The metadata API is temporarily unavailable. This usually resolves itself within a few hours. Try again later.".to_string(),
                    e,
                )
            } else if e.contains("not found") || e.contains("Not Found") {
                (
                    "Series Not Found".to_string(),
                    format!(
                        "Could not find a series with ID {}. It may have been removed from the metadata provider.",
                        request_id
                    ),
                    e,
                )
            } else {
                (
                    "Something Went Wrong".to_string(),
                    "An error occurred while loading this series. Please try again.".to_string(),
                    e,
                )
            };
            let template = ErrorTemplate {
                page: "library".to_string(),
                title,
                message,
                detail: tech_detail,
            };
            return Html(template.render().unwrap_or_default());
        }
    };
    let is_tracked = db_series.is_some();
    let db_id = db_series.as_ref().map(|s| s.id);
    let folder_name = db_series
        .as_ref()
        .map(|s| s.folder_name.clone())
        .unwrap_or_default();

    // Ensure monitoring rows first — this writes to DB, and `build_episodes`
    // below reads the monitored set, so these cannot run concurrently
    // without a read-your-writes race. Everything *after* this point is
    // read-only and fans out in parallel.
    let mut monitor_mode = "future".to_string();
    let mut monitor_mode_label = monitoring::MonitorMode::Future.label().to_string();
    if let Some(ref tracked) = db_series {
        if let Ok(summary) =
            monitoring_service::ensure_series_monitoring_rows(&state.db, tracked).await
        {
            monitor_mode = summary.mode.as_str().to_string();
            monitor_mode_label = summary.mode.label().to_string();
        } else {
            monitor_mode = tracked.monitor_mode.clone();
            monitor_mode_label = tracked.monitor_mode_enum().label().to_string();
        }
    }

    // Fan out the five independent read paths. Each one was previously
    // awaited serially — on a cold cache that meant 4+ sequential DB
    // round trips + the build_episodes fs-walk + the relation-group
    // artwork lookups all stacked end to end. Running them concurrently
    // collapses the total wait to ~max(...) instead of sum(...).
    let cfg_fut = config::get_config(&state.db);
    let relation_groups_fut = build_relation_groups(&state.db, db_id, &detail);

    let detail_for_episodes = detail.clone();
    let db_for_episodes = state.db.clone();
    let folder_for_episodes = folder_name.clone();
    let episodes_fut = async move {
        // Pull media_root from config inside the task so build_episodes
        // can still run in parallel with the outer config fetch. The
        // extra `get_config` hit here is harmless — the WAL page cache
        // will serve it from memory after the first concurrent fetch.
        let media_root = config::get_config(&db_for_episodes)
            .await
            .ok()
            .flatten()
            .map(|c| c.media_root)
            .unwrap_or_default();
        let out = build_episodes(
            &db_for_episodes,
            &detail_for_episodes,
            db_id,
            &folder_for_episodes,
            &media_root,
        )
        .await;
        (out, media_root)
    };

    let cover_key = db_series.as_ref().map(|s| format!("series-{}-cover", s.id));
    let banner_key = db_series
        .as_ref()
        .map(|s| format!("series-{}-banner", s.id));
    let cover_url_src = detail.cover_url.clone();
    let banner_url_src = detail.banner_url.clone();
    let detail_id = detail.id;
    let detail_mal_id = detail.id_mal;
    let db_for_art = state.db.clone();
    let cover_fut = async move {
        if let Some(key) = cover_key {
            artwork::cached_or_source_url(&db_for_art, &key, &cover_url_src).await
        } else if detail_id != 0 {
            artwork::first_cached_url(
                &db_for_art,
                &[
                    artwork::provider_cover_key(detail_id, detail_mal_id),
                    format!("provider-{}-cover", detail_id),
                ],
                &cover_url_src,
            )
            .await
        } else {
            cover_url_src
        }
    };
    let db_for_banner = state.db.clone();
    let banner_fut = async move {
        if let Some(key) = banner_key {
            artwork::cached_or_source_url(&db_for_banner, &key, &banner_url_src).await
        } else if detail_id != 0 {
            artwork::first_cached_url(
                &db_for_banner,
                &[
                    artwork::provider_banner_key(detail_id, detail_mal_id),
                    format!("provider-{}-banner", detail_id),
                ],
                &banner_url_src,
            )
            .await
        } else {
            banner_url_src
        }
    };

    let (cfg, relation_groups, episodes_out, cover_url, banner_url) = tokio::join!(
        cfg_fut,
        relation_groups_fut,
        episodes_fut,
        cover_fut,
        banner_fut,
    );
    let cfg = cfg.ok().flatten();
    let ((episodes, on_disk_count, downloaded_count, size_display, monitored_count), media_root) =
        episodes_out;
    detail.cover_url = cover_url;
    detail.banner_url = banner_url;

    let title_language = cfg
        .as_ref()
        .map(|c| c.title_language.clone())
        .unwrap_or_else(|| "english".to_string());

    let ep_total = detail.effective_episode_count();
    // #15a — render AL and MAL links independently. AL link is hidden
    // for the Jikan-fallback sentinel case (detail.id < 0); MAL link is
    // hidden only when no MAL id is known.
    let anilist_url = if detail.id > 0 {
        format!("https://anilist.co/anime/{}", detail.id)
    } else {
        String::new()
    };
    let mal_url = detail
        .id_mal
        .filter(|id| *id > 0)
        .map(|id| format!("https://myanimelist.net/anime/{}", id))
        .unwrap_or_default();

    // #15b — last metadata refresh. Look up by provider_id so both
    // AL-sourced and Jikan-fallback series route to the right cache row.
    let metadata_refreshed_at =
        match crate::models::metadata_cache::get_by_provider_id(&state.db, provider_id).await {
            Ok(Some(row)) => row.cached_at,
            _ => String::new(),
        };

    let all_monitored = ep_total > 0 && monitored_count >= ep_total;
    let allow_upgrades = db_series.as_ref().map(|s| s.allow_upgrades).unwrap_or(true);
    let custom_query_tokens = db_series
        .as_ref()
        .map(|s| s.custom_query_tokens.clone())
        .unwrap_or_default();
    let restrict_to_uploader = db_series
        .as_ref()
        .map(|s| s.restrict_to_uploader.clone())
        .unwrap_or_default();
    let default_custom_query_tokens = cfg
        .as_ref()
        .map(|c| c.default_custom_query_tokens.clone())
        .unwrap_or_default();
    let default_restrict_to_uploader = cfg
        .as_ref()
        .map(|c| c.default_restrict_to_uploader.clone())
        .unwrap_or_default();
    let post_processing_enabled = cfg
        .as_ref()
        .map(|c| c.post_processing_enabled)
        .unwrap_or(false);
    let template = SeriesTemplate {
        page: "library".to_string(),
        route_id: db_id.unwrap_or(provider_id),
        detail,
        is_tracked,
        db_id,
        folder_name,
        media_root,
        episodes,
        ep_total,
        on_disk_count,
        downloaded_count,
        size_display,
        title_language,
        relation_groups,
        anilist_url,
        mal_url,
        metadata_refreshed_at,
        monitor_mode,
        monitor_mode_label,
        monitored_count,
        all_monitored,
        allow_upgrades,
        custom_query_tokens,
        restrict_to_uploader,
        default_custom_query_tokens,
        default_restrict_to_uploader,
        post_processing_enabled,
    };
    Html(template.render().unwrap_or_default())
}

/// Maximum number of missing trailing Jikan episodes we'll tolerate before
/// falling back to Kitsu. MAL typically lags AniList's airing schedule by 1-2
/// episodes for long-running series (One Piece being the canonical case).
/// Without this tolerance, every One Piece page load re-runs the Kitsu title
/// search (`best_candidate` hits the Kitsu HTTP API before checking the
/// episode cache) to backfill 1-2 trailing episodes. And for long-running
/// shows Kitsu over-counts anyway — it lists episodes past the actual aired
/// count — so falling back here wouldn't even give us accurate titles.
const JIKAN_MAL_LAG_TOLERANCE: i32 = 10;

fn episode_needs_kitsu_backfill<F>(ep_count: i32, mut has_jikan_title: F) -> bool
where
    F: FnMut(i32) -> bool,
{
    if ep_count <= 1 {
        return false;
    }

    let missing = (1..=ep_count)
        .filter(|ep_num| !has_jikan_title(*ep_num))
        .count() as i32;
    missing > JIKAN_MAL_LAG_TOLERANCE
}

/// Build the episode list for a single series (no chain walking).
pub(super) async fn build_episodes(
    db: &SqlitePool,
    detail: &anilist::AnimeDetail,
    db_id: Option<i64>,
    folder_name: &str,
    media_root: &str,
) -> (Vec<Episode>, i32, i32, String, i32) {
    let ep_count = detail.effective_episode_count();
    // Fan out the four independent pre-fetches in parallel:
    //   1. disk file walk (blocking pool)
    //   2. cached episode metadata map (DB, with a fallback path)
    //   3. force_kitsu_fallback config flag (DB)
    //   4. monitored-episode set (DB, only when the series is tracked)
    //   5. per-episode quality tags (DB, only when the series is tracked)
    let media_root_owned = media_root.to_string();
    let folder_name_owned = folder_name.to_string();
    let disk_files_fut = tokio::task::spawn_blocking(move || {
        media::scan_series_folder(&media_root_owned, &folder_name_owned)
    });

    let detail_id = detail.id;
    let cached_eps_fut = async move {
        if let Some(sid) = db_id {
            let rows = local_metadata::get_episode_map_for_series(db, sid)
                .await
                .unwrap_or_default();
            if rows.is_empty() && detail_id != 0 {
                local_metadata::get_episode_map_for_provider(db, detail_id)
                    .await
                    .unwrap_or_default()
            } else {
                rows
            }
        } else if detail_id != 0 {
            local_metadata::get_episode_map_for_provider(db, detail_id)
                .await
                .unwrap_or_default()
        } else {
            HashMap::new()
        }
    };

    let monitored_fut = async move {
        match db_id {
            Some(id) => monitoring::get_monitored_episode_numbers(db, id)
                .await
                .unwrap_or_default()
                .into_iter()
                .collect::<std::collections::HashSet<i32>>(),
            None => std::collections::HashSet::new(),
        }
    };

    let quality_tags_fut = async move {
        match db_id {
            Some(id) => episode_tags::get_for_series(db, id)
                .await
                .unwrap_or_default(),
            None => std::collections::HashMap::new(),
        }
    };

    let (disk_files_res, cached_eps, force_kitsu_fallback, monitored_lookup, quality_tags) = tokio::join!(
        disk_files_fut,
        cached_eps_fut,
        force_kitsu_fallback_enabled(db),
        monitored_fut,
        quality_tags_fut,
    );
    let disk_files = disk_files_res.unwrap_or_default();
    let cached_matches_force =
        !force_kitsu_fallback || cached_eps.values().any(|ep| ep.source == "kitsu");
    let use_cached_eps = !cached_eps.is_empty() && cached_matches_force;

    let episodic_format = !matches!(detail.format.as_str(), "MOVIE" | "SPECIAL" | "OVA" | "ONA");
    // Issue #56: airing series whose episode total isn't known yet
    // (typical for MAL-fed currently-airing entries — Jikan reports
    // `episodes: null`) need the Jikan episodes endpoint as the *source*
    // of episode rows, not just titles. Without the `is_airing` arm an
    // ONA-format airing show like JoJo SBR ends up with `episodic_format
    // = false` AND `ep_count == 0`, so Jikan is skipped, the main
    // 1..=ep_count render loop emits nothing, and the page reads as a
    // zero-episode series even though `/anime/{id}/episodes` would have
    // returned the aired list.
    let is_airing_status = matches!(detail.status.as_str(), "RELEASING" | "CURRENTLY_AIRING");
    let should_fetch_jikan = !use_cached_eps
        && detail.id_mal.is_some()
        && (episodic_format || ep_count > 1 || is_airing_status);
    let jikan_eps = if should_fetch_jikan {
        jikan::fetch_episode_titles_for_detail(db, detail).await
    } else {
        HashMap::new()
    };

    // Promote the larger of (fresh Jikan fetch, locally-cached episode
    // map) into ep_count for airing series whose total wasn't known.
    // The downstream render loop (`for ep_num in 1..=ep_count`), the
    // template's `ep_total > 0` section gate, and the monitoring
    // counters all key off ep_count, so without this the fetched
    // episodes stay invisible.
    //
    // Both arms are needed: `jikan_eps` is only populated when
    // `should_fetch_jikan` fires, which requires `!use_cached_eps`. On
    // the cached path Jikan was skipped and `jikan_eps` stays empty, so
    // the promotion from `jikan_eps.len()` alone would be a no-op —
    // leaving an airing series rendered empty on every revisit after
    // the initial sync populated the local episode map.
    let ep_count = ep_count.max(jikan_eps.len() as i32).max(if use_cached_eps {
        cached_eps.len() as i32
    } else {
        0
    });

    let should_try_kitsu = !use_cached_eps
        && ep_count > 1
        && (force_kitsu_fallback
            || episode_needs_kitsu_backfill(ep_count.max(0), |ep_num| {
                jikan_eps
                    .get(&ep_num)
                    .map(|info| !info.title.trim().is_empty())
                    .unwrap_or(false)
            }));
    let kitsu_eps: HashMap<i32, kitsu::EpisodeInfo> = if should_try_kitsu {
        kitsu::fetch_episode_titles_fallback(
            db,
            &[
                detail.title_english.clone(),
                detail.title_romaji.clone(),
                detail.title_native.clone(),
            ],
            detail.season_year,
            detail.episodes,
        )
        .await
    } else {
        HashMap::new()
    };

    let is_tracked = db_id.is_some();

    let mut episodes = Vec::new();
    let mut on_disk_count = 0i32;
    let mut downloaded_count = 0i32;
    let mut total_size: u64 = 0;
    let mut monitored_count = 0i32;

    for ep_num in 1..=ep_count.max(0) {
        let disk_match = disk_files.iter().find(|f| {
            if let Some(s) = f.season_number {
                s == 1 && f.episode_number == ep_num
            } else {
                f.episode_number == ep_num
            }
        });

        let (on_disk, quality, size_display, filename) = match disk_match {
            Some(f) => (
                true,
                f.quality.clone(),
                f.size_display.clone(),
                f.filename.clone(),
            ),
            None => (false, String::new(), String::new(), String::new()),
        };

        if on_disk {
            on_disk_count += 1;
            if let Some(f) = disk_match {
                total_size += f.size_bytes;
            }
        }

        let use_series_fallback = ep_count <= 1;
        let fallback_title = if use_series_fallback {
            preferred_title(
                &detail.title_english,
                &detail.title_romaji,
                &detail.title_native,
            )
        } else {
            String::new()
        };
        let fallback_romaji = if use_series_fallback {
            non_empty_or(&detail.title_romaji, &fallback_title)
        } else {
            String::new()
        };
        let fallback_english = if use_series_fallback {
            non_empty_or(&detail.title_english, &fallback_title)
        } else {
            String::new()
        };
        let fallback_native = if use_series_fallback {
            non_empty_or(&detail.title_native, &fallback_title)
        } else {
            String::new()
        };

        let (ep_title, ep_title_romaji, ep_title_english, ep_title_native, ep_aired) =
            if use_cached_eps {
                if let Some(info) = cached_eps.get(&ep_num) {
                    (
                        non_empty_or(&info.title, &fallback_title),
                        non_empty_or(&info.title_romaji, &fallback_romaji),
                        non_empty_or(&info.title_english, &fallback_english),
                        non_empty_or(&info.title_native, &fallback_native),
                        info.aired.clone(),
                    )
                } else {
                    (
                        fallback_title.clone(),
                        fallback_romaji.clone(),
                        fallback_english.clone(),
                        fallback_native.clone(),
                        String::new(),
                    )
                }
            } else if force_kitsu_fallback {
                if let Some(kitsu_info) = kitsu_eps.get(&ep_num) {
                    let t = if !kitsu_info.title.trim().is_empty() {
                        kitsu_info.title.clone()
                    } else {
                        fallback_title.clone()
                    };
                    (t.clone(), t.clone(), t.clone(), t, kitsu_info.aired.clone())
                } else {
                    match jikan_eps.get(&ep_num) {
                        Some(info) if !info.title.trim().is_empty() => (
                            info.title.clone(),
                            info.title.clone(),
                            info.title.clone(),
                            info.title.clone(),
                            info.aired.clone(),
                        ),
                        Some(info) => (
                            fallback_title.clone(),
                            fallback_romaji.clone(),
                            fallback_english.clone(),
                            fallback_native.clone(),
                            info.aired.clone(),
                        ),
                        None => (
                            fallback_title,
                            fallback_romaji,
                            fallback_english,
                            fallback_native,
                            String::new(),
                        ),
                    }
                }
            } else {
                match jikan_eps.get(&ep_num) {
                    Some(info) if !info.title.trim().is_empty() => (
                        info.title.clone(),
                        info.title.clone(),
                        info.title.clone(),
                        info.title.clone(),
                        info.aired.clone(),
                    ),
                    Some(info) => (
                        fallback_title.clone(),
                        fallback_romaji.clone(),
                        fallback_english.clone(),
                        fallback_native.clone(),
                        info.aired.clone(),
                    ),
                    None => {
                        // Try Kitsu fallback for episode title/air date.
                        if let Some(kitsu_info) = kitsu_eps.get(&ep_num) {
                            let t = if !kitsu_info.title.trim().is_empty() {
                                kitsu_info.title.clone()
                            } else {
                                fallback_title.clone()
                            };
                            (t.clone(), t.clone(), t.clone(), t, kitsu_info.aired.clone())
                        } else {
                            (
                                fallback_title,
                                fallback_romaji,
                                fallback_english,
                                fallback_native,
                                String::new(),
                            )
                        }
                    }
                }
            };

        let monitored = monitored_lookup.contains(&ep_num);
        if monitored {
            monitored_count += 1;
        }

        // Quality display: disk file quality takes precedence; fall back to grab tag.
        let (display_quality, quality_state) = if !quality.is_empty() {
            (quality.clone(), "disk".to_string())
        } else if let Some(tag) = quality_tags.get(&ep_num) {
            (tag.quality_tag.clone(), tag.state.clone())
        } else {
            (String::new(), String::new())
        };

        let tag = quality_tags.get(&ep_num);
        let class_source = tag.map(|t| t.source.clone()).unwrap_or_default();
        let class_resolution = tag.map(|t| t.resolution.clone()).unwrap_or_default();
        let class_is_remux = tag.map(|t| t.is_remux).unwrap_or(false);
        let class_is_bdmv = tag.map(|t| t.is_bdmv).unwrap_or(false);
        let class_web_kind = tag.map(|t| t.web_kind.clone()).unwrap_or_default();
        let needs_review = tag.map(|t| t.needs_review).unwrap_or(false);
        let manual_override = tag.map(|t| t.manual_override).unwrap_or(false);

        let downloaded = on_disk || quality_state == "completed";
        if downloaded {
            downloaded_count += 1;
        }
        episodes.push(Episode {
            number: ep_num,
            title: ep_title,
            title_romaji: ep_title_romaji,
            title_english: ep_title_english,
            title_native: ep_title_native,
            aired: ep_aired,
            on_disk,
            downloaded,
            quality: display_quality,
            quality_state,
            size_display,
            filename,
            can_auto_search: is_tracked,
            monitored,
            class_source,
            class_resolution,
            class_is_remux,
            class_is_bdmv,
            class_web_kind,
            manual_override,
            needs_review,
        });
    }

    // Surface episodes the main 1..=ep_count loop didn't render. Two
    // cases:
    //   1. ep_count == 0 — movies or airing shows with no episodes yet;
    //      the main loop emits no rows, so every disk file lands here.
    //   2. ep_count > 0 but a release partitioned the series into more
    //      files than AniList's reported episode count. Canonical case:
    //      the [smol] Owarimonogatari BD splits the 48-min aired ep 1
    //      back into two ~24-min files, so S1 has 13 files on disk vs
    //      AL's 12 eps. Auto-expand backfills a grab-tag row for the
    //      overflow ep at grab time AND routes the file to the parent
    //      folder at post-process time. Both pre-import ("downloading"
    //      row from the grab tag) and post-import ("imported" row from
    //      the disk file) need to render — without this pass, the main
    //      loop only iterated 1..=ep_count and the overflow was
    //      orphaned in either state. See issue #45.
    let mut rendered_eps: std::collections::HashSet<i32> =
        episodes.iter().map(|e| e.number).collect();

    // Pass 1: on-disk files past ep_count. Takes precedence — a file
    // on disk carries size/filename/quality that a bare grab tag
    // doesn't, and we want the "imported" state to win over any stale
    // "grabbed" tag if the user somehow hits this for both sources.
    for f in &disk_files {
        // Match the main loop's season filter on the ep_count > 0 path:
        // only render season 1 / unseasoned files. Specials/ or S02
        // files under a tracked series folder aren't part of the main
        // episode list. The ep_count == 0 path historically rendered
        // every file regardless of season — preserve that behavior to
        // avoid regressions for movies and airing-with-no-episodes shows.
        if ep_count > 0
            && let Some(s) = f.season_number
            && s != 1
        {
            continue;
        }
        if f.episode_number <= 0 {
            continue;
        }
        if rendered_eps.contains(&f.episode_number) {
            continue;
        }

        on_disk_count += 1;
        downloaded_count += 1;
        total_size += f.size_bytes;
        let monitored = monitored_lookup.contains(&f.episode_number);
        if monitored {
            monitored_count += 1;
        }
        let (display_quality, quality_state) = if !f.quality.is_empty() {
            (f.quality.clone(), "disk".to_string())
        } else if let Some(tag) = quality_tags.get(&f.episode_number) {
            (tag.quality_tag.clone(), tag.state.clone())
        } else {
            (String::new(), String::new())
        };
        let tag = quality_tags.get(&f.episode_number);
        let class_source = tag.map(|t| t.source.clone()).unwrap_or_default();
        let class_resolution = tag.map(|t| t.resolution.clone()).unwrap_or_default();
        let class_is_remux = tag.map(|t| t.is_remux).unwrap_or(false);
        let class_is_bdmv = tag.map(|t| t.is_bdmv).unwrap_or(false);
        let class_web_kind = tag.map(|t| t.web_kind.clone()).unwrap_or_default();
        let needs_review = tag.map(|t| t.needs_review).unwrap_or(false);
        let manual_override = tag.map(|t| t.manual_override).unwrap_or(false);
        rendered_eps.insert(f.episode_number);
        episodes.push(Episode {
            number: f.episode_number,
            title: String::new(),
            title_romaji: String::new(),
            title_english: String::new(),
            title_native: String::new(),
            aired: String::new(),
            on_disk: true,
            // This branch only runs when the file already exists under
            // media_root (on_disk=true), so `downloaded` is
            // unconditionally true regardless of tag state.
            downloaded: true,
            quality: display_quality,
            quality_state,
            size_display: f.size_display.clone(),
            filename: f.filename.clone(),
            can_auto_search: is_tracked,
            monitored,
            class_source,
            class_resolution,
            class_is_remux,
            class_is_bdmv,
            class_web_kind,
            manual_override,
            needs_review,
        });
    }

    // Pass 2: grab-tag rows past ep_count with no matching disk file
    // yet. This is what makes the overflow row render as "downloading"
    // immediately after the batch is queued — auto-expand writes the
    // grab tag, the torrent is still downloading so nothing is on disk,
    // and without this pass the row would be invisible until post-
    // processing imports it.
    if ep_count > 0 {
        for (&ep_num, tag) in quality_tags.iter() {
            if ep_num <= ep_count {
                continue;
            }
            if rendered_eps.contains(&ep_num) {
                continue;
            }

            let monitored = monitored_lookup.contains(&ep_num);
            if monitored {
                monitored_count += 1;
            }
            // `downloaded` tracks completed-state episodes; an overflow
            // tag in 'grabbed' state is mid-download so it counts only
            // when the tag has already been flipped to 'completed' by
            // post-processing. Mirrors the main loop's treatment.
            let downloaded = tag.state == "completed";
            if downloaded {
                downloaded_count += 1;
            }
            rendered_eps.insert(ep_num);
            episodes.push(Episode {
                number: ep_num,
                title: String::new(),
                title_romaji: String::new(),
                title_english: String::new(),
                title_native: String::new(),
                aired: String::new(),
                on_disk: false,
                downloaded,
                quality: tag.quality_tag.clone(),
                quality_state: tag.state.clone(),
                size_display: String::new(),
                filename: String::new(),
                can_auto_search: is_tracked,
                monitored,
                class_source: tag.source.clone(),
                class_resolution: tag.resolution.clone(),
                class_is_remux: tag.is_remux,
                class_is_bdmv: tag.is_bdmv,
                class_web_kind: tag.web_kind.clone(),
                manual_override: tag.manual_override,
                needs_review: tag.needs_review,
            });
        }
    }

    episodes.sort_by_key(|e| std::cmp::Reverse(e.number));

    let size_display = format_size(total_size);
    (
        episodes,
        on_disk_count,
        downloaded_count,
        size_display,
        monitored_count,
    )
}

fn relation_identity_key(provider_id: i64, mal_id: Option<i64>) -> String {
    if let Some(mal_id) = mal_id {
        format!("mal:{mal_id}")
    } else {
        format!("provider:{provider_id}")
    }
}

/// Resolve the best link ID for a relation card.  If the related entry is
/// tracked in the library (by AniList ID or MAL ID), return the DB series ID
/// so the link always navigates to `/series/<db_id>`.  Otherwise fall back to
/// the provider ID (which may be negative for MAL-sourced entries, but the
/// detail resolver in `resolve_series_context` knows how to handle that).
async fn resolve_relation_card_id(db: &SqlitePool, provider_id: i64, mal_id: Option<i64>) -> i64 {
    // Try AniList ID first (positive IDs).
    if provider_id > 0
        && let Ok(Some(row)) = series::get_by_anilist_id(db, provider_id).await
    {
        return row.id;
    }
    // Try MAL ID.
    if let Some(mid) = mal_id
        && let Ok(Some(row)) = series::get_by_mal_id(db, mid).await
    {
        return row.id;
    }
    // For MAL-sourced entries, the anilist_id column stores -mal_id.
    if provider_id < 0
        && let Ok(Some(row)) = series::get_by_anilist_id(db, provider_id).await
    {
        return row.id;
    }
    provider_id
}

fn relation_richness(rel: &anilist::RelatedEntry) -> i32 {
    let mut score = 0;
    if !rel.cover_url.trim().is_empty() {
        score += 4;
    }
    if !rel.format.trim().is_empty() && rel.format != "TBA" {
        score += 2;
    }
    if !rel.status.trim().is_empty() && rel.status != "TBA" {
        score += 2;
    }
    if rel.episodes.unwrap_or(0) > 0 {
        score += 1;
    }
    if !preferred_title(&rel.title_english, &rel.title_romaji, &rel.title_native)
        .trim()
        .is_empty()
    {
        score += 1;
    }
    score
}

fn merge_relation_metadata(
    primary: &anilist::RelatedEntry,
    fallback: &anilist::RelatedEntry,
) -> anilist::RelatedEntry {
    let mut merged = primary.clone();

    if merged.title_romaji.trim().is_empty() {
        merged.title_romaji = fallback.title_romaji.clone();
    }
    if merged.title_english.trim().is_empty() {
        merged.title_english = fallback.title_english.clone();
    }
    if merged.title_native.trim().is_empty() {
        merged.title_native = fallback.title_native.clone();
    }
    if merged.cover_url.trim().is_empty() {
        merged.cover_url = fallback.cover_url.clone();
    }
    if merged.format.trim().is_empty() || merged.format == "TBA" {
        merged.format = fallback.format.clone();
    }
    if merged.status.trim().is_empty() || merged.status == "TBA" {
        merged.status = fallback.status.clone();
        merged.status_display = fallback.status_display.clone();
    }
    if merged.episodes.is_none() || merged.episodes == Some(0) {
        merged.episodes = fallback.episodes;
    }
    if merged.season_year.is_none() {
        merged.season_year = fallback.season_year;
    }
    if merged.id_mal.is_none() {
        merged.id_mal = fallback.id_mal;
    }
    if merged.media_type.trim().is_empty() {
        merged.media_type = fallback.media_type.clone();
    }

    merged
}

/// Group the detail's relations by type for display as cards.
async fn build_relation_groups(
    db: &SqlitePool,
    db_id: Option<i64>,
    detail: &anilist::AnimeDetail,
) -> Vec<RelationGroup> {
    let cached_relations = if let Some(series_id) = db_id {
        let rows = local_metadata::get_relations_for_series(db, series_id)
            .await
            .unwrap_or_default();
        if rows.is_empty() && detail.id != 0 {
            local_metadata::get_relations_for_provider(db, detail.id)
                .await
                .unwrap_or_default()
        } else {
            rows
        }
    } else if detail.id != 0 {
        local_metadata::get_relations_for_provider(db, detail.id)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Treat the current AniList detail payload as the canonical relation graph whenever it is
    // available. Cached relation rows can be stale from older MAL/Jikan hydration passes, which is
    // how the same title ends up rendered twice under two different relation tags.
    let has_authoritative_relations = !detail.relations.is_empty();
    let mut relations = if has_authoritative_relations {
        detail.relations.clone()
    } else {
        cached_relations.clone()
    };

    if has_authoritative_relations {
        let by_identity: HashMap<String, usize> = relations
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r.media_type.as_str(), "ANIME" | "MUSIC"))
            .map(|(idx, r)| (relation_identity_key(r.id, r.id_mal), idx))
            .collect();

        for cached in cached_relations {
            if !matches!(cached.media_type.as_str(), "ANIME" | "MUSIC") {
                continue;
            }
            let key = relation_identity_key(cached.id, cached.id_mal);
            let Some(idx) = by_identity.get(&key).copied() else {
                continue;
            };
            let merged = merge_relation_metadata(&relations[idx], &cached);
            relations[idx] = merged;
        }
    }

    if !has_authoritative_relations && (detail.id != 0 || detail.id_mal.is_some()) {
        let existing_relation_keys: std::collections::HashSet<String> = relations
            .iter()
            .filter(|r| matches!(r.media_type.as_str(), "ANIME" | "MUSIC"))
            .map(|r| relation_identity_key(r.id, r.id_mal))
            .collect();
        let incoming =
            local_metadata::get_incoming_relations_for_provider(db, detail.id, detail.id_mal)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|r| {
                    !existing_relation_keys.contains(&relation_identity_key(r.id, r.id_mal))
                })
                .collect::<Vec<_>>();
        relations.extend(incoming);
    }

    // Build identity key for the current series so we can filter self-references.
    let self_key = relation_identity_key(detail.id, detail.id_mal);

    let mut deduped: Vec<anilist::RelatedEntry> = Vec::new();
    let mut deduped_index: HashMap<(String, String), usize> = HashMap::new();
    for related in relations {
        if !matches!(related.media_type.as_str(), "ANIME" | "MUSIC") {
            continue;
        }
        // Skip self-references: relations that point back to the current series.
        let related_key = relation_identity_key(related.id, related.id_mal);
        if related_key == self_key {
            continue;
        }
        let normalized_type =
            local_metadata::normalize_relation_type(&related.relation_type).to_string();
        let key = (related_key, normalized_type);
        if let Some(idx) = deduped_index.get(&key).copied() {
            if relation_richness(&deduped[idx]) < relation_richness(&related) {
                deduped[idx] = related;
            }
        } else {
            deduped_index.insert(key, deduped.len());
            deduped.push(related);
        }
    }
    let relations = deduped;

    let type_order = [
        "PREQUEL",
        "SEQUEL",
        "SIDE_STORY",
        "ALTERNATIVE",
        "SUMMARY",
        "FULL_STORY",
        "SPIN_OFF",
        "OTHER",
        "CHARACTER",
        "PARENT",
        "ADAPTATION",
    ];

    // Resolve the per-relation card_id + cover_url concurrently.
    let mut join_set: tokio::task::JoinSet<(usize, i64, String)> = tokio::task::JoinSet::new();
    for (idx, related) in relations.iter().enumerate() {
        if !matches!(related.media_type.as_str(), "ANIME" | "MUSIC") {
            continue;
        }
        let db = db.clone();
        let rel_id = related.id;
        let rel_mal = related.id_mal;
        let rel_cover = related.cover_url.clone();
        join_set.spawn(async move {
            let card_id = resolve_relation_card_id(&db, rel_id, rel_mal).await;
            let cover_url = if let Some(series_id) = db_id {
                artwork::first_cached_url(
                    &db,
                    &[
                        artwork::series_relation_cover_key(series_id, rel_id, rel_mal),
                        format!("series-{}-relation-{}-cover", series_id, rel_id),
                        artwork::provider_cover_key(rel_id, rel_mal),
                        format!("provider-{}-cover", rel_id),
                    ],
                    &rel_cover,
                )
                .await
            } else if rel_id != 0 || rel_mal.is_some() {
                artwork::first_cached_url(
                    &db,
                    &[
                        artwork::provider_cover_key(rel_id, rel_mal),
                        format!("provider-{}-cover", rel_id),
                    ],
                    &rel_cover,
                )
                .await
            } else {
                rel_cover
            };
            (idx, card_id, cover_url)
        });
    }

    let mut resolved: HashMap<usize, (i64, String)> = HashMap::new();
    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok((idx, card_id, cover_url)) => {
                resolved.insert(idx, (card_id, cover_url));
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "build_relation_groups: relation resolver task failed; skipping one relation card"
                );
            }
        }
    }

    let mut groups: HashMap<String, Vec<RelationCard>> = HashMap::new();

    for (idx, related) in relations.iter().enumerate() {
        if !matches!(related.media_type.as_str(), "ANIME" | "MUSIC") {
            continue;
        }
        let Some((card_id, cover_url)) = resolved.remove(&idx) else {
            continue;
        };

        let normalized_relation_type =
            local_metadata::normalize_relation_type(&related.relation_type).to_string();
        let cards = groups.entry(normalized_relation_type).or_default();

        cards.push(RelationCard {
            id: card_id,
            title: preferred_title(
                &related.title_english,
                &related.title_romaji,
                &related.title_native,
            ),
            title_romaji: related.title_romaji.clone(),
            title_english: related.title_english.clone(),
            title_native: related.title_native.clone(),
            cover_url,
            format: related.format.clone(),
            status: related.status.clone(),
            episodes: related.episodes,
        });
    }

    let mut result: Vec<RelationGroup> = groups
        .into_iter()
        .map(|(rel_type, mut entries)| {
            entries.sort_by(|a, b| {
                let a_title = a.title.to_ascii_lowercase();
                let b_title = b.title.to_ascii_lowercase();
                a_title
                    .cmp(&b_title)
                    .then_with(|| {
                        a.title_romaji
                            .to_ascii_lowercase()
                            .cmp(&b.title_romaji.to_ascii_lowercase())
                    })
                    .then_with(|| a.id.cmp(&b.id))
            });
            let label = format_relation_label(&rel_type);
            RelationGroup {
                relation_type: rel_type,
                label,
                entries,
            }
        })
        .collect();

    result.sort_by_key(|g| {
        type_order
            .iter()
            .position(|t| *t == g.relation_type)
            .unwrap_or(99)
    });
    result
}

fn format_relation_label(rel_type: &str) -> String {
    match rel_type {
        "PREQUEL" => "Prequel".to_string(),
        "SEQUEL" => "Sequel".to_string(),
        "SIDE_STORY" => "Side Story".to_string(),
        "ALTERNATIVE" => "Alternative".to_string(),
        "SUMMARY" => "Summary".to_string(),
        "FULL_STORY" => "Full Story".to_string(),
        "SPIN_OFF" => "Spin Off".to_string(),
        "OTHER" => "Other".to_string(),
        "CHARACTER" => "Character".to_string(),
        "PARENT" => "Parent".to_string(),
        "ADAPTATION" => "Adaptation".to_string(),
        other => other.replace('_', " "),
    }
}

fn non_empty_or(value: &str, fallback: &str) -> String {
    if !value.trim().is_empty() {
        value.to_string()
    } else {
        fallback.to_string()
    }
}

fn preferred_title(english: &str, romaji: &str, native: &str) -> String {
    if !english.is_empty() {
        english.to_string()
    } else if !romaji.is_empty() {
        romaji.to_string()
    } else {
        native.to_string()
    }
}

fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        return String::new();
    }
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gb >= 1.0 {
        format!("{:.1} GiB", gb)
    } else {
        format!("{:.0} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::series;

    fn unique_media_root(suffix: &str) -> std::path::PathBuf {
        let nonce = format!(
            "ryokan_pages_test_{}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            suffix,
        );
        let root = std::env::temp_dir().join(nonce);
        std::fs::create_dir_all(&root).expect("create media root");
        root
    }

    fn empty_anime_detail(
        id: i64,
        title_english: &str,
        episodes: Option<i32>,
    ) -> anilist::AnimeDetail {
        anilist::AnimeDetail {
            id,
            id_mal: None,
            title_romaji: title_english.to_string(),
            title_english: title_english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            status_display: "Finished".to_string(),
            episodes,
            duration: Some(24),
            season: String::new(),
            season_year: Some(2015),
            end_year: Some(2015),
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

    /// Issue #45: a BD release can partition a series into more files
    /// than AniList reports (Owarimonogatari S1 — AL says 12 eps, the
    /// [smol] BD has 13 files because it splits the 48-min aired ep 1
    /// back into two halves). Before the fix, `build_episodes` only
    /// looped 1..=ep_count, so file 13 was routed to disk by
    /// auto-expand but never rendered in the UI. The fix surfaces
    /// any on-disk file with ep > ep_count as its own row.
    #[tokio::test]
    async fn build_episodes_surfaces_on_disk_files_beyond_anilist_episode_count() {
        let db = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (series_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 21320,
                mal_id: None,
                title: "Owarimonogatari",
                title_romaji: "Owarimonogatari",
                title_english: "Owarimonogatari",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(12),
                season_year: Some(2015),
                end_year: Some(2015),
            },
        )
        .await
        .expect("series upsert");

        // Write 13 synthetic episode files — ep 13 exceeds AL's count.
        let media_root = unique_media_root("surface_beyond_count");
        let series_folder = media_root.join("Owarimonogatari");
        std::fs::create_dir_all(&series_folder).expect("create series dir");
        for ep in 1..=13 {
            let fname = format!("Owarimonogatari - S01E{:02} - Episode.mkv", ep);
            std::fs::write(series_folder.join(&fname), b"x").expect("write ep file");
        }

        // AL reports 12 eps (the on-air ep 1 was a 48-min merged episode).
        let detail = empty_anime_detail(21320, "Owarimonogatari", Some(12));

        let (episodes, on_disk_count, downloaded_count, _size, _monitored) = build_episodes(
            &db,
            &detail,
            Some(series_id),
            "Owarimonogatari",
            media_root.to_str().expect("media root str"),
        )
        .await;

        // Sorted desc by number, so ep 13 is first.
        assert_eq!(
            episodes.len(),
            13,
            "expected 13 rows (1..=12 from AL count + 13 from disk overflow), got {}",
            episodes.len()
        );
        let ep13 = episodes
            .iter()
            .find(|e| e.number == 13)
            .expect("ep 13 row present");
        assert!(ep13.on_disk, "ep 13 must render as on_disk");
        assert_eq!(on_disk_count, 13, "on_disk_count must include the overflow");
        assert_eq!(downloaded_count, 13, "downloaded_count same");

        // Cleanup (best effort).
        std::fs::remove_dir_all(&media_root).ok();
    }

    /// Regression guard: when every on-disk file falls within AL's
    /// ep_count, the surface-beyond-count pass must not duplicate rows
    /// the main loop already rendered.
    #[tokio::test]
    async fn build_episodes_does_not_duplicate_rows_when_disk_matches_count() {
        let db = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (series_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 999,
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

        let media_root = unique_media_root("no_duplicates");
        let series_folder = media_root.join("Test Series");
        std::fs::create_dir_all(&series_folder).expect("create series dir");
        for ep in 1..=12 {
            let fname = format!("Test Series - S01E{:02} - Episode.mkv", ep);
            std::fs::write(series_folder.join(&fname), b"x").expect("write ep file");
        }

        let detail = empty_anime_detail(999, "Test Series", Some(12));

        let (episodes, _, _, _, _) = build_episodes(
            &db,
            &detail,
            Some(series_id),
            "Test Series",
            media_root.to_str().expect("media root str"),
        )
        .await;

        assert_eq!(episodes.len(), 12, "no duplicates: exactly 12 rows");

        std::fs::remove_dir_all(&media_root).ok();
    }

    /// Issue #45 follow-up: during the download the overflow file isn't
    /// on disk yet, but auto-expand has already written a grab-tag row
    /// for it. `build_episodes` must surface that tag as a row (in
    /// 'grabbed' state) so the user sees the extra episode's download
    /// progress immediately — not just after post-processing runs.
    #[tokio::test]
    async fn build_episodes_surfaces_grab_tags_beyond_ep_count_without_disk_file() {
        use crate::services::source::ClassificationResult;

        let db = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");

        let (series_id, _) = series::upsert(
            &db,
            series::SeriesCore {
                anilist_id: 21262,
                mal_id: None,
                title: "Owarimonogatari",
                title_romaji: "Owarimonogatari",
                title_english: "Owarimonogatari",
                title_native: "",
                cover_url: "",
                format: "TV",
                status: "FINISHED",
                episodes: Some(12),
                season_year: Some(2015),
                end_year: Some(2015),
            },
        )
        .await
        .expect("series upsert");

        // Write a grab tag for ep 13 (AL-overflow) — simulates what
        // auto_expand::expand_from_files does when it backfills a tag
        // for a parent file whose parsed ep exceeds AL's count.
        crate::models::episode_tags::record_grab(
            &db,
            series_id,
            13,
            &ClassificationResult::unknown(),
            "[smol] Monogatari - S07 [BD 1080p HEVC Opus]",
            "smol",
            0,
            true,
        )
        .await
        .expect("record_grab for ep 13");

        // Empty media root — torrent is still downloading, nothing
        // has landed in the library folder yet.
        let media_root = unique_media_root("surfaces_grab_tag_no_disk");
        let series_folder = media_root.join("Owarimonogatari");
        std::fs::create_dir_all(&series_folder).expect("create series dir");

        let detail = empty_anime_detail(21262, "Owarimonogatari", Some(12));

        let (episodes, on_disk_count, downloaded_count, _size, _monitored) = build_episodes(
            &db,
            &detail,
            Some(series_id),
            "Owarimonogatari",
            media_root.to_str().expect("media root str"),
        )
        .await;

        assert_eq!(
            episodes.len(),
            13,
            "expected 13 rows (1..=12 from AL + overflow E13 from grab tag), got {}",
            episodes.len()
        );
        let ep13 = episodes
            .iter()
            .find(|e| e.number == 13)
            .expect("ep 13 row present from grab tag");
        assert!(!ep13.on_disk, "no disk file yet, so on_disk must be false");
        assert!(!ep13.downloaded, "tag state is 'grabbed', not 'completed'");
        assert_eq!(ep13.quality_state, "grabbed");
        assert_eq!(on_disk_count, 0, "nothing on disk yet");
        assert_eq!(downloaded_count, 0, "nothing completed yet");

        std::fs::remove_dir_all(&media_root).ok();
    }
}
