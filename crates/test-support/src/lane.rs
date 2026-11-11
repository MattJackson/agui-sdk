//! Running a lane over a whole corpus: every item is checked (a panic in one
//! is caught and reported, not fatal to the rest) and the lane fails once,
//! listing every failing item.
//!
//! A lane over zero items fails, and skips are pinned: [`check_each`] allows
//! none, [`check_each_skipping`] exactly the named items. A regression that
//! turns passes into skips (an encoder that starts refusing everything)
//! therefore fails the lane instead of leaving it green.

use std::collections::BTreeSet;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Checks every item and panics with the full list of failures. No item may
/// be skipped; see [`check_each_skipping`].
///
/// `check` returns `Ok(Some(reason))` for a deliberate, computed skip,
/// `Ok(None)` for a pass, and `Err` for a failure.
///
/// # Panics
///
/// When `items` is empty, when any item fails, panics or is skipped.
pub fn check_each<T>(
    lane: &str,
    items: &[T],
    name: impl Fn(&T) -> String,
    check: impl Fn(&T) -> Result<Option<String>, String>,
) {
    check_each_skipping(lane, items, name, check, &[]);
}

/// [`check_each`], where exactly the items named in `expected_skips` are
/// skipped (printed with their reasons). The set of skipped items must equal
/// `expected_skips`: an extra skip, or a listed item that now runs, fails the
/// lane, so the list (and its length) is the lane's pinned skip budget.
///
/// # Panics
///
/// When `items` is empty, when any item fails or panics, or when the skipped
/// items are not exactly `expected_skips`.
pub fn check_each_skipping<T>(
    lane: &str,
    items: &[T],
    name: impl Fn(&T) -> String,
    check: impl Fn(&T) -> Result<Option<String>, String>,
    expected_skips: &[&str],
) {
    assert!(
        !items.is_empty(),
        "{lane}: no items; a lane over an empty corpus proves nothing"
    );
    let mut failures = Vec::new();
    let mut skipped = BTreeSet::new();
    for item in items {
        let id = name(item);
        match catch_unwind(AssertUnwindSafe(|| check(item))) {
            Ok(Ok(None)) => {}
            Ok(Ok(Some(reason))) => {
                eprintln!("{lane}: skipped {id}: {reason}");
                skipped.insert(id);
            }
            Ok(Err(e)) => failures.push(format!("{id}: {e}")),
            Err(panic) => {
                let msg = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_else(|| "panic".to_owned());
                failures.push(format!("{id}: panicked: {msg}"));
            }
        }
    }
    eprintln!(
        "{lane}: {} item(s), {} passed, {} skipped, {} failed",
        items.len(),
        items.len() - skipped.len() - failures.len(),
        skipped.len(),
        failures.len()
    );
    let expected: BTreeSet<String> = expected_skips.iter().map(|s| (*s).to_owned()).collect();
    for id in skipped.difference(&expected) {
        failures.push(format!(
            "{id}: skipped, but not in the lane's allowed skips"
        ));
    }
    for id in expected.difference(&skipped) {
        failures.push(format!(
            "{id}: listed as an allowed skip but was not skipped (missing or now runs): update the list"
        ));
    }
    if expected.len() != expected_skips.len() {
        failures.push("the allowed-skip list names an item twice".to_owned());
    }
    if !failures.is_empty() {
        panic!(
            "{lane}: {} failure(s):\n  {}",
            failures.len(),
            failures.join("\n  ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_every_failure_and_survives_panics() {
        let r = catch_unwind(|| {
            check_each(
                "demo",
                &[1, 2, 3, 4],
                |n| n.to_string(),
                |n| match n {
                    1 => Ok(None),
                    2 => Ok(Some("computed".into())),
                    3 => Err("wrong".into()),
                    _ => panic!("boom"),
                },
            )
        });
        let msg = r
            .err()
            .and_then(|p| p.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        assert!(
            msg.contains("3 failure(s)")
                && msg.contains("2: skipped, but not in the lane's allowed skips")
                && msg.contains("3: wrong")
                && msg.contains("4: panicked: boom"),
            "{msg}"
        );
    }

    fn message(r: std::thread::Result<()>) -> String {
        r.err()
            .and_then(|p| p.downcast_ref::<String>().cloned())
            .unwrap_or_default()
    }

    #[test]
    fn an_empty_lane_fails() {
        let msg = message(catch_unwind(|| {
            check_each("empty", &[] as &[u8], |n| n.to_string(), |_| Ok(None))
        }));
        assert!(msg.contains("no items"), "{msg}");
    }

    #[test]
    fn skips_must_be_exactly_the_pinned_list() {
        let skip_evens = |n: &u8| Ok((n % 2 == 0).then(|| "even".to_owned()));
        // Exactly the pinned list: green.
        check_each_skipping(
            "pinned",
            &[1, 2, 3, 4],
            |n| n.to_string(),
            skip_evens,
            &["2", "4"],
        );
        // One more skip than pinned.
        let msg = message(catch_unwind(|| {
            check_each_skipping(
                "extra",
                &[1, 2, 3, 4],
                |n| n.to_string(),
                skip_evens,
                &["2"],
            )
        }));
        assert!(
            msg.contains("4: skipped, but not in the lane's allowed skips"),
            "{msg}"
        );
        // A pinned skip that now runs.
        let msg = message(catch_unwind(|| {
            check_each_skipping(
                "stale",
                &[1, 2, 3, 4],
                |n| n.to_string(),
                skip_evens,
                &["2", "3", "4"],
            )
        }));
        assert!(
            msg.contains("3: listed as an allowed skip but was not skipped"),
            "{msg}"
        );
    }
}
