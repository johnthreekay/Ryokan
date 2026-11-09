//! `cargo nextest run --features test-support --lib rss::tests::bench
//! --run-ignored only --no-capture` times the history load's matching:
//! about 5,000 titles through the matcher against a few hundred tracked
//! rows, each with a two-entry franchise, with and without `identify`.

use std::time::Instant;

use super::super::*;
use crate::services::identify::{FranchiseGraph, GraphEdge, GraphNode};

fn sample_titles() -> Vec<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/nyaa-titles/sample/listing-sample.json");
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("sample")).expect("json");
    value
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|r| r["title"].as_str().map(String::from))
        .collect()
}

fn row(id: i64, title: &str) -> series::Series {
    series::Series {
        is_adult: false,
        id,
        anilist_id: id,
        mal_id: None,
        title: title.to_string(),
        title_romaji: title.to_string(),
        title_english: String::new(),
        title_native: String::new(),
        cover_url: String::new(),
        format: "TV".to_string(),
        status: "FINISHED".to_string(),
        episodes: Some(12),
        season_year: Some(2020),
        end_year: Some(2020),
        folder_name: title.to_string(),
        monitor_mode: "all".to_string(),
        allow_upgrades: true,
        allow_pt_upgrades: false,
        custom_query_tokens: String::new(),
        restrict_to_uploader: String::new(),
        alternate_titles: String::new(),
        cumulative_prior_episodes: 0,
        cumulative_offset_source: String::new(),
        episode_numbering: String::new(),
        monitor_mode_manual_override: false,
        user_score: None,
        added_at: String::new(),
    }
}

#[test]
#[ignore = "timing tool"]
fn history_matching() {
    let titles = sample_titles();
    let mut names: Vec<String> = titles
        .iter()
        .filter_map(|t| release_title::parse(t).series_title)
        .collect();
    names.sort();
    names.dedup();
    names.truncate(300);
    let metas: Vec<SeriesMeta> = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let id = i as i64 + 1;
            let node = |id, romaji: String| GraphNode {
                id,
                title_romaji: romaji,
                format: "TV".to_string(),
                episodes: Some(12),
                season_year: Some(2020),
                ..GraphNode::default()
            };
            let graph = FranchiseGraph {
                nodes: vec![
                    node(id, name.clone()),
                    node(100_000 + id, format!("{name} 2nd Season")),
                ],
                edges: vec![GraphEdge {
                    from: 100_000 + id,
                    to: id,
                    relation: "PREQUEL".to_string(),
                }],
                ..FranchiseGraph::default()
            };
            SeriesMeta::from_series(&row(id, name))
                .with_franchise(graph.build(id, &[]).map(Arc::new))
        })
        .collect();
    let history: Vec<String> = titles.iter().cycle().take(5000).cloned().collect();

    let start = Instant::now();
    let mut plain = CanonicalHistory::default();
    for title in &history {
        let fields = release_title::release_fields(title);
        let pseudo = RssItem {
            title: title.clone(),
            link: String::new(),
            guid: String::new(),
            torrent: String::new(),
            magnet: String::new(),
            info_hash: String::new(),
            group: fields.group,
            resolution: fields.resolution,
            is_batch: fields.is_batch,
            source: RssSource::Nyaa,
        };
        if let Some(found) = best_series_match(&pseudo, &metas) {
            plain.insert(&found, title);
        }
    }
    let plain_time = start.elapsed();

    let start = Instant::now();
    let mut identified = CanonicalHistory::default();
    for title in &history {
        identified.insert_title(title, &metas);
    }
    let identified_time = start.elapsed();
    eprintln!(
        "{} titles x {} rows: matcher {:?}, with identify {:?} ({} / {} keys)",
        history.len(),
        metas.len(),
        plain_time,
        identified_time,
        plain.items.len(),
        identified.items.len()
    );
}
