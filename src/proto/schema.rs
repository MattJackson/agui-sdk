//! The wire schema as static tables, written by hand from `events.proto`,
//! `types.proto` and `patch.proto`.
//!
//! Nothing is generated at build time. The `schema_sync` unit test parses the
//! pinned upstream `.proto` files (and `proto-freeze.txt`) from the spec cache
//! and fails if any message, field number, type, label, oneof arm or enum
//! value here drifts from them.
//!
//! Each field also records how it maps onto the JSON event shape ([`Json`]),
//! which is where the JSON and protobuf shapes differ: the flattened
//! `base_event`, the `outcome` object folded into sibling fields, and the
//! polymorphic `content` member.

/// A protobuf enum.
#[derive(Debug)]
pub struct EnumDesc {
    #[cfg_attr(not(test), allow(dead_code))] // read by the schema_sync test
    /// Proto name of the enum.
    pub name: &'static str,
    /// Values in declaration order.
    pub values: &'static [EnumValue],
}

/// One enum value and its JSON spelling.
#[derive(Debug)]
pub struct EnumValue {
    #[cfg_attr(not(test), allow(dead_code))] // read by the schema_sync test
    /// Proto identifier.
    pub proto: &'static str,
    /// JSON string the value is spelled as.
    pub json: &'static str,
    /// Wire number.
    pub number: i32,
}

impl EnumDesc {
    /// Looks up a value by its JSON spelling.
    pub fn by_json(&self, s: &str) -> Option<&EnumValue> {
        self.values.iter().find(|v| v.json == s)
    }
    /// Looks up a value by wire number.
    pub fn by_number(&self, n: i32) -> Option<&EnumValue> {
        self.values.iter().find(|v| v.number == n)
    }
}

/// Field value type.
#[derive(Debug, Clone, Copy)]
pub enum Ty {
    /// `string`
    String,
    /// `bool`
    Bool,
    /// `int64` (JSON number, safe-integer range)
    Int64,
    /// An enum, spelled as a JSON string.
    Enum(&'static EnumDesc),
    /// A message (plain or a oneof union).
    Message(&'static MsgDesc),
    /// `google.protobuf.Value`: any JSON value.
    Value,
    /// `google.protobuf.Struct`: a JSON object.
    Struct,
}

impl Ty {
    /// Protobuf wire type this field is carried with.
    pub fn wire_type(self) -> u8 {
        match self {
            Ty::Bool | Ty::Int64 | Ty::Enum(_) => super::wire::VARINT,
            Ty::String | Ty::Message(_) | Ty::Value | Ty::Struct => super::wire::LEN,
        }
    }
}

/// Field cardinality as declared in the `.proto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    /// No label: implicit presence (scalars) or message presence.
    Implicit,
    /// `optional`: explicit presence.
    Optional,
    /// `repeated`
    Repeated,
}

/// What `content` looks like in JSON for each of the wire fields it maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    /// `content` is a string.
    Text,
    /// `content` is an array of parts.
    Parts,
    /// `content` is an object (activity messages).
    Object,
}

/// How a wire field maps onto the JSON object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Json {
    /// A plain member with this key.
    Key(&'static str),
    /// The `content` member, when its JSON kind is the given one.
    Content(ContentKind),
    /// `outcome.type`.
    OutcomeType,
    /// A member of the `outcome` object.
    OutcomeKey(&'static str),
    /// `base_event`: its members are flattened into the event object.
    Base,
    /// `BaseEvent.type`: derived from the event's `type` / oneof arm.
    EventType,
}

/// One field of a message.
#[derive(Debug)]
pub struct Field {
    /// Proto field name.
    pub proto: &'static str,
    /// Wire number.
    pub number: u32,
    /// Value type.
    pub ty: Ty,
    /// Cardinality.
    pub label: Label,
    /// JSON mapping.
    pub json: Json,
}

/// One arm of a oneof union.
#[derive(Debug)]
pub struct Arm {
    #[cfg_attr(not(test), allow(dead_code))] // read by the schema_sync test
    /// Proto field name of the arm.
    pub proto: &'static str,
    /// The JSON discriminator value (`type`) selecting this arm.
    pub json: &'static str,
    /// Wire number.
    pub number: u32,
    /// The arm's message.
    pub msg: &'static MsgDesc,
    /// For event arms: the `EventType` number written into `base_event.type`.
    pub event_type: i32,
}

/// Message body: plain fields, or a single oneof discriminated in JSON by a
/// `type` member.
#[derive(Debug)]
pub enum Body {
    /// Fields, sorted by wire number.
    Fields(&'static [Field]),
    /// A message that is exactly one oneof.
    Union {
        #[cfg_attr(not(test), allow(dead_code))] // read by the schema_sync test
        /// Proto name of the oneof.
        oneof: &'static str,
        /// The arms, sorted by wire number.
        arms: &'static [Arm],
    },
}

/// A message.
#[derive(Debug)]
pub struct MsgDesc {
    /// Proto name (nested messages are dotted: `ToolCall.Function`).
    pub name: &'static str,
    /// Body.
    pub body: Body,
}

impl MsgDesc {
    /// The fields of a plain message (empty for unions).
    pub fn fields(&self) -> &'static [Field] {
        match self.body {
            Body::Fields(f) => f,
            Body::Union { .. } => &[],
        }
    }
}

/// Upper bound on fields per message; the encoder keeps per-field slots in a
/// fixed-size array. Checked by the sync test.
pub const MAX_FIELDS: usize = 16;

// ---------------------------------------------------------------------------
// Construction helpers.

const fn fld(proto: &'static str, number: u32, ty: Ty, label: Label, json: Json) -> Field {
    Field {
        proto,
        number,
        ty,
        label,
        json,
    }
}
/// Implicit-presence string.
const fn s(proto: &'static str, number: u32, key: &'static str) -> Field {
    fld(proto, number, Ty::String, Label::Implicit, Json::Key(key))
}
/// Optional string.
const fn os(proto: &'static str, number: u32, key: &'static str) -> Field {
    fld(proto, number, Ty::String, Label::Optional, Json::Key(key))
}
const fn oi(proto: &'static str, number: u32, key: &'static str) -> Field {
    fld(proto, number, Ty::Int64, Label::Optional, Json::Key(key))
}
const fn ov(proto: &'static str, number: u32, key: &'static str) -> Field {
    fld(proto, number, Ty::Value, Label::Optional, Json::Key(key))
}
const fn ost(proto: &'static str, number: u32, key: &'static str) -> Field {
    fld(proto, number, Ty::Struct, Label::Optional, Json::Key(key))
}
const fn rep(proto: &'static str, number: u32, m: &'static MsgDesc, key: &'static str) -> Field {
    fld(
        proto,
        number,
        Ty::Message(m),
        Label::Repeated,
        Json::Key(key),
    )
}
/// `base_event`, the first field of every event. A macro rather than a
/// `const` because it refers to a `static`.
macro_rules! base {
    () => {
        fld(
            "base_event",
            1,
            Ty::Message(&BASE_EVENT),
            Label::Implicit,
            Json::Base,
        )
    };
}
const fn sub(number: u32) -> Field {
    os("subagent_run_id", number, "subagentRunId")
}

macro_rules! msg {
    ($id:ident, $name:literal, [$($f:expr),* $(,)?]) => {
        #[doc = concat!("`", $name, "`")]
        pub static $id: MsgDesc = MsgDesc { name: $name, body: Body::Fields(&[$($f),*]) };
    };
}

// ---------------------------------------------------------------------------
// Enums.

macro_rules! ev {
    ($p:literal, $n:literal) => {
        EnumValue {
            proto: $p,
            json: $p,
            number: $n,
        }
    };
}

/// `EventType`
pub static EVENT_TYPE: EnumDesc = EnumDesc {
    name: "EventType",
    values: &[
        ev!("TEXT_MESSAGE_START", 0),
        ev!("TEXT_MESSAGE_CONTENT", 1),
        ev!("TEXT_MESSAGE_END", 2),
        ev!("TOOL_CALL_START", 3),
        ev!("TOOL_CALL_ARGS", 4),
        ev!("TOOL_CALL_END", 5),
        ev!("STATE_SNAPSHOT", 6),
        ev!("STATE_DELTA", 7),
        ev!("MESSAGES_SNAPSHOT", 8),
        ev!("RAW", 9),
        ev!("CUSTOM", 10),
        ev!("RUN_STARTED", 11),
        ev!("RUN_FINISHED", 12),
        ev!("RUN_ERROR", 13),
        ev!("STEP_STARTED", 14),
        ev!("STEP_FINISHED", 15),
        ev!("SUBAGENT_STARTED", 16),
        ev!("SUBAGENT_FINISHED", 17),
        ev!("SUBAGENT_ERROR", 18),
        ev!("TEXT_MESSAGE_CHUNK", 19),
        ev!("TOOL_CALL_CHUNK", 20),
        ev!("TOOL_CALL_RESULT", 21),
        ev!("ACTIVITY_SNAPSHOT", 22),
        ev!("ACTIVITY_DELTA", 23),
        ev!("REASONING_START", 24),
        ev!("REASONING_MESSAGE_START", 25),
        ev!("REASONING_MESSAGE_CONTENT", 26),
        ev!("REASONING_MESSAGE_END", 27),
        ev!("REASONING_MESSAGE_CHUNK", 28),
        ev!("REASONING_END", 29),
        ev!("REASONING_ENCRYPTED_VALUE", 30),
    ],
};

/// `JsonPatchOperationType`
pub static JSON_PATCH_OPERATION_TYPE: EnumDesc = EnumDesc {
    name: "JsonPatchOperationType",
    values: &[
        EnumValue {
            proto: "ADD",
            json: "add",
            number: 0,
        },
        EnumValue {
            proto: "REMOVE",
            json: "remove",
            number: 1,
        },
        EnumValue {
            proto: "REPLACE",
            json: "replace",
            number: 2,
        },
        EnumValue {
            proto: "MOVE",
            json: "move",
            number: 3,
        },
        EnumValue {
            proto: "COPY",
            json: "copy",
            number: 4,
        },
        EnumValue {
            proto: "TEST",
            json: "test",
            number: 5,
        },
    ],
};

/// Every enum, for the sync test.
#[cfg_attr(not(test), allow(dead_code))] // read by the schema_sync test
pub static ALL_ENUMS: &[&EnumDesc] = &[&EVENT_TYPE, &JSON_PATCH_OPERATION_TYPE];

// ---------------------------------------------------------------------------
// patch.proto

msg!(
    JSON_PATCH_OPERATION,
    "JsonPatchOperation",
    [
        fld(
            "op",
            1,
            Ty::Enum(&JSON_PATCH_OPERATION_TYPE),
            Label::Implicit,
            Json::Key("op")
        ),
        s("path", 2, "path"),
        os("from", 3, "from"),
        ov("value", 4, "value"),
    ]
);

// ---------------------------------------------------------------------------
// types.proto

msg!(
    TEXT_INPUT_PART,
    "TextInputPart",
    [
        s("text", 1, "text"),
        os("id", 2, "id"),
        ov("metadata", 3, "metadata"),
    ]
);

macro_rules! media_part {
    ($id:ident, $name:literal) => {
        msg!(
            $id,
            $name,
            [
                fld(
                    "source",
                    1,
                    Ty::Message(&INPUT_CONTENT_SOURCE),
                    Label::Implicit,
                    Json::Key("source")
                ),
                ov("metadata", 2, "metadata"),
                os("id", 3, "id"),
            ]
        );
    };
}
media_part!(IMAGE_INPUT_PART, "ImageInputPart");
media_part!(AUDIO_INPUT_PART, "AudioInputPart");
media_part!(VIDEO_INPUT_PART, "VideoInputPart");
media_part!(DOCUMENT_INPUT_PART, "DocumentInputPart");

msg!(
    INPUT_CONTENT_DATA_SOURCE,
    "InputContentDataSource",
    [s("value", 1, "value"), s("mime_type", 2, "mimeType"),]
);
msg!(
    INPUT_CONTENT_URL_SOURCE,
    "InputContentUrlSource",
    [s("value", 1, "value"), os("mime_type", 2, "mimeType"),]
);
msg!(
    INPUT_CONTENT_FILE_SOURCE,
    "InputContentFileSource",
    [
        s("value", 1, "value"),
        os("provider", 2, "provider"),
        os("mime_type", 3, "mimeType"),
    ]
);

const fn arm(proto: &'static str, json: &'static str, number: u32, msg: &'static MsgDesc) -> Arm {
    Arm {
        proto,
        json,
        number,
        msg,
        event_type: 0,
    }
}

/// `InputContentSource`
pub static INPUT_CONTENT_SOURCE: MsgDesc = MsgDesc {
    name: "InputContentSource",
    body: Body::Union {
        oneof: "source",
        arms: &[
            arm("data", "data", 1, &INPUT_CONTENT_DATA_SOURCE),
            arm("url", "url", 2, &INPUT_CONTENT_URL_SOURCE),
            arm("file", "file", 3, &INPUT_CONTENT_FILE_SOURCE),
        ],
    },
};

/// `InputContent`
pub static INPUT_CONTENT: MsgDesc = MsgDesc {
    name: "InputContent",
    body: Body::Union {
        oneof: "part",
        arms: &[
            arm("text", "text", 1, &TEXT_INPUT_PART),
            arm("image", "image", 2, &IMAGE_INPUT_PART),
            arm("audio", "audio", 3, &AUDIO_INPUT_PART),
            arm("video", "video", 4, &VIDEO_INPUT_PART),
            arm("document", "document", 5, &DOCUMENT_INPUT_PART),
        ],
    },
};

msg!(
    TOOL_CALL_FUNCTION,
    "ToolCall.Function",
    [s("name", 1, "name"), s("arguments", 2, "arguments"),]
);
msg!(
    TOOL_CALL,
    "ToolCall",
    [
        s("id", 1, "id"),
        s("type", 2, "type"),
        fld(
            "function",
            3,
            Ty::Message(&TOOL_CALL_FUNCTION),
            Label::Implicit,
            Json::Key("function")
        ),
        ost("metadata", 4, "metadata"),
        os("encrypted_value", 5, "encryptedValue"),
    ]
);
msg!(
    MESSAGE,
    "Message",
    [
        s("id", 1, "id"),
        s("role", 2, "role"),
        fld(
            "content",
            3,
            Ty::String,
            Label::Optional,
            Json::Content(ContentKind::Text)
        ),
        os("name", 4, "name"),
        rep("tool_calls", 5, &TOOL_CALL, "toolCalls"),
        os("tool_call_id", 6, "toolCallId"),
        os("error", 7, "error"),
        fld(
            "content_parts",
            8,
            Ty::Message(&INPUT_CONTENT),
            Label::Repeated,
            Json::Content(ContentKind::Parts)
        ),
        ost("metadata", 9, "metadata"),
        sub(10),
        os("encrypted_value", 11, "encryptedValue"),
        os("activity_type", 12, "activityType"),
        fld(
            "activity_content",
            13,
            Ty::Struct,
            Label::Optional,
            Json::Content(ContentKind::Object)
        ),
    ]
);
msg!(
    TOOL,
    "Tool",
    [
        s("name", 1, "name"),
        s("description", 2, "description"),
        ov("parameters", 3, "parameters"),
        ost("metadata", 4, "metadata"),
    ]
);
msg!(
    CONTEXT,
    "Context",
    [s("description", 1, "description"), s("value", 2, "value"),]
);
msg!(
    RESUME_ENTRY,
    "ResumeEntry",
    [
        s("interrupt_id", 1, "interruptId"),
        s("status", 2, "status"),
        ov("payload", 3, "payload"),
        ost("metadata", 4, "metadata"),
    ]
);
msg!(
    RUN_AGENT_INPUT,
    "RunAgentInput",
    [
        s("thread_id", 1, "threadId"),
        s("run_id", 2, "runId"),
        os("parent_run_id", 3, "parentRunId"),
        ov("state", 4, "state"),
        rep("messages", 5, &MESSAGE, "messages"),
        rep("tools", 6, &TOOL, "tools"),
        rep("context", 7, &CONTEXT, "context"),
        ov("forwarded_props", 8, "forwardedProps"),
        rep("resume", 9, &RESUME_ENTRY, "resume"),
        os("protocol_version", 10, "protocolVersion"),
    ]
);
msg!(
    INTERRUPT,
    "Interrupt",
    [
        s("id", 1, "id"),
        s("reason", 2, "reason"),
        os("message", 3, "message"),
        os("tool_call_id", 4, "toolCallId"),
        ov("response_schema", 5, "responseSchema"),
        os("expires_at", 6, "expiresAt"),
        ov("metadata", 7, "metadata"),
        sub(8),
    ]
);

// ---------------------------------------------------------------------------
// events.proto

msg!(
    BASE_EVENT,
    "BaseEvent",
    [
        fld(
            "type",
            1,
            Ty::Enum(&EVENT_TYPE),
            Label::Implicit,
            Json::EventType
        ),
        oi("timestamp", 2, "timestamp"),
        ov("raw_event", 3, "rawEvent"),
        ost("metadata", 4, "metadata"),
    ]
);

msg!(
    TEXT_MESSAGE_START_EVENT,
    "TextMessageStartEvent",
    [
        base!(),
        s("message_id", 2, "messageId"),
        os("role", 3, "role"),
        os("name", 4, "name"),
        sub(5),
    ]
);
msg!(
    TEXT_MESSAGE_CONTENT_EVENT,
    "TextMessageContentEvent",
    [
        base!(),
        s("message_id", 2, "messageId"),
        s("delta", 3, "delta"),
        sub(4),
    ]
);
msg!(
    TEXT_MESSAGE_END_EVENT,
    "TextMessageEndEvent",
    [base!(), s("message_id", 2, "messageId"), sub(3),]
);
msg!(
    TEXT_MESSAGE_CHUNK_EVENT,
    "TextMessageChunkEvent",
    [
        base!(),
        os("message_id", 2, "messageId"),
        os("role", 3, "role"),
        os("delta", 4, "delta"),
        os("name", 5, "name"),
        sub(6),
    ]
);
msg!(
    TOOL_CALL_START_EVENT,
    "ToolCallStartEvent",
    [
        base!(),
        s("tool_call_id", 2, "toolCallId"),
        s("tool_call_name", 3, "toolCallName"),
        os("parent_message_id", 4, "parentMessageId"),
        sub(5),
    ]
);
msg!(
    TOOL_CALL_ARGS_EVENT,
    "ToolCallArgsEvent",
    [
        base!(),
        s("tool_call_id", 2, "toolCallId"),
        s("delta", 3, "delta"),
        sub(4),
    ]
);
msg!(
    TOOL_CALL_END_EVENT,
    "ToolCallEndEvent",
    [base!(), s("tool_call_id", 2, "toolCallId"), sub(3),]
);
msg!(
    TOOL_CALL_CHUNK_EVENT,
    "ToolCallChunkEvent",
    [
        base!(),
        os("tool_call_id", 2, "toolCallId"),
        os("tool_call_name", 3, "toolCallName"),
        os("parent_message_id", 4, "parentMessageId"),
        os("delta", 5, "delta"),
        sub(6),
    ]
);
msg!(
    TOOL_CALL_RESULT_EVENT,
    "ToolCallResultEvent",
    [
        base!(),
        sub(2),
        s("message_id", 3, "messageId"),
        s("tool_call_id", 4, "toolCallId"),
        fld(
            "content",
            5,
            Ty::String,
            Label::Optional,
            Json::Content(ContentKind::Text)
        ),
        os("role", 6, "role"),
        fld(
            "content_parts",
            7,
            Ty::Message(&INPUT_CONTENT),
            Label::Repeated,
            Json::Content(ContentKind::Parts)
        ),
    ]
);
msg!(
    STATE_SNAPSHOT_EVENT,
    "StateSnapshotEvent",
    [
        base!(),
        fld(
            "snapshot",
            2,
            Ty::Value,
            Label::Implicit,
            Json::Key("snapshot")
        ),
        sub(3),
    ]
);
msg!(
    STATE_DELTA_EVENT,
    "StateDeltaEvent",
    [
        base!(),
        rep("delta", 2, &JSON_PATCH_OPERATION, "delta"),
        sub(3),
    ]
);
msg!(
    MESSAGES_SNAPSHOT_EVENT,
    "MessagesSnapshotEvent",
    [base!(), rep("messages", 2, &MESSAGE, "messages"),]
);
msg!(
    ACTIVITY_SNAPSHOT_EVENT,
    "ActivitySnapshotEvent",
    [
        base!(),
        sub(2),
        s("message_id", 3, "messageId"),
        s("activity_type", 4, "activityType"),
        fld(
            "content",
            5,
            Ty::Struct,
            Label::Implicit,
            Json::Key("content")
        ),
        fld(
            "replace",
            6,
            Ty::Bool,
            Label::Optional,
            Json::Key("replace")
        ),
    ]
);
msg!(
    ACTIVITY_DELTA_EVENT,
    "ActivityDeltaEvent",
    [
        base!(),
        sub(2),
        s("message_id", 3, "messageId"),
        s("activity_type", 4, "activityType"),
        rep("patch", 5, &JSON_PATCH_OPERATION, "patch"),
    ]
);
msg!(
    RAW_EVENT,
    "RawEvent",
    [
        base!(),
        fld("event", 2, Ty::Value, Label::Implicit, Json::Key("event")),
        os("source", 3, "source"),
        sub(4),
    ]
);
msg!(
    CUSTOM_EVENT,
    "CustomEvent",
    [
        base!(),
        s("name", 2, "name"),
        fld("value", 3, Ty::Value, Label::Implicit, Json::Key("value")),
        sub(4),
    ]
);
msg!(
    RUN_STARTED_EVENT,
    "RunStartedEvent",
    [
        base!(),
        s("thread_id", 2, "threadId"),
        s("run_id", 3, "runId"),
        os("parent_run_id", 4, "parentRunId"),
        fld(
            "input",
            5,
            Ty::Message(&RUN_AGENT_INPUT),
            Label::Optional,
            Json::Key("input")
        ),
        os("protocol_version", 6, "protocolVersion"),
    ]
);
msg!(
    RUN_FINISHED_EVENT,
    "RunFinishedEvent",
    [
        base!(),
        s("thread_id", 2, "threadId"),
        s("run_id", 3, "runId"),
        ov("result", 4, "result"),
        fld("outcome", 5, Ty::String, Label::Implicit, Json::OutcomeType),
        fld(
            "interrupts",
            6,
            Ty::Message(&INTERRUPT),
            Label::Repeated,
            Json::OutcomeKey("interrupts")
        ),
        rep("usage", 7, &USAGE, "usage"),
        fld(
            "pending_tool_call_ids",
            8,
            Ty::String,
            Label::Repeated,
            Json::OutcomeKey("pendingToolCallIds")
        ),
    ]
);
msg!(
    RUN_ERROR_EVENT,
    "RunErrorEvent",
    [
        base!(),
        os("code", 2, "code"),
        s("message", 3, "message"),
        rep("usage", 4, &USAGE, "usage"),
    ]
);
msg!(
    STEP_STARTED_EVENT,
    "StepStartedEvent",
    [base!(), s("step_name", 2, "stepName"), sub(3)]
);
msg!(
    STEP_FINISHED_EVENT,
    "StepFinishedEvent",
    [base!(), s("step_name", 2, "stepName"), sub(3)]
);
msg!(
    REASONING_START_EVENT,
    "ReasoningStartEvent",
    [base!(), sub(2), s("message_id", 3, "messageId")]
);
msg!(
    REASONING_MESSAGE_START_EVENT,
    "ReasoningMessageStartEvent",
    [
        base!(),
        sub(2),
        s("message_id", 3, "messageId"),
        s("role", 4, "role"),
    ]
);
msg!(
    REASONING_MESSAGE_CONTENT_EVENT,
    "ReasoningMessageContentEvent",
    [
        base!(),
        sub(2),
        s("message_id", 3, "messageId"),
        s("delta", 4, "delta"),
    ]
);
msg!(
    REASONING_MESSAGE_END_EVENT,
    "ReasoningMessageEndEvent",
    [base!(), sub(2), s("message_id", 3, "messageId"),]
);
msg!(
    REASONING_MESSAGE_CHUNK_EVENT,
    "ReasoningMessageChunkEvent",
    [
        base!(),
        sub(2),
        os("message_id", 3, "messageId"),
        os("delta", 4, "delta"),
    ]
);
msg!(
    REASONING_END_EVENT,
    "ReasoningEndEvent",
    [base!(), sub(2), s("message_id", 3, "messageId")]
);
msg!(
    REASONING_ENCRYPTED_VALUE_EVENT,
    "ReasoningEncryptedValueEvent",
    [
        base!(),
        sub(2),
        s("subtype", 3, "subtype"),
        s("entity_id", 4, "entityId"),
        s("encrypted_value", 5, "encryptedValue"),
    ]
);
msg!(
    SUBAGENT_STARTED_EVENT,
    "SubagentStartedEvent",
    [
        base!(),
        s("subagent_run_id", 2, "subagentRunId"),
        s("name", 3, "name"),
        os("description", 4, "description"),
        os("parent_subagent_run_id", 5, "parentSubagentRunId"),
        os("parent_tool_call_id", 6, "parentToolCallId"),
        os("parent_message_id", 7, "parentMessageId"),
    ]
);
msg!(
    SUBAGENT_FINISHED_EVENT,
    "SubagentFinishedEvent",
    [
        base!(),
        s("subagent_run_id", 2, "subagentRunId"),
        ov("result", 3, "result"),
        fld("outcome", 4, Ty::String, Label::Implicit, Json::OutcomeType),
        fld(
            "interrupt_ids",
            5,
            Ty::String,
            Label::Repeated,
            Json::OutcomeKey("interruptIds")
        ),
    ]
);
msg!(
    SUBAGENT_ERROR_EVENT,
    "SubagentErrorEvent",
    [
        base!(),
        s("subagent_run_id", 2, "subagentRunId"),
        s("message", 3, "message"),
        os("code", 4, "code"),
    ]
);
msg!(
    USAGE,
    "Usage",
    [
        os("provider", 1, "provider"),
        os("model", 2, "model"),
        oi("input_tokens", 3, "inputTokens"),
        oi("output_tokens", 4, "outputTokens"),
        oi("total_tokens", 5, "totalTokens"),
        oi("reasoning_tokens", 6, "reasoningTokens"),
        oi("cached_input_tokens", 7, "cachedInputTokens"),
        oi("cache_write_input_tokens", 8, "cacheWriteInputTokens"),
    ]
);

const fn earm(
    proto: &'static str,
    json: &'static str,
    number: u32,
    msg: &'static MsgDesc,
    event_type: i32,
) -> Arm {
    Arm {
        proto,
        json,
        number,
        msg,
        event_type,
    }
}

/// `Event`: the frame envelope.
pub static EVENT: MsgDesc = MsgDesc {
    name: "Event",
    body: Body::Union {
        oneof: "event",
        arms: &[
            earm(
                "text_message_start",
                "TEXT_MESSAGE_START",
                1,
                &TEXT_MESSAGE_START_EVENT,
                0,
            ),
            earm(
                "text_message_content",
                "TEXT_MESSAGE_CONTENT",
                2,
                &TEXT_MESSAGE_CONTENT_EVENT,
                1,
            ),
            earm(
                "text_message_end",
                "TEXT_MESSAGE_END",
                3,
                &TEXT_MESSAGE_END_EVENT,
                2,
            ),
            earm(
                "tool_call_start",
                "TOOL_CALL_START",
                4,
                &TOOL_CALL_START_EVENT,
                3,
            ),
            earm(
                "tool_call_args",
                "TOOL_CALL_ARGS",
                5,
                &TOOL_CALL_ARGS_EVENT,
                4,
            ),
            earm("tool_call_end", "TOOL_CALL_END", 6, &TOOL_CALL_END_EVENT, 5),
            earm(
                "state_snapshot",
                "STATE_SNAPSHOT",
                7,
                &STATE_SNAPSHOT_EVENT,
                6,
            ),
            earm("state_delta", "STATE_DELTA", 8, &STATE_DELTA_EVENT, 7),
            earm(
                "messages_snapshot",
                "MESSAGES_SNAPSHOT",
                9,
                &MESSAGES_SNAPSHOT_EVENT,
                8,
            ),
            earm("raw", "RAW", 10, &RAW_EVENT, 9),
            earm("custom", "CUSTOM", 11, &CUSTOM_EVENT, 10),
            earm("run_started", "RUN_STARTED", 12, &RUN_STARTED_EVENT, 11),
            earm("run_finished", "RUN_FINISHED", 13, &RUN_FINISHED_EVENT, 12),
            earm("run_error", "RUN_ERROR", 14, &RUN_ERROR_EVENT, 13),
            earm("step_started", "STEP_STARTED", 15, &STEP_STARTED_EVENT, 14),
            earm(
                "step_finished",
                "STEP_FINISHED",
                16,
                &STEP_FINISHED_EVENT,
                15,
            ),
            earm(
                "text_message_chunk",
                "TEXT_MESSAGE_CHUNK",
                17,
                &TEXT_MESSAGE_CHUNK_EVENT,
                19,
            ),
            earm(
                "tool_call_chunk",
                "TOOL_CALL_CHUNK",
                18,
                &TOOL_CALL_CHUNK_EVENT,
                20,
            ),
            earm(
                "subagent_started",
                "SUBAGENT_STARTED",
                19,
                &SUBAGENT_STARTED_EVENT,
                16,
            ),
            earm(
                "subagent_finished",
                "SUBAGENT_FINISHED",
                20,
                &SUBAGENT_FINISHED_EVENT,
                17,
            ),
            earm(
                "subagent_error",
                "SUBAGENT_ERROR",
                21,
                &SUBAGENT_ERROR_EVENT,
                18,
            ),
            earm(
                "tool_call_result",
                "TOOL_CALL_RESULT",
                22,
                &TOOL_CALL_RESULT_EVENT,
                21,
            ),
            earm(
                "activity_snapshot",
                "ACTIVITY_SNAPSHOT",
                23,
                &ACTIVITY_SNAPSHOT_EVENT,
                22,
            ),
            earm(
                "activity_delta",
                "ACTIVITY_DELTA",
                24,
                &ACTIVITY_DELTA_EVENT,
                23,
            ),
            earm(
                "reasoning_start",
                "REASONING_START",
                25,
                &REASONING_START_EVENT,
                24,
            ),
            earm(
                "reasoning_message_start",
                "REASONING_MESSAGE_START",
                26,
                &REASONING_MESSAGE_START_EVENT,
                25,
            ),
            earm(
                "reasoning_message_content",
                "REASONING_MESSAGE_CONTENT",
                27,
                &REASONING_MESSAGE_CONTENT_EVENT,
                26,
            ),
            earm(
                "reasoning_message_end",
                "REASONING_MESSAGE_END",
                28,
                &REASONING_MESSAGE_END_EVENT,
                27,
            ),
            earm(
                "reasoning_message_chunk",
                "REASONING_MESSAGE_CHUNK",
                29,
                &REASONING_MESSAGE_CHUNK_EVENT,
                28,
            ),
            earm(
                "reasoning_end",
                "REASONING_END",
                30,
                &REASONING_END_EVENT,
                29,
            ),
            earm(
                "reasoning_encrypted_value",
                "REASONING_ENCRYPTED_VALUE",
                31,
                &REASONING_ENCRYPTED_VALUE_EVENT,
                30,
            ),
        ],
    },
};

/// Every message in the tables, for the sync test.
#[cfg_attr(not(test), allow(dead_code))] // read by the schema_sync test
pub static ALL_MESSAGES: &[&MsgDesc] = &[
    &JSON_PATCH_OPERATION,
    &TEXT_INPUT_PART,
    &IMAGE_INPUT_PART,
    &AUDIO_INPUT_PART,
    &VIDEO_INPUT_PART,
    &DOCUMENT_INPUT_PART,
    &INPUT_CONTENT_DATA_SOURCE,
    &INPUT_CONTENT_URL_SOURCE,
    &INPUT_CONTENT_FILE_SOURCE,
    &INPUT_CONTENT_SOURCE,
    &INPUT_CONTENT,
    &TOOL_CALL_FUNCTION,
    &TOOL_CALL,
    &MESSAGE,
    &TOOL,
    &CONTEXT,
    &RESUME_ENTRY,
    &RUN_AGENT_INPUT,
    &INTERRUPT,
    &BASE_EVENT,
    &TEXT_MESSAGE_START_EVENT,
    &TEXT_MESSAGE_CONTENT_EVENT,
    &TEXT_MESSAGE_END_EVENT,
    &TEXT_MESSAGE_CHUNK_EVENT,
    &TOOL_CALL_START_EVENT,
    &TOOL_CALL_ARGS_EVENT,
    &TOOL_CALL_END_EVENT,
    &TOOL_CALL_CHUNK_EVENT,
    &TOOL_CALL_RESULT_EVENT,
    &STATE_SNAPSHOT_EVENT,
    &STATE_DELTA_EVENT,
    &MESSAGES_SNAPSHOT_EVENT,
    &ACTIVITY_SNAPSHOT_EVENT,
    &ACTIVITY_DELTA_EVENT,
    &RAW_EVENT,
    &CUSTOM_EVENT,
    &RUN_STARTED_EVENT,
    &RUN_FINISHED_EVENT,
    &RUN_ERROR_EVENT,
    &STEP_STARTED_EVENT,
    &STEP_FINISHED_EVENT,
    &REASONING_START_EVENT,
    &REASONING_MESSAGE_START_EVENT,
    &REASONING_MESSAGE_CONTENT_EVENT,
    &REASONING_MESSAGE_END_EVENT,
    &REASONING_MESSAGE_CHUNK_EVENT,
    &REASONING_END_EVENT,
    &REASONING_ENCRYPTED_VALUE_EVENT,
    &SUBAGENT_STARTED_EVENT,
    &SUBAGENT_FINISHED_EVENT,
    &SUBAGENT_ERROR_EVENT,
    &EVENT,
    &USAGE,
];
