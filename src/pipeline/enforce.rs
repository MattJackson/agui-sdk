//! Enforcement: strip what is unknown, reject what is malformed.
//!
//! Driven by a shape table generated from `schema.json` by `cargo xtask
//! codegen` (closed and open objects, required and optional members, union
//! discriminators, enum sets). The rules (`basic/processing.mdx`):
//!
//! * unknown event type → drop the event, warn naming the type (PROC-01, PROC-11, PROC-12);
//! * unknown property → strip it, warn naming the path (PROC-02, PROC-13);
//! * unknown union member or closed-set value → in an optional position remove
//!   the field, in a required position remove the containing value, in a list
//!   drop the element, always with a warning and never fatal (PROC-03,
//!   PROC-17..PROC-20; decision D3: the spec wins over the upstream fixture
//!   `unknown-enum-value-role-fatal`, which the Rust lane overrides as
//!   `intentional`). "Required position → drop the container" means: an
//!   unknown `Message.role` drops that message from its list (`messages` of
//!   `MESSAGES_SNAPSHOT` or run input); an unknown `ReasoningMessageStart.role`
//!   drops the event; an unknown `PartSource` arm drops the part (RUNIN-18);
//! * a known field with a schema-rejected value → fatal, never repaired or
//!   coerced (PROC-04, PROC-05, PROC-26, PROC-27);
//! * open objects keep every member (PROC-15, BASIC-11, META-03);
//! * a repeated member → the last occurrence wins, as `JSON.parse` has it,
//!   on every path (lenient); [`validate_strict`] rejects it at a described
//!   (schema) position, while an open object is last-wins in both modes.
//!   This also applies to a run input received by a server.
//!
//! The same asymmetry applies to run input: outgoing on the client
//! ([`Enforcer::check_outgoing_input`] on the typed input, then
//! [`Enforcer::enforce_outgoing_value`] only when middleware rewrote it,
//! PROC-32..PROC-34) and incoming on the server
//! ([`Enforcer::enforce_incoming_input`], PROC-35, RUNIN-29, RUNIN-30).
//!
//! [`validate_strict`] is the strict-schema check for authoring and tests
//! (SCHF-01); it is never used on a receive path.
//!
//! A role literal the schema pins to one value
//! (`REASONING_MESSAGE_START.role`, `TOOL_CALL_RESULT.role`) is judged
//! against the role set: another *known* role there is malformed and fatal
//! (`expected \"reasoning\"`); a value outside the set is unrecognised
//! material, so the event (or, for an optional role, the field) is removed.
//!
//! # Cost
//!
//! [`Enforcer::enforce_event_json`] enforces while the typed event is being
//! built from the frame's bytes: no `serde_json::Value` is made, strings
//! without escapes are lent straight from the frame, and a conformant event
//! costs no allocation beyond the event's own. Budgets (container entries,
//! depth) are charged as the bytes are read, so an oversized event is
//! refused before it is built. The one exception is an unrecognised value in
//! a required position, whose container must go: that rare frame is
//! re-walked as a `Value`. [`Enforcer::enforce_event_value`] strips in place
//! and then moves the `Value`'s strings into the event.

use serde::de::{DeserializeOwned, IgnoredAny};
use serde_json::Value;

use crate::diagnostics::{Diagnostic, DiagnosticCode, Reporter, Scope};
use crate::error::{Error, ErrorCode, Result};
use crate::event::{Event, EventType};
use crate::run_input::RunAgentInput;

use super::Limits;

mod report;
mod shape;
mod stream;
mod tree;

#[cfg(test)]
mod tests;

use report::{Fault, FaultKind, Findings, Options, Path, Removal};
use shape::{Shape, Union};
use stream::{RootTag, Rooted, Stream};
use tree::{Tree, Verdict};

/// Hard ceiling on the JSON nesting enforcement walks, whatever
/// [`Limits::max_depth`] says: the byte walk and the value walk recurse once
/// per level, so a raised limit must not let a peer frame exhaust the stack
/// (the patch engine and the protobuf codec have ceilings of their own).
/// An optimized build walks about 1,500 levels on a 2 MiB thread stack, so
/// 512 leaves room for the caller's own frames.
const JSON_DEPTH_CEILING: usize = 512;

/// The enforcement stage.
#[derive(Debug, Clone)]
pub struct Enforcer {
    limits: Limits,
}

impl Enforcer {
    /// An enforcer with the given budgets.
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Enforcer { limits }
    }

    fn check_frame_size(&self, frame: &[u8]) -> Result<()> {
        if frame.len() > self.limits.max_frame_bytes {
            return Err(Error::new(
                ErrorCode::FrameTooLarge,
                format!(
                    "frame of {} bytes exceeds the {}-byte limit",
                    frame.len(),
                    self.limits.max_frame_bytes
                ),
            ));
        }
        Ok(())
    }

    /// Parses a raw frame into a `Value` for the compatibility boundary,
    /// charging the frame, container-entry and depth budgets while reading:
    /// a frame over them fails before its tree is built, never after.
    pub(crate) fn parse_frame(&self, frame: &[u8]) -> Result<Value> {
        self.check_frame_size(frame)?;
        // Any JSON value parses here, so a fault is a syntax fault (or a
        // budget), reported as the direct path reports it; a frame that is
        // not an object is judged by enforcement afterwards.
        parse_value(frame, &self.options()).map_err(|fault| match fault.kind {
            FaultKind::Limit => fault_error(fault, ErrorCode::LimitExceeded),
            _ => Error::new(ErrorCode::MalformedEvent, fault.message),
        })
    }

    fn options(&self) -> Options {
        Options {
            strict: false,
            null_as_absent: true,
            max_entries: self.limits.max_container_entries,
            max_depth: self.limits.max_depth.min(JSON_DEPTH_CEILING),
            max_findings: self.limits.max_diagnostics_per_event,
        }
    }

    /// Enforces one event from JSON bytes in a single parse, applying the
    /// compatibility boundary's null rule as it goes. `Ok(None)` when the
    /// event was dropped.
    ///
    /// Diagnostics go to `reporter`; ending the event's diagnostic budget
    /// ([`Reporter::end_event`]) is the caller's, which knows when an event
    /// has left every stage.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::FrameTooLarge`], [`ErrorCode::MalformedEvent`],
    /// [`ErrorCode::InvalidValue`] or [`ErrorCode::LimitExceeded`].
    pub fn enforce_event_json(
        &self,
        frame: &[u8],
        reporter: &mut Reporter,
    ) -> Result<Option<Event>> {
        self.check_frame_size(frame)?;
        let opts = self.options();
        let mut findings = Findings::for_reporter(opts.max_findings, reporter);
        let outcome = event_from_bytes::<Event>(frame, &opts, &mut findings);
        flush(
            findings,
            reporter,
            Scope::Event,
            outcome.as_ref().ok().and_then(|o| o.1),
        );
        outcome.map(|o| o.0).map_err(event_error)
    }

    /// Enforces one event that middleware produced, consuming the `Value`
    /// (strings and arrays are moved into the typed event, not copied).
    ///
    /// # Errors
    ///
    /// [`ErrorCode::MalformedEvent`], [`ErrorCode::InvalidValue`] or
    /// [`ErrorCode::LimitExceeded`]. There is no frame-size check: the
    /// event is already a `Value`.
    pub fn enforce_event_value(
        &self,
        frame: Value,
        reporter: &mut Reporter,
    ) -> Result<Option<Event>> {
        let opts = self.options();
        let mut findings = Findings::for_reporter(opts.max_findings, reporter);
        let outcome = event_from_value::<Event>(frame, &opts, &mut findings);
        flush(
            findings,
            reporter,
            Scope::Event,
            outcome.as_ref().ok().and_then(|o| o.1),
        );
        outcome.map(|o| o.0).map_err(event_error)
    }

    /// Client side, typed input: a [`RunAgentInput`] carries no unknown
    /// members except inside open JSON (preserved by definition), so this
    /// only validates known fields; no `Value` round trip (PROC-34).
    ///
    /// The input is serialized once into a buffer and that text is walked
    /// with the same rules as received material; anything it would strip is
    /// reported, though a typed input should never produce any.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InvalidRequest`].
    pub fn check_outgoing_input(
        &self,
        input: &RunAgentInput,
        reporter: &mut Reporter,
    ) -> Result<()> {
        let text = serde_json::to_vec(input).map_err(|e| {
            Error::new(
                ErrorCode::InvalidRequest,
                format!("run input could not be serialized: {e}"),
            )
        })?;
        let opts = self.options();
        let mut findings = Findings::for_reporter(opts.max_findings, reporter);
        let outcome =
            document_from_bytes::<IgnoredAny>(&text, run_input_shape(), &opts, &mut findings);
        flush(findings, reporter, Scope::RunInput, None);
        input_result(outcome, ErrorCode::InvalidRequest).map(|_| ())
    }

    /// Client side, after middleware rewrote the input as JSON: strips
    /// unknown material with warnings and rejects malformed known fields,
    /// in place, immediately before sending (PROC-32..PROC-34).
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InvalidRequest`].
    pub fn enforce_outgoing_value(&self, input: &mut Value, reporter: &mut Reporter) -> Result<()> {
        let opts = self.options();
        let mut findings = Findings::for_reporter(opts.max_findings, reporter);
        let verdict = Tree::new(&opts, &mut findings).root(run_input_shape(), input);
        flush(findings, reporter, Scope::RunInput, None);
        match verdict {
            Ok(Verdict::Keep) => Ok(()),
            Ok(Verdict::Drop(_)) => input_result::<()>(Ok(None), ErrorCode::InvalidRequest),
            Err(fault) => input_result::<()>(Err(fault), ErrorCode::InvalidRequest),
        }
    }

    /// Server side, on receipt: same asymmetry; a rejection happens before
    /// `RUN_STARTED` through the transport's error path (PROC-35, RUNIN-29).
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InputRejected`].
    pub fn enforce_incoming_input(
        &self,
        body: &[u8],
        reporter: &mut Reporter,
    ) -> Result<RunAgentInput> {
        let opts = self.options();
        let mut findings = Findings::for_reporter(opts.max_findings, reporter);
        let outcome =
            document_from_bytes::<RunAgentInput>(body, run_input_shape(), &opts, &mut findings);
        flush(findings, reporter, Scope::RunInput, None);
        input_result(outcome, ErrorCode::InputRejected)
    }
}

/// Strict validation of one document against a `$defs` anchor (e.g.
/// `"RunFinishedEvent"`), rejecting unknown material as the strict schema does.
/// Used by the value-corpus lane and by producers' own tests.
///
/// # Errors
///
/// [`ErrorCode::InvalidValue`] with a pointer;
/// [`ErrorCode::MalformedEvent`] (no pointer) for bytes that are not JSON;
/// [`ErrorCode::LimitExceeded`] for a document over the entry or depth
/// budget;
/// [`ErrorCode::InvalidConfig`] for an unknown anchor.
pub fn validate_strict(anchor: &str, document: &[u8]) -> Result<()> {
    let shape = anchor_shape(anchor)?;
    let opts = Options::strict();
    let mut findings = Findings::new(0);
    match document_from_bytes::<IgnoredAny>(document, shape, &opts, &mut findings) {
        Ok(_) => Ok(()),
        Err(fault) => Err(fault_error(fault, ErrorCode::InvalidValue)),
    }
}

/// [`validate_strict`] against `"Event"`, decoding the typed event through
/// the same walk, so whatever the strict schema admits (an integral float in
/// an integer position, say) decodes as it validated.
pub(crate) fn parse_strict_event(document: &[u8]) -> Result<Event> {
    let shape = anchor_shape("Event")?;
    let opts = Options::strict();
    let mut findings = Findings::new(0);
    match document_from_bytes::<Event>(document, shape, &opts, &mut findings) {
        Ok(Some(event)) => Ok(event),
        // Strict mode rejects unknown material instead of dropping it, so a
        // dropped root is not expected; report it rather than guess.
        Ok(None) => Err(Error::new(
            ErrorCode::InvalidValue,
            "event passed strict validation but does not fit the typed model",
        )
        .with_pointer("")),
        Err(fault) => Err(fault_error(fault, ErrorCode::InvalidValue)),
    }
}

/// Consumer-pipeline treatment of one document against a `$defs` anchor:
/// strips unknown material (returning the diagnostics) and rejects malformed
/// values. Used by the value-corpus lane's second gate.
///
/// No compatibility shim applies here: a `null` in an optional position is
/// malformed, as the schema says. When the document's root is itself
/// unrecognised material (an event of an unknown type, or a value whose
/// required member is unrecognised) the whole document is dropped: the
/// result is `Value::Null` and a diagnostic says why.
///
/// # Errors
///
/// [`ErrorCode::InvalidValue`] with a pointer for a malformed known value;
/// [`ErrorCode::LimitExceeded`] for a document over the entry or depth
/// budget; [`ErrorCode::InvalidConfig`] for an unknown anchor.
pub fn enforce_document(anchor: &str, document: Value, reporter: &mut Reporter) -> Result<Value> {
    let shape = anchor_shape(anchor)?;
    let opts = Options::lenient(false);
    let mut findings = Findings::for_reporter(opts.max_findings, reporter);
    let mut document = document;
    let verdict = Tree::new(&opts, &mut findings).root(shape, &mut document);
    let out = match verdict {
        Ok(Verdict::Keep) => Ok(document),
        Ok(Verdict::Drop(removal)) => {
            report_root_drop(&mut findings, removal, root_type(&document));
            Ok(Value::Null)
        }
        Err(fault) => Err(fault),
    };
    let scope = if anchor == "RunAgentInput" {
        Scope::RunInput
    } else if anchor.ends_with("Capabilities") {
        Scope::Capabilities
    } else {
        Scope::Event
    };
    flush(findings, reporter, scope, None);
    out.map_err(|fault| fault_error(fault, ErrorCode::InvalidValue))
}

// ---- the engine's entry points (generic, so tests can target any type) ----

fn event_union() -> Union {
    shape::event_union()
}

fn run_input_shape() -> Shape {
    Shape::of_def(crate::pipeline::shapes::RUN_AGENT_INPUT, false)
}

fn anchor_shape(anchor: &str) -> Result<Shape> {
    shape::anchor(anchor).ok_or_else(|| {
        Error::new(
            ErrorCode::InvalidConfig,
            format!("`{anchor}` is not a schema anchor"),
        )
    })
}

/// An enforced event and its wire type (for diagnostics), or `None` when
/// the event was dropped.
type EventOutcome<T> = (Option<T>, Option<&'static str>);

/// The byte path: one pass, rerouted through the `Value` walk only when a
/// container must be removed.
pub(crate) fn event_from_bytes<T: DeserializeOwned>(
    frame: &[u8],
    opts: &Options,
    findings: &mut Findings,
) -> std::result::Result<EventOutcome<T>, Fault> {
    let walked = event_walk::<T>(frame, opts, findings);
    let rooted = match walked {
        Ok(rooted) => rooted,
        Err(fault) => return retry_as_value(fault, frame, opts, findings, event_from_value::<T>),
    };
    match rooted {
        (Rooted::Value(t), subject) => Ok((Some(t), subject)),
        (Rooted::Dropped, _) => Ok((None, None)),
        (Rooted::Reroute, _) => {
            findings.clear();
            let value = parse_value(frame, opts)?;
            event_from_value(value, opts, findings)
        }
    }
}

/// One forward pass over an event frame.
fn event_walk<T: DeserializeOwned>(
    frame: &[u8],
    opts: &Options,
    findings: &mut Findings,
) -> std::result::Result<(Rooted<T>, Option<&'static str>), Fault> {
    {
        let mut st = Stream::new(frame, opts, findings);
        let (shape, subject) = match st.root_tag(event_union())? {
            RootTag::NotObject => return Err(Fault::not_an_event("not a JSON object")),
            RootTag::Missing => return Err(Fault::not_an_event("no `type` member")),
            RootTag::NotString | RootTag::Empty => {
                return Err(Fault::not_an_event("`type` is not a non-empty string"))
            }
            // Known: walk the variant directly rather than look its tag up again.
            RootTag::Known(name, obj) => (Shape::Object(obj), Some(name)),
            RootTag::Unknown => (Shape::Union(event_union()), None),
            RootTag::Reroute => return Ok((Rooted::Reroute, None)),
        };
        Ok((st.run::<T>(shape)?, subject))
    }
}

/// A byte walk that faulted may have faulted on a member a later repeat
/// shadows (`JSON.parse` keeps the last occurrence, so the value walk never
/// sees the first). In lenient mode the frame is therefore judged once more
/// as a `Value`; its result stands only if it is `Ok`, otherwise the byte
/// path's own fault is the verdict, so no message or pointer changes for a
/// frame without a repeated member. A budget fault is final: the value walk
/// charges the same budgets.
fn retry_as_value<O>(
    fault: Fault,
    frame: &[u8],
    opts: &Options,
    findings: &mut Findings,
    judge: impl FnOnce(Value, &Options, &mut Findings) -> std::result::Result<O, Fault>,
) -> std::result::Result<O, Fault> {
    if opts.strict || fault.kind == FaultKind::Limit {
        return Err(fault);
    }
    let Ok(value) = parse_value(frame, opts) else {
        return Err(fault);
    };
    let mut retry = findings.fresh();
    match judge(value, opts, &mut retry) {
        Ok(out) => {
            *findings = retry;
            Ok(out)
        }
        Err(_) => Err(fault),
    }
}

/// The `Value` path for events.
pub(crate) fn event_from_value<T: DeserializeOwned>(
    mut frame: Value,
    opts: &Options,
    findings: &mut Findings,
) -> std::result::Result<EventOutcome<T>, Fault> {
    let Value::Object(map) = &frame else {
        return Err(Fault::not_an_event("not a JSON object"));
    };
    let subject = match map.get("type") {
        None => return Err(Fault::not_an_event("no `type` member")),
        Some(Value::String(t)) if !t.is_empty() => event_union().variant(t).map(|v| v.0),
        Some(_) => return Err(Fault::not_an_event("`type` is not a non-empty string")),
    };
    match Tree::new(opts, findings).root(Shape::Union(event_union()), &mut frame)? {
        Verdict::Keep => Ok((
            Some(
                crate::json::from_value_tag_first(frame, "type")
                    .map_err(|e| Fault::invalid(&Path::ROOT, format_args!("{e}")))?,
            ),
            subject,
        )),
        Verdict::Drop(removal) => {
            report_root_drop(findings, removal, root_type(&frame));
            Ok((None, subject))
        }
    }
}

/// The byte path for any document.
pub(crate) fn document_from_bytes<T: DeserializeOwned>(
    bytes: &[u8],
    shape: Shape,
    opts: &Options,
    findings: &mut Findings,
) -> std::result::Result<Option<T>, Fault> {
    let walked = Stream::new(bytes, opts, findings).run::<T>(shape);
    let rooted = match walked {
        Ok(rooted) => rooted,
        Err(fault) => {
            return retry_as_value(fault, bytes, opts, findings, |v, o, f| {
                document_from_value(v, shape, o, f)
            })
        }
    };
    match rooted {
        Rooted::Value(t) => Ok(Some(t)),
        Rooted::Dropped => Ok(None),
        Rooted::Reroute => {
            findings.clear();
            let value = parse_value(bytes, opts)?;
            document_from_value(value, shape, opts, findings)
        }
    }
}

/// The `Value` path for any document.
pub(crate) fn document_from_value<T: DeserializeOwned>(
    mut document: Value,
    shape: Shape,
    opts: &Options,
    findings: &mut Findings,
) -> std::result::Result<Option<T>, Fault> {
    match Tree::new(opts, findings).root(shape, &mut document)? {
        Verdict::Keep => Ok(Some(typed(document)?)),
        Verdict::Drop(removal) => {
            report_root_drop(findings, removal, None);
            Ok(None)
        }
    }
}

/// Parses bytes into a `Value` with the same reader and budgets as the byte
/// path, so both paths agree on what is JSON and what is too big.
fn parse_value(bytes: &[u8], opts: &Options) -> std::result::Result<Value, Fault> {
    let mut none = Findings::new(0);
    let rooted = Stream::new(bytes, opts, &mut none).run::<Value>(Shape::Any)?;
    match rooted {
        Rooted::Value(v) => Ok(v),
        Rooted::Dropped | Rooted::Reroute => Err(Fault::invalid(
            &Path::ROOT,
            format_args!("internal: an open value cannot be dropped"),
        )),
    }
}

fn typed<T: DeserializeOwned>(value: Value) -> std::result::Result<T, Fault> {
    T::deserialize(value).map_err(|e| Fault::invalid(&Path::ROOT, format_args!("{e}")))
}

fn root_type(v: &Value) -> Option<&str> {
    v.get("type").and_then(Value::as_str)
}

fn report_root_drop(findings: &mut Findings, removal: Removal, event_type: Option<&str>) {
    if removal.code == DiagnosticCode::UnknownEvent && findings.builds_next() {
        // Cut as `Diagnostic::with_unknown_type` cuts it (a preview of a
        // preview is unchanged), so a long name is not copied whole.
        findings.unknown_type = event_type.map(|t| {
            crate::diagnostics::preview(t, crate::diagnostics::MAX_UNKNOWN_TYPE_BYTES).into_owned()
        });
    }
    findings.push(removal.code, &Path::ROOT, || {
        match (removal.code, event_type) {
            (DiagnosticCode::UnknownEvent, Some(t)) => {
                format!(
                    "dropped an event of unrecognised type `{}`",
                    crate::diagnostics::preview(t, crate::diagnostics::MAX_UNKNOWN_TYPE_BYTES)
                )
            }
            _ => removal.message(),
        }
    });
}

/// Hands findings to the reporter, then the summary for any beyond the cap.
fn flush(
    findings: Findings,
    reporter: &mut Reporter,
    scope: Scope,
    event_type: Option<&'static str>,
) {
    if findings.is_empty() {
        return;
    }
    let suppressed = findings.suppressed;
    let muted = findings.muted;
    let unknown_type = findings.unknown_type;
    for finding in findings.items {
        let unknown_type = match finding.code {
            DiagnosticCode::UnknownEvent => unknown_type.clone(),
            _ => None,
        };
        reporter.report(move || {
            let d = Diagnostic::new(finding.code, scope, finding.pointer, finding.message);
            let d = match event_type.and_then(EventType::parse) {
                Some(t) => d.with_event_type(t),
                None => d,
            };
            match unknown_type {
                Some(t) => d.with_unknown_type(t),
                None => d,
            }
        });
    }
    // Kept but not built: the reporter would not have delivered them.
    for _ in 0..muted {
        reporter.report_unbuilt();
    }
    if suppressed > 0 {
        reporter.report(move || {
            let d = Diagnostic::new(
                DiagnosticCode::Limit,
                scope,
                "",
                format!("{suppressed} more diagnostic(s) for this document suppressed"),
            );
            match event_type.and_then(EventType::parse) {
                Some(t) => d.with_event_type(t),
                None => d,
            }
        });
    }
}

fn fault_error(fault: Fault, invalid: ErrorCode) -> Error {
    let code = match fault.kind {
        FaultKind::Malformed => ErrorCode::MalformedEvent,
        FaultKind::Invalid => invalid,
        FaultKind::Limit => ErrorCode::LimitExceeded,
    };
    let error = Error::new(code, fault.message);
    if fault.kind == FaultKind::Malformed {
        error
    } else {
        error.with_pointer(fault.pointer)
    }
}

fn event_error(fault: Fault) -> Error {
    fault_error(fault, ErrorCode::InvalidValue)
}

/// Run input: every fault is the input's rejection, whatever its class.
fn input_result<T>(outcome: std::result::Result<Option<T>, Fault>, code: ErrorCode) -> Result<T> {
    match outcome {
        Ok(Some(input)) => Ok(input),
        Ok(None) => Err(Error::new(code, "run input is not a usable RunAgentInput")),
        Err(fault) => {
            let error = Error::new(code, fault.message);
            Err(if fault.pointer.is_empty() {
                error
            } else {
                error.with_pointer(fault.pointer)
            })
        }
    }
}
