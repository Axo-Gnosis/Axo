//! The session journal under concurrency: writes in flight never wedge the run, a steer,
//! an attach or a job's own records, and maintenance commits only on the transcript it
//! read. Every wait has a hard deadline; a hang fails the test.

mod common;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axo::store::{Entry, Header, JsonlStore, SeqEntry, SessionInfo, SessionStore, SessionWriter};
use axo::{Axo, Command, JobStatus, Session, SessionEvent};
use axo_agent::RunOutcome;
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{
    AssistantEvent, Message, Model, ScriptedApi, ScriptedTurn, SimpleStreamOptions, Streamer,
    Transcript, UserBlock,
};
use common::{bash_options, journal, only, within};
use futures::stream::{self, BoxStream, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;

/// Holds the first append matching `stops` until released.
struct WriteGate {
    stops: fn(&Entry) -> bool,
    armed: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl WriteGate {
    fn new(stops: fn(&Entry) -> bool) -> Arc<Self> {
        Arc::new(Self {
            stops,
            armed: AtomicBool::new(true),
            entered: Notify::new(),
            release: Notify::new(),
        })
    }
}

struct GatedWriter {
    inner: Box<dyn SessionWriter>,
    gate: Arc<WriteGate>,
}

#[async_trait]
impl SessionWriter for GatedWriter {
    async fn append(&mut self, entry: Entry) -> Result<SeqEntry, axo::Error> {
        if (self.gate.stops)(&entry) && self.gate.armed.swap(false, Ordering::AcqRel) {
            self.gate.entered.notify_one();
            self.gate.release.notified().await;
        }
        self.inner.append(entry).await
    }

    fn path(&self) -> Option<&Path> {
        self.inner.path()
    }

    fn planned_path(&self) -> &Path {
        self.inner.planned_path()
    }
}

/// A `JsonlStore` whose writers stop at `gate`.
struct GatedStore {
    inner: JsonlStore,
    gate: Arc<WriteGate>,
}

#[async_trait]
impl SessionStore for GatedStore {
    async fn create(&self, header: Header) -> Result<Box<dyn SessionWriter>, axo::Error> {
        Ok(Box::new(GatedWriter {
            inner: self.inner.create(header).await?,
            gate: self.gate.clone(),
        }))
    }

    async fn open_write(
        &self,
        path: &Path,
    ) -> Result<(Header, Vec<SeqEntry>, Box<dyn SessionWriter>), axo::Error> {
        let (header, entries, inner) = self.inner.open_write(path).await?;
        let writer = GatedWriter {
            inner,
            gate: self.gate.clone(),
        };
        Ok((header, entries, Box::new(writer)))
    }

    async fn open_read(&self, path: &Path) -> Result<(Header, Vec<SeqEntry>), axo::Error> {
        self.inner.open_read(path).await
    }

    async fn read_header(&self, path: &Path) -> Result<Header, axo::Error> {
        self.inner.read_header(path).await
    }

    async fn list(&self, cwd: &Path) -> Result<Vec<SessionInfo>, axo::Error> {
        self.inner.list(cwd).await
    }
}

/// Streams from `api`, holding request number `at` (0-based) until released.
struct GatedStreamer {
    api: Arc<ScriptedApi>,
    at: usize,
    count: AtomicUsize,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl Streamer for GatedStreamer {
    fn stream(
        &self,
        model: &Model,
        transcript: Transcript,
        opts: SimpleStreamOptions,
        cancel: CancellationToken,
    ) -> BoxStream<'static, AssistantEvent> {
        let events = self.api.stream(model, transcript, opts, cancel);
        if self.count.fetch_add(1, Ordering::AcqRel) != self.at {
            return events;
        }
        let (entered, release) = (self.entered.clone(), self.release.clone());
        stream::once(async move {
            entered.notify_one();
            release.notified().await;
            events
        })
        .flatten()
        .boxed()
    }
}

/// An `Axo` over `store` with one model `test/m`.
async fn build(
    dir: &Path,
    settings: Value,
    streamer: Arc<dyn Streamer>,
    store: Arc<dyn SessionStore>,
) -> Axo {
    common::axo_with(
        dir,
        json!([{"id": "m", "contextWindow": 100_000, "maxTokens": 4096}]),
        settings,
        store,
        streamer,
    )
    .await
    .0
}

/// An `Axo` streaming `turns` whose writers stop at `gate`.
async fn gated_writes(
    dir: &Path,
    turns: Vec<ScriptedTurn>,
    gate: &Arc<WriteGate>,
) -> (Axo, Arc<ScriptedApi>) {
    let api = Arc::new(ScriptedApi::new(turns));
    let store = Arc::new(GatedStore {
        inner: JsonlStore::new(dir.join("sessions")),
        gate: gate.clone(),
    });
    (build(dir, json!({}), api.clone(), store).await, api)
}

fn admits(entry: &Entry) -> bool {
    matches!(entry, Entry::Message { admitted_jobs, .. } if !admitted_jobs.is_empty())
}

fn names(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Checks each admitted job: one `job_start`, then its admitting result, then one
/// `job_end`, then one delivery receipt.
fn assert_jobs_journaled(lines: &[Value]) -> Vec<String> {
    let admitted: Vec<String> = lines
        .iter()
        .flat_map(|line| names(&line["admittedJobs"]))
        .collect();
    for job in &admitted {
        let start = only(lines, "job_start", |v| {
            v["type"] == "job_start" && v["jobId"] == job.as_str()
        });
        let admitting = only(lines, "admitting entry", |v| {
            names(&v["admittedJobs"]).contains(job)
        });
        let end = only(lines, "job_end", |v| {
            v["type"] == "job_end" && v["jobId"] == job.as_str()
        });
        let delivered = only(lines, "delivery receipt", |v| {
            names(&v["deliveredJobs"]).contains(job)
        });
        assert!(
            start < admitting && admitting < end && end < delivered,
            "{job}: {lines:#?}"
        );
    }
    admitted
}

/// Waits until every admitted job's result is delivered and the session is idle.
async fn await_settled(session: &Session) {
    within(async {
        loop {
            let attach = session.attach(None).await;
            let mut admitted = HashSet::new();
            let mut delivered = HashSet::new();
            for seq in &attach.missed {
                if let Entry::Message {
                    admitted_jobs,
                    delivered_jobs,
                    ..
                } = &seq.entry
                {
                    admitted.extend(admitted_jobs.iter().cloned());
                    delivered.extend(delivered_jobs.iter().cloned());
                }
            }
            if admitted.is_subset(&delivered) && !attach.live.agent.running {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
}

/// Every text the model saw in `transcript`.
fn texts(transcript: &Transcript) -> Vec<String> {
    transcript
        .iter()
        .flat_map(|message| match message {
            Message::User(user) => user.content.clone(),
            Message::ToolResult(result) => result.content.clone(),
            _ => Vec::new(),
        })
        .filter_map(|block| match block {
            UserBlock::Text { text } => Some(text),
            UserBlock::Image { .. } => None,
        })
        .collect()
}

/// Waits for `path` to exist.
async fn await_file(path: &Path) {
    within(async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
}

/// Waits for an appended entry matching `pred`.
async fn await_entry(events: &mut broadcast::Receiver<SessionEvent>, pred: fn(&Entry) -> bool) {
    within(async {
        loop {
            match events.recv().await {
                Ok(SessionEvent::Entry(seq)) if pred(&seq.entry) => return,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("session closed"),
            }
        }
    })
    .await;
}

#[tokio::test]
async fn parallel_async_jobs_settle_and_reach_the_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[
                (
                    "c1",
                    "bash",
                    json!({"command": "echo marker-one", "async": true}),
                ),
                (
                    "c2",
                    "bash",
                    json!({"command": "echo marker-two", "async": true}),
                ),
            ])),
            ScriptedTurn::Reply(reply_text("first")),
            ScriptedTurn::Reply(reply_text("second")),
            ScriptedTurn::Reply(reply_text("third")),
        ],
    )
    .await;
    let session = host
        .axo
        .create_session(bash_options(dir.path()))
        .await
        .unwrap();

    assert_eq!(
        within(session.prompt("Run both.")).await.unwrap(),
        RunOutcome::Done
    );
    await_settled(&session).await;
    session.shutdown().await;

    // Whether each call finished in the foreground or as a job, its result is in the
    // context of the last request.
    let lines = journal(&session);
    let admitted = assert_jobs_journaled(&lines);
    let seen = texts(&host.api.requests().last().unwrap().transcript).join("\n");
    for (call, marker) in [("c1", "marker-one"), ("c2", "marker-two")] {
        let result = lines
            .iter()
            .find(|v| v["message"]["toolCallId"] == call)
            .unwrap_or_else(|| panic!("no result for {call}: {lines:#?}"));
        match names(&result["admittedJobs"]).first() {
            Some(job) => assert!(
                seen.contains(&format!("[{job}] echo {marker}\n{marker}")),
                "{job}: {seen}"
            ),
            None => assert!(seen.contains(marker), "{call}: {seen}"),
        }
    }
    assert!(admitted.len() <= 2, "{admitted:?}");
}

#[tokio::test]
async fn a_job_ending_while_its_admission_persists_journals_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let gate = WriteGate::new(admits);
    let (axo, _api) = gated_writes(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c1",
                "bash",
                json!({"command": "touch finished; echo done", "async": true}),
            )])),
            ScriptedTurn::Reply(reply_text("started it")),
            ScriptedTurn::Reply(reply_text("it printed done")),
        ],
        &gate,
    )
    .await;
    let options = bash_options(dir.path());
    let finished = options.cwd.join("finished");
    let session = Arc::new(axo.create_session(options).await.unwrap());
    let prompt = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("Run it in the background.").await }
    });

    // The admitting batch is in flight while the command finishes.
    within(gate.entered.notified()).await;
    await_file(&finished).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let missed = within(session.attach(None)).await.missed;
    assert!(
        !missed
            .iter()
            .any(|seq| matches!(seq.entry, Entry::JobEnd { .. } | Entry::JobStart { .. })),
        "nothing of the job is visible before its batch commits: {missed:#?}"
    );
    // The result is visible at once, though its `job_end` waits for the admission.
    assert!(
        session
            .jobs()
            .iter()
            .any(|job| job.id == "bg_1" && job.status == JobStatus::Finished),
        "{:?}",
        session.jobs()
    );

    gate.release.notify_one();
    assert_eq!(within(prompt).await.unwrap().unwrap(), RunOutcome::Done);
    await_settled(&session).await;
    session.shutdown().await;
    let lines = journal(&session);
    assert_eq!(assert_jobs_journaled(&lines), vec!["bg_1".to_owned()]);
    let end = lines.iter().position(|v| v["type"] == "job_end").unwrap();
    assert!(
        lines[end]["text"].as_str().unwrap().contains("done"),
        "{lines:#?}"
    );
}

#[tokio::test]
async fn a_steer_during_a_batch_is_accepted_and_drained() {
    let dir = tempfile::tempdir().unwrap();
    let gate = WriteGate::new(admits);
    let (axo, api) = gated_writes(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c1",
                "bash",
                json!({"command": "sleep 0.2; echo late", "async": true}),
            )])),
            ScriptedTurn::Reply(reply_text("ok")),
            ScriptedTurn::Reply(reply_text("ok again")),
            ScriptedTurn::Reply(reply_text("and again")),
        ],
        &gate,
    )
    .await;
    let session = Arc::new(axo.create_session(bash_options(dir.path())).await.unwrap());
    let prompt = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("Run it in the background.").await }
    });

    within(gate.entered.notified()).await;
    let steer = tokio::spawn({
        let session = session.clone();
        async move { session.steer("hi").await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.release.notify_one();
    within(steer).await.unwrap().unwrap();
    assert_eq!(within(prompt).await.unwrap().unwrap(), RunOutcome::Done);

    let requests = api.requests();
    assert!(
        requests[1..]
            .iter()
            .any(|request| texts(&request.transcript).iter().any(|text| text == "hi")),
        "the steer never reached the model"
    );
    await_settled(&session).await;
    session.shutdown().await;
    assert_jobs_journaled(&journal(&session));
}

#[tokio::test]
async fn attach_returns_while_a_write_is_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let gate = WriteGate::new(|entry| matches!(entry, Entry::Submission { .. }));
    let (axo, _api) = gated_writes(
        dir.path(),
        vec![ScriptedTurn::Reply(reply_text("hello"))],
        &gate,
    )
    .await;
    let session = Arc::new(
        axo.create_session(common::options(dir.path()))
            .await
            .unwrap(),
    );
    let prompt = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("hi").await }
    });

    within(gate.entered.notified()).await;
    let attach = tokio::time::timeout(Duration::from_secs(1), session.attach(None))
        .await
        .expect("attach waited on the write");
    assert!(
        !attach
            .missed
            .iter()
            .any(|seq| matches!(seq.entry, Entry::Submission { .. })),
        "the submission is not written yet"
    );
    assert!(attach.live.agent.running, "the prompt owns the session");

    gate.release.notify_one();
    assert_eq!(within(prompt).await.unwrap().unwrap(), RunOutcome::Done);
    session.shutdown().await;
}

/// A session in `<dir>/work` whose summary request (request 2) waits for release; its
/// first prompt leaves `bg_1` running until `<dir>/work/go` exists. With no recent tail
/// kept, compaction cuts at the final reply: the long prompt, the call and its result
/// form one split-turn prefix (one summary request per attempt) that a short summary
/// replaces, so the projected context shrinks.
async fn compaction_with_running_job(
    dir: &Path,
    summaries: [&str; 2],
) -> (Session, Arc<ScriptedApi>, Arc<Notify>, Arc<Notify>, PathBuf) {
    let api = Arc::new(ScriptedApi::new(vec![
        ScriptedTurn::Reply(reply_tool_calls(&[(
            "c1",
            "bash",
            json!({"command": "while [ ! -e go ]; do sleep 0.02; done; echo done", "async": true}),
        )])),
        ScriptedTurn::Reply(reply_text("started it")),
        ScriptedTurn::Reply(reply_text(summaries[0])),
        ScriptedTurn::Reply(reply_text(summaries[1])),
        ScriptedTurn::Reply(reply_text("delivered")),
    ]));
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let streamer = Arc::new(GatedStreamer {
        api: api.clone(),
        at: 2,
        count: AtomicUsize::new(0),
        entered: entered.clone(),
        release: release.clone(),
    });
    let settings = json!({"compaction": {
        "enabled": false,
        "maxContext": null,
        "reserveTokens": 1024,
        "keepRecentTokens": 0,
        "summaryRequest": "separate",
    }});
    let store = Arc::new(JsonlStore::new(dir.join("sessions")));
    let axo = build(dir, settings, streamer, store).await;
    let options = bash_options(dir);
    let go = options.cwd.join("go");
    let session = axo.create_session(options).await.unwrap();
    assert_eq!(
        within(session.prompt(format!("Start the job. {}", "padding ".repeat(2000))))
            .await
            .unwrap(),
        RunOutcome::Done
    );
    (session, api, entered, release, go)
}

fn compactions(session: &Session) -> Vec<Value> {
    journal(session)
        .into_iter()
        .filter(|v| v["type"] == "compaction")
        .collect()
}

#[tokio::test]
async fn compaction_commit_fails_when_a_job_end_lands_after_prepare() {
    let dir = tempfile::tempdir().unwrap();
    let (session, api, entered, release, go) =
        compaction_with_running_job(dir.path(), ["Summary one.", "Summary two."]).await;
    let session = Arc::new(session);
    let mut events = session.attach(None).await.events;
    let compact = tokio::spawn({
        let session = session.clone();
        async move { session.compact(None).await }
    });

    // Prepared and summarizing; the job ends meanwhile, which moves the revision.
    within(entered.notified()).await;
    std::fs::write(&go, "").unwrap();
    await_entry(&mut events, |entry| matches!(entry, Entry::JobEnd { .. })).await;
    release.notify_one();
    within(compact).await.unwrap().unwrap();

    // The first summary was refused at commit; the retry read the new transcript. (The
    // job's delivery run may already have sent request 5.)
    assert!(api.requests().len() >= 4);
    let compactions = compactions(&session);
    assert_eq!(compactions.len(), 1, "{compactions:#?}");
    let summary = compactions[0]["summary"].as_str().unwrap();
    assert!(summary.contains("Summary two."), "{summary}");
    assert!(!summary.contains("Summary one."), "{summary}");
    let lines = journal(&session);
    let end = lines.iter().position(|v| v["type"] == "job_end").unwrap();
    let checkpoint = lines
        .iter()
        .position(|v| v["type"] == "compaction")
        .unwrap();
    assert!(end < checkpoint, "{lines:#?}");

    await_settled(&session).await;
    session.shutdown().await;
}

#[tokio::test]
async fn compaction_commits_while_a_job_keeps_running() {
    let dir = tempfile::tempdir().unwrap();
    let (session, api, entered, release, _go) =
        compaction_with_running_job(dir.path(), ["Summary one.", "Summary two."]).await;
    let session = Arc::new(session);
    let compact = tokio::spawn({
        let session = session.clone();
        async move { session.compact(None).await }
    });

    within(entered.notified()).await;
    release.notify_one();
    within(compact).await.unwrap().unwrap();

    assert_eq!(api.requests().len(), 3);
    let compactions = compactions(&session);
    assert_eq!(compactions.len(), 1, "{compactions:#?}");
    let summary = compactions[0]["summary"].as_str().unwrap();
    assert!(summary.contains("Summary one."), "{summary}");
    assert!(
        !journal(&session).iter().any(|v| v["type"] == "job_end"),
        "the job is still running"
    );
    session.shutdown().await;
}

/// A selection change is not a transcript change: compaction summarizing meanwhile
/// commits its first summary.
#[tokio::test]
async fn a_model_change_during_compaction_does_not_invalidate_it() {
    let dir = tempfile::tempdir().unwrap();
    let (session, api, entered, release, _go) =
        compaction_with_running_job(dir.path(), ["Summary one.", "Summary two."]).await;
    let session = Arc::new(session);
    let compact = tokio::spawn({
        let session = session.clone();
        async move { session.compact(None).await }
    });

    within(entered.notified()).await;
    within(session.run_command(Command::Model {
        model: "test/m".into(),
    }))
    .await
    .unwrap();
    release.notify_one();
    within(compact).await.unwrap().unwrap();

    assert_eq!(api.requests().len(), 3, "one summary attempt, no retry");
    let compactions = compactions(&session);
    assert_eq!(compactions.len(), 1, "{compactions:#?}");
    let summary = compactions[0]["summary"].as_str().unwrap();
    assert!(summary.contains("Summary one."), "{summary}");
    let lines = journal(&session);
    let change = lines
        .iter()
        .position(|v| v["type"] == "model_change")
        .unwrap();
    let checkpoint = lines
        .iter()
        .position(|v| v["type"] == "compaction")
        .unwrap();
    assert!(change < checkpoint, "{lines:#?}");
    session.shutdown().await;
}

/// Dropping `prompt` while its submission is being written leaves the run to settle on
/// its own, so the session takes the next prompt.
#[tokio::test]
async fn dropping_a_prompt_mid_submission_never_wedges_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let gate = WriteGate::new(|entry| matches!(entry, Entry::Submission { .. }));
    let (axo, _api) = gated_writes(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_text("first")),
            ScriptedTurn::Reply(reply_text("second")),
        ],
        &gate,
    )
    .await;
    let session = Arc::new(axo.create_session(bash_options(dir.path())).await.unwrap());
    let task = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("one").await }
    });
    within(gate.entered.notified()).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    gate.release.notify_one();
    await_settled(&session).await;

    assert_eq!(
        within(session.prompt("two")).await.unwrap(),
        RunOutcome::Done
    );
    let submissions = journal(&session)
        .iter()
        .filter(|v| v["type"] == "submission")
        .count();
    assert_eq!(submissions, 2);
}
