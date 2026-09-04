//! `TorznabIndexer` — HTTP client + [`Indexer`] trait impl.
//!
//! Build a `TorznabIndexer` from a [`crate::models::indexers::Indexer`]
//! row via [`TorznabIndexer::from_row`], then drop it into the
//! search pipeline as `Arc<dyn Indexer>`. The client owns its
//! reqwest::Client (built per indexer so per-row timeout knobs
//! apply) but shares no state with other instances — concurrent
//! fan-out is safe.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;

use super::super::{
    DEFAULT_REQUEST_TIMEOUT_SECS, Indexer, IndexerCaps, Release, SearchQuery, TORZNAB_CAT_ANIME,
};
use super::parser::{TorznabError, parse_caps_response, parse_search_response};
use crate::models::indexers as model;

/// Concrete [`Indexer`] for any torznab/newznab endpoint
/// (Prowlarr, Jackett, raw indexer). Wire format is identical
/// across providers; per-row config (URL, apikey, timeout,
/// min_seeders) drives the HTTP shape.
pub struct TorznabIndexer {
    id: i64,
    name: String,
    /// Multi-RSS — protocol kind of the source row
    /// ("torznab" / "newznab"). Surfaced via the trait's `kind()`
    /// method so the RSS fan-out can stamp the right
    /// `RssSource::Indexer { kind }` on each item for the
    /// download-client protocol guard at grab time.
    kind: String,
    /// Opaque base URL — the user pastes Prowlarr's "Copy
    /// Torznab Url" verbatim, ending in `/api`. Ryokan
    /// appends `?t=...&apikey=...&...` and never tries to
    /// parse or reconstruct the prefix.
    base_url: String,
    api_key: String,
    priority: i32,
    is_private_tracker: bool,
    /// Pre-grab seeder filter. Releases below this threshold
    /// are dropped before the search pipeline scores them.
    /// `0` disables the filter.
    min_seeders: i32,
    /// Multi-client routing pin — id of the row in
    /// `download_clients` this indexer is bound to. `None` =
    /// fall through to the default client at grab time.
    download_client_id: Option<i64>,
    /// Category ids from the cached caps probe, empty until probed.
    known_categories: Vec<i32>,
    /// The user's per-indexer override; sent as written when set.
    configured_categories: Vec<i32>,
    http: Client,
}

impl TorznabIndexer {
    /// Build a [`TorznabIndexer`] from a DB row. Fails if the URL
    /// is empty or the reqwest client can't build (effectively
    /// never under normal config — TLS provider missing would be
    /// the only realistic case).
    pub fn from_row(row: &model::Indexer) -> Result<Self, String> {
        if row.url.trim().is_empty() {
            return Err(format!("indexer #{} has empty URL", row.id));
        }
        let timeout_secs = row
            .request_timeout_secs
            .map(|n| n as u64)
            .or_else(default_timeout_from_env)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS);
        let http = Client::builder()
            .user_agent("Ryokan/0.1")
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| format!("reqwest client build failed: {e}"))?;
        Ok(Self {
            id: row.id,
            name: row.name.clone(),
            kind: row.kind.clone(),
            base_url: row.url.trim_end_matches('/').to_string(),
            api_key: row.api_key.trim().to_string(),
            priority: row.priority,
            is_private_tracker: row.is_private_tracker,
            min_seeders: row.min_seeders,
            download_client_id: row.download_client_id,
            known_categories: super::super::known_category_ids(&row.caps_json),
            configured_categories: row.category_list(),
            http,
        })
    }

    /// Wrap the row build into an `Arc<dyn Indexer>` for the
    /// fan-out path, which stores indexers as trait objects.
    pub fn from_row_arc(row: &model::Indexer) -> Result<Arc<dyn Indexer>, String> {
        Self::from_row(row).map(|i| Arc::new(i) as Arc<dyn Indexer>)
    }

    fn build_url(&self, function: &str, query: &[(&str, String)]) -> String {
        // Caller passes `function` like `"caps"` or `"tvsearch"`.
        // Prowlarr/Jackett both accept `?t=<function>` after the
        // base URL; the apikey rides in the same querystring.
        let mut url = format!("{}?t={}", self.base_url, urlencoding::encode(function));
        if !self.api_key.is_empty() {
            url.push_str(&format!("&apikey={}", urlencoding::encode(&self.api_key)));
        }
        for (key, value) in query {
            url.push('&');
            url.push_str(&urlencoding::encode(key));
            url.push('=');
            url.push_str(&urlencoding::encode(value));
        }
        url
    }

    /// Issue a GET, return the body string. Wraps the protocol-
    /// level error handling described in the module doc comment:
    /// HTTP 200 with `<error/>` body, non-200 (Prowlarr 401), 429
    /// plus Retry-After. Doesn't try to parse — that's the caller's
    /// job since caps and search have different schemas.
    ///
    /// Short-circuits at the top with the active per-id cooldown
    /// from `super::super::cooldown` if a prior 429 stamped one;
    /// the cooldown's lifecycle is documented there.
    async fn fetch(&self, url: &str) -> Result<String, String> {
        if let Some(remaining) = super::super::cooldown::remaining(self.id) {
            return Err(format!(
                "Indexer rate-limited (cooldown {}s remaining)",
                remaining.as_secs()
            ));
        }
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| format!("indexer request failed: {e}"))?;
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // Surface the upstream Retry-After so the caller can
            // apply a cooldown. Mirroring AniList's pattern, the
            // header value is in seconds; 0 / missing → fall back
            // to a default cooldown.
            let retry = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            super::super::cooldown::record_429(self.id, retry);
            let body_excerpt = resp.text().await.unwrap_or_default();
            return Err(format!(
                "Indexer rate-limited (429); retry_after={:?}s; body: {}",
                retry,
                truncate_body(&body_excerpt)
            ));
        }
        if !status.is_success() {
            // Prowlarr returns 401 on bad apikey BEFORE the
            // torznab layer sees the request. Surface the status
            // so the caller's error message tells the operator
            // whether the indexer URL is reachable at all.
            let body = crate::services::rss::feed::read_capped_body(resp)
                .await
                .unwrap_or_default();
            return Err(format!(
                "Indexer returned HTTP {}: {}",
                status,
                truncate_body(&body)
            ));
        }
        // PR 112 review #A — share the same 10 MB body cap with
        // the RSS path. Prowlarr is local-trust but a misconfigured
        // upstream indexer behind it can still return the full
        // historical archive on `t=tvsearch&q=`; the cap turns OOM
        // into a clean Err the caller logs + skips.
        crate::services::rss::feed::read_capped_body(resp).await
    }

    /// Apply pre-score filtering to a release set. Currently:
    /// drop releases below the indexer's configured min_seeders.
    /// Other filters (size cap, date floor) live in the search
    /// pipeline since they're not per-indexer concerns.
    fn apply_min_seeders(&self, releases: Vec<Release>) -> Vec<Release> {
        if self.min_seeders <= 0 {
            return releases;
        }
        releases
            .into_iter()
            .filter(|r| r.seeders >= self.min_seeders)
            .collect()
    }
}

#[async_trait::async_trait]
impl Indexer for TorznabIndexer {
    fn id(&self) -> i64 {
        self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> &str {
        &self.kind
    }
    fn priority(&self) -> i32 {
        self.priority
    }
    fn is_private_tracker(&self) -> bool {
        self.is_private_tracker
    }
    fn download_client_id(&self) -> Option<i64> {
        self.download_client_id
    }

    async fn caps(&self) -> Result<IndexerCaps, String> {
        let url = self.build_url("caps", &[]);
        let body = self.fetch(&url).await?;
        parse_caps_response(&body)
    }

    async fn search(&self, query: &SearchQuery) -> Result<Vec<Release>, String> {
        // Build the parameter set. For anime searches we always
        // use `t=tvsearch` with `cat=5070` per protocol research:
        // anime trackers key on absolute episode numbers in the
        // release title, so `season`/`ep` params don't translate.
        let mut params: Vec<(&str, String)> = Vec::new();
        if !query.q.is_empty() {
            params.push(("q", query.q.clone()));
        }
        let requested = if query.categories.is_empty() {
            vec![TORZNAB_CAT_ANIME]
        } else {
            query.categories.clone()
        };
        let cats = if self.configured_categories.is_empty() {
            super::super::resolve_request_categories(&requested, &self.known_categories)
        } else {
            self.configured_categories.clone()
        };
        if cats != requested {
            tracing::debug!(
                "indexer {}: asking for categories {:?} instead of {:?} (caps)",
                self.name,
                cats,
                requested
            );
        }
        let cat_csv = cats
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        params.push(("cat", cat_csv));
        if let Some(limit) = query.limit {
            params.push(("limit", limit.to_string()));
        }
        if let Some(offset) = query.offset {
            params.push(("offset", offset.to_string()));
        }

        let url = self.build_url("tvsearch", &params);
        let body = self.fetch(&url).await?;
        let parsed = parse_search_response(&body, self.id, self.priority, &self.name)?;
        let releases = match parsed {
            Ok(rs) => rs,
            Err(e) => return Err(format_torznab_error(&e)),
        };
        Ok(self.apply_min_seeders(releases))
    }
}

/// Format a [`TorznabError`] into a human-readable string.
/// Codes per protocol:
/// - 100/101 = bad credentials
/// - 102 = insufficient privileges
/// - 200 = missing required parameter
/// - 201 = incorrect parameter
/// - 202/203 = unsupported function
/// - 300 = no such item
/// - 900 = unknown failure
/// - 910 = API disabled
fn format_torznab_error(err: &TorznabError) -> String {
    let category = match err.code {
        100 | 101 => "credentials",
        102 => "permissions",
        200 | 201 => "parameter",
        202 | 203 => "function",
        300 => "missing",
        900 => "server",
        910 => "disabled",
        _ => "other",
    };
    format!(
        "Indexer error code {} ({}): {}",
        err.code, category, err.description
    )
}

fn truncate_body(s: &str) -> String {
    const MAX_CHARS: usize = 240;
    let mut iter = s.chars();
    let prefix: String = iter.by_ref().take(MAX_CHARS).collect();
    if iter.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn default_timeout_from_env() -> Option<u64> {
    // PR #107 review fix #11: clamp the env-var override into the
    // same [1, 600] range the form-side parser enforces. Without
    // this, a misconfigured deployment with `RYOKAN_INDEXER_DEFAULT_
    // TIMEOUT_SECS=0` would force every search to time out
    // immediately; a `=99999` value would block the search loop
    // for hours per indexer.
    std::env::var("RYOKAN_INDEXER_DEFAULT_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| (1..=600).contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> model::Indexer {
        model::Indexer {
            id: 7,
            name: "Test".to_string(),
            kind: model::KIND_TORZNAB.to_string(),
            url: "https://prowlarr.local/1/api".to_string(),
            api_key: "secret".to_string(),
            priority: 25,
            enabled: true,
            is_private_tracker: false,
            seed_ratio: None,
            seed_time_minutes: None,
            min_seeders: 1,
            request_timeout_secs: None,
            download_client_id: None,
            rss_enabled: false,
            rss_last_polled_at: None,
            rss_last_poll_error: String::new(),
            rss_last_item_count: 0,
            categories: String::new(),
            caps_json: String::new(),
            caps_refreshed_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn from_row_rejects_empty_url() {
        let mut row = sample_row();
        row.url = String::new();
        assert!(TorznabIndexer::from_row(&row).is_err());
    }

    #[test]
    fn from_row_trims_trailing_slash_on_base_url() {
        let mut row = sample_row();
        row.url = "https://prowlarr.local/1/api/".to_string();
        let idx = TorznabIndexer::from_row(&row).expect("must build");
        assert_eq!(idx.base_url, "https://prowlarr.local/1/api");
    }

    #[test]
    fn build_url_appends_apikey_and_function() {
        let idx = TorznabIndexer::from_row(&sample_row()).unwrap();
        let url = idx.build_url("caps", &[]);
        assert!(
            url.starts_with("https://prowlarr.local/1/api?t=caps"),
            "wrong shape: {url}"
        );
        assert!(url.contains("&apikey=secret"));
    }

    #[test]
    fn build_url_omits_apikey_when_empty() {
        // Some indexers don't require an api key (rare but the
        // protocol allows it). Don't append `&apikey=` when the
        // row's value is empty — Prowlarr would 401 a request
        // with `apikey=`.
        let mut row = sample_row();
        row.api_key = String::new();
        let idx = TorznabIndexer::from_row(&row).unwrap();
        let url = idx.build_url("caps", &[]);
        assert!(!url.contains("apikey="), "must omit empty apikey: {url}");
    }

    #[test]
    fn build_url_url_encodes_query_values() {
        let idx = TorznabIndexer::from_row(&sample_row()).unwrap();
        let url = idx.build_url(
            "tvsearch",
            &[("q", "Show with spaces & ampersand".to_string())],
        );
        // Spaces → `%20`, `&` → `%26`.
        assert!(
            url.contains("Show%20with%20spaces%20%26%20ampersand"),
            "url: {url}"
        );
    }

    #[test]
    fn build_url_passes_multiple_query_params_in_order() {
        let idx = TorznabIndexer::from_row(&sample_row()).unwrap();
        let url = idx.build_url(
            "tvsearch",
            &[
                ("q", "test".to_string()),
                ("cat", "5070".to_string()),
                ("limit", "10".to_string()),
            ],
        );
        // Verify each param is present without committing to a
        // specific separator order beyond the function/apikey
        // prefix.
        assert!(url.contains("&q=test"));
        assert!(url.contains("&cat=5070"));
        assert!(url.contains("&limit=10"));
    }

    #[test]
    fn apply_min_seeders_filters_below_threshold() {
        let mut row = sample_row();
        row.min_seeders = 5;
        let idx = TorznabIndexer::from_row(&row).unwrap();
        let releases = vec![
            sample_release(10),
            sample_release(4),
            sample_release(5), // exactly at the floor — kept
            sample_release(0),
        ];
        let filtered = idx.apply_min_seeders(releases);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|r| r.seeders >= 5));
    }

    #[test]
    fn apply_min_seeders_zero_disables_filter() {
        let mut row = sample_row();
        row.min_seeders = 0;
        let idx = TorznabIndexer::from_row(&row).unwrap();
        let releases = vec![sample_release(0), sample_release(5)];
        let kept = idx.apply_min_seeders(releases);
        assert_eq!(kept.len(), 2, "min_seeders=0 keeps everything");
    }

    fn sample_release(seeders: i32) -> Release {
        Release {
            indexer_id: 7,
            indexer_priority: 25,
            indexer_name: "TestIndexer".to_string(),
            title: "Show".to_string(),
            guid: "g".to_string(),
            link: String::new(),
            magnet: String::new(),
            publish_date: 0,
            size_bytes: 0,
            seeders,
            leechers: 0,
            info_hash: String::new(),
            categories: Vec::new(),
            download_volume_factor: None,
            upload_volume_factor: None,
            extra: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn format_torznab_error_categorizes_codes() {
        assert!(
            format_torznab_error(&TorznabError {
                code: 100,
                description: "bad key".to_string()
            })
            .contains("credentials")
        );
        assert!(
            format_torznab_error(&TorznabError {
                code: 200,
                description: "missing q".to_string()
            })
            .contains("parameter")
        );
        assert!(
            format_torznab_error(&TorznabError {
                code: 999,
                description: "unknown".to_string()
            })
            .contains("other")
        );
        // PR #107 review fix #15: 910 = API disabled.
        assert!(
            format_torznab_error(&TorznabError {
                code: 910,
                description: "API disabled".to_string()
            })
            .contains("disabled")
        );
        assert!(
            format_torznab_error(&TorznabError {
                code: 102,
                description: "permissions".to_string()
            })
            .contains("permissions")
        );
        assert!(
            format_torznab_error(&TorznabError {
                code: 300,
                description: "no such item".to_string()
            })
            .contains("missing")
        );
    }
}
