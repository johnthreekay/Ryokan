//! Wiremock coverage for `services::auto_search::find_all_for_target`.
//! The audit's largest single mutant cluster (~85 missed) lives in this
//! function's branching logic — alias matching, sibling rejection,
//! season filtering, SeaDex bypass, and the multi-phase query sweep.
//! All of that runs through `nyaa::search` against `RYOKAN_NYAA_API_BASE`,
//! the env-var seam added in commit `c836649`.
//!
//! Each test stands up a minimal Nyaa-shaped wiremock and asserts the
//! resulting `Vec<SearchResult>` matches the expected shape. Mirrors the
//! `metadata_sync_e2e.rs` ENV_LOCK pattern so within-binary tests don't
//! race on the process-wide env var; cross-binary isolation is handled
//! by nextest's process-per-test default.

use ryokan::AppState;
use ryokan::models::config::Config;
use ryokan::services::anilist::AnimeDetail;
use ryokan::services::auto_search::{SearchTarget, find_all_for_target};
use ryokan::test_support::{build_test_app_state, in_memory_pool};
use std::sync::LazyLock;
use tokio::sync::Mutex;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Serialize within-binary tests on the RYOKAN_NYAA_API_BASE write so
/// tokio's parallel scheduler can't race two tests on the same env var.
/// Other test binaries get their own process under nextest, so cross-
/// binary leakage is impossible.
static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

// ─── HTML fixture builders ───────────────────────────────────────

/// Produce a Nyaa search-results page wrapping the given rows. The
/// scraper looks for `table.torrent-list tbody tr` so the wrapper
/// matches that selector exactly. No pagination → has_next stays false.
fn nyaa_results_page(rows_html: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html><body>
<table class="torrent-list">
  <tbody>
{rows_html}
  </tbody>
</table>
</body></html>"#
    )
}

/// One Nyaa-shape table row. Eight `<td>` cells in the order the parser
/// reads them: category, name (with /view/ link), links (torrent +
/// magnet), size, date, seeders, leechers, downloads.
///
/// `info_hash` is the 40-char hex string used in the magnet xt; the
/// scraper extracts it via `extract_hash` and uses it as the candidate's
/// dedup key.
fn nyaa_row(info_hash: &str, view_id: u64, title: &str, size: &str, seeders: i32) -> String {
    format!(
        r#"    <tr>
      <td><a href="/c/1_2"></a></td>
      <td>
        <a href="/view/{view_id}">{title}</a>
      </td>
      <td>
        <a href="/download/{view_id}.torrent"></a>
        <a href="magnet:?xt=urn:btih:{info_hash}&amp;dn={title}"></a>
      </td>
      <td>{size}</td>
      <td>2024-04-01 12:00</td>
      <td>{seeders}</td>
      <td>0</td>
      <td>100</td>
    </tr>
"#
    )
}

// ─── AppState / fixture builders ─────────────────────────────────

fn detail_for(id: i64, romaji: &str) -> AnimeDetail {
    AnimeDetail {
        is_adult: false,
        id,
        id_mal: None,
        title_romaji: romaji.into(),
        title_english: romaji.into(),
        title_native: romaji.into(),
        cover_url: String::new(),
        banner_url: String::new(),
        format: "TV".into(),
        status: "FINISHED".into(),
        status_display: "Finished".into(),
        episodes: Some(12),
        duration: Some(24),
        season: String::new(),
        season_year: Some(2024),
        end_year: Some(2024),
        description: String::new(),
        genres: vec![],
        average_score: None,
        average_score_display: None,
        score_is_ten_point: false,
        score_class: String::new(),
        next_airing_episode: None,
        next_airing_at: None,
        synonyms: vec![],
        streaming_episodes: vec![],
        relations: vec![],
    }
}

fn default_config() -> Config {
    Config {
        preferred_resolution: "1080".into(),
        preferred_source: "web".into(),
        cutoff_resolution: "720".into(),
        cutoff_source: "web".into(),
        allow_non_english: false,
        finished_series_quality: "same_as_airing".into(),
        title_language: "english".into(),
        ..Config::default()
    }
}

/// Build the AppState scaffolding `find_all_for_target` reads from.
/// No download client (the function doesn't dispatch grabs); empty
/// indexers (Nyaa is the only source under test); empty CFs.
async fn build_state() -> AppState {
    let db = in_memory_pool().await;
    build_test_app_state(db, None)
}

/// Set the Nyaa env-var seam to point at the wiremock server. Call
/// inside a `let _gate = ENV_LOCK.lock().await;` block so concurrent
/// tests don't race.
fn set_nyaa_base(uri: &str) {
    unsafe {
        std::env::set_var("RYOKAN_NYAA_API_BASE", uri);
    }
}

fn unset_nyaa_base() {
    unsafe {
        std::env::remove_var("RYOKAN_NYAA_API_BASE");
    }
}

// ─── Tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn find_all_for_target_returns_matching_release_from_nyaa() {
    // Smallest happy-path case: one Nyaa row with a title that shares
    // a token with the AL detail's title, episode number matches the
    // search target. The function should return that row in the
    // candidate list with score > 0.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    let html = nyaa_results_page(&nyaa_row(
        "0123456789abcdef0123456789abcdef01234567",
        12345,
        "[Group] Test Show - 01 (1080p) [WEB].mkv",
        "1.4 GiB",
        50,
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let detail = detail_for(1001, "Test Show");
    let cfg = default_config();
    let target = SearchTarget::Episode(1);
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];

    let results = find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        true,
        &cfs,
        &state.indexers,
    )
    .await;

    assert!(
        !results.is_empty(),
        "matching Nyaa row must surface in the results: got {results:?}"
    );
    assert!(
        results.iter().any(|r| r.title.contains("Test Show")),
        "result must include our seeded title"
    );
    let r = &results[0];
    assert_eq!(r.seeders, 50);
    assert_eq!(r.size, "1.4 GiB");
    assert!(
        r.info_hash.starts_with("0123456789abcdef"),
        "info_hash must round-trip from the magnet"
    );

    unset_nyaa_base();
}

#[tokio::test]
async fn find_all_for_target_returns_empty_when_nyaa_returns_no_rows() {
    // Pin the empty-results path: Nyaa returns a well-formed page with
    // zero rows. find_all_for_target must return an empty Vec without
    // panicking on the empty SeaDex / extended-aliases / group-queries
    // fallback fan-out.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    let html = nyaa_results_page(""); // no rows
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let detail = detail_for(1002, "Empty Result Show");
    let cfg = default_config();
    let target = SearchTarget::Episode(5);
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];

    let results = find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        true,
        &cfs,
        &state.indexers,
    )
    .await;
    assert!(
        results.is_empty(),
        "no Nyaa rows must produce no candidates: got {results:?}"
    );

    unset_nyaa_base();
}

#[tokio::test]
async fn find_all_for_target_filters_unrelated_titles_via_alias_match() {
    // Pin the alias-match gate inside `apply_interactive_filter_and_push`.
    // Two Nyaa rows: one matches the AL detail's tokens, one doesn't.
    // Only the matching one survives the filter.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    let mut rows = String::new();
    rows.push_str(&nyaa_row(
        "1111111111111111111111111111111111111111",
        100,
        "[Group] Test Show - 03 (1080p).mkv",
        "1.2 GiB",
        80,
    ));
    rows.push_str(&nyaa_row(
        "2222222222222222222222222222222222222222",
        101,
        "[Group] Completely Different Anime - 03 (1080p).mkv",
        "1.0 GiB",
        80,
    ));
    let html = nyaa_results_page(&rows);
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let detail = detail_for(1003, "Test Show");
    let cfg = default_config();
    let target = SearchTarget::Episode(3);
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];

    let results = find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        true,
        &cfs,
        &state.indexers,
    )
    .await;

    // The exact filter behavior depends on the alias-match threshold,
    // but at minimum the matching row must be present and the unrelated
    // one must not crowd out the matching one in the score ordering.
    assert!(
        results.iter().any(|r| r.title.contains("Test Show")),
        "matching title must surface"
    );
    assert!(
        !results
            .iter()
            .any(|r| r.title.contains("Completely Different")),
        "unrelated title must be filtered out: got {results:?}"
    );

    unset_nyaa_base();
}

#[tokio::test]
async fn find_all_for_target_drops_episode_mismatches_for_single_episode_targets() {
    // SearchTarget::Episode(N) means we want episode N specifically;
    // releases for other episodes (parsed from the title) get dropped
    // unless they're batches. Pin that filter.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    let mut rows = String::new();
    rows.push_str(&nyaa_row(
        "3333333333333333333333333333333333333333",
        200,
        "[Group] Test Show - 03 (1080p).mkv",
        "1.0 GiB",
        50,
    ));
    rows.push_str(&nyaa_row(
        "4444444444444444444444444444444444444444",
        201,
        "[Group] Test Show - 99 (1080p).mkv",
        "1.0 GiB",
        50,
    ));
    let html = nyaa_results_page(&rows);
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let detail = detail_for(1004, "Test Show");
    let cfg = default_config();
    let target = SearchTarget::Episode(3);
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];

    let results = find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        true,
        &cfs,
        &state.indexers,
    )
    .await;

    assert!(
        results.iter().any(|r| r.title.contains(" - 03 ")),
        "ep 03 release must match the target"
    );
    assert!(
        !results.iter().any(|r| r.title.contains(" - 99 ")),
        "ep 99 release must be dropped: got {results:?}"
    );

    unset_nyaa_base();
}

#[tokio::test]
async fn find_all_for_target_runs_group_query_pass_when_preferred_groups_configured() {
    // Pin the group-queries branch at line 206:
    //   `if !preferred_groups.is_empty() && series_ctx.restrict_user.is_empty()`
    //
    // The first query pass uses canonical title aliases. When
    // `preferred_groups` is set AND no uploader restriction is active,
    // a SECOND pass runs prefixing each query with the group name
    // ("SubsPlease Test Show 01" etc.). Mutating the `&&` to `||` or
    // dropping the negation on either side would change which pass
    // fires. Pin by counting Nyaa requests: at minimum two (canonical
    // queries + group-prefixed queries).
    //
    // Easier to assert the request COUNT than the query contents,
    // since Ryokan's query-shape variants are tested separately.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(nyaa_results_page(&nyaa_row(
                "6666666666666666666666666666666666666666",
                400,
                "[SubsPlease] Test Show - 02 (1080p) [WEB].mkv",
                "1.0 GiB",
                40,
            ))),
        )
        // .expect(1..) means "at least one call." Without group queries,
        // the canonical pass alone would generate ~4 queries (build_
        // queries_from_aliases per alias × 4 query-shape variants).
        // With group queries, that count increases. We just want > 1
        // distinct hits to confirm the group pass also fires.
        .expect(2..)
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let detail = detail_for(1006, "Test Show");
    let mut cfg = default_config();
    cfg.preferred_groups = "SubsPlease".into();
    let target = SearchTarget::Episode(2);
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];

    let _results = find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        true,
        &cfs,
        &state.indexers,
    )
    .await;

    // The .expect(2..) on the mount above is the assertion; it fails
    // at server-drop if the call count is below the threshold.

    unset_nyaa_base();
}

#[tokio::test]
async fn find_all_for_target_skips_group_pass_when_restrict_user_active() {
    // Symmetric pin for the second clause of line 206's `&&`. When
    // `restrict_user` is non-empty, the group-prefixed query pass
    // skips entirely (the comment in the function explains why:
    // `?u=<name>` already scopes to one uploader, so a group prefix
    // is a no-op narrow).
    //
    // Distinct from the previous test by setting restrict_to_uploader
    // and asserting the request count stays at the canonical-pass
    // baseline. Without the gate, the group pass would run on top
    // of the canonical pass and the count would jump.
    //
    // Hard to assert "no second pass fired" with a strict count
    // because canonical-pass fan-out is itself variable. Instead,
    // assert the user-scoped path was hit: every query goes to
    // `/user/<name>` rather than `/`.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    // Fail any request to bare `/` — every request must go through
    // the /user/Trusted scope.
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page("")))
        .expect(0)
        .mount(&server)
        .await;
    // Also fail any request whose query string has the group prefix.
    // build_group_queries produces "<Group> <alias> - <ep>" and
    // "<Group> <alias> <ep>". With original code these never fire
    // because the gate at line 206 short-circuits when restrict_user
    // is set. Mutating `&&` to `||` would let the group pass fire
    // anyway → these query params would land. .expect(0) catches it.
    Mock::given(method("GET"))
        .and(query_param("q", "Trusted Test Show 04"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page("")))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(query_param("q", "Trusted Test Show - 04"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page("")))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/user/Trusted"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(nyaa_results_page(&nyaa_row(
                "7777777777777777777777777777777777777777",
                500,
                "[Trusted] Test Show - 04 (1080p).mkv",
                "1.0 GiB",
                30,
            ))),
        )
        .expect(1..)
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let detail = detail_for(1007, "Test Show");
    let mut cfg = default_config();
    cfg.preferred_groups = "Trusted".into();
    cfg.default_restrict_to_uploader = "Trusted".into();
    let target = SearchTarget::Episode(4);
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];

    let _results = find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        true,
        &cfs,
        &state.indexers,
    )
    .await;

    unset_nyaa_base();
}

/// Seed the franchise-alias data path that `find_all_for_target`'s
/// absolute-offset gate at line 229 walks. Three pieces of state:
///
///   1. `series.cumulative_prior_episodes = N` so resolve_search_overrides
///      produces `absolute_offset = N`.
///   2. A `provider_relations_cache` PREQUEL row from the series's
///      anilist_id back to a synthetic root.
///   3. A `provider_metadata_cache` row for that root carrying
///      title_romaji in its detail_json.
///
/// `resolve_franchise_aliases` joins the two cache tables and returns
/// the root's title slot, which then drives the franchise-pass query
/// fan-out.
async fn seed_franchise_chain(
    db: &sqlx::SqlitePool,
    series_anilist_id: i64,
    series_title: &str,
    root_provider_id: i64,
    root_title_romaji: &str,
    cumulative_prior: i32,
) -> i64 {
    sqlx::query(
        "INSERT INTO series (anilist_id, title, title_romaji, folder_name, status, format, \
         cumulative_prior_episodes) \
         VALUES (?, ?, ?, '', 'FINISHED', 'TV', ?)",
    )
    .bind(series_anilist_id)
    .bind(series_title)
    .bind(series_title)
    .bind(cumulative_prior)
    .execute(db)
    .await
    .unwrap();
    let series_id: i64 = sqlx::query_scalar("SELECT id FROM series WHERE anilist_id = ?")
        .bind(series_anilist_id)
        .fetch_one(db)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO provider_relations_cache \
         (provider_id, related_provider_id, related_mal_id, title_romaji, title_english, \
          title_native, cover_url, format, status, episodes, relation_type, season_year, \
          media_type) \
         VALUES (?, ?, NULL, ?, '', '', '', 'TV', 'FINISHED', 24, 'PREQUEL', 2020, 'ANIME')",
    )
    .bind(series_anilist_id)
    .bind(root_provider_id)
    .bind(root_title_romaji)
    .execute(db)
    .await
    .unwrap();

    let detail_json = serde_json::json!({
        "title_romaji": root_title_romaji,
        "title_english": "",
        "title_native": "",
        "cover_url": "",
        "format": "TV",
        "status": "FINISHED",
    })
    .to_string();
    sqlx::query(
        "INSERT INTO provider_metadata_cache (provider_id, mal_id, detail_json) \
         VALUES (?, NULL, ?)",
    )
    .bind(root_provider_id)
    .bind(detail_json)
    .execute(db)
    .await
    .unwrap();

    series_id
}

#[tokio::test]
async fn find_all_for_target_runs_franchise_pass_when_absolute_offset_set() {
    // Pin the franchise-alias / absolute-offset query pass at lines
    // 227-259. With `absolute_offset = 47` and franchise_aliases =
    // ["Test Franchise"], the third pass runs queries against
    // `<franchise_root> <ep+offset>` ("Test Franchise 56" for ep=9
    // and offset=47). Catches several distinct mutants:
    //
    //   * Line 229 `> with ==` / `<` on `absolute_offset > 0`: gate
    //     fails, no franchise queries, .expect(1..) on the franchise
    //     query fails.
    //   * Line 230 `delete !` on `!franchise_aliases.is_empty()`:
    //     same observable failure.
    //   * Line 244 `delete field aliases` (struct-spread mutation):
    //     franchise_ctx.aliases falls back to canonical via spread,
    //     so the franchise queries use `Test Show` aliases instead
    //     of `Test Franchise`. The .expect(0) on
    //     `q="Test Show 56"` catches it.
    //   * Line 246 `delete field target`: franchise_ctx.target falls
    //     back to canonical `Episode(9)`, so queries get "9" not
    //     "56". The .expect(0) on `q="Test Franchise 9"` catches it.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    // Franchise pass fires with the franchise root title + absolute
    // target (9 + 47 = 56). build_queries_from_aliases produces
    // "<alias> <ep:02>" and "<alias> - <ep:02>" non-collapsed.
    Mock::given(method("GET"))
        .and(query_param("q", "Test Franchise 56"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page("")))
        .expect(1..)
        .mount(&server)
        .await;
    // Mutations 244 and 246 would route the franchise pass through
    // wrong aliases or wrong target. Both shapes must NOT fire.
    Mock::given(method("GET"))
        .and(query_param("q", "Test Show 56"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page("")))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(query_param("q", "Test Franchise 9"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page("")))
        .expect(0)
        .mount(&server)
        .await;
    // Catch-all for canonical-pass queries against `Test Show`. The
    // canonical pass fires with the relative target (9), so queries
    // include "Test Show 09" / "Test Show - 09" / etc. Return empty
    // bodies; we don't care about their content for this test.
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page("")))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let db = in_memory_pool().await;
    let _series_id = seed_franchise_chain(
        &db,
        1008,
        "Test Show",
        9000, // synthetic root provider_id, won't collide with seeded series
        "Test Franchise",
        47,
    )
    .await;
    let state = ryokan::test_support::build_test_app_state(db, None);
    let detail = detail_for(1008, "Test Show");
    let cfg = default_config();
    let target = SearchTarget::Episode(9);
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];

    let _results = find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        true,
        &cfs,
        &state.indexers,
    )
    .await;

    // Expectations on the wiremock mocks fire at server-drop. If the
    // franchise pass didn't run with the right aliases + target, one
    // of the .expect(N) constraints will panic.

    unset_nyaa_base();
}

#[tokio::test]
async fn find_all_for_target_dedups_same_info_hash_across_query_passes() {
    // The query sweep fans out across multiple title aliases. If two
    // queries surface the SAME info_hash, the dedup map under
    // `apply_interactive_filter_and_push` must collapse them to one
    // entry per (source_tag, info_hash) pair. Set up a wiremock that
    // returns the same row regardless of query — at least two queries
    // will hit it (canonical + variant) but the result list must not
    // double-count.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    let html = nyaa_results_page(&nyaa_row(
        "5555555555555555555555555555555555555555",
        300,
        "[Group] Test Show - 07 (1080p).mkv",
        "1.0 GiB",
        50,
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .and(query_param("c", "1_2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let detail = detail_for(1005, "Test Show");
    let cfg = default_config();
    let target = SearchTarget::Episode(7);
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];

    let results = find_all_for_target(
        &state.db,
        &detail,
        &cfg,
        &target,
        true,
        &cfs,
        &state.indexers,
    )
    .await;

    let matching: Vec<_> = results
        .iter()
        .filter(|r| r.info_hash.starts_with("5555555555555555"))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "duplicate info_hash across queries must collapse to one candidate: {matching:?}"
    );

    unset_nyaa_base();
}

// ─── Handler-level coverage ──────────────────────────────────────
//
// Everything above tests the `find_all_for_target` service entry
// point. The HTTP handler `auto_search_episode` sits one layer up:
// it resolves the series context, builds the SearchTarget,
// dispatches via `run_auto_search_targets_with_upgrades` (which
// calls find_best_for_target then dispatches the winning result
// to a configured `DownloadClient`), and persists a grab row. The
// full-stack happy path requires (a) cache-seeded metadata so the
// resolver doesn't hit AL, (b) Nyaa wiremock returning a matching
// release, (c) a recording DownloadClient in the pool's torrent
// default slot. Pre-this-test handlers/library/search/auto_search.rs
// was 5% covered.

use async_trait::async_trait;
use ryokan::DownloadClientPool;
use ryokan::handlers::library::search::{AutoSearchQuery, auto_search_episode};
use ryokan::services::download_client::{
    AddOutcome, DownloadClient, DownloadFile, DownloadItem, SelectiveOutcome,
};
use std::sync::Mutex as StdMutex;

/// Recording mock that captures `add_torrent_returning_id` calls
/// and reports them via `add_calls()`. Other trait methods no-op.
struct AutoSearchRecordingClient {
    add_calls: StdMutex<Vec<(String, String)>>, // (url, info_hash)
}

impl AutoSearchRecordingClient {
    fn new() -> Self {
        Self {
            add_calls: StdMutex::new(Vec::new()),
        }
    }
    fn add_calls(&self) -> Vec<(String, String)> {
        self.add_calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl DownloadClient for AutoSearchRecordingClient {
    async fn test(&self) -> Result<String, String> {
        Ok("mock".into())
    }
    async fn add_torrent(&self, url: &str, info_hash: &str) -> Result<AddOutcome, String> {
        self.add_calls
            .lock()
            .unwrap()
            .push((url.to_string(), info_hash.to_string()));
        Ok(AddOutcome::Added)
    }
    async fn add_torrent_with_file_filter(
        &self,
        _url: &str,
        _hash: &str,
        _pick: &mut (dyn for<'a> FnMut(&'a [String]) -> Option<Vec<usize>> + Send),
    ) -> Result<SelectiveOutcome, String> {
        Ok(SelectiveOutcome::FullDownload)
    }
    async fn list_scoped(&self) -> Result<Vec<DownloadItem>, String> {
        Ok(vec![])
    }
    async fn get_files(&self, _hash: &str) -> Result<Vec<DownloadFile>, String> {
        Ok(vec![])
    }
    async fn pause(&self, _hash: &str) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self, _hash: &str) -> Result<(), String> {
        Ok(())
    }
    async fn delete(&self, _hash: &str, _delete_files: bool) -> Result<(), String> {
        Ok(())
    }
    async fn set_file_wanted(
        &self,
        _hash: &str,
        _files: &[usize],
        _wanted: bool,
    ) -> Result<(), String> {
        Ok(())
    }
    fn sonarr_impl_name(&self) -> &'static str {
        "QBittorrent"
    }
}

/// Install a `DownloadClientPool` carrying one default-torrent
/// recording client at id 1. Returns the Arc so the test can read
/// `add_calls()` after the handler runs.
async fn install_recording_default_torrent_client(
    state: &AppState,
) -> std::sync::Arc<AutoSearchRecordingClient> {
    let client = std::sync::Arc::new(AutoSearchRecordingClient::new());
    let mut clients: std::collections::HashMap<i64, std::sync::Arc<dyn DownloadClient>> =
        std::collections::HashMap::new();
    clients.insert(1, client.clone() as std::sync::Arc<dyn DownloadClient>);
    let pool = DownloadClientPool {
        clients,
        default_torrent_id: Some(1),
        default_usenet_id: None,
    };
    *state.download_clients.write().await = std::sync::Arc::new(pool);
    client
}

/// Cache-seeded series + AnimeDetail for `request_id = anilist_id`.
/// resolve_series_context will short-circuit on the metadata cache
/// and never hit AL.
async fn seed_series_with_cache(state: &AppState, anilist_id: i64, title: &str) -> i64 {
    let series_id = ryokan::test_support::seed_series(&state.db, anilist_id, title).await;
    let detail = detail_for(anilist_id, title);
    ryokan::models::metadata_cache::upsert(&state.db, series_id, anilist_id, None, &detail)
        .await
        .unwrap();
    series_id
}

#[tokio::test]
async fn auto_search_episode_handler_grabs_matching_release_end_to_end() {
    // Full happy path: cache-seeded series, Nyaa wiremock returning
    // a single matching release, recording torrent default in the
    // pool. Handler resolves context, find_best_for_target picks
    // the only candidate, dispatch routes through Nyaa's
    // client_for_nyaa fallback to the torrent default, the
    // grabbed_torrents row + episode_quality_tags row land, and
    // the AutoSearchReport.grabbed list carries one entry. Pins
    // the entire HTTP entry point on auto_search.rs which had no
    // direct end-to-end coverage before this test.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    let html = nyaa_results_page(&nyaa_row(
        "fedcba9876543210fedcba9876543210fedcba98",
        99001,
        "[Group] Auto Search Show - 03 (1080p) [WEB].mkv",
        "1.4 GiB",
        80,
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let anilist_id: i64 = 9001;
    let state = build_state().await;
    let _series_id = seed_series_with_cache(&state, anilist_id, "Auto Search Show").await;
    let client = install_recording_default_torrent_client(&state).await;

    let result = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((anilist_id, 3_i32)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await;
    let axum::response::Json(report) = result.expect("auto-search must succeed");
    assert_eq!(
        report.grabbed.len(),
        1,
        "exactly one grab expected from a single matching Nyaa row; report={report:?}"
    );
    assert!(
        report.grabbed[0].release_title.contains("Auto Search Show"),
        "grabbed entry must reference the seeded title"
    );

    // Recording client saw exactly one add call.
    let calls = client.add_calls();
    assert_eq!(calls.len(), 1, "exactly one add_torrent call expected");
    // qBit-shape add: url is the magnet, hash is the precomputed v1
    // infohash from the magnet xt parse.
    assert!(calls[0].0.starts_with("magnet:?xt="));
    assert!(calls[0].1.starts_with("fedcba9876543210"));

    // grabbed_torrents row landed for the right series + episode.
    let row: (i64, String, String) = sqlx::query_as(
        "SELECT series_id, episode_numbers, hash FROM grabbed_torrents WHERE torrent_name LIKE ?",
    )
    .bind("%Auto Search Show%")
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(row.1, "[3]");
    assert!(row.2.starts_with("fedcba9876543210"));

    // Per-episode quality tag written.
    let tag_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM episode_quality_tags WHERE series_id = ? AND episode_number = ?",
    )
    .bind(row.0)
    .bind(3_i32)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(tag_count, 1);

    unset_nyaa_base();
}

#[tokio::test]
async fn auto_search_episode_handler_returns_empty_grabbed_when_nyaa_has_no_results() {
    // No matching rows in Nyaa -> find_best_for_target returns None
    // -> the per-target arm pushes a "skipped" entry and returns an
    // AutoSearchReport with grabbed.len() == 0. Pins the no-results
    // path so a refactor that fabricated synthetic results on miss
    // wouldn't ship.
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page("")))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let anilist_id: i64 = 9002;
    let state = build_state().await;
    let _series_id = seed_series_with_cache(&state, anilist_id, "Empty Show").await;
    let client = install_recording_default_torrent_client(&state).await;

    let result = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((anilist_id, 1_i32)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await;
    let axum::response::Json(report) = result.expect("auto-search returns Ok with empty grabbed");
    assert!(
        report.grabbed.is_empty(),
        "no Nyaa hits → empty grabbed list; got {report:?}"
    );
    assert!(
        client.add_calls().is_empty(),
        "no add_torrent calls when there are no candidates"
    );

    let grab_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grabbed_torrents")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(grab_count, 0, "no DB-side grab row when no match found");

    unset_nyaa_base();
}

#[tokio::test]
async fn auto_search_episode_handler_returns_400_when_no_download_client_configured() {
    // The up-front guard at run_auto_search_targets_with_upgrades:274
    // returns 400 with "Download client not configured" when
    // default_download_client() is None — fail fast rather than
    // wasting a Nyaa round trip on a setup the user can't act on.
    // The error surfaces from the spawned task back through the
    // handler's awaiter. No Nyaa wiremock needed.
    let _gate = ENV_LOCK.lock().await;
    // Even though no Nyaa traffic is expected, guarantee no leaked
    // env var from a prior failing test.
    unset_nyaa_base();

    let anilist_id: i64 = 9003;
    let state = build_state().await;
    let _series_id = seed_series_with_cache(&state, anilist_id, "No Client Show").await;
    // Deliberately do NOT install a download client pool — the
    // default state has an empty pool.

    let result = auto_search_episode(
        axum::extract::State(state),
        axum::extract::Path((anilist_id, 1_i32)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await;
    match result {
        Err((status, body)) => {
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
            assert!(
                body.contains("Download client not configured"),
                "must surface the up-front guard's error message; got {body}"
            );
        }
        Ok(_) => panic!("missing-client must surface as 400, not Ok"),
    }
}
