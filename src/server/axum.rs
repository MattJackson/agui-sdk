//! axum 0.8 integration (feature `axum`), built on `axum-core` 0.5.
//!
//! No router: two types you use in your own handler, mounted wherever your
//! `Router` puts it.
//!
//! * [`AguiRequest`] implements `FromRequest`: it takes `Content-Type`,
//!   `Accept` and the body. It reads at most the smaller of axum's
//!   `DefaultBodyLimit` (2 MB unless your router sets
//!   `DefaultBodyLimit::max` or `disable`) and 16 MiB, and answers a
//!   `Content-Length` over 16 MiB with `413` before reading any of the body.
//!   `disable()` lifts only axum's cap, never the 16 MiB one, so one request
//!   holds at most about twice the cap while it is read (the frames and
//!   their copy). [`respond`] then applies
//!   [`InputPolicy::max_body_bytes`](super::input::InputPolicy::max_body_bytes),
//!   which binds when it is lower. Every cap answers `413`. To accept run
//!   inputs over 16 MiB, read the body with your own limit and build the
//!   request with [`AguiRequest::new`].
//! * [`respond`] runs [`super::http::handle`] and returns an [`AguiResponse`],
//!   which implements `IntoResponse`: a rejection becomes its status and JSON
//!   body; a stream becomes a `200` whose body interleaves SSE keep-alives on a
//!   tokio timer and cancels the run's [`CancelToken`]
//!   when the client disconnects (the body is dropped). Every response carries
//!   `Vary: Accept`.
//!
//! ```
//! use std::sync::Arc;
//!
//! use agui_sdk::server::axum::{respond, AguiRequest, AguiResponse};
//! use agui_sdk::server::http::ServeConfig;
//! use agui_sdk::server::{Agent, BoxError, RunContext, RunOutcome, Runner};
//! use axum::extract::State;
//!
//! struct Echo;
//!
//! impl Agent for Echo {
//!     type State = serde_json::Value;
//!
//!     async fn run(&self, ctx: RunContext) -> Result<RunOutcome, BoxError> {
//!         ctx.emitter().say("Hello").await?;
//!         Ok(RunOutcome::success())
//!     }
//! }
//!
//! #[derive(Clone)]
//! struct App {
//!     runner: Runner<Echo>,
//!     serve: Arc<ServeConfig>,
//! }
//!
//! async fn agent(State(app): State<App>, req: AguiRequest) -> AguiResponse {
//!     respond(&app.runner, &app.serve, req)
//! }
//!
//! let app = App {
//!     runner: Runner::new(Echo),
//!     serve: Arc::new(ServeConfig::default()),
//! };
//! let router: axum::Router = axum::Router::new()
//!     .route("/agent", axum::routing::post(agent))
//!     .with_state(app);
//! ```
//!
//! The `FromRequest`/`IntoResponse` impls are this module's only third-party
//! surface; they follow axum-core 0.5 (axum 0.8). A future axum-core major
//! gets a new feature, not a change to this one.

use std::future::Future as _;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum_core::body::Body;
use axum_core::extract::Request;
use axum_core::response::Response;
use bytes::Bytes;
use http::header::{HeaderValue, ACCEPT, CACHE_CONTROL, CONTENT_TYPE, VARY};
use http::StatusCode;
use http_body::Frame;
use http_body_util::BodyExt;

use super::http::{HttpRejection, HttpRequest, HttpResponse, ResponseBody, ServeConfig};
use super::{Agent, CancelToken, Extensions, Runner};
use crate::error::ErrorCode;
use crate::wire::{Chunk, JSON_MEDIA_TYPE};

/// The parts of an agent request, extracted by axum.
#[derive(Debug, Clone)]
pub struct AguiRequest {
    content_type: Option<String>,
    accept: Option<String>,
    body: Chunk,
    extensions: Extensions,
}

impl AguiRequest {
    /// A request built by hand (for tests or other glue).
    pub fn new(content_type: Option<String>, accept: Option<String>, body: Chunk) -> Self {
        AguiRequest {
            content_type,
            accept,
            body,
            extensions: Extensions::default(),
        }
    }

    /// Attaches request-scoped data for the agent: copy the principal or
    /// tenant out of your own extractors here.
    #[must_use]
    pub fn with_extensions(self, extensions: Extensions) -> Self {
        AguiRequest { extensions, ..self }
    }
}

fn header(request: &Request, name: http::header::HeaderName) -> Option<String> {
    request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// A list-valued field: repeated field lines are one comma-joined line
/// (RFC 9110 5.3). A line that is not text is skipped; none left is missing.
fn header_list(request: &Request, name: http::header::HeaderName) -> Option<String> {
    let mut joined: Option<String> = None;
    for value in request.headers().get_all(name) {
        let Ok(value) = value.to_str() else { continue };
        match &mut joined {
            Some(all) => {
                all.push_str(", ");
                all.push_str(value);
            }
            None => joined = Some(value.to_owned()),
        }
    }
    joined
}

fn too_large() -> AguiResponse {
    AguiResponse::rejection(
        413,
        ErrorCode::RequestTooLarge,
        "the request body is larger than the server's limit",
    )
}

/// Whether a body error is a length limit (axum's or the extractor's).
fn is_length_limit(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        current = error.source();
    }
    false
}

impl<S: Send + Sync> axum_core::extract::FromRequest<S> for AguiRequest {
    type Rejection = AguiResponse;

    async fn from_request(
        request: axum_core::extract::Request,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let _ = state;
        let content_type = header(&request, CONTENT_TYPE);
        let accept = header_list(&request, ACCEPT);
        // Never read more than the crate's own cap, whatever axum's
        // `DefaultBodyLimit` allows (`disable()` included), and refuse a
        // declared length over it before reading anything (D20).
        let cap = super::input::DEFAULT_MAX_BODY_BYTES;
        let declared = request
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        if declared.is_some_and(|n| n > cap as u64) {
            return Err(too_large());
        }
        let body =
            http_body_util::Limited::new(axum_core::RequestExt::into_limited_body(request), cap);
        let collected = body.collect().await.map_err(|error| {
            if is_length_limit(error.as_ref()) {
                too_large()
            } else {
                AguiResponse::rejection(
                    400,
                    ErrorCode::InputRejected,
                    "the request body could not be read",
                )
            }
        })?;
        Ok(AguiRequest {
            content_type,
            accept,
            body: Chunk::from_bytes(collected.to_bytes()),
            extensions: Extensions::default(),
        })
    }
}

/// An agent response for axum: a pre-stream rejection or a streaming body.
#[derive(Debug)]
pub struct AguiResponse {
    inner: Inner,
}

#[derive(Debug)]
enum Inner {
    Stream {
        response: HttpResponse,
        keepalive: Option<Duration>,
    },
    Rejection(HttpRejection),
    /// A JSON document with a status (capabilities, 404).
    Json {
        status: u16,
        body: String,
    },
}

impl AguiResponse {
    fn rejection(status: u16, code: ErrorCode, message: &str) -> Self {
        let body = super::http::json_error_body(code.as_str(), message);
        AguiResponse {
            inner: Inner::Rejection(HttpRejection { status, code, body }),
        }
    }

    /// The capability declaration (`200` JSON), or `404` when none is
    /// configured (CAP-12). The `404` body's code, `server.not_found`, is not
    /// an [`ErrorCode`]: `ErrorCode::parse` returns `None` for it.
    #[must_use]
    pub fn capabilities(config: &ServeConfig) -> Self {
        let inner = match super::http::capabilities_response(config) {
            Some(body) => Inner::Json { status: 200, body },
            None => Inner::Json {
                status: 404,
                body: super::http::json_error_body(
                    "server.not_found",
                    "this endpoint declares no capabilities",
                ),
            },
        };
        AguiResponse { inner }
    }
}

/// Keeps the keep-alive interval of the [`ServeConfig`] the response was
/// made with ([`super::http::handle`]).
impl From<HttpResponse> for AguiResponse {
    fn from(response: HttpResponse) -> Self {
        let keepalive = response.body.keepalive_every();
        AguiResponse {
            inner: Inner::Stream {
                response,
                keepalive,
            },
        }
    }
}

impl From<HttpRejection> for AguiResponse {
    fn from(rejection: HttpRejection) -> Self {
        AguiResponse {
            inner: Inner::Rejection(rejection),
        }
    }
}

fn json_response(status: u16, body: String) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(JSON_MEDIA_TYPE));
    headers.insert(VARY, HeaderValue::from_static("Accept"));
    response
}

impl axum_core::response::IntoResponse for AguiResponse {
    fn into_response(self) -> axum_core::response::Response {
        match self.inner {
            Inner::Rejection(rejection) => json_response(rejection.status, rejection.body),
            Inner::Json { status, body } => json_response(status, body),
            Inner::Stream {
                response,
                keepalive,
            } => {
                let content_type = response.content_type;
                let body = StreamBody::new(response.body, keepalive);
                let mut response = Response::new(Body::new(body));
                let headers = response.headers_mut();
                headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
                headers.insert(VARY, HeaderValue::from_static("Accept"));
                headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                response
            }
        }
    }
}

/// The response body: the encoded chunks, with a keep-alive whenever the
/// stream has been quiet for the interval (one timer per response).
struct StreamBody {
    body: ResponseBody,
    keepalive: Option<(Duration, Pin<Box<tokio::time::Sleep>>)>,
    ended: bool,
}

impl StreamBody {
    fn new(body: ResponseBody, interval: Option<Duration>) -> Self {
        let keepalive = match (interval, body.keepalive()) {
            (Some(every), Some(_)) if !every.is_zero() => {
                Some((every, Box::pin(tokio::time::sleep(every))))
            }
            _ => None,
        };
        StreamBody {
            body,
            keepalive,
            ended: false,
        }
    }

    fn rearm(&mut self) {
        let Some((every, sleep)) = &mut self.keepalive else {
            return;
        };
        match tokio::time::Instant::now().checked_add(*every) {
            Some(at) => sleep.as_mut().reset(at),
            // Beyond any reachable instant: the keep-alive never fires.
            None => self.keepalive = None,
        }
    }
}

impl http_body::Body for StreamBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        if this.ended {
            return Poll::Ready(None);
        }
        match this.body.poll_next(cx) {
            Poll::Ready(Some(chunk)) => {
                this.rearm();
                return Poll::Ready(Some(Ok(Frame::data(chunk.into_bytes()))));
            }
            Poll::Ready(None) => {
                this.ended = true;
                return Poll::Ready(None);
            }
            Poll::Pending => {}
        }
        let fired = match &mut this.keepalive {
            Some((_, sleep)) => sleep.as_mut().poll(cx).is_ready(),
            None => false,
        };
        if fired {
            if let Some(chunk) = this.body.keepalive() {
                this.rearm();
                return Poll::Ready(Some(Ok(Frame::data(chunk.into_bytes()))));
            }
        }
        Poll::Pending
    }

    fn is_end_stream(&self) -> bool {
        self.ended
    }
}

/// Handles one agent request: negotiation, admission and the run, as
/// [`super::http::handle`] does, with a fresh cancel token wired to client
/// disconnect.
#[must_use]
pub fn respond<A: Agent>(
    runner: &Runner<A>,
    config: &ServeConfig,
    request: AguiRequest,
) -> AguiResponse {
    let AguiRequest {
        content_type,
        accept,
        body,
        extensions,
    } = request;
    let http_request = HttpRequest::new(content_type.as_deref(), accept.as_deref(), &body)
        .with_extensions(extensions);
    match super::http::handle(runner, config, http_request, CancelToken::new()) {
        Ok(response) => AguiResponse {
            inner: Inner::Stream {
                response,
                keepalive: config.keepalive,
            },
        },
        Err(rejection) => rejection.into(),
    }
}
