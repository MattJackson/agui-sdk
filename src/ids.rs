//! Identifier newtypes.
//!
//! Every identifier on the wire is a string. Each kind gets its own type so a
//! run id cannot be passed where a message id is expected, and each is backed
//! by an `Arc<str>`: cloning one (which the reducer, the verifier and the
//! chunk expander all do per event) is a reference-count increment, not an
//! allocation. Serialization is transparent: `MessageId::new("m1")` is the
//! JSON string `"m1"`.
//!
//! The protocol attaches no format to identifiers. [`ThreadId::random`] and
//! friends mint 128-bit random hex strings for callers that need a fresh one
//! (a new thread or run); they use the standard library's randomly seeded
//! hasher plus a process-wide counter, so no extra dependency is pulled in.
//! They are unique, not unguessable: never use one as a secret.
//!
//! Spec rules: BASIC-13 (thread ids are minted by the application), BASIC-14
//! (run ids are never reused on a thread), SUB-03 (subagent run ids are never
//! reused within a run).

use std::borrow::Borrow;
use std::cell::RefCell;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

/// 32 lowercase hex digits: two 64-bit outputs of std's randomly keyed
/// SipHash over a process-wide counter and the clock.
///
/// Each `RandomState::new()` carries fresh keys (per-thread random seed, bumped
/// on every call), the counter separates calls within one keying, and the
/// clock separates processes that happen to share a seed.
fn random_hex() -> Arc<str> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let keys = RandomState::new();

    let mut high = keys.build_hasher();
    high.write_u64(count);
    high.write_u128(nanos);
    let high = high.finish();

    let mut low = keys.build_hasher();
    low.write_u64(high);
    low.write_u64(!count);
    low.write_u128(nanos);
    let low = low.finish();

    Arc::from(format!("{high:016x}{low:016x}"))
}

/// Identifiers recently read on this thread, most streamed ones repeat on
/// every event of an item (`messageId` on each content delta, `toolCallId` on
/// each argument delta). A repeat shares the earlier buffer instead of
/// allocating its own: equal text, so the result is indistinguishable.
const RECENT_IDS: usize = 4;

/// Longer identifiers are not kept, so a peer cannot pin a large string in
/// the cache.
const RECENT_ID_MAX_LEN: usize = 128;

struct RecentIds {
    slots: [Option<Arc<str>>; RECENT_IDS],
    /// The slot the next miss replaces (round robin).
    next: usize,
}

thread_local! {
    static RECENT: RefCell<RecentIds> = const {
        RefCell::new(RecentIds {
            slots: [None, None, None, None],
            next: 0,
        })
    };
}

/// `id` as a shared buffer: a recent equal one if there is one, else a new
/// allocation (remembered when short).
fn shared_id(id: &str) -> Arc<str> {
    if id.len() > RECENT_ID_MAX_LEN {
        return Arc::from(id);
    }
    RECENT
        .try_with(|recent| {
            let Ok(mut recent) = recent.try_borrow_mut() else {
                return Arc::from(id);
            };
            if let Some(hit) = recent.slots.iter().flatten().find(|s| ***s == *id) {
                return Arc::clone(hit);
            }
            let fresh: Arc<str> = Arc::from(id);
            let at = recent.next;
            recent.slots[at] = Some(Arc::clone(&fresh));
            recent.next = (at + 1) % RECENT_IDS;
            fresh
        })
        .unwrap_or_else(|_| Arc::from(id))
}

/// Reads an identifier straight into its shared buffer: at most one
/// allocation (none for a recently seen id), where going through `String`
/// would allocate twice.
struct IdVisitor;

impl<'de> Visitor<'de> for IdVisitor {
    type Value = Arc<str>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an identifier string")
    }

    fn visit_str<E: de::Error>(self, id: &str) -> Result<Arc<str>, E> {
        Ok(shared_id(id))
    }
}

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(Arc<str>);

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                deserializer.deserialize_str(IdVisitor).map(Self)
            }
        }

        impl $name {
            /// Wraps an existing identifier. Allocates once; later clones are free.
            pub fn new(id: impl Into<Arc<str>>) -> Self {
                Self(id.into())
            }

            /// Mints a fresh 128-bit random identifier rendered as 32 lowercase
            /// hex digits. Unique within and across processes with
            /// overwhelming probability; not suitable as a secret.
            pub fn random() -> Self {
                Self(random_hex())
            }

            /// The identifier text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// The shared backing string, for callers that keep their own index.
            #[must_use]
            pub fn as_arc(&self) -> &Arc<str> {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name)).field(&&*self.0).finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(id: &str) -> Self {
                Self(Arc::from(id))
            }
        }

        impl From<String> for $name {
            fn from(id: String) -> Self {
                Self(Arc::from(id))
            }
        }

        impl From<Arc<str>> for $name {
            fn from(id: Arc<str>) -> Self {
                Self(id)
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                *self.0 == *other
            }
        }
    };
}

id_type! {
    /// Identifies a conversation. Minted by the application and stable across
    /// the thread's runs (BASIC-13).
    ThreadId
}

id_type! {
    /// Identifies one run. Never reused for another run on the same thread
    /// (BASIC-14); a re-run after truncation is a new run with a new id
    /// (TRN-11).
    RunId
}

id_type! {
    /// Identifies a message of any role, and also a reasoning span (spans and
    /// reasoning messages may share an id; each is checked against its own
    /// open set, REAS-12).
    MessageId
}

id_type! {
    /// Identifies a tool call.
    ToolCallId
}

id_type! {
    /// Identifies one subagent invocation within a run (SUB-03). Absence of
    /// this id on an attributable event means the parent agent (SUB-01).
    SubagentRunId
}

id_type! {
    /// Identifies an interrupt; unique within the run that raised it (INTR-04).
    InterruptId
}
