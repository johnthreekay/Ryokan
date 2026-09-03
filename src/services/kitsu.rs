use serde::Deserialize;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::Duration;

use crate::services::anilist::AnimeDetail;
use crate::services::html::sanitize_rich_description;

const KITSU_API_DEFAULT: &str = "https://kitsu.io/api/edge";

/// Kitsu API base URL, with a `RYOKAN_KITSU_API_BASE` override the same
/// shape as `RYOKAN_ANILIST_API_BASE` / `JIKAN_API_BASE`. Re-read on
/// every call rather than cached so wiremock fixtures can flip it
/// per-fixture without process restart.
fn kitsu_api_base() -> String {
    std::env::var("RYOKAN_KITSU_API_BASE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| KITSU_API_DEFAULT.to_string())
}

/// Shared reqwest client. Replaces a per-call `Client::new()` so the
/// connection pool is reused across the search/detail fetch helpers.
/// Timeouts (10s connect, 30s overall) bound a hung connection so it
/// can't pin a pool slot for hours waiting on TCP keepalive.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("building the Kitsu reqwest client should not fail")
});
const CACHE_TTL_SECS: i64 = 7 * 24 * 60 * 60;
const NEGATIVE_CACHE_SENTINEL: &str = "__RYOKAN_EMPTY__";

#[derive(Debug, Clone)]
pub struct EpisodeInfo {
    pub title: String,
    pub aired: String,
}

#[derive(Debug, Clone)]
struct Candidate {
    id: i64,
    canonical_title: String,
    titles: HashMap<String, String>,
    abbreviated_titles: Vec<String>,
    synopsis: String,
    poster_image: ImageSet,
    cover_image: ImageSet,
    subtype: String,
    status: String,
    episode_count: Option<i32>,
    episode_length: Option<i32>,
    start_date: Option<String>,
    end_date: Option<String>,
    average_rating: Option<String>,
    nsfw: bool,
}

#[derive(Debug, Deserialize)]
struct CollectionResponse<T> {
    data: Vec<Resource<T>>,
    links: Option<PaginationLinks>,
}

#[derive(Debug, Deserialize)]
struct Resource<T> {
    id: String,
    attributes: T,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct PaginationLinks {
    next: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct ImageSet {
    tiny: Option<String>,
    small: Option<String>,
    medium: Option<String>,
    large: Option<String>,
    original: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnimeAttributes {
    canonical_title: Option<String>,
    titles: Option<HashMap<String, String>>,
    abbreviated_titles: Option<Vec<String>>,
    synopsis: Option<String>,
    poster_image: Option<ImageSet>,
    cover_image: Option<ImageSet>,
    subtype: Option<String>,
    status: Option<String>,
    episode_count: Option<i32>,
    episode_length: Option<i32>,
    start_date: Option<String>,
    end_date: Option<String>,
    average_rating: Option<String>,
    #[serde(default)]
    nsfw: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EpisodeAttributes {
    canonical_title: Option<String>,
    titles: Option<HashMap<String, String>>,
    number: Option<i32>,
    relative_number: Option<i32>,
    air_date: Option<String>,
}

fn first_image(images: &ImageSet) -> String {
    images
        .original
        .clone()
        .or_else(|| images.large.clone())
        .or_else(|| images.medium.clone())
        .or_else(|| images.small.clone())
        .or_else(|| images.tiny.clone())
        .unwrap_or_default()
}

fn normalize_title(value: &str) -> String {
    value
        .chars()
        .map(|c| match c.to_ascii_lowercase() {
            '\'' | '’' | '"' | ':' | ',' | '.' | '!' | '?' | '-' | '_' | '/' | '(' | ')' => ' ',
            other => other,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn nonempty(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = normalize_title(trimmed);
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        out.push(trimmed.to_string());
    }
    out
}

fn candidate_titles(candidate: &Candidate) -> Vec<String> {
    let mut vals = vec![candidate.canonical_title.clone()];
    vals.extend(candidate.titles.values().cloned());
    vals.extend(candidate.abbreviated_titles.clone());
    nonempty(vals)
}

fn parse_year(date: Option<&str>) -> Option<i32> {
    date.and_then(|s| s.get(0..4))
        .and_then(|y| y.parse::<i32>().ok())
}

fn score_candidate(
    candidate: &Candidate,
    wanted_titles: &[String],
    wanted_year: Option<i32>,
    wanted_eps: Option<i32>,
) -> i64 {
    let mut score = 0_i64;
    let cand_titles = candidate_titles(candidate)
        .into_iter()
        .map(|t| normalize_title(&t))
        .collect::<Vec<_>>();

    for wanted in wanted_titles {
        let wanted_norm = normalize_title(wanted);
        if wanted_norm.is_empty() {
            continue;
        }
        for cand in &cand_titles {
            if *cand == wanted_norm {
                score += 220;
            } else if cand.contains(&wanted_norm) || wanted_norm.contains(cand) {
                score += 120;
            }
        }
    }

    if let (Some(wy), Some(cy)) = (wanted_year, parse_year(candidate.start_date.as_deref())) {
        let delta = (wy - cy).abs();
        if delta == 0 {
            score += 40;
        } else if delta == 1 {
            score += 15;
        }
    }

    if let (Some(we), Some(ce)) = (wanted_eps, candidate.episode_count) {
        let delta = (we - ce).abs();
        if delta == 0 {
            score += 40;
        } else if delta <= 2 {
            score += 18;
        } else if delta <= 6 {
            score += 8;
        }
    }

    if candidate.subtype.eq_ignore_ascii_case("TV") {
        score += 10;
    }

    score
}

async fn fetch_collection<T: for<'de> serde::Deserialize<'de>>(
    url: &str,
    params: &[(&str, &str)],
) -> Result<CollectionResponse<T>, String> {
    HTTP_CLIENT
        .get(url)
        .query(params)
        .header("Accept", "application/vnd.api+json")
        .header("Content-Type", "application/vnd.api+json")
        .header("User-Agent", "Ryokan/0.1")
        .send()
        .await
        .map_err(|e| format!("Kitsu request failed: {}", e))?
        .error_for_status()
        .map_err(|e| format!("Kitsu request failed: {}", e))?
        .json::<CollectionResponse<T>>()
        .await
        .map_err(|e| format!("Failed to parse Kitsu response: {}", e))
}

fn to_candidate(resource: Resource<AnimeAttributes>) -> Option<Candidate> {
    let id = resource.id.parse::<i64>().ok()?;
    let attrs = resource.attributes;
    Some(Candidate {
        nsfw: attrs.nsfw.unwrap_or(false),
        id,
        canonical_title: attrs.canonical_title.unwrap_or_default(),
        titles: attrs.titles.unwrap_or_default(),
        abbreviated_titles: attrs.abbreviated_titles.unwrap_or_default(),
        synopsis: attrs.synopsis.unwrap_or_default(),
        poster_image: attrs.poster_image.unwrap_or_default(),
        cover_image: attrs.cover_image.unwrap_or_default(),
        subtype: attrs.subtype.unwrap_or_default(),
        status: attrs.status.unwrap_or_default(),
        episode_count: attrs.episode_count,
        episode_length: attrs.episode_length,
        start_date: attrs.start_date,
        end_date: attrs.end_date,
        average_rating: attrs.average_rating,
    })
}

async fn best_candidate(
    queries: &[String],
    wanted_year: Option<i32>,
    wanted_eps: Option<i32>,
) -> Result<Option<Candidate>, String> {
    let queries = nonempty(queries.to_vec());
    if queries.is_empty() {
        return Ok(None);
    }

    let mut best: Option<(Candidate, i64)> = None;
    for query in &queries {
        let response = match fetch_collection::<AnimeAttributes>(
            &format!("{}/anime", kitsu_api_base()),
            &[("filter[text]", query.as_str()), ("page[limit]", "10")],
        )
        .await
        {
            Ok(v) => v,
            Err(_) => continue,
        };

        for item in response.data.into_iter().filter_map(to_candidate) {
            let score = score_candidate(&item, &queries, wanted_year, wanted_eps);
            if best.as_ref().map(|(_, s)| score > *s).unwrap_or(true) {
                best = Some((item, score));
            }
        }
    }

    Ok(best.map(|(c, _)| c))
}

fn to_anime_detail(item: Candidate) -> AnimeDetail {
    let title_romaji = item
        .titles
        .get("en_jp")
        .cloned()
        .unwrap_or_else(|| item.canonical_title.clone());
    let title_english = item.titles.get("en").cloned().unwrap_or_default();
    let title_native = item.titles.get("ja_jp").cloned().unwrap_or_default();
    let score = item
        .average_rating
        .as_deref()
        .and_then(|v| v.parse::<f32>().ok())
        .map(|v| v.round() as i32);
    let score_class = match score {
        Some(s) if s >= 85 => "tag-score-purple",
        Some(s) if s >= 75 => "tag-score-green",
        Some(s) if s > 65 => "tag-score-yellow",
        _ => "tag-score-red",
    }
    .to_string();

    AnimeDetail {
        is_adult: item.nsfw,
        id: item.id,
        id_mal: None,
        title_romaji,
        title_english,
        title_native,
        cover_url: first_image(&item.poster_image),
        banner_url: first_image(&item.cover_image),
        format: item.subtype.to_ascii_uppercase(),
        status: item.status.to_ascii_uppercase().replace(' ', "_"),
        status_display: item.status.replace('-', " "),
        episodes: item.episode_count.filter(|&n| n > 0),
        duration: item.episode_length,
        season: String::new(),
        season_year: parse_year(item.start_date.as_deref()),
        end_year: parse_year(item.end_date.as_deref()),
        description: sanitize_rich_description(&item.synopsis, false),
        genres: Vec::new(),
        average_score: score,
        average_score_display: score.map(|s| format!("{:.2}/10", s as f32 / 10.0)),
        score_is_ten_point: false,
        score_class,
        next_airing_episode: None,
        next_airing_at: None,
        synonyms: Vec::new(),
        streaming_episodes: Vec::new(),
        relations: Vec::new(),
    }
}

pub async fn get_anime_detail_by_titles(
    titles: &[String],
    wanted_year: Option<i32>,
    wanted_eps: Option<i32>,
) -> Result<AnimeDetail, String> {
    let candidate = best_candidate(titles, wanted_year, wanted_eps)
        .await?
        .ok_or_else(|| "Kitsu returned no matching anime".to_string())?;
    Ok(to_anime_detail(candidate))
}

/// Resolve Kitsu detail by MAL id via the dedicated `/mappings`
/// endpoint. One round-trip, exact match — collapses what
/// `best_candidate` does across 1–4 fuzzy title queries when the
/// caller already has the MAL id (the AniList → Jikan path always
/// does). Returns `Ok(None)` when no Kitsu mapping exists for this
/// MAL id, so the caller can fall through to the title-fuzz path.
///
/// Endpoint shape (verified live 2026-04-19):
/// `GET /mappings?filter[externalSite]=myanimelist/anime
///       &filter[externalId]={mal_id}&include=item`
/// returns a mapping resource plus the linked anime in the JSON:API
/// `included` array — so we get both the mapping confirmation and
/// the full anime attributes in a single request.
///
/// Note: filtering on `/anime?filter[mappings.externalSite]=…` is
/// rejected by Kitsu (`Filter not allowed`); the dedicated
/// `/mappings` endpoint with a top-level `filter[externalSite]` is
/// the supported shape.
pub async fn get_anime_detail_by_mal_id(mal_id: i64) -> Result<Option<AnimeDetail>, String> {
    let mal_id_str = mal_id.to_string();
    let url = format!("{}/mappings", kitsu_api_base());
    let resp = HTTP_CLIENT
        .get(&url)
        .query(&[
            ("filter[externalSite]", "myanimelist/anime"),
            ("filter[externalId]", mal_id_str.as_str()),
            ("include", "item"),
            ("page[limit]", "1"),
        ])
        .header("Accept", "application/vnd.api+json")
        .header("Content-Type", "application/vnd.api+json")
        .header("User-Agent", "Ryokan/0.1")
        .send()
        .await
        .map_err(|e| format!("Kitsu mapping request failed: {}", e))?
        .error_for_status()
        .map_err(|e| format!("Kitsu mapping request failed: {}", e))?;

    let body: MappingLookupResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse Kitsu mapping response: {}", e))?;

    Ok(body
        .included
        .into_iter()
        .find(|r| r.item_type == "anime")
        .and_then(|r| {
            // Reuse `to_candidate` by adapting the mapping-included
            // resource into the same `Resource<AnimeAttributes>` shape
            // — `to_candidate` parses the id string and pulls fields
            // off `attributes`.
            to_candidate(Resource {
                id: r.id,
                attributes: r.attributes,
            })
        })
        .map(to_anime_detail))
}

/// JSON:API response shape for `GET /mappings?...&include=item`.
/// The mapping resources themselves live in `data` but we don't
/// actually need them — the linked anime resource is in `included`,
/// which is what we want.
#[derive(Debug, Deserialize)]
struct MappingLookupResponse {
    #[serde(default)]
    included: Vec<MappingIncludedItem>,
}

/// One item from the `included` array. `type` lets us filter to
/// anime resources only — manga mappings would land here too if
/// some future caller asked for `myanimelist/manga`, and we don't
/// want to feed manga attributes into `to_candidate`.
#[derive(Debug, Deserialize)]
struct MappingIncludedItem {
    id: String,
    #[serde(rename = "type")]
    item_type: String,
    attributes: AnimeAttributes,
}

async fn fetch_episode_page_via_relationship(
    kitsu_id: i64,
    offset: i32,
) -> Result<CollectionResponse<EpisodeAttributes>, String> {
    let offset_str = offset.to_string();
    let params = [
        ("page[limit]", "20"),
        ("page[offset]", offset_str.as_str()),
        ("sort", "number"),
    ];
    fetch_collection::<EpisodeAttributes>(
        &format!("{}/anime/{}/episodes", kitsu_api_base(), kitsu_id),
        &params,
    )
    .await
}

async fn fetch_episode_page_via_filter(
    kitsu_id: i64,
    offset: i32,
) -> Result<CollectionResponse<EpisodeAttributes>, String> {
    let kitsu_id_str = kitsu_id.to_string();
    let offset_str = offset.to_string();
    let params = [
        ("filter[mediaId]", kitsu_id_str.as_str()),
        ("page[limit]", "20"),
        ("page[offset]", offset_str.as_str()),
        ("sort", "number"),
    ];
    fetch_collection::<EpisodeAttributes>(&format!("{}/episodes", kitsu_api_base()), &params).await
}

async fn get_cached_kitsu_episodes(
    db: &SqlitePool,
    kitsu_id: i64,
) -> Result<Option<HashMap<i32, EpisodeInfo>>, sqlx::Error> {
    let rows: Vec<(i32, String, String)> = sqlx::query_as(
        r#"
        SELECT episode_number, title, aired FROM kitsu_episode_cache
        WHERE kitsu_id = ?
        AND cached_at > datetime('now', ? || ' seconds')
        "#,
    )
    .bind(kitsu_id)
    .bind(-CACHE_TTL_SECS)
    .fetch_all(db)
    .await?;

    if rows.is_empty() {
        return Ok(None);
    }

    let mut map = HashMap::new();
    let mut has_negative_sentinel = false;
    for (num, title, aired) in rows {
        if num == 0 && title == NEGATIVE_CACHE_SENTINEL {
            has_negative_sentinel = true;
            continue;
        }
        map.insert(num, EpisodeInfo { title, aired });
    }

    if has_negative_sentinel || !map.is_empty() {
        Ok(Some(map))
    } else {
        Ok(None)
    }
}

async fn cache_kitsu_episodes(
    db: &SqlitePool,
    kitsu_id: i64,
    episodes: &HashMap<i32, EpisodeInfo>,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM kitsu_episode_cache WHERE kitsu_id = ?")
        .bind(kitsu_id)
        .execute(db)
        .await?;

    if episodes.is_empty() {
        sqlx::query(
            "INSERT INTO kitsu_episode_cache (kitsu_id, episode_number, title, aired) VALUES (?, 0, ?, '')",
        )
        .bind(kitsu_id)
        .bind(NEGATIVE_CACHE_SENTINEL)
        .execute(db)
        .await?;
        return Ok(());
    }

    for (num, info) in episodes {
        sqlx::query(
            "INSERT INTO kitsu_episode_cache (kitsu_id, episode_number, title, aired) VALUES (?, ?, ?, ?)",
        )
        .bind(kitsu_id)
        .bind(num)
        .bind(&info.title)
        .bind(&info.aired)
        .execute(db)
        .await?;
    }

    Ok(())
}

pub async fn fetch_episode_titles_fallback(
    db: &SqlitePool,
    titles: &[String],
    wanted_year: Option<i32>,
    wanted_eps: Option<i32>,
) -> HashMap<i32, EpisodeInfo> {
    let candidate = match best_candidate(titles, wanted_year, wanted_eps).await {
        Ok(Some(c)) => c,
        _ => return HashMap::new(),
    };

    if let Ok(Some(cached)) = get_cached_kitsu_episodes(db, candidate.id).await {
        return cached;
    }

    let mut out = HashMap::new();
    let mut offset = 0;
    let mut pages = 0;

    loop {
        let response = match fetch_episode_page_via_relationship(candidate.id, offset).await {
            Ok(v) => Ok(v),
            Err(_) => fetch_episode_page_via_filter(candidate.id, offset).await,
        };

        let response = match response {
            Ok(v) => v,
            Err(_) => break,
        };

        let count = response.data.len();
        let has_next = response
            .links
            .as_ref()
            .and_then(|l| l.next.as_ref())
            .is_some();

        for resource in response.data {
            let attrs = resource.attributes;
            let ep_num = attrs.relative_number.or(attrs.number);
            let Some(ep_num) = ep_num else {
                continue;
            };
            let raw_title = attrs
                .canonical_title
                .or_else(|| attrs.titles.as_ref().and_then(|m| m.get("en").cloned()))
                .or_else(|| attrs.titles.as_ref().and_then(|m| m.get("en_jp").cloned()))
                .or_else(|| attrs.titles.as_ref().and_then(|m| m.get("ja_jp").cloned()))
                .unwrap_or_default();
            let title = if raw_title.trim().is_empty() {
                format!("Episode {}", ep_num)
            } else {
                raw_title
            };
            let aired = match attrs.air_date {
                Some(d) if !d.trim().is_empty() => d,
                _ => "-".to_string(),
            };
            out.insert(ep_num, EpisodeInfo { title, aired });
        }

        pages += 1;
        if count < 20 || pages >= 20 || !has_next {
            break;
        }
        offset += 20;
    }

    let _ = cache_kitsu_episodes(db, candidate.id, &out).await;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_candidate(
        canonical: &str,
        titles: &[(&str, &str)],
        start_date: Option<&str>,
        episode_count: Option<i32>,
        subtype: &str,
    ) -> Candidate {
        Candidate {
            nsfw: false,
            id: 42,
            canonical_title: canonical.to_string(),
            titles: titles
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            abbreviated_titles: vec![],
            synopsis: String::new(),
            poster_image: ImageSet::default(),
            cover_image: ImageSet::default(),
            subtype: subtype.to_string(),
            status: "finished".to_string(),
            episode_count,
            episode_length: None,
            start_date: start_date.map(String::from),
            end_date: None,
            average_rating: None,
        }
    }

    // ─── first_image ─────────────────────────────────────────────

    #[test]
    fn first_image_prefers_original_then_large_then_medium_etc() {
        let all_filled = ImageSet {
            tiny: Some("tiny.jpg".into()),
            small: Some("small.jpg".into()),
            medium: Some("medium.jpg".into()),
            large: Some("large.jpg".into()),
            original: Some("original.jpg".into()),
        };
        assert_eq!(first_image(&all_filled), "original.jpg");
    }

    #[test]
    fn first_image_falls_back_through_size_chain() {
        let no_original = ImageSet {
            tiny: Some("tiny.jpg".into()),
            small: None,
            medium: None,
            large: Some("large.jpg".into()),
            original: None,
        };
        assert_eq!(first_image(&no_original), "large.jpg");
    }

    #[test]
    fn first_image_returns_empty_when_all_sizes_missing() {
        assert_eq!(first_image(&ImageSet::default()), "");
    }

    // ─── normalize_title ─────────────────────────────────────────

    #[test]
    fn normalize_title_lowercases_and_strips_punctuation() {
        // Kitsu's title matching has to survive apostrophes, colons,
        // and smart-quote variants — all replaced with a space so
        // downstream token equality matches across punctuation
        // styles.
        assert_eq!(normalize_title("Your Name."), "your name");
        assert_eq!(
            normalize_title("A.I.C.O.: Incarnation"),
            "a i c o incarnation"
        );
    }

    #[test]
    fn normalize_title_collapses_multiple_spaces_into_one() {
        assert_eq!(normalize_title("foo   bar"), "foo bar");
    }

    #[test]
    fn normalize_title_replaces_smart_quote_apostrophe() {
        // Provider responses mix `'` and `’` (U+2019). Both must
        // normalize to the same token or otherwise-identical titles
        // won't match.
        assert_eq!(normalize_title("don't"), normalize_title("don’t"));
    }

    #[test]
    fn normalize_title_empty_input_returns_empty() {
        assert_eq!(normalize_title(""), "");
        assert_eq!(normalize_title("   "), "");
    }

    // ─── nonempty ────────────────────────────────────────────────

    #[test]
    fn nonempty_drops_empty_and_whitespace_entries() {
        let inputs = vec!["foo".into(), "  ".into(), String::new(), "bar".into()];
        assert_eq!(nonempty(inputs), vec!["foo".to_string(), "bar".to_string()]);
    }

    #[test]
    fn nonempty_dedupes_by_normalized_form() {
        // "Attack on Titan" and "Attack on Titan!" normalize to the
        // same token — the second one should drop.
        let inputs = vec!["Attack on Titan".into(), "Attack on Titan!".into()];
        let result = nonempty(inputs);
        assert_eq!(result.len(), 1);
        // First wins — the exclamation-point variant is the dup.
        assert_eq!(result[0], "Attack on Titan");
    }

    // ─── candidate_titles ────────────────────────────────────────

    #[test]
    fn candidate_titles_combines_canonical_and_localized_variants() {
        let c = test_candidate(
            "Canon",
            &[("en", "English"), ("ja_jp", "日本語")],
            None,
            None,
            "TV",
        );
        let titles = candidate_titles(&c);
        assert!(titles.contains(&"Canon".to_string()));
        assert!(titles.contains(&"English".to_string()));
        assert!(titles.contains(&"日本語".to_string()));
    }

    // ─── parse_year ──────────────────────────────────────────────

    #[test]
    fn parse_year_extracts_leading_four_digits() {
        assert_eq!(parse_year(Some("2024-03-15")), Some(2024));
    }

    #[test]
    fn parse_year_returns_none_on_missing_or_malformed() {
        assert_eq!(parse_year(None), None);
        assert_eq!(parse_year(Some("")), None);
        assert_eq!(parse_year(Some("bad-date")), None);
    }

    // ─── score_candidate ─────────────────────────────────────────

    #[test]
    fn score_candidate_awards_exact_title_match_highest() {
        // Exact-token match is worth 220 — substantially more than a
        // partial-contains match (120). If the two ever collapse to
        // similar weights, the wrong candidate wins on ambiguous
        // series titles.
        let c = test_candidate("Your Name", &[], None, None, "TV");
        let wanted = vec!["Your Name".to_string()];
        let score = score_candidate(&c, &wanted, None, None);
        assert!(
            score >= 220,
            "exact-match score should dominate: got {score}"
        );
    }

    #[test]
    fn score_candidate_awards_year_bonus_only_when_matching() {
        let c = test_candidate("Show", &[], Some("2024-01-01"), None, "TV");
        let wanted = vec!["Show".to_string()];
        let match_score = score_candidate(&c, &wanted, Some(2024), None);
        let off_by_one_score = score_candidate(&c, &wanted, Some(2023), None);
        let far_off_score = score_candidate(&c, &wanted, Some(2000), None);
        assert!(
            match_score > off_by_one_score,
            "exact year should beat off-by-one: {match_score} vs {off_by_one_score}"
        );
        assert!(
            off_by_one_score > far_off_score,
            "off-by-one year should beat far-off: {off_by_one_score} vs {far_off_score}"
        );
    }

    #[test]
    fn score_candidate_tv_subtype_gets_bonus() {
        let tv = test_candidate("Show", &[], None, None, "TV");
        let ova = test_candidate("Show", &[], None, None, "OVA");
        let wanted = vec!["Show".to_string()];
        assert!(
            score_candidate(&tv, &wanted, None, None) > score_candidate(&ova, &wanted, None, None)
        );
    }

    #[test]
    fn score_candidate_ignores_empty_wanted_title() {
        // Empty wanted entry must not blow up or score weirdly —
        // the normalized-empty guard in the impl returns an empty
        // string and the per-candidate loop skips it.
        let c = test_candidate("Show", &[], None, None, "TV");
        let wanted = vec!["".to_string(), "   ".to_string()];
        let score = score_candidate(&c, &wanted, None, None);
        // Score still picks up the TV-subtype bonus but no title match.
        assert_eq!(score, 10);
    }

    #[test]
    fn score_candidate_episode_count_delta_tiers() {
        // Bands per impl: exact=40, ±1-2=18, ±3-6=8, else=0.
        let c = test_candidate("Show", &[], None, Some(12), "TV");
        let wanted = vec!["Show".to_string()];
        let exact = score_candidate(&c, &wanted, None, Some(12));
        let near = score_candidate(&c, &wanted, None, Some(13));
        let mid = score_candidate(&c, &wanted, None, Some(18));
        let far = score_candidate(&c, &wanted, None, Some(50));
        assert!(exact > near);
        assert!(near > mid);
        assert!(mid > far);
    }

    #[test]
    fn to_anime_detail_carries_nsfw_as_is_adult() {
        let mut c = test_candidate("Kowaremono", &[], Some("2016-01-01"), Some(1), "OVA");
        assert!(!to_anime_detail(c.clone()).is_adult);
        c.nsfw = true;
        assert!(to_anime_detail(c).is_adult);
    }
}
