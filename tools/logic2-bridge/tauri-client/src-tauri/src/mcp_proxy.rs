//! A transparent proxy in front of Logic 2's MCP server.
//!
//! Logic 2 hosts the MCP server itself, so the Bridge cannot provide one; what it can
//! do is sit in the path and report what an agent is asking for, next to the waveform
//! the agent is talking about. Running the agent in a terminal otherwise means watching
//! two places at once.
//!
//! The proxy deliberately lives in the desktop client rather than in a capture session.
//! Logic 2's MCP server is independent of the Bridge, so a session-scoped proxy would
//! leave anyone driving a real Saleae device -- who never starts a session -- without
//! the panel. Here it is available whenever the app is.
//!
//! This is a forwarder, not an MCP implementation. `initialize`, `tools/list`,
//! notifications, the server-to-client SSE stream and session teardown all pass through
//! untouched; only `tools/call` is inspected.

use std::{
    collections::HashSet,
    convert::Infallible,
    future::Future,
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
};

use bytes::Bytes;
use futures_util::stream;
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::{
    body::{Body, Frame, Incoming},
    header::{HeaderMap, HeaderName, HeaderValue},
    service::service_fn,
    Method, Request, Response, StatusCode,
};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioIo},
};
use tokio::net::{TcpListener, TcpStream};

/// Logic 2's own default. Configurable in its Settings > Automation panel, so the
/// upstream port is a setting here too.
pub const DEFAULT_UPSTREAM_PORT: u16 = 10530;
/// Adjacent to Logic 2's port so the pair is recognisable. Fixed by default because an
/// agent's MCP registration is written once and expected to keep working.
pub const DEFAULT_LISTEN_PORT: u16 = 10531;

/// HTTP/1 hop-by-hop headers belong to one connection and must not cross the proxy.
/// Everything else is forwarded, including MCP headers and future extension headers the
/// proxy does not know yet.
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
];

type ProxyBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// What the proxy ended up listening on, and what it was asked for.
///
/// The two differ when the preferred port was taken, which the UI has to say out loud:
/// the agent's registration points at a specific port, so a silent fallback would look
/// like the proxy simply not working.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoundPorts {
    pub requested_listen_port: u16,
    pub listen_port: u16,
    pub upstream_port: u16,
}

impl BoundPorts {
    pub fn fell_back(&self) -> bool {
        self.listen_port != self.requested_listen_port
    }
}

/// Everything the proxy needs from its host application.
///
/// Kept as a trait object so the forwarding path has no idea it is inside Tauri: the
/// tests drive it with a recorder instead.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObservationContext {
    pub session_id: Option<String>,
}

pub trait ProxyObserver: Send + Sync + 'static {
    /// A complete JSON-RPC message on its way to Logic 2.
    fn observe_request(&self, _context: &ObservationContext, _body: &[u8]) {}
    /// A complete JSON-RPC message on its way back, reassembled from JSON or an SSE
    /// `data:` event. Observation is best-effort and never changes forwarded bytes.
    fn observe_response(&self, _context: &ObservationContext, _body: &[u8]) {}
    /// A successfully accepted DELETE ends the transport session. Keeping this hook at
    /// the transport boundary also gives the later approval gate one place to clear its
    /// session-scoped decisions.
    fn observe_session_closed(&self, _context: &ObservationContext) {}
    /// Whether a `tools/call` may proceed. The default lets everything through, which
    /// is what a transparent proxy means.
    fn review<'a>(
        &'a self,
        _context: &'a ObservationContext,
        _call: &'a ToolCall,
    ) -> Pin<Box<dyn Future<Output = Verdict> + Send + 'a>> {
        Box::pin(async { Verdict::Allow })
    }
    /// Tools this host serves itself, added to whatever Logic 2 advertises.
    ///
    /// Logic 2's own tools are defined inside its renderer against `rapidDataStore`, so
    /// what that store holds without a tool in front of it -- timing markers among it --
    /// is unreachable through the protocol. Adding them here keeps one endpoint and one
    /// tool list for the agent, which cannot tell the two apart and does not need to.
    fn local_tools(&self) -> Vec<serde_json::Value> {
        Vec::new()
    }
    /// Answers a call to one of `local_tools`. `None` means it is not ours and must be
    /// forwarded, so an unrecognised name can never be silently swallowed.
    fn call_local_tool<'a>(
        &'a self,
        _call: &'a ToolCall,
    ) -> Pin<Box<dyn Future<Output = Option<serde_json::Value>> + Send + 'a>> {
        Box::pin(async { None })
    }
}

/// A `tools/call` awaiting a decision.
#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: serde_json::Value,
    pub tool: String,
    pub arguments: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Refused, with a reason the agent will read.
    Deny(String),
}

pub struct ProxyRuntime {
    client: Client<HttpConnector, Full<Bytes>>,
    upstream: SocketAddr,
    observer: Arc<dyn ProxyObserver>,
}

/// Binds the preferred port, falling back to an ephemeral one when it is taken.
///
/// Returns the listener so the caller learns the real port before anything is served,
/// which is what the UI displays.
pub async fn bind_listener(preferred: u16) -> std::io::Result<(TcpListener, u16)> {
    // Loopback only. An MCP server on 0.0.0.0 would be reachable from the network with
    // no authentication whatsoever, which is exactly what the transport spec warns about.
    let loopback = Ipv4Addr::LOCALHOST;
    let listener = match TcpListener::bind(SocketAddr::from((loopback, preferred))).await {
        Ok(listener) => listener,
        Err(_) => TcpListener::bind(SocketAddr::from((loopback, 0))).await?,
    };
    // Read back rather than echoing `preferred`: the two differ after a fallback, and
    // also when the caller asked for port 0 to mean "anything free".
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

/// Serves until the listener fails. Each connection is handled independently so one
/// agent disconnecting cannot disturb another.
pub async fn serve(listener: TcpListener, runtime: Arc<ProxyRuntime>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let runtime = Arc::clone(&runtime);
                async move { Ok::<_, Infallible>(handle(request, runtime).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

impl ProxyRuntime {
    pub fn new(upstream_port: u16, observer: Arc<dyn ProxyObserver>) -> Self {
        Self {
            client: Client::builder(TokioExecutor::new()).build_http(),
            upstream: SocketAddr::from((Ipv4Addr::LOCALHOST, upstream_port)),
            observer,
        }
    }
}

async fn handle(request: Request<Incoming>, runtime: Arc<ProxyRuntime>) -> Response<ProxyBody> {
    if let Some(rejection) = reject_foreign_origin(request.headers()) {
        return rejection;
    }
    let method = request.method().clone();
    let (parts, body) = request.into_parts();
    let context = ObservationContext {
        session_id: parts
            .headers
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    };
    // The request body is collected because a decision may depend on it -- and because a
    // single JSON-RPC message is small. Responses are the opposite case and must never
    // be collected: an SSE stream stays open for the length of a tool call.
    let Ok(collected) = body.collect().await else {
        return json_rpc_error(
            StatusCode::BAD_REQUEST,
            &serde_json::Value::Null,
            "无法读取请求内容",
        );
    };
    let body = collected.to_bytes();

    if method == Method::POST && !body.is_empty() {
        runtime.observer.observe_request(&context, &body);
        if let Some(call) = parse_tool_call(&body) {
            if let Verdict::Deny(reason) = runtime.observer.review(&context, &call).await {
                // A refusal is answered rather than dropped: the agent gets a JSON-RPC
                // error carrying its own request id, instead of a request that never
                // returns. Report the local response too so the activity does not remain
                // misleadingly pending.
                let payload = json_rpc_error_payload(&call.id, &reason);
                if let Ok(encoded) = serde_json::to_vec(&payload) {
                    runtime.observer.observe_response(&context, &encoded);
                }
                return json_rpc_error(StatusCode::OK, &call.id, &reason);
            }
            // A tool this host serves is answered here and never reaches Logic 2. The
            // gate above ran first, so a local tool is reviewed on the same terms as a
            // forwarded one.
            if let Some(result) = runtime.observer.call_local_tool(&call).await {
                let payload = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": call.id,
                    "result": result,
                });
                if let Ok(encoded) = serde_json::to_vec(&payload) {
                    runtime.observer.observe_response(&context, &encoded);
                    return json_response(StatusCode::OK, encoded);
                }
            }
        }
    }

    // Noted before the request body is handed upstream, because the response arm shadows
    // `body` with the reply's own.
    let requested_tool_list = method == Method::POST && is_tools_list_request(&body);

    let uri = match upstream_uri(
        &runtime.upstream,
        parts.uri.path_and_query().map(|p| p.as_str()),
    ) {
        Some(uri) => uri,
        None => {
            return json_rpc_error(
                StatusCode::BAD_REQUEST,
                &request_id(&body),
                "无法构造上游地址",
            )
        }
    };
    let mut upstream_request = Request::builder().method(method.clone()).uri(uri);
    for (name, value) in parts.headers.iter() {
        if should_forward_header(name, &parts.headers) {
            upstream_request = upstream_request.header(name, value);
        }
    }
    let Ok(upstream_request) = upstream_request.body(Full::new(body.clone())) else {
        return json_rpc_error(
            StatusCode::BAD_REQUEST,
            &request_id(&body),
            "无法构造上游请求",
        );
    };

    match runtime.client.request(upstream_request).await {
        Ok(response) => {
            let (parts, body) = response.into_parts();
            let mut response_context = context.clone();
            if let Some(session_id) = parts
                .headers
                .get("mcp-session-id")
                .and_then(|value| value.to_str().ok())
            {
                response_context.session_id = Some(session_id.to_owned());
            }
            if method == Method::DELETE && parts.status.is_success() {
                runtime.observer.observe_session_closed(&response_context);
            }
            let content_type = parts.headers.get("content-type").cloned();
            // The one response that is rewritten rather than streamed. It is safe to
            // collect precisely because it is a tool list: a small JSON reply that the
            // server has already finished producing. The check is narrow on purpose --
            // an SSE tool list would be left alone rather than buffered, since holding a
            // stream open to edit it is what must never happen here.
            let local_tools = runtime.observer.local_tools();
            let rewrite_tool_list = requested_tool_list
                && !local_tools.is_empty()
                && content_type
                    .as_ref()
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.starts_with("application/json"));
            if rewrite_tool_list {
                let payload = match body.collect().await {
                    Ok(collected) => {
                        let upstream_body = collected.to_bytes();
                        merge_local_tools(&upstream_body, &local_tools)
                            .unwrap_or_else(|| upstream_body.to_vec())
                    }
                    Err(_) => {
                        return json_rpc_error(
                            StatusCode::BAD_GATEWAY,
                            &request_id(&[]),
                            "无法读取上游工具清单",
                        )
                    }
                };
                runtime
                    .observer
                    .observe_response(&response_context, &payload);
                let mut forwarded = Response::builder().status(parts.status);
                for (name, value) in parts.headers.iter() {
                    // Content-Length is dropped: the merged body has its own length.
                    if should_forward_header(name, &parts.headers)
                        && name.as_str() != "content-length"
                    {
                        forwarded = forwarded.header(name, value);
                    }
                }
                return forwarded
                    .body(
                        Full::new(Bytes::from(payload))
                            .map_err(|never| match never {})
                            .boxed(),
                    )
                    .unwrap_or_else(|_| {
                        json_rpc_error(
                            StatusCode::BAD_GATEWAY,
                            &request_id(&[]),
                            "上游响应无法转发",
                        )
                    });
            }
            let mut forwarded = Response::builder().status(parts.status);
            // Response headers pass through wholesale: `Mcp-Session-Id` is assigned here
            // and the client is required to echo it on every later request.
            for (name, value) in parts.headers.iter() {
                if should_forward_header(name, &parts.headers) {
                    forwarded = forwarded.header(name, value);
                }
            }
            let streamed = observed_response_body(
                body,
                Arc::clone(&runtime.observer),
                response_context,
                content_type.as_ref(),
            );
            forwarded.body(streamed).unwrap_or_else(|_| {
                json_rpc_error(
                    StatusCode::BAD_GATEWAY,
                    &request_id(&[]),
                    "上游响应无法转发",
                )
            })
        }
        // Logic 2 not running, or its MCP server switched off. Naming the setting is the
        // difference between a confusing transport error and something actionable.
        Err(_) => json_rpc_error(
            StatusCode::BAD_GATEWAY,
            &request_id(&body),
            &format!(
                "无法连接 Logic 2 的 MCP 服务（127.0.0.1:{}）。请确认 Logic 2 正在运行，\
                 并在 Settings > Automation 中启用 MCP Server。",
                runtime.upstream.port()
            ),
        ),
    }
}

const MAX_OBSERVED_MESSAGE_BYTES: usize = 1024 * 1024;

enum ResponseObservation {
    Json(Vec<u8>),
    Sse(Vec<u8>),
    Ignore,
}

impl ResponseObservation {
    fn for_content_type(content_type: Option<&HeaderValue>) -> Self {
        let media_type = content_type
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .unwrap_or_default();
        if media_type.eq_ignore_ascii_case("text/event-stream") {
            Self::Sse(Vec::new())
        } else if media_type.eq_ignore_ascii_case("application/json")
            || media_type.to_ascii_lowercase().ends_with("+json")
        {
            Self::Json(Vec::new())
        } else {
            Self::Ignore
        }
    }

    fn push(&mut self, bytes: &[u8], observer: &dyn ProxyObserver, context: &ObservationContext) {
        match self {
            Self::Json(buffer) => {
                if buffer.len().saturating_add(bytes.len()) > MAX_OBSERVED_MESSAGE_BYTES {
                    *self = Self::Ignore;
                } else {
                    buffer.extend_from_slice(bytes);
                }
            }
            Self::Sse(buffer) => {
                if buffer.len().saturating_add(bytes.len()) > MAX_OBSERVED_MESSAGE_BYTES {
                    *self = Self::Ignore;
                    return;
                }
                buffer.extend_from_slice(bytes);
                while let Some((end, delimiter_len)) = sse_event_end(buffer) {
                    let event = buffer[..end].to_vec();
                    buffer.drain(..end + delimiter_len);
                    let mut data = Vec::new();
                    for line in event.split(|byte| *byte == b'\n') {
                        let line = line.strip_suffix(b"\r").unwrap_or(line);
                        let Some(value) = line.strip_prefix(b"data:") else {
                            continue;
                        };
                        let value = value.strip_prefix(b" ").unwrap_or(value);
                        if !data.is_empty() {
                            data.push(b'\n');
                        }
                        data.extend_from_slice(value);
                    }
                    if !data.is_empty() {
                        observer.observe_response(context, &data);
                    }
                }
            }
            Self::Ignore => {}
        }
    }

    fn finish(self, observer: &dyn ProxyObserver, context: &ObservationContext) {
        if let Self::Json(buffer) = self {
            if !buffer.is_empty() {
                observer.observe_response(context, &buffer);
            }
        }
    }
}

fn sse_event_end(buffer: &[u8]) -> Option<(usize, usize)> {
    for index in 0..buffer.len().saturating_sub(1) {
        if buffer[index..].starts_with(b"\r\n\r\n") {
            return Some((index, 4));
        }
        if buffer[index..].starts_with(b"\n\n") {
            return Some((index, 2));
        }
    }
    None
}

fn observed_response_body(
    body: Incoming,
    observer: Arc<dyn ProxyObserver>,
    context: ObservationContext,
    content_type: Option<&HeaderValue>,
) -> ProxyBody {
    struct State {
        body: Incoming,
        observer: Arc<dyn ProxyObserver>,
        context: ObservationContext,
        observation: ResponseObservation,
    }

    let state = State {
        body,
        observer,
        context,
        observation: ResponseObservation::for_content_type(content_type),
    };
    let frames = stream::unfold(state, |mut state| async move {
        match state.body.frame().await {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    state
                        .observation
                        .push(data, state.observer.as_ref(), &state.context);
                }
                // A client may stop polling as soon as Content-Length bytes arrive, so
                // waiting for one more poll that returns `None` would lose the event.
                if state.body.is_end_stream() {
                    let observation =
                        std::mem::replace(&mut state.observation, ResponseObservation::Ignore);
                    observation.finish(state.observer.as_ref(), &state.context);
                }
                Some((
                    Ok::<Frame<Bytes>, Box<dyn std::error::Error + Send + Sync>>(frame),
                    state,
                ))
            }
            Some(Err(error)) => Some((
                Err(Box::new(error) as Box<dyn std::error::Error + Send + Sync>),
                state,
            )),
            None => {
                state
                    .observation
                    .finish(state.observer.as_ref(), &state.context);
                None
            }
        }
    });
    StreamBody::new(frames).boxed()
}

/// Refuses a request whose `Origin` is not local.
///
/// The transport spec requires this: without it a web page could drive a local MCP
/// server through DNS rebinding. A missing `Origin` is allowed because command-line MCP
/// clients do not send one.
pub fn reject_foreign_origin(headers: &HeaderMap<HeaderValue>) -> Option<Response<ProxyBody>> {
    let value = headers.get("origin")?;
    let origin = match value.to_str() {
        Ok(origin) => origin,
        Err(_) => {
            return Some(json_rpc_error(
                StatusCode::FORBIDDEN,
                &serde_json::Value::Null,
                "拒绝无法验证的 Origin 请求头",
            ))
        }
    };
    if is_local_origin(origin) {
        return None;
    }
    Some(json_rpc_error(
        StatusCode::FORBIDDEN,
        &serde_json::Value::Null,
        "拒绝非本机来源的请求",
    ))
}

pub fn is_local_origin(origin: &str) -> bool {
    let Some(host) = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .map(|rest| rest.split('/').next().unwrap_or(rest))
    else {
        return false;
    };
    let host = host.rsplit_once(':').map_or(host, |(name, _)| name);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

fn should_forward_header(name: &HeaderName, headers: &HeaderMap<HeaderValue>) -> bool {
    if HOP_BY_HOP_HEADERS
        .iter()
        .any(|blocked| name.as_str().eq_ignore_ascii_case(blocked))
    {
        return false;
    }
    !headers
        .get_all("connection")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|nominated| name.as_str().eq_ignore_ascii_case(nominated))
}

fn upstream_uri(upstream: &SocketAddr, path_and_query: Option<&str>) -> Option<hyper::Uri> {
    format!("http://{}{}", upstream, path_and_query.unwrap_or("/"))
        .parse()
        .ok()
}

/// Extracts the `id` of a JSON-RPC request so a synthesised error can be matched to it.
/// Absent or unparseable bodies yield null, which is what the spec uses for errors that
/// cannot be attributed.
pub fn request_id(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("id").cloned())
        .unwrap_or(serde_json::Value::Null)
}

/// Recognises a `tools/list` request, whose reply is the only one this proxy rewrites.
pub fn is_tools_list_request(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("method")
                .and_then(|method| method.as_str())
                .map(|method| method == "tools/list")
        })
        .unwrap_or(false)
}

/// Recognises a `tools/call` request. Anything else -- including malformed JSON -- is
/// simply not a tool call, and is forwarded.
pub fn parse_tool_call(body: &[u8]) -> Option<ToolCall> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    if value.get("method")?.as_str()? != "tools/call" {
        return None;
    }
    let params = value.get("params")?;
    Some(ToolCall {
        id: value.get("id").cloned().unwrap_or(serde_json::Value::Null),
        tool: params.get("name")?.as_str()?.to_string(),
        arguments: params
            .get("arguments")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    })
}

/// Builds a JSON-RPC error response. Used for refusals and for transport problems alike,
/// so the agent always receives a JSON-RPC answer rather than a bare HTTP failure.
pub fn json_rpc_error_payload(id: &serde_json::Value, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32000, "message": message },
    })
}

/// Sends an already-encoded JSON-RPC message as the response body.
pub fn json_response(status: StatusCode, body: Vec<u8>) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .header(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        )
        .body(
            Full::new(Bytes::from(body))
                .map_err(|never| match never {})
                .boxed(),
        )
        .expect("a JSON response is always well formed")
}

/// Adds this host's tools to a `tools/list` result.
///
/// The upstream reply is rewritten rather than replaced, so Logic 2 stays the authority
/// on its own tools and a version that adds one needs no change here. A body that is
/// not a tool list is returned untouched: guessing at an unexpected shape would be
/// worse than forwarding it.
pub fn merge_local_tools(body: &[u8], local: &[serde_json::Value]) -> Option<Vec<u8>> {
    if local.is_empty() {
        return None;
    }
    let mut value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let tools = value.get_mut("result")?.get_mut("tools")?.as_array_mut()?;
    // A name Logic 2 already serves wins, so a future official tool is never shadowed
    // by ours.
    let existing: HashSet<String> = tools
        .iter()
        .filter_map(|tool| tool.get("name")?.as_str().map(str::to_owned))
        .collect();
    let mut added = false;
    for tool in local {
        let Some(name) = tool.get("name").and_then(|name| name.as_str()) else {
            continue;
        };
        if existing.contains(name) {
            continue;
        }
        tools.push(tool.clone());
        added = true;
    }
    if !added {
        return None;
    }
    serde_json::to_vec(&value).ok()
}

pub fn json_rpc_error(
    status: StatusCode,
    id: &serde_json::Value,
    message: &str,
) -> Response<ProxyBody> {
    let payload = json_rpc_error_payload(id, message);
    let body = Bytes::from(serde_json::to_vec(&payload).unwrap_or_default());
    Response::builder()
        .status(status)
        .header(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        )
        .body(Full::new(body).map_err(|never| match never {}).boxed())
        .expect("a JSON error response is always well formed")
}

/// Opens a connection to the upstream port to see whether anything is listening.
pub async fn upstream_reachable(upstream_port: u16) -> bool {
    TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, upstream_port)))
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use http_body_util::StreamBody;
    use hyper::body::Frame;
    use std::{
        sync::Mutex,
        time::{Duration, Instant},
    };

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime with IO and timers")
    }

    fn headers_with(name: &str, value: &str) -> HeaderMap<HeaderValue> {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
        headers
    }

    #[test]
    fn only_local_origins_are_accepted() {
        // Required by the transport spec: without this a web page could drive a local
        // MCP server through DNS rebinding.
        for origin in [
            "http://localhost",
            "http://localhost:3000",
            "http://127.0.0.1:10531",
            "https://LocalHost:8080",
            "http://[::1]:10531",
        ] {
            assert!(is_local_origin(origin), "{origin} should be local");
        }
        for origin in [
            "null",
            "http://evil.com",
            "https://localhost.evil.com",
            "http://127.0.0.1.evil.com",
            "http://10.0.0.5:10531",
            "garbage",
        ] {
            assert!(!is_local_origin(origin), "{origin} should be rejected");
        }
    }

    #[test]
    fn a_missing_origin_is_allowed_but_a_foreign_one_is_not() {
        // Command-line MCP clients send no Origin at all, so absence cannot be fatal.
        assert!(reject_foreign_origin(&HeaderMap::new()).is_none());
        assert!(reject_foreign_origin(&headers_with("origin", "http://127.0.0.1:1")).is_none());
        let rejected = reject_foreign_origin(&headers_with("origin", "http://evil.com"))
            .expect("a foreign origin must be refused");
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        let mut invalid = HeaderMap::new();
        invalid.insert(
            "origin",
            HeaderValue::from_bytes(b"http://local\xffhost").unwrap(),
        );
        assert_eq!(
            reject_foreign_origin(&invalid).unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn a_synthesised_error_carries_the_request_it_answers() {
        // The point of answering rather than dropping: the agent matches the error to its
        // own request instead of waiting for a response that never comes.
        let id = serde_json::json!(7);
        let response = json_rpc_error(StatusCode::OK, &id, "被用户拒绝");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json"
        );
    }

    #[test]
    fn a_request_id_survives_anything_the_body_might_be() {
        assert_eq!(
            request_id(br#"{"id":4,"method":"tools/call"}"#),
            serde_json::json!(4)
        );
        assert_eq!(request_id(br#"{"id":"abc"}"#), serde_json::json!("abc"));
        // Notifications have no id, and a malformed body has nothing to read.
        assert_eq!(
            request_id(br#"{"method":"notify"}"#),
            serde_json::Value::Null
        );
        assert_eq!(request_id(b"{ not json"), serde_json::Value::Null);
        assert_eq!(request_id(b""), serde_json::Value::Null);
    }

    #[test]
    fn only_a_tools_call_is_recognised_as_one() {
        let call = parse_tool_call(
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"start_capture","arguments":{"seconds":2}}}"#,
        )
        .expect("a tools/call must be recognised");
        assert_eq!(call.tool, "start_capture");
        assert_eq!(call.id, serde_json::json!(1));
        assert_eq!(call.arguments, serde_json::json!({"seconds": 2}));

        // Everything else passes through untouched, malformed bodies included: the
        // observer must never be the reason a message fails to reach Logic 2.
        assert!(parse_tool_call(br#"{"id":1,"method":"tools/list"}"#).is_none());
        assert!(parse_tool_call(br#"{"id":1,"method":"initialize"}"#).is_none());
        assert!(parse_tool_call(br#"{"id":1,"method":"tools/call"}"#).is_none());
        assert!(parse_tool_call(b"{ not json").is_none());
        assert!(parse_tool_call(b"").is_none());
    }

    #[test]
    fn a_taken_port_falls_back_instead_of_failing() {
        // The window says so when this happens, because an agent registered against the
        // preferred port would otherwise fail to connect for no visible reason.
        runtime().block_on(async {
            let (first, port) = bind_listener(0).await.expect("an ephemeral port");
            assert_ne!(port, 0, "the real port must be reported, not the request");
            let (_second, fallback) = bind_listener(port)
                .await
                .expect("binding must fall back rather than fail");
            assert_ne!(fallback, port);
            assert_ne!(fallback, 0);
            drop(first);
        });
    }

    /// What a fake Logic 2 recorded about the request it was given.
    #[derive(Default)]
    struct Recorder {
        method: Mutex<Option<String>>,
        headers: Mutex<HeaderMap<HeaderValue>>,
        body: Mutex<Vec<u8>>,
    }

    /// Runs a stand-in for Logic 2's MCP server, and a proxy in front of it. Returns the
    /// proxy's port and what the upstream saw.
    async fn proxy_in_front_of<F>(respond: F) -> (u16, Arc<Recorder>)
    where
        F: Fn(&Recorder) -> Response<ProxyBody> + Send + Sync + 'static,
    {
        proxy_in_front_of_observer(respond, Arc::new(TransparentObserver)).await
    }

    async fn proxy_in_front_of_observer<F>(
        respond: F,
        observer: Arc<dyn ProxyObserver>,
    ) -> (u16, Arc<Recorder>)
    where
        F: Fn(&Recorder) -> Response<ProxyBody> + Send + Sync + 'static,
    {
        let recorder = Arc::new(Recorder::default());
        let upstream = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let respond = Arc::new(respond);
        let upstream_recorder = Arc::clone(&recorder);
        tokio::spawn(async move {
            while let Ok((stream, _)) = upstream.accept().await {
                let recorder = Arc::clone(&upstream_recorder);
                let respond = Arc::clone(&respond);
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let recorder = Arc::clone(&recorder);
                        let respond = Arc::clone(&respond);
                        async move {
                            *recorder.method.lock().unwrap() = Some(request.method().to_string());
                            *recorder.headers.lock().unwrap() = request.headers().clone();
                            let body = request.into_body().collect().await.unwrap().to_bytes();
                            *recorder.body.lock().unwrap() = body.to_vec();
                            Ok::<_, Infallible>(respond(&recorder))
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });

        let (listener, listen_port) = bind_listener(0).await.unwrap();
        let proxy = Arc::new(ProxyRuntime::new(upstream_port, observer));
        tokio::spawn(serve(listener, proxy));
        (listen_port, recorder)
    }

    struct TransparentObserver;
    impl ProxyObserver for TransparentObserver {}

    /// Spelled out so inference does not pick an error type of its own.
    fn body_of(bytes: &'static [u8]) -> ProxyBody {
        Full::new(Bytes::from_static(bytes))
            .map_err(|never| match never {})
            .boxed()
    }

    fn client() -> Client<HttpConnector, Full<Bytes>> {
        Client::builder(TokioExecutor::new()).build_http()
    }

    #[test]
    fn a_tools_list_request_is_recognised_and_others_are_not() {
        assert!(is_tools_list_request(
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#
        ));
        assert!(!is_tools_list_request(
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call"}"#
        ));
        assert!(!is_tools_list_request(b"not json"));
        assert!(!is_tools_list_request(b""));
    }

    #[test]
    fn local_tools_are_appended_to_the_upstream_catalogue() {
        let upstream = br#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"get_devices"}]}}"#;
        let local = vec![serde_json::json!({"name":"add_timing_marker"})];
        let merged = merge_local_tools(upstream, &local).expect("merged");
        let value: serde_json::Value = serde_json::from_slice(&merged).unwrap();
        let names: Vec<&str> = value["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["get_devices", "add_timing_marker"]);
    }

    #[test]
    fn an_upstream_tool_of_the_same_name_is_never_shadowed() {
        // If Logic 2 ever ships its own marker tool, its definition has to win.
        let upstream =
            br#"{"result":{"tools":[{"name":"add_timing_marker","description":"official"}]}}"#;
        let local = vec![serde_json::json!({"name":"add_timing_marker","description":"ours"})];
        assert!(merge_local_tools(upstream, &local).is_none());
    }

    #[test]
    fn a_body_that_is_not_a_tool_list_is_left_alone() {
        let local = vec![serde_json::json!({"name":"add_timing_marker"})];
        assert!(merge_local_tools(b"not json", &local).is_none());
        assert!(merge_local_tools(br#"{"result":{}}"#, &local).is_none());
        assert!(merge_local_tools(br#"{"error":{"code":-1}}"#, &local).is_none());
    }

    #[test]
    fn merging_nothing_leaves_the_catalogue_untouched() {
        let upstream = br#"{"result":{"tools":[{"name":"get_devices"}]}}"#;
        assert!(merge_local_tools(upstream, &[]).is_none());
    }

    #[derive(Default)]
    struct LocalToolObserver {
        served: Mutex<Vec<String>>,
    }

    impl ProxyObserver for LocalToolObserver {
        fn local_tools(&self) -> Vec<serde_json::Value> {
            vec![serde_json::json!({
                "name": "add_timing_marker",
                "description": "ours",
                "inputSchema": {"type":"object"},
            })]
        }
        fn call_local_tool<'a>(
            &'a self,
            call: &'a ToolCall,
        ) -> Pin<Box<dyn Future<Output = Option<serde_json::Value>> + Send + 'a>> {
            Box::pin(async move {
                if call.tool != "add_timing_marker" {
                    return None;
                }
                self.served.lock().unwrap().push(call.tool.clone());
                Some(serde_json::json!({
                    "content": [{"type":"text","text":"{\"id\":4}"}],
                }))
            })
        }
    }

    /// POSTs one JSON-RPC message through the proxy and returns the raw response body.
    async fn post_through(port: u16, body: &'static [u8]) -> Vec<u8> {
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("http://127.0.0.1:{port}/"))
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from_static(body)))
            .unwrap();
        let response = client().request(request).await.unwrap();
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    #[test]
    fn the_tool_list_the_agent_sees_includes_both_sources() {
        runtime().block_on(async {
            let (port, _recorder) = proxy_in_front_of_observer(
                |_| {
                    Response::builder()
                        .header("content-type", "application/json")
                        .body(body_of(
                            br#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"get_devices"}]}}"#,
                        ))
                        .unwrap()
                },
                Arc::new(LocalToolObserver::default()),
            )
            .await;
            let response =
                post_through(port, br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).await;
            let value: serde_json::Value = serde_json::from_slice(&response).unwrap();
            let names: Vec<&str> = value["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect();
            assert_eq!(names, vec!["get_devices", "add_timing_marker"]);
        });
    }

    #[test]
    fn a_local_tool_call_is_answered_without_reaching_logic_2() {
        runtime().block_on(async {
            let observer = Arc::new(LocalToolObserver::default());
            let served = Arc::clone(&observer);
            let (port, recorder) = proxy_in_front_of_observer(
                |_| {
                    Response::builder()
                        .header("content-type", "application/json")
                        .body(body_of(br#"{"jsonrpc":"2.0","id":9,"error":{"code":-32601}}"#))
                        .unwrap()
                },
                observer,
            )
            .await;
            let response = post_through(
                port,
                br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"add_timing_marker","arguments":{"timeSec":1}}}"#,
            )
            .await;
            let value: serde_json::Value = serde_json::from_slice(&response).unwrap();
            assert_eq!(value["id"], 7);
            assert_eq!(value["result"]["content"][0]["type"], "text");
            assert_eq!(served.served.lock().unwrap().len(), 1);
            // The upstream never saw it: its recorder is still empty.
            assert!(recorder.body.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn a_tool_this_host_does_not_serve_is_still_forwarded() {
        runtime().block_on(async {
            let (port, recorder) = proxy_in_front_of_observer(
                |_| {
                    Response::builder()
                        .header("content-type", "application/json")
                        .body(body_of(br#"{"jsonrpc":"2.0","id":3,"result":{"devices":[]}}"#))
                        .unwrap()
                },
                Arc::new(LocalToolObserver::default()),
            )
            .await;
            let response = post_through(
                port,
                br#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_devices","arguments":{}}}"#,
            )
            .await;
            let value: serde_json::Value = serde_json::from_slice(&response).unwrap();
            assert_eq!(value["result"]["devices"].as_array().unwrap().len(), 0);
            let forwarded = recorder.body.lock().unwrap().clone();
            assert!(String::from_utf8_lossy(&forwarded).contains("get_devices"));
        });
    }

    #[test]
    fn an_sse_tool_list_is_streamed_rather_than_rewritten() {
        // Rewriting means collecting, and collecting an SSE stream would hold it open.
        // Correctness here is that the stream passes through untouched.
        runtime().block_on(async {
            let (port, _recorder) = proxy_in_front_of_observer(
                |_| {
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(body_of(
                            b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n",
                        ))
                        .unwrap()
                },
                Arc::new(LocalToolObserver::default()),
            )
            .await;
            let response =
                post_through(port, br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).await;
            let text = String::from_utf8_lossy(&response);
            assert!(text.starts_with("data: "));
            assert!(!text.contains("add_timing_marker"));
        });
    }

    #[derive(Default)]
    struct ObservationRecorder {
        responses: Mutex<Vec<(ObservationContext, Vec<u8>)>>,
    }

    impl ProxyObserver for ObservationRecorder {
        fn observe_response(&self, context: &ObservationContext, body: &[u8]) {
            self.responses
                .lock()
                .unwrap()
                .push((context.clone(), body.to_vec()));
        }
    }

    #[test]
    fn a_json_response_is_observed_without_changing_its_body() {
        runtime().block_on(async {
            let observer = Arc::new(ObservationRecorder::default());
            let (port, _) = proxy_in_front_of_observer(
                |_| {
                    Response::builder()
                        .header("content-type", "application/json; charset=utf-8")
                        .header("mcp-session-id", "observed-session")
                        .body(body_of(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#))
                        .unwrap()
                },
                observer.clone(),
            )
            .await;
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!("http://127.0.0.1:{port}/"))
                .body(Full::new(Bytes::from_static(
                    br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                )))
                .unwrap();
            let response = client().request(request).await.unwrap();
            let body = response.into_body().collect().await.unwrap().to_bytes();

            assert_eq!(&body[..], br#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
            assert_eq!(
                *observer.responses.lock().unwrap(),
                vec![(
                    ObservationContext {
                        session_id: Some("observed-session".to_string())
                    },
                    br#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_vec()
                )]
            );
        });
    }

    #[test]
    fn sse_data_events_are_reassembled_across_chunks_while_bytes_stream_unchanged() {
        runtime().block_on(async {
            let observer = Arc::new(ObservationRecorder::default());
            let expected = b"event: message\r\ndata: {\"jsonrpc\":\"2.0\",\r\ndata: \"id\":1}\r\n\r\ndata: {\"method\":\"notifications/progress\"}\n\n";
            let (port, _) = proxy_in_front_of_observer(
                |_| {
                    let chunks = [
                        &expected[..17],
                        &expected[17..49],
                        &expected[49..83],
                        &expected[83..],
                    ];
                    let frames = futures_util::stream::iter(chunks.into_iter().map(|chunk| {
                        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(Frame::data(
                            Bytes::copy_from_slice(chunk),
                        ))
                    }));
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(BodyExt::boxed(StreamBody::new(frames)))
                        .unwrap()
                },
                observer.clone(),
            )
            .await;
            let response = client()
                .request(
                    Request::builder()
                        .method(Method::GET)
                        .uri(format!("http://127.0.0.1:{port}/"))
                        .header("mcp-session-id", "sse-session")
                        .body(Full::new(Bytes::new()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = response.into_body().collect().await.unwrap().to_bytes();

            assert_eq!(&body[..], expected);
            let seen = observer.responses.lock().unwrap();
            assert_eq!(seen.len(), 2);
            assert_eq!(seen[0].0.session_id.as_deref(), Some("sse-session"));
            assert_eq!(&seen[0].1, b"{\"jsonrpc\":\"2.0\",\n\"id\":1}");
            assert_eq!(
                &seen[1].1,
                b"{\"method\":\"notifications/progress\"}"
            );
        });
    }

    #[test]
    fn a_post_round_trips_with_its_transport_headers_intact() {
        runtime().block_on(async {
            let (port, recorder) = proxy_in_front_of(|_| {
                Response::builder()
                    // Assigned by the server during initialization and required on every
                    // later request, so it has to survive the trip back.
                    .header("mcp-session-id", "session-abc")
                    .header("content-type", "application/json")
                    .body(body_of(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#))
                    .unwrap()
            })
            .await;

            let request = Request::builder()
                .method(Method::POST)
                .uri(format!("http://127.0.0.1:{port}/"))
                .header("accept", "application/json, text/event-stream")
                .header("content-type", "application/json")
                .header("mcp-session-id", "session-abc")
                .header("mcp-protocol-version", "2025-06-18")
                .header("x-mcp-extension", "survives")
                .header("connection", "x-one-hop")
                .header("x-one-hop", "must-not-survive")
                .body(Full::new(Bytes::from_static(
                    br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                )))
                .unwrap();
            let response = client().request(request).await.expect("the proxy answers");

            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers().get("mcp-session-id").unwrap(),
                "session-abc"
            );
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(&body[..], br#"{"jsonrpc":"2.0","id":1,"result":{}}"#);

            let seen = recorder.headers.lock().unwrap();
            assert_eq!(seen.get("mcp-session-id").unwrap(), "session-abc");
            assert_eq!(seen.get("mcp-protocol-version").unwrap(), "2025-06-18");
            assert_eq!(
                seen.get("accept").unwrap(),
                "application/json, text/event-stream"
            );
            assert_eq!(seen.get("x-mcp-extension").unwrap(), "survives");
            assert!(seen.get("connection").is_none());
            assert!(seen.get("x-one-hop").is_none());
            assert_eq!(recorder.method.lock().unwrap().as_deref(), Some("POST"));
            assert_eq!(
                &recorder.body.lock().unwrap()[..],
                br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#
            );
        });
    }

    #[test]
    fn an_sse_response_reaches_the_client_before_the_stream_ends() {
        // The whole point of streaming rather than collecting: a tool call's SSE stream
        // stays open for as long as the call takes, and buffering it would stall the agent
        // until the very end.
        runtime().block_on(async {
            let (port, _) = proxy_in_front_of(|_| {
                let stream = futures_util::stream::unfold(0usize, |step| async move {
                    match step {
                        0 => Some((
                            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(Frame::data(
                                Bytes::from_static(b"event: first\ndata: 1\n\n"),
                            )),
                            1,
                        )),
                        1 => {
                            tokio::time::sleep(Duration::from_millis(400)).await;
                            Some((
                                Ok(Frame::data(Bytes::from_static(b"event: last\ndata: 2\n\n"))),
                                2,
                            ))
                        }
                        _ => None,
                    }
                });
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(BodyExt::boxed(StreamBody::new(stream)))
                    .unwrap()
            })
            .await;

            let request = Request::builder()
                .method(Method::POST)
                .uri(format!("http://127.0.0.1:{port}/"))
                .header("accept", "text/event-stream")
                .body(Full::new(Bytes::from_static(
                    br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"read","arguments":{}}}"#,
                )))
                .unwrap();
            let started = Instant::now();
            let response = client().request(request).await.expect("the proxy answers");
            assert_eq!(
                response.headers().get("content-type").unwrap(),
                "text/event-stream"
            );

            let mut body = response.into_body().into_data_stream();
            let first = body.next().await.expect("a first chunk").unwrap();
            let first_arrived = started.elapsed();
            assert_eq!(&first[..], b"event: first\ndata: 1\n\n");
            // Arrived while the upstream was still sleeping, which a collected body could
            // not have done.
            assert!(
                first_arrived < Duration::from_millis(300),
                "first chunk took {first_arrived:?}, so the body was buffered"
            );

            let last = body.next().await.expect("a second chunk").unwrap();
            assert_eq!(&last[..], b"event: last\ndata: 2\n\n");
            assert!(started.elapsed() >= Duration::from_millis(400));
        });
    }

    #[test]
    fn session_teardown_is_forwarded_rather_than_answered_locally() {
        runtime().block_on(async {
            let (port, recorder) = proxy_in_front_of(|_| {
                Response::builder()
                    .status(StatusCode::NO_CONTENT)
                    .body(body_of(b""))
                    .unwrap()
            })
            .await;

            let request = Request::builder()
                .method(Method::DELETE)
                .uri(format!("http://127.0.0.1:{port}/"))
                .header("mcp-session-id", "session-abc")
                .body(Full::new(Bytes::new()))
                .unwrap();
            let response = client().request(request).await.expect("the proxy answers");

            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            assert_eq!(recorder.method.lock().unwrap().as_deref(), Some("DELETE"));
            assert_eq!(
                recorder
                    .headers
                    .lock()
                    .unwrap()
                    .get("mcp-session-id")
                    .unwrap(),
                "session-abc"
            );
        });
    }

    #[test]
    fn an_unreachable_logic_2_is_reported_as_something_to_go_and_fix() {
        runtime().block_on(async {
            // Nothing is listening on this port: bind it, learn the number, drop it.
            let dead_port = {
                let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                    .await
                    .unwrap();
                listener.local_addr().unwrap().port()
            };
            let (listener, port) = bind_listener(0).await.unwrap();
            let proxy = Arc::new(ProxyRuntime::new(dead_port, Arc::new(TransparentObserver)));
            tokio::spawn(serve(listener, proxy));

            let request = Request::builder()
                .method(Method::POST)
                .uri(format!("http://127.0.0.1:{port}/"))
                .body(Full::new(Bytes::from_static(
                    br#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#,
                )))
                .unwrap();
            let response = client().request(request).await.expect("the proxy answers");

            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
            // Answered as JSON-RPC, matched to the request, and naming the setting to turn
            // on rather than leaving a bare transport failure.
            assert_eq!(payload["id"], serde_json::json!(9));
            let message = payload["error"]["message"].as_str().unwrap();
            assert!(message.contains("Settings > Automation"), "{message}");
            assert!(message.contains(&dead_port.to_string()), "{message}");
        });
    }

    #[test]
    fn a_foreign_origin_is_refused_before_anything_is_forwarded() {
        // The spec requires the check; this proves it happens on the served path and not
        // merely in a helper nobody calls.
        runtime().block_on(async {
            let (port, recorder) =
                proxy_in_front_of(|_| Response::builder().body(body_of(b"")).unwrap()).await;

            let request = Request::builder()
                .method(Method::POST)
                .uri(format!("http://127.0.0.1:{port}/"))
                .header("origin", "http://evil.example")
                .body(Full::new(Bytes::from_static(
                    br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                )))
                .unwrap();
            let response = client().request(request).await.expect("the proxy answers");

            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert!(recorder.method.lock().unwrap().is_none());
        });
    }

    #[test]
    fn a_refused_tool_call_never_reaches_logic_2() {
        struct Refuse;
        impl ProxyObserver for Refuse {
            fn review<'a>(
                &'a self,
                _context: &'a ObservationContext,
                call: &'a ToolCall,
            ) -> Pin<Box<dyn Future<Output = Verdict> + Send + 'a>> {
                Box::pin(async move {
                    assert_eq!(call.tool, "start_capture");
                    Verdict::Deny("被用户拒绝".to_string())
                })
            }
        }

        runtime().block_on(async {
            let recorder = Arc::new(Recorder::default());
            let upstream = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .await
                .unwrap();
            let upstream_port = upstream.local_addr().unwrap().port();
            let upstream_recorder = Arc::clone(&recorder);
            tokio::spawn(async move {
                while let Ok((stream, _)) = upstream.accept().await {
                    let recorder = Arc::clone(&upstream_recorder);
                    tokio::spawn(async move {
                        let service = service_fn(move |request: Request<Incoming>| {
                            let recorder = Arc::clone(&recorder);
                            async move {
                                *recorder.method.lock().unwrap() =
                                    Some(request.method().to_string());
                                Ok::<_, Infallible>(Response::new(body_of(b"")))
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            });

            let (listener, port) = bind_listener(0).await.unwrap();
            tokio::spawn(serve(
                listener,
                Arc::new(ProxyRuntime::new(upstream_port, Arc::new(Refuse))),
            ));

            let request = Request::builder()
                .method(Method::POST)
                .uri(format!("http://127.0.0.1:{port}/"))
                .body(Full::new(Bytes::from_static(
                    br#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"start_capture","arguments":{}}}"#,
                )))
                .unwrap();
            let response = client().request(request).await.expect("the proxy answers");

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(payload["id"], serde_json::json!(3));
            assert_eq!(payload["error"]["message"], serde_json::json!("被用户拒绝"));
            // The upstream was never contacted at all.
            assert!(recorder.method.lock().unwrap().is_none());
        });
    }
}
