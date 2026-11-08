//! JSON-RPC 2.0 over a pair of byte streams, framed one message per line.
//!
//! This is the MCP stdio transport, written against `AsyncRead` /
//! `AsyncWrite` rather than a process so the routing can be tested over an
//! in-memory pipe. What it guarantees:
//!
//! * **Responses are routed by id**, not by arrival order. A server may answer
//!   concurrent calls in any order.
//! * **Dropping a request cancels it.** The pending entry is removed and
//!   `notifications/cancelled` is sent, so the timeout decorator (or an
//!   interrupted turn) both frees the slot and tells the server to stop. A
//!   response that arrives afterwards matches nothing and is discarded.
//! * **Every message is written whole.** One task owns the outgoing stream
//!   and is handed complete lines over a queue; a caller that is dropped has
//!   either queued its line or not, and never leaves half of one behind for
//!   the next message to be glued onto. `write_all` is not cancel-safe, so
//!   writing from the caller's own future would.
//! * **No message may exceed the cap.** A server that sends one larger is
//!   treated as broken and the connection is closed: there is no way to tell
//!   which call an unread message answered, so skipping it would leave that
//!   call waiting for nothing.
//! * **What waits to be written is bounded** ([`MAX_QUEUED_MESSAGES`]). A
//!   server that keeps sending requests while never reading the answers would
//!   otherwise make this process hold them without limit; past the bound the
//!   connection is closed.
//! * **Once closed, always closed.** Every waiting call fails, and every later
//!   one fails at once with the reason.
//!
//! Requests from the server are answered "method not found", except `ping`.
//! This client offers no capabilities, so a well-behaved server sends none.
//! A request whose id is neither a number nor a short string
//! ([`MAX_REQUEST_ID_BYTES`]) is ignored: the answer would echo it, and an id
//! can be megabytes long.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinHandle};
use tracing::debug;

/// The largest message either side may send, in bytes.
pub(crate) const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Messages queued for the server and not yet written. Shared by this
/// client's own messages and its answers to the server's requests; a server
/// that has let this many pile up is not reading its input.
pub(crate) const MAX_QUEUED_MESSAGES: usize = 64;

/// The longest string id a request from the server may carry and still be
/// answered.
pub(crate) const MAX_REQUEST_ID_BYTES: usize = 128;

/// How much of a server's error message is kept.
const MAX_ERROR_BYTES: usize = 1024;

const METHOD_NOT_FOUND: i64 = -32601;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RpcError {
    /// The connection is gone; nothing more will be answered.
    Closed(String),
    /// The server answered with a JSON-RPC error.
    Remote { code: i64, message: String },
}

type Reply = oneshot::Sender<Result<Value, RpcError>>;

#[derive(Default)]
struct State {
    pending: HashMap<u64, Reply>,
    closed: Option<String>,
}

struct Shared {
    /// Whole lines, for the writer task.
    outgoing: mpsc::Sender<Vec<u8>>,
    /// The writer task, stopped when the connection closes. Stopping it drops
    /// the stream - for a process, closes its stdin - even when a write to a
    /// server that does not read is stuck.
    writer: OnceLock<AbortHandle>,
    state: Mutex<State>,
    next_id: AtomicU64,
}

impl Shared {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // Nothing panics while holding it, but a poisoned map is still the
        // right map.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `message`, waiting for room. Cancel-safe: a caller dropped
    /// while waiting has queued nothing, and a queued line is written whole
    /// whatever becomes of the caller.
    async fn send(&self, message: &Value) -> Result<(), RpcError> {
        let line = encode(message)?;
        if let Some(reason) = &self.state().closed {
            return Err(RpcError::Closed(reason.clone()));
        }
        self.outgoing
            .send(line)
            .await
            .map_err(|_| RpcError::Closed(self.closed_reason()))
    }

    /// Queues `message` if there is room right now. For callers that cannot
    /// wait: `Drop`, and the reader, which must not stall behind a server
    /// that is not reading.
    fn try_send(&self, message: &Value) -> Result<(), TrySendError<()>> {
        let line = encode(message).map_err(|_| TrySendError::Closed(()))?;
        self.outgoing.try_send(line).map_err(|error| match error {
            TrySendError::Full(_) => TrySendError::Full(()),
            TrySendError::Closed(_) => TrySendError::Closed(()),
        })
    }

    fn closed_reason(&self) -> String {
        self.state()
            .closed
            .clone()
            .unwrap_or_else(|| "the connection is closed".to_string())
    }

    /// Marks the connection closed (keeping the first reason), fails every
    /// call still waiting, and stops the writer.
    fn close_with(&self, reason: String) {
        let waiting: Vec<Reply> = {
            let mut state = self.state();
            if state.closed.is_none() {
                state.closed = Some(reason);
            }
            state.pending.drain().map(|(_, reply)| reply).collect()
        };
        if let Some(writer) = self.writer.get() {
            writer.abort();
        }
        let reason = self.closed_reason();
        for reply in waiting {
            let _ = reply.send(Err(RpcError::Closed(reason.clone())));
        }
    }

    /// Routes a response to whoever is waiting for it, if anyone still is.
    fn deliver(&self, id: u64, outcome: Result<Value, RpcError>) {
        let reply = self.state().pending.remove(&id);
        match reply {
            Some(reply) => {
                let _ = reply.send(outcome);
            }
            None => debug!(id, "discarding a response nobody is waiting for"),
        }
    }
}

fn encode(message: &Value) -> Result<Vec<u8>, RpcError> {
    let mut line = serde_json::to_vec(message)
        .map_err(|error| RpcError::Closed(format!("cannot encode a message: {error}")))?;
    line.push(b'\n');
    Ok(line)
}

/// A connection to one server.
pub(crate) struct RpcClient {
    shared: Arc<Shared>,
    reader: JoinHandle<()>,
}

impl RpcClient {
    /// Starts reading `reader` and writing `writer` in the background. Must
    /// be called inside a tokio runtime.
    pub(crate) fn start<R, W>(reader: R, writer: W, max_message_bytes: usize) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (outgoing, queue) = mpsc::channel(MAX_QUEUED_MESSAGES);
        let shared = Arc::new(Shared {
            outgoing,
            writer: OnceLock::new(),
            state: Mutex::new(State::default()),
            next_id: AtomicU64::new(1),
        });
        let writing = tokio::spawn(write_loop(writer, queue, Arc::downgrade(&shared)));
        let _ = shared.writer.set(writing.abort_handle());
        let reader = tokio::spawn(read_loop(
            BufReader::new(reader),
            shared.clone(),
            max_message_bytes,
        ));
        Self { shared, reader }
    }

    /// Sends a request and waits for its response. Dropping the future
    /// cancels the request.
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, response) = oneshot::channel();
        {
            let mut state = self.shared.state();
            if let Some(reason) = &state.closed {
                return Err(RpcError::Closed(reason.clone()));
            }
            state.pending.insert(id, reply);
        }
        let mut guard = Cancellation {
            shared: self.shared.clone(),
            id,
            queued: false,
            finished: false,
        };

        self.shared
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        guard.queued = true;

        let outcome = response
            .await
            .unwrap_or_else(|_| Err(RpcError::Closed(self.shared.closed_reason())));
        guard.finished = true;
        outcome
    }

    /// Sends a notification (a message that gets no response).
    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        self.shared
            .send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    /// Closes the outgoing stream - for a process, its stdin, which is how a
    /// server is told to shut down - and stops reading. Synchronous, for
    /// `Drop`; the stream itself is dropped as soon as the runtime gets to
    /// the stopped writer.
    pub(crate) fn close(&self) {
        self.reader.abort();
        self.shared
            .close_with("the client shut the connection down".to_string());
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        self.close();
    }
}

/// Removes a request that is being abandoned, and tells the server.
struct Cancellation {
    shared: Arc<Shared>,
    id: u64,
    /// The request is on its way to the server, so there is something there
    /// to cancel.
    queued: bool,
    /// The response arrived (or never will); nothing to clean up.
    finished: bool,
}

impl Drop for Cancellation {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let still_waiting = self.shared.state().pending.remove(&self.id).is_some();
        if !still_waiting || !self.queued {
            return;
        }
        // Without waiting, since `Drop` cannot. With the queue full the
        // notice is skipped: the server is not reading, and closing the
        // connection over a courtesy would turn one abandoned call into a
        // stopped server.
        let _ = self.shared.try_send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": self.id, "reason": "the client abandoned the request"},
        }));
    }
}

/// The only place the outgoing stream is written. Runs until the connection
/// closes and the task is stopped, or a write fails.
async fn write_loop<W: AsyncWrite + Unpin>(
    mut stream: W,
    mut queue: mpsc::Receiver<Vec<u8>>,
    shared: Weak<Shared>,
) {
    while let Some(line) = queue.recv().await {
        let written = async {
            stream.write_all(&line).await?;
            stream.flush().await
        }
        .await;
        if let Err(error) = written {
            if let Some(shared) = shared.upgrade() {
                shared.close_with(format!("writing to the server failed: {error}"));
            }
            return;
        }
    }
}

enum LineError {
    TooLong,
    Io(std::io::Error),
}

/// The next line without its terminator, or `None` at the end of the stream.
/// Refuses to hold more than `max` bytes.
async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<Option<Vec<u8>>, LineError> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(LineError::Io)?;
        if available.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let (take, complete) = match available.iter().position(|&byte| byte == b'\n') {
            Some(end) => (end, true),
            None => (available.len(), false),
        };
        if line.len() + take > max {
            return Err(LineError::TooLong);
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(if complete { take + 1 } else { take });
        if complete {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

async fn read_loop<R: AsyncBufRead + Unpin>(mut reader: R, shared: Arc<Shared>, max: usize) {
    let reason = loop {
        match read_line(&mut reader, max).await {
            Ok(Some(line)) => {
                if let Err(reason) = handle(&shared, &line) {
                    break reason;
                }
            }
            Ok(None) => break "the server closed its output".to_string(),
            Err(LineError::TooLong) => {
                break format!("the server sent a message larger than {max} bytes");
            }
            Err(LineError::Io(error)) => break format!("reading from the server failed: {error}"),
        }
    };
    debug!(%reason, "the connection to a tool server closed");
    shared.close_with(reason);
}

/// Acts on one line. `Err` is the reason to close the connection.
fn handle(shared: &Arc<Shared>, line: &[u8]) -> Result<(), String> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    let Ok(message) = serde_json::from_slice::<Value>(line) else {
        debug!("ignoring a line from the server that is not JSON");
        return Ok(());
    };

    if let Some(method) = message.get("method").and_then(Value::as_str) {
        // A request from the server carries an id; a notification does not.
        // Notifications - `list_changed` included - are ignored: the tool
        // list is fixed at startup.
        let Some(id) = message.get("id").filter(|id| !id.is_null()) else {
            return Ok(());
        };
        if !answerable(id) {
            debug!("ignoring a request from the server with an unusable id");
            return Ok(());
        }
        let reply = if method == "ping" {
            json!({"jsonrpc": "2.0", "id": id, "result": {}})
        } else {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": METHOD_NOT_FOUND, "message": "Method not found"},
            })
        };
        // Queued without waiting: a server that is not reading its stdin
        // must not be able to stall the reading of its stdout, nor to make
        // the answers pile up.
        return match shared.try_send(&reply) {
            Ok(()) | Err(TrySendError::Closed(())) => Ok(()),
            Err(TrySendError::Full(())) => Err(format!(
                "the server sent requests faster than it read the answers ({MAX_QUEUED_MESSAGES} \
                 messages were waiting to be written)"
            )),
        };
    }

    let Some(id) = message.get("id").and_then(response_id) else {
        debug!("ignoring a response without a usable id");
        return Ok(());
    };
    let outcome = match message.get("error") {
        Some(error) => Err(RpcError::Remote {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: agent_domain::text::clip(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("(no message)"),
                MAX_ERROR_BYTES,
            ),
        }),
        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
    };
    shared.deliver(id, outcome);
    Ok(())
}

/// Whether a request's id may be echoed back: JSON-RPC allows a number or a
/// string, and the string is what could be made enormous.
fn answerable(id: &Value) -> bool {
    match id {
        Value::Number(_) => true,
        Value::String(text) => text.len() <= MAX_REQUEST_ID_BYTES,
        _ => false,
    }
}

/// Ids are sent as numbers; a server that echoes them as strings is
/// tolerated.
fn response_id(id: &Value) -> Option<u64> {
    match id {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{DuplexStream, ReadHalf, WriteHalf, duplex, split};

    use super::*;

    /// The server's end of the pipe, driven by hand.
    struct FakeServer {
        lines: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
        writer: WriteHalf<DuplexStream>,
    }

    impl FakeServer {
        async fn receive(&mut self) -> Value {
            let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
                .await
                .expect("the client sent nothing")
                .unwrap()
                .expect("the client closed the pipe");
            serde_json::from_str(&line).unwrap()
        }

        async fn send(&mut self, message: Value) {
            let mut line = serde_json::to_vec(&message).unwrap();
            line.push(b'\n');
            self.writer.write_all(&line).await.unwrap();
        }

        async fn send_raw(&mut self, bytes: &[u8]) {
            self.writer.write_all(bytes).await.unwrap();
        }
    }

    fn connect(max: usize) -> (RpcClient, FakeServer) {
        connect_buffered(max, 64 * 1024)
    }

    /// `buffer` is how much each direction of the pipe holds before a write
    /// blocks.
    fn connect_buffered(max: usize, buffer: usize) -> (RpcClient, FakeServer) {
        let (client_end, server_end) = duplex(buffer);
        let (client_read, client_write) = split(client_end);
        let (server_read, server_write) = split(server_end);
        (
            RpcClient::start(client_read, client_write, max),
            FakeServer {
                lines: BufReader::new(server_read).lines(),
                writer: server_write,
            },
        )
    }

    fn result(id: &Value, value: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "result": value})
    }

    #[tokio::test]
    async fn responses_are_routed_by_id_not_by_order() {
        let (client, mut server) = connect(MAX_MESSAGE_BYTES);
        let first = client.request("a", json!({}));
        let second = client.request("b", json!({}));

        let answer = async {
            let a = server.receive().await;
            let b = server.receive().await;
            // Answered in reverse.
            server.send(result(&b["id"], json!("for b"))).await;
            server.send(result(&a["id"], json!("for a"))).await;
            (a, b)
        };
        let (a, b, (sent_a, sent_b)) = tokio::join!(first, second, answer);

        assert_eq!(sent_a["method"], "a");
        assert_eq!(sent_b["method"], "b");
        assert_eq!(a.unwrap(), json!("for a"));
        assert_eq!(b.unwrap(), json!("for b"));
    }

    #[tokio::test]
    async fn a_dropped_request_is_cancelled_and_its_late_response_discarded() {
        let (client, mut server) = connect(MAX_MESSAGE_BYTES);

        // Start a request, let it reach the server, then abandon it.
        let mut abandoned = Box::pin(client.request("slow", json!({})));
        let sent = tokio::select! {
            _ = &mut abandoned => panic!("nothing has answered yet"),
            sent = server.receive() => sent,
        };
        drop(abandoned);

        let cancelled = server.receive().await;
        assert_eq!(cancelled["method"], "notifications/cancelled");
        assert_eq!(cancelled["params"]["requestId"], sent["id"]);
        assert!(cancelled.get("id").is_none(), "a notification has no id");

        // The server answers anyway, late. The next request must still get
        // its own answer, not the stale one.
        server.send(result(&sent["id"], json!("stale"))).await;
        let next = client.request("next", json!({}));
        let answer = async {
            let next = server.receive().await;
            server.send(result(&next["id"], json!("fresh"))).await;
        };
        let (next, ()) = tokio::join!(next, answer);
        assert_eq!(next.unwrap(), json!("fresh"));
    }

    #[tokio::test]
    async fn an_oversized_message_closes_the_connection() {
        let (client, mut server) = connect(64);
        let pending = client.request("a", json!({}));
        let flood = async {
            server.receive().await;
            server.send_raw(&[b'x'; 100]).await;
        };
        let (outcome, ()) = tokio::join!(pending, flood);

        match outcome {
            Err(RpcError::Closed(reason)) => assert!(reason.contains("larger than 64"), "{reason}"),
            other => panic!("expected a closed connection, got {other:?}"),
        }
        // And it stays closed.
        assert!(matches!(
            client.request("b", json!({})).await,
            Err(RpcError::Closed(_))
        ));
    }

    #[tokio::test]
    async fn a_message_just_under_the_cap_is_accepted() {
        let (client, mut server) = connect(128);
        let pending = client.request("a", json!({}));
        let reply = async {
            let request = server.receive().await;
            let mut message = result(&request["id"], json!(""));
            let padding = 120 - serde_json::to_vec(&message).unwrap().len();
            message["result"] = json!("p".repeat(padding));
            assert_eq!(serde_json::to_vec(&message).unwrap().len(), 120);
            server.send(message).await;
        };
        let (outcome, ()) = tokio::join!(pending, reply);
        assert!(outcome.is_ok(), "{outcome:?}");
    }

    #[tokio::test]
    async fn the_server_going_away_fails_the_waiting_call() {
        let (client, server) = connect(MAX_MESSAGE_BYTES);
        let pending = client.request("a", json!({}));
        drop(server);
        assert!(matches!(pending.await, Err(RpcError::Closed(_))));
    }

    #[tokio::test]
    async fn an_error_response_carries_the_servers_message() {
        let (client, mut server) = connect(MAX_MESSAGE_BYTES);
        let pending = client.request("a", json!({}));
        let reply = async {
            let request = server.receive().await;
            server
                .send(json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32602, "message": "bad params"}}))
                .await;
        };
        let (outcome, ()) = tokio::join!(pending, reply);
        assert_eq!(
            outcome,
            Err(RpcError::Remote {
                code: -32602,
                message: "bad params".into()
            })
        );
    }

    #[tokio::test]
    async fn requests_from_the_server_are_refused_and_pings_answered() {
        let (_client, mut server) = connect(MAX_MESSAGE_BYTES);
        server
            .send(json!({"jsonrpc": "2.0", "id": 7, "method": "sampling/createMessage", "params": {}}))
            .await;
        let refused = server.receive().await;
        assert_eq!(refused["id"], 7);
        assert_eq!(refused["error"]["code"], METHOD_NOT_FOUND);

        server
            .send(json!({"jsonrpc": "2.0", "id": "p", "method": "ping"}))
            .await;
        let pong = server.receive().await;
        assert_eq!(pong["id"], "p");
        assert_eq!(pong["result"], json!({}));
    }

    #[tokio::test]
    async fn a_request_with_an_unusable_id_is_ignored() {
        let (_client, mut server) = connect(MAX_MESSAGE_BYTES);
        for id in [
            json!("x".repeat(MAX_REQUEST_ID_BYTES + 1)),
            json!({"nested": 1}),
            json!([1]),
            json!(true),
        ] {
            server
                .send(json!({"jsonrpc": "2.0", "id": id, "method": "ping"}))
                .await;
        }
        let longest = "y".repeat(MAX_REQUEST_ID_BYTES);
        server
            .send(json!({"jsonrpc": "2.0", "id": longest, "method": "ping"}))
            .await;

        // The first answer is to the last request: none of the others got one.
        assert_eq!(server.receive().await["id"], json!(longest));
    }

    #[tokio::test]
    async fn a_flood_of_requests_the_server_does_not_read_the_answers_to_closes_it() {
        // A small pipe, so the answers back up almost at once.
        let (client, server) = connect_buffered(MAX_MESSAGE_BYTES, 256);
        // `lines` is kept but never read: the server is alive and not reading.
        let FakeServer { lines, mut writer } = server;
        let flood = tokio::spawn(async move {
            for id in 0..10 * MAX_QUEUED_MESSAGES {
                let line = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"ping\"}}\n");
                // Fails once the client has hung up, which is the point.
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        let outcome = tokio::time::timeout(Duration::from_secs(10), client.request("a", json!({})))
            .await
            .expect("the connection must close, not wait");
        match outcome {
            Err(RpcError::Closed(reason)) => {
                assert!(reason.contains("faster than it read"), "{reason}");
            }
            other => panic!("expected a closed connection, got {other:?}"),
        }
        flood.await.unwrap();
        drop(lines);
    }

    #[tokio::test]
    async fn a_request_dropped_mid_write_does_not_corrupt_the_next_message() {
        // The pipe holds 64 bytes; the request is far larger, so its write is
        // still under way when the request is dropped.
        let (client, mut server) = connect_buffered(MAX_MESSAGE_BYTES, 64);
        let blob = "x".repeat(64 * 1024);
        let mut big = Box::pin(client.request("big", json!({"blob": blob})));
        tokio::select! {
            biased;
            _ = &mut big => panic!("nothing has answered yet"),
            () = async {
                for _ in 0..16 {
                    tokio::task::yield_now().await;
                }
            } => {}
        }
        drop(big);

        let next = client.request("next", json!({}));
        let answer = async {
            // Each message arrives as its own well-formed line - `receive`
            // fails on anything that is not JSON.
            let first = server.receive().await;
            assert_eq!(first["method"], "big");
            assert_eq!(
                first["params"]["blob"].as_str().map(str::len),
                Some(blob.len())
            );
            let cancelled = server.receive().await;
            assert_eq!(cancelled["method"], "notifications/cancelled");
            assert_eq!(cancelled["params"]["requestId"], first["id"]);
            let next = server.receive().await;
            assert_eq!(next["method"], "next");
            server.send(result(&next["id"], json!("fresh"))).await;
        };
        let (next, ()) = tokio::join!(next, answer);
        assert_eq!(next.unwrap(), json!("fresh"));
    }

    #[tokio::test]
    async fn notifications_and_noise_are_ignored() {
        let (client, mut server) = connect(MAX_MESSAGE_BYTES);
        let pending = client.request("a", json!({}));
        let reply = async {
            let request = server.receive().await;
            server
                .send(json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}))
                .await;
            server.send_raw(b"not json\n\r\n").await;
            server.send(result(&request["id"], json!(1))).await;
        };
        let (outcome, ()) = tokio::join!(pending, reply);
        assert_eq!(outcome.unwrap(), json!(1));
    }

    #[tokio::test]
    async fn closing_closes_the_outgoing_stream() {
        let (client, mut server) = connect(MAX_MESSAGE_BYTES);
        client.close();
        let next = tokio::time::timeout(Duration::from_secs(10), server.lines.next_line())
            .await
            .expect("the stream must end, not hang")
            .unwrap();
        assert_eq!(next, None);
    }
}
