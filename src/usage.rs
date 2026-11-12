//! Token usage (`events/lifecycle.mdx`, "Token usage").
//!
//! [`TokenUsage`] is the wire type. [`TokenUsageBuilder`] is the producer helper
//! that keeps the accounting rules: parts are inside totals, a part reported
//! beside a smaller total is added in once, unknown counts stay absent rather
//! than zero, and the total is computed (LIFE-25..LIFE-32). Usage covers the
//! run's own model calls, subagents included, not separate runs (LIFE-34..LIFE-36).

sparse_record! {
    /// Token accounting for one provider and model.
    ///
    /// Every count is optional: absent means "not reported", which is different
    /// from a reported zero (LIFE-32, LIFE-33). The cache and reasoning counts are
    /// parts of the totals, never additions (LIFE-26). Producers should build this
    /// through [`TokenUsageBuilder`], which enforces the accounting rules.
    ///
    /// Schema: `$defs/TokenUsage`. Spec rules: LIFE-25..LIFE-36.
    pub struct TokenUsage {
        /// Lowercase vendor id, e.g. `anthropic` (RUNIN-14).
        pub provider: Option<String>,
        /// Model identifier.
        pub model: Option<String>,
        /// Every prompt token charged, cached or not (LIFE-25).
        pub input_tokens: Option<u64>,
        /// Every generated token, reasoning included (LIFE-25).
        pub output_tokens: Option<u64>,
        /// `input_tokens + output_tokens` (LIFE-29..LIFE-31).
        pub total_tokens: Option<u64>,
        /// Part of `output_tokens` (LIFE-26).
        pub reasoning_tokens: Option<u64>,
        /// Part of `input_tokens`, read from cache (LIFE-26).
        pub cached_input_tokens: Option<u64>,
        /// Part of `input_tokens`, written to cache; disjoint from the cached count (LIFE-26).
        pub cache_write_input_tokens: Option<u64>,
    }
}

impl TokenUsage {
    /// Starts a [`TokenUsageBuilder`]; the same as [`TokenUsageBuilder::new`].
    pub fn builder(provider: impl Into<String>, model: impl Into<String>) -> TokenUsageBuilder {
        TokenUsageBuilder::new(provider, model)
    }

    /// Sets [`provider`](Self::provider).
    #[must_use]
    pub fn with_provider(self, provider: impl Into<String>) -> Self {
        Self {
            provider: Some(provider.into()),
            ..self
        }
    }

    /// Sets [`model`](Self::model).
    #[must_use]
    pub fn with_model(self, model: impl Into<String>) -> Self {
        Self {
            model: Some(model.into()),
            ..self
        }
    }

    /// Sets [`input_tokens`](Self::input_tokens).
    #[must_use]
    pub fn with_input_tokens(self, input_tokens: u64) -> Self {
        Self {
            input_tokens: Some(input_tokens),
            ..self
        }
    }

    /// Sets [`output_tokens`](Self::output_tokens).
    #[must_use]
    pub fn with_output_tokens(self, output_tokens: u64) -> Self {
        Self {
            output_tokens: Some(output_tokens),
            ..self
        }
    }

    /// Sets [`total_tokens`](Self::total_tokens).
    #[must_use]
    pub fn with_total_tokens(self, total_tokens: u64) -> Self {
        Self {
            total_tokens: Some(total_tokens),
            ..self
        }
    }

    /// Sets [`reasoning_tokens`](Self::reasoning_tokens).
    #[must_use]
    pub fn with_reasoning_tokens(self, reasoning_tokens: u64) -> Self {
        Self {
            reasoning_tokens: Some(reasoning_tokens),
            ..self
        }
    }

    /// Sets [`cached_input_tokens`](Self::cached_input_tokens).
    #[must_use]
    pub fn with_cached_input_tokens(self, cached_input_tokens: u64) -> Self {
        Self {
            cached_input_tokens: Some(cached_input_tokens),
            ..self
        }
    }

    /// Sets [`cache_write_input_tokens`](Self::cache_write_input_tokens).
    #[must_use]
    pub fn with_cache_write_input_tokens(self, cache_write_input_tokens: u64) -> Self {
        Self {
            cache_write_input_tokens: Some(cache_write_input_tokens),
            ..self
        }
    }
}
/// Accumulates provider-reported counts into a rule-abiding [`TokenUsage`].
///
/// Feed it what the provider reports, call by call; it never invents a count.
///
/// ```
/// use agui_sdk::usage::TokenUsageBuilder;
/// let usage = TokenUsageBuilder::new("anthropic", "claude-x")
///     .input(1200, /* provider total includes cache reads: */ true)
///     .cached_input(1000)
///     .output(300, true)
///     .build();
/// assert_eq!(usage.total_tokens, Some(1500));
/// ```
#[derive(Debug, Clone, Default)]
#[must_use = "a builder does nothing until `build` is called"]
pub struct TokenUsageBuilder {
    usage: TokenUsage,
    cached_included: bool,
    output_includes_reasoning: bool,
}

impl TokenUsageBuilder {
    /// Starts accounting for one provider and model. `provider` is normalised
    /// to its lowercase vendor id (RUNIN-14).
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        let raw: String = provider.into();
        let changed = match crate::metadata::provider::normalize(&raw) {
            std::borrow::Cow::Borrowed(same) if same.len() == raw.len() => None,
            normalized => Some(normalized.into_owned()),
        };
        let provider = changed.unwrap_or(raw);
        Self {
            usage: TokenUsage {
                provider: Some(provider),
                model: Some(model.into()),
                ..TokenUsage::default()
            },
            cached_included: false,
            output_includes_reasoning: false,
        }
    }

    /// Adds prompt tokens. `includes_parts` says whether the provider's figure
    /// already contains the cache counts; if not, the cache parts are added in
    /// when they are reported (LIFE-27, LIFE-28).
    ///
    /// Call this before the cache counts of the same provider call: the flag
    /// applies to the parts reported after it.
    pub fn input(mut self, tokens: u64, includes_parts: bool) -> Self {
        add(&mut self.usage.input_tokens, tokens);
        self.cached_included = includes_parts;
        self
    }

    /// Adds generated tokens; `includes_reasoning` as for [`input`](Self::input).
    pub fn output(mut self, tokens: u64, includes_reasoning: bool) -> Self {
        add(&mut self.usage.output_tokens, tokens);
        self.output_includes_reasoning = includes_reasoning;
        self
    }

    /// Records cache-read prompt tokens (a part of input).
    pub fn cached_input(mut self, tokens: u64) -> Self {
        add(&mut self.usage.cached_input_tokens, tokens);
        if !self.cached_included {
            add_into_known(&mut self.usage.input_tokens, tokens);
        }
        self
    }

    /// Records cache-write prompt tokens (a part of input, disjoint from cache reads).
    pub fn cache_write_input(mut self, tokens: u64) -> Self {
        add(&mut self.usage.cache_write_input_tokens, tokens);
        if !self.cached_included {
            add_into_known(&mut self.usage.input_tokens, tokens);
        }
        self
    }

    /// Records reasoning tokens (a part of output).
    pub fn reasoning(mut self, tokens: u64) -> Self {
        add(&mut self.usage.reasoning_tokens, tokens);
        if !self.output_includes_reasoning {
            add_into_known(&mut self.usage.output_tokens, tokens);
        }
        self
    }

    /// Adds another call's counts for the same provider and model.
    ///
    /// `other` is taken to follow the protocol's accounting already (parts
    /// inside totals), so its counts are summed field by field; its
    /// `total_tokens` is ignored and recomputed by [`build`](Self::build).
    pub fn absorb(mut self, other: &TokenUsage) -> Self {
        let pairs = [
            (&mut self.usage.input_tokens, other.input_tokens),
            (&mut self.usage.output_tokens, other.output_tokens),
            (&mut self.usage.reasoning_tokens, other.reasoning_tokens),
            (
                &mut self.usage.cached_input_tokens,
                other.cached_input_tokens,
            ),
            (
                &mut self.usage.cache_write_input_tokens,
                other.cache_write_input_tokens,
            ),
        ];
        for (mine, theirs) in pairs {
            if let Some(tokens) = theirs {
                add(mine, tokens);
            }
        }
        self
    }

    /// Finishes: `total_tokens` is computed from the two totals when both are
    /// known, never copied from the provider (LIFE-29..LIFE-31). Every sum
    /// the builder makes saturates at 2^53 − 1, the largest count JSON
    /// carries exactly.
    #[must_use]
    pub fn build(self) -> TokenUsage {
        let mut usage = self.usage;
        usage.total_tokens = match (usage.input_tokens, usage.output_tokens) {
            (Some(input), Some(output)) => Some(sum(input, output)),
            _ => None,
        };
        usage
    }
}

/// Adds two counts, saturating at 2^53 − 1, the largest count JSON carries
/// exactly (and the producer verifier accepts).
fn sum(a: u64, b: u64) -> u64 {
    a.saturating_add(b).min(crate::json::MAX_SAFE_INTEGER)
}

/// Adds a reported count, turning "not reported" into the count.
fn add(slot: &mut Option<u64>, tokens: u64) {
    *slot = Some(slot.map_or(tokens, |known| sum(known, tokens)));
}

/// Adds a part into a total only when the total was reported: a part alone
/// is not a total, and inventing one would report data the provider never
/// gave (LIFE-32).
fn add_into_known(slot: &mut Option<u64>, tokens: u64) {
    if let Some(known) = slot {
        *known = sum(*known, tokens);
    }
}
