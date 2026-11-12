//! Agent capabilities (`basic/capabilities.mdx`).
//!
//! Capabilities are a declaration, not a contract the stream is judged
//! against: a client never rejects a stream or fails a run because an event was
//! not declared or a declaration went unexercised (CAP-04), and an omitted
//! declaration means "undeclared" rather than "unsupported" (CAP-02). How they
//! are fetched is application-defined; this module validates and reads them.
//!
//! * [`ingest`] / [`ingest_slice`] validate a received declaration: unknown
//!   groups and fields are dropped, not rejected, and each is listed as a
//!   `strip.unknown_field` diagnostic in [`Ingested::diagnostics`] (CAP-13,
//!   CAP-14); members of the open objects `identity.metadata` and `custom`
//!   are kept. A known
//!   field with a schema-rejected value rejects the declaration (CAP-15), and
//!   a rejection never prevents a run (CAP-16).
//! * [`CapabilitiesBuilder`] produces a declaration in the schema's shape (CAP-12).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::diagnostics::{
    CollectingSink, Diagnostic, DiagnosticCode, Reporter, Scope, DEFAULT_MAX_PER_EVENT,
};
use crate::error::{Error, ErrorCode, Result};
use crate::json::JsonObject;
use crate::metadata::Metadata;
use crate::patch::push_token;
use crate::tool::Tool;

/// A tri-state reading of an optional capability flag (CAP-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Support {
    /// Declared `true`.
    Supported,
    /// Declared `false`.
    Unsupported,
    /// Not declared. Never read as unsupported.
    Undeclared,
}

impl Support {
    /// Reads an optional flag.
    #[must_use]
    pub const fn of(flag: Option<bool>) -> Self {
        match flag {
            Some(true) => Support::Supported,
            Some(false) => Support::Unsupported,
            None => Support::Undeclared,
        }
    }
}

/// A validated declaration plus what was dropped from it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Ingested {
    /// The declaration, unknown members removed.
    pub capabilities: AgentCapabilities,
    /// Diagnostics for removed members. Empty for a schema-exact declaration.
    /// Capped at [`DEFAULT_MAX_PER_EVENT`]; a `diagnostics.limit` summary
    /// closes a truncated list.
    pub diagnostics: Vec<crate::diagnostics::Diagnostic>,
}

impl Ingested {
    /// An ingestion result, for tests of code that consumes one. Received
    /// declarations come from [`ingest`].
    #[must_use]
    pub fn new(capabilities: AgentCapabilities, diagnostics: Vec<Diagnostic>) -> Self {
        Ingested {
            capabilities,
            diagnostics,
        }
    }
}

/// Validates a received declaration (CAP-11..CAP-16).
///
/// Unrecognised groups and fields (including the 0.x `subAgents` key,
/// CHG-05) are dropped and listed in [`Ingested::diagnostics`] as
/// `strip.unknown_field` (at most [`DEFAULT_MAX_PER_EVENT`], then one
/// `diagnostics.limit` summary with the suppressed count); nothing is sent to
/// a sink. Members inside the open
/// objects `identity.metadata` and `custom` are kept as they are, `null`
/// values included (CAP-11).
///
/// # Errors
///
/// [`ErrorCode::CapabilitiesRejected`]
/// when a known field carries a value the schema rejects. The caller then
/// proceeds as for an agent that declared nothing (CAP-16);
/// [`ingest_or_undeclared`] does exactly that.
pub fn ingest(value: Value) -> Result<Ingested> {
    let Value::Object(mut root) = value else {
        return Err(rejected(""));
    };
    let collected = Arc::new(CollectingSink::new());
    let mut walk = Walk::new(&collected);
    walk.object(&mut root, ROOT)?;
    walk.reporter.end_capabilities();
    let capabilities = serde_json::from_value(Value::Object(root))
        .map_err(|error| rejected("").with_source(error))?;
    Ok(Ingested {
        capabilities,
        diagnostics: collected.take(),
    })
}

/// [`ingest`] from raw JSON bytes.
///
/// A declaration is read once per agent, not per event, so the bytes are
/// parsed into a [`Value`] and handed to [`ingest`].
///
/// # Errors
///
/// As [`ingest`]; bytes that are not JSON are rejected the same way, with
/// [`ErrorCode::CapabilitiesRejected`] at the root pointer (a peer
/// declaration the consumer cannot validate, CAP-16).
pub fn ingest_slice(bytes: &[u8]) -> Result<Ingested> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        Error::new(
            ErrorCode::CapabilitiesRejected,
            "capability declaration is not JSON",
        )
        .with_pointer("")
        .with_source(error)
    })?;
    ingest(value)
}

/// [`ingest`], falling back to an empty (all-undeclared) declaration on
/// rejection and reporting the rejection to `sink` (CAP-16).
pub fn ingest_or_undeclared(
    value: Value,
    sink: &dyn crate::diagnostics::DiagnosticSink,
) -> AgentCapabilities {
    match ingest(value) {
        Ok(ingested) => ingested.capabilities,
        Err(error) => {
            sink.report(&Diagnostic::new(
                DiagnosticCode::CapabilitiesIgnored,
                Scope::Capabilities,
                error.pointer().unwrap_or(""),
                "capability declaration rejected; proceeding as if nothing was declared",
            ));
            AgentCapabilities::default()
        }
    }
}

/// Builds a declaration in the schema's shape (CAP-03, CAP-12).
#[derive(Debug, Clone, Default)]
#[must_use = "a builder does nothing until `build` is called"]
pub struct CapabilitiesBuilder {
    capabilities: AgentCapabilities,
}

impl CapabilitiesBuilder {
    /// An empty declaration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares the agent's own tools (never merged with input tools, CAP-05).
    pub fn agent_tools(mut self, tools: impl IntoIterator<Item = Tool>) -> Self {
        self.capabilities
            .tools
            .get_or_insert_with(ToolsCapabilities::default)
            .items = Some(tools.into_iter().collect());
        self
    }

    /// Declares subagent definitions (CAP-06).
    pub fn subagents(mut self, subagents: impl IntoIterator<Item = SubagentInfo>) -> Self {
        self.capabilities
            .multi_agent
            .get_or_insert_with(MultiAgentCapabilities::default)
            .subagents = Some(subagents.into_iter().collect());
        self
    }

    /// Edits the declaration directly.
    pub fn with(mut self, edit: impl FnOnce(&mut AgentCapabilities)) -> Self {
        edit(&mut self.capabilities);
        self
    }

    /// Finishes, validating the result against the schema.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::CapabilitiesRejected`]
    /// if an edit produced a value the schema rejects.
    pub fn build(self) -> Result<AgentCapabilities> {
        let value = serde_json::to_value(&self.capabilities)
            .map_err(|error| rejected("").with_source(error))?;
        let Value::Object(mut root) = value else {
            return Err(rejected(""));
        };
        let mut walk = Walk::new(&Arc::new(CollectingSink::new()));
        walk.object(&mut root, ROOT)?;
        Ok(self.capabilities)
    }
}

/// Largest integer the schema admits for a count (`2^53 - 1`).
const MAX_SAFE_INTEGER: u64 = crate::json::MAX_SAFE_INTEGER;

/// What a known capability member must hold.
#[derive(Clone, Copy)]
enum Kind {
    Bool,
    Str,
    /// Integer in `0..=2^53-1`.
    Count,
    /// Array of strings.
    Strings,
    /// Open object (`Metadata`, `custom`): any members, `null` values included.
    Open,
    /// Any JSON value except `null`.
    NonNull,
    /// Array of closed records.
    Records(&'static [Field]),
    /// Closed object.
    Group(&'static [Field]),
}

struct Field {
    name: &'static str,
    kind: Kind,
    required: bool,
}

const fn opt(name: &'static str, kind: Kind) -> Field {
    Field {
        name,
        kind,
        required: false,
    }
}

const fn req(name: &'static str, kind: Kind) -> Field {
    Field {
        name,
        kind,
        required: true,
    }
}

// The shape of `$defs/AgentCapabilities` and the records it references,
// written from `schema.json`.

const TOOL: &[Field] = &[
    req("name", Kind::Str),
    req("description", Kind::Str),
    opt("parameters", Kind::NonNull),
    opt("metadata", Kind::Open),
];

const SUBAGENT_INFO: &[Field] = &[req("name", Kind::Str), opt("description", Kind::Str)];

const IDENTITY: &[Field] = &[
    opt("name", Kind::Str),
    opt("type", Kind::Str),
    opt("description", Kind::Str),
    opt("version", Kind::Str),
    opt("provider", Kind::Str),
    opt("documentationUrl", Kind::Str),
    opt("metadata", Kind::Open),
];

const TRANSPORT: &[Field] = &[
    opt("streaming", Kind::Bool),
    opt("websocket", Kind::Bool),
    opt("httpBinary", Kind::Bool),
    opt("pushNotifications", Kind::Bool),
    opt("resumable", Kind::Bool),
];

const TOOLS: &[Field] = &[
    opt("supported", Kind::Bool),
    opt("items", Kind::Records(TOOL)),
    opt("parallelCalls", Kind::Bool),
    opt("clientProvided", Kind::Bool),
];

const OUTPUT: &[Field] = &[
    opt("structuredOutput", Kind::Bool),
    opt("supportedMimeTypes", Kind::Strings),
];

const STATE: &[Field] = &[
    opt("snapshots", Kind::Bool),
    opt("deltas", Kind::Bool),
    opt("memory", Kind::Bool),
    opt("persistentState", Kind::Bool),
];

const MULTI_AGENT: &[Field] = &[
    opt("supported", Kind::Bool),
    opt("delegation", Kind::Bool),
    opt("handoffs", Kind::Bool),
    opt("subagents", Kind::Records(SUBAGENT_INFO)),
];

const REASONING: &[Field] = &[
    opt("supported", Kind::Bool),
    opt("streaming", Kind::Bool),
    opt("encrypted", Kind::Bool),
];

const MULTIMODAL_INPUT: &[Field] = &[
    opt("image", Kind::Bool),
    opt("audio", Kind::Bool),
    opt("video", Kind::Bool),
    opt("pdf", Kind::Bool),
    opt("file", Kind::Bool),
];

const MULTIMODAL_OUTPUT: &[Field] = &[opt("image", Kind::Bool), opt("audio", Kind::Bool)];

const MULTIMODAL: &[Field] = &[
    opt("input", Kind::Group(MULTIMODAL_INPUT)),
    opt("output", Kind::Group(MULTIMODAL_OUTPUT)),
];

const EXECUTION: &[Field] = &[
    opt("codeExecution", Kind::Bool),
    opt("sandboxed", Kind::Bool),
    opt("maxIterations", Kind::Count),
    opt("maxExecutionTime", Kind::Count),
];

const HUMAN_IN_THE_LOOP: &[Field] = &[
    opt("supported", Kind::Bool),
    opt("approvals", Kind::Bool),
    opt("interventions", Kind::Bool),
    opt("feedback", Kind::Bool),
    opt("interrupts", Kind::Bool),
    opt("approveWithEdits", Kind::Bool),
];

const ROOT: &[Field] = &[
    opt("identity", Kind::Group(IDENTITY)),
    opt("transport", Kind::Group(TRANSPORT)),
    opt("tools", Kind::Group(TOOLS)),
    opt("output", Kind::Group(OUTPUT)),
    opt("state", Kind::Group(STATE)),
    opt("multiAgent", Kind::Group(MULTI_AGENT)),
    opt("reasoning", Kind::Group(REASONING)),
    opt("multimodal", Kind::Group(MULTIMODAL)),
    opt("execution", Kind::Group(EXECUTION)),
    opt("humanInTheLoop", Kind::Group(HUMAN_IN_THE_LOOP)),
    opt("custom", Kind::Open),
];

fn rejected(pointer: &str) -> Error {
    Error::new(
        ErrorCode::CapabilitiesRejected,
        "a known capability field carries a value the schema rejects",
    )
    .with_pointer(pointer)
}

/// One validation pass: drops unknown members of closed objects, rejects
/// malformed known ones.
struct Walk {
    pointer: String,
    reporter: Reporter,
}

impl Walk {
    fn new(sink: &Arc<CollectingSink>) -> Self {
        Walk {
            pointer: String::new(),
            reporter: Reporter::new(sink.clone(), DEFAULT_MAX_PER_EVENT),
        }
    }

    fn object(&mut self, object: &mut Map<String, Value>, fields: &[Field]) -> Result<()> {
        let unknown: Vec<String> = object
            .keys()
            .filter(|key| !fields.iter().any(|field| field.name == key.as_str()))
            .cloned()
            .collect();
        for key in unknown {
            object.remove(&key);
            let at = self.pointer.len();
            push_token(&mut self.pointer, &key);
            let pointer = self.pointer.as_str();
            self.reporter.report(|| {
                Diagnostic::new(
                    DiagnosticCode::UnknownField,
                    Scope::Capabilities,
                    pointer,
                    "unrecognised capability member dropped",
                )
            });
            self.pointer.truncate(at);
        }
        for field in fields {
            let at = self.pointer.len();
            push_token(&mut self.pointer, field.name);
            match object.get_mut(field.name) {
                Some(value) => self.value(value, field.kind)?,
                None if field.required => return Err(rejected(&self.pointer)),
                None => {}
            }
            self.pointer.truncate(at);
        }
        Ok(())
    }

    fn value(&mut self, value: &mut Value, kind: Kind) -> Result<()> {
        let ok = match (kind, &mut *value) {
            (Kind::Bool, Value::Bool(_))
            | (Kind::Str, Value::String(_))
            | (Kind::Open, Value::Object(_)) => true,
            (Kind::NonNull, other) => !other.is_null(),
            (Kind::Count, Value::Number(number)) => match number.as_u64() {
                Some(count) => count <= MAX_SAFE_INTEGER,
                // JSON Schema's `integer` admits `3.0`; store it as `3`.
                None => match number.as_f64() {
                    Some(float)
                        if float.fract() == 0.0
                            && (0.0..=MAX_SAFE_INTEGER as f64).contains(&float) =>
                    {
                        *value = Value::from(float as u64);
                        true
                    }
                    _ => false,
                },
            },
            (Kind::Strings, Value::Array(items)) => items.iter().all(Value::is_string),
            (Kind::Records(fields), Value::Array(items)) => {
                for (index, item) in items.iter_mut().enumerate() {
                    let at = self.pointer.len();
                    push_token(&mut self.pointer, &index.to_string());
                    let Value::Object(record) = item else {
                        return Err(rejected(&self.pointer));
                    };
                    self.object(record, fields)?;
                    self.pointer.truncate(at);
                }
                true
            }
            (Kind::Group(fields), Value::Object(group)) => {
                self.object(group, fields)?;
                true
            }
            _ => false,
        };
        if ok {
            Ok(())
        } else {
            Err(rejected(&self.pointer))
        }
    }
}

sparse_record! {
    /// Everything an agent declares about itself. Every group and field is
    /// optional; an omitted declaration means "undeclared", never "unsupported"
    /// (CAP-01, CAP-02). Read flags through [`Support::of`].
    ///
    /// Schema: `$defs/AgentCapabilities`. Spec rules: CAP-01..CAP-16.
    pub struct AgentCapabilities {
        /// Who the agent is.
        pub identity: Option<IdentityCapabilities>,
        /// Transport features.
        pub transport: Option<TransportCapabilities>,
        /// Tool support and the agent's own tools.
        pub tools: Option<ToolsCapabilities>,
        /// Output features.
        pub output: Option<OutputCapabilities>,
        /// State features.
        pub state: Option<StateCapabilities>,
        /// Delegation and subagents.
        pub multi_agent: Option<MultiAgentCapabilities>,
        /// Reasoning features.
        pub reasoning: Option<ReasoningCapabilities>,
        /// Multimodal input and output.
        pub multimodal: Option<MultimodalCapabilities>,
        /// Execution limits.
        pub execution: Option<ExecutionCapabilities>,
        /// Approval and interrupt features.
        pub human_in_the_loop: Option<HumanInTheLoopCapabilities>,
        /// Application-defined declarations; preserved (CAP-11).
        pub custom: Option<JsonObject>,
    }
}

impl AgentCapabilities {
    /// Starts a [`CapabilitiesBuilder`]; the same as [`CapabilitiesBuilder::new`].
    pub fn builder() -> CapabilitiesBuilder {
        CapabilitiesBuilder::new()
    }

    /// Sets [`identity`](Self::identity).
    #[must_use]
    pub fn with_identity(self, identity: IdentityCapabilities) -> Self {
        Self {
            identity: Some(identity),
            ..self
        }
    }

    /// Sets [`transport`](Self::transport).
    #[must_use]
    pub fn with_transport(self, transport: TransportCapabilities) -> Self {
        Self {
            transport: Some(transport),
            ..self
        }
    }

    /// Sets [`tools`](Self::tools).
    #[must_use]
    pub fn with_tools(self, tools: ToolsCapabilities) -> Self {
        Self {
            tools: Some(tools),
            ..self
        }
    }

    /// Sets [`output`](Self::output).
    #[must_use]
    pub fn with_output(self, output: OutputCapabilities) -> Self {
        Self {
            output: Some(output),
            ..self
        }
    }

    /// Sets [`state`](Self::state).
    #[must_use]
    pub fn with_state(self, state: StateCapabilities) -> Self {
        Self {
            state: Some(state),
            ..self
        }
    }

    /// Sets [`multi_agent`](Self::multi_agent).
    #[must_use]
    pub fn with_multi_agent(self, multi_agent: MultiAgentCapabilities) -> Self {
        Self {
            multi_agent: Some(multi_agent),
            ..self
        }
    }

    /// Sets [`reasoning`](Self::reasoning).
    #[must_use]
    pub fn with_reasoning(self, reasoning: ReasoningCapabilities) -> Self {
        Self {
            reasoning: Some(reasoning),
            ..self
        }
    }

    /// Sets [`multimodal`](Self::multimodal).
    #[must_use]
    pub fn with_multimodal(self, multimodal: MultimodalCapabilities) -> Self {
        Self {
            multimodal: Some(multimodal),
            ..self
        }
    }

    /// Sets [`execution`](Self::execution).
    #[must_use]
    pub fn with_execution(self, execution: ExecutionCapabilities) -> Self {
        Self {
            execution: Some(execution),
            ..self
        }
    }

    /// Sets [`human_in_the_loop`](Self::human_in_the_loop).
    #[must_use]
    pub fn with_human_in_the_loop(self, human_in_the_loop: HumanInTheLoopCapabilities) -> Self {
        Self {
            human_in_the_loop: Some(human_in_the_loop),
            ..self
        }
    }

    /// Sets [`custom`](Self::custom).
    #[must_use]
    pub fn with_custom(self, custom: JsonObject) -> Self {
        Self {
            custom: Some(custom),
            ..self
        }
    }
}

sparse_record! {
    /// Identity of the agent.
    ///
    /// Schema: `$defs/IdentityCapabilities`. Spec rules: CAP-01, CAP-12.
    pub struct IdentityCapabilities {
        /// Agent name.
        pub name: Option<String>,
        /// Agent kind, free-form.
        #[serde(rename = "type")]
        pub kind: Option<String>,
        /// Description.
        pub description: Option<String>,
        /// Agent version (not the protocol version).
        pub version: Option<String>,
        /// Who provides the agent.
        pub provider: Option<String>,
        /// Documentation link.
        pub documentation_url: Option<String>,
        /// Open annotations; unknown members preserved (CAP-11).
        pub metadata: Option<Metadata>,
    }
}

impl IdentityCapabilities {
    /// Sets [`name`](Self::name).
    #[must_use]
    pub fn with_name(self, name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            ..self
        }
    }

    /// Sets [`kind`](Self::kind).
    #[must_use]
    pub fn with_kind(self, kind: impl Into<String>) -> Self {
        Self {
            kind: Some(kind.into()),
            ..self
        }
    }

    /// Sets [`description`](Self::description).
    #[must_use]
    pub fn with_description(self, description: impl Into<String>) -> Self {
        Self {
            description: Some(description.into()),
            ..self
        }
    }

    /// Sets [`version`](Self::version).
    #[must_use]
    pub fn with_version(self, version: impl Into<String>) -> Self {
        Self {
            version: Some(version.into()),
            ..self
        }
    }

    /// Sets [`provider`](Self::provider).
    #[must_use]
    pub fn with_provider(self, provider: impl Into<String>) -> Self {
        Self {
            provider: Some(provider.into()),
            ..self
        }
    }

    /// Sets [`documentation_url`](Self::documentation_url).
    #[must_use]
    pub fn with_documentation_url(self, documentation_url: impl Into<String>) -> Self {
        Self {
            documentation_url: Some(documentation_url.into()),
            ..self
        }
    }

    /// Sets [`metadata`](Self::metadata).
    #[must_use]
    pub fn with_metadata(self, metadata: Metadata) -> Self {
        Self {
            metadata: Some(metadata),
            ..self
        }
    }
}

sparse_record! {
    /// Transport features. The standard HTTP bindings honour neither
    /// `resumable` nor `push_notifications` (CAP-10).
    ///
    /// Schema: `$defs/TransportCapabilities`. Spec rules: CAP-01, CAP-12.
    pub struct TransportCapabilities {
        /// Streams events.
        pub streaming: Option<bool>,
        /// Offers a WebSocket transport.
        pub websocket: Option<bool>,
        /// Offers the protobuf binding.
        pub http_binary: Option<bool>,
        /// Offers push notifications on a transport of its own (CAP-09).
        pub push_notifications: Option<bool>,
        /// Offers resumption on a transport of its own (CAP-09).
        pub resumable: Option<bool>,
    }
}

impl TransportCapabilities {
    /// Sets [`streaming`](Self::streaming).
    #[must_use]
    pub fn with_streaming(self, streaming: bool) -> Self {
        Self {
            streaming: Some(streaming),
            ..self
        }
    }

    /// Sets [`websocket`](Self::websocket).
    #[must_use]
    pub fn with_websocket(self, websocket: bool) -> Self {
        Self {
            websocket: Some(websocket),
            ..self
        }
    }

    /// Sets [`http_binary`](Self::http_binary).
    #[must_use]
    pub fn with_http_binary(self, http_binary: bool) -> Self {
        Self {
            http_binary: Some(http_binary),
            ..self
        }
    }

    /// Sets [`push_notifications`](Self::push_notifications).
    #[must_use]
    pub fn with_push_notifications(self, push_notifications: bool) -> Self {
        Self {
            push_notifications: Some(push_notifications),
            ..self
        }
    }

    /// Sets [`resumable`](Self::resumable).
    #[must_use]
    pub fn with_resumable(self, resumable: bool) -> Self {
        Self {
            resumable: Some(resumable),
            ..self
        }
    }
}

sparse_record! {
    /// Tool support.
    ///
    /// Schema: `$defs/ToolsCapabilities`. Spec rules: CAP-01, CAP-12.
    pub struct ToolsCapabilities {
        /// Calls tools at all.
        pub supported: Option<bool>,
        /// The agent's own tools; never merged with input tools (CAP-05).
        pub items: Option<Vec<Tool>>,
        /// May issue several calls at once.
        pub parallel_calls: Option<bool>,
        /// Accepts frontend tools from the input.
        pub client_provided: Option<bool>,
    }
}

impl ToolsCapabilities {
    /// Sets [`supported`](Self::supported).
    #[must_use]
    pub fn with_supported(self, supported: bool) -> Self {
        Self {
            supported: Some(supported),
            ..self
        }
    }

    /// Sets [`items`](Self::items).
    #[must_use]
    pub fn with_items(self, items: impl IntoIterator<Item = Tool>) -> Self {
        Self {
            items: Some(items.into_iter().collect()),
            ..self
        }
    }

    /// Sets [`parallel_calls`](Self::parallel_calls).
    #[must_use]
    pub fn with_parallel_calls(self, parallel_calls: bool) -> Self {
        Self {
            parallel_calls: Some(parallel_calls),
            ..self
        }
    }

    /// Sets [`client_provided`](Self::client_provided).
    #[must_use]
    pub fn with_client_provided(self, client_provided: bool) -> Self {
        Self {
            client_provided: Some(client_provided),
            ..self
        }
    }
}

sparse_record! {
    /// Output features. No standard event carries structured-output markers (CAP-07).
    ///
    /// Schema: `$defs/OutputCapabilities`. Spec rules: CAP-01, CAP-12.
    pub struct OutputCapabilities {
        /// Produces structured output.
        pub structured_output: Option<bool>,
        /// Output media types.
        pub supported_mime_types: Option<Vec<String>>,
    }
}

impl OutputCapabilities {
    /// Sets [`structured_output`](Self::structured_output).
    #[must_use]
    pub fn with_structured_output(self, structured_output: bool) -> Self {
        Self {
            structured_output: Some(structured_output),
            ..self
        }
    }

    /// Sets [`supported_mime_types`](Self::supported_mime_types).
    #[must_use]
    pub fn with_supported_mime_types(
        self,
        supported_mime_types: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            supported_mime_types: Some(supported_mime_types.into_iter().collect()),
            ..self
        }
    }
}

sparse_record! {
    /// State features.
    ///
    /// Schema: `$defs/StateCapabilities`. Spec rules: CAP-01, CAP-12.
    pub struct StateCapabilities {
        /// Emits state snapshots.
        pub snapshots: Option<bool>,
        /// Emits state deltas.
        pub deltas: Option<bool>,
        /// Keeps memory across threads.
        pub memory: Option<bool>,
        /// Persists state.
        pub persistent_state: Option<bool>,
    }
}

impl StateCapabilities {
    /// Sets [`snapshots`](Self::snapshots).
    #[must_use]
    pub fn with_snapshots(self, snapshots: bool) -> Self {
        Self {
            snapshots: Some(snapshots),
            ..self
        }
    }

    /// Sets [`deltas`](Self::deltas).
    #[must_use]
    pub fn with_deltas(self, deltas: bool) -> Self {
        Self {
            deltas: Some(deltas),
            ..self
        }
    }

    /// Sets [`memory`](Self::memory).
    #[must_use]
    pub fn with_memory(self, memory: bool) -> Self {
        Self {
            memory: Some(memory),
            ..self
        }
    }

    /// Sets [`persistent_state`](Self::persistent_state).
    #[must_use]
    pub fn with_persistent_state(self, persistent_state: bool) -> Self {
        Self {
            persistent_state: Some(persistent_state),
            ..self
        }
    }
}

sparse_record! {
    /// Multi-agent features. The 0.x `subAgents` key is not read (CHG-05).
    ///
    /// Schema: `$defs/MultiAgentCapabilities`. Spec rules: CAP-01, CAP-12.
    pub struct MultiAgentCapabilities {
        /// Runs subagents.
        pub supported: Option<bool>,
        /// Delegates to other agents.
        pub delegation: Option<bool>,
        /// Hands off conversations.
        pub handoffs: Option<bool>,
        /// Subagent definitions, not invocations (CAP-06).
        pub subagents: Option<Vec<SubagentInfo>>,
    }
}

impl MultiAgentCapabilities {
    /// Sets [`supported`](Self::supported).
    #[must_use]
    pub fn with_supported(self, supported: bool) -> Self {
        Self {
            supported: Some(supported),
            ..self
        }
    }

    /// Sets [`delegation`](Self::delegation).
    #[must_use]
    pub fn with_delegation(self, delegation: bool) -> Self {
        Self {
            delegation: Some(delegation),
            ..self
        }
    }

    /// Sets [`handoffs`](Self::handoffs).
    #[must_use]
    pub fn with_handoffs(self, handoffs: bool) -> Self {
        Self {
            handoffs: Some(handoffs),
            ..self
        }
    }

    /// Sets [`subagents`](Self::subagents).
    #[must_use]
    pub fn with_subagents(self, subagents: impl IntoIterator<Item = SubagentInfo>) -> Self {
        Self {
            subagents: Some(subagents.into_iter().collect()),
            ..self
        }
    }
}

/// A subagent definition.
///
/// Schema: `$defs/SubagentInfo`. Spec rules: CAP-01, CAP-12.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SubagentInfo {
    /// Subagent name, as used on `SUBAGENT_STARTED`.
    pub name: String,
    /// What it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl SubagentInfo {
    /// Creates the value from its required fields (`name`); every optional
    /// field starts absent.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
        }
    }

    /// Sets [`description`](Self::description).
    #[must_use]
    pub fn with_description(self, description: impl Into<String>) -> Self {
        Self {
            description: Some(description.into()),
            ..self
        }
    }
}

sparse_record! {
    /// Reasoning features.
    ///
    /// Schema: `$defs/ReasoningCapabilities`. Spec rules: CAP-01, CAP-12.
    pub struct ReasoningCapabilities {
        /// Emits reasoning.
        pub supported: Option<bool>,
        /// Streams reasoning text.
        pub streaming: Option<bool>,
        /// Emits encrypted reasoning values.
        pub encrypted: Option<bool>,
    }
}

impl ReasoningCapabilities {
    /// Sets [`supported`](Self::supported).
    #[must_use]
    pub fn with_supported(self, supported: bool) -> Self {
        Self {
            supported: Some(supported),
            ..self
        }
    }

    /// Sets [`streaming`](Self::streaming).
    #[must_use]
    pub fn with_streaming(self, streaming: bool) -> Self {
        Self {
            streaming: Some(streaming),
            ..self
        }
    }

    /// Sets [`encrypted`](Self::encrypted).
    #[must_use]
    pub fn with_encrypted(self, encrypted: bool) -> Self {
        Self {
            encrypted: Some(encrypted),
            ..self
        }
    }
}

sparse_record! {
    /// Accepted input media.
    ///
    /// Schema: `$defs/MultimodalInputCapabilities`. Spec rules: CAP-01, CAP-12.
    pub struct MultimodalInputCapabilities {
        /// Images.
        pub image: Option<bool>,
        /// Audio.
        pub audio: Option<bool>,
        /// Video.
        pub video: Option<bool>,
        /// PDF documents.
        pub pdf: Option<bool>,
        /// Provider file handles.
        pub file: Option<bool>,
    }
}

impl MultimodalInputCapabilities {
    /// Sets [`image`](Self::image).
    #[must_use]
    pub fn with_image(self, image: bool) -> Self {
        Self {
            image: Some(image),
            ..self
        }
    }

    /// Sets [`audio`](Self::audio).
    #[must_use]
    pub fn with_audio(self, audio: bool) -> Self {
        Self {
            audio: Some(audio),
            ..self
        }
    }

    /// Sets [`video`](Self::video).
    #[must_use]
    pub fn with_video(self, video: bool) -> Self {
        Self {
            video: Some(video),
            ..self
        }
    }

    /// Sets [`pdf`](Self::pdf).
    #[must_use]
    pub fn with_pdf(self, pdf: bool) -> Self {
        Self {
            pdf: Some(pdf),
            ..self
        }
    }

    /// Sets [`file`](Self::file).
    #[must_use]
    pub fn with_file(self, file: bool) -> Self {
        Self {
            file: Some(file),
            ..self
        }
    }
}

sparse_record! {
    /// Produced output media. No standard event carries it (CAP-08).
    ///
    /// Schema: `$defs/MultimodalOutputCapabilities`. Spec rules: CAP-01, CAP-12.
    pub struct MultimodalOutputCapabilities {
        /// Images.
        pub image: Option<bool>,
        /// Audio.
        pub audio: Option<bool>,
    }
}

impl MultimodalOutputCapabilities {
    /// Sets [`image`](Self::image).
    #[must_use]
    pub fn with_image(self, image: bool) -> Self {
        Self {
            image: Some(image),
            ..self
        }
    }

    /// Sets [`audio`](Self::audio).
    #[must_use]
    pub fn with_audio(self, audio: bool) -> Self {
        Self {
            audio: Some(audio),
            ..self
        }
    }
}

sparse_record! {
    /// Multimodal features.
    ///
    /// Schema: `$defs/MultimodalCapabilities`. Spec rules: CAP-01, CAP-12.
    pub struct MultimodalCapabilities {
        /// Accepted input media.
        pub input: Option<MultimodalInputCapabilities>,
        /// Produced output media.
        pub output: Option<MultimodalOutputCapabilities>,
    }
}

impl MultimodalCapabilities {
    /// Sets [`input`](Self::input).
    #[must_use]
    pub fn with_input(self, input: MultimodalInputCapabilities) -> Self {
        Self {
            input: Some(input),
            ..self
        }
    }

    /// Sets [`output`](Self::output).
    #[must_use]
    pub fn with_output(self, output: MultimodalOutputCapabilities) -> Self {
        Self {
            output: Some(output),
            ..self
        }
    }
}

sparse_record! {
    /// Execution features and limits.
    ///
    /// Schema: `$defs/ExecutionCapabilities`. Spec rules: CAP-01, CAP-12.
    pub struct ExecutionCapabilities {
        /// Executes code.
        pub code_execution: Option<bool>,
        /// Executes in a sandbox.
        pub sandboxed: Option<bool>,
        /// Iteration cap.
        pub max_iterations: Option<u64>,
        /// Execution time cap.
        pub max_execution_time: Option<u64>,
    }
}

impl ExecutionCapabilities {
    /// Sets [`code_execution`](Self::code_execution).
    #[must_use]
    pub fn with_code_execution(self, code_execution: bool) -> Self {
        Self {
            code_execution: Some(code_execution),
            ..self
        }
    }

    /// Sets [`sandboxed`](Self::sandboxed).
    #[must_use]
    pub fn with_sandboxed(self, sandboxed: bool) -> Self {
        Self {
            sandboxed: Some(sandboxed),
            ..self
        }
    }

    /// Sets [`max_iterations`](Self::max_iterations).
    #[must_use]
    pub fn with_max_iterations(self, max_iterations: u64) -> Self {
        Self {
            max_iterations: Some(max_iterations),
            ..self
        }
    }

    /// Sets [`max_execution_time`](Self::max_execution_time).
    #[must_use]
    pub fn with_max_execution_time(self, max_execution_time: u64) -> Self {
        Self {
            max_execution_time: Some(max_execution_time),
            ..self
        }
    }
}

sparse_record! {
    /// Human-in-the-loop features.
    ///
    /// Schema: `$defs/HumanInTheLoopCapabilities`. Spec rules: CAP-01, CAP-12.
    pub struct HumanInTheLoopCapabilities {
        /// Supports human input at all.
        pub supported: Option<bool>,
        /// Asks for approvals.
        pub approvals: Option<bool>,
        /// Accepts interventions.
        pub interventions: Option<bool>,
        /// Accepts feedback.
        pub feedback: Option<bool>,
        /// Ends runs with interrupts.
        pub interrupts: Option<bool>,
        /// Accepts edited approvals.
        pub approve_with_edits: Option<bool>,
    }
}

impl HumanInTheLoopCapabilities {
    /// Sets [`supported`](Self::supported).
    #[must_use]
    pub fn with_supported(self, supported: bool) -> Self {
        Self {
            supported: Some(supported),
            ..self
        }
    }

    /// Sets [`approvals`](Self::approvals).
    #[must_use]
    pub fn with_approvals(self, approvals: bool) -> Self {
        Self {
            approvals: Some(approvals),
            ..self
        }
    }

    /// Sets [`interventions`](Self::interventions).
    #[must_use]
    pub fn with_interventions(self, interventions: bool) -> Self {
        Self {
            interventions: Some(interventions),
            ..self
        }
    }

    /// Sets [`feedback`](Self::feedback).
    #[must_use]
    pub fn with_feedback(self, feedback: bool) -> Self {
        Self {
            feedback: Some(feedback),
            ..self
        }
    }

    /// Sets [`interrupts`](Self::interrupts).
    #[must_use]
    pub fn with_interrupts(self, interrupts: bool) -> Self {
        Self {
            interrupts: Some(interrupts),
            ..self
        }
    }

    /// Sets [`approve_with_edits`](Self::approve_with_edits).
    #[must_use]
    pub fn with_approve_with_edits(self, approve_with_edits: bool) -> Self {
        Self {
            approve_with_edits: Some(approve_with_edits),
            ..self
        }
    }
}
