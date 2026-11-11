use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

/// Request-scoped values keyed by type: the authenticated principal, the
/// tenant, trace context. Put them in at the HTTP layer
/// ([`HttpRequest::with_extensions`](super::http::HttpRequest::with_extensions),
/// [`AguiRequest::with_extensions`][super::axum::AguiRequest::with_extensions]
/// or [`Runner::run`](super::Runner::run)); the agent reads them with
/// [`RunContext::extensions`](super::RunContext::extensions).
///
/// Values are shared (`Arc`), so cloning the map is cheap.
#[cfg_attr(
    not(feature = "axum"),
    doc = "",
    doc = "[super::axum::AguiRequest::with_extensions]: https://docs.rs/agui-sdk/latest/agui_sdk/server/axum/struct.AguiRequest.html#method.with_extensions"
)]
#[derive(Clone, Default)]
pub struct Extensions {
    map: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl Extensions {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `value`, replacing any earlier value of the same type.
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) {
        self.map.insert(TypeId::of::<T>(), Arc::new(value));
    }

    /// The value of type `T`, if one was stored.
    #[must_use]
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|value| value.downcast_ref::<T>())
    }

    /// Whether nothing is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl std::fmt::Debug for Extensions {
    /// Counts only; values are application data.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Values are opaque (and may be secrets): show only how many.
        f.debug_struct("Extensions")
            .field("len", &self.map.len())
            .finish()
    }
}
