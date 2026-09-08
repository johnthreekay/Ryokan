//! `Page.airingSchedules` batch fetcher (issue #115 — calendar feed).
//!
//! Returns every upcoming-or-recent episode for a list of AniList
//! media IDs in a single AL query (paged through `Page.pageInfo`'s
//! `hasNextPage`). Used by `services::calendar` as the data source
//! for both the iCal feed and the in-app calendar page.
//!
//! ## Why this lives here
//!
//! AL-specific concerns (GraphQL shape, rate-limit pacing,
//! cooldown-on-429/5xx, header-driven throttle adjustment) all
//! belong inside `services::anilist`. The cache + DB join layers
//! live in `services::calendar` so the AL/transport part can be
//! tested in isolation against a wiremock fixture without
//! materializing a series table.
//!
//! Per the parent CLAUDE.md "Airing schedule batch query" note: the
//! AL rate limit degrades to **30/min** on this endpoint, so the
//! caller (`services::airing_refresh`) only fires this query on its
//! 12h supervised tick and writes the result to the local
//! `episode_airings` table; calendar requests never round-trip to
//! AL so a thundering herd of iCal subscribers doesn't drain the
//! budget.

use serde_json::json;

use super::rate_limit::{
    ANILIST_COOLDOWN_DEFAULT, extract_graphql_error, record_rate_limit_headers,
    set_anilist_cooldown, throttle_before_anilist_request,
};
use super::{HTTP_CLIENT, anilist_post};

/// One row of AL's `Page.airingSchedules` response. Exactly the
/// shape the GraphQL query asks for; transformation into the
/// caller-friendly [`UpcomingEpisode`](crate::services::calendar::UpcomingEpisode)
/// happens in `services::calendar`.
#[derive(Debug, Clone)]
pub struct AiringSchedule {
    pub episode: i32,
    pub airing_at: i64,
    pub media_id: i32,
    /// Per-episode runtime in minutes. AL exposes this on `Media.duration`,
    /// not on the schedule row itself, so it's the same value for every
    /// episode of a given series. `None` when AL hasn't populated it
    /// (rare — most listed series have a duration). Caller defaults to
    /// 24 minutes when emitting `DTEND` on iCal events.
    pub duration_minutes: Option<i32>,
    pub title_romaji: String,
    pub title_english: String,
    pub title_native: String,
}

const PER_PAGE: usize = 50;

/// Fetch every airing schedule for `ids` whose `airingAt` falls in
/// `from..to` (Unix epoch seconds). Returns an empty Vec when `ids`
/// is empty (no AL request issued). Walks `Page.pageInfo.hasNextPage`
/// until exhausted; with `PER_PAGE = 50` and AL's 30/min degraded
/// budget, even a 500-series library (~5 pages worst case) costs
/// 1/6 of the per-minute budget per refresh.
///
/// Negative-AL-id sentinel series (Jikan-fallback rows where
/// `series.anilist_id = -mal_id`) are caller-filtered, not handled
/// here — passing a negative id would hit AL with an invalid ID
/// and return an empty result. The caller in `services::calendar`
/// applies the `anilist_id > 0` filter, matching the convention
/// every other AL call site uses.
pub async fn fetch_airing_schedules(
    ids: &[i32],
    from: i64,
    to: i64,
) -> Result<Vec<AiringSchedule>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut out: Vec<AiringSchedule> = Vec::new();
    let mut page = 1;
    loop {
        let (mut chunk, has_next) = fetch_page(ids, from, to, page).await?;
        out.append(&mut chunk);
        if !has_next {
            break;
        }
        page += 1;
        // Defensive cap. A 500-series library at perPage=50 with
        // every series airing in-window would top out at ~5 pages.
        // 50 pages = 2500 in-window airings, far past anything a
        // single user has. If we ever hit this, something has
        // gone wrong with the response (infinite hasNextPage loop)
        // and we'd rather bail than spend the whole rate budget
        // on it.
        if page > 50 {
            return Err(
                "AniList airingSchedules pagination exceeded 50 pages — aborting".to_string(),
            );
        }
    }
    Ok(out)
}

async fn fetch_page(
    ids: &[i32],
    from: i64,
    to: i64,
    page: i32,
) -> Result<(Vec<AiringSchedule>, bool), String> {
    let query = r#"
        query ($ids: [Int], $from: Int, $to: Int, $page: Int, $perPage: Int) {
          Page(page: $page, perPage: $perPage) {
            pageInfo { hasNextPage }
            airingSchedules(mediaId_in: $ids, airingAt_greater: $from, airingAt_lesser: $to, sort: TIME) {
              episode
              airingAt
              mediaId
              media {
                duration
                title { romaji english native }
              }
            }
          }
        }
    "#;
    let gql = json!({
        "query": query,
        "variables": {
            "ids": ids,
            "from": from,
            "to": to,
            "page": page,
            "perPage": PER_PAGE,
        },
    });

    throttle_before_anilist_request().await;
    let resp = match anilist_post(&HTTP_CLIENT).json(&gql).send().await {
        Ok(r) => r,
        Err(e) => {
            return Err(format!(
                "AniList unavailable: airingSchedules request failed: {e}"
            ));
        }
    };

    let status = resp.status();
    record_rate_limit_headers(resp.headers());

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        let retry_after_secs = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        set_anilist_cooldown(retry_after_secs, ANILIST_COOLDOWN_DEFAULT);
        // Use the standard prefix so downstream callers can match on
        // the failure taxonomy ("AniList rate-limited" / "AniList
        // unavailable") rather than HTTP codes — same shape every
        // other AL call site uses.
        return Err(if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            format!(
                "AniList rate-limited (airingSchedules){}",
                retry_after_secs
                    .map(|r| format!(" — retry in {r}s"))
                    .unwrap_or_default()
            )
        } else {
            format!("AniList unavailable: airingSchedules HTTP {status}")
        });
    }

    let body_text = resp
        .text()
        .await
        .map_err(|e| format!("AniList unavailable: read body: {e}"))?;
    let body: serde_json::Value = serde_json::from_str(&body_text).map_err(|e| {
        format!(
            "AniList unavailable: parse JSON ({}): {}",
            e,
            body_text.chars().take(200).collect::<String>()
        )
    })?;

    if !status.is_success() {
        // The 429/5xx cooldown branch above handles those statuses;
        // anything else here is a 4xx that the caller can treat as
        // unavailable (no cooldown — we don't know the cause).
        let msg = extract_graphql_error(&body).unwrap_or_else(|| body.to_string());
        return Err(format!(
            "AniList unavailable: airingSchedules HTTP {status}: {msg}"
        ));
    }
    if let Some(msg) = extract_graphql_error(&body) {
        return Err(format!(
            "AniList unavailable: airingSchedules GraphQL error: {msg}"
        ));
    }

    let page_obj = &body["data"]["Page"];
    let has_next = page_obj["pageInfo"]["hasNextPage"]
        .as_bool()
        .unwrap_or(false);
    let arr = match page_obj["airingSchedules"].as_array() {
        Some(a) => a,
        None => {
            // Schema mismatch — log and return empty for this page so
            // the caller can still serve a stale-cache or empty
            // calendar instead of erroring out the whole feed.
            tracing::warn!(
                target: "ryokan::anilist::airing",
                "Page.airingSchedules missing in response — schema mismatch?"
            );
            return Ok((Vec::new(), false));
        }
    };

    let mut out: Vec<AiringSchedule> = Vec::with_capacity(arr.len());
    for item in arr {
        let episode = item["episode"].as_i64().unwrap_or(0) as i32;
        let airing_at = item["airingAt"].as_i64().unwrap_or(0);
        let media_id = item["mediaId"].as_i64().unwrap_or(0) as i32;
        if episode <= 0 || airing_at <= 0 || media_id <= 0 {
            // Defensive — AL rarely returns missing fields here, but
            // a partial schedule row is unusable for SUMMARY/UID/DTSTART
            // emission; skip rather than emit a broken VEVENT.
            continue;
        }
        let duration_minutes = item["media"]["duration"].as_i64().map(|d| d as i32);
        let title = &item["media"]["title"];
        let title_romaji = title["romaji"].as_str().unwrap_or("").to_string();
        let title_english = title["english"].as_str().unwrap_or("").to_string();
        let title_native = title["native"].as_str().unwrap_or("").to_string();
        out.push(AiringSchedule {
            episode,
            airing_at,
            media_id,
            duration_minutes,
            title_romaji,
            title_english,
            title_native,
        });
    }
    Ok((out, has_next))
}
