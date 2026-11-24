//! The loopback authorization, token and inference service the `lab-provider` fixture talks
//! to. Every route validates what it receives and answers wrong requests with 4xx; nothing
//! here is merely recorded. Counters advance once a request has been read, before any
//! barrier or response, so a test can wait for "the request arrived" without races.
//!
//! Contract (shared with `tests/fixtures/lab-provider`):
//! - `POST /device {}` → `{userCode:"LAB-CODE", verificationUrl:"<url>/verify"}`
//! - `POST /token {answer:"approved"}` → `{access:"access-1", refresh:"refresh-1"}`
//! - `POST /refresh {refresh:"refresh-1"}` → `{access:"access-2", refresh:"refresh-2"}`
//!   (held by [`Server::hold_refresh`])
//! - `POST /inference` with `authorization: Bearer access-2`, `x-lab: yes`, body
//!   `model:"m"`, `stream:true`, `lab:true` → an OpenAI-completions SSE stream with the text
//!   `hello lab` and one `keep_name` tool call. The first chunk is flushed before
//!   [`Server::hold_sse`] can stop the rest.
//! - `POST /native/chat/completions`: the same stream without auth or rewrite checks, for a
//!   native Rust provider whose `baseUrl` is `<url>/native`.
//!
//! Any other path (e.g. a package registry pointed at `<url>/registry/`) answers 404 and
//! counts as [`Server::unexpected`]; [`Server::paths`] lists every path read.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axo_ai::testing::read_http_request_parts;
use axo_ai::{AssistantBlock, AssistantEvent, StopReason};
use serde_json::{Value, json};

/// How long a held route waits for its release before failing the connection.
const HOLD_LIMIT: Duration = Duration::from_secs(120);
/// The native route: a Rust provider's completions URL under `<url>/native`.
pub const NATIVE: &str = "/native/chat/completions";

pub const TEXT_HEAD: &str = "hello ";
pub const TEXT_TAIL: &str = "lab";
pub const TOOL: &str = "keep_name";

/// A normal completion: Rust parsed the lab text and the unchanged tool name.
pub fn assert_reply(events: &[AssistantEvent]) {
    let Some(AssistantEvent::Done { reason, message }) = events.last() else {
        panic!("not a completed reply: {:?}", events.last());
    };
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AssistantEvent::Error { .. })),
        "{events:?}"
    );
    assert_eq!(*reason, StopReason::ToolUse, "{message:?}");
    let text: String = message
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, format!("{TEXT_HEAD}{TEXT_TAIL}"));
    let tools: Vec<&str> = message
        .tool_calls()
        .map(|call| call.name.as_str())
        .collect();
    assert_eq!(tools, [TOOL], "{message:?}");
}

/// A validated request: its head (request line and headers) and JSON body.
#[derive(Debug, Clone)]
pub struct Request {
    pub head: String,
    pub body: Value,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.head, name)
    }
}

pub struct Server {
    pub url: String,
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    stopped: AtomicBool,
}

#[derive(Default)]
struct State {
    /// Every request path read, accepted or not, in arrival order.
    paths: Vec<String>,
    /// Accepted requests per path.
    accepted: HashMap<String, Vec<Request>>,
    rejected: Vec<String>,
    unexpected: usize,
    sse_started: usize,
    refresh_held: bool,
    sse_held: bool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.lock());
        self.changed.notify_all();
    }

    /// Blocks the connection thread while `held` says so; released or timed out either way.
    fn pass(&self, held: impl Fn(&State) -> bool) {
        let state = self.lock();
        let (_state, timeout) = self
            .changed
            .wait_timeout_while(state, HOLD_LIMIT, |state| {
                held(state) && !self.stopped.load(Ordering::SeqCst)
            })
            .unwrap_or_else(|e| e.into_inner());
        assert!(!timeout.timed_out(), "a held lab route was never released");
    }
}

impl Server {
    pub fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let shared = Arc::new(Shared::default());
        let accept = Arc::clone(&shared);
        let origin = url.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if accept.stopped.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let shared = Arc::clone(&accept);
                let origin = origin.clone();
                // One thread per connection: a held refresh must not stall an inference.
                std::thread::spawn(move || serve(&shared, &origin, stream));
            }
        });
        Self { url, shared }
    }

    /// Requests read on `path` so far, whether accepted or rejected.
    pub fn count(&self, path: &str) -> usize {
        self.shared
            .lock()
            .paths
            .iter()
            .filter(|p| *p == path)
            .count()
    }

    /// Every request path read, on any route, in arrival order.
    pub fn paths(&self) -> Vec<String> {
        self.shared.lock().paths.clone()
    }

    /// The accepted requests on `path`, in arrival order.
    pub fn requests(&self, path: &str) -> Vec<Request> {
        self.shared
            .lock()
            .accepted
            .get(path)
            .cloned()
            .unwrap_or_default()
    }

    /// Why each rejected contract request was refused (no secrets: fixed token values only).
    pub fn rejected(&self) -> Vec<String> {
        self.shared.lock().rejected.clone()
    }

    /// Requests to paths outside the contract, e.g. registry fetches.
    pub fn unexpected(&self) -> usize {
        self.shared.lock().unexpected
    }

    /// Every request read, on any path.
    pub fn total(&self) -> usize {
        self.shared.lock().paths.len()
    }

    /// Inference streams whose first chunk has been flushed to the client.
    pub fn sse_started(&self) -> usize {
        self.shared.lock().sse_started
    }

    /// `/refresh` requests are read and counted, then wait for `release_refresh`.
    pub fn hold_refresh(&self) {
        self.shared.update(|state| state.refresh_held = true);
    }

    pub fn release_refresh(&self) {
        self.shared.update(|state| state.refresh_held = false);
    }

    /// Inference streams flush their first chunk, count in `sse_started`, then wait for
    /// `release_sse` before sending the rest.
    pub fn hold_sse(&self) {
        self.shared.update(|state| state.sse_held = true);
    }

    pub fn release_sse(&self) {
        self.shared.update(|state| state.sse_held = false);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        self.shared.update(|state| {
            state.refresh_held = false;
            state.sse_held = false;
        });
        // Wake the accept loop so it sees `stopped`.
        let _ = TcpStream::connect(self.url.trim_start_matches("http://"));
    }
}

fn serve(shared: &Shared, origin: &str, mut stream: TcpStream) {
    let Some(head) = peek_head(&mut stream) else {
        return;
    };
    let path = head
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .unwrap_or_default()
        .to_owned();
    let post = head.starts_with("POST ");
    if header(&head, "content-length").is_none() {
        // Bodyless (e.g. a registry GET): consume the head and refuse it.
        let mut consumed = vec![0; head.len() + 4];
        if stream.read_exact(&mut consumed).is_ok() {
            shared.update(|state| {
                state.paths.push(path);
                state.unexpected += 1;
            });
            respond(
                &mut stream,
                404,
                &json!({"error": {"message": "not found"}}),
            );
        }
        return;
    }
    let (head, body) = read_http_request_parts(&mut stream);
    shared.update(|state| state.paths.push(path.clone()));
    let request = Request { head, body };
    let checked = match (post, path.as_str()) {
        (true, "/device") => check(request.body == json!({}), "device body"),
        (true, "/token") => check(request.body == json!({"answer": "approved"}), "token body"),
        (true, "/refresh") => check(
            request.body == json!({"refresh": "refresh-1"}),
            "refresh body",
        ),
        (true, "/inference") => check_inference(&request),
        (true, NATIVE) => Ok(()),
        _ => {
            shared.update(|state| state.unexpected += 1);
            respond(
                &mut stream,
                404,
                &json!({"error": {"message": "not found"}}),
            );
            return;
        }
    };
    if let Err(reason) = checked {
        let status = if reason.contains("authorization") {
            401
        } else {
            400
        };
        shared.update(|state| state.rejected.push(format!("{path}: {reason}")));
        respond(&mut stream, status, &json!({"error": {"message": reason}}));
        return;
    }
    shared.update(|state| {
        state
            .accepted
            .entry(path.clone())
            .or_default()
            .push(request)
    });
    match path.as_str() {
        "/device" => respond(
            &mut stream,
            200,
            &json!({"userCode": "LAB-CODE", "verificationUrl": format!("{origin}/verify")}),
        ),
        "/token" => respond(
            &mut stream,
            200,
            &json!({"access": "access-1", "refresh": "refresh-1"}),
        ),
        "/refresh" => {
            shared.pass(|state| state.refresh_held);
            respond(
                &mut stream,
                200,
                &json!({"access": "access-2", "refresh": "refresh-2"}),
            );
        }
        _ => stream_completion(shared, &mut stream),
    }
}

fn check(ok: bool, what: &str) -> Result<(), String> {
    ok.then_some(()).ok_or_else(|| format!("wrong {what}"))
}

fn check_inference(request: &Request) -> Result<(), String> {
    check(
        request.header("authorization") == Some("Bearer access-2"),
        "authorization",
    )?;
    check(request.header("x-lab") == Some("yes"), "x-lab header")?;
    let body = &request.body;
    check(body["model"] == "m", "model")?;
    check(body["stream"] == true, "stream flag")?;
    check(body["lab"] == true, "lab body field")?;
    check(
        body["messages"].as_array().is_some_and(|m| !m.is_empty()),
        "messages",
    )
}

/// Waits until a complete request head is buffered, without consuming it. `None` when the
/// client closed or stalled first.
fn peek_head(stream: &mut TcpStream) -> Option<String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut buffer = vec![0; 64 * 1024];
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = 0;
    loop {
        let count = stream.peek(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        if let Some(end) = buffer[..count].windows(4).position(|w| w == b"\r\n\r\n") {
            return String::from_utf8(buffer[..end].to_vec()).ok();
        }
        if count == buffer.len() || Instant::now() > deadline {
            return None;
        }
        if count == seen {
            std::thread::sleep(Duration::from_millis(5));
        }
        seen = count;
    }
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}

fn respond(stream: &mut TcpStream, status: u16, body: &Value) {
    let body = body.to_string();
    let response = format!(
        "HTTP/1.1 {status} Lab\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // The client may already be gone (e.g. a killed runner); that is its outcome to report.
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
}

fn chunk(delta: Value, finish: Option<&str>) -> String {
    let chunk = json!({
        "id": "lab-1",
        "object": "chat.completion.chunk",
        "model": "m",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    format!("data: {chunk}\n\n")
}

/// A close-delimited SSE body: first chunk, flush, the `hold_sse` barrier, then the rest.
fn stream_completion(shared: &Shared, stream: &mut TcpStream) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    let first = chunk(json!({"role": "assistant", "content": TEXT_HEAD}), None);
    if stream
        .write_all(format!("{head}{first}").as_bytes())
        .and_then(|()| stream.flush())
        .is_err()
    {
        return;
    }
    shared.update(|state| state.sse_started += 1);
    shared.pass(|state| state.sse_held);
    let rest = [
        chunk(json!({"content": TEXT_TAIL}), None),
        chunk(
            json!({"tool_calls": [{
                "index": 0,
                "id": "call_lab",
                "type": "function",
                "function": {"name": TOOL, "arguments": "{\"keep\":true}"},
            }]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
        "data: [DONE]\n\n".to_owned(),
    ]
    .concat();
    let _ = stream.write_all(rest.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
}
