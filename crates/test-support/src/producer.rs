//! Producer fixtures: this repository's own JSON streams under
//! `tests/fixtures/producer/`, each either producer-legal (`"verdict": "ok"`)
//! or a must-refuse case naming the [`ProducerRule`](agui_sdk::pipeline::verify::ProducerRule)
//! it breaks and the index of the event the verifier must refuse. Consumer
//! fixtures are never replayed through the producer verifier.
//!
//! ```json
//! { "description": "...", "covers": ["SUB-08"],
//!   "events": [ { "type": "RUN_STARTED", "threadId": "t", "runId": "r" } ],
//!   "verdict": { "refuse": "SubagentIdentity", "at": 3 } }
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

/// Directory of the producer fixtures, relative to the workspace root.
pub const PRODUCER_DIR: &str = "tests/fixtures/producer";

/// One producer fixture.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ProducerFixture {
    /// File stem.
    pub name: String,
    /// Path.
    pub path: PathBuf,
    /// One line: what the fixture proves.
    pub description: String,
    /// Ledger rows it covers (read by the traceability gate's report).
    pub covers: Vec<String>,
    /// Events, as JSON.
    pub events: Vec<Value>,
    /// `None` for producer-legal; otherwise the expected rule name and the
    /// index of the first refused event.
    pub refuse: Option<(String, usize)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    description: String,
    #[serde(default)]
    covers: Vec<String>,
    events: Vec<Value>,
    verdict: RawVerdict,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawVerdict {
    Ok(String),
    Refuse { refuse: String, at: usize },
}

impl ProducerFixture {
    /// Parses one fixture file.
    ///
    /// # Errors
    ///
    /// Unreadable, unknown fields, a verdict string other than `"ok"`, an
    /// empty description, or a refusal index outside `events`.
    pub fn from_path(path: &Path) -> Result<Self, String> {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        let text = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
        let raw: Raw = serde_json::from_slice(&text).map_err(|e| format!("{name}: {e}"))?;
        if raw.description.trim().is_empty() {
            return Err(format!("{name}: empty description"));
        }
        let refuse = match raw.verdict {
            RawVerdict::Ok(s) if s == "ok" => None,
            RawVerdict::Ok(s) => {
                return Err(format!(
                    "{name}: verdict {s:?} is neither \"ok\" nor a refusal"
                ))
            }
            RawVerdict::Refuse { refuse, at } => {
                if at >= raw.events.len() {
                    return Err(format!("{name}: refusal index {at} is past the last event"));
                }
                Some((refuse, at))
            }
        };
        Ok(ProducerFixture {
            name,
            path: path.to_path_buf(),
            description: raw.description,
            covers: raw.covers,
            events: raw.events,
            refuse,
        })
    }
}

/// Loads every `*.json` under `dir`, sorted by name.
///
/// # Errors
///
/// Every malformed fixture.
pub fn fixtures_in(dir: &Path) -> Result<Vec<ProducerFixture>, Vec<String>> {
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(vec![format!("{}: {e}", dir.display())]),
    };
    paths.sort();
    let mut ok = Vec::new();
    let mut problems = Vec::new();
    for path in paths {
        match ProducerFixture::from_path(&path) {
            Ok(f) => ok.push(f),
            Err(e) => problems.push(e),
        }
    }
    if problems.is_empty() {
        Ok(ok)
    } else {
        Err(problems)
    }
}

/// All producer fixtures.
///
/// # Panics
///
/// On any malformed fixture, and when there are none (a missing or emptied
/// `tests/fixtures/producer/` would otherwise pass the lane vacuously).
pub fn fixtures() -> Vec<ProducerFixture> {
    let dir = crate::workspace_root().join(PRODUCER_DIR);
    let all =
        fixtures_in(&dir).unwrap_or_else(|p| panic!("producer fixtures:\n  {}", p.join("\n  ")));
    assert!(
        !all.is_empty(),
        "no producer fixtures under {}: the producer lane would prove nothing",
        dir.display()
    );
    let refusals = all.iter().filter(|f| f.refuse.is_some()).count();
    eprintln!(
        "producer fixtures: {} ({} legal, {refusals} must-refuse)",
        all.len(),
        all.len() - refusals
    );
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_parse_and_are_checked() {
        let dir = std::env::temp_dir().join(format!("agui-ts-producer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        let write =
            |n: &str, t: &str| std::fs::write(dir.join(n), t).unwrap_or_else(|e| panic!("{e}"));
        write(
            "a-ok.json",
            r#"{"description":"d","covers":["SUB-08"],"events":[{"type":"X"}],"verdict":"ok"}"#,
        );
        write(
            "b-refuse.json",
            r#"{"description":"d","events":[{"type":"X"},{"type":"Y"}],"verdict":{"refuse":"Rule","at":1}}"#,
        );
        let all = fixtures_in(&dir).unwrap_or_else(|p| panic!("{p:?}"));
        assert_eq!(all[0].refuse, None);
        assert_eq!(all[1].refuse, Some(("Rule".to_owned(), 1)));
        write(
            "c-bad.json",
            r#"{"description":"d","events":[],"verdict":{"refuse":"Rule","at":0}}"#,
        );
        write(
            "d-bad.json",
            r#"{"description":"d","events":[],"verdict":"maybe"}"#,
        );
        assert_eq!(fixtures_in(&dir).err().map(|p| p.len()), Some(2));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
