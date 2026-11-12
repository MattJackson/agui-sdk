//! The traceability gate.
//!
//! Tests declare the ledger rows they prove with a doc line directly above
//! the test function:
//!
//! ```text
//! /// covers: LIFE-41, STRM-12
//! #[test]
//! fn late_run_error_after_finish_is_accepted() { ... }
//! ```
//!
//! In a file shared by both sides (see [`SHARED_FILES`]) a tag for a row of
//! role `both` must say which half it proves: `/// covers(client): BASIC-01`
//! or `/// covers(server): BASIC-01`.
//!
//! # Inputs
//!
//! * `docs/compliance/compliance.toml`: class, role, scope, keyword and area
//!   of every ledger row, plus `docs` evidence paths;
//! * `docs/SPEC-MAP.md`: the test file(s) each gated row must be proven in,
//!   and the corpus fixtures that exercise it;
//! * `docs/compliance/pending.toml`: rows still open (`client` / `server`
//!   lists of row ids);
//! * `tests/**/*.rs`: the `covers` tags of integration tests;
//! * `src/**/*.rs`: the `covers` tags of unit tests (`#[test]` functions in
//!   `#[cfg(test)]` modules). A tag in `src/server/**` proves the server
//!   half, one in `src/client/**` the client half, and one anywhere else in
//!   `src/` (a shared module) every half the row has. A `covers(...)`
//!   qualifier narrows that;
//! * `tests/corpus/rust-lane-overrides.toml`: fixtures the Rust lane
//!   overrides, which therefore prove nothing about their rows;
//! * the spec cache's stream-corpus `MANIFEST.txt`, when the cache is
//!   present: the fixture names SPEC-MAP may cite (absent: not verified,
//!   noted in the report).
//!
//! # Rules
//!
//! A gated row is every `sdk-ownable` and `sdk-assistable` row of scope
//! `1.0`. Each row needs one *half* per role it plays: `client`, `server`, or
//! both. A half is covered by:
//!
//! 1. a tagged test in the file SPEC-MAP names for that half (for `a.rs +
//!    b.rs` rows the first file is the client half and the second the server
//!    half). Server halves need a server-side file and client halves a
//!    client-side file; shared files serve either. A unit-test tag in `src/`
//!    proves its half wherever SPEC-MAP points;
//! 2. for client halves only, a fixture listed in SPEC-MAP's fixtures column,
//!    replayed by the stream lane (`tests/corpus_streams.rs`) and not
//!    overridden for the Rust lane;
//! 3. for `sdk-assistable` rows only, a `docs` path in compliance.toml that
//!    exists and names the row.
//!
//! The gate fails on: an uncovered half not listed as pending; a pending
//! entry for an unknown row or a half the row does
//! not have; a stale pending entry (the half is covered now: delete it); a
//! tag naming an unknown or ungated row; a half tagged only in files other
//! than its mapped one; an unqualified tag for a `both` row in a shared file;
//! docs evidence on a non-assistable row; a `covers` tag (in `tests/` or
//! `src/`) that is not on a `#[test]` function that runs: one marked
//! `#[ignore]`, one under a `#[cfg]` other than `test` or features, or one
//! in a file an inner `#![cfg]` compiles out (such a tag covers nothing); a
//! `*.rs` file named in a compliance.toml `reason` that does not exist under
//! `tests/` or `src/`; and any disagreement between SPEC-MAP and
//! compliance.toml. With [`Mode::NoPending`] (release) any pending entry is
//! a failure too.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Test files that hold tests for both sides of the protocol.
pub const SHARED_FILES: &[&str] = &[
    "tests/basic_events.rs",
    "tests/transport_contract.rs",
    "tests/passthrough_events.rs",
    "tests/security_consent.rs",
    "tests/lifecycle_usage.rs",
];

/// The stream-lane driver whose presence makes fixture evidence count.
pub const STREAM_LANE: &str = "tests/corpus_streams.rs";

/// One side of a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Half {
    /// The consumer side.
    Client,
    /// The producer side.
    Server,
}

impl Half {
    /// `client` or `server`.
    pub fn as_str(self) -> &'static str {
        match self {
            Half::Client => "client",
            Half::Server => "server",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "client" => Some(Half::Client),
            "server" => Some(Half::Server),
            _ => None,
        }
    }
}

/// Which halves a test file can prove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSide {
    /// Client-side rule tests.
    Client,
    /// Server-side tests (`*_producer.rs`, `*_server.rs`, `*_builder.rs`,
    /// `producer_fixtures.rs`).
    Server,
    /// Both ([`SHARED_FILES`], and `src/` outside `client/` and `server/`).
    Shared,
}

impl FileSide {
    /// Classifies a workspace-relative test path.
    pub fn of(file: &str) -> Self {
        let name = file.rsplit('/').next().unwrap_or(file);
        if let Some(module) = file.strip_prefix("src/") {
            return if module.starts_with("server/") {
                FileSide::Server
            } else if module.starts_with("client/") {
                FileSide::Client
            } else {
                FileSide::Shared
            };
        }
        if SHARED_FILES.contains(&file) {
            FileSide::Shared
        } else if name.ends_with("_producer.rs")
            || name.ends_with("_server.rs")
            || name.ends_with("_builder.rs")
            || name == "producer_fixtures.rs"
        {
            FileSide::Server
        } else {
            FileSide::Client
        }
    }

    /// Whether this file may prove `half`.
    pub fn serves(self, half: Half) -> bool {
        matches!(
            (self, half),
            (FileSide::Shared, _)
                | (FileSide::Client, Half::Client)
                | (FileSide::Server, Half::Server)
        )
    }
}

/// Paths of every input.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Inputs {
    /// Workspace root; tag and evidence paths are relative to it.
    pub root: PathBuf,
    /// `docs/SPEC-MAP.md`.
    pub spec_map: PathBuf,
    /// `docs/compliance/compliance.toml`.
    pub compliance: PathBuf,
    /// `docs/compliance/pending.toml` (may be absent: nothing pending).
    pub pending: PathBuf,
    /// `tests/`.
    pub tests_dir: PathBuf,
    /// `src/` (unit tests).
    pub src_dir: PathBuf,
    /// `tests/corpus/rust-lane-overrides.toml` (may be absent).
    pub overrides: PathBuf,
    /// `MANIFEST.txt` of the stream corpus, to check SPEC-MAP fixture names
    /// against (`None`: not verified, noted in the report).
    pub streams_manifest: Option<PathBuf>,
}

impl Inputs {
    /// The standard layout under `root`.
    pub fn under(root: &Path) -> Self {
        Inputs {
            root: root.to_path_buf(),
            spec_map: root.join("docs/SPEC-MAP.md"),
            compliance: root.join("docs/compliance/compliance.toml"),
            pending: root.join("docs/compliance/pending.toml"),
            tests_dir: root.join("tests"),
            src_dir: root.join("src"),
            overrides: root.join(crate::overrides::OVERRIDES_FILE),
            streams_manifest: None,
        }
    }

    /// The standard layout of this workspace.
    pub fn repo() -> Self {
        let mut inputs = Self::under(&crate::workspace_root());
        if let crate::spec::CacheStatus::Ready(cache) = crate::spec::locate() {
            inputs.streams_manifest =
                Some(crate::spec::streams_dir_in(&cache).join("MANIFEST.txt"));
        }
        inputs
    }
}

/// Whether pending entries are tolerated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Pending entries are allowed (and must not be stale).
    Pending,
    /// Nothing may be pending (the release gate).
    NoPending,
}

/// A row from compliance.toml.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct Rule {
    /// Row id.
    pub id: String,
    /// RFC 2119 keyword, `implicit`, `REQUIRED` or `OPTIONAL`.
    pub keyword: String,
    /// `sdk-ownable`, `sdk-assistable`, `application` or `informative`.
    pub class: String,
    /// `client`, `server`, `both` or `n/a`.
    pub role: String,
    /// Spec area.
    pub area: String,
    /// `1.0` or `draft-1.1`.
    pub scope: String,
    /// In-repo documentation that states the row (assistable rows only).
    #[serde(default)]
    pub docs: Vec<String>,
    /// Why the row is classified as it is; any `*.rs` file it names must
    /// exist (see [`missing_rs_files`]).
    #[serde(default)]
    pub reason: String,
}

impl Rule {
    /// Whether the gate holds this row.
    pub fn gated(&self) -> bool {
        self.scope == "1.0" && (self.class == "sdk-ownable" || self.class == "sdk-assistable")
    }

    /// The halves the row needs.
    pub fn halves(&self) -> Vec<Half> {
        match self.role.as_str() {
            "client" => vec![Half::Client],
            "server" => vec![Half::Server],
            "both" => vec![Half::Client, Half::Server],
            _ => Vec::new(),
        }
    }
}

#[derive(Deserialize)]
struct ComplianceDoc {
    rule: Vec<Rule>,
}

/// Reads compliance.toml.
///
/// # Errors
///
/// Unreadable or invalid file, or duplicate ids.
pub fn rules(path: &Path) -> Result<Vec<Rule>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let doc: ComplianceDoc =
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut seen = BTreeSet::new();
    for r in &doc.rule {
        if !seen.insert(r.id.clone()) {
            return Err(format!("{}: duplicate id {}", path.display(), r.id));
        }
    }
    Ok(doc.rule)
}

/// A gated row from SPEC-MAP.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MappedRow {
    /// Rule id.
    pub id: String,
    /// `ownable` or `assistable`.
    pub class: String,
    /// `client`, `server` or `both`.
    pub role: String,
    /// The test file(s) SPEC-MAP assigns, workspace-relative.
    pub test_files: Vec<String>,
    /// Stream fixtures that exercise the row.
    pub fixtures: Vec<String>,
}

fn is_row_id(text: &str) -> bool {
    let Some((prefix, number)) = text.split_once('-') else {
        return false;
    };
    (2..=6).contains(&prefix.len())
        && prefix.bytes().all(|b| b.is_ascii_uppercase())
        && (2..=3).contains(&number.len())
        && number.bytes().all(|b| b.is_ascii_digit())
}

/// Rows parsed from the table under `## The map` in SPEC-MAP.
///
/// # Errors
///
/// Unreadable file, a row with the wrong column count, or a duplicate id.
pub fn mapped_rows(spec_map: &Path) -> Result<Vec<MappedRow>, String> {
    let text =
        std::fs::read_to_string(spec_map).map_err(|e| format!("{}: {e}", spec_map.display()))?;
    let mut in_map = false;
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    for (n, line) in text.lines().enumerate() {
        if line.starts_with("## ") {
            in_map = line.trim() == "## The map";
            continue;
        }
        if !in_map || !line.starts_with('|') {
            continue;
        }
        let cols: Vec<&str> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect();
        if !is_row_id(cols[0]) {
            continue;
        }
        if cols.len() != 9 {
            return Err(format!(
                "SPEC-MAP line {}: {} columns, expected 9",
                n + 1,
                cols.len()
            ));
        }
        let test_files = cols[6]
            .split('+')
            .map(|f| f.trim().trim_matches('`').trim().to_owned())
            .filter(|f| !f.is_empty())
            .collect();
        // A fixture may carry a note: `name (Rust-lane override: ...)`.
        let mut depth = 0usize;
        let names: String = cols[7]
            .chars()
            .filter(|c| {
                match c {
                    '(' => depth += 1,
                    ')' => depth = depth.saturating_sub(1),
                    _ => return depth == 0,
                }
                false
            })
            .collect();
        let fixtures = names
            .split(',')
            .filter_map(|f| f.split_whitespace().next())
            .map(|f| f.trim_matches('`').to_owned())
            .filter(|f| !f.is_empty())
            .collect();
        if !seen.insert(cols[0].to_owned()) {
            return Err(format!("SPEC-MAP line {}: duplicate id {}", n + 1, cols[0]));
        }
        rows.push(MappedRow {
            id: cols[0].to_owned(),
            class: cols[2].to_owned(),
            role: cols[3].to_owned(),
            test_files,
            fixtures,
        });
    }
    Ok(rows)
}

/// One `covers` tag.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Tag {
    /// Row id.
    pub id: String,
    /// Workspace-relative file.
    pub file: String,
    /// The test function's name.
    pub test: String,
    /// `covers(client)` / `covers(server)` qualifier.
    pub half: Option<Half>,
    /// 1-based line of the tag.
    pub line: usize,
}

/// Parses one line; `Some(Ok((qualifier, ids)))` for a tag line.
type TagLine = Result<(Option<Half>, Vec<String>), String>;

fn parse_tag_line(line: &str) -> Option<TagLine> {
    let rest = line.trim_start().strip_prefix("///")?.trim_start();
    let rest = rest.strip_prefix("covers")?;
    let malformed = || Some(Err(format!("malformed covers line: {}", line.trim())));
    let (half, rest) = if let Some(q) = rest.strip_prefix('(') {
        // `covers(` is an attempted tag: a missing `)` or `:` is a problem,
        // not prose.
        let Some((q, after)) = q.split_once(')') else {
            return malformed();
        };
        match Half::parse(q.trim()) {
            Some(h) => match after.strip_prefix(':') {
                Some(rest) => (Some(h), rest),
                None => return malformed(),
            },
            None => return Some(Err(format!("unknown qualifier covers({q})"))),
        }
    } else {
        (None, rest.strip_prefix(':')?)
    };
    let ids: Vec<String> = rest.split(',').map(|s| s.trim().to_owned()).collect();
    if ids.is_empty() || ids.iter().any(|id| !is_row_id(id)) {
        return malformed();
    }
    Some(Ok((half, ids)))
}

/// The path of an attribute line: `#[tokio::test(flavor = ..)]` gives
/// `tokio::test`, `#[ignore = ".."]` gives `ignore`, `#![cfg(..)]` gives `cfg`.
fn attr_path(t: &str) -> Option<&str> {
    let inner = t.strip_prefix("#![").or_else(|| t.strip_prefix("#["))?;
    let end = inner
        .find(|c: char| c == '(' || c == ']' || c == '=' || c.is_whitespace())
        .unwrap_or(inner.len());
    Some(&inner[..end])
}

/// Whether a `#[cfg(..)]` / `#![cfg(..)]` line leaves its item compiled in
/// some CI lane. The gate reads source text and cannot evaluate an arbitrary
/// predicate, so it accepts only the forms it can vouch for: `cfg(test)`,
/// `cfg(feature = "..")` and `cfg(all(feature = "..", ..))` (an all-features
/// lane compiles them; a
/// misspelt feature fails clippy through `unexpected_cfgs`). Every other
/// predicate, and one not on the attribute's first line, is refused.
fn cfg_compiles_in(t: &str) -> bool {
    let p: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    let Some(pred) = p
        .strip_prefix("#![cfg(")
        .or_else(|| p.strip_prefix("#[cfg("))
        .and_then(|r| r.strip_suffix(")]"))
    else {
        return false;
    };
    let feature = |f: &str| {
        f.strip_prefix("feature=\"")
            .and_then(|r| r.strip_suffix('"'))
            .is_some_and(|name| !name.is_empty() && !name.contains('"'))
    };
    pred == "test"
        || feature(pred)
        || pred
            .strip_prefix("all(")
            .and_then(|r| r.strip_suffix(')'))
            .is_some_and(|list| !list.is_empty() && list.split(',').all(feature))
}

/// Whether the inner `#![cfg(..)]` attributes at the top of a file (before
/// its first item) leave the file compiled in, by the rule of
/// [`cfg_compiles_in`]. The scan reads past comments, inner block docs
/// (`/*! .. */`) and the continuation lines of a multi-line inner attribute,
/// so a `#![cfg(..)]` after any of them is still judged; a `#![cfg(..)]`
/// whose predicate is not on its first line is refused, as for items.
fn file_compiles_in(lines: &[&str]) -> bool {
    // Open `[` of the inner attribute being read, and whether a block
    // comment is open.
    let mut depth = 0usize;
    let mut in_block = false;
    for line in lines {
        let t = line.trim();
        if in_block {
            in_block = !t.contains("*/");
            continue;
        }
        if depth > 0 {
            depth = bracket_depth(t, depth);
            continue;
        }
        if t.starts_with("#![") {
            if attr_path(t) == Some("cfg") && !cfg_compiles_in(t) {
                return false;
            }
            depth = bracket_depth(t, 0);
            continue;
        }
        if let Some(rest) = t.strip_prefix("/*") {
            in_block = !rest.contains("*/");
            continue;
        }
        if t.is_empty() || t.starts_with("//") {
            continue;
        }
        break;
    }
    true
}

/// `depth` after the square brackets of `line`, string literals skipped.
fn bracket_depth(line: &str, mut depth: usize) -> usize {
    let mut in_str = false;
    let mut escaped = false;
    for c in line.chars() {
        if in_str {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    depth
}

/// The function a tag at `lines[i]` belongs to: the first `fn` after it,
/// past doc lines, comments and attributes only.
/// Also reports whether the function is a test that runs:
/// - a test attribute sits between the tag and the function, recognised by
///   its path (`#[test]`, `#[tokio::test]`, any path ending in `::test`),
///   not by the word "test" appearing somewhere in the attribute;
/// - `#[ignore]` does not (`#[cfg_attr(.., ignore)]` still counts as
///   running: the test runs in the other configurations);
/// - every `#[cfg(..)]` there is one [`cfg_compiles_in`] accepts.
///
/// File-level `#![cfg(..)]` is checked by the caller ([`file_compiles_in`]).
/// A `#[cfg(..)]` on an enclosing `mod` is not seen: that needs scope
/// tracking, and every tagged module today is `#[cfg(test)]` or
/// feature-gated.
fn function_after(lines: &[&str], i: usize) -> Option<(String, bool)> {
    let mut test_attr = false;
    let mut ignored = false;
    let mut compiled_out = false;
    for line in lines.iter().skip(i + 1).take(40) {
        let t = line.trim();
        match attr_path(t) {
            Some(path) if path == "test" || path.ends_with("::test") => test_attr = true,
            Some("ignore") => ignored = true,
            Some("cfg") if !cfg_compiles_in(t) => compiled_out = true,
            _ => {}
        }
        if t.is_empty()
            || t.starts_with("//")
            || t.starts_with("#[")
            || t.starts_with(')')
            || t.starts_with(']')
        {
            continue;
        }
        // `pub(crate)`, `pub(super)`, `pub(in path)`: drop the restriction so
        // its parenthesis is not taken for the start of the parameter list.
        let t = t
            .strip_prefix("pub(")
            .and_then(|r| r.split_once(')'))
            .map_or(t, |(_, rest)| rest);
        let mut words = t.split(|c: char| c.is_whitespace() || c == '(' || c == '<');
        while let Some(w) = words.next() {
            if w == "fn" {
                return words
                    .next()
                    .map(|name| (name.to_owned(), test_attr && !ignored && !compiled_out));
            }
            if !matches!(w, "pub" | "async" | "const" | "unsafe" | "") {
                return None;
            }
        }
        return None;
    }
    None
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Every `covers` tag in the `.rs` files under `dirs`, with paths relative
/// to `root`. Malformed tags are returned as problems, as are tags that are
/// not on a `#[test]` function that runs (not `#[ignore]`d, not under an
/// unvouched `#[cfg]`).
pub fn scan_tags(root: &Path, dirs: &[&Path]) -> (Vec<Tag>, Vec<String>) {
    let mut files = Vec::new();
    for dir in dirs {
        rust_files(dir, &mut files);
    }
    files.sort();
    let mut tags = Vec::new();
    let mut problems = Vec::new();
    for path in files {
        let file = relative(root, &path);
        let Ok(text) = std::fs::read_to_string(&path) else {
            problems.push(format!("{file}: unreadable"));
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        let file_runs = file_compiles_in(&lines);
        for (i, line) in lines.iter().enumerate() {
            match parse_tag_line(line) {
                None => {}
                Some(Err(e)) => problems.push(format!("{file}:{}: {e}", i + 1)),
                Some(Ok((half, ids))) => match function_after(&lines, i) {
                    None => {
                        problems.push(format!("{file}:{}: covers tag is not on a function", i + 1))
                    }
                    Some((_, runs)) if !runs || !file_runs => problems.push(format!(
                        "{file}:{}: covers tag must be on a #[test] function that runs (not #[ignore]d, not under a #[cfg] other than test or features)",
                        i + 1
                    )),
                    Some((test, _)) => tags.extend(ids.into_iter().map(|id| Tag {
                        id,
                        file: file.clone(),
                        test: test.clone(),
                        half,
                        line: i + 1,
                    })),
                },
            }
        }
    }
    (tags, problems)
}

/// One pending half.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Pending {
    /// Row id.
    pub id: String,
    /// The open half.
    pub half: Half,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingDoc {
    #[serde(default)]
    client: Vec<String>,
    #[serde(default)]
    server: Vec<String>,
}

/// Reads pending.toml (absent: nothing pending).
///
/// # Errors
///
/// Invalid TOML or a half listed twice.
pub fn pending(path: &Path) -> Result<Vec<Pending>, Vec<String>> {
    let (out, problems) = pending_lenient(path);
    if problems.is_empty() {
        Ok(out)
    } else {
        Err(problems)
    }
}

/// [`pending`], keeping the valid entries alongside the problems: a half
/// listed twice is reported once and its first listing still counts, so the
/// other pending halves are not also reported as uncovered.
fn pending_lenient(path: &Path) -> (Vec<Pending>, Vec<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), Vec::new()),
        Err(e) => return (Vec::new(), vec![format!("{}: {e}", path.display())]),
    };
    let doc: PendingDoc = match toml::from_str(&text) {
        Ok(d) => d,
        Err(e) => return (Vec::new(), vec![format!("{}: {e}", path.display())]),
    };
    let mut out = Vec::new();
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    for (half, ids) in [(Half::Client, doc.client), (Half::Server, doc.server)] {
        for id in ids {
            if !seen.insert((id.clone(), half)) {
                problems.push(format!("pending: {id} ({}) is listed twice", half.as_str()));
                continue;
            }
            out.push(Pending { id, half });
        }
    }
    (out, problems)
}

/// What covers a half.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    /// A tagged test.
    Test {
        /// File.
        file: String,
        /// Function.
        test: String,
    },
    /// A stream fixture replayed by the stream lane.
    Fixture(String),
    /// Documentation (assistable rows only).
    Docs(String),
}

/// One half of a gated row.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HalfStatus {
    /// Which half.
    pub half: Half,
    /// The file SPEC-MAP assigns to it.
    pub file: String,
    /// What covers it (empty: open).
    pub evidence: Vec<Evidence>,
    /// Whether the half is listed in pending.toml.
    pub pending: bool,
}

/// One gated row and its halves.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RowStatus {
    /// Ledger data.
    pub rule: Rule,
    /// Halves, in client-then-server order.
    pub halves: Vec<HalfStatus>,
}

impl RowStatus {
    /// Every half covered.
    pub fn covered(&self) -> bool {
        self.halves.iter().all(|h| !h.evidence.is_empty())
    }
}

/// Everything the gate found.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Report {
    /// Gated rows, in SPEC-MAP order.
    pub rows: Vec<RowStatus>,
    /// Gate failures.
    pub problems: Vec<String>,
    /// Informational notes (not failures).
    pub notes: Vec<String>,
}

/// Runs every check and returns the full picture.
pub fn analyze(inputs: &Inputs, mode: Mode) -> Report {
    let mut report = Report::default();
    let problems = &mut report.problems;

    let rules = match rules(&inputs.compliance) {
        Ok(r) => r,
        Err(e) => {
            problems.push(e);
            return report;
        }
    };
    let mapped = match mapped_rows(&inputs.spec_map) {
        Ok(m) => m,
        Err(e) => {
            problems.push(e);
            return report;
        }
    };
    let (pending_list, pending_problems) = pending_lenient(&inputs.pending);
    problems.extend(pending_problems);
    // A missing file means no overrides; an unreadable or invalid one is a
    // problem (the stream lane panics on it), not silently "none".
    let overridden: BTreeSet<String> = match std::fs::read_to_string(&inputs.overrides) {
        Ok(text) => match crate::overrides::Overrides::parse(&text) {
            Ok(o) => o.iter().map(|e| e.fixture.clone()).collect(),
            Err(p) => {
                problems.extend(p);
                BTreeSet::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
        Err(e) => {
            problems.push(format!("{}: {e}", inputs.overrides.display()));
            BTreeSet::new()
        }
    };
    let stream_lane = inputs.root.join(STREAM_LANE).is_file();
    let mut extra_notes = Vec::new();
    // The corpus's fixture file names, when they can be checked.
    let known_fixtures: Option<BTreeSet<String>> = if stream_lane {
        match inputs
            .streams_manifest
            .as_deref()
            .map(crate::corpus::read_manifest)
        {
            Some(Ok(set)) => Some(set),
            Some(Err(e)) => {
                problems.push(e);
                None
            }
            None => {
                extra_notes
                    .push("SPEC-MAP fixture names not verified: spec cache absent".to_owned());
                None
            }
        }
    } else {
        None
    };
    problems.extend(missing_rs_files(inputs, &rules));
    let (tags, tag_problems) = scan_tags(&inputs.root, &[&inputs.tests_dir, &inputs.src_dir]);
    problems.extend(tag_problems);

    let by_id: BTreeMap<&str, &Rule> = rules.iter().map(|r| (r.id.as_str(), r)).collect();
    let map_by_id: BTreeMap<&str, &MappedRow> = mapped.iter().map(|m| (m.id.as_str(), m)).collect();

    // SPEC-MAP and compliance.toml must describe the same gated rows.
    for r in rules.iter().filter(|r| r.gated()) {
        match map_by_id.get(r.id.as_str()) {
            None => problems.push(format!(
                "{}: gated in compliance.toml but missing from SPEC-MAP",
                r.id
            )),
            Some(m) => {
                if format!("sdk-{}", m.class) != r.class || m.role != r.role {
                    problems.push(format!(
                        "{}: SPEC-MAP says {}/{}, compliance.toml says {}/{}",
                        r.id, m.class, m.role, r.class, r.role
                    ));
                }
            }
        }
    }
    for m in &mapped {
        if !by_id.get(m.id.as_str()).is_some_and(|r| r.gated()) {
            problems.push(format!(
                "{}: in SPEC-MAP but not a gated row of compliance.toml",
                m.id
            ));
        }
    }

    // Tags must name gated rows.
    for t in &tags {
        if !by_id.get(t.id.as_str()).is_some_and(|r| r.gated())
            || !map_by_id.contains_key(t.id.as_str())
        {
            problems.push(format!(
                "{}:{}: covers {} which is not a gated row of SPEC-MAP",
                t.file, t.line, t.id
            ));
        }
    }

    let pending_by: BTreeSet<(String, Half)> = pending_list
        .iter()
        .map(|p| (p.id.clone(), p.half))
        .collect();
    let mut used_pending = BTreeSet::new();

    for m in &mapped {
        let Some(rule) = by_id.get(m.id.as_str()).copied().filter(|r| r.gated()) else {
            continue;
        };
        let halves = rule.halves();
        if let Some(known) = &known_fixtures {
            for f in m
                .fixtures
                .iter()
                .filter(|f| !known.contains(&format!("{f}.json")))
            {
                problems.push(format!(
                    "{}: SPEC-MAP fixture {f} is not in the stream corpus",
                    m.id
                ));
            }
        }
        // Which file proves which half.
        let files: Vec<(Half, String)> = match (halves.as_slice(), m.test_files.as_slice()) {
            ([h], [f]) => vec![(*h, f.clone())],
            ([Half::Client, Half::Server], [f]) => {
                vec![(Half::Client, f.clone()), (Half::Server, f.clone())]
            }
            ([Half::Client, Half::Server], [c, s]) => {
                vec![(Half::Client, c.clone()), (Half::Server, s.clone())]
            }
            _ => {
                problems.push(format!(
                    "{}: role {} cannot be mapped onto test file(s) {:?}",
                    m.id, rule.role, m.test_files
                ));
                continue;
            }
        };
        let docs_ok: Vec<String> = rule
            .docs
            .iter()
            .filter(|p| {
                std::fs::read_to_string(inputs.root.join(p))
                    .is_ok_and(|t| t.contains(rule.id.as_str()))
            })
            .cloned()
            .collect();
        if !rule.docs.is_empty() && rule.class != "sdk-assistable" {
            problems.push(format!(
                "{}: docs evidence is accepted for sdk-assistable rows only",
                rule.id
            ));
        }
        for p in rule.docs.iter().filter(|p| !docs_ok.contains(p)) {
            problems.push(format!(
                "{}: docs evidence {p} does not exist or does not name the row",
                rule.id
            ));
        }
        let row_tags: Vec<&Tag> = tags.iter().filter(|t| t.id == rule.id).collect();
        let mut status = RowStatus {
            rule: rule.clone(),
            halves: Vec::new(),
        };
        for (half, file) in files {
            if !FileSide::of(&file).serves(half) {
                problems.push(format!(
                    "{}: SPEC-MAP assigns the {} half to {file}, which is not a {}-side test file",
                    rule.id,
                    half.as_str(),
                    half.as_str()
                ));
            }
            let mut evidence = Vec::new();
            let mut elsewhere = Vec::new();
            for t in &row_tags {
                let side = FileSide::of(&t.file);
                let in_src = t.file.starts_with("src/");
                let tag_half = match (t.half, side) {
                    (Some(h), s) if s.serves(h) => Some(h),
                    (Some(h), _) => {
                        // Once per tag, not once per half of the row.
                        if half == halves[0] {
                            problems.push(format!(
                                "{}:{}: covers({}) in a file that cannot prove that half",
                                t.file,
                                t.line,
                                h.as_str()
                            ));
                        }
                        None
                    }
                    (None, FileSide::Client) => Some(Half::Client),
                    (None, FileSide::Server) => Some(Half::Server),
                    (None, FileSide::Shared) if halves.len() == 1 => Some(halves[0]),
                    // A shared src module proves every half the row has.
                    (None, FileSide::Shared) if in_src => Some(half),
                    (None, FileSide::Shared) => {
                        if half == Half::Client {
                            problems.push(format!(
                                "{}:{}: {} has both halves; in a shared file write covers(client) or covers(server)",
                                t.file, t.line, t.id
                            ));
                        }
                        None
                    }
                };
                if tag_half != Some(half) {
                    continue;
                }
                if t.file == file || in_src {
                    evidence.push(Evidence::Test {
                        file: t.file.clone(),
                        test: t.test.clone(),
                    });
                } else {
                    elsewhere.push(format!("{}:{}", t.file, t.line));
                }
            }
            if evidence.is_empty() && !elsewhere.is_empty() {
                problems.push(format!(
                    "{} ({}): tagged only outside {file} ({}): move the test or change SPEC-MAP",
                    rule.id,
                    half.as_str(),
                    elsewhere.join(", ")
                ));
            }
            if half == Half::Client && stream_lane {
                evidence.extend(
                    m.fixtures
                        .iter()
                        .filter(|f| !overridden.contains(*f))
                        .filter(|f| {
                            known_fixtures
                                .as_ref()
                                .map_or(true, |k| k.contains(&format!("{f}.json")))
                        })
                        .map(|f| Evidence::Fixture(f.clone())),
                );
            }
            if rule.class == "sdk-assistable" {
                evidence.extend(docs_ok.iter().map(|d| Evidence::Docs(d.clone())));
            }
            let key = (rule.id.clone(), half);
            let is_pending = pending_by.contains(&key);
            if is_pending {
                used_pending.insert(key);
            }
            match (is_pending, evidence.is_empty()) {
                (true, false) => problems.push(format!(
                    "{} ({}): stale pending entry: the half is covered now; delete it from pending.toml",
                    rule.id,
                    half.as_str()
                )),
                (true, true) if mode == Mode::NoPending => problems.push(format!(
                    "{} ({}): still pending; nothing may be pending in release mode",
                    rule.id,
                    half.as_str()
                )),
                (false, true) => problems.push(format!(
                    "{} ({}): no covering test in {file} and not listed as pending",
                    rule.id,
                    half.as_str()
                )),
                _ => {}
            }
            status.halves.push(HalfStatus {
                half,
                file,
                evidence,
                pending: is_pending,
            });
        }
        report.rows.push(status);
    }
    for p in &pending_list {
        if !used_pending.contains(&(p.id.clone(), p.half)) {
            problems.push(format!(
                "pending: {} ({}) is not a half of any gated row",
                p.id,
                p.half.as_str()
            ));
        }
    }
    report.notes.extend(extra_notes);
    let open = pending_list.len();
    if open > 0 {
        report.notes.push(format!(
            "{open} half/halves pending (see docs/compliance/progress.md)"
        ));
    }
    report
}

/// The `*.rs` names a reason mentions: maximal runs of path characters
/// (`[A-Za-z0-9_./-]`) ending in `.rs` at a word boundary.
pub fn rs_mentions(text: &str) -> Vec<String> {
    let path_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-');
    let mut out = Vec::new();
    for word in text.split(|c: char| !path_char(c)) {
        let word = word.trim_end_matches('.');
        if let Some(stem) = word.strip_suffix(".rs") {
            if !stem.is_empty() && !stem.ends_with('/') {
                out.push(word.to_owned());
            }
        }
    }
    out
}

/// One problem per `*.rs` file a compliance.toml reason names that does not
/// exist: a bare name must match a file somewhere under `tests/` or `src/`,
/// a path (`tests/a.rs`, `src/x/y.rs`) must exist as written and lie under
/// one of them. Stops reasons citing tests that are not in this crate.
pub fn missing_rs_files(inputs: &Inputs, rules: &[Rule]) -> Vec<String> {
    let mut files = Vec::new();
    rust_files(&inputs.tests_dir, &mut files);
    rust_files(&inputs.src_dir, &mut files);
    let names: BTreeSet<String> = files
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    let mut problems = Vec::new();
    for rule in rules {
        for mention in rs_mentions(&rule.reason) {
            let found = if mention.contains('/') {
                let path = inputs.root.join(&mention);
                path.is_file()
                    && (path.starts_with(&inputs.tests_dir) || path.starts_with(&inputs.src_dir))
            } else {
                names.contains(&mention)
            };
            if !found {
                problems.push(format!(
                    "{}: reason cites {mention}, which is not a file under tests/ or src/",
                    rule.id
                ));
            }
        }
    }
    problems
}

/// Runs the gate over `inputs`.
///
/// # Errors
///
/// One line per problem.
pub fn gate_with(inputs: &Inputs, mode: Mode) -> Result<(), Vec<String>> {
    let report = analyze(inputs, mode);
    if report.problems.is_empty() {
        Ok(())
    } else {
        Err(report.problems)
    }
}

fn pct(n: usize, d: usize) -> String {
    if d == 0 {
        "n/a".to_owned()
    } else {
        format!("{:.1}%", 100.0 * n as f64 / d as f64)
    }
}

/// Renders the progress report (`docs/compliance/progress.md`).
pub fn progress_markdown(report: &Report) -> String {
    let rows = &report.rows;
    let halves: Vec<(&RowStatus, &HalfStatus)> = rows
        .iter()
        .flat_map(|r| r.halves.iter().map(move |h| (r, h)))
        .collect();
    let covered_rows = rows.iter().filter(|r| r.covered()).count();
    let covered_halves = halves
        .iter()
        .filter(|(_, h)| !h.evidence.is_empty())
        .count();
    let mut md = String::new();
    let _ = writeln!(md, "# AG-UI 1.0 compliance progress\n");
    let _ = writeln!(
        md,
        "Generated by `cargo run -p agui-test-support --bin agui-trace -- progress`; do not edit by hand.\n\
         Inputs: `docs/compliance/compliance.toml`, `docs/SPEC-MAP.md`, `docs/compliance/pending.toml`, `tests/**/*.rs`, `src/**/*.rs`, `tests/corpus/rust-lane-overrides.toml`, and the spec cache's stream-corpus `MANIFEST.txt` when present.\n"
    );
    let _ = writeln!(md, "| | covered | total | % |\n|---|---|---|---|");
    let _ = writeln!(
        md,
        "| gated rows (all halves) | {covered_rows} | {} | {} |",
        rows.len(),
        pct(covered_rows, rows.len())
    );
    let _ = writeln!(
        md,
        "| row halves | {covered_halves} | {} | {} |",
        halves.len(),
        pct(covered_halves, halves.len())
    );
    let (mut tests, mut fixtures, mut docs) = (0, 0, 0);
    for (_, h) in &halves {
        if h.evidence
            .iter()
            .any(|e| matches!(e, Evidence::Test { .. }))
        {
            tests += 1;
        } else if h.evidence.iter().any(|e| matches!(e, Evidence::Fixture(_))) {
            fixtures += 1;
        } else if !h.evidence.is_empty() {
            docs += 1;
        }
    }
    let _ = writeln!(md, "\nCovered halves by strongest evidence: {tests} by tagged test, {fixtures} by corpus fixture only, {docs} by docs only.\n");

    let table = |md: &mut String, title: &str, key: &dyn Fn(&RowStatus, &HalfStatus) -> String| {
        let mut groups: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for (r, h) in &halves {
            let g = groups.entry(key(r, h)).or_default();
            g.1 += 1;
            if !h.evidence.is_empty() {
                g.0 += 1;
            }
        }
        let _ = writeln!(
            md,
            "## By {title}\n\n| {title} | covered | halves | % |\n|---|---|---|---|"
        );
        for (k, (c, t)) in groups {
            let _ = writeln!(md, "| {k} | {c} | {t} | {} |", pct(c, t));
        }
        let _ = writeln!(md);
    };
    table(&mut md, "keyword", &|r, _| r.rule.keyword.clone());
    table(&mut md, "area", &|r, _| r.rule.area.clone());
    table(&mut md, "test file", &|_, h| format!("`{}`", h.file));

    let open: Vec<String> = halves
        .iter()
        .filter(|(_, h)| h.evidence.is_empty())
        .map(|(r, h)| format!("{} ({})", r.rule.id, h.half.as_str()))
        .collect();
    let _ = writeln!(md, "## Open\n");
    if open.is_empty() {
        let _ = writeln!(md, "Nothing is open.");
    } else {
        let _ = writeln!(md, "{} halves: {}", open.len(), open.join(", "));
    }
    if !report.problems.is_empty() {
        let _ = writeln!(md, "\n## Gate problems\n");
        for p in &report.problems {
            let _ = writeln!(md, "- {p}");
        }
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("agui-ts-trace-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let f = Fixture { root };
            f.write(
                "docs/compliance/compliance.toml",
                r#"
[[rule]]
id = "AAA-01"
keyword = "MUST"
class = "sdk-ownable"
role = "client"
area = "x"
scope = "1.0"

[[rule]]
id = "BBB-01"
keyword = "MUST"
class = "sdk-ownable"
role = "both"
area = "x"
scope = "1.0"

[[rule]]
id = "CCC-01"
keyword = "SHOULD"
class = "sdk-assistable"
role = "server"
area = "y"
scope = "1.0"
docs = ["src/doc.rs"]

[[rule]]
id = "DDD-01"
keyword = "MUST"
class = "sdk-ownable"
role = "both"
area = "y"
scope = "1.0"

[[rule]]
id = "EEE-01"
keyword = "MUST"
class = "informative"
role = "n/a"
area = "y"
scope = "1.0"
"#,
            );
            f.write(
                "docs/SPEC-MAP.md",
                "# map\n\n## The map\n\n| ID | kw | class | role | rule | impl | test | fixtures | flag |\n|---|---|---|---|---|---|---|---|---|\n\
| AAA-01 | MUST | ownable | client | r | i | `tests/a_client.rs` | fx-one (a note, with words), fx-two |  |\n\
| BBB-01 | MUST | ownable | both | r | i | `tests/a_client.rs + tests/a_producer.rs` |  |  |\n\
| CCC-01 | SHOULD | assistable | server | r | i | `tests/a_producer.rs` |  |  |\n\
| DDD-01 | MUST | ownable | both | r | i | `tests/basic_events.rs` |  |  |\n",
            );
            f.write("src/doc.rs", "//! The helper documents CCC-01.\n");
            f
        }

        fn write(&self, rel: &str, text: &str) {
            let p = self.root.join(rel);
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).unwrap_or_else(|e| panic!("{e}"));
            }
            std::fs::write(p, text).unwrap_or_else(|e| panic!("{e}"));
        }

        fn problems(&self, mode: Mode) -> Vec<String> {
            analyze(&Inputs::under(&self.root), mode).problems
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn has(problems: &[String], needle: &str) -> bool {
        problems.iter().any(|p| p.contains(needle))
    }

    const FULL_CLIENT: &str = "/// covers: AAA-01, BBB-01\n#[test]\nfn t() {}\n";
    const FULL_SERVER: &str = "/// covers: BBB-01\n#[test]\nfn s() {}\n";
    const SHARED: &str = "/// covers(client): DDD-01\n#[test]\nfn c() {}\n\n/// covers(server): DDD-01\n#[tokio::test]\nasync fn s() {}\n";

    #[test]
    fn fully_covered_passes_strict() {
        let f = Fixture::new("full");
        f.write("tests/a_client.rs", FULL_CLIENT);
        f.write("tests/a_producer.rs", FULL_SERVER);
        f.write("tests/basic_events.rs", SHARED);
        assert_eq!(f.problems(Mode::NoPending), Vec::<String>::new());
        let report = analyze(&Inputs::under(&f.root), Mode::Pending);
        assert!(report.rows.iter().all(RowStatus::covered));
        assert!(progress_markdown(&report).contains("| gated rows (all halves) | 4 | 4 | 100.0% |"));
    }

    #[test]
    fn uncovered_halves_fail_unless_pending() {
        let f = Fixture::new("pending");
        f.write("tests/a_client.rs", FULL_CLIENT);
        f.write("tests/basic_events.rs", SHARED);
        let p = f.problems(Mode::Pending);
        assert!(
            has(
                &p,
                "BBB-01 (server): no covering test in tests/a_producer.rs"
            ),
            "{p:?}"
        );
        f.write("docs/compliance/pending.toml", "server = [\"BBB-01\"]\n");
        assert_eq!(f.problems(Mode::Pending), Vec::<String>::new());
        assert!(has(&f.problems(Mode::NoPending), "still pending"));
    }

    #[test]
    fn pending_halves_are_each_optional() {
        let f = Fixture::new("pending-halves");
        let path = f.root.join("docs/compliance/pending.toml");
        for (text, want) in [
            ("client = [\"AAA-01\"]\n", vec![("AAA-01", Half::Client)]),
            ("server = [\"CCC-01\"]\n", vec![("CCC-01", Half::Server)]),
            ("# nothing open\n", vec![]),
            ("", vec![]),
        ] {
            f.write("docs/compliance/pending.toml", text);
            let got = pending(&path).unwrap_or_else(|e| panic!("{text:?}: {e:?}"));
            let got: Vec<_> = got.iter().map(|p| (p.id.as_str(), p.half)).collect();
            assert_eq!(got, want, "{text:?}");
        }
    }

    #[test]
    fn a_duplicate_pending_entry_keeps_the_rest_of_the_list() {
        let f = Fixture::new("pending-dup");
        f.write("tests/a_client.rs", FULL_CLIENT);
        f.write("tests/basic_events.rs", SHARED);
        // BBB-01's server half is legitimately pending, but listed twice.
        f.write(
            "docs/compliance/pending.toml",
            "server = [\"BBB-01\", \"BBB-01\"]\n",
        );
        let p = f.problems(Mode::Pending);
        assert!(has(&p, "BBB-01 (server) is listed twice"), "{p:?}");
        assert!(!has(&p, "not listed as pending"), "{p:?}");
        assert_eq!(p.len(), 1, "{p:?}");
    }

    #[test]
    fn stale_and_bogus_pending_entries_fail() {
        let f = Fixture::new("stale");
        f.write("tests/a_client.rs", FULL_CLIENT);
        f.write("tests/a_producer.rs", FULL_SERVER);
        f.write("tests/basic_events.rs", SHARED);
        f.write(
            "docs/compliance/pending.toml",
            "client = [\"AAA-01\"]\nserver = [\"AAA-01\", \"ZZZ-01\"]\n",
        );
        let p = f.problems(Mode::Pending);
        assert!(has(&p, "AAA-01 (client): stale pending entry"), "{p:?}");
        assert!(has(&p, "AAA-01 (server) is not a half"), "{p:?}");
        assert!(has(&p, "ZZZ-01 (server) is not a half"), "{p:?}");
    }

    #[test]
    fn role_awareness() {
        let f = Fixture::new("roles");
        // BBB-01's server half tagged in the client file only: wrong side.
        f.write("tests/a_client.rs", "/// covers: AAA-01\n#[test]\nfn t() {}\n/// covers(server): BBB-01\n#[test]\nfn u() {}\n");
        f.write(
            "tests/a_producer.rs",
            "/// covers: CCC-01\n#[test]\nfn s() {}\n",
        );
        // Unqualified tag for a both row in a shared file.
        f.write(
            "tests/basic_events.rs",
            "/// covers: DDD-01\n#[test]\nfn c() {}\n",
        );
        let p = f.problems(Mode::Pending);
        assert!(
            has(&p, "covers(server) in a file that cannot prove that half"),
            "{p:?}"
        );
        assert!(has(&p, "BBB-01 (client): no covering test"), "{p:?}");
        assert!(has(&p, "BBB-01 (server): no covering test"), "{p:?}");
        assert!(has(&p, "write covers(client) or covers(server)"), "{p:?}");
    }

    #[test]
    fn unknown_spec_map_fixture_is_a_problem() {
        let f = Fixture::new("unknown-fixture");
        f.write(
            "tests/a_client.rs",
            "/// covers: BBB-01\n#[test]\nfn t() {}\n",
        );
        f.write("tests/a_producer.rs", FULL_SERVER);
        f.write("tests/basic_events.rs", SHARED);
        f.write(STREAM_LANE, "// lane\n");
        f.write("corpus/MANIFEST.txt", "fx-one.json\n");
        let inputs = Inputs {
            streams_manifest: Some(f.root.join("corpus/MANIFEST.txt")),
            ..Inputs::under(&f.root)
        };
        let report = analyze(&inputs, Mode::Pending);
        assert!(
            has(
                &report.problems,
                "SPEC-MAP fixture fx-two is not in the stream corpus"
            ),
            "{:?}",
            report.problems
        );
        let aaa = report.rows.iter().find(|r| r.rule.id == "AAA-01");
        let evidence = format!("{:?}", aaa.map(|r| &r.halves));
        assert!(evidence.contains("fx-one"), "{evidence}");
        assert!(!evidence.contains("fx-two"), "{evidence}");
    }

    #[test]
    fn malformed_overrides_is_a_problem() {
        let f = Fixture::new("bad-overrides");
        f.write("tests/a_client.rs", FULL_CLIENT);
        f.write("tests/a_producer.rs", FULL_SERVER);
        f.write("tests/basic_events.rs", SHARED);
        f.write(STREAM_LANE, "// lane\n");
        f.write(
            crate::overrides::OVERRIDES_FILE,
            "[[override]]\nfixture = 1\n",
        );
        let p = f.problems(Mode::Pending);
        assert!(has(&p, "rust-lane-overrides"), "{p:?}");
    }

    #[test]
    fn function_after_accepts_restricted_visibility() {
        for decl in [
            "pub fn t() {}",
            "pub(crate) fn t() {}",
            "pub(super) fn t() {}",
            "pub(in crate::x) async fn t() {}",
        ] {
            assert_eq!(
                function_after(&["/// covers: AAA-01", "#[test]", decl], 0),
                Some(("t".to_owned(), true)),
                "{decl}"
            );
        }
    }

    #[test]
    fn function_after_does_not_count_an_ignored_test() {
        for ignore in ["#[ignore]", "#[ignore = \"slow\"]"] {
            assert_eq!(
                function_after(&["/// covers: AAA-01", "#[test]", ignore, "fn t() {}"], 0),
                Some(("t".to_owned(), false)),
                "{ignore}"
            );
        }
        // Ignored only in some configuration: it runs in the others.
        assert_eq!(
            function_after(
                &[
                    "/// covers: AAA-01",
                    "#[test]",
                    "#[cfg_attr(miri, ignore)]",
                    "fn t() {}"
                ],
                0
            ),
            Some(("t".to_owned(), true))
        );
    }

    #[test]
    fn a_covers_tag_on_an_ignored_test_is_a_problem_and_covers_nothing() {
        let f = Fixture::new("ignored");
        f.write(
            "tests/a_client.rs",
            "/// covers: AAA-01, BBB-01\n#[test]\n#[ignore = \"slow\"]\nfn t() {}\n",
        );
        f.write("tests/a_producer.rs", FULL_SERVER);
        f.write("tests/basic_events.rs", SHARED);
        let p = f.problems(Mode::NoPending);
        assert!(has(&p, "not #[ignore]d"), "{p:?}");
        let report = analyze(&Inputs::under(&f.root), Mode::Pending);
        let aaa = report
            .rows
            .iter()
            .find(|r| r.rule.id == "AAA-01")
            .expect("AAA-01");
        assert!(!aaa.covered(), "an ignored test covers nothing");
    }

    #[test]
    fn function_after_counts_only_real_tests_that_compile() {
        // Not a test at all: "test" appears only in the attribute's text.
        for attr in [
            "#[allow(dead_code)] // test helper",
            "#[cfg_attr(not(test), allow(unused))]",
            "#[doc = \"used by the tests\"]",
        ] {
            assert_eq!(
                function_after(&["/// covers: AAA-01", attr, "fn helper() {}"], 0),
                Some(("helper".to_owned(), false)),
                "{attr}"
            );
        }
        // A real test that a cfg compiles out (or that no static reading can
        // vouch for) does not run.
        for cfg in [
            "#[cfg(any())]",
            "#[cfg(not(all()))]",
            "#[cfg(not(test))]",
            "#[cfg(all(unix, windows))]",
            "#[cfg(all(feature = \"client\", target_os = \"linux\"))]",
            "#[cfg(",
        ] {
            assert_eq!(
                function_after(&["/// covers: AAA-01", "#[test]", cfg, "fn t() {}"], 0),
                Some(("t".to_owned(), false)),
                "{cfg}"
            );
        }
        // Still counted.
        for attrs in [
            &["#[test]", "#[cfg(feature = \"proto\")]"][..],
            &[
                "#[test]",
                "#[cfg(all(feature = \"server\", feature = \"axum\"))]",
            ][..],
            &["#[cfg(test)]", "#[test]"][..],
            &["#[tokio::test(flavor = \"current_thread\")]"][..],
            &["#[tokio::test]"][..],
            &["#[test]", "#[cfg_attr(miri, ignore)]"][..],
        ] {
            let mut lines = vec!["/// covers: AAA-01"];
            lines.extend_from_slice(attrs);
            lines.push("fn t() {}");
            assert_eq!(
                function_after(&lines, 0),
                Some(("t".to_owned(), true)),
                "{attrs:?}"
            );
        }
        // `#[ignore_me]` is not `#[ignore]`.
        assert_eq!(
            function_after(
                &["/// covers: AAA-01", "#[test]", "#[ignore_me]", "fn t() {}"],
                0
            ),
            Some(("t".to_owned(), true))
        );
    }

    #[test]
    fn a_covers_tag_in_a_file_compiled_out_by_an_inner_cfg_covers_nothing() {
        let f = Fixture::new("inner-cfg");
        f.write(
            "tests/a_client.rs",
            "//! Client tests.\n#![cfg(any())]\n\n/// covers: AAA-01, BBB-01\n#[test]\nfn t() {}\n",
        );
        f.write("tests/a_producer.rs", FULL_SERVER);
        f.write("tests/basic_events.rs", SHARED);
        let p = f.problems(Mode::NoPending);
        assert!(has(&p, "a #[test] function that runs"), "{p:?}");
        let report = analyze(&Inputs::under(&f.root), Mode::Pending);
        let aaa = report
            .rows
            .iter()
            .find(|r| r.rule.id == "AAA-01")
            .expect("AAA-01");
        assert!(!aaa.covered(), "a compiled-out file covers nothing");

        // A feature-gated file still counts.
        let g = Fixture::new("inner-cfg-feature");
        g.write(
            "tests/a_client.rs",
            "#![cfg(feature = \"client\")]\n/// covers: AAA-01, BBB-01\n#[test]\nfn t() {}\n",
        );
        g.write("tests/a_producer.rs", FULL_SERVER);
        g.write("tests/basic_events.rs", SHARED);
        let p = g.problems(Mode::NoPending);
        assert!(!has(&p, "a #[test] function that runs"), "{p:?}");
    }

    /// The scan reads past a multi-line inner attribute (rustfmt wraps a long
    /// `#![allow(..)]`) and an inner block doc, so a later `#![cfg(..)]` is
    /// still judged.
    #[test]
    fn an_inner_cfg_after_a_multi_line_attribute_or_block_doc_is_judged() {
        let cases: &[(&str, &[&str])] = &[
            (
                "multi-line allow",
                &[
                    "#![allow(clippy::unwrap_used,",
                    "    clippy::expect_used)]",
                    "#![cfg(any())]",
                ],
            ),
            (
                "multi-line cfg_attr",
                &[
                    "#![cfg_attr(",
                    "    feature = \"client\",",
                    "    doc = \"x [y]\"",
                    ")]",
                    "#![cfg(any())]",
                ],
            ),
            (
                "block doc",
                &["/*! Client tests.", "", "   More. */", "#![cfg(any())]"],
            ),
            (
                "one-line block doc",
                &["/*! Client tests. */", "#![cfg(any())]"],
            ),
        ];
        for (name, lines) in cases {
            assert!(!file_compiles_in(lines), "{name}");
        }
        // The first item still ends the scan.
        assert!(file_compiles_in(&[
            "#![allow(",
            "  dead_code)]",
            "use x;",
            "#![cfg(any())]"
        ]));
        assert!(file_compiles_in(&[
            "#![allow(dead_code)]",
            "#![cfg(feature = \"client\")]"
        ]));
    }

    #[test]
    fn misplaced_half_tag_is_reported_once_for_a_two_half_row() {
        let f = Fixture::new("tag-once");
        f.write(
            "tests/a_client.rs",
            "/// covers: AAA-01\n#[test]\nfn t() {}\n/// covers(server): BBB-01\n#[test]\nfn u() {}\n",
        );
        f.write("tests/a_producer.rs", FULL_SERVER);
        f.write("tests/basic_events.rs", SHARED);
        let p = f.problems(Mode::Pending);
        let n = p
            .iter()
            .filter(|l| l.contains("covers(server) in a file that cannot prove that half"))
            .count();
        assert_eq!(n, 1, "{p:?}");
    }

    #[test]
    fn unit_test_tags_in_src_prove_their_side() {
        let f = Fixture::new("src");
        f.write(
            "tests/a_client.rs",
            "/// covers: AAA-01\n#[test]\nfn t() {}\n",
        );
        // BBB-01: client half from src/client, server half from src/server.
        f.write(
            "src/client/x.rs",
            "#[cfg(test)]\nmod tests {\n    /// covers: BBB-01\n    #[test]\n    fn c() {}\n}\n",
        );
        f.write(
            "src/server/y.rs",
            "#[cfg(test)]\nmod tests {\n    /// covers: BBB-01\n    #[test]\n    fn s() {}\n}\n",
        );
        // DDD-01: a shared module proves both halves with one tag.
        f.write(
            "src/patch.rs",
            "#[cfg(test)]\nmod tests {\n    /// covers: DDD-01\n    #[test]\n    fn p() {}\n}\n",
        );
        assert_eq!(f.problems(Mode::NoPending), Vec::<String>::new());
        // A src/client tag cannot prove a server half.
        f.write("src/server/y.rs", "");
        f.write(
            "src/client/z.rs",
            "/// covers(server): BBB-01\n#[test]\nfn z() {}\n",
        );
        let p = f.problems(Mode::Pending);
        assert!(has(&p, "BBB-01 (server): no covering test"), "{p:?}");
        assert!(has(&p, "cannot prove that half"), "{p:?}");
        // Outside a test function a src tag is an error, not evidence.
        f.write(
            "src/client/z.rs",
            "/// covers: BBB-01\npub fn helper() {}\n",
        );
        assert!(has(
            &f.problems(Mode::Pending),
            "must be on a #[test] function"
        ));
    }

    #[test]
    fn a_qualified_tag_missing_its_paren_or_colon_is_malformed() {
        for line in ["/// covers(client: LIFE-41", "/// covers(client) LIFE-41"] {
            assert!(
                matches!(parse_tag_line(line), Some(Err(ref e)) if e.contains("malformed covers line")),
                "{line}: {:?}",
                parse_tag_line(line)
            );
        }
        // Prose that merely starts with the word is not a tag.
        assert_eq!(parse_tag_line("/// covers the rest of the run"), None);
        assert!(matches!(
            parse_tag_line("/// covers(client): LIFE-41"),
            Some(Ok((Some(Half::Client), _)))
        ));
    }

    #[test]
    fn wrong_file_unknown_ids_and_malformed_tags() {
        let f = Fixture::new("wrong");
        f.write("tests/a_client.rs", "/// covers: BBB-01\n#[test]\nfn t() {}\n/// covers: EEE-01, QQQ-09\n#[test]\nfn u() {}\n/// covers: lower-1\nfn v() {}\n/// covers: AAA-01\nconst X: u8 = 1;\n");
        f.write(
            "tests/other_client.rs",
            "/// covers: AAA-01\n#[test]\nfn w() {}\n",
        );
        let p = f.problems(Mode::Pending);
        assert!(has(&p, "covers EEE-01 which is not a gated row"), "{p:?}");
        assert!(has(&p, "covers QQQ-09 which is not a gated row"), "{p:?}");
        assert!(has(&p, "malformed covers line"), "{p:?}");
        assert!(has(&p, "covers tag is not on a function"), "{p:?}");
        assert!(
            has(&p, "AAA-01 (client): tagged only outside tests/a_client.rs"),
            "{p:?}"
        );
    }

    #[test]
    fn fixture_evidence_needs_the_stream_lane_and_no_override() {
        let f = Fixture::new("fixtures");
        f.write(
            "tests/a_client.rs",
            "/// covers: BBB-01\n#[test]\nfn t() {}\n",
        );
        f.write("tests/a_producer.rs", FULL_SERVER);
        f.write("tests/basic_events.rs", SHARED);
        assert!(has(
            &f.problems(Mode::Pending),
            "AAA-01 (client): no covering test"
        ));
        f.write(STREAM_LANE, "// lane\n");
        assert_eq!(f.problems(Mode::Pending), Vec::<String>::new());
        let over = "[[override]]\nfixture = \"{F}\"\nplan = \"A2\"\nintentional = \"x\"\n[override.expect]\noutcome = \"completed\"\n";
        f.write(
            crate::overrides::OVERRIDES_FILE,
            &format!(
                "{}{}",
                over.replace("{F}", "fx-one"),
                over.replace("{F}", "fx-two")
            ),
        );
        assert!(has(
            &f.problems(Mode::Pending),
            "AAA-01 (client): no covering test"
        ));
    }

    #[test]
    fn docs_evidence_only_for_assistable_rows() {
        let f = Fixture::new("docs");
        f.write("tests/a_client.rs", FULL_CLIENT);
        f.write("tests/a_producer.rs", FULL_SERVER);
        f.write("tests/basic_events.rs", SHARED);
        // CCC-01 is covered by its docs alone.
        assert_eq!(f.problems(Mode::NoPending), Vec::<String>::new());
        f.write("src/doc.rs", "//! nothing here\n");
        assert!(has(
            &f.problems(Mode::Pending),
            "does not exist or does not name the row"
        ));
        let text = std::fs::read_to_string(f.root.join("docs/compliance/compliance.toml"))
            .unwrap_or_default();
        f.write(
            "docs/compliance/compliance.toml",
            &text.replace(
                "role = \"client\"\narea = \"x\"\nscope = \"1.0\"\n",
                "role = \"client\"\narea = \"x\"\nscope = \"1.0\"\ndocs = [\"src/doc.rs\"]\n",
            ),
        );
        assert!(has(
            &f.problems(Mode::Pending),
            "AAA-01: docs evidence is accepted for sdk-assistable rows only"
        ));
    }

    #[test]
    fn spec_map_and_compliance_must_agree() {
        let f = Fixture::new("agree");
        let text = std::fs::read_to_string(f.root.join("docs/SPEC-MAP.md")).unwrap_or_default();
        f.write(
            "docs/SPEC-MAP.md",
            &text.replace(
                "| AAA-01 | MUST | ownable | client |",
                "| AAA-01 | MUST | ownable | server |",
            ),
        );
        let p = f.problems(Mode::Pending);
        assert!(has(&p, "AAA-01: SPEC-MAP says ownable/server"), "{p:?}");
    }

    #[test]
    fn rs_mentions_finds_file_names_and_paths() {
        assert_eq!(
            rs_mentions("tested (a_client.rs, src/x/y.rs; also b-c.rs). Not .rs or tests/.rs"),
            ["a_client.rs", "src/x/y.rs", "b-c.rs"]
        );
        assert!(rs_mentions("no files, rust, .rsx, x.rsa").is_empty());
    }

    #[test]
    fn reasons_may_only_cite_files_that_exist() {
        let f = Fixture::new("reasons");
        f.write("tests/a_client.rs", FULL_CLIENT);
        f.write("tests/a_producer.rs", FULL_SERVER);
        f.write("tests/basic_events.rs", SHARED);
        let text = std::fs::read_to_string(f.root.join("docs/compliance/compliance.toml"))
            .unwrap_or_default();
        f.write(
            "docs/compliance/compliance.toml",
            &text
                .replacen(
                    "scope = \"1.0\"\n",
                    "scope = \"1.0\"\nreason = \"Tested in a_client.rs and src/doc.rs.\"\n",
                    1,
                )
                .replace(
                    "class = \"informative\"\nrole = \"n/a\"\narea = \"y\"\nscope = \"1.0\"\n",
                    "class = \"informative\"\nrole = \"n/a\"\narea = \"y\"\nscope = \"1.0\"\n\
                     reason = \"Both halves tested (old_suite.rs; tests/gone.rs; docs/x.rs).\"\n",
                ),
        );
        let p = f.problems(Mode::NoPending);
        assert!(!has(&p, "AAA-01: reason cites"), "{p:?}");
        assert!(
            has(&p, "EEE-01: reason cites old_suite.rs, which is not a file"),
            "{p:?}"
        );
        assert!(has(&p, "EEE-01: reason cites tests/gone.rs"), "{p:?}");
        assert!(has(&p, "EEE-01: reason cites docs/x.rs"), "{p:?}");
        assert_eq!(p.len(), 3, "{p:?}");
    }

    /// The real repository data is self-consistent (whatever is covered).
    #[test]
    fn repository_inputs_parse_and_agree() {
        let inputs = Inputs::repo();
        let rules = rules(&inputs.compliance).unwrap_or_else(|e| panic!("{e}"));
        let mapped = mapped_rows(&inputs.spec_map).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(rules.iter().filter(|r| r.gated()).count(), mapped.len());
        let report = analyze(&inputs, Mode::Pending);
        let config: Vec<&String> = report
            .problems
            .iter()
            .filter(|p| {
                p.contains("SPEC-MAP says")
                    || p.contains("cannot be mapped")
                    || p.contains("not a server-side")
                    || p.contains("not a client-side")
            })
            .collect();
        assert!(config.is_empty(), "{config:?}");
    }
}
