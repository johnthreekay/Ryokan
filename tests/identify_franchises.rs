//! `services::identify` against real franchises.
//!
//! `tests/fixtures/franchises/<name>.json` holds a trimmed AniList graph
//! per franchise (entries with their titles, synonyms, format, counts and
//! years; relation edges; TVDB seasons from anibridge; curated offsets
//! from anime-relations), vendored by the ignored `fetch_franchise_fixtures`.
//! The table below is real release titles (the AnimeTosho corpus unless a
//! row says otherwise) and the verdict each should get; it is the
//! acceptance test for the rule.

use std::path::PathBuf;

use ryokan::services::identify::{self, FranchiseGraph, Tier, Verdict};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct Fixture {
    name: String,
    roots: Vec<i64>,
    #[serde(flatten)]
    graph: FranchiseGraph,
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/franchises")
}

fn load(name: &str) -> FranchiseGraph {
    let path = fixture_dir().join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    let fixture: Fixture = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    fixture.graph
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    Target,
    Rival(Tier, i64),
    Spans,
    Ambiguous(i64),
    Neither,
}

fn matches(verdict: &Verdict, expect: Expect) -> bool {
    match (verdict, expect) {
        (Verdict::Target, Expect::Target)
        | (Verdict::Spans, Expect::Spans)
        | (Verdict::Neither, Expect::Neither) => true,
        (Verdict::Rival(r), Expect::Rival(tier, entry)) => r.tier == tier && r.entry == entry,
        (Verdict::Ambiguous { entry, .. }, Expect::Ambiguous(e)) => *entry == e,
        _ => false,
    }
}

use Expect::{Ambiguous, Neither, Rival, Spans, Target};
use Tier::{Marker, NamesFully, NamesMore, Structure};

/// `(franchise, target AniList id, title, expected)`. Titles are from
/// the AnimeTosho corpus unless the row's comment says otherwise.
const CASES: &[(&str, i64, &str, Expect)] = &[
    // #300: the franchise title alone is the 2012 series' name.
    // (The issue's title.)
    (
        "jojo",
        210482,
        "[RH] JoJo no Kimyou na Bouken - 01-03 [1080p]",
        Rival(NamesFully, 14719),
    ),
    // TVDB's season 6 and the TVDB show's first year are Steel Ball Run's.
    (
        "jojo",
        210482,
        "JoJos Bizarre Adventure (2012) S06E04 1080p NF WEB-DL MULTi AAC2.0 AV1 (Multi-Audio / Dual-Audio, Multi-Subs) (JoJo no Kimyou na Bouken: Steel Ball Run) (Weekly) [Trix]",
        Target,
    ),
    (
        "jojo",
        210482,
        "JoJos Bizarre Adventure S06E03 The Desert-Born Outlaws 1080p NF WEB-DL DUAL AAC2.0 H.264-VARYG (JoJo no Kimyou na Bouken Part 5: Ougon no Kaze, Dual-Audio, Multi-Subs)",
        Target,
    ),
    // The sequel's subtitle names more than the first season's name.
    // (Constructed: SubsPlease's shape for the sequel.)
    (
        "jujutsu-kaisen",
        113415,
        "[SubsPlease] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06 (1080p) [ABCD1234].mkv",
        Rival(NamesMore, 172463),
    ),
    (
        "jujutsu-kaisen",
        113415,
        "JUJUTSU KAISEN S03E08 Tokyo Colony No 1-Part 2 1080p CR WEB-DL AAC2.0 H 264-VARYG (Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen, Multi-Subs)",
        Rival(Structure, 172463),
    ),
    // The gate's old sibling-overlap cases, now identify's.
    (
        "jujutsu-kaisen",
        113415,
        "[Erai-raws] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06 [1080p CR WEBRip HEVC AAC].mkv",
        Rival(NamesMore, 172463),
    ),
    (
        "jujutsu-kaisen",
        113415,
        "[Erai-raws] Jujutsu Kaisen - 06 [1080p].mkv",
        Target,
    ),
    (
        "jujutsu-kaisen",
        172463,
        "[Erai-raws] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06 [1080p].mkv",
        Target,
    ),
    // 24 + 23 episodes before it: 56 is the third season's ninth.
    (
        "jujutsu-kaisen",
        172463,
        "[SubsPlease] Jujutsu Kaisen - 56 (1080p) [0F106B43].mkv",
        Target,
    ),
    // A `Part 2` in the episode's own title is no claim about the show
    // (the third season's English name says Part 1). (Constructed.)
    (
        "jujutsu-kaisen",
        172463,
        "[Group] Jujutsu Kaisen - 51 - Tokyo Colony Part 2 (1080p)",
        Target,
    ),
    // The third Kizumonogatari film, numbered as a part.
    (
        "monogatari",
        9260,
        "(HI10)_Kizumonogatari_-_03_(BD_1080p)_(ReinForce-Tactical).mkv",
        Rival(Structure, 21400),
    ),
    (
        "monogatari",
        21400,
        "(HI10)_Kizumonogatari_-_03_(BD_1080p)_(ReinForce-Tactical).mkv",
        Target,
    ),
    // The Final Chapters' first special, in absolute numbering.
    (
        "shingeki-no-kyojin",
        146984,
        "[Anime Chap] Shingeki no Kyojin (Attack on Titan) The Final Season - Part 3 - Episode 88 [WEB 1080p] OP Lyrics (S04E88)",
        Target,
    ),
    // The year is the remake's.
    (
        "fruits-basket",
        120,
        "[Leopard-Raws] Fruits Basket (2019) - 25 END (TX 1280x720 x264 AAC).mp4",
        Rival(Structure, 105334),
    ),
    // Same words, both long enough, but the name is the 1999 series'
    // exactly (the 2011 one is `HUNTER×HUNTER (2011)`): Sonarr's reading.
    // An alternate title on the 2011 series restores the tie.
    (
        "hunter-x-hunter",
        11061,
        "[DP] Hunter x Hunter - 003 [Multi Subs][Tri Audio][BDRip][1080p][Hi10][FLAC/EAC3]",
        Rival(NamesFully, 136),
    ),
    (
        "hunter-x-hunter",
        136,
        "[DP] Hunter x Hunter - 003 [Multi Subs][Tri Audio][BDRip][1080p][Hi10][FLAC/EAC3]",
        Target,
    ),
    // The first two seasons' names differ by a question mark, and the
    // third's (`Kaguya-sama wa Kokurasetai: Ultra Romantic`) leaves the
    // fewest words unsaid, so it is the closest of the ties.
    // (Constructed: HorribleSubs' shape.)
    (
        "kaguya-sama",
        112641,
        "[HorribleSubs] Kaguya-sama wa Kokurasetai - 05 [1080p].mkv",
        Ambiguous(125367),
    ),
    (
        "kaguya-sama",
        101921,
        "[HorribleSubs] Kaguya-sama wa Kokurasetai - 05 [1080p].mkv",
        Ambiguous(125367),
    ),
    // Part 2 is named only by its marker, which the title doesn't state.
    (
        "spy-x-family",
        142838,
        "[GJM-Kaleido] Spy x Family - 05 (WEB 1080p) [EFCDA5AA].mkv",
        Rival(Marker, 140960),
    ),
    // Another show sharing a word.
    (
        "mushoku-tensei",
        108465,
        "[SubsPlease] Mushoku no Eiyuu - 01 (480p) [31FFF7DB].mkv",
        Neither,
    ),
    // Packs across seasons, written in CJK and Latin.
    (
        "kaguya-sama",
        101921,
        "[Furretar] 中台粤配 - 辉夜姬想让人告白 天才们的恋爱头脑战 (Kaguya-sama wa Kokurasetai) (Season 1-3) (Mandarin Chinese Mainland + Taiwanese + Cantonese Dub) (Matching Soft Subs)",
        Spans,
    ),
    (
        "spy-x-family",
        140960,
        "[Furretar] 中台粤配 - 间谍过家家 (Spy Family) (Season 1-2 + Movie) (Mainland + Taiwanese Mandarin + Cantonese Chinese Dub) (Matching Soft Subs) [國語][粵語][国语][中配版] (間諜家家酒)",
        Spans,
    ),
    // TVDB seasons name later entries.
    (
        "oshi-no-ko",
        182587,
        "OSHI NO KO S03E01 Down Bad 1080p CR WEB-DL DUAL AAC2.0 H 264-VARYG ([Oshi no Ko], Dual-Audio, Multi-Subs)",
        Target,
    ),
    (
        "oshi-no-ko",
        150672,
        "OSHI NO KO S03E01 Down Bad 1080p CR WEB-DL DUAL AAC2.0 H 264-VARYG ([Oshi no Ko], Dual-Audio, Multi-Subs)",
        Rival(Structure, 182587),
    ),
    (
        "dr-stone",
        172019,
        "[ASW] Dr. Stone S4 - 07 [1080p HEVC x265 10Bit][AAC]",
        Target,
    ),
    (
        "dr-stone",
        105333,
        "[ASW] Dr. Stone S4 - 07 [1080p HEVC x265 10Bit][AAC]",
        Rival(Structure, 172019),
    ),
    (
        "mushoku-tensei",
        178789,
        "Mushoku Tensei Jobless Reincarnation S03E03 MULTi AD 1080p CR WEB-DL AAC2.0 H.264-Tsundere-Raws",
        Target,
    ),
    (
        "mushoku-tensei",
        108465,
        "Mushoku Tensei Jobless Reincarnation S03E03 MULTi AD 1080p CR WEB-DL AAC2.0 H.264-Tsundere-Raws",
        Rival(Structure, 178789),
    ),
    (
        "kimetsu-no-yaiba",
        101922,
        "[Leopard-Raws] Kimetsu no Yaiba - 25 RAW (BS11 1280x720 x264 AAC).mp4",
        Target,
    ),
    (
        "kimetsu-no-yaiba",
        142329,
        "[Leopard-Raws] Kimetsu no Yaiba - 25 RAW (BS11 1280x720 x264 AAC).mp4",
        Rival(Structure, 101922),
    ),
    (
        "fate-zero",
        10087,
        "Fate/Zero.S02.1080p.Blu-Ray.10-Bit.Dual-Audio.LPCM.x265-iAHD",
        Rival(Structure, 11741),
    ),
];

#[test]
fn real_titles_identify_against_real_franchises() {
    let mut failures = Vec::new();
    for &(name, target, title, expect) in CASES {
        let franchise = load(name)
            .build(target, &[])
            .unwrap_or_else(|| panic!("{name}: {target} is no entry"));
        let verdict = identify::identify(&franchise, title);
        if !matches(&verdict, expect) {
            failures.push(format!(
                "{name} {target}: {title}\n    expected {expect:?}\n    got      {verdict:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// The franchises and the AniList entries their walks start from.
const FRANCHISES: &[(&str, &[i64])] = &[
    ("jojo", &[14719]),
    ("kimetsu-no-yaiba", &[101922]),
    ("jujutsu-kaisen", &[113415]),
    ("monogatari", &[9260, 5081]),
    ("shingeki-no-kyojin", &[16498]),
    ("mushoku-tensei", &[108465]),
    ("kaguya-sama", &[101921]),
    ("dr-stone", &[105333]),
    ("fate-zero", &[10087]),
    ("spy-x-family", &[140960]),
    ("fruits-basket", &[120, 105334]),
    ("hunter-x-hunter", &[136, 11061]),
    ("oshi-no-ko", &[150672]),
];

/// Fetches the fixtures from AniList and anibridge. One GraphQL request
/// per walk level (paced 2.5 s apart); the anibridge mappings are cached
/// under `ID_CACHE` (default `target/identify-cache`, never a RAM-backed
/// `/tmp`). `ID_ONLY=name` refetches one franchise. It runs for minutes,
/// past nextest's slow-test limit, so run it with `cargo test`:
/// `cargo test --features test-support --test identify_franchises -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "fixture tool: fetches AniList and anibridge; ID_CACHE, ID_ONLY"]
async fn fetch_franchise_fixtures() {
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
    let only = std::env::var("ID_ONLY").ok();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("client");
    std::fs::create_dir_all(fixture_dir()).expect("fixture dir");
    for (name, roots) in FRANCHISES {
        if only.as_deref().is_some_and(|o| o != *name) {
            continue;
        }
        let graph = fetch::walk(&client, roots).await;
        let fixture = Fixture {
            name: name.to_string(),
            roots: roots.to_vec(),
            graph,
        };
        let path = fixture_dir().join(format!("{name}.json"));
        let mut text = serde_json::to_string_pretty(&fixture).expect("serialize");
        text.push('\n');
        std::fs::write(&path, text).expect("write fixture");
        println!("{name}: {} entries", fixture.graph.nodes.len());
    }
}

#[path = "common/anilist_graph.rs"]
mod fetch;
