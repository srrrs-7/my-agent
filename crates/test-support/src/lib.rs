//! Test doubles shared by the integration suites.
//!
//! The mock LLM is raw TCP rather than an HTTP framework on purpose: it is
//! about sixty lines, it adds no dependency to a project whose whole premise is
//! keeping the supply chain small, and it lets a test assert on the *exact*
//! bytes the client put on the wire.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// One canned HTTP reply.
#[derive(Debug, Clone)]
pub struct Response {
    status_line: String,
    content_type: &'static str,
    body: String,
    location: Option<String>,
}

impl Response {
    /// `200 OK` with the given body.
    pub fn ok(body: impl Into<String>) -> Self {
        Self::status("200 OK", body)
    }

    /// An arbitrary status line, e.g. `"429 Too Many Requests"`.
    pub fn status(status_line: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            status_line: status_line.into(),
            content_type: "application/json",
            body: body.into(),
            location: None,
        }
    }

    /// An HTTP redirect to `location`.
    pub fn redirect(status_line: impl Into<String>, location: impl Into<String>) -> Self {
        Self {
            status_line: status_line.into(),
            content_type: "text/plain",
            body: String::new(),
            location: Some(location.into()),
        }
    }

    /// Full control over the content type - for servers that are not LLM
    /// endpoints (the web-fetch tests serve HTML and redirects with this).
    pub fn with_content_type(
        status_line: impl Into<String>,
        content_type: &'static str,
        body: impl Into<String>,
    ) -> Self {
        Self {
            status_line: status_line.into(),
            content_type,
            body: body.into(),
            location: None,
        }
    }

    /// A `text/event-stream` reply: each payload becomes one `data:` event,
    /// followed by `data: [DONE]`. Pass raw JSON strings (or use
    /// [`Self::sse_text_stream`] / [`Self::sse_tool_call_stream`] for the
    /// common shapes).
    pub fn sse(payloads: impl IntoIterator<Item = String>) -> Self {
        let mut body = String::new();
        for payload in payloads {
            body.push_str("data: ");
            body.push_str(&payload);
            body.push_str("\n\n");
        }
        body.push_str("data: [DONE]\n\n");
        Self {
            status_line: "200 OK".into(),
            content_type: "text/event-stream",
            body,
            location: None,
        }
    }

    /// An OpenAI-shaped streamed text answer: one content-delta chunk per
    /// string, then a `finish_reason: "stop"` chunk and a usage chunk.
    pub fn sse_text_stream(deltas: &[&str]) -> Self {
        let mut payloads: Vec<String> = deltas
            .iter()
            .map(|delta| {
                json!({
                    "model": "mock",
                    "choices": [{"index": 0, "delta": {"content": delta}, "finish_reason": null}]
                })
                .to_string()
            })
            .collect();
        payloads.push(
            json!({
                "model": "mock",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
            })
            .to_string(),
        );
        payloads.push(
            json!({
                "model": "mock",
                "choices": [],
                "usage": {"prompt_tokens": 150, "completion_tokens": 30}
            })
            .to_string(),
        );
        Self::sse(payloads)
    }

    /// An OpenAI-shaped streamed tool call whose arguments arrive split into
    /// the given fragments - the case that forces consumers to aggregate
    /// before executing.
    pub fn sse_tool_call_stream(id: &str, name: &str, argument_fragments: &[&str]) -> Self {
        let mut payloads = vec![
            json!({
                "model": "mock",
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0, "id": id, "type": "function",
                    "function": {"name": name, "arguments": ""}
                }]}, "finish_reason": null}]
            })
            .to_string(),
        ];
        for fragment in argument_fragments {
            payloads.push(
                json!({
                    "model": "mock",
                    "choices": [{"index": 0, "delta": {"tool_calls": [{
                        "index": 0,
                        "function": {"arguments": fragment}
                    }]}, "finish_reason": null}]
                })
                .to_string(),
            );
        }
        payloads.push(
            json!({
                "model": "mock",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
            })
            .to_string(),
        );
        payloads.push(
            json!({
                "model": "mock",
                "choices": [],
                "usage": {"prompt_tokens": 100, "completion_tokens": 20}
            })
            .to_string(),
        );
        Self::sse(payloads)
    }

    /// An OpenAI-shaped final answer.
    pub fn assistant_text(text: &str) -> Self {
        Self::ok(
            json!({
                "model": "mock",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": text},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 150, "completion_tokens": 30}
            })
            .to_string(),
        )
    }

    /// An OpenAI-shaped tool call. `arguments` is the JSON *string* the spec
    /// requires, so callers can also exercise malformed payloads.
    pub fn tool_call(id: &str, name: &str, arguments: &str) -> Self {
        Self::ok(
            json!({
                "model": "mock",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": arguments}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {"prompt_tokens": 100, "completion_tokens": 20}
            })
            .to_string(),
        )
    }

    fn render(&self) -> String {
        let mut response = String::new();
        let _ = write!(
            response,
            "HTTP/1.1 {}\r\ncontent-type: {}\r\ncontent-length: {}\r\n",
            self.status_line,
            self.content_type,
            self.body.len(),
        );
        if let Some(location) = &self.location {
            let _ = write!(response, "location: {location}\r\n");
        }
        let _ = write!(response, "connection: close\r\n\r\n{}", self.body);
        response
    }
}

/// A single-use HTTP server that replays `responses` in order.
///
/// Every reply closes the connection, so requests arrive one per connection and
/// the ordering is deterministic.
pub struct MockLlmServer {
    base_url: String,
    handle: JoinHandle<Vec<String>>,
}

impl MockLlmServer {
    pub async fn start(responses: Vec<Response>) -> Self {
        // Bind before spawning: the port must be listening by the time `start`
        // returns, or a fast client races the server into a refused connection.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let address = listener.local_addr().expect("local address");

        let handle = tokio::spawn(async move {
            let mut received = Vec::new();

            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };

                match read_request_body(&mut socket).await {
                    Some(body) => received.push(body),
                    None => break,
                }

                let _ = socket.write_all(response.render().as_bytes()).await;
                let _ = socket.shutdown().await;
            }

            received
        });

        Self {
            base_url: format!("http://{address}/v1"),
            handle,
        }
    }

    /// The `/v1` prefixed URL an OpenAI-compatible client should be pointed at.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Waits for the script to be consumed and returns the request bodies.
    pub async fn requests(self) -> Vec<String> {
        self.handle.await.expect("the mock server panicked")
    }

    /// [`Self::requests`], parsed as JSON.
    pub async fn json_requests(self) -> Vec<Value> {
        self.requests()
            .await
            .iter()
            .map(|body| serde_json::from_str(body).expect("the client sent valid JSON"))
            .collect()
    }
}

/// One request as [`RecordingServer`] saw it.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    /// The request target as sent, e.g. `/` or `/services/T0/B0/x`.
    pub path: String,
    /// Header names lowercased, in the order they arrived.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl RecordedRequest {
    /// The body parsed as JSON. Panics with the raw body if it is not.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("not JSON ({error}): {}", self.body))
    }

    /// The first header called `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A server that answers *every* request the same way and remembers each one.
///
/// [`MockLlmServer`] replays a script and then stops listening, and its
/// requests can only be read after the script is used up. A webhook receiver
/// needs the opposite: any number of requests - including none - and a look
/// at what arrived without waiting for anything. A request is recorded before
/// it is answered, so a client that saw the reply is guaranteed to find its
/// request in [`Self::received`].
pub struct RecordingServer {
    url: String,
    received: Arc<Mutex<Vec<RecordedRequest>>>,
    handle: JoinHandle<()>,
}

impl RecordingServer {
    /// Replies with `response` to every request.
    pub async fn start(response: Response) -> Self {
        Self::spawn(Some(response)).await
    }

    /// Records each request and never answers it, holding the connection
    /// open - a receiver that has stopped responding.
    pub async fn unresponsive() -> Self {
        Self::spawn(None).await
    }

    async fn spawn(response: Option<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let address = listener.local_addr().expect("local address");
        let received = Arc::new(Mutex::new(Vec::new()));

        let log = received.clone();
        let handle = tokio::spawn(async move {
            // Held so that an unresponsive server keeps its connections open
            // rather than resetting them, which a client would see at once.
            let mut held = Vec::new();
            while let Ok((mut socket, _)) = listener.accept().await {
                let Some(request) = read_request(&mut socket).await else {
                    continue;
                };
                log.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(request);
                match &response {
                    Some(response) => {
                        let _ = socket.write_all(response.render().as_bytes()).await;
                        let _ = socket.shutdown().await;
                    }
                    None => held.push(socket),
                }
            }
        });

        Self {
            url: format!("http://{address}"),
            received,
            handle,
        }
    }

    /// `http://127.0.0.1:<port>`, without a trailing slash.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Everything received so far, oldest first.
    pub fn received(&self) -> Vec<RecordedRequest> {
        self.received
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for RecordingServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Reads one request: the request line, the headers, and a
/// `content-length`-sized body.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<RecordedRequest> {
    let (head, body) = read_head_and_body(socket).await?;
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    Some(RecordedRequest {
        method,
        path,
        headers,
        body,
    })
}

/// Reads headers, honours `content-length`, and returns the body.
async fn read_request_body(socket: &mut tokio::net::TcpStream) -> Option<String> {
    read_head_and_body(socket).await.map(|(_, body)| body)
}

/// The raw head (request line and headers, without the blank line) and the
/// body.
async fn read_head_and_body(socket: &mut tokio::net::TcpStream) -> Option<(String, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];

    let (header_end, content_length) = loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);

        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buffer[..position]).to_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (position + 4, length);
        }
    };

    while buffer.len() < header_end + content_length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    let end = (header_end + content_length).min(buffer.len());
    let head = String::from_utf8_lossy(&buffer[..header_end - 4]).to_string();
    Some((
        head,
        String::from_utf8_lossy(&buffer[header_end..end]).to_string(),
    ))
}

/// What [`StreamingServer`] does with one request.
#[derive(Debug, Clone)]
pub enum Reply {
    /// Answer and close, like [`RecordingServer`].
    Respond(Response),
    /// Answer `200 OK` with a body that never ends on its own: every line
    /// later given to [`StreamingServer::push_line`] is written to it, until
    /// the client hangs up. A long-poll subscription, as ntfy serves one.
    Stream,
}

/// What [`StreamingServer`] has seen so far.
#[derive(Debug, Clone, Default)]
pub struct ServerSnapshot {
    /// Every request, oldest first.
    pub requests: Vec<RecordedRequest>,
    /// Streams currently held open.
    pub open_streams: usize,
    /// Streams the client hung up on.
    pub closed_streams: usize,
}

type Route = Arc<dyn Fn(&RecordedRequest) -> Reply + Send + Sync>;

/// A server that decides per request whether to answer or to hold a stream
/// open, and can write to its open streams later.
///
/// What a subscription needs that [`RecordingServer`] cannot give: a reply
/// written *after* the client did something else (published a message), and
/// a way to see that the client hung up. Waiting is done with
/// [`Self::wait_until`], which wakes on every change, so a test needs no
/// sleep to see a request arrive or a stream close.
pub struct StreamingServer {
    url: String,
    state: Arc<Mutex<ServerSnapshot>>,
    changes: tokio::sync::watch::Sender<u64>,
    lines: tokio::sync::broadcast::Sender<String>,
    handle: JoinHandle<()>,
}

impl StreamingServer {
    pub async fn start(route: impl Fn(&RecordedRequest) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let address = listener.local_addr().expect("local address");
        let state = Arc::new(Mutex::new(ServerSnapshot::default()));
        let (changes, _) = tokio::sync::watch::channel(0_u64);
        let (lines, _) = tokio::sync::broadcast::channel(64);
        let route: Route = Arc::new(route);

        let handle = {
            let state = state.clone();
            let changes = changes.clone();
            let lines = lines.clone();
            tokio::spawn(async move {
                // Owned by the accept loop, so aborting it ends every
                // connection too.
                let mut connections = tokio::task::JoinSet::new();
                while let Ok((socket, _)) = listener.accept().await {
                    connections.spawn(serve(
                        socket,
                        route.clone(),
                        state.clone(),
                        changes.clone(),
                        lines.clone(),
                    ));
                }
            })
        };

        Self {
            url: format!("http://{address}"),
            state,
            changes,
            lines,
            handle,
        }
    }

    /// `http://127.0.0.1:<port>`, without a trailing slash.
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn snapshot(&self) -> ServerSnapshot {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Writes `line` and a newline to every stream open right now.
    pub fn push_line(&self, line: &str) {
        let _ = self.lines.send(line.to_string());
    }

    /// Waits until `condition` holds, and returns the snapshot it held for.
    pub async fn wait_until(
        &self,
        mut condition: impl FnMut(&ServerSnapshot) -> bool,
    ) -> ServerSnapshot {
        let mut changes = self.changes.subscribe();
        loop {
            changes.borrow_and_update();
            let snapshot = self.snapshot();
            if condition(&snapshot) {
                return snapshot;
            }
            changes
                .changed()
                .await
                .expect("the streaming server stopped");
        }
    }
}

impl Drop for StreamingServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve(
    mut socket: tokio::net::TcpStream,
    route: Route,
    state: Arc<Mutex<ServerSnapshot>>,
    changes: tokio::sync::watch::Sender<u64>,
    lines: tokio::sync::broadcast::Sender<String>,
) {
    let update = |change: &dyn Fn(&mut ServerSnapshot)| {
        change(&mut state.lock().unwrap_or_else(PoisonError::into_inner));
        changes.send_modify(|version| *version += 1);
    };

    let Some(request) = read_request(&mut socket).await else {
        return;
    };
    let reply = route(&request);
    // Subscribed before the stream is announced as open, so a line pushed by
    // a test that saw it open is not missed.
    let mut receiver = lines.subscribe();

    match reply {
        Reply::Respond(response) => {
            // Recorded before answering, as in `RecordingServer`: a client
            // that saw the reply finds its request in the record.
            update(&|snapshot| snapshot.requests.push(request.clone()));
            let _ = socket.write_all(response.render().as_bytes()).await;
            let _ = socket.shutdown().await;
        }
        Reply::Stream => {
            let head = "HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\n\
                        connection: close\r\n\r\n";
            update(&|snapshot| {
                snapshot.requests.push(request.clone());
                snapshot.open_streams += 1;
            });
            let mut probe = [0_u8; 1024];
            let mut open = socket.write_all(head.as_bytes()).await.is_ok();
            while open {
                tokio::select! {
                    line = receiver.recv() => {
                        open = match line {
                            Ok(line) => socket.write_all(format!("{line}\n").as_bytes()).await.is_ok(),
                            Err(_) => false,
                        };
                    }
                    read = socket.read(&mut probe) => {
                        open = !matches!(read, Ok(0) | Err(_));
                    }
                }
            }
            update(&|snapshot| {
                snapshot.open_streams -= 1;
                snapshot.closed_streams += 1;
            });
        }
    }
}
