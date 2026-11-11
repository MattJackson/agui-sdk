//! The Rust lane's corpus overrides (`tests/corpus/rust-lane-overrides.toml`).
//!
//! The upstream corpus records where its own lanes differ in
//! `expectOverrides.<lane>`. This repository cannot edit upstream fixtures,
//! so the Rust lane keeps its overrides in one local file, with the same
//! discipline: every entry says why (`intentional`, required), names the plan
//! item that allows it, and lists exactly which expectation keys it replaces
//! or removes. The only plan item allowed today is `A2` (D3: an unknown
//! closed-set value is stripped with a warning, as the spec says, where the
//! upstream fixture pins the TypeScript lane's admitted gap).
//!
//! ```toml
//! [[override]]
//! fixture = "unknown-enum-value-role-fatal"
//! plan = "A2"
//! intentional = "why the Rust lane differs"
//! remove = ["errorContains"]          # keys dropped from the base expect
//! [override.expect]                  # keys replaced (or added)
//! outcome = "completed"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::corpus::StreamFixture;
use crate::oracle::EXPECT_KEYS;

/// Plan items that may justify an override. Adding one is a reviewed change
/// to this list, not to the data file.
pub const ALLOWED_PLANS: &[&str] = &["A2"];

/// Location of the override file, relative to the workspace root.
pub const OVERRIDES_FILE: &str = "tests/corpus/rust-lane-overrides.toml";

/// One override.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Override {
    /// Fixture name.
    pub fixture: String,
    /// Plan item allowing it (one of [`ALLOWED_PLANS`]).
    pub plan: String,
    /// Why the Rust lane differs. Required and non-empty.
    pub intentional: String,
    /// Base expectation keys this lane drops.
    pub remove: Vec<String>,
    /// Expectation keys this lane replaces or adds.
    pub expect: Map<String, Value>,
}

/// Every override, by fixture name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overrides {
    entries: BTreeMap<String, Override>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileDoc {
    #[serde(default, rename = "override")]
    entries: Vec<RawOverride>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOverride {
    fixture: String,
    plan: String,
    #[serde(default)]
    intentional: String,
    #[serde(default)]
    remove: Vec<String>,
    #[serde(default)]
    expect: toml::Table,
}

fn toml_to_json(value: toml::Value) -> Result<Value, String> {
    Ok(match value {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::from(i),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .ok_or_else(|| format!("non-finite number {f}"))?,
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(toml_to_json)
                .collect::<Result<_, _>>()?,
        ),
        toml::Value::Table(table) => Value::Object(
            table
                .into_iter()
                .map(|(k, v)| toml_to_json(v).map(|v| (k, v)))
                .collect::<Result<_, _>>()?,
        ),
    })
}

impl Overrides {
    /// Parses and validates override text.
    ///
    /// # Errors
    ///
    /// Every problem: unknown fields, a missing or empty `intentional`, a plan
    /// item not in [`ALLOWED_PLANS`], an entry that changes nothing, an
    /// unknown expectation key (fail closed), a key both removed and
    /// replaced, and duplicate fixtures.
    pub fn parse(text: &str) -> Result<Self, Vec<String>> {
        let doc: FileDoc =
            toml::from_str(text).map_err(|e| vec![format!("{OVERRIDES_FILE}: {e}")])?;
        let mut problems = Vec::new();
        let mut entries = BTreeMap::new();
        for raw in doc.entries {
            let who = format!("{OVERRIDES_FILE}: {}", raw.fixture);
            if raw.intentional.trim().is_empty() {
                problems.push(format!("{who}: `intentional` is required and must say why"));
            }
            if !ALLOWED_PLANS.contains(&raw.plan.as_str()) {
                problems.push(format!(
                    "{who}: plan {:?} does not allow an override (allowed: {ALLOWED_PLANS:?})",
                    raw.plan
                ));
            }
            if raw.remove.is_empty() && raw.expect.is_empty() {
                problems.push(format!("{who}: changes no expectation"));
            }
            for key in raw.remove.iter().chain(raw.expect.keys()) {
                if !EXPECT_KEYS.contains(&key.as_str()) {
                    problems.push(format!("{who}: unknown expectation key {key:?}"));
                }
            }
            for key in &raw.remove {
                if raw.expect.contains_key(key) {
                    problems.push(format!("{who}: {key:?} is both removed and replaced"));
                }
            }
            let mut expect = Map::new();
            for (key, value) in raw.expect {
                match toml_to_json(value) {
                    Ok(v) => {
                        expect.insert(key, v);
                    }
                    Err(e) => problems.push(format!("{who}: {key}: {e}")),
                }
            }
            let entry = Override {
                fixture: raw.fixture.clone(),
                plan: raw.plan,
                intentional: raw.intentional,
                remove: raw.remove,
                expect,
            };
            if entries.insert(raw.fixture, entry).is_some() {
                problems.push(format!("{who}: more than one override"));
            }
        }
        if problems.is_empty() {
            Ok(Overrides { entries })
        } else {
            Err(problems)
        }
    }

    /// The override file's path.
    pub fn path() -> PathBuf {
        crate::workspace_root().join(OVERRIDES_FILE)
    }

    /// Loads [`OVERRIDES_FILE`]. A missing file means no overrides.
    ///
    /// # Panics
    ///
    /// When the file is unreadable or invalid.
    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    /// Loads an override file from `path`.
    ///
    /// # Panics
    ///
    /// When the file is unreadable or invalid.
    pub fn load_from(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => panic!("{}: {e}", path.display()),
        };
        Self::parse(&text).unwrap_or_else(|p| panic!("invalid overrides:\n  {}", p.join("\n  ")))
    }

    /// The override for `fixture`, if any.
    pub fn get(&self, fixture: &str) -> Option<&Override> {
        self.entries.get(fixture)
    }

    /// Every override.
    pub fn iter(&self) -> impl Iterator<Item = &Override> {
        self.entries.values()
    }

    /// Stale-entry check: every override must name a fixture in the corpus.
    ///
    /// # Errors
    ///
    /// One line per override whose fixture no longer exists.
    pub fn check_against(&self, fixtures: &[StreamFixture]) -> Result<(), Vec<String>> {
        let problems: Vec<String> = self
            .entries
            .keys()
            .filter(|name| !fixtures.iter().any(|f| &f.name == *name))
            .map(|name| {
                format!("{OVERRIDES_FILE}: {name}: no such fixture in the corpus (stale override)")
            })
            .collect();
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }

    /// The expectations the Rust lane holds `fixture` to: the base `expect`
    /// with this lane's override (if any) applied. Upstream
    /// `expectOverrides` blocks describe other clients and are ignored, except
    /// a `rust` block, which applies like a local override and must also
    /// carry `intentional`.
    ///
    /// # Errors
    ///
    /// A `rust` upstream override without `intentional`, or with an unknown key.
    pub fn resolve(&self, fixture: &StreamFixture) -> Result<Map<String, Value>, Vec<String>> {
        let mut expect = fixture.expect().clone();
        let mut problems = Vec::new();
        if let Some(upstream) = fixture
            .document
            .pointer("/expectOverrides/rust")
            .and_then(Value::as_object)
        {
            if upstream
                .get("intentional")
                .and_then(Value::as_str)
                .map_or(true, |s| s.trim().is_empty())
            {
                problems.push(format!(
                    "{}: expectOverrides.rust has no `intentional`",
                    fixture.name
                ));
            }
            for (key, value) in upstream {
                if key == "intentional" {
                    continue;
                }
                if !EXPECT_KEYS.contains(&key.as_str()) {
                    problems.push(format!(
                        "{}: expectOverrides.rust: unknown expectation key {key:?}",
                        fixture.name
                    ));
                }
                expect.insert(key.clone(), value.clone());
            }
        }
        if let Some(local) = self.get(&fixture.name) {
            for key in &local.remove {
                expect.remove(key);
            }
            for (key, value) in &local.expect {
                expect.insert(key.clone(), value.clone());
            }
        }
        if problems.is_empty() {
            Ok(expect)
        } else {
            Err(problems)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
[[override]]
fixture = "f"
plan = "A2"
intentional = "the spec wins (D3)"
remove = ["errorContains"]
[override.expect]
outcome = "completed"
warnings = ["/role"]
messages = [{ id = "m", role = "assistant" }]
"#;

    #[test]
    fn a_valid_override_parses_into_json() {
        let o = Overrides::parse(GOOD).unwrap_or_else(|p| panic!("{p:?}"));
        let e = o.get("f").unwrap_or_else(|| panic!("missing"));
        assert_eq!(e.expect["messages"][0]["role"], "assistant");
        assert_eq!(e.remove, vec!["errorContains".to_owned()]);
    }

    #[test]
    fn intentional_is_required() {
        let text = GOOD.replace(
            "intentional = \"the spec wins (D3)\"",
            "intentional = \"  \"",
        );
        let p = Overrides::parse(&text).err().unwrap_or_default();
        assert!(
            p.iter().any(|l| l.contains("`intentional` is required")),
            "{p:?}"
        );
        let text = GOOD.replace("intentional = \"the spec wins (D3)\"\n", "");
        assert!(Overrides::parse(&text).is_err());
    }

    #[test]
    fn unknown_plans_keys_and_fields_fail_closed() {
        let p = Overrides::parse(&GOOD.replace("\"A2\"", "\"B7\""))
            .err()
            .unwrap_or_default();
        assert!(p.iter().any(|l| l.contains("does not allow")), "{p:?}");
        let p = Overrides::parse(&GOOD.replace("warnings", "warningz"))
            .err()
            .unwrap_or_default();
        assert!(
            p.iter()
                .any(|l| l.contains("unknown expectation key \"warningz\"")),
            "{p:?}"
        );
        assert!(Overrides::parse(&GOOD.replace("plan =", "extra = 1\nplan =")).is_err());
        let both = GOOD.replace("remove = [\"errorContains\"]", "remove = [\"outcome\"]");
        let p = Overrides::parse(&both).err().unwrap_or_default();
        assert!(
            p.iter().any(|l| l.contains("both removed and replaced")),
            "{p:?}"
        );
    }

    #[test]
    fn the_repository_file_is_valid() {
        let _ = Overrides::load();
    }
}
