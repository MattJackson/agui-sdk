use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

mod schema;

use crate::content::Content;
use crate::diagnostics::{Diagnostic, DiagnosticCode, Scope};
use crate::error::{Error, ErrorCode, Result};
use crate::ids::{InterruptId, MessageId, RunId, ThreadId, ToolCallId};
use crate::interrupt::{Expiry, Interrupt, ResumeEntry, ResumeStatus};
use crate::lifecycle::RunFinishedOutcome;
use crate::message::{Message, ToolMessage};
use crate::pipeline::reduce::{PendingToolCall, Reducer};
use crate::tool::Tool;

use super::{AgentClient, HttpTransport, RunHandle, RunReport, RunRequest, RunStatus, Transport};

/// Source of "now" for interrupt expiry (INTR-14). Replaceable for tests.
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// Who approved a frontend tool call (IDX-06, TOOL-40): recorded with the
/// approval so an action is never represented as user-approved when it was not.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecidedBy {
    /// The end user, interactively.
    User {
        /// Application user id, if known.
        user_id: Option<String>,
    },
    /// An automatic policy (allow-list, sandbox), named.
    Policy {
        /// Policy name, for audit logs (IDX-11).
        name: String,
    },
}

/// A pending frontend call that was explicitly approved. The intended way to
/// take a call's arguments for execution: approval records who decided
/// (IDX-05, TOOL-39). The pending call's arguments are readable, but
/// executing from them skips that record. Carries the tool as advertised on
/// the thread at approval time, if it was.
#[derive(Debug, Clone)]
pub struct ApprovedToolCall {
    call: PendingToolCall,
    decided_by: DecidedBy,
    tool: Option<Tool>,
}

/// What [`ApprovedToolCall::check_args_subset`] evaluated. The check covers
/// a JSON Schema subset (`type`, `required`, `properties`, `items`, `enum`,
/// `const`); every other keyword in the tool's schema is listed here, *not*
/// evaluated. An empty list means the whole schema was checked.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SchemaCheck {
    /// Keywords present in the schema that were not evaluated, as JSON
    /// pointers into the schema (e.g. `/properties/when/format`).
    pub unevaluated: Vec<String>,
}

impl ApprovedToolCall {
    /// The call.
    #[must_use]
    pub fn call(&self) -> &PendingToolCall {
        &self.call
    }

    /// Who approved it.
    #[must_use]
    pub fn decided_by(&self) -> &DecidedBy {
        &self.decided_by
    }

    /// The tool as advertised on the thread when the call was approved;
    /// `None` for an unadvertised call, which is not a violation but deserves
    /// new-tool scrutiny (TOOL-35, TOOL-42).
    #[must_use]
    pub fn tool(&self) -> Option<&Tool> {
        self.tool.as_ref()
    }

    /// Whether the tool was advertised when the call was approved.
    #[must_use]
    pub fn is_advertised(&self) -> bool {
        self.tool.is_some()
    }

    /// Parses the arguments as JSON. No schema check: the caller validates
    /// what it acts on (TOOL-38, IDX-07), with its own validator or
    /// [`check_args_subset`](Self::check_args_subset). Empty argument text
    /// (a call that streamed no `TOOL_CALL_ARGS`) reads as `{}`.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::ToolArguments`](crate::ErrorCode::ToolArguments) when the
    /// arguments are not JSON.
    pub fn parse_args(&self) -> Result<Value> {
        let text = self.call.arguments.trim();
        if text.is_empty() {
            return Ok(Value::Object(serde_json::Map::new()));
        }
        serde_json::from_str(text).map_err(|err| {
            Error::new(
                ErrorCode::ToolArguments,
                "the tool arguments are not valid JSON",
            )
            .with_source(err)
        })
    }

    /// Checks the arguments against the advertised tool's `parameters`
    /// schema, for the subset of JSON Schema [`SchemaCheck`] describes. Not a
    /// full validator: read [`SchemaCheck::unevaluated`] before trusting a
    /// pass. An unadvertised tool yields a check with nothing evaluated
    /// (`unevaluated == [""]`, the whole schema); an advertised tool with no
    /// `parameters` has nothing to check. The arguments are parsed once.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::ToolArguments`](crate::ErrorCode::ToolArguments) for
    /// unparseable arguments or a violation of an evaluated keyword; the
    /// error's [`pointer`](crate::Error::pointer) locates the argument.
    pub fn check_args_subset(&self) -> Result<SchemaCheck> {
        let args = self.parse_args()?;
        let Some(tool) = &self.tool else {
            return Ok(SchemaCheck {
                unevaluated: vec![String::new()],
            });
        };
        let Some(schema) = &tool.parameters else {
            return Ok(SchemaCheck::default());
        };
        schema::check(schema, &args)?;
        Ok(SchemaCheck {
            unevaluated: schema::unevaluated(schema),
        })
    }
}

/// Everything a [`Thread`] needs to continue after a process restart:
/// history, state, open interrupts, pending frontend calls, staged answers
/// and advertised tools. Persist it with serde; bring it back with
/// [`Thread::restore`] (TOOL-33 and INTR-10 then still hold).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ThreadSnapshot {
    /// The thread id.
    pub thread_id: ThreadId,
    /// The last absorbed run's id.
    pub last_run_id: Option<RunId>,
    /// History.
    pub messages: Vec<Message>,
    /// State.
    pub state: Value,
    /// Interrupts the last run ended with, still unanswered.
    pub open_interrupts: Vec<Interrupt>,
    /// Frontend calls still unanswered.
    pub pending_tool_calls: Vec<PendingToolCall>,
    /// Tool answers staged for the next run.
    pub staged: Vec<ToolMessage>,
    /// Frontend tools advertised on every run.
    pub tools: Vec<Tool>,
    /// Tools the last absorbed run's request advertised besides `tools`,
    /// so a call to one is still advertised after a restore. Absent from
    /// snapshots written before 0.9.1, which read as empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub run_tools: Vec<Tool>,
}

/// A multi-turn conversation with one agent.
///
/// Keeps history and state across runs (STATE-10, LIFE-45), sends the
/// accumulated history with each run (RUNIN-09), and enforces before anything
/// is sent:
///
/// * resume entries exactly cover the open interrupts; omission is not
///   abandonment (INTR-08, INTR-10..INTR-12);
/// * a `resolved` entry for an expired interrupt is refused, a `cancelled`
///   one allowed; an unparseable expiry never expires and warns (INTR-14,
///   INTR-15);
/// * every pending frontend call is answered by a tool message (TOOL-33),
///   which [`complete`](Self::complete), [`fail`](Self::fail) and
///   [`decline`](Self::decline) stage; a call an open interrupt names
///   ([`Interrupt::tool_call_id`]) is answered by that interrupt's resume
///   entry instead and needs no tool message (D13). That answer is not in
///   the history: a thread rebuilt with [`with_history`](Self::with_history)
///   lists such a call as pending again; [`restore`](Self::restore) keeps
///   the exact pending set.
///
/// Runs are owned: [`run`](Self::run) returns a `'static` [`RunHandle`]
/// that can be driven on another task, and the thread takes in the outcome
/// with [`absorb`](Self::absorb), including everything applied before a
/// failure (LIFE-19). [`run_and_absorb`](Self::run_and_absorb) does both.
/// Until a run is absorbed the thread still describes the previous one, and
/// a second `run` is refused.
///
/// ```
/// # use agui_sdk::client::transport::{Transport, TransportRequest};
/// # use agui_sdk::client::{Frame, FrameStream};
/// # use serde_json::json;
/// #
/// # /// Answers every run with one assistant message, as an agent would.
/// # struct Canned;
/// #
/// # impl Transport for Canned {
/// #     async fn open(&self, req: TransportRequest) -> agui_sdk::Result<FrameStream> {
/// #         let (thread, run) = (req.thread_id.as_str(), req.run_id.as_str());
/// #         let id = format!("reply-{run}");
/// #         let events = [
/// #             json!({ "type": "RUN_STARTED", "threadId": thread, "runId": run }),
/// #             json!({ "type": "TEXT_MESSAGE_START", "messageId": id, "role": "assistant" }),
/// #             json!({ "type": "TEXT_MESSAGE_CONTENT", "messageId": id, "delta": "Hi!" }),
/// #             json!({ "type": "TEXT_MESSAGE_END", "messageId": id }),
/// #             json!({ "type": "RUN_FINISHED", "threadId": thread, "runId": run }),
/// #         ];
/// #         Ok(FrameStream::from_frames(events.map(|e| Ok(Frame::from_value(e)))))
/// #     }
/// # }
/// use agui_sdk::client::{AgentClient, Thread};
///
/// # let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
/// # rt.block_on(async {
/// let client = AgentClient::with_transport(Canned).build()?;
/// let mut thread = Thread::new(client, "thread-1");
///
/// // Send, then hand the outcome back so the thread carries it forward.
/// let outcome = thread.send(agui_sdk::Message::user("Hello"))?.await;
/// thread.absorb(&outcome)?;
/// assert_eq!(thread.messages().len(), 2); // the user's message and the reply
///
/// // The next run sends the whole history.
/// let outcome = thread.send(agui_sdk::Message::user("And again"))?.await;
/// thread.absorb(&outcome)?;
/// assert_eq!(thread.messages().len(), 4);
/// # Ok::<(), agui_sdk::Error>(())
/// # }).unwrap();
/// ```
pub struct Thread<T = HttpTransport> {
    client: AgentClient<T>,
    id: ThreadId,
    /// History and state after the last absorbed run.
    reducer: Reducer,
    interrupts: Vec<Interrupt>,
    /// Unanswered frontend calls not yet staged.
    pending: Vec<PendingToolCall>,
    /// Position of each call in `pending`, so answering is O(1).
    pending_index: HashMap<ToolCallId, usize>,
    /// `pending` holds one id twice (only a restored snapshot can), so
    /// removal by position would leave a stale twin.
    pending_has_twins: bool,
    staged: Vec<ToolMessage>,
    tools: Vec<Tool>,
    /// Tools the last run's request advertised besides `tools`.
    run_tools: Vec<Tool>,
    clock: Clock,
    /// The run started and not yet absorbed, with the tools its request
    /// advertised: they replace `run_tools` only if the run is absorbed.
    in_flight: Option<(RunId, Vec<Tool>)>,
    last_run_id: Option<RunId>,
}

impl<T: Transport> Thread<T> {
    /// A thread on `client` with id `id`.
    pub fn new(client: AgentClient<T>, id: impl Into<ThreadId>) -> Self {
        Thread {
            client,
            id: id.into(),
            reducer: Reducer::new(),
            interrupts: Vec::new(),
            pending: Vec::new(),
            pending_index: HashMap::new(),
            pending_has_twins: false,
            staged: Vec::new(),
            tools: Vec::new(),
            run_tools: Vec::new(),
            clock: Arc::new(SystemTime::now),
            in_flight: None,
            last_run_id: None,
        }
    }

    /// Restores history and state from storage. Pending frontend calls are
    /// derived from the history (assistant tool calls with no tool message),
    /// so TOOL-33 holds across restarts; open interrupts cannot be derived and
    /// need [`with_interrupts`](Self::with_interrupts). A call a resume
    /// entry answered (D13) has no tool message, so history alone lists it
    /// as pending again, and it must be [`decline`](Self::decline)d before
    /// the thread continues. Prefer [`restore`](Self::restore), which brings
    /// back everything, including the exact pending set.
    #[must_use]
    pub fn with_history(mut self, messages: Vec<Message>, state: Value) -> Self {
        self.reducer = Reducer::with_history(messages, state);
        self.set_pending(self.reducer.pending_tool_calls().to_vec());
        self
    }

    /// A thread exactly as [`snapshot`](Self::snapshot) left it.
    #[must_use]
    pub fn restore(client: AgentClient<T>, snapshot: ThreadSnapshot) -> Self {
        let ThreadSnapshot {
            thread_id,
            last_run_id,
            messages,
            state,
            open_interrupts,
            pending_tool_calls,
            staged,
            tools,
            run_tools,
        } = snapshot;
        let mut thread = Thread::new(client, thread_id);
        thread.reducer = Reducer::restore(
            messages,
            state,
            pending_tool_calls.clone(),
            open_interrupts.clone(),
        );
        thread.interrupts = open_interrupts;
        thread.set_pending(pending_tool_calls);
        thread.staged = staged;
        thread.tools = tools;
        thread.run_tools = run_tools;
        thread.last_run_id = last_run_id;
        thread
    }

    /// Everything needed to continue this thread later. Taken between runs;
    /// an unabsorbed run is not included.
    #[must_use]
    pub fn snapshot(&self) -> ThreadSnapshot {
        ThreadSnapshot {
            thread_id: self.id.clone(),
            last_run_id: self.last_run_id.clone(),
            messages: self.reducer.messages().to_vec(),
            state: self.reducer.state().clone(),
            open_interrupts: self.interrupts.clone(),
            pending_tool_calls: self.pending.clone(),
            staged: self.staged.clone(),
            tools: self.tools.clone(),
            run_tools: self.run_tools.clone(),
        }
    }

    /// Restores interrupts left open by the previous run.
    #[must_use]
    pub fn with_interrupts(mut self, interrupts: Vec<Interrupt>) -> Self {
        self.interrupts = interrupts;
        self
    }

    /// Frontend tools advertised on every run of this thread.
    #[must_use]
    pub fn with_tools(mut self, tools: impl IntoIterator<Item = Tool>) -> Self {
        self.tools.extend(tools);
        self
    }

    /// Replaces the clock used for expiry.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The thread id.
    #[must_use]
    pub fn id(&self) -> &ThreadId {
        &self.id
    }

    /// History so far.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        self.reducer.messages()
    }

    /// State so far.
    #[must_use]
    pub fn state(&self) -> &Value {
        self.reducer.state()
    }

    /// Interrupts the last run ended with, still unanswered.
    #[must_use]
    pub fn open_interrupts(&self) -> &[Interrupt] {
        &self.interrupts
    }

    /// Frontend calls still unanswered: derived from the stream (TOOL-32),
    /// minus those already answered with [`complete`](Self::complete),
    /// [`fail`](Self::fail) or [`decline`](Self::decline). The stream's order
    /// is kept until a call is answered; after that the order is not
    /// preserved.
    #[must_use]
    pub fn pending_tool_calls(&self) -> &[PendingToolCall] {
        &self.pending
    }

    /// The last run's id.
    #[must_use]
    pub fn last_run_id(&self) -> Option<&RunId> {
        self.last_run_id.as_ref()
    }

    /// Checks resume entries against the open interrupts without sending:
    /// every entry names an open interrupt, once (INTR-08); every open
    /// interrupt has an entry, because omission is not abandonment (INTR-10,
    /// INTR-11); a `resolved` entry may not answer an interrupt that expired
    /// at or before now, while a `cancelled` one may (INTR-14, INTR-15). An
    /// `expiresAt` that cannot be read never expires, with an
    /// `interrupt.unparseable_expiry` warning.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::ResumeCoverage`](crate::ErrorCode::ResumeCoverage) or
    /// [`ErrorCode::InterruptExpired`](crate::ErrorCode::InterruptExpired).
    pub fn validate_resume(&self, entries: &[ResumeEntry]) -> Result<()> {
        // Indexed once: the interrupt count is the peer's (O(E + I), not
        // O(E * I)). The first of two interrupts with one id is the one used.
        let mut open: HashMap<&InterruptId, &Interrupt> =
            HashMap::with_capacity(self.interrupts.len());
        for interrupt in &self.interrupts {
            open.entry(&interrupt.id).or_insert(interrupt);
        }
        let mut seen: HashSet<&InterruptId> = HashSet::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            let id = entry.interrupt_id.as_str();
            if !seen.insert(&entry.interrupt_id) {
                return Err(coverage(format!(
                    "interrupt `{id}` is answered more than once"
                )));
            }
            if !open.contains_key(&entry.interrupt_id) {
                return Err(coverage(format!(
                    "the resume entry for `{id}` names no interrupt the last run left open"
                ))
                .with_pointer(format!("/resume/{index}/interruptId")));
            }
        }
        for interrupt in &self.interrupts {
            if !seen.contains(&interrupt.id) {
                return Err(coverage(format!(
                    "interrupt `{}` has no resume entry; omission is not abandonment, so resolve or cancel it",
                    interrupt.id.as_str()
                )));
            }
        }
        let now = (self.clock)();
        for (index, entry) in entries.iter().enumerate() {
            if entry.status != ResumeStatus::Resolved {
                continue;
            }
            let Some(open) = open.get(&entry.interrupt_id) else {
                continue;
            };
            if open.expiry() == Expiry::Unparseable {
                self.client.config.diagnostics.report(&Diagnostic::new(
                    DiagnosticCode::UnparseableExpiry,
                    Scope::RunInput,
                    format!("/resume/{index}"),
                    "the interrupt's expiresAt could not be read; it is treated as never expiring",
                ));
            } else if open.is_expired_at(now) {
                return Err(Error::new(
                    ErrorCode::InterruptExpired,
                    format!(
                        "interrupt `{}` has expired: it can no longer be resolved, only cancelled",
                        open.id.as_str()
                    ),
                )
                .with_pointer(format!("/resume/{index}/status")));
            }
        }
        Ok(())
    }

    /// Approves a pending call for execution, recording who decided and
    /// capturing the advertised tool, if any.
    ///
    /// This is the consent gate the security model asks applications to run
    /// (IDX-05, IDX-06, TOOL-39, TOOL-40): record [`DecidedBy::User`] only for
    /// an interactive approval and [`DecidedBy::Policy`] otherwise, so an
    /// automatic decision is never represented as the user's. Arguments are
    /// model output: validate them before acting
    /// ([`ApprovedToolCall::check_args_subset`], TOOL-38, IDX-07) and give an
    /// unadvertised tool ([`ApprovedToolCall::is_advertised`]) new-tool
    /// scrutiny (TOOL-42). Log `decided_by` for audit (IDX-11).
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InvalidRequest`](crate::ErrorCode::InvalidRequest) if the call is not pending.
    pub fn approve(
        &self,
        tool_call_id: &ToolCallId,
        decided_by: DecidedBy,
    ) -> Result<ApprovedToolCall> {
        let call = self.pending_call(tool_call_id)?;
        let tool = self
            .tools
            .iter()
            .chain(&self.run_tools)
            .find(|tool| tool.name == call.name)
            .cloned();
        Ok(ApprovedToolCall {
            call: call.clone(),
            decided_by,
            tool,
        })
    }

    /// Stages the result of an executed call.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InvalidRequest`](crate::ErrorCode::InvalidRequest) if the call is not pending.
    pub fn complete(
        &mut self,
        call: ApprovedToolCall,
        content: impl Into<crate::content::Content>,
    ) -> Result<()> {
        let id = call.call.tool_call_id;
        self.stage(
            ToolMessage::new(MessageId::random(), content.into(), id.clone()),
            &id,
        )
    }

    /// Stages a failure for an executed call (a tool message with `error`).
    ///
    /// # Errors
    ///
    /// As [`complete`](Self::complete).
    pub fn fail(&mut self, call: ApprovedToolCall, error: impl Into<String>) -> Result<()> {
        let id = call.call.tool_call_id;
        let error = error.into();
        let message = ToolMessage::new(
            MessageId::random(),
            Content::from(error.as_str()),
            id.clone(),
        )
        .with_error(error);
        self.stage(message, &id)
    }

    /// Stages a refusal for a call the user declined (TOOL-33: declining is
    /// still an answer).
    ///
    /// # Errors
    ///
    /// As [`complete`](Self::complete).
    pub fn decline(&mut self, tool_call_id: &ToolCallId, reason: impl Into<String>) -> Result<()> {
        let message = ToolMessage::new(
            MessageId::random(),
            Content::from(reason.into()),
            tool_call_id.clone(),
        );
        self.stage(message, tool_call_id)
    }

    /// Replaces the pending calls, indexing them by id.
    fn set_pending(&mut self, pending: Vec<PendingToolCall>) {
        self.pending_index = HashMap::with_capacity(pending.len());
        self.pending_has_twins = false;
        for (position, call) in pending.iter().enumerate() {
            if self
                .pending_index
                .insert(call.tool_call_id.clone(), position)
                .is_some()
            {
                self.pending_has_twins = true;
            }
        }
        // The first occurrence is the one found.
        if self.pending_has_twins {
            self.pending_index.clear();
            for (position, call) in pending.iter().enumerate() {
                self.pending_index
                    .entry(call.tool_call_id.clone())
                    .or_insert(position);
            }
        }
        self.pending = pending;
    }

    fn pending_call(&self, tool_call_id: &ToolCallId) -> Result<&PendingToolCall> {
        self.pending_index
            .get(tool_call_id)
            .and_then(|position| self.pending.get(*position))
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::InvalidRequest,
                    format!(
                        "tool call `{}` is not pending on this thread",
                        tool_call_id.as_str()
                    ),
                )
            })
    }

    fn stage(&mut self, message: ToolMessage, tool_call_id: &ToolCallId) -> Result<()> {
        self.pending_call(tool_call_id)?;
        if self.pending_has_twins {
            let kept = std::mem::take(&mut self.pending)
                .into_iter()
                .filter(|call| &call.tool_call_id != tool_call_id)
                .collect();
            self.set_pending(kept);
        } else if let Some(position) = self.pending_index.remove(tool_call_id) {
            // O(1): the last call takes the answered call's place.
            self.pending.swap_remove(position);
            if let Some(moved) = self.pending.get(position) {
                if let Some(slot) = self.pending_index.get_mut(&moved.tool_call_id) {
                    *slot = position;
                }
            }
        }
        self.staged.push(message);
        Ok(())
    }

    /// Starts a run with the thread's history, staged answers and tools
    /// added to `request`. The handle is `'static` and `Send`; hand its
    /// outcome back with [`absorb`](Self::absorb).
    ///
    /// The request's own messages follow the history and staged answers; its
    /// state, when set, replaces the thread's (otherwise the thread's state
    /// is sent, STATE-10, INTR-16); its tools add to the thread's. The
    /// thread id is always this thread's.
    ///
    /// # Errors
    ///
    /// The conversation errors above, before anything is sent;
    /// [`ErrorCode::InvalidRequest`](crate::ErrorCode::InvalidRequest) while
    /// an earlier run is not yet absorbed. A pending call named by an open
    /// interrupt the request's resume entries cover is not counted as
    /// unanswered.
    pub fn run(&mut self, request: RunRequest) -> Result<RunHandle> {
        if let Some((run_id, _)) = &self.in_flight {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                format!(
                    "run `{}` on this thread has not been absorbed yet",
                    run_id.as_str()
                ),
            ));
        }
        let mut request = request;
        let (_, run_id) = request.ids();
        let (input, headers, cancel) = request.into_parts();
        let mut input = input?;

        // Conversation rules, before anything is sent (INTR-11, TOOL-33).
        self.validate_resume(input.resume())?;
        let answered: HashSet<&ToolCallId> = input
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Tool(tool) => Some(&tool.tool_call_id),
                _ => None,
            })
            .collect();
        // A call an open interrupt names (`toolCallId`, the approval it
        // concerns) is answered by that interrupt's resume entry, not by a
        // tool message (interrupt-resume.mdx, Message Flow; D13).
        let resumed: HashSet<&InterruptId> = input
            .resume()
            .iter()
            .map(|entry| &entry.interrupt_id)
            .collect();
        let by_resume: HashSet<&ToolCallId> = self
            .interrupts
            .iter()
            .filter(|interrupt| resumed.contains(&interrupt.id))
            .filter_map(|interrupt| interrupt.tool_call_id.as_ref())
            .collect();
        let unanswered = self
            .pending
            .iter()
            .filter(|call| {
                !answered.contains(&call.tool_call_id) && !by_resume.contains(&call.tool_call_id)
            })
            .count();
        if unanswered > 0 {
            return Err(Error::new(
                ErrorCode::PendingToolCalls,
                format!(
                    "{unanswered} frontend tool call(s) are unanswered; complete, fail or decline each before continuing"
                ),
            ));
        }

        let mut messages = Vec::with_capacity(
            self.reducer.messages().len() + self.staged.len() + input.messages.len(),
        );
        messages.extend_from_slice(self.reducer.messages());
        messages.extend(self.staged.iter().cloned().map(Message::Tool));
        messages.append(&mut input.messages);
        input.messages = messages;
        input.thread_id = self.id.clone();
        if input.state.is_none() {
            input.state = Some(self.reducer.state().clone()).filter(|s| !s.is_null());
        }
        let extra = input.tools.take().unwrap_or_default();
        let mut tools = self.tools.clone();
        tools.extend(extra.iter().cloned());
        input.tools = if tools.is_empty() { None } else { Some(tools) };
        self.in_flight = Some((run_id.clone(), extra));
        Ok(self
            .client
            .start(Ok(input), headers, cancel, self.id.clone(), run_id))
    }

    /// Sends one message (usually a user message).
    ///
    /// # Errors
    ///
    /// As [`run`](Self::run).
    pub fn send(&mut self, message: impl Into<Message>) -> Result<RunHandle> {
        self.run(RunRequest::new().message(message))
    }

    /// Answers the open interrupts and continues.
    ///
    /// # Errors
    ///
    /// As [`run`](Self::run).
    pub fn resume(&mut self, entries: Vec<ResumeEntry>) -> Result<RunHandle> {
        let request = entries
            .into_iter()
            .fold(RunRequest::new(), RunRequest::resume);
        self.run(request)
    }

    /// Takes in the outcome of the run started last: the report's history,
    /// state, interrupts and pending calls, or, for an error, its partial
    /// report (LIFE-19). Clears the staged answers that run sent. An error
    /// with no partial report (nothing was delivered: a refused request, a
    /// connect failure) changes nothing but the in-flight mark, so the
    /// staged answers are sent again by the next run.
    ///
    /// The outcome is matched by the run the request named
    /// ([`RunReport::requested_run_id`]), so a stream on which the producer
    /// started another run after it (after a `RUN_ERROR`, say) is absorbed
    /// like any other.
    ///
    /// Open interrupts stay open until a run that carried their resume
    /// entries closes with `RUN_FINISHED`: a resuming run that fails or is
    /// cut off leaves them for the next run to answer (INTR-11, INTR-13),
    /// and a later run that ends interrupted replaces them with its own.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InvalidRequest`](crate::ErrorCode::InvalidRequest) if the
    /// outcome is for a different run than the one in flight. The in-flight
    /// mark is cleared even then, so the thread can run again; it still
    /// describes the run before.
    pub fn absorb(&mut self, outcome: &Result<RunReport>) -> Result<()> {
        let Some((in_flight, tools)) = self.in_flight.take() else {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "no run on this thread is waiting to be absorbed",
            ));
        };
        let report = match outcome {
            Ok(report) => Some(report),
            Err(err) => err.partial(),
        };
        if let Some(report) = report {
            if report.requested_run_id != in_flight || report.thread_id != self.id {
                return Err(Error::new(
                    ErrorCode::InvalidRequest,
                    format!(
                        "the outcome is for run `{}`, not the run in flight (`{}`)",
                        report.requested_run_id.as_str(),
                        in_flight.as_str()
                    ),
                ));
            }
            if let Some(open) = interrupts_after(report) {
                self.interrupts = open;
            }
            self.reducer = Reducer::restore(
                report.messages.clone(),
                report.state.clone(),
                report.pending_tool_calls.clone(),
                self.interrupts.clone(),
            );
            self.set_pending(report.pending_tool_calls.clone());
            self.run_tools = tools;
            self.staged.clear();
            self.last_run_id = Some(report.run_id.clone());
        }
        Ok(())
    }

    /// [`run`](Self::run), drive it to the end, then [`absorb`](Self::absorb).
    ///
    /// # Cancel safety
    ///
    /// Dropping the returned future before it completes (a timeout, a
    /// `select!` branch that lost) abandons the run: the run is cancelled,
    /// its observers are told (`run.cancelled`), and the thread is left
    /// exactly as it was before the call — history, state, interrupts,
    /// pending calls and staged answers unchanged, nothing in flight — so the
    /// next [`run`](Self::run) is accepted and re-sends the staged answers.
    /// What the abandoned run delivered is not absorbed; the server may
    /// still have acted on the request.
    ///
    /// # Errors
    ///
    /// As `run`, or the run's own error (already absorbed).
    pub async fn run_and_absorb(&mut self, request: RunRequest) -> Result<RunReport> {
        let handle = self.run(request)?;
        let mut guard = AbandonOnDrop(Some(self));
        let outcome = handle.await;
        if let Some(thread) = guard.0.take() {
            thread.absorb(&outcome)?;
        }
        outcome
    }
}

/// Clears the in-flight mark of a [`Thread::run_and_absorb`] that was
/// dropped before it absorbed its run.
struct AbandonOnDrop<'a, T>(Option<&'a mut Thread<T>>);

impl<T> Drop for AbandonOnDrop<'_, T> {
    fn drop(&mut self) {
        if let Some(thread) = self.0.take() {
            thread.in_flight = None;
        }
    }
}

/// The interrupts left open once `report` is absorbed, or `None` to keep
/// those the thread holds: the most recent interrupted run's, unless the run
/// that carried their resume entries closed with `RUN_FINISHED`.
fn interrupts_after(report: &RunReport) -> Option<Vec<Interrupt>> {
    if report.status.is_interrupted() {
        // The stream's last run is now the most recent interrupted run.
        return Some(report.interrupts.clone());
    }
    match &report.requested_status {
        RunStatus::Finished {
            outcome: Some(RunFinishedOutcome::Interrupt { interrupts }),
            ..
        } => Some(interrupts.clone()),
        // The resuming run closed: its entries answered what was open.
        RunStatus::Finished { .. } => Some(Vec::new()),
        // It failed or never closed: nothing was answered (INTR-13).
        RunStatus::Errored(_) => None,
    }
}

fn coverage(message: String) -> Error {
    Error::new(ErrorCode::ResumeCoverage, message)
}

impl<T> std::fmt::Debug for Thread<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Thread")
            .field("id", &self.id)
            .field("messages", &self.reducer.messages().len())
            .field("open_interrupts", &self.interrupts.len())
            .field("pending_tool_calls", &self.pending.len())
            .field("staged", &self.staged.len())
            .field("in_flight", &self.in_flight.as_ref().map(|(id, _)| id))
            .finish_non_exhaustive()
    }
}
