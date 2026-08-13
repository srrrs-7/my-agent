//! Test doubles shared by the integration suites.
//!
//! The mock LLM is raw TCP rather than an HTTP framework on purpose: it is
//! about sixty lines, it adds no dependency to a project whose whole premise is
//! keeping the supply chain small, and it lets a test assert on the *exact*
//! bytes the client put on the wire.

use std::fmt::Write as _;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// One canned HTTP reply.
#[derive(Debug, Clone)]
pub struct Response {
    status_line: String,
    body: String,
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
            body: body.into(),
        }
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
            "HTTP/1.1 {}\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{}",
            self.status_line,
            self.body.len(),
            self.body
        );
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

/// Reads headers, honours `content-length`, and returns the body.
async fn read_request_body(socket: &mut tokio::net::TcpStream) -> Option<String> {
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
    Some(String::from_utf8_lossy(&buffer[header_end..end]).to_string())
}
