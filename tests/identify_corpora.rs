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
//!
//! `identify_over_the_corpus_holds_its_numbers` judges every labeled
//! title as each entry the title names would (its label's entries, and
//! the main-chain relatives of their franchises) and holds the totals as
//! floors and ceilings, with a snapshot of every missed, wrong and tied
//! title. The labels are AniDB's grain, not AniList's (`Chitose-kun wa
//! Ramune Bin no Naka - 14` is labeled the first cour; anibridge has no
//! AniList entry for the second in that AniDB anime) and AnimeTosho's
//! matching is sometimes wrong, so the numbers are a baseline to move
//! from, not a score. `probe` (ignored, `ID_PROBE='<id>|<title>'`) prints
//! one entry's franchise and its verdict on one title.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, Read};
use std::path::PathBuf;

use ryokan::services::identify::{self, Franchise, FranchiseGraph, GraphIndex, Verdict};
use ryokan::services::{auto_search, release_title};
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

/// What `identify` made of one labeled title, as each entry would judge
/// it were it the library series: the label's own entries, then the
/// relatives on their franchises' main chains. Only entries the title
/// names are judged: the automatic gate takes a release only when it
/// names the series verbatim (one of its titles, normalized, inside the
/// normalized release title), and `identify` runs after that.
struct Judged {
    title: String,
    label: Vec<i64>,
    own: Vec<(i64, Verdict)>,
    relatives: Vec<(i64, Verdict)>,
}

impl Judged {
    /// A label entry the title names, and every one of them vetoes it
    /// for an entry outside the label. (A veto for another label entry is
    /// right: `Jujutsu Kaisen - 55` is the third season's, which the
    /// AniDB anime covers too.)
    fn missed(&self) -> bool {
        !self.own.is_empty()
            && self.own.iter().all(|(_, v)| match v {
                Verdict::Rival(r) => !self.label.contains(&r.entry),
                _ => false,
            })
    }

    /// A label entry takes the title as its own.
    fn recognized(&self) -> bool {
        self.own.iter().any(|(_, v)| *v == Verdict::Target)
    }

    /// A relative the title names takes it as its own: a wrong-entry
    /// accept.
    fn wrong_target(&self) -> bool {
        self.relatives.iter().any(|(_, v)| *v == Verdict::Target)
    }

    /// A relative the title names ties over it (accepted at a small
    /// score cost).
    fn wrong_tie(&self) -> bool {
        self.relatives
            .iter()
            .any(|(_, v)| matches!(v, Verdict::Ambiguous { .. }))
    }

    /// Every relative the title names vetoes it: the name gate alone
    /// would have taken it for the wrong entry.
    fn blocked(&self) -> bool {
        !self.relatives.is_empty() && self.relatives.iter().all(|(_, v)| v.is_veto())
    }
}

fn short(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Target => "target".to_string(),
        Verdict::Rival(r) => format!("rival {} ({})", r.entry, r.tier.as_str()),
        Verdict::Spans => "spans".to_string(),
        Verdict::Ambiguous { entry, .. } => format!("tie {entry}"),
        Verdict::Neither => "neither".to_string(),
    }
}

/// `work` on every item, on every core: each thread takes the next item
/// off a shared queue (a few franchises hold most of the work) and keeps
/// its own index of `graph` and its own `state`. Results in `items`' order.
fn in_parallel<T: Send, R: Send, S: Default>(
    graph: &FranchiseGraph,
    items: Vec<T>,
    work: impl Fn(&GraphIndex<'_>, &mut S, T) -> R + Sync,
) -> Vec<R> {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let items: Vec<Mutex<Option<T>>> = items.into_iter().map(|t| Mutex::new(Some(t))).collect();
    let results: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let index = graph.index();
                let mut state = S::default();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(slot) = items.get(i) else {
                        break;
                    };
                    let item = slot.lock().unwrap().take().expect("taken once");
                    *results[i].lock().unwrap() = Some(work(&index, &mut state, item));
                }
            });
        }
    });
    results
        .into_iter()
        .map(|r| r.into_inner().unwrap().expect("every item worked"))
        .collect()
}

/// One labeled anime: its label, the main-chain relatives of its label
/// entries, and the smallest id among them (the anime of one franchise
/// sort together).
struct Group {
    key: i64,
    label: Vec<i64>,
    relatives: Vec<i64>,
    titles: Vec<String>,
}

/// Every labeled title judged. Franchises are built once per entry and
/// kept while the titles of one franchise run.
fn judge_corpus() -> Vec<Judged> {
    let vendored = vendored();
    // Each entry's titles as the gate compares them.
    let names: HashMap<i64, Vec<String>> = vendored
        .graph
        .nodes
        .iter()
        .map(|n| {
            let mut forms: Vec<String> = [&n.title_romaji, &n.title_english]
                .into_iter()
                .chain(&n.synonyms)
                .map(|t| auto_search::normalize_title(t))
                .filter(|t| t.chars().filter(|c| c.is_alphanumeric()).count() >= 3)
                .collect();
            forms.sort();
            forms.dedup();
            (n.id, forms)
        })
        .collect();
    let mut by_aid: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for (title, aid) in labeled_titles() {
        if vendored.labels.contains_key(&aid) {
            by_aid.entry(aid).or_default().push(title);
        }
    }
    let mut groups = in_parallel(
        &vendored.graph,
        by_aid.into_iter().collect(),
        |index, _: &mut (), (aid, titles): (i64, Vec<String>)| {
            let label = vendored.labels[&aid].clone();
            let mut chain: BTreeSet<i64> = BTreeSet::new();
            for id in &label {
                if let Some(franchise) = index.franchise(*id) {
                    chain.extend(
                        franchise
                            .entries()
                            .iter()
                            .filter(|e| e.on_chain)
                            .map(|e| e.id),
                    );
                }
            }
            Group {
                key: chain.first().copied().unwrap_or(label[0]),
                relatives: chain.into_iter().filter(|id| !label.contains(id)).collect(),
                label,
                titles,
            }
        },
    );
    groups.sort_by(|a, b| (a.key, &a.label).cmp(&(b.key, &b.label)));
    // The long-runners (Detective Conan, Pokémon, One Piece) have
    // hundreds of titles and franchises of a hundred entries: split
    // every anime's titles so the threads share them.
    let groups: Vec<Group> = groups
        .into_iter()
        .flat_map(|group| {
            group
                .titles
                .chunks(16)
                .map(|titles| Group {
                    key: group.key,
                    label: group.label.clone(),
                    relatives: group.relatives.clone(),
                    titles: titles.to_vec(),
                })
                .collect::<Vec<_>>()
        })
        .collect();

    let judged: Vec<Vec<Judged>> = in_parallel(
        &vendored.graph,
        groups,
        |index, cache: &mut (Option<i64>, HashMap<i64, Option<Franchise>>), group: Group| {
            if cache.0 != Some(group.key) {
                cache.1.clear();
                cache.0 = Some(group.key);
            }
            let franchises = &mut cache.1;
            for id in group.label.iter().chain(&group.relatives) {
                franchises
                    .entry(*id)
                    .or_insert_with(|| index.franchise(*id));
            }
            let judge = |ids: &[i64], normalized: &str, prepared: &identify::PreparedTitle| {
                ids.iter()
                    .filter(|id| {
                        names.get(id).is_some_and(|forms| {
                            forms.iter().any(|f| normalized.contains(f.as_str()))
                        })
                    })
                    .filter_map(|id| {
                        let franchise = franchises.get(id)?.as_ref()?;
                        Some((*id, identify::identify_prepared(franchise, prepared)))
                    })
                    .collect::<Vec<_>>()
            };
            group
                .titles
                .into_iter()
                .map(|title| {
                    let prepared = identify::PreparedTitle::of(&release_title::parse(&title));
                    let normalized = auto_search::normalize_title(&title);
                    Judged {
                        own: judge(&group.label, &normalized, &prepared),
                        relatives: judge(&group.relatives, &normalized, &prepared),
                        label: group.label.clone(),
                        title,
                    }
                })
                .collect()
        },
    );
    judged.into_iter().flatten().collect()
}

fn verdicts(v: &[(i64, Verdict)]) -> String {
    v.iter()
        .map(|(id, v)| format!("{id}: {}", short(v)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The corpus through `identify`, held to its numbers. When a change
/// moves one: if it improved, tighten the limit; if it is a deliberate
/// trade, loosen it and say why in the commit. The snapshot lists every
/// missed, wrong and tied title, grouped by what happened, so a moved
/// number shows the titles that moved.
#[test]
fn identify_over_the_corpus_holds_its_numbers() {
    const RECOGNIZED_MIN: usize = 16_121;
    const MISSED_MAX: usize = 1_216;
    const BLOCKED_MIN: usize = 6_382;
    const WRONG_TARGET_MAX: usize = 1_326;
    const WRONG_TIE_MAX: usize = 337;

    let judged = judge_corpus();
    let count = |f: fn(&Judged) -> bool| judged.iter().filter(|j| f(j)).count();
    let (recognized, missed, blocked, wrong_target, wrong_tie) = (
        count(Judged::recognized),
        count(Judged::missed),
        count(Judged::blocked),
        count(Judged::wrong_target),
        count(Judged::wrong_tie),
    );
    eprintln!(
        "{} titles, {} naming a label entry: recognized {recognized}, missed {missed}; \
         naming a relative: blocked {blocked}, a relative's target {wrong_target}, tie {wrong_tie} \
         (accepted for a relative either way: {})",
        judged.len(),
        count(|j| !j.own.is_empty()),
        count(|j| j.wrong_target() || j.wrong_tie()),
    );

    let mut groups: BTreeMap<String, (usize, &str)> = BTreeMap::new();
    for j in &judged {
        for (kind, hit) in [
            ("missed", j.missed()),
            ("wrong", j.wrong_target()),
            ("tie", j.wrong_tie()),
        ] {
            if hit {
                let key = format!(
                    "{kind} {:?} [{}] relatives [{}]",
                    j.label,
                    verdicts(&j.own),
                    verdicts(&j.relatives)
                );
                groups.entry(key).or_insert((0, &j.title)).0 += 1;
            }
        }
    }
    let lines: Vec<String> = groups
        .iter()
        .map(|(key, (n, title))| format!("{key} x{n}\n    {title}"))
        .collect();
    insta::assert_snapshot!("identify_corpus", lines.join("\n"));

    let mut failures = Vec::new();
    for (name, value, limit, floor) in [
        ("recognized", recognized, RECOGNIZED_MIN, true),
        ("missed", missed, MISSED_MAX, false),
        ("blocked", blocked, BLOCKED_MIN, true),
        ("a relative's target", wrong_target, WRONG_TARGET_MAX, false),
        ("a relative's tie", wrong_tie, WRONG_TIE_MAX, false),
    ] {
        if floor && value < limit {
            failures.push(format!("{name}: {value}, floor {limit}"));
        }
        if !floor && value > limit {
            failures.push(format!("{name}: {value}, ceiling {limit}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
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

#[test]
#[ignore = "probe: ID_PROBE='<anilist id>|<title>'"]
fn probe() {
    let probe = std::env::var("ID_PROBE").expect("ID_PROBE");
    let (id, title) = probe.split_once('|').expect("id|title");
    let id: i64 = id.parse().expect("id");
    let vendored = vendored();
    let index = vendored.graph.index();
    let franchise = index.franchise(id).expect("entry");
    for e in franchise.entries() {
        eprintln!(
            "  {} {:?} {} {:?} chain={}",
            e.id, e.title, e.format, e.episodes, e.on_chain
        );
    }
    eprintln!("{:?}", identify::identify(&franchise, title));
}
