//! The Rust end of the runner protocol (`runner.ts` is the other end): one authenticated
//! Unix-socket connection bound to one generation.
//!
//! Frames are a 4-byte big-endian length plus UTF-8 JSON, at most [`MAX_FRAME`] bytes both
//! ways. One reader demultiplexes without awaiting user code. Calls and callback replies
//! reserve their encoded bytes before allocation; only heartbeat pings bypass the data
//! queue. Rust calls use IDs `h:<n>` (shared with pings), runner calls `r:<n>`. Dropping a
//! sent call queues a cancel in a separately bounded slot behind its call frame.
//! A runner call naming a `parent` belongs to that host call and runs inside its future
//! through the [`Callbacks`] it was given; one without a parent is a session action, run
//! on its own task through the connection's [`Actions`] with the [`RunnerRef`] it arrived on.
//! The peer knows no method names.
//! Any protocol violation, EOF, write failure or missed heartbeat closes the peer and fails
//! every pending call with [`CallError::Unavailable`].

use std::collections::{BTreeSet, HashMap};
use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use async_trait::async_trait;
use axo::ext::{DispatchCode, InvokeError, InvokeErrorCode};
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval_at};
use tokio_util::sync::CancellationToken;

/// Largest frame body either side may send or accept.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;
/// Bytes of data frames queued ahead of the socket before senders wait for room.
const QUEUE_LIMIT: usize = 2 * MAX_FRAME;
/// Heartbeat pings waiting ahead of the data queue.
const CONTROL_SLOTS: usize = 64;
/// Cancel frames queued behind their calls; none left means the writer is stuck.
const CANCEL_SLOTS: usize = 256;
/// Live runner calls (parented and session actions) across one connection.
const MAX_CALLBACKS: usize = 256;
pub const PING_EVERY: Duration = Duration::from_secs(2);
pub const PONG_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, specta::Type)]
#[serde(untagged)]
pub enum HostErrorCode {
    Invoke(InvokeErrorCode),
    Bridge(BridgeCode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum BridgeCode {
    NoLifecycle,
    NoSubscription,
    Unsupported,
    #[allow(dead_code)] // Produced by the SDK runner.
    Stalled,
    Unavailable,
    Invalid,
    TooLarge,
    Interaction,
}

impl From<InvokeErrorCode> for HostErrorCode {
    fn from(code: InvokeErrorCode) -> Self {
        Self::Invoke(code)
    }
}

impl From<DispatchCode> for HostErrorCode {
    fn from(code: DispatchCode) -> Self {
        Self::Invoke(InvokeErrorCode::Dispatch(code))
    }
}

impl From<BridgeCode> for HostErrorCode {
    fn from(code: BridgeCode) -> Self {
        Self::Bridge(code)
    }
}

/// Why Rust refuses a runner call; the runner receives these fields as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: HostErrorCode,
    pub message: String,
    pub details: Option<Value>,
}

impl Refusal {
    pub(crate) fn new(code: impl Into<HostErrorCode>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }

    pub(crate) fn invalid_arguments() -> Self {
        Self::new(BridgeCode::Invalid, "invalid arguments")
    }
}

pub(crate) fn decode<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, Refusal> {
    serde_json::from_value(args).map_err(|_| Refusal::invalid_arguments())
}

impl From<InvokeError> for Refusal {
    fn from(error: InvokeError) -> Self {
        let code = error.code();
        if let InvokeError::Failed {
            message, details, ..
        } = error
        {
            match details.map(serde_json::to_value).transpose() {
                Ok(details) => Self {
                    code: code.into(),
                    message,
                    details,
                },
                Err(error) => Self::new(DispatchCode::Encode, error.to_string()),
            }
        } else {
            Self::new(code, error.to_string())
        }
    }
}

/// Serves the runner's calls made under one host call (`parent`).
#[async_trait]
pub trait Callbacks: Send + Sync {
    async fn call(&self, method: &str, args: Value) -> Result<Value, Refusal>;
}

/// Serves runner calls that belong to no host call.
pub trait Actions: Send + Sync {
    fn call(
        &self,
        runner: RunnerRef,
        ext: String,
        method: String,
        args: Value,
    ) -> BoxFuture<'static, Result<Value, Refusal>>;
}

/// The connection a session action arrived on. Calls reach that generation only, never a
/// replacement.
#[derive(Clone)]
pub struct RunnerRef {
    inner: Weak<Inner>,
    closed: CancellationToken,
}

impl RunnerRef {
    /// Calls a runner method for `ext` on this connection; `CallError::Unavailable` once it
    /// is gone. Dropping the future withdraws the call.
    pub async fn call(&self, ext: &str, method: &str, args: Value) -> Result<Value, CallError> {
        let inner = self.inner.upgrade().ok_or(CallError::Unavailable)?;
        inner.call(ext, method, args, None).await
    }

    /// Resolves once the connection is closed.
    pub async fn closed(&self) {
        self.closed.cancelled().await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    /// The connection is closed or closed while the call was pending.
    #[error("extension runner is unavailable")]
    Unavailable,
    /// The runner answered with an error; the message is safe to show.
    #[error("{0}")]
    Method(&'static str),
    /// Rejected locally (method/argument shape or frame size); nothing was sent.
    #[error("extension request is invalid or exceeds the 64 MiB frame bound")]
    Invalid,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Out<'a> {
    Call {
        id: HostId,
        ext: &'a str,
        method: &'a str,
        args: &'a Value,
    },
    Ok {
        id: &'a str,
        value: &'a Value,
    },
    Error {
        id: &'a str,
        code: HostErrorCode,
        message: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<&'a Value>,
    },
    Cancel {
        id: HostId,
    },
    Ping {
        id: HostId,
    },
}

struct HostId(u64);

impl Serialize for HostId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!("h:{}", self.0))
    }
}

/// Counts JSON bytes without storing them or accepting an oversized body.
#[derive(Default)]
struct BodyLength(usize);

impl Write for BodyLength {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_FRAME - self.0 {
            return Err(io::Error::other("frame too large"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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
    closed: CancellationToken,
    state: Mutex<State>,
    control: mpsc::Sender<Vec<u8>>,
    data: mpsc::UnboundedSender<(Vec<u8>, OwnedSemaphorePermit)>,
    budget: Arc<Semaphore>,
    cancels: Arc<Semaphore>,
    /// The extension IDs this runner hosts; a session action may only name one of them.
    members: Arc<[String]>,
    actions: Arc<dyn Actions>,
}

struct State {
    /// Next `h:` counter; every lower number was issued (live, finished or a ping).
    next: u64,
    calls: HashMap<u64, Pending>,
    pings: BTreeSet<u64>,
    last_pong: Instant,
    /// Live runner calls by `r:` ID.
    callbacks: HashMap<String, Live>,
}

struct Pending {
    ext: String,
    reply: oneshot::Sender<Result<Value, CallError>>,
    /// Present only for a call given callbacks: routes its runner calls to its future.
    callbacks: Option<mpsc::UnboundedSender<Callback>>,
}

struct Live {
    /// The host call it belongs to; `None` for a session action.
    parent: Option<u64>,
    cancel: CancellationToken,
}

struct Callback {
    id: String,
    method: String,
    args: Value,
    cancel: CancellationToken,
}

impl Peer {
    /// Authenticates the runner's hello on `stream` and starts the reader, writer and
    /// heartbeat on this connection; `actions` serves the session actions of `members`, the
    /// extensions this runner hosts.
    pub async fn connect(
        stream: UnixStream,
        nonce: &str,
        members: Arc<[String]>,
        actions: Arc<dyn Actions>,
    ) -> Result<Arc<Peer>, String> {
        let (read, write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let hello = match read_frame(&mut reader).await {
            Ok(Some(hello)) => hello,
            Ok(None) => {
                return Err("extension runner closed the connection before its hello".into());
            }
            Err(reason) => return Err(format!("extension runner handshake failed: {reason}")),
        };
        let valid = hello.get("type").and_then(Value::as_str) == Some("hello")
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
            members,
            actions,
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

    /// Calls a runner method for `ext`. Runner calls parented to this one are served by
    /// `callbacks` inside this future until the terminal reply; without `callbacks` they are
    /// a protocol violation. Dropping the future withdraws the call; deadlines belong to the
    /// caller.
    pub async fn call(
        &self,
        ext: &str,
        method: &str,
        args: Value,
        callbacks: Option<&dyn Callbacks>,
    ) -> Result<Value, CallError> {
        self.inner.call(ext, method, args, callbacks).await
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
            let keep = live.parent != Some(id);
            if !keep {
                live.cancel.cancel();
            }
            keep
        });
        Some(pending)
    }

    /// The number of an issued `h:` ID.
    fn issued(&self, id: &str) -> Option<u64> {
        host_id(id).filter(|&n| n < self.next)
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// See [`Peer::call`].
    async fn call(
        &self,
        ext: &str,
        method: &str,
        args: Value,
        callbacks: Option<&dyn Callbacks>,
    ) -> Result<Value, CallError> {
        let (reply, mut result) = oneshot::channel();
        let (route, mut routed) = mpsc::unbounded_channel();
        let id = {
            let mut state = self.lock();
            if self.closed.is_cancelled() {
                return Err(CallError::Unavailable);
            }
            let id = state.next;
            state.next += 1;
            state.calls.insert(
                id,
                Pending {
                    ext: ext.to_owned(),
                    reply,
                    callbacks: callbacks.is_some().then_some(route),
                },
            );
            id
        };
        let mut outstanding = Outstanding {
            inner: self,
            id,
            sent: false,
        };
        self.enqueue(&Out::Call {
            id: HostId(id),
            ext,
            method,
            args: &args,
        })
        .await?;
        drop(args);
        outstanding.sent = true;

        let mut served = FuturesUnordered::new();
        loop {
            tokio::select! {
                biased;
                done = &mut result => return done.unwrap_or(Err(CallError::Unavailable)),
                Some(callback) = routed.recv() => {
                    if let Some(callbacks) = callbacks {
                        served.push(self.interact(callbacks, callback));
                    }
                }
                Some(()) = served.next(), if !served.is_empty() => {}
            }
        }
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

    /// Counts the body, reserving its exact framed size before encoding.
    async fn enqueue(&self, frame: &Out<'_>) -> Result<(), CallError> {
        let mut length = BodyLength::default();
        serde_json::to_writer(&mut length, frame).map_err(|_| CallError::Invalid)?;
        let frame_len = 4 + length.0;
        let permit = tokio::select! {
            biased;
            _ = self.closed.cancelled() => return Err(CallError::Unavailable),
            permit = self.budget.clone().acquire_many_owned(frame_len as u32) => {
                permit.map_err(|_| CallError::Unavailable)?
            }
        };
        let mut encoded = Vec::with_capacity(frame_len);
        encoded.extend_from_slice(&(length.0 as u32).to_be_bytes());
        serde_json::to_writer(&mut encoded, frame).map_err(|_| CallError::Invalid)?;
        self.data
            .send((encoded, permit))
            .map_err(|_| CallError::Unavailable)
    }

    /// Only locally generated pings and cancels bypass the byte budget.
    fn small_frame(frame: &Out<'_>) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(52);
        encoded.extend_from_slice(&[0; 4]);
        serde_json::to_writer(&mut encoded, frame).expect("fixed control frame");
        let len = (encoded.len() - 4) as u32;
        encoded[..4].copy_from_slice(&len.to_be_bytes());
        encoded
    }

    /// Queues a cancel behind the call it withdraws; tears down rather than block.
    fn cancel(&self, id: u64) {
        let queued = self.cancels.clone().try_acquire_owned().is_ok_and(|slot| {
            self.data
                .send((Self::small_frame(&Out::Cancel { id: HostId(id) }), slot))
                .is_ok()
        });
        if !queued {
            self.close();
        }
    }

    /// Sends a heartbeat ahead of the data queue without waiting.
    fn ping(&self, id: u64) {
        if self
            .control
            .try_send(Self::small_frame(&Out::Ping { id: HostId(id) }))
            .is_err()
        {
            self.close();
        }
    }

    fn dispatch(self: &Arc<Self>, frame: Value) -> Result<(), &'static str> {
        let Value::Object(mut frame) = frame else {
            return Err("frame is not an object");
        };
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
                    .unwrap_or_default();
                self.settle(&id, Err(method_error(code)))
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
    fn settle(&self, id: &str, result: Result<Value, &'static str>) -> Result<(), &'static str> {
        let pending = {
            let mut state = self.lock();
            let n = state.issued(id).ok_or("reply to an unissued id")?;
            state.finish(n)
        };
        if let Some(pending) = pending {
            let _ = pending.reply.send(result.map_err(CallError::Method));
        }
        Ok(())
    }

    fn callback(
        self: &Arc<Self>,
        id: String,
        mut frame: Map<String, Value>,
    ) -> Result<(), &'static str> {
        if !is_runner_id(&id) {
            return Err("call id is not a runner id");
        }
        let args = frame.remove("args").unwrap_or(Value::Null);
        let Some(Value::String(method)) = frame.remove("method") else {
            return Err("malformed call");
        };
        let Some(Value::String(ext)) = frame.remove("ext") else {
            return Err("malformed call");
        };
        let parent = match frame.get("parent") {
            None | Some(Value::Null) => None,
            Some(Value::String(parent)) => Some(parent.as_str()),
            Some(_) => return Err("malformed call"),
        };
        let mut state = self.lock();
        let parent = match parent {
            Some(parent) => Some(
                state
                    .issued(parent)
                    .ok_or("callback parent was never issued")?,
            ),
            None => None,
        };
        if state.callbacks.contains_key(&id) {
            return Err("duplicate live callback id");
        }
        let route = match parent {
            Some(parent) => {
                let Some(pending) = state.calls.get(&parent) else {
                    return Ok(()); // the call already ended; the runner drops its own callback
                };
                if pending.ext != ext {
                    return Err("callback for another extension's call");
                }
                let Some(route) = pending.callbacks.clone() else {
                    return Err("callback parent takes no callbacks");
                };
                Some(route)
            }
            None if !self.members.contains(&ext) => {
                return Err("session action for an extension outside this runner");
            }
            None => None,
        };
        if state.callbacks.len() >= MAX_CALLBACKS {
            return Err("too many live callbacks");
        }
        let cancel = CancellationToken::new();
        state.callbacks.insert(
            id.clone(),
            Live {
                parent,
                cancel: cancel.clone(),
            },
        );
        drop(state);
        match route {
            Some(route) => {
                let _ = route.send(Callback {
                    id,
                    method,
                    args,
                    cancel,
                });
            }
            None => {
                let inner = Arc::clone(self);
                let runner = RunnerRef {
                    inner: Arc::downgrade(self),
                    closed: self.closed.clone(),
                };
                tokio::spawn(async move {
                    let answer = tokio::select! {
                        _ = cancel.cancelled() => return,
                        answer = async { inner.actions.call(runner, ext, method, args).await } => answer,
                    };
                    inner.answer(id, answer).await;
                });
            }
        }
        Ok(())
    }

    /// Runs one parented runner call through the host call's callbacks and answers it,
    /// unless the runner or the host call withdrew it first.
    async fn interact(&self, callbacks: &dyn Callbacks, callback: Callback) {
        let Callback {
            id,
            method,
            args,
            cancel,
        } = callback;
        let answer = tokio::select! {
            _ = cancel.cancelled() => return,
            answer = callbacks.call(&method, args) => answer,
        };
        self.answer(id, answer).await;
    }

    /// Replies to a live runner call; a withdrawn one gets nothing.
    async fn answer(&self, id: String, answer: Result<Value, Refusal>) {
        if self.lock().callbacks.remove(&id).is_none() {
            return;
        }
        let frame = match &answer {
            Ok(value) => Out::Ok { id: &id, value },
            Err(Refusal {
                code,
                message,
                details,
            }) => Out::Error {
                id: &id,
                code: *code,
                message,
                details: details.as_ref(),
            },
        };
        if self.enqueue(&frame).await == Err(CallError::Invalid) {
            drop(answer);
            let _ = self
                .enqueue(&Out::Error {
                    id: &id,
                    code: BridgeCode::TooLarge.into(),
                    message: "host reply exceeds 64 MiB",
                    details: None,
                })
                .await;
        }
    }
}

async fn read_loop(inner: Arc<Inner>, mut reader: BufReader<OwnedReadHalf>) {
    loop {
        let frame = tokio::select! {
            _ = inner.closed.cancelled() => return,
            frame = read_frame(&mut reader) => frame,
        };
        let violation = match frame {
            Ok(Some(frame)) => inner.dispatch(frame).err(),
            Ok(None) => break,
            Err(reason) => Some(reason),
        };
        if let Some(reason) = violation {
            tracing::warn!(reason = reason, "extension runner protocol violation");
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
        inner.ping(id);
    }
}

fn is_counter(n: &str) -> bool {
    !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
}

fn host_id(id: &str) -> Option<u64> {
    id.strip_prefix("h:")
        .filter(|n| is_counter(n))?
        .parse()
        .ok()
}

fn is_runner_id(id: &str) -> bool {
    id.strip_prefix("r:").is_some_and(is_counter)
}

/// Runner text never reaches the host, even when a peer sends an unexpected message field.
fn method_error(code: &str) -> &'static str {
    match code {
        "activation" => "activation rejected",
        "invalid" => "invalid argument or result",
        "unsupported" => "unsupported operation",
        "too_large" => "reply exceeds 64 MiB",
        _ => "handler error",
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

    use axo_ai::{DeviceCode, Error, LoginInteraction, LoginPrompt, LoginSelect};

    use serde_json::json;

    use super::*;
    use crate::proxy::LoginCallbacks;

    /// Drops of the `never` session action's future.
    static NEVER_DROPPED: AtomicUsize = AtomicUsize::new(0);
    /// Starts of the `never` session action: the task has created its future.
    static NEVER_STARTED: AtomicUsize = AtomicUsize::new(0);

    /// Echoes `echo`, refuses `refuse`, never answers `never`.
    struct TestActions;

    impl Actions for TestActions {
        fn call(
            &self,
            _runner: RunnerRef,
            ext: String,
            method: String,
            args: Value,
        ) -> BoxFuture<'static, Result<Value, Refusal>> {
            if method == "never" {
                NEVER_STARTED.fetch_add(1, Ordering::SeqCst);
            }
            // Marked from creation: a withdrawn action may never be polled.
            let mark = (method == "never").then(|| DropMark(&NEVER_DROPPED));
            Box::pin(async move {
                match method.as_str() {
                    "echo" => Ok(json!({"ext": ext, "args": args})),
                    "never" => {
                        let _mark = mark;
                        std::future::pending().await
                    }
                    _ => Err(Refusal::new(
                        DispatchCode::NoSession,
                        "session s is not attached",
                    )),
                }
            })
        }
    }

    fn actions() -> Arc<dyn Actions> {
        Arc::new(TestActions)
    }

    /// The extensions the test runner hosts: every frame names one of these, except `z`.
    fn members() -> Arc<[String]> {
        ["a".to_owned(), "b".to_owned()].into()
    }

    const NONCE: &str = "4e6f6e6365";

    /// The runner's end of the connection.
    struct Raw {
        reader: OwnedReadHalf,
        writer: OwnedWriteHalf,
    }

    impl Raw {
        async fn send(&mut self, frame: Value) {
            self.writer.write_all(&bytes(&frame)).await.unwrap();
        }

        /// The next non-ping frame from Rust, answering pings on the way; `None` at EOF.
        async fn next(&mut self) -> Option<Value> {
            loop {
                let frame = next_frame(&mut self.reader).await?;
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

    #[test]
    fn outbound_error_details_and_control_ids_keep_their_shapes() {
        let details = json!({"status": 400});
        let expected = json!({"type": "error", "id": "r:1", "code": "invalid", "message": "bad"});
        let mut expected_with_details = expected.clone();
        expected_with_details["details"] = details.clone();
        for (details, expected) in [(None, expected), (Some(&details), expected_with_details)] {
            let frame = Out::Error {
                id: "r:1",
                code: BridgeCode::Invalid.into(),
                message: "bad",
                details,
            };
            assert_eq!(serde_json::to_value(frame).unwrap(), expected);
        }

        for (frame, kind) in [
            (
                Out::Cancel {
                    id: HostId(u64::MAX),
                },
                "cancel",
            ),
            (
                Out::Ping {
                    id: HostId(u64::MAX),
                },
                "ping",
            ),
        ] {
            let encoded = Inner::small_frame(&frame);
            assert_eq!(
                u32::from_be_bytes(encoded[..4].try_into().unwrap()) as usize,
                encoded.len() - 4
            );
            assert_eq!(
                serde_json::from_slice::<Value>(&encoded[4..]).unwrap(),
                json!({"type": kind, "id": "h:18446744073709551615"})
            );
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
        json!({"type": "hello", "nonce": NONCE})
    }

    /// A connected peer and the runner end.
    async fn pair() -> (Arc<Peer>, Raw) {
        let (host, runner) = UnixStream::pair().unwrap();
        let (reader, writer) = runner.into_split();
        let mut raw = Raw { reader, writer };
        let (peer, ()) = tokio::join!(Peer::connect(host, NONCE, members(), actions()), async {
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
                "oversized" => Ok("x".repeat(MAX_FRAME)),
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
        let (peer, ()) = tokio::join!(Peer::connect(host, NONCE, members(), actions()), async {
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
                let reply = bytes(&ok(&id, credential("new")));
                let (head, tail) = reply.split_at(3);
                raw.writer.write_all(head).await.unwrap();
                tokio::task::yield_now().await;
                raw.writer.write_all(tail).await.unwrap();
            }
        );
        assert_eq!(result.unwrap()["access"], "new");
        peer.close();
        peer.join().await;

        let bad = [
            json!({"type": "hello", "nonce": "wrong"}),
            json!({"type": "hello"}),
            json!({"type": "pong", "id": "h:0"}),
        ];
        for frame in bad {
            let (host, mut runner) = UnixStream::pair().unwrap();
            let (peer, ()) =
                tokio::join!(Peer::connect(host, NONCE, members(), actions()), async {
                    runner.write_all(&bytes(&frame)).await.unwrap();
                });
            assert!(peer.is_err(), "accepted {frame}");
        }
    }

    #[tokio::test]
    async fn retired_nonce_does_not_authenticate_a_foreign_connection() {
        let (retired, mut old_socket) = pair().await;
        retired.close();
        old_socket.expect_eof().await;
        retired.join().await;

        let (host, mut foreign) = UnixStream::pair().unwrap();
        // Even a complete hello replayed from the previous runner cannot bind a new peer.
        foreign.write_all(&bytes(&hello())).await.unwrap();
        let result = Peer::connect(host, "new nonce", members(), actions()).await;
        assert!(
            matches!(result, Err(reason) if reason == "extension runner sent an invalid hello")
        );
        assert!(next_frame(&mut foreign).await.is_none());
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
                let mut both = bytes(&json!({"type": "error", "id": rewrite.0, "code": "failed"}));
                both.extend(bytes(&ok(&refresh.0, credential("new"))));
                raw.writer.write_all(&both).await.unwrap();
            }
        );
        assert_eq!(refresh.unwrap()["access"], "new");
        assert_eq!(rewrite, Err(CallError::Method("handler error")));

        // Runner text never reaches the host, including unrecognized or malformed codes.
        for (code, phrase) in [
            (Some(json!("failed")), "handler error"),
            (Some(json!("invalid")), "invalid argument or result"),
            (Some(json!("unsupported")), "unsupported operation"),
            (Some(json!("too_large")), "reply exceeds 64 MiB"),
            (Some(json!("activation")), "activation rejected"),
            (Some(json!("secret-marker")), "handler error"),
            (Some(json!(42)), "handler error"),
            (None, "handler error"),
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
                    let mut frame = json!({"type": "error", "id": id, "message": "token sk-secret leaked\nstack"});
                    if let Some(code) = &code {
                        frame["code"] = code.clone();
                    }
                    raw.send(frame).await;
                }
            );
            assert_eq!(result, Err(CallError::Method(phrase)), "{code:?}");
        }
    }

    #[tokio::test]
    async fn login_interleaves_callbacks_and_cancels_one_prompt() {
        let (peer, mut raw) = pair().await;
        let ui = Ui::default();
        let callbacks = LoginCallbacks(&ui);
        let (login, refresh, ()) = tokio::join!(
            peer.call("a", "login", json!({"provider": "p"}), Some(&callbacks)),
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
                let callback = |id: &str, method: &str, args: Value| json!({"type": "call", "id": id, "ext": "a", "method": method, "args": args, "parent": login});
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
        assert_eq!(cancel, json!({"type": "cancel", "id": id}));
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
        let callbacks = LoginCallbacks(&ui);
        {
            let login = peer.call("a", "login", json!({"provider": "p"}), Some(&callbacks));
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
        let callbacks = LoginCallbacks(&ui);
        let (login, ()) = tokio::join!(
            peer.call("a", "login", json!({"provider": "p"}), Some(&callbacks)),
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
                raw.send(callback("r:4", "teleport", json!(7))).await;
                raw.send(callback(
                    "r:5",
                    "prompt",
                    json!({"message": "typo", "placeholer": "x", "secret": false}),
                ))
                .await;
                for (id, options) in [
                    ("r:6", json!([{"id": "i", "label": "L", "extra": 1}])),
                    ("r:7", json!([["i", "L"]])),
                    ("r:8", json!([])),
                ] {
                    raw.send(callback(
                        id,
                        "select",
                        json!({"message": "invalid", "options": options}),
                    ))
                    .await;
                }
                let mut replies = std::collections::BTreeMap::new();
                while replies.len() < 9 {
                    let frame = raw.next().await.unwrap();
                    replies.insert(frame["id"].as_str().unwrap().to_owned(), frame);
                }
                assert_eq!(replies["r:0"]["type"], "error");
                assert_eq!(replies["r:1"]["code"], "invalid");
                assert_eq!(replies["r:2"]["value"], "one");
                for id in ["r:3", "r:5", "r:6", "r:7", "r:8"] {
                    assert_eq!(replies[id]["type"], "error");
                    assert_eq!(replies[id]["code"], "invalid");
                    assert_eq!(replies[id]["message"], "invalid arguments");
                }
                assert_eq!(replies["r:4"]["type"], "error");
                assert_eq!(replies["r:4"]["code"], "unsupported");
                assert_eq!(replies["r:4"]["message"], "unknown host method");
                raw.send(ok(&login, credential("done"))).await;
            }
        );
        assert_eq!(login.unwrap()["access"], "done");
        let mut events = ui.events();
        events.sort();
        assert_eq!(events, ["prompt refuse", "select bogus", "select pick"]);
    }

    #[tokio::test]
    async fn session_actions_run_outside_host_calls() {
        let (peer, mut raw) = pair().await;
        let action = |id: &str, method: &str| json!({"type": "call", "id": id, "ext": "a", "method": method, "args": {"session": "s"}});
        // No host call is pending: unparented calls are answered on their own.
        raw.send(action("r:0", "never")).await;
        raw.send(action("r:1", "echo")).await;
        raw.send(action("r:2", "refuse")).await;
        let mut replies = std::collections::BTreeMap::new();
        while replies.len() < 2 {
            let frame = raw.next().await.unwrap();
            replies.insert(frame["id"].as_str().unwrap().to_owned(), frame);
        }
        assert_eq!(
            replies["r:1"]["value"],
            json!({"ext": "a", "args": {"session": "s"}})
        );
        assert_eq!(
            (
                replies["r:2"]["code"].as_str(),
                replies["r:2"]["message"].as_str()
            ),
            (Some("no_session"), Some("session s is not attached"))
        );
        // The runner withdraws the pending one once it runs: its future is dropped, nothing
        // is sent. An action withdrawn before its task starts never creates a future.
        wait_for("action started", || {
            NEVER_STARTED.load(Ordering::SeqCst) == 1
        })
        .await;
        raw.send(json!({"type": "cancel", "id": "r:0"})).await;
        wait_for("action dropped", || {
            NEVER_DROPPED.load(Ordering::SeqCst) == 1
        })
        .await;
        // A live action id cannot be reused while it runs.
        raw.send(action("r:3", "never")).await;
        wait_for("second action started", || {
            NEVER_STARTED.load(Ordering::SeqCst) == 2
        })
        .await;
        raw.send(action("r:3", "never")).await;
        raw.expect_eof().await;
        closes(&peer).await;
        // Closing dropped the remaining action too.
        wait_for("action dropped on close", || {
            NEVER_DROPPED.load(Ordering::SeqCst) == 2
        })
        .await;
    }

    #[tokio::test]
    async fn session_action_outside_the_runner_closes_without_running() {
        let (peer, mut raw) = pair().await;
        raw.send(json!({"type": "call", "id": "r:0", "ext": "z", "method": "echo", "args": {"session": "s"}}))
            .await;
        raw.expect_eof().await;
        closes(&peer).await;
        peer.join().await;
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
                "unknown type",
                bytes(&json!({"type": "shout", "id": "r:0"})),
            ),
            (
                "parent takes no callbacks",
                bytes(
                    &json!({"type": "call", "id": "r:0", "ext": "a", "method": "progress", "args": {"message": "x"}, "parent": "h:0"}),
                ),
            ),
            (
                "parent never issued",
                bytes(
                    &json!({"type": "call", "id": "r:0", "ext": "a", "method": "progress", "args": {}, "parent": "h:99"}),
                ),
            ),
            (
                "cancel of a host call",
                bytes(&json!({"type": "cancel", "id": "h:0"})),
            ),
            ("reply to unknown host id", bytes(&ok("h:99", Value::Null))),
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
        let callbacks = LoginCallbacks(&ui);
        let (result, ()) = tokio::join!(
            peer.call("a", "login", json!({"provider": "p"}), Some(&callbacks)),
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
    async fn outbound_frames_reserve_encoded_bytes_before_allocation() {
        let (peer, mut raw) = pair().await;
        let held = peer
            .inner
            .budget
            .try_acquire_many(QUEUE_LIMIT as u32)
            .unwrap();
        {
            let mut pending = std::pin::pin!(peer.call("a", "withdrawn", Value::Null, None));
            assert!(futures::poll!(&mut pending).is_pending());
        }
        drop(held);
        let (result, ()) = tokio::join!(peer.call("a", "next", Value::Null, None), async {
            let (id, _, method) = called(&raw.next().await.unwrap());
            assert_eq!(method, "next");
            raw.send(ok(&id, json!("delivered"))).await;
        });
        assert_eq!(result.unwrap(), "delivered");
    }

    #[test]
    fn counting_body_length_stops_at_the_limit_without_retaining_bytes() {
        let mut count = BodyLength::default();
        let chunk = [0; 4096];
        for _ in 0..MAX_FRAME / chunk.len() {
            count.write_all(&chunk).unwrap();
        }
        count.write_all(&chunk[..MAX_FRAME % chunk.len()]).unwrap();
        assert_eq!(count.0, MAX_FRAME);
        assert!(count.write(&[0]).is_err());
        assert_eq!(count.0, MAX_FRAME);
        assert_eq!(size_of::<BodyLength>(), size_of::<usize>());
    }

    #[tokio::test]
    async fn oversized_callback_answer_is_refused_without_closing() {
        let (peer, mut raw) = pair().await;
        let ui = Ui::default();
        let callbacks = LoginCallbacks(&ui);
        let (login, ()) = tokio::join!(
            peer.call("a", "login", json!({"provider": "p"}), Some(&callbacks)),
            async {
                let (parent, ..) = called(&raw.next().await.unwrap());
                let prompt = |id: &str, message: &str| {
                    json!({"type": "call", "id": id,
                    "parent": parent, "ext": "a", "method": "prompt",
                    "args": {"message": message, "secret": false}})
                };
                raw.send(prompt("r:0", "oversized")).await;
                assert_eq!(
                    raw.next().await.unwrap(),
                    json!({"type": "error", "id": "r:0",
                        "code": "too_large", "message": "host reply exceeds 64 MiB"})
                );
                raw.send(prompt("r:1", "next")).await;
                assert_eq!(raw.next().await.unwrap(), ok("r:1", json!("answer-next")));
                raw.send(ok(&parent, credential("fresh"))).await;
            }
        );
        assert_eq!(login.unwrap()["access"], "fresh");
    }

    #[tokio::test]
    async fn oversized_outgoing_call_fails_locally() {
        let (peer, mut raw) = pair().await;
        let huge = "x".repeat(MAX_FRAME);
        let result = peer
            .call("a", "rewrite", json!({"provider": "p", "model": {}, "request": {"url": "http://x/", "headers": [], "body": {"big": huge}}}), None)
            .await;
        assert!(matches!(result, Err(CallError::Invalid)), "{result:?}");
        // Nothing was written for it, and the connection is healthy.
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
        let callbacks = LoginCallbacks(&ui);
        let (result, ()) = tokio::join!(
            peer.call("a", "login", json!({"provider": "p"}), Some(&callbacks)),
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
