use serde::Serialize;
use serde_json::Value;

use crate::cancel::CancelToken;
use crate::content::Content;
use crate::error::{Error, ErrorCode};
use crate::ids::{MessageId, RunId, ThreadId};
use crate::interrupt::ResumeEntry;
use crate::message::{Message, UserMessage};
use crate::run_input::{Context, RunAgentInput};
use crate::tool::Tool;

use super::transport::Header;

/// What to send for one run. An infallible builder; [`RunRequest::into_input`]
/// produces the [`RunAgentInput`].
///
/// Defaults: random thread and run ids, state absent (the agent starts from
/// `{}`, RUNIN-24), no `parentRunId` (never set implicitly), `forwardedProps`
/// absent. `protocolVersion` is set by the client's outgoing pipeline, not
/// here (RUNIN-08).
#[derive(Clone, Default)]
pub struct RunRequest {
    input: Option<RunAgentInput>,
    /// Why typed state did not serialize; cleared by a later good state.
    state_error: Option<String>,
    /// As `state_error`, for `forwardedProps`.
    props_error: Option<String>,
    headers: Vec<Header>,
    cancel: Option<CancelToken>,
}

impl std::fmt::Debug for RunRequest {
    /// Ids, counts and headers (whose values are redacted) only: state and
    /// `forwardedProps` may carry secrets, as `MiddlewareRequest`'s
    /// `Debug` assumes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let input = self.input.as_ref();
        f.debug_struct("RunRequest")
            .field("thread_id", &input.map(|i| &i.thread_id))
            .field("run_id", &input.map(|i| &i.run_id))
            .field(
                "parent_run_id",
                &input.and_then(|i| i.parent_run_id.as_ref()),
            )
            .field("messages", &input.map_or(0, |i| i.messages.len()))
            .field(
                "tools",
                &input.and_then(|i| i.tools.as_ref()).map_or(0, Vec::len),
            )
            .field(
                "resume",
                &input.and_then(|i| i.resume.as_ref()).map_or(0, Vec::len),
            )
            .field("headers", &self.headers)
            .field("cancel", &self.cancel.is_some())
            .finish_non_exhaustive()
    }
}

/// The input of a request that set nothing: random ids, no messages.
fn fresh_input() -> RunAgentInput {
    RunAgentInput::new(ThreadId::random(), RunId::random(), Vec::new())
}

impl RunRequest {
    /// An empty request.
    #[must_use]
    pub fn new() -> Self {
        RunRequest::default()
    }

    fn input_mut(&mut self) -> &mut RunAgentInput {
        self.input.get_or_insert_with(fresh_input)
    }

    fn edit(mut self, f: impl FnOnce(&mut RunAgentInput)) -> Self {
        f(self.input_mut());
        self
    }

    /// Sets the thread id.
    #[must_use]
    pub fn thread_id(self, id: impl Into<ThreadId>) -> Self {
        let id = id.into();
        self.edit(|input| input.thread_id = id)
    }

    /// Sets the run id. Never reuse one on a thread (BASIC-14).
    #[must_use]
    pub fn run_id(self, id: impl Into<RunId>) -> Self {
        let id = id.into();
        self.edit(|input| input.run_id = id)
    }

    /// Sets the parent run (RUNIN-07).
    #[must_use]
    pub fn parent_run_id(self, id: impl Into<RunId>) -> Self {
        let id = id.into();
        self.edit(|input| input.parent_run_id = Some(id))
    }

    /// Appends one message to the history.
    #[must_use]
    pub fn message(self, message: impl Into<Message>) -> Self {
        let message = message.into();
        self.edit(|input| input.messages.push(message))
    }

    /// Appends messages.
    #[must_use]
    pub fn messages(self, messages: impl IntoIterator<Item = Message>) -> Self {
        self.edit(|input| input.messages.extend(messages))
    }

    /// Appends a user message with a random id.
    #[must_use]
    pub fn user(self, content: impl Into<Content>) -> Self {
        let message = UserMessage::new(MessageId::random(), content.into());
        self.message(message)
    }

    /// Sets state from any serializable value. A serialization failure is
    /// reported by [`into_input`](Self::into_input), not here.
    #[must_use]
    pub fn state<S: Serialize + ?Sized>(mut self, state: &S) -> Self {
        match serde_json::to_value(state) {
            Ok(value) => self.state_value(value),
            Err(err) => {
                self.state_error = Some(format!("state did not serialize: {err}"));
                self
            }
        }
    }

    /// Sets state from a JSON value.
    #[must_use]
    pub fn state_value(mut self, state: Value) -> Self {
        self.state_error = None;
        // `null` reads as absent: the agent starts from `{}` (RUNIN-24).
        self.edit(|input| input.state = Some(state).filter(|s| !s.is_null()))
    }

    /// Sets `forwardedProps` from any serializable value (RUNIN-26).
    #[must_use]
    pub fn forwarded_props<P: Serialize + ?Sized>(mut self, props: &P) -> Self {
        match serde_json::to_value(props) {
            Ok(value) => self.forwarded_props_value(value),
            Err(err) => {
                self.props_error = Some(format!("forwardedProps did not serialize: {err}"));
                self
            }
        }
    }

    /// Sets `forwardedProps` from a JSON value. `null` means omit (RUNIN-26).
    #[must_use]
    pub fn forwarded_props_value(mut self, props: Value) -> Self {
        self.props_error = None;
        self.edit(|input| input.forwarded_props = Some(props).filter(|p| !p.is_null()))
    }

    /// Advertises a frontend tool.
    #[must_use]
    pub fn tool(self, tool: Tool) -> Self {
        self.edit(|input| input.tools.get_or_insert_with(Vec::new).push(tool))
    }

    /// Advertises frontend tools.
    #[must_use]
    pub fn tools(self, tools: impl IntoIterator<Item = Tool>) -> Self {
        self.edit(|input| input.tools.get_or_insert_with(Vec::new).extend(tools))
    }

    /// Adds a grounding entry.
    #[must_use]
    pub fn context(self, context: Context) -> Self {
        self.edit(|input| input.context.get_or_insert_with(Vec::new).push(context))
    }

    /// Adds a resume entry. Prefer [`Thread::resume`](super::Thread::resume),
    /// which checks coverage and expiry (INTR-10..INTR-15).
    #[must_use]
    pub fn resume(self, entry: ResumeEntry) -> Self {
        self.edit(|input| input.resume.get_or_insert_with(Vec::new).push(entry))
    }

    /// Adds a header to this run's request only (e.g. per-tenant auth on a
    /// shared client). It replaces the client's headers of the same name for
    /// this run; several values added under one name are all sent. Reserved names as for
    /// [`ClientBuilder::header`](super::ClientBuilder::header); a reserved
    /// name fails the run with `request.invalid` before anything is sent.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(Header {
            name: name.into(),
            value: value.into(),
            sensitive: false,
        });
        self
    }

    /// As [`header`](Self::header) (including replacing the client's headers of
    /// the same name), redacted from `Debug` and diagnostics.
    #[must_use]
    pub fn sensitive_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(Header {
            name: name.into(),
            value: value.into(),
            sensitive: true,
        });
        self
    }

    /// Uses `token` to cancel the run, so cancellation can be wired up
    /// before the run starts (for example to a UI stop button).
    #[must_use]
    pub fn cancel_token(mut self, token: CancelToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// The input this request describes.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InvalidRequest`](crate::ErrorCode::InvalidRequest) if typed
    /// state or props failed to serialize.
    pub fn into_input(self) -> crate::Result<RunAgentInput> {
        self.into_parts().0
    }

    /// The thread and run ids the request will use (minting them now if
    /// unset, so the handle and the input agree).
    pub(super) fn ids(&mut self) -> (ThreadId, RunId) {
        let input = self.input_mut();
        (input.thread_id.clone(), input.run_id.clone())
    }

    /// Splits the request into its input, per-run headers and cancel token.
    pub(super) fn into_parts(
        self,
    ) -> (
        crate::Result<RunAgentInput>,
        Vec<Header>,
        Option<CancelToken>,
    ) {
        let input = match self.state_error.or(self.props_error) {
            Some(message) => Err(Error::new(ErrorCode::InvalidRequest, message)),
            None => Ok(self.input.unwrap_or_else(fresh_input)),
        };
        (input, self.headers, self.cancel)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;

    /// Not serializable to JSON: object keys must be strings.
    fn unserializable() -> HashMap<(i32, i32), i32> {
        HashMap::from([((1, 2), 3)])
    }

    #[test]
    fn a_later_good_setter_clears_the_error_of_the_same_field() {
        let state = RunRequest::new()
            .state(&unserializable())
            .state_value(json!({}));
        assert!(state.into_input().is_ok());
        let props = RunRequest::new()
            .forwarded_props(&unserializable())
            .forwarded_props_value(json!({}));
        assert!(props.into_input().is_ok());
    }

    #[test]
    fn state_and_props_errors_do_not_overwrite_each_other() {
        let Err(err) = RunRequest::new()
            .state(&unserializable())
            .forwarded_props(&unserializable())
            .into_input()
        else {
            panic!("an unserializable state must fail the request");
        };
        assert!(err.message().contains("state"), "{err}");
        // A good state does not hide the props error either.
        let Err(err) = RunRequest::new()
            .forwarded_props(&unserializable())
            .state_value(json!({}))
            .into_input()
        else {
            panic!("unserializable props must fail the request");
        };
        assert!(err.message().contains("forwardedProps"), "{err}");
    }
}
