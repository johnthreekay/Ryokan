//! Corpus dumps for checking a classification change (the phase 3
//! anitomy benchmark lived here until the crate was dropped in phase
//! 12e).

use super::corpus::{sample_titles, verified_titles};
use crate::services::source_filename::classify_filename;

fn all_titles() -> Vec<String> {
    let mut titles: Vec<String> = verified_titles().into_iter().collect();
    titles.extend(sample_titles());
    titles.sort();
    titles.dedup();
    titles
}

/// `cargo t release_title::bench::classify_dump --run-ignored only
/// --no-capture` prints `classify_filename`'s answer for every corpus
/// title as JSON lines, for diffing a source-classification change.
#[test]
#[ignore]
fn classify_dump() {
    for title in all_titles() {
        let fc = classify_filename(&title);
        let mut evidence: Vec<String> = fc
            .evidence
            .iter()
            .map(|e| format!("{:?}:{:.2}:{}", e.source, e.confidence, e.detail))
            .collect();
        evidence.sort();
        eprintln!(
            "{}",
            serde_json::json!({
                "title": title,
                "evidence": evidence,
                "resolution": format!("{:?}", fc.resolution),
                "remux": fc.is_remux,
                "bdmv": fc.is_bdmv,
                "web": format!("{:?}", fc.web_kind),
                "group": fc.release_group,
            })
        );
    }
}
