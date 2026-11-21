//! The Rust end of the runner protocol (`runner.ts` is the other end): one authenticated
//! Unix-socket connection bound to one generation.
//!
//! Frames are a 4-byte big-endian length plus UTF-8 JSON, at most [`MAX_FRAME`] bytes both
//! ways. One reader demultiplexes without awaiting user code; one writer serializes frames,
//! letting pings and small reader-originated replies jump ahead of the byte-budgeted data
//! queue. Rust calls use IDs `h:<n>` (shared with pings), runner callbacks `r:<n>`. A call's
//! future owns its pending entry: dropping it queues a cancel frame behind the call frame,
//! and a login drives its borrowed UI interactions inside its own future. Any protocol
//! violation, EOF, write failure or missed heartbeat closes the peer and fails every pending
//! call with [`CallError::Unavailable`].

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axo_ai::{DeviceCode, LoginInteraction, LoginOption, LoginPrompt, LoginSelect};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval_at, timeout};
use tokio_util::sync::CancellationToken;

pub const PROTOCOL: u64 = 1;
/// Largest frame body either side may send or accept.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;
/// Bytes of data frames queued ahead of the socket before senders wait for room.
const QUEUE_LIMIT: usize = 2 * MAX_FRAME;
/// Small frames (pings, reader-originated replies) waiting ahead of the data queue.
const CONTROL_SLOTS: usize = 64;
/// Cancel frames queued behind their calls; none left means the writer is stuck.
const CANCEL_SLOTS: usize = 256;
/// Live runner callbacks across all logins on one connection.
const MAX_CALLBACKS: usize = 256;
pub const HANDSHAKE: Duration = Duration::from_secs(15);
pub const PING_EVERY: Duration = Duration::from_secs(2);
pub const PONG_DEADLINE: Duration = Duration::from_secs(10);

const HOST_METHODS: [&str; 4] = ["activate", "login", "refresh", "rewrite"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    /// The connection is closed or closed while the call was pending.
    #[error("extension runner is unavailable")]
    Unavailable,
    /// The runner answered with an error; the message is safe to show.
    #[error("{0}")]
    Method(String),
    /// Rejected locally (method/argument shape or frame size); nothing was sent.
    #[error("extension request is invalid or exceeds the 64 MiB frame bound")]
    Invalid,
}

/// Encodes one frame: 4-byte big-endian length plus JSON. `None` if it exceeds the bound.
pub fn encode(value: &Value) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 4];
    serde_json::to_writer(&mut buf, value).ok()?;
    let len = buf.len() - 4;
    if len > MAX_FRAME {
        return None;
    }
    buf[..4].copy_from_slice(&(len as u32).to_be_bytes());
    Some(buf)
}

/// Reads one frame, checking the length before allocating. `Ok(None)` is a clean EOF at a
/// frame boundary.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Value>, &'static str> {
    let mut len = [0u8; 4];
    match r.read(&mut len[..1]).await {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(_) => return Err("read failed"),
    }
    r.read_exact(&mut len[1..])
        .await
        .map_err(|_| "truncated frame")?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err("frame exceeds 64 MiB");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)
        .await
        .map_err(|_| "truncated frame")?;
    match serde_json::from_slice(&body) {
        Ok(value @ Value::Object(_)) => Ok(Some(value)),
        Ok(_) => Err("frame is not an object"),
        Err(_) => Err("frame is not UTF-8 JSON"),
    }
}

pub struct Peer {
    inner: Arc<Inner>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

struct Inner {
    /// The generation as the wire's opaque decimal string.
    tag: String,
    closed: CancellationToken,
    state: Mutex<State>,
    control: mpsc::Sender<Vec<u8>>,
    data: mpsc::UnboundedSender<(Vec<u8>, OwnedSemaphorePermit)>,
    budget: Arc<Semaphore>,
    cancels: Arc<Semaphore>,
}

struct State {
    /// Next `h:` counter; every lower number was issued (live, finished or a ping).
    next: u64,
    calls: HashMap<u64, Pending>,
    pings: BTreeSet<u64>,
    last_pong: Instant,
    /// Live runner callbacks by `r:` ID.
    callbacks: HashMap<String, Live>,
}

struct Pending {
    ext: String,
    method: &'static str,
    reply: oneshot::Sender<Result<Value, CallError>>,
    /// Present only for a login: routes its callbacks to the login's future.
    callbacks: Option<mpsc::UnboundedSender<Callback>>,
}

struct Live {
    parent: u64,
    cancel: CancellationToken,
}

struct Callback {
    id: String,
    request: Interaction,
    cancel: CancellationToken,
}

enum Interaction {
    OpenUrl {
        url: String,
        instructions: Option<String>,
    },
    DeviceCode(DeviceCode),
    Prompt(LoginPrompt),
    Select(LoginSelect),
    Progress(String),
}

impl Peer {
    /// Authenticates the runner's hello on `stream` and starts the reader, writer and
    /// heartbeat for `generation`.
    pub async fn connect(
        stream: UnixStream,
        generation: u64,
        nonce: &str,
    ) -> Result<Arc<Peer>, String> {
        let tag = generation.to_string();
        let (read, write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let hello = match timeout(HANDSHAKE, read_frame(&mut reader)).await {
            Err(_) => return Err("extension runner handshake timed out".into()),
            Ok(Ok(Some(hello))) => hello,
            Ok(Ok(None)) => {
                return Err("extension runner closed the connection before its hello".into());
            }
            Ok(Err(reason)) => return Err(format!("extension runner handshake failed: {reason}")),
        };
        let valid = hello.get("type").and_then(Value::as_str) == Some("hello")
            && hello.get("protocol").and_then(Value::as_u64) == Some(PROTOCOL)
            && hello.get("generation").and_then(Value::as_str) == Some(&tag)
            && hello
                .get("nonce")
                .and_then(Value::as_str)
                .is_some_and(|sent| same(sent, nonce));
        if !valid {
            return Err("extension runner sent an invalid hello".into());
        }

        let (control, control_rx) = mpsc::channel(CONTROL_SLOTS);
        let (data, data_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            tag,
            closed: CancellationToken::new(),
            state: Mutex::new(State {
                next: 0,
                calls: HashMap::new(),
                pings: BTreeSet::new(),
                last_pong: Instant::now(),
                callbacks: HashMap::new(),
            }),
            control,
            data,
            budget: Arc::new(Semaphore::new(QUEUE_LIMIT)),
            cancels: Arc::new(Semaphore::new(CANCEL_SLOTS)),
        });
        let tasks = vec![
            tokio::spawn(read_loop(inner.clone(), reader)),
            tokio::spawn(write_loop(inner.clone(), write, control_rx, data_rx)),
            tokio::spawn(heartbeat(inner.clone())),
        ];
        Ok(Arc::new(Peer {
            inner,
            tasks: Mutex::new(tasks),
        }))
    }

    /// Calls a runner method for `ext`. `login` needs `ui` and drives its interactions
    /// until the runner's terminal reply; other methods take none. Dropping the future
    /// withdraws the call; deadlines belong to the caller.
    pub async fn call(
        &self,
        ext: &str,
        method: &str,
        args: Value,
        ui: Option<&dyn LoginInteraction>,
    ) -> Result<Value, CallError> {
        let Some(method) = HOST_METHODS.into_iter().find(|m| *m == method) else {
            return Err(CallError::Invalid);
        };
        if (method == "login") != ui.is_some() {
            return Err(CallError::Invalid);
        }
        let inner = &*self.inner;
        let (reply, mut result) = oneshot::channel();
        let (callbacks, mut routed) = mpsc::unbounded_channel();
        let id = {
            let mut state = inner.lock();
            if inner.closed.is_cancelled() {
                return Err(CallError::Unavailable);
            }
            let id = state.next;
            state.next += 1;
            let callbacks = ui.is_some().then_some(callbacks);
            state.calls.insert(
                id,
                Pending {
                    ext: ext.to_owned(),
                    method,
                    reply,
                    callbacks,
                },
            );
            id
        };
        let mut outstanding = Outstanding {
            inner,
            id,
            sent: false,
        };
        let mut call = Map::new();
        call.insert("type".into(), "call".into());
        call.insert("id".into(), format!("h:{id}").into());
        call.insert("ext".into(), ext.into());
        call.insert("method".into(), method.into());
        call.insert("args".into(), args);
        let frame = inner.frame(Value::Object(call)).ok_or(CallError::Invalid)?;
        inner.enqueue(frame).await?;
        outstanding.sent = true;

        let mut interactions = FuturesUnordered::new();
        loop {
            tokio::select! {
                biased;
                done = &mut result => return done.unwrap_or(Err(CallError::Unavailable)),
                Some(callback) = routed.recv() => {
                    if let Some(ui) = ui {
                        interactions.push(inner.interact(ui, callback));
                    }
                }
                Some(()) = interactions.next(), if !interactions.is_empty() => {}
            }
        }
    }

    /// Resolves once the connection is closed.
    pub async fn closed(&self) {
        self.inner.closed.cancelled().await;
    }

    /// Closes the connection and fails every pending call.
    pub fn close(&self) {
        self.inner.close();
    }

    /// Waits for the reader, writer and heartbeat tasks to end.
    pub async fn join(&self) {
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(PoisonError::into_inner));
        for task in tasks {
            let _ = task.await;
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.inner.close();
    }
}

/// A sent or registering call; dropping it before the reply withdraws it.
struct Outstanding<'a> {
    inner: &'a Inner,
    id: u64,
    sent: bool,
}

impl Drop for Outstanding<'_> {
    fn drop(&mut self) {
        let live = self.inner.lock().finish(self.id).is_some();
        if live && self.sent {
            self.inner.cancel(self.id);
        }
    }
}

impl State {
    /// Removes a call and drops its live callbacks.
    fn finish(&mut self, id: u64) -> Option<Pending> {
        let pending = self.calls.remove(&id)?;
        self.callbacks.retain(|_, live| {
            let keep = live.parent != id;
            if !keep {
                live.cancel.cancel();
            }
            keep
        });
        Some(pending)
    }

    /// The number of an issued `h:` ID.
    fn issued(&self, id: &str) -> Option<u64> {
        let n = id
            .strip_prefix("h:")
            .filter(|n| is_counter(n))?
            .parse()
            .ok()?;
        (n < self.next).then_some(n)
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Closes once: fails every pending call and drops every live callback.
    fn close(&self) {
        let (calls, callbacks) = {
            let mut state = self.lock();
            self.closed.cancel();
            (
                std::mem::take(&mut state.calls),
                std::mem::take(&mut state.callbacks),
            )
        };
        for live in callbacks.into_values() {
            live.cancel.cancel();
        }
        drop(calls);
    }

    fn frame(&self, mut value: Value) -> Option<Vec<u8>> {
        value
            .as_object_mut()?
            .insert("generation".into(), Value::String(self.tag.clone()));
        encode(&value)
    }

    /// Queues a data frame once the byte budget has room.
    async fn enqueue(&self, frame: Vec<u8>) -> Result<(), CallError> {
        let permit = tokio::select! {
            _ = self.closed.cancelled() => return Err(CallError::Unavailable),
            permit = self.budget.clone().acquire_many_owned(frame.len() as u32) => {
                permit.map_err(|_| CallError::Unavailable)?
            }
        };
        self.data
            .send((frame, permit))
            .map_err(|_| CallError::Unavailable)
    }

    /// Queues a cancel behind the call it withdraws; tears down rather than block.
    fn cancel(&self, id: u64) {
        let frame = self.frame(json!({"type": "cancel", "id": format!("h:{id}")}));
        let queued = frame.zip(self.cancels.clone().try_acquire_owned().ok());
        if !queued.is_some_and(|queued| self.data.send(queued).is_ok()) {
            self.close();
        }
    }

    /// Sends a small frame ahead of the data queue without waiting.
    fn control(&self, value: Value) {
        let sent = self
            .frame(value)
            .is_some_and(|frame| self.control.try_send(frame).is_ok());
        if !sent {
            self.close();
        }
    }

    fn reply_error(&self, id: &str, code: &str, message: &str) {
        self.control(json!({"type": "error", "id": id, "code": code, "message": message}));
    }

    fn dispatch(&self, frame: Value) -> Result<(), &'static str> {
        let Value::Object(mut frame) = frame else {
            return Err("frame is not an object");
        };
        if frame.get("generation").and_then(Value::as_str) != Some(&self.tag) {
            return Err("wrong generation");
        }
        let Some(Value::String(id)) = frame.remove("id") else {
            return Err("frame without string id");
        };
        match frame.get("type").and_then(Value::as_str) {
            Some("pong") => self.pong(&id),
            Some("ok") => self.settle(&id, Ok(frame.remove("value").unwrap_or(Value::Null))),
            Some("error") => {
                let code = frame
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                self.settle(&id, Err(code))
            }
            Some("call") => self.callback(id, frame),
            Some("cancel") => {
                if !is_runner_id(&id) {
                    return Err("runner canceled a host call");
                }
                if let Some(live) = self.lock().callbacks.remove(&id) {
                    live.cancel.cancel();
                }
                Ok(())
            }
            _ => Err("unknown frame type"),
        }
    }

    fn pong(&self, id: &str) -> Result<(), &'static str> {
        let mut state = self.lock();
        let n = state.issued(id).ok_or("pong for an unissued id")?;
        // The runner may coalesce pongs: one answers every older ping too.
        if state.pings.remove(&n) {
            state.pings = state.pings.split_off(&n);
            state.last_pong = Instant::now();
        }
        Ok(())
    }

    /// Delivers a terminal reply; late replies to finished calls are dropped.
    fn settle(&self, id: &str, result: Result<Value, String>) -> Result<(), &'static str> {
        let pending = {
            let mut state = self.lock();
            let n = state.issued(id).ok_or("reply to an unissued id")?;
            state.finish(n)
        };
        if let Some(pending) = pending {
            let result = result
                .map_err(|code| CallError::Method(label(&pending.ext, pending.method, &code)));
            let _ = pending.reply.send(result);
        }
        Ok(())
    }

    fn callback(&self, id: String, mut frame: Map<String, Value>) -> Result<(), &'static str> {
        if !is_runner_id(&id) {
            return Err("call id is not a runner id");
        }
        let args = frame.remove("args");
        let method = frame
            .get("method")
            .and_then(Value::as_str)
            .ok_or("malformed call")?;
        if HOST_METHODS.contains(&method) {
            return Err("runner called a host method");
        }
        let ext = frame
            .get("ext")
            .and_then(Value::as_str)
            .ok_or("malformed call")?;
        let parent = frame
            .get("parent")
            .and_then(Value::as_str)
            .ok_or("unparented callback")?;
        let request = Interaction::parse(method, args);
        let mut state = self.lock();
        let parent = state
            .issued(parent)
            .ok_or("callback parent was never issued")?;
        if state.callbacks.contains_key(&id) {
            return Err("duplicate live callback id");
        }
        let Some(pending) = state.calls.get(&parent) else {
            return Ok(()); // the login already ended; the runner drops its own callback
        };
        if pending.ext != ext {
            return Err("callback for another extension's login");
        }
        let Some(route) = pending.callbacks.clone() else {
            return Err("callback parent is not a login");
        };
        if state.callbacks.len() >= MAX_CALLBACKS {
            return Err("too many live callbacks");
        }
        let request = match request {
            Some(Ok(request)) => request,
            failure => {
                drop(state);
                match failure {
                    None => self.reply_error(&id, "unsupported", "unknown host method"),
                    _ => self.reply_error(&id, "invalid", "invalid arguments"),
                }
                return Ok(());
            }
        };
        let cancel = CancellationToken::new();
        state.callbacks.insert(
            id.clone(),
            Live {
                parent,
                cancel: cancel.clone(),
            },
        );
        let _ = route.send(Callback {
            id,
            request,
            cancel,
        });
        Ok(())
    }

    /// Runs one runner callback against the borrowed host UI and answers it, unless the
    /// runner or the login withdrew it first.
    async fn interact(&self, ui: &dyn LoginInteraction, callback: Callback) {
        let Callback {
            id,
            request,
            cancel,
        } = callback;
        let answer = tokio::select! {
            _ = cancel.cancelled() => return,
            answer = request.run(ui) => answer,
        };
        if self.lock().callbacks.remove(&id).is_none() {
            return;
        }
        let reply = match answer {
            Ok(value) => json!({"type": "ok", "id": &id, "value": value}),
            Err(code) => {
                json!({"type": "error", "id": &id, "code": code, "message": format!("host {code}")})
            }
        };
        let frame = self.frame(reply).or_else(|| {
            self.frame(json!({"type": "error", "id": id, "code": "too_large", "message": "host reply exceeds 64 MiB"}))
        });
        if let Some(frame) = frame {
            let _ = self.enqueue(frame).await;
        }
    }
}

impl Interaction {
    /// `None` for an unknown method, `Some(Err)` for malformed arguments.
    fn parse(method: &str, args: Option<Value>) -> Option<Result<Interaction, ()>> {
        let args = match args {
            Some(Value::Object(args)) => args,
            _ => return is_runner_method(method).then_some(Err(())),
        };
        let text = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(())
        };
        let optional = |key: &str| match args.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(_) => Err(()),
        };
        let parsed = (|| -> Result<Option<Interaction>, ()> {
            Ok(Some(match method {
                "openUrl" => Interaction::OpenUrl {
                    url: text("url")?,
                    instructions: optional("instructions")?,
                },
                "deviceCode" => Interaction::DeviceCode(DeviceCode {
                    user_code: text("userCode")?,
                    verification_url: text("verificationUrl")?,
                    expires_in_seconds: match args.get("expiresInSeconds") {
                        None | Some(Value::Null) => None,
                        Some(value) => Some(value.as_u64().ok_or(())?),
                    },
                }),
                "prompt" => Interaction::Prompt(LoginPrompt {
                    message: text("message")?,
                    placeholder: optional("placeholder")?,
                    secret: args.get("secret").and_then(Value::as_bool).ok_or(())?,
                }),
                "select" => {
                    let options = args.get("options").and_then(Value::as_array).ok_or(())?;
                    let options = options
                        .iter()
                        .map(|option| {
                            let field = |key: &str| {
                                option
                                    .get(key)
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                                    .ok_or(())
                            };
                            Ok(LoginOption {
                                id: field("id")?,
                                label: field("label")?,
                            })
                        })
                        .collect::<Result<Vec<_>, ()>>()?;
                    if options.is_empty() {
                        return Err(());
                    }
                    Interaction::Select(LoginSelect {
                        message: text("message")?,
                        options,
                    })
                }
                "progress" => Interaction::Progress(text("message")?),
                _ => return Ok(None),
            }))
        })();
        match parsed {
            Ok(Some(request)) => Some(Ok(request)),
            Ok(None) => None,
            Err(()) => Some(Err(())),
        }
    }

    /// Notices are acknowledged once shown; prompt/select resolve with a validated answer.
    /// Host UI errors reach the runner only as the fixed code `interaction`.
    async fn run(self, ui: &dyn LoginInteraction) -> Result<Value, &'static str> {
        let failed = |_: axo_ai::Error| "interaction";
        match self {
            Interaction::OpenUrl { url, instructions } => {
                ui.open_url(&url, instructions.as_deref()).await
            }
            Interaction::DeviceCode(code) => ui.device_code(&code).await,
            Interaction::Progress(message) => ui.progress(&message).await,
            Interaction::Prompt(prompt) => {
                return ui.prompt(&prompt).await.map(Value::String).map_err(failed);
            }
            Interaction::Select(select) => {
                let choice = ui.select(&select).await.map_err(failed)?;
                if !select.options.iter().any(|option| option.id == choice) {
                    return Err("invalid");
                }
                return Ok(Value::String(choice));
            }
        }
        Ok(Value::Null)
    }
}

async fn read_loop(inner: Arc<Inner>, mut reader: BufReader<OwnedReadHalf>) {
    loop {
        let frame = tokio::select! {
            _ = inner.closed.cancelled() => return,
            frame = read_frame(&mut reader) => frame,
        };
        // Any violation's reason is for the reader alone; the lifecycle sees a closed peer.
        let Ok(Some(frame)) = frame else { break };
        if inner.dispatch(frame).is_err() {
            break;
        }
    }
    inner.close();
}

async fn write_loop(
    inner: Arc<Inner>,
    mut writer: OwnedWriteHalf,
    mut control: mpsc::Receiver<Vec<u8>>,
    mut data: mpsc::UnboundedReceiver<(Vec<u8>, OwnedSemaphorePermit)>,
) {
    loop {
        // The permit is held until the frame is written, so queued bytes stay bounded.
        let (frame, _permit) = tokio::select! {
            biased;
            _ = inner.closed.cancelled() => return,
            Some(frame) = control.recv() => (frame, None),
            Some((frame, permit)) = data.recv() => (frame, Some(permit)),
        };
        let written = tokio::select! {
            _ = inner.closed.cancelled() => return,
            written = writer.write_all(&frame) => written,
        };
        if written.is_err() {
            return inner.close();
        }
    }
}

/// Pings through the runner's event loop; a blocked JS thread stops answering.
async fn heartbeat(inner: Arc<Inner>) {
    let mut ticks = interval_at(Instant::now() + PING_EVERY, PING_EVERY);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = inner.closed.cancelled() => return,
            _ = ticks.tick() => {}
        }
        let id = {
            let mut state = inner.lock();
            if state.last_pong.elapsed() >= PONG_DEADLINE {
                drop(state);
                return inner.close();
            }
            let id = state.next;
            state.next += 1;
            state.pings.insert(id);
            id
        };
        inner.control(json!({"type": "ping", "id": format!("h:{id}")}));
    }
}

fn is_counter(n: &str) -> bool {
    !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
}

fn is_runner_id(id: &str) -> bool {
    id.strip_prefix("r:").is_some_and(is_counter)
}

fn is_runner_method(method: &str) -> bool {
    matches!(
        method,
        "openUrl" | "deviceCode" | "prompt" | "select" | "progress"
    )
}

/// A fixed, host-owned label for a runner error code. The runner's message is never shown:
/// author text (and anything secret in it) may follow any prefix.
fn label(ext: &str, method: &str, code: &str) -> String {
    match code {
        "activation" => format!("extension {ext}: activation was rejected"),
        "invalid" => format!("extension {ext}: {method} had an invalid argument or result"),
        "unsupported" => format!("extension {ext}: {method} is not supported"),
        "too_large" => format!("extension {ext}: {method} reply exceeds 64 MiB"),
        _ => format!("extension {ext}: {method} failed"),
    }
}

/// Compares nonces without an early exit on the first differing byte.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// The real Rust peer against raw frames written by the test over `UnixStream::pair`.
#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use axo_ai::Error;

    use super::*;

    const NONCE: &str = "4e6f6e6365";
    const GENERATION: u64 = 7;

    /// The runner's end of the connection.
    struct Raw {
        reader: OwnedReadHalf,
        writer: OwnedWriteHalf,
    }

    impl Raw {
        /// Sends `frame` with this connection's generation.
        async fn send(&mut self, mut frame: Value) {
            frame["generation"] = GENERATION.to_string().into();
            self.writer.write_all(&bytes(&frame)).await.unwrap();
        }

        /// The next non-ping frame from Rust, answering pings on the way; `None` at EOF.
        async fn next(&mut self) -> Option<Value> {
            loop {
                let frame = next_frame(&mut self.reader).await?;
                assert_eq!(frame["generation"], GENERATION.to_string(), "{frame}");
                if frame["type"] != "ping" {
                    return Some(frame);
                }
                let id = frame["id"].clone();
                self.send(json!({"type": "pong", "id": id})).await;
            }
        }

        async fn expect_eof(&mut self) {
            assert_eq!(self.next().await, None, "expected the peer to close");
        }
    }

    fn bytes(frame: &Value) -> Vec<u8> {
        let body = serde_json::to_vec(frame).unwrap();
        let mut out = (body.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&body);
        out
    }

    /// The next frame, or `None` at EOF (or a broken connection). Lenient on purpose: the
    /// production `read_frame` is the code under test.
    async fn next_frame(reader: &mut (impl AsyncRead + Unpin)) -> Option<Value> {
        let mut len = [0u8; 4];
        reader.read_exact(&mut len).await.ok()?;
        let len = u32::from_be_bytes(len) as usize;
        assert!(len <= MAX_FRAME, "peer sent an oversized frame");
        let mut body = vec![0; len];
        reader.read_exact(&mut body).await.ok()?;
        Some(serde_json::from_slice(&body).expect("peer sent invalid JSON"))
    }

    /// Polls `check` on the wall clock until it holds.
    async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
        let start = std::time::Instant::now();
        while !check() {
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn hello() -> Value {
        json!({"type": "hello", "protocol": 1, "nonce": NONCE, "generation": GENERATION.to_string()})
    }

    /// A connected peer and the runner end.
    async fn pair() -> (Arc<Peer>, Raw) {
        let (host, runner) = UnixStream::pair().unwrap();
        let (reader, writer) = runner.into_split();
        let mut raw = Raw { reader, writer };
        let (peer, ()) = tokio::join!(Peer::connect(host, GENERATION, NONCE), async {
            raw.writer.write_all(&bytes(&hello())).await.unwrap();
        });
        (peer.unwrap(), raw)
    }

    async fn closes(peer: &Peer) {
        tokio::time::timeout(Duration::from_secs(5), peer.closed())
            .await
            .expect("peer did not close");
    }

    /// Records display callbacks; prompts named `never` wait forever and record when dropped.
    #[derive(Default)]
    struct Ui {
        events: Mutex<Vec<String>>,
        dropped: AtomicUsize,
    }

    struct DropMark<'a>(&'a AtomicUsize);

    impl Drop for DropMark<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Ui {
        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }

        fn push(&self, event: String) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[async_trait]
    impl LoginInteraction for Ui {
        async fn open_url(&self, url: &str, instructions: Option<&str>) {
            self.push(format!("url {url} {instructions:?}"));
        }

        async fn device_code(&self, code: &DeviceCode) {
            self.push(format!("code {} {}", code.user_code, code.verification_url));
        }

        async fn prompt(&self, prompt: &LoginPrompt) -> Result<String, Error> {
            self.push(format!("prompt {}", prompt.message));
            match prompt.message.as_str() {
                "never" => {
                    let _mark = DropMark(&self.dropped);
                    std::future::pending().await
                }
                "refuse" => Err(Error::Auth("refused".into())),
                message => Ok(format!("answer-{message}")),
            }
        }

        async fn select(&self, select: &LoginSelect) -> Result<String, Error> {
            self.push(format!("select {}", select.message));
            Ok(if select.message == "bogus" {
                "not-an-option".into()
            } else {
                select.options[0].id.clone()
            })
        }

        async fn progress(&self, message: &str) {
            self.push(format!("progress {message}"));
        }
    }

    /// The `(id, ext, method)` of a call frame.
    fn called(frame: &Value) -> (String, String, String) {
        assert_eq!(frame["type"], "call", "{frame}");
        assert!(frame.get("parent").is_none() || frame["parent"].is_null());
        (
            frame["id"].as_str().unwrap().into(),
            frame["ext"].as_str().unwrap().into(),
            frame["method"].as_str().unwrap().into(),
        )
    }

    fn ok(id: &str, value: Value) -> Value {
        json!({"type": "ok", "id": id, "value": value})
    }

    fn credential(access: &str) -> Value {
        json!({"type": "oauth", "access": access, "refresh": "r", "expires": 1})
    }

    #[tokio::test]
    async fn hello_is_validated_even_when_fragmented() {
        // One byte per write: the reader must reassemble the hello.
        let (host, runner) = UnixStream::pair().unwrap();
        let (reader, mut writer) = runner.into_split();
        let (peer, ()) = tokio::join!(Peer::connect(host, GENERATION, NONCE), async {
            for byte in bytes(&hello()) {
                writer.write_all(&[byte]).await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        let peer = peer.unwrap();
        let mut raw = Raw { reader, writer };
        // And it works: one call round-trips, its reply split across writes too.
        let (result, ()) = tokio::join!(
            peer.call(
                "a",
                "refresh",
                json!({"provider": "p", "credential": credential("old")}),
                None
            ),
            async {
                let frame = raw.next().await.unwrap();
                let (id, ext, method) = called(&frame);
                assert_eq!((ext.as_str(), method.as_str()), ("a", "refresh"));
                assert!(id.starts_with("h:"), "{id}");
                assert_eq!(frame["args"]["credential"]["access"], "old");
                let reply = bytes(
                    &json!({"type": "ok", "id": id, "value": credential("new"), "generation": "7"}),
                );
                let (head, tail) = reply.split_at(3);
                raw.writer.write_all(head).await.unwrap();
                tokio::task::yield_now().await;
                raw.writer.write_all(tail).await.unwrap();
            }
        );
        assert_eq!(result.unwrap()["access"], "new");
        peer.close();
        peer.join().await;

        // Wrong nonce, wrong protocol, missing generation or a non-hello first frame fail.
        let bad = [
            json!({"type": "hello", "protocol": 1, "nonce": "wrong", "generation": "7"}),
            json!({"type": "hello", "protocol": 2, "nonce": NONCE, "generation": "7"}),
            json!({"type": "hello", "protocol": 1, "nonce": NONCE, "generation": "8"}),
            json!({"type": "hello", "protocol": 1, "nonce": NONCE}),
            json!({"type": "pong", "id": "h:0", "generation": "7"}),
        ];
        for frame in bad {
            let (host, mut runner) = UnixStream::pair().unwrap();
            let (peer, ()) = tokio::join!(Peer::connect(host, GENERATION, NONCE), async {
                runner.write_all(&bytes(&frame)).await.unwrap();
            });
            assert!(peer.is_err(), "accepted {frame}");
        }
    }

    #[tokio::test]
    async fn coalesced_replies_resolve_their_own_calls() {
        let (peer, mut raw) = pair().await;
        let rewrite_args = json!({
            "provider": "p",
            "model": {"id": "m"},
            "request": {"url": "http://x/", "headers": [], "body": {}},
        });
        let (refresh, rewrite, ()) = tokio::join!(
            peer.call(
                "a",
                "refresh",
                json!({"provider": "p", "credential": credential("old")}),
                None
            ),
            peer.call("b", "rewrite", rewrite_args, None),
            async {
                let first = called(&raw.next().await.unwrap());
                let second = called(&raw.next().await.unwrap());
                assert_ne!(first.0, second.0, "call ids must be unique");
                let (refresh, rewrite) = if first.2 == "refresh" {
                    (first, second)
                } else {
                    (second, first)
                };
                assert_eq!(refresh.1, "a");
                assert_eq!(rewrite.1, "b");
                // Both replies in one write, in the opposite order of the calls.
                let mut both = bytes(
                    &json!({"type": "error", "id": rewrite.0, "code": "failed", "message": "extension b provider p rewrite failed", "generation": "7"}),
                );
                both.extend(bytes(&json!({"type": "ok", "id": refresh.0, "value": credential("new"), "generation": "7"})));
                raw.writer.write_all(&both).await.unwrap();
            }
        );
        assert_eq!(refresh.unwrap()["access"], "new");
        assert_eq!(
            rewrite,
            Err(CallError::Method("extension b: rewrite failed".into()))
        );

        // Runner text never reaches the host: each error code maps to a fixed label.
        for (code, label) in [
            ("failed", "extension a: refresh failed"),
            (
                "invalid",
                "extension a: refresh had an invalid argument or result",
            ),
            ("unsupported", "extension a: refresh is not supported"),
            ("too_large", "extension a: refresh reply exceeds 64 MiB"),
            ("activation", "extension a: activation was rejected"),
        ] {
            let (result, ()) = tokio::join!(
                peer.call(
                    "a",
                    "refresh",
                    json!({"provider": "p", "credential": credential("old")}),
                    None
                ),
                async {
                    let (id, ..) = called(&raw.next().await.unwrap());
                    raw.send(json!({"type": "error", "id": id, "code": code, "message": "token sk-secret leaked\nstack"})).await;
                }
            );
            assert_eq!(result, Err(CallError::Method(label.into())), "{code}");
        }
    }

    #[tokio::test]
    async fn login_interleaves_callbacks_and_cancels_one_prompt() {
        let (peer, mut raw) = pair().await;
        let ui = Ui::default();
        let (login, refresh, ()) = tokio::join!(
            peer.call("a", "login", json!({"provider": "p"}), Some(&ui)),
            peer.call(
                "a",
                "refresh",
                json!({"provider": "p", "credential": credential("old")}),
                None
            ),
            async {
                let mut login = None;
                let mut refresh = None;
                for _ in 0..2 {
                    let frame = called(&raw.next().await.unwrap());
                    match frame.2.as_str() {
                        "login" => login = Some(frame.0),
                        _ => refresh = Some(frame.0),
                    }
                }
                let (login, refresh) = (login.unwrap(), refresh.unwrap());
                let callback = |id: &str, method: &str, args: Value| json!({"type": "call", "id": id, "ext": "a", "method": method, "args": args, "parent": login, "generation": "7"});
                // A burst in one write: a display call, a prompt that never answers, another
                // prompt and a device code.
                let mut burst = bytes(&callback(
                    "r:0",
                    "openUrl",
                    json!({"url": "https://login.test/a", "instructions": "go"}),
                ));
                burst.extend(bytes(&callback(
                    "r:1",
                    "prompt",
                    json!({"message": "never", "secret": true}),
                )));
                burst.extend(bytes(&callback(
                    "r:2",
                    "prompt",
                    json!({"message": "code", "placeholder": "x", "secret": false}),
                )));
                burst.extend(bytes(&callback("r:3", "deviceCode", json!({"userCode": "UC", "verificationUrl": "https://login.test/d", "expiresInSeconds": 60}))));
                raw.writer.write_all(&burst).await.unwrap();
                let mut replies = std::collections::BTreeMap::new();
                while replies.len() < 3 {
                    let frame = raw.next().await.unwrap();
                    assert_eq!(frame["type"], "ok", "{frame}");
                    replies.insert(
                        frame["id"].as_str().unwrap().to_owned(),
                        frame["value"].clone(),
                    );
                }
                assert_eq!(replies["r:0"], Value::Null);
                assert_eq!(replies["r:2"], "answer-code");
                assert_eq!(replies["r:3"], Value::Null);
                // While "never" is still pending, an unrelated call completes.
                raw.send(ok(&refresh, credential("refreshed"))).await;
                // Withdraw only the pending prompt (once the host UI shows it), then stream
                // progress and finish.
                wait_for("prompt shown", || {
                    ui.events().contains(&"prompt never".to_owned())
                })
                .await;
                raw.send(json!({"type": "cancel", "id": "r:1"})).await;
                raw.send(callback(
                    "r:4",
                    "progress",
                    json!({"message": "exchanging"}),
                ))
                .await;
                let ack = raw.next().await.unwrap();
                assert_eq!(
                    (ack["type"].as_str(), ack["id"].as_str()),
                    (Some("ok"), Some("r:4"))
                );
                raw.send(ok(&login, credential("logged-in"))).await;
            }
        );
        assert_eq!(refresh.unwrap()["access"], "refreshed");
        assert_eq!(login.unwrap()["access"], "logged-in");
        let events = ui.events();
        assert!(
            events.contains(&"url https://login.test/a Some(\"go\")".to_owned()),
            "{events:?}"
        );
        assert!(
            events.contains(&"code UC https://login.test/d".to_owned()),
            "{events:?}"
        );
        assert_eq!(events.last().unwrap(), "progress exchanging");
        assert_eq!(
            ui.dropped.load(Ordering::SeqCst),
            1,
            "the canceled prompt was not dropped"
        );
    }

    #[tokio::test]
    async fn dropped_call_sends_cancel_and_ignores_its_late_reply() {
        let (peer, mut raw) = pair().await;
        let pending = peer.call("a", "rewrite", json!({"provider": "p", "model": {}, "request": {"url": "http://x/", "headers": [], "body": {}}}), None);
        let id = tokio::select! {
            _ = pending => panic!("finished without a reply"),
            frame = raw.next() => called(&frame.unwrap()).0,
        };
        // `pending` was dropped by the select above.
        let cancel = raw.next().await.unwrap();
        assert_eq!(
            cancel,
            json!({"type": "cancel", "id": id, "generation": "7"})
        );
        // A late reply to the withdrawn call is dropped; the connection stays healthy.
        raw.send(ok(&id, credential("late"))).await;
        let (result, ()) = tokio::join!(
            peer.call(
                "a",
                "refresh",
                json!({"provider": "p", "credential": credential("old")}),
                None
            ),
            async {
                let (next, ..) = called(&raw.next().await.unwrap());
                assert_ne!(next, id);
                raw.send(ok(&next, credential("fresh"))).await;
            }
        );
        assert_eq!(result.unwrap()["access"], "fresh");
    }

    #[tokio::test]
    async fn dropped_login_drops_its_ui_and_late_callbacks() {
        let (peer, mut raw) = pair().await;
        let ui = Ui::default();
        {
            let login = peer.call("a", "login", json!({"provider": "p"}), Some(&ui));
            tokio::pin!(login);
            let id = tokio::select! {
                _ = &mut login => panic!("finished without a reply"),
                frame = raw.next() => called(&frame.unwrap()).0,
            };
            raw.send(json!({"type": "call", "id": "r:0", "ext": "a", "method": "prompt", "args": {"message": "never", "secret": false}, "parent": id})).await;
            // Drive the login until the host UI is waiting on the prompt.
            tokio::select! {
                _ = &mut login => panic!("login finished"),
                () = wait_for("prompt shown", || !ui.events().is_empty()) => {}
            }
            assert_eq!(ui.dropped.load(Ordering::SeqCst), 0);
        }
        // Dropping the login dropped its host UI future and withdrew the call.
        assert_eq!(ui.dropped.load(Ordering::SeqCst), 1);
        let cancel = raw.next().await.unwrap();
        assert_eq!(cancel["type"], "cancel");
        // A callback naming the finished login is ignored, not forwarded and not fatal.
        let parent = cancel["id"].clone();
        raw.send(json!({"type": "call", "id": "r:1", "ext": "a", "method": "progress", "args": {"message": "late"}, "parent": parent})).await;
        let (result, ()) = tokio::join!(
            peer.call(
                "a",
                "refresh",
                json!({"provider": "p", "credential": credential("old")}),
                None
            ),
            async {
                let (id, ..) = called(&raw.next().await.unwrap());
                raw.send(ok(&id, credential("fresh"))).await;
            }
        );
        assert_eq!(result.unwrap()["access"], "fresh");
        assert_eq!(ui.events(), ["prompt never"]);
    }

    #[tokio::test]
    async fn ui_errors_and_bad_selections_answer_without_closing() {
        let (peer, mut raw) = pair().await;
        let ui = Ui::default();
        let (login, ()) = tokio::join!(
            peer.call("a", "login", json!({"provider": "p"}), Some(&ui)),
            async {
                let (login, ..) = called(&raw.next().await.unwrap());
                let callback = |id: &str, method: &str, args: Value| json!({"type": "call", "id": id, "ext": "a", "method": method, "args": args, "parent": login});
                let options = json!([{"id": "one", "label": "One"}, {"id": "two", "label": "Two"}]);
                raw.send(callback(
                    "r:0",
                    "prompt",
                    json!({"message": "refuse", "secret": false}),
                ))
                .await;
                raw.send(callback(
                    "r:1",
                    "select",
                    json!({"message": "bogus", "options": options}),
                ))
                .await;
                raw.send(callback(
                    "r:2",
                    "select",
                    json!({"message": "pick", "options": options}),
                ))
                .await;
                raw.send(callback("r:3", "prompt", json!({"message": 5})))
                    .await;
                raw.send(callback("r:4", "teleport", json!({}))).await;
                let mut replies = std::collections::BTreeMap::new();
                while replies.len() < 5 {
                    let frame = raw.next().await.unwrap();
                    replies.insert(frame["id"].as_str().unwrap().to_owned(), frame);
                }
                assert_eq!(replies["r:0"]["type"], "error");
                assert_eq!(replies["r:1"]["code"], "invalid");
                assert_eq!(replies["r:2"]["value"], "one");
                assert_eq!(replies["r:3"]["code"], "invalid");
                assert_eq!(replies["r:4"]["code"], "unsupported");
                raw.send(ok(&login, credential("done"))).await;
            }
        );
        assert_eq!(login.unwrap()["access"], "done");
    }

    #[tokio::test]
    async fn protocol_violations_close_and_fail_pending_calls() {
        let violations: Vec<(&str, Vec<u8>)> = vec![
            ("invalid json", {
                let mut out = 5u32.to_be_bytes().to_vec();
                out.extend_from_slice(b"{nope");
                out
            }),
            ("not an object", bytes(&json!([1, 2]))),
            (
                "wrong generation",
                bytes(&json!({"type": "pong", "id": "h:0", "generation": "8"})),
            ),
            (
                "unknown type",
                bytes(&json!({"type": "shout", "id": "r:0", "generation": "7"})),
            ),
            (
                "unparented callback",
                bytes(
                    &json!({"type": "call", "id": "r:0", "ext": "a", "method": "progress", "args": {"message": "x"}, "generation": "7"}),
                ),
            ),
            (
                "parent is not a login",
                bytes(
                    &json!({"type": "call", "id": "r:0", "ext": "a", "method": "progress", "args": {"message": "x"}, "parent": "h:0", "generation": "7"}),
                ),
            ),
            (
                "host method from runner",
                bytes(
                    &json!({"type": "call", "id": "r:0", "ext": "a", "method": "login", "args": {}, "generation": "7"}),
                ),
            ),
            (
                "cancel of a host call",
                bytes(&json!({"type": "cancel", "id": "h:0", "generation": "7"})),
            ),
            (
                "reply to unknown host id",
                bytes(&json!({"type": "ok", "id": "h:99", "value": null, "generation": "7"})),
            ),
        ];
        for (name, frame) in violations {
            let (peer, mut raw) = pair().await;
            let (result, ()) = tokio::join!(
                peer.call(
                    "a",
                    "refresh",
                    json!({"provider": "p", "credential": credential("old")}),
                    None
                ),
                async {
                    raw.next().await.unwrap();
                    raw.writer.write_all(&frame).await.unwrap();
                    raw.expect_eof().await;
                }
            );
            assert!(
                matches!(result, Err(CallError::Unavailable)),
                "{name}: {result:?}"
            );
            closes(&peer).await;
            // Retired: new calls fail at once without writing anything.
            assert!(matches!(
                peer.call(
                    "a",
                    "refresh",
                    json!({"provider": "p", "credential": credential("old")}),
                    None
                )
                .await,
                Err(CallError::Unavailable)
            ));
            peer.join().await;
        }

        // Duplicate live callback IDs inside one login.
        let (peer, mut raw) = pair().await;
        let ui = Ui::default();
        let (result, ()) = tokio::join!(
            peer.call("a", "login", json!({"provider": "p"}), Some(&ui)),
            async {
                let (login, ..) = called(&raw.next().await.unwrap());
                let prompt = json!({"type": "call", "id": "r:0", "ext": "a", "method": "prompt", "args": {"message": "never", "secret": false}, "parent": login});
                raw.send(prompt.clone()).await;
                wait_for("prompt shown", || !ui.events().is_empty()).await;
                raw.send(prompt).await;
                raw.expect_eof().await;
            }
        );
        assert!(matches!(result, Err(CallError::Unavailable)), "{result:?}");
        assert_eq!(
            ui.dropped.load(Ordering::SeqCst),
            1,
            "connection loss must drop the UI future"
        );

        // EOF fails every pending call once.
        let (peer, mut raw) = pair().await;
        let (a, b, ()) = tokio::join!(
            peer.call(
                "a",
                "refresh",
                json!({"provider": "p", "credential": credential("old")}),
                None
            ),
            peer.call(
                "b",
                "refresh",
                json!({"provider": "p", "credential": credential("old")}),
                None
            ),
            async {
                raw.next().await.unwrap();
                raw.next().await.unwrap();
                raw.writer.shutdown().await.unwrap();
            }
        );
        assert!(matches!(a, Err(CallError::Unavailable)), "{a:?}");
        assert!(matches!(b, Err(CallError::Unavailable)), "{b:?}");
        closes(&peer).await;
    }

    #[tokio::test]
    async fn oversized_length_closes_before_the_body_arrives() {
        let (peer, mut raw) = pair().await;
        // Only the prefix: a peer that waited for (or allocated) the body would hang here.
        raw.writer
            .write_all(&((MAX_FRAME as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        closes(&peer).await;
        raw.expect_eof().await;
        assert!(matches!(
            peer.call(
                "a",
                "refresh",
                json!({"provider": "p", "credential": credential("old")}),
                None
            )
            .await,
            Err(CallError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn oversized_outgoing_call_fails_locally() {
        let (peer, mut raw) = pair().await;
        let huge = "x".repeat(MAX_FRAME);
        let result = peer
            .call("a", "rewrite", json!({"provider": "p", "model": {}, "request": {"url": "http://x/", "headers": [], "body": {"big": huge}}}), None)
            .await;
        assert!(matches!(result, Err(CallError::Invalid)), "{result:?}");
        // Unknown methods and login without a UI are also rejected before writing.
        assert!(matches!(
            peer.call("a", "teleport", json!({}), None).await,
            Err(CallError::Invalid)
        ));
        assert!(matches!(
            peer.call("a", "login", json!({"provider": "p"}), None)
                .await,
            Err(CallError::Invalid)
        ));
        // Nothing was written for them, and the connection is healthy.
        let (result, ()) = tokio::join!(
            peer.call(
                "a",
                "refresh",
                json!({"provider": "p", "credential": credential("old")}),
                None
            ),
            async {
                let frame = raw.next().await.unwrap();
                assert_eq!(called(&frame).2, "refresh");
                raw.send(ok(frame["id"].as_str().unwrap(), credential("fresh")))
                    .await;
            }
        );
        assert_eq!(result.unwrap()["access"], "fresh");
    }

    #[tokio::test]
    async fn heartbeat_keeps_a_live_runner_and_kills_a_silent_one() {
        tokio::time::pause();
        let (peer, mut raw) = pair().await;
        let start = tokio::time::Instant::now();
        // Answering pings (inside `next`) for 30 virtual seconds keeps the connection open.
        let pings = tokio::select! {
            () = peer.closed() => panic!("closed a live runner"),
            count = async {
                let mut count = 0;
                while start.elapsed() < Duration::from_secs(30) {
                    let frame = next_frame(&mut raw.reader).await.unwrap();
                    assert_eq!(frame["type"], "ping", "{frame}");
                    count += 1;
                    raw.send(json!({"type": "pong", "id": frame["id"]})).await;
                }
                count
            } => count,
        };
        assert!((14..=16).contains(&pings), "{pings} pings in 30 s");

        // A runner that stops answering is closed after 10 s without a pong.
        let silent = tokio::time::Instant::now();
        let ui = Ui::default();
        let (result, ()) = tokio::join!(
            peer.call("a", "login", json!({"provider": "p"}), Some(&ui)),
            async {
                peer.closed().await;
            }
        );
        let waited = silent.elapsed();
        assert!(matches!(result, Err(CallError::Unavailable)), "{result:?}");
        assert!(
            waited >= Duration::from_secs(8) && waited <= Duration::from_secs(13),
            "closed after {waited:?}"
        );
        drop(raw);
    }
}
