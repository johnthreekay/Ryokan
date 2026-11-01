//! Fetches trimmed AniList relation graphs for the identify fixture
//! tools (`tests/identify_franchises.rs`, `tests/identify_corpora.rs`):
//! entries with their titles, synonyms, format, counts and years;
//! relation edges of the kinds `services::identify` reads; TVDB seasons
//! from anibridge (which must be loaded); curated offsets from
//! anime-relations. One GraphQL request per 50 entries, paced 2.5 s
//! apart.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::time::Duration;

use ryokan::services::identify::{
    FranchiseGraph, GraphEdge, GraphNode, OffsetRow, Relation, TvdbRow,
};
use ryokan::services::{anibridge, anilist, anime_relations};
use serde_json::Value;

/// Entries on the main chain a franchise walk takes at most.
const MAX_CHAIN: usize = 40;

const FIELDS: &str = "id idMal type title { romaji english native } synonyms format episodes \
                      seasonYear startDate { year } endDate { year }";

/// Entries and edges fetched so far, each entry's relations fetched once.
#[derive(Default)]
pub struct Collector {
    nodes: BTreeMap<i64, GraphNode>,
    edges: Vec<GraphEdge>,
    fetched: HashSet<i64>,
    requests: usize,
}

impl Collector {
    /// Fetches the entries of `ids` not fetched yet with their relations,
    /// and returns the chain edges found as `(from, to)`.
    pub async fn fetch(&mut self, client: &reqwest::Client, ids: &[i64]) -> Vec<(i64, i64)> {
        let ids: Vec<i64> = ids
            .iter()
            .copied()
            .filter(|id| self.fetched.insert(*id))
            .collect();
        let mut chain = Vec::new();
        for chunk in ids.chunks(50) {
            self.requests += 1;
            for media in page(client, chunk).await {
                let node = to_node(&media);
                let from = node.id;
                self.nodes.insert(from, node);
                for edge in media["relations"]["edges"].as_array().into_iter().flatten() {
                    let related = &edge["node"];
                    if related["type"].as_str() != Some("ANIME") {
                        continue;
                    }
                    let relation_raw = edge["relationType"].as_str().unwrap_or_default();
                    let relation = Relation::parse(relation_raw);
                    if !(relation.is_chain() || relation.is_side()) {
                        continue;
                    }
                    let to = related["id"].as_i64().expect("related id");
                    self.edges.push(GraphEdge {
                        from,
                        to,
                        relation: relation_raw.to_string(),
                    });
                    self.nodes.entry(to).or_insert_with(|| to_node(related));
                    if relation.is_chain() {
                        chain.push((from, to));
                    }
                }
            }
        }
        chain
    }

    pub fn is_fetched(&self, id: i64) -> bool {
        self.fetched.contains(&id)
    }

    pub fn requests(&self) -> usize {
        self.requests
    }

    /// The graph so far, with no TVDB rows or offsets.
    pub fn snapshot(&self) -> FranchiseGraph {
        let mut edges = self.edges.clone();
        edges.retain(|e| self.nodes.contains_key(&e.from) && self.nodes.contains_key(&e.to));
        edges.sort_by(|a, b| (a.from, a.to, &a.relation).cmp(&(b.from, b.to, &b.relation)));
        edges.dedup();
        FranchiseGraph {
            nodes: self.nodes.values().cloned().collect(),
            edges,
            ..FranchiseGraph::default()
        }
    }

    /// The graph, with TVDB rows and curated offsets for every entry.
    pub async fn finish(self) -> FranchiseGraph {
        let mut graph = self.snapshot();
        for node in &graph.nodes {
            for placement in anibridge::tvdb_placements(node.id).await {
                graph.tvdb.push(TvdbRow {
                    id: node.id,
                    tvdb_id: placement.tvdb_id,
                    season: placement.season,
                    episode_offset: placement.episode_offset,
                });
            }
            if let Some(rule) = anime_relations::offset_for(node.id, node.mal_id) {
                graph.offsets.push(OffsetRow {
                    id: node.id,
                    episodes: rule.episodes,
                });
            }
        }
        graph
    }
}

/// One franchise's graph: chain edges either way from `roots`, up to
/// `MAX_CHAIN` entries, and every relation of the entries walked.
pub async fn walk(client: &reqwest::Client, roots: &[i64]) -> FranchiseGraph {
    let mut collector = Collector::default();
    let mut chain: HashSet<i64> = roots.iter().copied().collect();
    let mut level: VecDeque<i64> = roots.iter().copied().collect();
    while !level.is_empty() {
        let ids: Vec<i64> = level
            .drain(..)
            .filter(|id| !collector.is_fetched(*id))
            .collect();
        if ids.is_empty() {
            break;
        }
        for (_, to) in collector.fetch(client, &ids).await {
            if chain.len() < MAX_CHAIN && chain.insert(to) {
                level.push_back(to);
            }
        }
    }
    collector.finish().await
}

async fn page(client: &reqwest::Client, ids: &[i64]) -> Vec<Value> {
    let query = format!(
        "query ($ids: [Int]) {{ Page(perPage: 50) {{ media(id_in: $ids, type: ANIME) {{ {FIELDS} \
         relations {{ edges {{ relationType node {{ {FIELDS} }} }} }} }} }} }}"
    );
    let body = serde_json::json!({ "query": query, "variables": { "ids": ids } });
    let mut attempt = 0;
    let value: Value = loop {
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let resp = anilist::anilist_post(client)
            .json(&body)
            .send()
            .await
            .expect("AniList request");
        if resp.status().as_u16() == 429 && attempt < 5 {
            attempt += 1;
            let wait = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(60);
            tokio::time::sleep(Duration::from_secs(wait + 1)).await;
            continue;
        }
        assert!(resp.status().is_success(), "AniList {}", resp.status());
        break resp.json().await.expect("AniList json");
    };
    value["data"]["Page"]["media"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn to_node(media: &Value) -> GraphNode {
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let year = |v: &Value| v.as_i64().map(|y| y as i32);
    GraphNode {
        id: media["id"].as_i64().expect("media id"),
        mal_id: media["idMal"].as_i64(),
        title_romaji: s(&media["title"]["romaji"]),
        title_english: s(&media["title"]["english"]),
        title_native: s(&media["title"]["native"]),
        synonyms: media["synonyms"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        format: s(&media["format"]),
        episodes: media["episodes"].as_i64().map(|n| n as i32),
        season_year: year(&media["seasonYear"]).or(year(&media["startDate"]["year"])),
        end_year: year(&media["endDate"]["year"]),
    }
}
