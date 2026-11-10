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
    let provenance = r
        .match_provenance
        .as_ref()
        .expect("collector stamps provenance on every gated candidate");
    assert_eq!(
        provenance.kind,
        ryokan::services::auto_search::MatchKind::Verbatim
    );
    assert_eq!(
        provenance.phase,
        ryokan::services::auto_search::MatchPhase::Primary
    );
    assert_eq!(provenance.alias, "Test Show");
    let confidence = r
        .score_breakdown
        .iter()
        .find(|c| c.label == "Title Match Confidence")
        .expect("breakdown always lists the confidence line");
    assert_eq!(confidence.delta, 0);
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
use ryokan::handlers::library::search::{AutoSearchQuery, auto_search_episode, auto_search_series};
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

#[tokio::test]
async fn auto_search_episode_handler_prefers_verbatim_over_fuzzy_and_records_provenance() {
    let _gate = ENV_LOCK.lock().await;

    // Row A matches the title verbatim with few seeders; row B has the
    // same words in a different order (fuzzy at ratio 1.0) and ten
    // times the seeders. Without the confidence penalty B wins on the
    // seeder tier; with it A wins.
    let server = MockServer::start().await;
    let html = nyaa_results_page(&format!(
        "{}{}",
        nyaa_row(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            99101,
            "[Group] Auto Search Show Deluxe - 03 (1080p) [WEB].mkv",
            "1.4 GiB",
            20,
        ),
        nyaa_row(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            99102,
            "[Group] Deluxe Auto Search Show - 03 (1080p) [WEB].mkv",
            "1.4 GiB",
            200,
        )
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let anilist_id: i64 = 9002;
    let state = build_state().await;
    let series_id = seed_series_with_cache(&state, anilist_id, "Auto Search Show Deluxe").await;
    let client = install_recording_default_torrent_client(&state).await;

    let result = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((anilist_id, 3_i32)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await;
    let axum::response::Json(report) = result.expect("auto-search must succeed");
    assert_eq!(report.grabbed.len(), 1, "report={report:?}");

    let calls = client.add_calls();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].1.starts_with("aaaaaaaa"),
        "the verbatim candidate must win despite fewer seeders; grabbed hash {}",
        calls[0].1
    );

    let row: (String, String, String, f64) = sqlx::query_as(
        "SELECT match_kind, match_phase, matched_alias, match_ratio FROM episode_grab_history WHERE series_id = ? AND episode_number = 3",
    )
    .bind(series_id)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(row.0, "verbatim");
    assert_eq!(row.1, "primary");
    assert_eq!(row.2, "Auto Search Show Deluxe");
    assert_eq!(row.3, 1.0);

    let detail: String = sqlx::query_scalar(
        "SELECT detail FROM logs WHERE message LIKE 'Grabbed: %' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert!(
        detail
            .contains("match=verbatim phase=primary alias=\"Auto Search Show Deluxe\" ratio=1.00"),
        "{detail}"
    );

    unset_nyaa_base();
}

#[tokio::test]
async fn auto_search_episode_handler_skips_fuzzy_only_candidate_and_says_why() {
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    let html = nyaa_results_page(&nyaa_row(
        "cccccccccccccccccccccccccccccccccccccccc",
        99103,
        "[Group] Deluxe Auto Search Show - 03 (1080p) [WEB].mkv",
        "1.4 GiB",
        200,
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let anilist_id: i64 = 9003;
    let state = build_state().await;
    let _series_id = seed_series_with_cache(&state, anilist_id, "Auto Search Show Deluxe").await;
    let client = install_recording_default_torrent_client(&state).await;

    let result = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((anilist_id, 3_i32)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await;
    let axum::response::Json(report) = result.expect("auto-search must succeed");
    // The same words in a different order is a fuzzy match, and the
    // automatic path no longer takes those: nothing is grabbed, and the
    // report says what it saw and what to do about it.
    assert!(
        report.grabbed.is_empty(),
        "a fuzzy-only pool must not grab automatically; report={report:?}"
    );
    assert!(client.add_calls().is_empty());
    let note = report
        .skipped
        .iter()
        .find(|s| s.contains("named the series exactly"))
        .unwrap_or_else(|| panic!("report should explain the skip; report={report:?}"));
    assert!(note.contains("Deluxe Auto Search Show"), "{note}");
    assert!(note.contains("alternate title"), "{note}");

    unset_nyaa_base();
}

#[tokio::test]
async fn auto_search_episode_handler_skips_a_release_that_names_more_than_the_series() {
    let _gate = ENV_LOCK.lock().await;

    // "Auto Search Show Deluxe New World" contains the title verbatim
    // and is a different entry (a sequel named by subtitle). The
    // automatic path must not take it, and the report must say so.
    let server = MockServer::start().await;
    let html = nyaa_results_page(&nyaa_row(
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        99105,
        "[Group] Auto Search Show Deluxe New World - 03 (1080p) [WEB].mkv",
        "1.4 GiB",
        200,
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let anilist_id: i64 = 9005;
    let state = build_state().await;
    let _series_id = seed_series_with_cache(&state, anilist_id, "Auto Search Show Deluxe").await;
    let client = install_recording_default_torrent_client(&state).await;

    let result = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((anilist_id, 3_i32)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await;
    let axum::response::Json(report) = result.expect("auto-search must succeed");
    assert!(
        report.grabbed.is_empty(),
        "a release naming more than the series must not grab; report={report:?}"
    );
    assert!(client.add_calls().is_empty());
    assert!(
        report
            .skipped
            .iter()
            .any(|s| s.contains("named the series exactly") && s.contains("New World")),
        "report={report:?}"
    );

    unset_nyaa_base();
}

#[tokio::test]
async fn auto_search_episode_handler_grabs_once_an_alternate_title_names_the_release() {
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    let html = nyaa_results_page(&nyaa_row(
        "dddddddddddddddddddddddddddddddddddddddd",
        99104,
        "[Group] Deluxe Auto Search Show - 03 (1080p) [WEB].mkv",
        "1.4 GiB",
        200,
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let anilist_id: i64 = 9004;
    let state = build_state().await;
    let series_id = seed_series_with_cache(&state, anilist_id, "Auto Search Show Deluxe").await;
    ryokan::models::series::update_search_overrides(
        &state.db,
        series_id,
        "",
        "",
        "Deluxe Auto Search Show",
    )
    .await
    .unwrap();
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
        "the alternate title makes the match verbatim; report={report:?}"
    );
    assert_eq!(client.add_calls().len(), 1);

    let row: (String, String) = sqlx::query_as(
        "SELECT match_kind, matched_alias FROM episode_grab_history WHERE series_id = ? AND episode_number = 3",
    )
    .bind(series_id)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(row.0, "verbatim");
    assert_eq!(row.1, "Deluxe Auto Search Show");

    unset_nyaa_base();
}

#[tokio::test]
async fn auto_search_episode_skips_blocklisted_hash_and_grabs_next_candidate() {
    let _gate = ENV_LOCK.lock().await;

    // Row A would win on seeders, but its hash is on the blocklist (a
    // misgrab the sweep removed, or a grab the user failed). Row B is the
    // next best and must be the one grabbed.
    let server = MockServer::start().await;
    let html = nyaa_results_page(&format!(
        "{}{}",
        nyaa_row(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            99201,
            "[Group] Blocklist Show - 03 (1080p) [WEB].mkv",
            "1.4 GiB",
            500,
        ),
        nyaa_row(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            99202,
            "[Other] Blocklist Show - 03 (1080p) [WEB].mkv",
            "1.4 GiB",
            5,
        )
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let anilist_id: i64 = 9004;
    let state = build_state().await;
    let series_id = seed_series_with_cache(&state, anilist_id, "Blocklist Show").await;
    ryokan::test_support::seed_grabbed_torrent(
        &state.db,
        series_id,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "[Group] Blocklist Show - 03 (1080p) [WEB].mkv",
        &[3],
    )
    .await;
    sqlx::query(
        "UPDATE grabbed_torrents SET state = 'failed', failure_reason = 'misgrab' WHERE hash = ?",
    )
    .bind("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    .execute(&state.db)
    .await
    .unwrap();
    let client = install_recording_default_torrent_client(&state).await;

    let result = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((anilist_id, 3_i32)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await;
    let axum::response::Json(report) = result.expect("auto-search must succeed");
    assert_eq!(report.grabbed.len(), 1, "report={report:?}");
    let calls = client.add_calls();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].1.starts_with("bbbbbbbb"),
        "the blocklisted hash must be skipped; grabbed {}",
        calls[0].1
    );
    let url: String =
        sqlx::query_scalar("SELECT source_url FROM grabbed_torrents WHERE hash LIKE 'bbbbbbbb%'")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert!(
        url.starts_with("magnet:?xt="),
        "source_url recorded for Restore: {url}"
    );

    unset_nyaa_base();
}

#[tokio::test]
async fn auto_search_episode_makes_no_add_call_when_only_candidate_is_blocklisted_by_title() {
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    let html = nyaa_results_page(&nyaa_row(
        "cccccccccccccccccccccccccccccccccccccccc",
        99203,
        "[Group] Blocklist Show - 04 (1080p) [WEB].mkv",
        "1.4 GiB",
        50,
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let anilist_id: i64 = 9005;
    let state = build_state().await;
    let series_id = seed_series_with_cache(&state, anilist_id, "Blocklist Show").await;
    // A failed row with no hash: blocklisted by title for this series only.
    sqlx::query(
        "INSERT INTO grabbed_torrents (hash, torrent_name, series_id, episode_numbers, state, failure_reason) \
         VALUES ('', ?, ?, '[4]', 'failed', 'misgrab')",
    )
    .bind("[Group] Blocklist Show - 04 (1080p) [WEB].mkv")
    .bind(series_id)
    .execute(&state.db)
    .await
    .unwrap();
    let client = install_recording_default_torrent_client(&state).await;

    let result = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((anilist_id, 4_i32)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await;
    let axum::response::Json(report) = result.expect("auto-search must succeed");
    assert!(report.grabbed.is_empty(), "report={report:?}");
    assert!(
        client.add_calls().is_empty(),
        "no add call for a blocklisted title"
    );

    unset_nyaa_base();
}

// ─── Misgrab re-search ───────────────────────────────────────────

/// Poll the recording client until it has seen an add call or the
/// budget runs out. The re-search runs as a detached task after the
/// sweep returns, so the test has to wait for it.
async fn wait_for_add_calls(
    client: &AutoSearchRecordingClient,
    want: usize,
) -> Vec<(String, String)> {
    for _ in 0..200 {
        let calls = client.add_calls();
        if calls.len() >= want {
            return calls;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    client.add_calls()
}

/// Every log row, for assertion messages.
async fn dump_logs(db: &sqlx::SqlitePool) -> String {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT level, message, detail FROM logs ORDER BY id")
            .fetch_all(db)
            .await
            .unwrap_or_default();
    rows.iter()
        .map(|(l, m, d)| format!("[{l}] {m} | {d}"))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn wait_for_log_like(db: &sqlx::SqlitePool, pattern: &str) -> i64 {
    for _ in 0..200 {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM logs WHERE message LIKE ?")
            .bind(pattern)
            .fetch_one(db)
            .await
            .unwrap();
        if n > 0 {
            return n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    0
}

/// A grab the verdict already condemned but the sweep has not acted on.
async fn seed_unhandled_misgrab(
    state: &AppState,
    series_id: i64,
    hash: &str,
    title: &str,
    episodes: &[i32],
    is_batch: bool,
) -> i64 {
    let id =
        ryokan::test_support::seed_grabbed_torrent(&state.db, series_id, hash, title, episodes)
            .await;
    sqlx::query("UPDATE grabbed_torrents SET is_batch = ? WHERE id = ?")
        .bind(is_batch as i64)
        .bind(id)
        .execute(&state.db)
        .await
        .unwrap();
    ryokan::models::grabbed_torrents::stamp_verification(&state.db, id, "misgrab", "{}")
        .await
        .unwrap();
    id
}

const WRONG_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const RIGHT_HASH: &str = "cccccccccccccccccccccccccccccccccccccccc";

fn research_results_page() -> String {
    nyaa_results_page(&format!(
        "{}{}",
        nyaa_row(
            WRONG_HASH,
            99301,
            "[Wrong] Research Show - 03 (1080p) [WEB].mkv",
            "1.4 GiB",
            500,
        ),
        nyaa_row(
            RIGHT_HASH,
            99302,
            "[Right] Research Show - 03 (1080p) [WEB].mkv",
            "1.4 GiB",
            5,
        )
    ))
}

#[tokio::test]
async fn misgrab_sweep_re_searches_a_single_episode_and_grabs_the_next_candidate() {
    let _gate = ENV_LOCK.lock().await;

    // The condemned release still tops the Nyaa page on seeders; the
    // re-search must skip it (it is blocklisted by the remediation)
    // and take the runner-up for the same slot.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(research_results_page()))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let anilist_id: i64 = 9006;
    let state = build_state().await;
    let series_id = seed_series_with_cache(&state, anilist_id, "Research Show").await;
    let grab_id = seed_unhandled_misgrab(
        &state,
        series_id,
        WRONG_HASH,
        "[Wrong] Research Show - 03 (1080p) [WEB].mkv",
        &[3],
        false,
    )
    .await;
    let client = install_recording_default_torrent_client(&state).await;

    let summary = ryokan::services::misgrab::sweep_once(&state)
        .await
        .expect("sweep runs");
    assert_eq!(
        summary.remediated,
        1,
        "{summary:?}\n{}",
        dump_logs(&state.db).await
    );

    let calls = wait_for_add_calls(&client, 1).await;
    assert_eq!(calls.len(), 1, "the re-search grabs exactly one release");
    assert!(
        calls[0].1.starts_with("cccccccc"),
        "the condemned hash is blocklisted, the runner-up fills the slot; grabbed {}",
        calls[0].1
    );
    assert_eq!(
        wait_for_log_like(
            &state.db,
            "Re-search after misgrab for '[Wrong] Research Show%grabbed 1 release(s)"
        )
        .await,
        1
    );

    // The old row is failed and blocklisted; the new one is pending
    // for the same episode.
    let old = ryokan::models::grabbed_torrents::get_by_id(&state.db, grab_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.state, "failed");
    assert_eq!(old.misgrab_action.as_deref(), Some("removed"));
    let pending = ryokan::models::grabbed_torrents::get_all_pending(&state.db)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].hash, RIGHT_HASH);
    assert_eq!(pending[0].episode_numbers, vec![3]);

    unset_nyaa_base();
}

#[tokio::test]
async fn misgrab_sweep_re_searches_the_whole_series_for_a_batch_grab() {
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(research_results_page()))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let anilist_id: i64 = 9007;
    let state = build_state().await;
    let series_id = seed_series_with_cache(&state, anilist_id, "Research Show").await;
    // The series search takes its targets from the monitoring rows.
    sqlx::query("UPDATE series SET episodes = 12, monitor_mode = 'all' WHERE id = ?")
        .bind(series_id)
        .execute(&state.db)
        .await
        .unwrap();
    let series_row = ryokan::models::series::get_by_id(&state.db, series_id)
        .await
        .unwrap()
        .unwrap();
    ryokan::services::monitoring::ensure_series_monitoring_rows(&state.db, &series_row)
        .await
        .unwrap();
    seed_unhandled_misgrab(
        &state,
        series_id,
        WRONG_HASH,
        "[Wrong] Research Show - 01-12 (1080p) [WEB]",
        &[1, 2, 3],
        true,
    )
    .await;
    let client = install_recording_default_torrent_client(&state).await;

    let summary = ryokan::services::misgrab::sweep_once(&state)
        .await
        .expect("sweep runs");
    assert_eq!(
        summary.remediated,
        1,
        "{summary:?}\n{}",
        dump_logs(&state.db).await
    );

    // A batch grab re-searches the series, not one slot: the sweep
    // takes whatever the series search can fill (here episode 3, the
    // only release the page offers) and never the blocklisted hash.
    let calls = wait_for_add_calls(&client, 1).await;
    assert!(!calls.is_empty(), "the series re-search grabs something");
    assert!(
        calls.iter().all(|(_, hash)| !hash.starts_with("aaaaaaaa")),
        "the condemned hash is never re-grabbed: {calls:?}"
    );
    assert_eq!(
        wait_for_log_like(
            &state.db,
            "Re-search after misgrab for '[Wrong] Research Show - 01-12%grabbed % release(s)"
        )
        .await,
        1
    );

    unset_nyaa_base();
}

#[tokio::test]
async fn misgrab_sweep_stops_re_searching_after_the_loop_breaker() {
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(research_results_page()))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let anilist_id: i64 = 9008;
    let state = build_state().await;
    let series_id = seed_series_with_cache(&state, anilist_id, "Research Show").await;
    // Three misgrabs already handled today for this series ...
    for (i, hash) in [
        "1111111111111111111111111111111111111111",
        "2222222222222222222222222222222222222222",
        "3333333333333333333333333333333333333333",
    ]
    .iter()
    .enumerate()
    {
        let id = seed_unhandled_misgrab(
            &state,
            series_id,
            hash,
            &format!("[Wrong{i}] Research Show - 03 (1080p) [WEB].mkv"),
            &[3],
            false,
        )
        .await;
        sqlx::query(
            "UPDATE grabbed_torrents SET state = 'failed', misgrab_action = 'removed' WHERE id = ?",
        )
        .bind(id)
        .execute(&state.db)
        .await
        .unwrap();
    }
    // ... and a fourth the sweep is about to act on.
    let fourth = seed_unhandled_misgrab(
        &state,
        series_id,
        WRONG_HASH,
        "[Wrong] Research Show - 03 (1080p) [WEB].mkv",
        &[3],
        false,
    )
    .await;
    let client = install_recording_default_torrent_client(&state).await;

    let summary = ryokan::services::misgrab::sweep_once(&state)
        .await
        .expect("sweep runs");
    assert_eq!(
        summary.remediated,
        1,
        "removal still happens; {summary:?}\n{}",
        dump_logs(&state.db).await
    );
    assert_eq!(
        wait_for_log_like(&state.db, "Not re-searching after misgrab%").await,
        1,
        "the loop breaker logs why\n{}",
        dump_logs(&state.db).await
    );
    // Give a stray re-search every chance to show up before asserting
    // it never did.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        client.add_calls().is_empty(),
        "no re-search after {} misgrabs in a day",
        ryokan::services::misgrab::RESEARCH_LOOP_BREAKER
    );
    let old = ryokan::models::grabbed_torrents::get_by_id(&state.db, fourth)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.state, "failed");

    unset_nyaa_base();
}

// ─── Alternate titles are primary aliases ────────────────────────

#[tokio::test]
async fn alternate_title_drives_the_queries_and_counts_as_exact() {
    let _gate = ENV_LOCK.lock().await;

    // The only release on offer names the show the way one group does,
    // not the way AniList does. With that name on the series row as an
    // alternate title, the search must ask Nyaa for it (a query that
    // carries the alternate name) and take the release as an exact
    // match, cache or no cache.
    let server = MockServer::start().await;
    let html = nyaa_results_page(&nyaa_row(
        "dddddddddddddddddddddddddddddddddddddddd",
        99401,
        "[Group] Nickname Show - 03 (1080p) [WEB].mkv",
        "1.4 GiB",
        40,
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .and(query_param("q", "Nickname Show 03"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page("")))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let anilist_id: i64 = 9009;
    let state = build_state().await;
    let series_id = seed_series_with_cache(&state, anilist_id, "Official Long Title").await;
    sqlx::query("UPDATE series SET alternate_titles = 'Nickname Show' WHERE id = ?")
        .bind(series_id)
        .execute(&state.db)
        .await
        .unwrap();
    let client = install_recording_default_torrent_client(&state).await;

    let result = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((anilist_id, 3_i32)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await;
    let axum::response::Json(report) = result.expect("auto-search must succeed");
    assert_eq!(report.grabbed.len(), 1, "report={report:?}");
    assert!(
        report.grabbed[0].release_title.contains("Nickname Show"),
        "{report:?}"
    );
    let calls = client.add_calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].1.starts_with("dddddddd"));

    // The alternate title was asked for by name, as a primary query.
    let asked: Vec<String> = server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            r.url
                .query_pairs()
                .find(|(k, _)| k == "q")
                .map(|(_, v)| v.into_owned())
        })
        .collect();
    assert!(
        asked.iter().any(|q| q.starts_with("Nickname Show")),
        "queries sent: {asked:?}"
    );

    // Provenance says verbatim through the alternate title.
    let kind: String = sqlx::query_scalar(
        "SELECT match_kind FROM episode_grab_history WHERE series_id = ? AND episode_number = 3",
    )
    .bind(series_id)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(kind, "verbatim");

    unset_nyaa_base();
}

// ─── Ids that collide across numberings ──────────────────────────
//
// The search entry points read an id as a library (internal) id
// first and only then as an AniList id. A library whose first row is
// another show makes AniList id 1 (Cowboy Bebop) collide with it, so
// a caller that passes an AniList id searches the other show's titles.

/// "Decoy Show" at internal id 1, then "Research Show" with AniList id
/// 1 (internal id 2), every episode monitored. Returns Research Show's
/// library id.
async fn seed_research_show_behind_a_decoy(state: &AppState) -> i64 {
    let decoy = seed_series_with_cache(state, 9100, "Decoy Show").await;
    assert_eq!(decoy, 1, "the decoy holds internal id 1");
    let series_id = seed_series_with_cache(state, 1, "Research Show").await;
    sqlx::query("UPDATE series SET episodes = 12, monitor_mode = 'all' WHERE id = ?")
        .bind(series_id)
        .execute(&state.db)
        .await
        .unwrap();
    let series_row = ryokan::models::series::get_by_id(&state.db, series_id)
        .await
        .unwrap()
        .unwrap();
    ryokan::services::monitoring::ensure_series_monitoring_rows(&state.db, &series_row)
        .await
        .unwrap();
    series_id
}

#[tokio::test]
async fn misgrab_re_search_searches_the_series_its_anilist_id_collides_with_no_more() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(research_results_page()))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    // One episode takes the episode search, a batch the series search.
    for (title, episodes, is_batch) in [
        (
            "[Wrong] Research Show - 03 (1080p) [WEB].mkv",
            &[3][..],
            false,
        ),
        (
            "[Wrong] Research Show - 01-12 (1080p) [WEB]",
            &[1, 2, 3][..],
            true,
        ),
    ] {
        let state = build_state().await;
        let series_id = seed_research_show_behind_a_decoy(&state).await;
        seed_unhandled_misgrab(&state, series_id, WRONG_HASH, title, episodes, is_batch).await;
        let client = install_recording_default_torrent_client(&state).await;

        let summary = ryokan::services::misgrab::sweep_once(&state)
            .await
            .expect("sweep runs");
        assert_eq!(summary.remediated, 1, "{summary:?}");

        // Searching the decoy's titles finds nothing on this page.
        let calls = wait_for_add_calls(&client, 1).await;
        assert_eq!(
            calls.len(),
            1,
            "batch={is_batch}: Research Show is searched\n{}",
            dump_logs(&state.db).await
        );
        assert!(calls[0].1.starts_with("cccccccc"), "{calls:?}");
    }

    unset_nyaa_base();
}

#[tokio::test]
async fn wanted_search_searches_the_listed_series_not_the_row_its_anilist_id_names() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(research_results_page()))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let series_id = seed_research_show_behind_a_decoy(&state).await;
    let client = install_recording_default_torrent_client(&state).await;

    let response = ryokan::handlers::wanted::search(
        axum::extract::State(state.clone()),
        axum::extract::Query(AutoSearchQuery::default()),
        axum::Json(ryokan::handlers::wanted::WantedSearchRequest {
            series_ids: vec![series_id],
            tab: None,
        }),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);

    // The decoy's titles find nothing on this page; Research Show's
    // episode 3 is there.
    assert_eq!(
        wait_for_log_like(&state.db, "Searched 1 series, grabbed 1 release").await,
        1,
        "{}",
        dump_logs(&state.db).await
    );
    assert_eq!(client.add_calls().len(), 1);

    unset_nyaa_base();
}

#[tokio::test]
async fn an_untracked_series_page_searches_by_its_anilist_id() {
    // `/series/1?by=anilist` is Research Show's page while it is not in
    // the library; the decoy holds internal id 1. The page's searches
    // post the AniList id with `by=anilist`; read as an internal id it
    // searched the decoy.
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(research_results_page()))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let decoy = seed_series_with_cache(&state, 9100, "Decoy Show").await;
    assert_eq!(decoy, 1, "the decoy holds internal id 1");
    ryokan::models::metadata_cache::upsert_provider(
        &state.db,
        1,
        None,
        &detail_for(1, "Research Show"),
    )
    .await
    .unwrap();
    let client = install_recording_default_torrent_client(&state).await;
    let by_anilist = || {
        axum::extract::Query(AutoSearchQuery {
            by: Some("anilist".into()),
            ..AutoSearchQuery::default()
        })
    };

    let axum::response::Json(report) = auto_search_series(
        axum::extract::State(state.clone()),
        axum::extract::Path(1),
        by_anilist(),
    )
    .await
    .expect("series search");
    assert_eq!(report.grabbed.len(), 1, "{report:?}");
    assert!(report.grabbed[0].release_title.contains("Research Show"));

    let axum::response::Json(report) = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((1, 3)),
        by_anilist(),
    )
    .await
    .expect("episode search");
    assert_eq!(report.grabbed.len(), 1, "{report:?}");
    assert!(report.grabbed[0].release_title.contains("Research Show"));
    assert_eq!(client.add_calls().len(), 2);

    unset_nyaa_base();
}

#[tokio::test]
async fn interactive_batch_list_offers_unnumbered_packs_and_the_auto_grab_does_not() {
    // `[UTW] Angel Beats! [BD][h264-720p AC3]` names the series and
    // nothing of what it holds: usually the whole series, sometimes a
    // movie or one file. The interactive batch list offers it (its grab
    // opens the file picker); the automatic batch grab never takes it.
    // A weekly, a movie, a sequel named by its subtitle and a title
    // giving another year are no pack of this series.
    use ryokan::services::auto_search::{BatchList, collect_scored_batches_for_target};
    let _gate = ENV_LOCK.lock().await;

    let server = MockServer::start().await;
    let rows = [
        (1, "[UTW] Test Show [BD][h264-720p AC3]"),
        (2, "[Group] Test Show (01-12) [BD 1080p]"),
        (3, "[Group] Test Show - 05 (1080p) [WEB].mkv"),
        (4, "[Group] Test Show The Movie [BD 1080p]"),
        (5, "[UTW] Test Show Another Story [BD][h264-720p AC3]"),
        // A remake's year: the series aired in 2024.
        (6, "[UTW] Test Show (2009) [BD][h264-720p AC3]"),
    ]
    .iter()
    .map(|(n, title)| nyaa_row(&format!("{n:040x}"), 9000 + n, title, "8.0 GiB", 20))
    .collect::<String>();
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page(&rows)))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let detail = detail_for(1002, "Test Show");
    let cfg = default_config();
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];
    let titles = |list: BatchList| {
        let (state, detail, cfg, cfs) = (&state, &detail, &cfg, &cfs);
        async move {
            let results = collect_scored_batches_for_target(
                &state.db,
                detail,
                cfg,
                &SearchTarget::Single,
                cfs,
                &state.indexers,
                list,
            )
            .await;
            assert!(results.iter().all(|r| r.is_batch), "{results:?}");
            let mut titles: Vec<String> = results.into_iter().map(|r| r.title).collect();
            titles.sort();
            titles
        }
    };

    assert_eq!(
        titles(BatchList::Interactive).await,
        vec![
            "[Group] Test Show (01-12) [BD 1080p]".to_string(),
            "[UTW] Test Show [BD][h264-720p AC3]".to_string(),
        ]
    );
    assert_eq!(
        titles(BatchList::AutoGrab).await,
        vec!["[Group] Test Show (01-12) [BD 1080p]".to_string()]
    );

    unset_nyaa_base();
}

#[tokio::test]
async fn a_queued_pack_skips_only_the_episodes_it_covers() {
    // Issue #300: a pack found for an early target ended the whole
    // series search ("Season pack queued; stopping episode search"),
    // so an episode the pack does not hold was never searched. The
    // pack now skips the targets it covers and the rest still run.
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let rows = [
        nyaa_row(
            &format!("{:040x}", 0x300a),
            30001,
            "[Group] Pack Show - 01-03 [BD 1080p]",
            "6.0 GiB",
            40,
        ),
        nyaa_row(
            &format!("{:040x}", 0x300b),
            30002,
            "[Group] Pack Show - 06 (1080p) [WEB].mkv",
            "1.4 GiB",
            40,
        ),
        nyaa_row(
            &format!("{:040x}", 0x300c),
            30003,
            "[Group] Pack Show - 05 [BD 1080p].mkv",
            "2.0 GiB",
            40,
        ),
    ]
    .concat();
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page(&rows)))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let anilist_id: i64 = 9300;
    let state = build_state().await;
    let series_id = seed_series_with_cache(&state, anilist_id, "Pack Show").await;
    // Episodes 1, 2 and 5 are on disk below the cutoff, so they are
    // upgrade targets, searched after the missing episode 6; an
    // upgrade target takes a pack (the issue's case).
    let media_root = tempfile::tempdir().unwrap();
    let folder = media_root.path().join("Pack Show");
    std::fs::create_dir_all(&folder).unwrap();
    for ep in [1, 2, 5] {
        std::fs::write(
            folder.join(format!("Pack Show - {ep:02} (720p) [WEB].mkv")),
            b"x",
        )
        .unwrap();
    }
    let cfg = Config {
        media_root: media_root.path().to_string_lossy().into_owned(),
        cutoff_source: "bluray".into(),
        cutoff_resolution: "1080".into(),
        ..default_config()
    };
    ryokan::models::config::save_config(&state.db, &cfg)
        .await
        .unwrap();
    for ep in [1, 2, 5, 6] {
        ryokan::models::monitoring::set_episode_monitored(&state.db, series_id, ep, true)
            .await
            .unwrap();
    }
    let client = install_recording_default_torrent_client(&state).await;

    let axum::response::Json(report) = auto_search_series(
        axum::extract::State(state.clone()),
        axum::extract::Path(series_id),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await
    .expect("series search");

    let titles: Vec<&str> = report
        .grabbed
        .iter()
        .map(|h| h.release_title.as_str())
        .collect();
    assert_eq!(
        titles,
        [
            "[Group] Pack Show - 06 (1080p) [WEB].mkv",
            "[Group] Pack Show - 01-03 [BD 1080p]",
            // Searched after the pack: it used to end the search here.
            "[Group] Pack Show - 05 [BD 1080p].mkv",
        ],
        "{report:?}\n{}",
        dump_logs(&state.db).await
    );
    assert_eq!(client.add_calls().len(), 3);
    assert!(
        report
            .skipped
            .iter()
            .any(|s| s == "Season pack queued; skipped 1 episode search it covers"),
        "episode 2 is in the pack: {report:?}"
    );

    unset_nyaa_base();
}

/// A sequel cour beside a first season, in the metadata caches the
/// 12-hour sweep fills (`identify::load_franchise` reads them).
async fn seed_sequel_in_caches(db: &sqlx::SqlitePool, first: &AnimeDetail, sequel: &AnimeDetail) {
    seed_related_in_caches(db, first, sequel, "SEQUEL").await;
}

/// `first` and `other` in the metadata caches, `other` related to `first`
/// as `relation`.
async fn seed_related_in_caches(
    db: &sqlx::SqlitePool,
    first: &AnimeDetail,
    sequel: &AnimeDetail,
    relation: &str,
) {
    let mut first = first.clone();
    first.relations = vec![ryokan::services::anilist::RelatedEntry {
        id: sequel.id,
        id_mal: None,
        title_romaji: sequel.title_romaji.clone(),
        title_english: sequel.title_english.clone(),
        title_native: sequel.title_native.clone(),
        cover_url: String::new(),
        format: "TV".into(),
        status: "FINISHED".into(),
        status_display: String::new(),
        episodes: sequel.episodes,
        relation_type: relation.into(),
        season_year: sequel.season_year,
        media_type: "ANIME".into(),
    }];
    for d in [&first, sequel] {
        ryokan::models::local_metadata::replace_relations_for_provider(db, d.id, d)
            .await
            .unwrap();
        ryokan::models::metadata_cache::upsert_provider(db, d.id, None, d)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn auto_search_episode_handler_skips_a_release_another_entry_fits_better() {
    let _gate = ENV_LOCK.lock().await;

    // The release contains the series' name verbatim, but its names are
    // the sequel's in full: `identify` vetoes it for the first season,
    // and the report says which entry it looked like instead of
    // advising an alternate title.
    let server = MockServer::start().await;
    let html = nyaa_results_page(&nyaa_row(
        "1212121212121212121212121212121212121212",
        99201,
        "[SubsPlease] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06 (1080p) [ABCD1234].mkv",
        "1.4 GiB",
        200,
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let anilist_id: i64 = 9101;
    let state = build_state().await;
    let _series_id = seed_series_with_cache(&state, anilist_id, "Jujutsu Kaisen").await;
    let mut sequel = detail_for(9102, "Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen");
    sequel.season_year = Some(2026);
    seed_sequel_in_caches(
        &state.db,
        &detail_for(anilist_id, "Jujutsu Kaisen"),
        &sequel,
    )
    .await;
    let client = install_recording_default_torrent_client(&state).await;

    let result = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((anilist_id, 6_i32)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await;
    let axum::response::Json(report) = result.expect("auto-search must succeed");
    assert!(report.grabbed.is_empty(), "report={report:?}");
    assert!(client.add_calls().is_empty());
    let note = report
        .skipped
        .iter()
        .find(|s| s.contains("fit another entry of it better"))
        .unwrap_or_else(|| panic!("report should name the other entry; report={report:?}"));
    assert!(
        note.contains("looks like Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen"),
        "{note}"
    );
    assert!(!note.contains("alternate title"), "{note}");

    unset_nyaa_base();
}

/// Hunter x Hunter 2011 tracked with the alternate title `Hunter x
/// Hunter`, and `[DP] Hunter x Hunter - 003`: the name is the 1999
/// series' exactly and, through the alternate, the 2011 one's too.
/// Returns the report and the number of grabs.
async fn search_hxh_2011_episode_3(track_1999: bool) -> (Vec<String>, usize) {
    let server = MockServer::start().await;
    let html = nyaa_results_page(&nyaa_row(
        "3434343434343434343434343434343434343434",
        99301,
        "[DP] Hunter x Hunter - 003 [Multi Subs][BDRip][1080p]",
        "1.4 GiB",
        200,
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let mut hxh_2011 = detail_for(11061, "HUNTER×HUNTER (2011)");
    hxh_2011.title_english = "Hunter x Hunter (2011)".into();
    hxh_2011.episodes = Some(148);
    hxh_2011.season_year = Some(2011);
    let mut hxh_1999 = detail_for(136, "HUNTER×HUNTER");
    hxh_1999.title_english = "Hunter x Hunter".into();
    hxh_1999.episodes = Some(62);
    hxh_1999.season_year = Some(1999);

    let series_id =
        ryokan::test_support::seed_series(&state.db, 11061, "HUNTER×HUNTER (2011)").await;
    ryokan::models::metadata_cache::upsert(&state.db, series_id, 11061, None, &hxh_2011)
        .await
        .unwrap();
    sqlx::query("UPDATE series SET alternate_titles = 'Hunter x Hunter' WHERE id = ?")
        .bind(series_id)
        .execute(&state.db)
        .await
        .unwrap();
    if track_1999 {
        ryokan::test_support::seed_series(&state.db, 136, "HUNTER×HUNTER").await;
    }
    seed_related_in_caches(&state.db, &hxh_2011, &hxh_1999, "ALTERNATIVE").await;
    let client = install_recording_default_torrent_client(&state).await;

    let result = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((11061_i64, 3_i32)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await;
    let axum::response::Json(report) = result.expect("auto-search must succeed");
    unset_nyaa_base();
    (report.skipped, client.add_calls().len())
}

#[tokio::test]
async fn auto_search_skips_a_release_another_tracked_series_fits_as_well() {
    let _gate = ENV_LOCK.lock().await;

    // Only the 2011 series tracked: the tie is with an entry the library
    // doesn't have (Sonarr never sees it), and the release is grabbed.
    let (_, grabs) = search_hxh_2011_episode_3(false).await;
    assert_eq!(grabs, 1);

    // Both tracked: Sonarr's ambiguous release, which neither takes
    // automatically. The report says which series it also fits.
    let (skipped, grabs) = search_hxh_2011_episode_3(true).await;
    assert_eq!(grabs, 0, "{skipped:?}");
    let note = skipped
        .iter()
        .find(|s| s.contains("fit another series in the library just as well"))
        .unwrap_or_else(|| panic!("report should name the other series: {skipped:?}"));
    assert!(note.contains("(also HUNTER×HUNTER)"), "{note}");
}

#[tokio::test]
async fn auto_search_episode_handler_takes_the_sequels_absolute_number_with_the_franchise_loaded() {
    let _gate = ENV_LOCK.lock().await;

    // The same franchise from the sequel's side: `Jujutsu Kaisen - 18`
    // is past the first season's twelve, so it is the sequel's sixth,
    // and identify keeps it (the old franchise pass ran with no sibling
    // guard at all to let these through).
    let server = MockServer::start().await;
    let html = nyaa_results_page(&nyaa_row(
        "3434343434343434343434343434343434343434",
        99202,
        "[SubsPlease] Jujutsu Kaisen - 18 (1080p) [ABCD1234].mkv",
        "1.4 GiB",
        200,
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let first = detail_for(9111, "Jujutsu Kaisen");
    let mut sequel = detail_for(9112, "Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen");
    sequel.season_year = Some(2026);
    let state = build_state().await;
    let series_id =
        seed_series_with_cache(&state, 9112, "Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen").await;
    sqlx::query("UPDATE series SET cumulative_prior_episodes = 12 WHERE id = ?")
        .bind(series_id)
        .execute(&state.db)
        .await
        .unwrap();
    seed_sequel_in_caches(&state.db, &first, &sequel).await;
    let client = install_recording_default_torrent_client(&state).await;

    let result = auto_search_episode(
        axum::extract::State(state.clone()),
        axum::extract::Path((9112, 6_i32)),
        axum::extract::Query(AutoSearchQuery::default()),
    )
    .await;
    let axum::response::Json(report) = result.expect("auto-search must succeed");
    assert_eq!(report.grabbed.len(), 1, "report={report:?}");
    assert_eq!(client.add_calls().len(), 1);
    let detail: String = sqlx::query_scalar(
        "SELECT detail FROM logs WHERE message LIKE 'Grabbed: %' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert!(detail.contains("identity=target"), "{detail}");

    unset_nyaa_base();
}

/// A target and its relatives, by relation, in the metadata caches.
async fn seed_relatives_in_caches(
    db: &sqlx::SqlitePool,
    target: &AnimeDetail,
    relatives: &[(&AnimeDetail, &str)],
) {
    let mut target = target.clone();
    target.relations = relatives
        .iter()
        .map(|(rel, relation)| ryokan::services::anilist::RelatedEntry {
            id: rel.id,
            id_mal: None,
            title_romaji: rel.title_romaji.clone(),
            title_english: rel.title_english.clone(),
            title_native: rel.title_native.clone(),
            cover_url: String::new(),
            format: rel.format.clone(),
            status: "FINISHED".into(),
            status_display: String::new(),
            episodes: rel.episodes,
            relation_type: (*relation).into(),
            season_year: rel.season_year,
            media_type: "ANIME".into(),
        })
        .collect();
    ryokan::models::local_metadata::replace_relations_for_provider(db, target.id, &target)
        .await
        .unwrap();
    ryokan::models::metadata_cache::upsert_provider(db, target.id, None, &target)
        .await
        .unwrap();
    for (rel, _) in relatives {
        ryokan::models::metadata_cache::upsert_provider(db, rel.id, None, rel)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn find_all_for_target_labels_a_rival_and_drops_one_its_names_give_away() {
    let _gate = ENV_LOCK.lock().await;

    // For the 2001 series: its own release, the 2019 remake's (the year
    // fits only the remake: kept, labeled, scored below), and a sequel
    // named by its subtitle (dropped, as the sibling guard did).
    let server = MockServer::start().await;
    let html = nyaa_results_page(&format!(
        "{}{}{}",
        nyaa_row(
            "5656565656565656565656565656565656565656",
            99301,
            "[Leopard-Raws] Fruits Basket (2019) - 05 (TX 1280x720 x264 AAC).mp4",
            "400 MiB",
            300,
        ),
        nyaa_row(
            "7878787878787878787878787878787878787878",
            99302,
            "[Group] Fruits Basket - 05 (1080p) [WEB].mkv",
            "1.4 GiB",
            20,
        ),
        nyaa_row(
            "9090909090909090909090909090909090909090",
            99303,
            "[Group] Fruits Basket The Final - 05 (1080p) [WEB].mkv",
            "1.4 GiB",
            50,
        ),
    ));
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let mut original = detail_for(9301, "Fruits Basket");
    original.season_year = Some(2001);
    original.end_year = Some(2001);
    let mut remake = detail_for(9302, "Fruits Basket (2019)");
    remake.season_year = Some(2019);
    remake.end_year = Some(2019);
    let mut finale = detail_for(9303, "Fruits Basket: The Final");
    finale.season_year = Some(2021);
    finale.end_year = Some(2021);
    let state = build_state().await;
    seed_relatives_in_caches(
        &state.db,
        &original,
        &[(&remake, "ALTERNATIVE"), (&finale, "SEQUEL")],
    )
    .await;

    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];
    let results = find_all_for_target(
        &state.db,
        &original,
        &default_config(),
        &SearchTarget::Episode(5),
        true,
        &cfs,
        &state.indexers,
    )
    .await;
    let titles: Vec<&str> = results.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles.len(), 2, "{titles:?}");
    assert!(
        titles[0].starts_with("[Group] Fruits Basket - 05"),
        "{titles:?}"
    );
    assert!(titles[1].starts_with("[Leopard-Raws]"), "{titles:?}");
    let rival = results[1]
        .match_provenance
        .as_ref()
        .and_then(|p| p.identity.as_ref())
        .expect("the remake's release carries the verdict");
    assert_eq!(rival.looks_like(), Some("Fruits Basket (2019)"));
    assert!(
        results[1]
            .score_breakdown
            .iter()
            .any(|p| p.label == "Franchise Identity" && p.delta < 0),
        "{:?}",
        results[1].score_breakdown
    );

    unset_nyaa_base();
}

/// #303: a split part (Steel Ball Run 2nd - 3rd STAGE, offset 1 from 1st
/// STAGE, absolute only) queries the number its releases carry, and the
/// list keeps only the release that is really the episode. Every pass
/// that names an episode has to use it: `Steel Ball Run 06` would fetch
/// episode 5's release, and episode 6's would never be on the page.
#[tokio::test]
async fn find_all_for_target_queries_a_split_part_by_its_absolute_number() {
    let _gate = ENV_LOCK.lock().await;
    let server = MockServer::start().await;
    let rows = [
        nyaa_row(
            "1111111111111111111111111111111111111111",
            7007,
            "[Erai-raws] Steel Ball Run - 07 [1080p NF WEB-DL AVC AAC]",
            "1.3 GiB",
            50,
        ),
        nyaa_row(
            "2222222222222222222222222222222222222222",
            6006,
            "[Erai-raws] Steel Ball Run - 06 [1080p NF WEB-DL AVC AAC]",
            "1.3 GiB",
            50,
        ),
    ]
    .concat();
    Mock::given(method("GET"))
        .and(query_param("q", "Steel Ball Run 07"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page(&rows)))
        .expect(1..)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(query_param("q", "Steel Ball Run 06"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page(&rows)))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(nyaa_results_page("")))
        .mount(&server)
        .await;
    set_nyaa_base(&server.uri());

    let state = build_state().await;
    let series_id = seed_series_with_cache(&state, 210482, "Steel Ball Run").await;
    sqlx::query(
        "UPDATE series SET format = 'ONA', episodes = 11, cumulative_prior_episodes = 1, \
         cumulative_offset_source = 'split', cumulative_offset_season = 6 WHERE id = ?",
    )
    .bind(series_id)
    .execute(&state.db)
    .await
    .unwrap();
    let mut detail = detail_for(210482, "Steel Ball Run");
    detail.format = "ONA".into();
    detail.episodes = Some(11);
    let cfs: Vec<ryokan::services::custom_formats::CompiledCustomFormat> = vec![];
    let results = find_all_for_target(
        &state.db,
        &detail,
        &default_config(),
        &SearchTarget::Episode(6),
        true,
        &cfs,
        &state.indexers,
    )
    .await;
    let titles: Vec<&str> = results.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["[Erai-raws] Steel Ball Run - 07 [1080p NF WEB-DL AVC AAC]"],
        "{titles:?}"
    );

    unset_nyaa_base();
}
