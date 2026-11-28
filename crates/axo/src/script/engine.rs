use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use axo_ai::JsonMap;
use futures::{StreamExt, stream::FuturesUnordered};
use indexmap::IndexMap;
use rquickjs::{
    Array, Context, Ctx, Error, Exception, Function, Persistent, Promise, Runtime,
    String as JsString, Value as JsValue, context::EvalOptions,
};
use serde_json::Value;
use tokio::sync::{Notify, Semaphore, oneshot};
use tokio_util::sync::CancellationToken;

use super::{Limits, ScriptEnd, ScriptError, ScriptHost, StoreChanges};

// ponytail: in-process QuickJS. Move this module behind a supervised subprocess
// or wasm if engine memory safety or non-interruptible builtins become a problem.
const THREAD_STACK: usize = 8 << 20;
const SOURCE: &str = "axo-script.js";
const RESULT_TOO_LARGE: &str = "Result too large";
const CANCELLED: &str = "Script cancelled";
static SLOTS: Semaphore = Semaphore::const_new(4);

pub(super) async fn run(
    code: &str,
    store: BTreeMap<String, Value>,
    host: Arc<dyn ScriptHost>,
    limits: Limits,
    cancel: CancellationToken,
) -> ScriptEnd {
    let slot = tokio::select! {
        biased;
        () = cancel.cancelled() => return failed(CANCELLED),
        slot = SLOTS.acquire() => slot.expect("script semaphore stays open"),
    };
    if !(1..=u32::MAX as usize).contains(&limits.max_in_flight)
        || limits.max_in_flight > Semaphore::MAX_PERMITS
        || limits.stack_bytes == 0
        || limits.stack_bytes > THREAD_STACK / 2
    {
        return failed("Invalid script stack or in-flight limit");
    }
    let cancel = cancel.child_token();
    // If the caller drops run, the driver still stops and drains the calls it started.
    let _stop_on_drop = cancel.clone().drop_guard();
    let (request_tx, request_rx) = mpsc::sync_channel(limits.max_in_flight.saturating_mul(2));
    let (replies, reply_rx) = mpsc::channel();
    let (end_tx, end_rx) = oneshot::channel();
    let ready = Arc::new(Notify::new());
    let worker = Worker {
        requests: request_tx,
        ready: ready.clone(),
        replies: reply_rx,
        limits: limits.clone(),
        cancel: cancel.clone(),
        host: host.clone(),
    };
    let code = code.to_owned();
    if let Err(error) = thread::Builder::new()
        .stack_size(THREAD_STACK)
        .spawn(move || {
            let _slot = slot;
            let _ = end_tx.send(worker.execute(&code, store));
        })
    {
        return failed(format!("Could not start script: {error}"));
    }
    tokio::spawn(drive(
        host, limits, cancel, request_rx, ready, replies, end_rx,
    ))
    .await
    .unwrap_or_else(|error| failed(format!("Script host stopped: {error}")))
}

struct Request {
    id: u32,
    op: Operation,
}

enum Operation {
    Call(String, JsonMap),
    Search(String),
    Describe(String),
}

struct Completion {
    id: u32,
    result: Result<String, String>,
}

enum Reply {
    Complete(Completion),
    Stop,
}

async fn drive(
    host: Arc<dyn ScriptHost>,
    limits: Limits,
    cancel: CancellationToken,
    requests: mpsc::Receiver<Request>,
    ready: Arc<Notify>,
    replies: mpsc::Sender<Reply>,
    mut end: oneshot::Receiver<ScriptEnd>,
) -> ScriptEnd {
    // The synchronous receiver is Send but not Sync. Use a mutable reference in
    // the receive future so the driver's task stays Send.
    let mut requests = requests;
    let permits = Semaphore::new(limits.max_in_flight);
    let stopping = AtomicBool::new(false);
    let running = AtomicUsize::new(0);
    let start_call = {
        let (host, permits, stopping, running) = (&host, &permits, &stopping, &running);
        move |id, name: String, args| async move {
            let _permit = permits
                .acquire()
                .await
                .expect("script call semaphore stays open");
            if stopping.load(Relaxed) {
                return None;
            }
            running.fetch_add(1, Relaxed);
            let result = host.call(&name, args).await;
            running.fetch_sub(1, Relaxed);
            Some((id, result))
        }
    };
    let mut calls = FuturesUnordered::new();
    let result = loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                let _ = replies.send(Reply::Stop);
                break None;
            }
            request = receive(&mut requests, &ready) => {
                let id = request.id;
                let memory = limits.memory_bytes;
                match request.op {
                    Operation::Call(name, args) => {
                        calls.push(start_call(id, name, args));
                    }
                    op => {
                        let value = match op {
                            Operation::Search(query) => Ok(host.search(&query)),
                            Operation::Describe(name) => host.describe(&name),
                            Operation::Call(..) => unreachable!(),
                        };
                        let _ = replies.send(Reply::Complete(Completion { id, result: wire(value, memory) }));
                    }
                }
            }
            Some(Some((id, result))) = calls.next(), if !calls.is_empty() => {
                let _ = replies.send(Reply::Complete(Completion { id, result: wire(result, limits.memory_bytes) }));
            }
            result = &mut end => break Some(result),
        }
    };
    let result = match result {
        Some(Ok(mut end)) if end.error.is_none() => {
            // The VM has stopped publishing. A send could have raced the select's
            // final receive poll, so take any calls it dispatched before returning.
            for Request { id, op } in requests.try_iter() {
                if let Operation::Call(name, args) = op {
                    calls.push(start_call(id, name, args));
                }
            }
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    end.error = Some(problem(CANCELLED));
                    end.store = StoreChanges::default();
                }
                () = async { while calls.next().await.is_some() {} } => return end,
            }
            Some(Ok(end))
        }
        result => result,
    };
    // On failure or cancellation, stop active calls without starting any still queued.
    stopping.store(true, Relaxed);
    if running.load(Relaxed) != 0 {
        host.abandon();
    }
    while calls.next().await.is_some() {}
    match result {
        Some(result) => result,
        None => end.await,
    }
    .unwrap_or_else(|_| failed("Script worker stopped unexpectedly"))
}

async fn receive(rx: &mut mpsc::Receiver<Request>, ready: &Notify) -> Request {
    loop {
        if let Ok(request) = rx.try_recv() {
            return request;
        }
        ready.notified().await;
    }
}

struct Worker {
    requests: mpsc::SyncSender<Request>,
    ready: Arc<Notify>,
    replies: mpsc::Receiver<Reply>,
    limits: Limits,
    cancel: CancellationToken,
    host: Arc<dyn ScriptHost>,
}

type HeldValue = Persistent<JsValue<'static>>;

struct DetachRejectionTracker<'a>(&'a Runtime);

impl Drop for DetachRejectionTracker<'_> {
    fn drop(&mut self) {
        self.0.set_host_promise_rejection_tracker(None);
    }
}

impl Worker {
    fn execute(self, code: &str, store: BTreeMap<String, Value>) -> ScriptEnd {
        let meter = Rc::new(Meter::new(self.limits.cpu, self.cancel.clone()));
        let calls = Rc::new(Cell::new(0));
        let result = (|| {
            let store = Rc::new(RefCell::new(
                Store::new(store, &self.limits).map_err(problem)?,
            ));
            self.eval(code, &meter, &calls, &store)?;
            Ok(store.take().changes())
        })();
        let (error, store) = match meter.check().and(result) {
            Ok(store) => (None, store),
            Err(error) => (Some(error), StoreChanges::default()),
        };
        ScriptEnd {
            error,
            store,
            calls: calls.get(),
            cpu: meter.used.get(),
        }
    }

    fn eval(
        self,
        code: &str,
        meter: &Rc<Meter>,
        calls: &Rc<Cell<u32>>,
        store: &Rc<RefCell<Store>>,
    ) -> Result<(), ScriptError> {
        let rt = Runtime::new().map_err(|_| self.oom())?;
        rt.set_max_stack_size(self.limits.stack_bytes);
        // Build the trusted context before setting a caller-controlled heap cap.
        // In this engine release, context creation under OOM has a known bug.
        let context = Context::full(&rt).map_err(|_| self.oom())?;
        if rt.memory_usage().malloc_size as usize >= self.limits.memory_bytes {
            return Err(self.oom());
        }
        rt.set_memory_limit(self.limits.memory_bytes);
        let interrupt = meter.clone();
        rt.set_interrupt_handler(Some(Box::new(move || interrupt.exhausted())));
        let rejections = Rc::new(RefCell::new(IndexMap::<HeldValue, HeldValue>::new()));
        let tracker = rejections.clone();
        rt.set_host_promise_rejection_tracker(Some(Box::new(
            move |cx, promise, reason, handled| {
                let key = Persistent::save(&cx, promise);
                if handled {
                    tracker.borrow_mut().shift_remove(&key);
                } else {
                    tracker
                        .borrow_mut()
                        .insert(key, Persistent::save(&cx, reason));
                }
            },
        )));
        // Detach the tracker and drop its JS roots before the runtime, including
        // when a trusted host callback panics.
        let _detach_tracker = DetachRejectionTracker(&rt);
        let (settle, print, promise) = meter.slice(|| {
            context.with(|cx| {
                let initialized = (|| {
                    let (settle, print) = self.prelude(&cx, calls, store)?;
                    let mut options = EvalOptions::default();
                    options.filename = Some(SOURCE.into());
                    let promise: Promise =
                        cx.eval_with_options(format!("(async () => {{\n{code}\n}})()"), options)?;
                    Ok((
                        Persistent::save(&cx, settle),
                        Persistent::save(&cx, print),
                        Persistent::save(&cx, promise),
                    ))
                })();
                initialized.map_err(|error| self.js_error(&cx, error))
            })
        })?;
        loop {
            meter.slice(|| {
                loop {
                    match rt.execute_pending_job() {
                        Ok(true) => meter.check()?,
                        Ok(false) => return Ok(()),
                        Err(job) => return Err(job.0.with(|cx| self.exception(cx.catch()))),
                    }
                }
            })?;
            let value = meter.slice(|| {
                context.with(|cx| {
                    let promise = promise
                        .clone()
                        .restore(&cx)
                        .map_err(|error| self.js_error(&cx, error))?;
                    match promise.result::<JsValue>() {
                        Some(Err(error)) => Err(self.js_error(&cx, error)),
                        result => {
                            let orphan = rejections
                                .borrow()
                                .first()
                                .map(|(_, reason)| reason.clone());
                            if let Some(reason) = orphan {
                                let value = reason
                                    .restore(&cx)
                                    .map_err(|error| self.js_error(&cx, error))?;
                                return Err(self.exception(value));
                            }
                            result
                                .transpose()
                                .map(|value| value.map(|value| Persistent::save(&cx, value)))
                                .map_err(|error| self.js_error(&cx, error))
                        }
                    }
                })
            })?;
            if let Some(value) = value {
                return meter.slice(|| {
                    context.with(|cx| {
                        let output = (|| {
                            let value = value.restore(&cx)?;
                            if !value.is_undefined() {
                                print.restore(&cx)?.call::<_, ()>((value,))?;
                            }
                            Ok(())
                        })();
                        output.map_err(|error| self.js_error(&cx, error))
                    })
                });
            }
            let Ok(Reply::Complete(completion)) = self.replies.recv() else {
                return Err(problem(CANCELLED));
            };
            meter.slice(|| {
                context.with(|cx| {
                    let settled = (|| {
                        let settle = settle.clone().restore(&cx)?;
                        let (ok, json) = match completion.result {
                            Ok(json) => (true, json),
                            Err(error) => (false, error),
                        };
                        // If even the JSON text cannot fit, use the prelude's
                        // preallocated error string rather than allocate another.
                        match JsString::from_str(cx.clone(), &json) {
                            Ok(json) => settle.call((completion.id, ok, Some(json))),
                            Err(Error::Exception) => {
                                cx.catch();
                                settle.call::<_, ()>((completion.id, false, None::<JsString>))
                            }
                            Err(error) => Err(error),
                        }
                    })();
                    settled.map_err(|error| self.js_error(&cx, error))
                })
            })?;
        }
    }

    fn prelude<'js>(
        &self,
        cx: &Ctx<'js>,
        calls: &Rc<Cell<u32>>,
        store: &Rc<RefCell<Store>>,
    ) -> rquickjs::Result<(Function<'js>, Function<'js>)> {
        let prelude: Function = cx.eval(include_str!("prelude.js"))?;
        let limits = &self.limits;
        let max_calls = limits.max_calls;
        let requests = self.requests.clone();
        let ready = self.ready.clone();
        let calls = calls.clone();
        let send = Function::new(
            cx.clone(),
            move |cx: Ctx<'js>, id: u32, op: String, json: String| {
                if calls.get() >= max_calls {
                    return Err(Exception::throw_message(
                        &cx,
                        &format!("Tool call limit reached ({max_calls})"),
                    ));
                }
                let op = match op.as_str() {
                    "call" => {
                        serde_json::from_str(&json).map(|(name, args)| Operation::Call(name, args))
                    }
                    "search" => serde_json::from_str(&json).map(Operation::Search),
                    "describe" => serde_json::from_str(&json).map(Operation::Describe),
                    _ => unreachable!("only the prelude can submit requests"),
                }
                .map_err(|_| {
                    Exception::throw_type(&cx, "Tool arguments must encode a JSON object")
                })?;
                requests
                    .try_send(Request { id, op })
                    .map_err(|_| Exception::throw_message(&cx, "Script bridge closed"))?;
                calls.set(calls.get() + 1);
                ready.notify_one();
                Ok(())
            },
        )?;
        let host = self.host.clone();
        let cancel = self.cancel.clone();
        let (max_bytes, max_items) = (limits.max_output_bytes, limits.max_output_items);
        let mut bytes = 0;
        let mut items = 0;
        let emit = Function::new(
            cx.clone(),
            rquickjs::function::MutFn::new(move |cx: Ctx<'js>, text: String| {
                if cancel.is_cancelled() {
                    return Err(Exception::throw_message(&cx, CANCELLED));
                }
                if items >= max_items || text.len() > max_bytes - bytes {
                    return Err(Exception::throw_message(&cx, "Output limit exceeded"));
                }
                bytes += text.len();
                items += 1;
                host.output(&text);
                Ok(())
            }),
        )?;
        let data = store.clone();
        let read = Function::new(cx.clone(), move |key: String| {
            data.borrow().values.get(&key).cloned()
        })?;
        let (max_value, max_total) = (limits.store_value_bytes, limits.store_total_bytes);
        let data = store.clone();
        let write = Function::new(
            cx.clone(),
            move |cx: Ctx<'js>, key: String, json: Option<String>| {
                data.borrow_mut()
                    .write(key, json, max_value, max_total)
                    .map_err(|message| Exception::throw_message(&cx, message))
            },
        )?;
        let functions: Array = prelude.call((
            send,
            emit,
            read,
            write,
            limits.max_in_flight as u32,
            max_calls,
        ))?;
        Ok((functions.get(0)?, functions.get(1)?))
    }

    fn js_error(&self, cx: &Ctx<'_>, error: Error) -> ScriptError {
        match error {
            Error::Exception => self.exception(cx.catch()),
            Error::Allocation => self.oom(),
            other => problem(other.to_string()),
        }
    }

    fn exception(&self, value: JsValue<'_>) -> ScriptError {
        let (message, stack) = match value.as_exception() {
            Some(error) => (
                error
                    .message()
                    .unwrap_or_else(|| "Script threw an Error".into()),
                error.stack(),
            ),
            None => (
                value
                    .as_string()
                    .and_then(|string| string.to_string().ok())
                    .unwrap_or_else(|| "Script threw a non-Error value".into()),
                None,
            ),
        };
        if message == "out of memory" {
            return self.oom();
        }
        let mut line = None;
        let stack = stack.map(|stack| {
            stack
                .lines()
                .map(|frame| {
                    let label = format!("{SOURCE}:");
                    if let Some((before, rest)) = frame.split_once(&label) {
                        let len = rest.bytes().take_while(u8::is_ascii_digit).count();
                        if let Ok(wrapped) = rest[..len].parse::<u32>() {
                            let number = wrapped.saturating_sub(1);
                            if number != 0 && line.is_none() {
                                line = Some(number);
                            }
                            return format!("{before}{label}{number}{}", &rest[len..]);
                        }
                    }
                    frame.to_owned()
                })
                .collect::<Vec<_>>()
                .join("\n")
        });
        ScriptError {
            message,
            line,
            stack,
        }
    }

    fn oom(&self) -> ScriptError {
        problem(format!(
            "Script ran out of memory ({} MiB limit)",
            self.limits.memory_bytes as f64 / (1 << 20) as f64
        ))
    }
}

struct Meter {
    cancel: CancellationToken,
    limit: Duration,
    used: Cell<Duration>,
    start: Cell<Option<Instant>>,
}

impl Meter {
    fn new(limit: Duration, cancel: CancellationToken) -> Self {
        Self {
            cancel,
            limit,
            used: Cell::default(),
            start: Cell::default(),
        }
    }

    fn elapsed(&self) -> Duration {
        self.used.get().saturating_add(
            self.start
                .get()
                .map_or(Duration::ZERO, |start| start.elapsed()),
        )
    }

    fn exhausted(&self) -> bool {
        self.cancel.is_cancelled() || self.elapsed() >= self.limit
    }

    fn check(&self) -> Result<(), ScriptError> {
        if self.cancel.is_cancelled() {
            Err(problem(CANCELLED))
        } else if self.elapsed() >= self.limit {
            Err(problem(format!(
                "Script exceeded its CPU limit of {} s",
                self.limit.as_secs_f64()
            )))
        } else {
            Ok(())
        }
    }

    fn slice<T>(&self, work: impl FnOnce() -> Result<T, ScriptError>) -> Result<T, ScriptError> {
        self.check()?;
        self.start.set(Some(Instant::now()));
        let result = work();
        self.used.set(self.elapsed());
        self.start.set(None);
        self.check()?;
        result
    }
}

#[derive(Default)]
struct Store {
    values: BTreeMap<String, String>,
    changes: StoreChanges,
    original: BTreeSet<String>,
    deleted: BTreeSet<String>,
    bytes: usize,
}

impl Store {
    fn new(values: BTreeMap<String, Value>, limits: &Limits) -> Result<Self, &'static str> {
        let mut store = Self::default();
        for (key, value) in values {
            let json = bounded_json(&value, limits.store_value_bytes)
                .ok_or("Store value limit exceeded")?;
            store.original.insert(key.clone());
            store.replace(
                key,
                Some(json),
                limits.store_value_bytes,
                limits.store_total_bytes,
            )?;
        }
        Ok(store)
    }

    fn write(
        &mut self,
        key: String,
        json: Option<String>,
        max_value: usize,
        max_total: usize,
    ) -> Result<(), &'static str> {
        let value = self.replace(key.clone(), json, max_value, max_total)?;
        if let Some(value) = value {
            self.deleted.remove(&key);
            self.changes.set.insert(key, value);
        } else {
            self.changes.set.remove(&key);
            if self.original.contains(&key) {
                self.deleted.insert(key);
            }
        }
        Ok(())
    }

    fn replace(
        &mut self,
        key: String,
        json: Option<String>,
        max_value: usize,
        max_total: usize,
    ) -> Result<Option<Value>, &'static str> {
        let old = self
            .values
            .get(&key)
            .map_or(0, |value| key.len() + value.len());
        let bytes = self.bytes - old;
        let value = if let Some(json) = json {
            if json.len() > max_value {
                return Err("Store value limit exceeded");
            }
            let size = key.len().saturating_add(json.len());
            if size > max_total - bytes {
                return Err("Store total limit exceeded");
            }
            let value = serde_json::from_str(&json).map_err(|_| "Value is not serializable")?;
            self.bytes = bytes + size;
            self.values.insert(key, json);
            Some(value)
        } else {
            self.values.remove(&key);
            self.bytes = bytes;
            None
        };
        Ok(value)
    }

    fn changes(self) -> StoreChanges {
        StoreChanges {
            delete: self.deleted.into_iter().collect(),
            ..self.changes
        }
    }
}

fn wire(value: Result<Value, String>, limit: usize) -> Result<String, String> {
    match value {
        Ok(value) => bounded_json(&value, limit).ok_or_else(|| RESULT_TOO_LARGE.into()),
        Err(error) if error.len() > limit => Err(RESULT_TOO_LARGE.into()),
        Err(error) => Err(error),
    }
}

fn bounded_json(value: &Value, limit: usize) -> Option<String> {
    struct Buffer(Vec<u8>, usize);
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.1 - self.0.len() {
                return Err(io::Error::other("JSON byte limit exceeded"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Buffer(Vec::new(), limit);
    serde_json::to_writer(&mut writer, value).ok()?;
    String::from_utf8(writer.0).ok()
}

fn problem(message: impl Into<String>) -> ScriptError {
    ScriptError {
        message: message.into(),
        line: None,
        stack: None,
    }
}

fn failed(message: impl Into<String>) -> ScriptEnd {
    ScriptEnd {
        error: Some(problem(message)),
        store: StoreChanges::default(),
        calls: 0,
        cpu: Duration::ZERO,
    }
}
