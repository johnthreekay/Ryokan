//! `services::identify` over the AnimeTosho corpus
//! (`tests/fixtures/animetosho-corpus/`), whose titles AnimeTosho labels
//! with an AniDB anime.
//!
//! `franchises.json.gz` beside the corpus holds what the tests need with
//! no network: `labels`, each corpus AniDB anime as the set of AniList
//! entries anibridge pairs it with (AniDB and AniList split shows
//! differently, so a label is a set), and `graph`, one AniList graph
//! holding every labeled entry's franchise the way the metadata sweep
//! would cache it. `GraphIndex::around` cuts one entry's franchise out of
//! it with the walk `identify::load_graph` makes. Both are vendored by the
//! ignored `vendor_corpus_franchises`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read};
use std::path::PathBuf;

use ryokan::services::identify::FranchiseGraph;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[path = "common/anilist_graph.rs"]
mod fetch;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Vendored {
    /// AniDB anime id → AniList entries, ascending.
    labels: BTreeMap<i64, Vec<i64>>,
    graph: FranchiseGraph,
}

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/animetosho-corpus")
}

fn vendored() -> Vendored {
    let path = corpus_dir().join("franchises.json.gz");
    let file =
        std::fs::File::open(&path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let mut text = String::new();
    flate2::read::GzDecoder::new(file)
        .read_to_string(&mut text)
        .expect("read franchises");
    serde_json::from_str(&text).expect("franchises JSON")
}

/// Every corpus title that carries an AniDB anime, with that anime.
fn labeled_titles() -> Vec<(String, i64)> {
    let path = corpus_dir().join("titles.jsonl.gz");
    let file =
        std::fs::File::open(&path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    std::io::BufReader::new(flate2::read::GzDecoder::new(file))
        .lines()
        .filter_map(|line| {
            let row: Value = serde_json::from_str(&line.expect("read line")).expect("JSON line");
            Some((
                row["title"].as_str()?.to_string(),
                row["anidb_aid"].as_i64()?,
            ))
        })
        .collect()
}

#[test]
fn vendored_franchises_are_well_formed() {
    let vendored = vendored();
    let titles = labeled_titles();
    let aids: BTreeSet<i64> = titles.iter().map(|(_, aid)| *aid).collect();
    // Every labeled anime anibridge maps, and nothing else.
    for aid in vendored.labels.keys() {
        assert!(
            aids.contains(aid),
            "label for AniDB {aid}, not in the corpus"
        );
    }
    let nodes: BTreeSet<i64> = vendored.graph.nodes.iter().map(|n| n.id).collect();
    let mut missing: Vec<i64> = Vec::new();
    for (aid, entries) in &vendored.labels {
        assert!(!entries.is_empty(), "AniDB {aid}: empty label");
        assert!(
            entries.windows(2).all(|w| w[0] < w[1]),
            "AniDB {aid}: unsorted"
        );
        missing.extend(entries.iter().filter(|id| !nodes.contains(id)).copied());
    }
    missing.sort_unstable();
    missing.dedup();
    for edge in &vendored.graph.edges {
        assert!(
            nodes.contains(&edge.from) && nodes.contains(&edge.to),
            "edge {edge:?} leaves the graph"
        );
    }
    // AniList no longer serves a handful of the entries anibridge names.
    assert!(
        missing.len() <= 20,
        "{} labeled entries missing: {missing:?}",
        missing.len()
    );
    let labeled = titles
        .iter()
        .filter(|(_, aid)| vendored.labels.contains_key(aid))
        .count();
    assert!(labeled >= 24_000, "{labeled} labeled titles");
}

/// Vendors `franchises.json.gz`: loads anibridge's mappings (cached under
/// `ID_CACHE`, default `target/identify-cache`), labels every corpus AniDB
/// anime with its AniList entries, then fetches from AniList, level by
/// level for all of them at once, every entry each one's franchise walk
/// reaches on the chain, until no walk wants more. About ten minutes at
/// one request per 2.5 s; run it with `cargo test --release`:
/// `cargo test --release --features test-support --test identify_corpora -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "fixture tool: fetches anibridge and AniList; ID_CACHE"]
async fn vendor_corpus_franchises() {
    let cache = std::env::var("ID_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/identify-cache")
        });
    std::fs::create_dir_all(&cache).expect("cache dir");
    // SAFETY: set before anything reads the environment on other threads.
    unsafe { std::env::set_var("RYOKAN_ANIBRIDGE_CACHE_DIR", &cache) };
    assert!(
        ryokan::services::anibridge::ensure_loaded().await,
        "anibridge mappings did not load"
    );

    let aids: BTreeSet<i64> = labeled_titles().into_iter().map(|(_, aid)| aid).collect();
    let mut labels = BTreeMap::new();
    for aid in aids {
        let entries = ryokan::services::anibridge::anilist_ids_for_anidb(aid).await;
        if !entries.is_empty() {
            labels.insert(aid, entries);
        }
    }
    let roots: BTreeSet<i64> = labels.values().flatten().copied().collect();
    println!(
        "{} labeled anime, {} AniList entries",
        labels.len(),
        roots.len()
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("client");
    let mut collector = fetch::Collector::default();
    let mut pending: Vec<i64> = roots.iter().copied().collect();
    while !pending.is_empty() {
        collector.fetch(&client, &pending).await;
        let graph = collector.snapshot();
        let index = graph.index();
        let mut wanted = BTreeSet::new();
        for root in &roots {
            let Some(franchise) = index.franchise(*root) else {
                continue;
            };
            wanted.extend(
                franchise
                    .entries()
                    .iter()
                    .filter(|e| e.on_chain && !collector.is_fetched(e.id))
                    .map(|e| e.id),
            );
        }
        pending = wanted.into_iter().collect();
        println!(
            "{} requests, {} entries; {} more to fetch",
            collector.requests(),
            graph.nodes.len(),
            pending.len()
        );
    }
    let vendored = Vendored {
        labels,
        graph: collector.finish().await,
    };
    let path = corpus_dir().join("franchises.json.gz");
    let file = std::fs::File::create(&path).expect("create franchises");
    let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::best());
    serde_json::to_writer(&mut gz, &vendored).expect("write franchises");
    gz.finish().expect("finish gzip");
    println!(
        "{}: {} entries, {} edges",
        path.display(),
        vendored.graph.nodes.len(),
        vendored.graph.edges.len()
    );
}
