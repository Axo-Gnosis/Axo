//! Async script jobs use the same registry and admission receipts as commands.

// integration: gate this module on `any(test, feature = "codemode")` when the feature lands.
#![allow(dead_code)]

use std::future::{Future, pending};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axo_agent::{ToolContext, ToolError, ToolOutput};
use axo_ai::UserBlock;
use futures::FutureExt;
use serde_json::{Map, Value, json};
use tokio::fs::{File, OpenOptions};
use tokio::sync::{Notify, oneshot};
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use super::command::{
    CommandRunner, JobControl, RegisterCommand, Stop, StopOnDrop, lock, with_status,
};
use super::{CommandExit, JobId, Killer, Promotion};
use crate::schemes::artifact::command_log_tail;
use crate::tools::FileRecord;

pub(crate) struct TaskRequest {
    pub label: String,
    pub run_async: bool,
    /// Zero disables timed promotion.
    pub promote_after: Duration,
    /// The wall-clock deadline stays armed after promotion.
    pub timeout: Option<Duration>,
}

#[derive(Default)]
pub(crate) struct TaskProgress {
    /// Output so far, shown when the call promotes.
    pub text: String,
    /// Records not yet reported on a message.
    pub files: Vec<FileRecord>,
}

pub(crate) struct TaskScope {
    /// Fires on foreground abort, timeout, kill and shutdown. Work must clean up and return.
    pub cancel: CancellationToken,
    /// The append-only output log. Flush it before reading it through the artifact store.
    pub log: File,
    pub progress: Arc<Mutex<TaskProgress>>,
    pub job: String,
    /// The log's artifact id; available before promotion for output shaping.
    pub artifact: u64,
}

#[derive(Default)]
pub(crate) struct TaskEnd {
    /// Result body; the runner appends the stop status line.
    pub text: String,
    pub is_error: bool,
    pub details: Map<String, Value>,
}

impl CommandRunner {
    pub(crate) async fn run_task<W, F>(
        &self,
        cx: &ToolContext,
        request: TaskRequest,
        work: W,
    ) -> Result<ToolOutput, ToolError>
    where
        W: FnOnce(TaskScope) -> F,
        F: Future<Output = TaskEnd> + Send + 'static,
    {
        if cx.cancel.is_cancelled() {
            return Err(ToolError("Script aborted".into()));
        }
        let jobs = &self.jobs;
        let (artifact, path, live) = self
            .artifacts
            .create(&command_log_tail(&cx.call.name, &cx.call.id))
            .await
            .map_err(|err| ToolError(err.to_string()))?;
        let log = match OpenOptions::new().append(true).open(&path).await {
            Ok(log) => log,
            Err(err) => {
                drop(live);
                let _ = tokio::fs::remove_file(&path).await;
                return Err(ToolError(format!("Failed to open script log: {err}")));
            }
        };
        let control = Arc::new(JobControl {
            cancel: CancellationToken::new(),
            stop: Mutex::new(None),
            container: None,
            host_promotable: true,
            promote: Notify::new(),
        });
        let started_at = axo_ai::now_ms();
        let start = Instant::now();
        let id = jobs.register(RegisterCommand {
            command: request.label,
            script: true,
            tool: cx.call.name.clone(),
            cwd: Default::default(),
            call_id: cx.call.id.clone(),
            started_at,
            artifact,
            control: control.clone(),
            output: None,
        });
        let mut abandon = StopOnDrop(Some((jobs, id)));
        let progress = Arc::new(Mutex::new(TaskProgress::default()));
        let (done, mut ended) = oneshot::channel();
        let scope = TaskScope {
            cancel: control.cancel.clone(),
            log,
            progress: progress.clone(),
            job: id.to_string(),
            artifact,
        };
        let work = catch_unwind(AssertUnwindSafe(|| work(scope)));
        let task = {
            let jobs = jobs.clone();
            let control = control.clone();
            let progress = progress.clone();
            let timeout = request.timeout;
            tokio::spawn(async move {
                let work = async {
                    match work {
                        Ok(work) => AssertUnwindSafe(work).catch_unwind().await,
                        Err(panic) => Err(panic),
                    }
                };
                tokio::pin!(work);
                let result = tokio::select! {
                    biased;
                    result = &mut work => result,
                    secs = async {
                        match timeout {
                            Some(timeout) => {
                                sleep_until(start + timeout).await;
                                timeout.as_secs_f64()
                            }
                            None => pending().await,
                        }
                    } => {
                        control.stop(Stop::TimedOut(secs));
                        work.await
                    }
                };
                let mut end = result.unwrap_or_else(|_| TaskEnd {
                    text: "Script task crashed".into(),
                    is_error: true,
                    ..TaskEnd::default()
                });
                let promoted = jobs.finishing(id);
                let stop = *lock(&control.stop);
                let exit = match stop {
                    Some(stop) => {
                        let (exit, status) = stop_status(id, stop);
                        end.text = with_status(end.text, &status);
                        end.is_error = true;
                        exit
                    }
                    None => CommandExit::Exited {
                        code: i32::from(end.is_error),
                    },
                };
                let files = std::mem::take(&mut lock(&progress).files);
                drop(live);
                if !promoted && !end.details.contains_key("fullOutput") {
                    let _ = tokio::fs::remove_file(&path).await;
                }
                if promoted {
                    jobs.command_done(id, end.text, end.is_error, Some(exit), files)
                        .await;
                    jobs.closed(id);
                } else {
                    jobs.closed(id);
                    let output = if stop == Some(Stop::Aborted) {
                        Err(ToolError(end.text))
                    } else {
                        end.details.insert("exit".into(), json!(exit));
                        end.details
                            .insert("durationMs".into(), json!(axo_ai::now_ms() - started_at));
                        end.details.insert("files".into(), json!(files));
                        Ok(output(end.text, end.details, end.is_error))
                    };
                    let _ = done.send(output);
                }
            })
        };
        jobs.set_task(id, task);

        let mut steer = cx.steer.clone();
        let mut aborting = false;
        let mut promotable = true;
        loop {
            tokio::select! {
                biased;
                end = &mut ended => {
                    abandon.0 = None;
                    return end.map_err(|_| ToolError("Script terminated without a result".into()))?;
                }
                () = cx.cancel.cancelled(), if !aborting => {
                    aborting = true;
                    jobs.stop(id, Stop::Aborted);
                }
                cause = async {
                    if request.run_async {
                        Promotion::Async
                    } else {
                        tokio::select! {
                            () = control.promote.notified() => Promotion::Host,
                            () = steer.pending() => Promotion::Steer,
                            () = sleep_until(start + request.promote_after), if !request.promote_after.is_zero() => Promotion::Timer,
                        }
                    }
                }, if promotable && !aborting => {
                    promotable = false;
                    // Keep completion from taking the files before this message takes its share.
                    let mut progress = lock(&progress);
                    if jobs.promote(id, cx.invocation, &cx.call.id, cause) {
                        abandon.0 = None;
                        let name = id.to_string();
                        let artifact = format!("artifact://{artifact}");
                        let snapshot = if progress.text.is_empty() { "(no output yet)" } else { &progress.text };
                        let text = format!(
                            "{snapshot}\n\nScript is still running in the background as {name}. Output so far: {artifact}. Its result is delivered automatically when it finishes; call wait if you have nothing else to do. Inspect with read proc://{name}; stop with kill."
                        );
                        let details = Map::from_iter([
                            ("job".into(), json!(name)),
                            ("fullOutput".into(), json!(artifact)),
                            ("files".into(), json!(std::mem::take(&mut progress.files))),
                        ]);
                        return Ok(output(text, details, false));
                    }
                }
            }
        }
    }
}

fn output(text: String, details: Map<String, Value>, is_error: bool) -> ToolOutput {
    ToolOutput {
        content: vec![UserBlock::Text { text }],
        details: Some(Value::Object(details)),
        is_error,
        ..ToolOutput::default()
    }
}

fn stop_status(id: JobId, stop: Stop) -> (CommandExit, String) {
    match stop {
        Stop::Aborted => (CommandExit::Aborted, "Script aborted".into()),
        Stop::TimedOut(secs) => (
            CommandExit::TimedOut { after_secs: secs },
            format!("Script timed out after {secs} seconds"),
        ),
        Stop::Killed(by) => (
            CommandExit::Killed { by },
            match by {
                Killer::Model => format!("Script was stopped (kill {id})"),
                Killer::Host => "Script was stopped by the user".into(),
            },
        ),
        Stop::Shutdown => (
            CommandExit::Interrupted,
            "Script was interrupted when the session closed".into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::host::{ProcessSupervisor, Supervision};
    use crate::jobs::{JOB_RESULT, JobHost, JobKind, JobRegistry};
    use crate::schemes::artifact::Artifacts;
    use crate::session::journal;
    use crate::store::{Entry, SeqEntry};
    use crate::tools::{FileAccess, FileLedger, LedgerRecord, file_records_in};
    use axo_agent::{
        Agent, AgentConfig, AgentMessage, MessageOrigin, SteerSignal, Tool, ToolInvocationId,
        ToolUpdates,
    };
    use axo_ai::testing::{reply_text, reply_tool_calls, scripted_model};
    use axo_ai::{Message, ScriptedApi, ScriptedTurn, ToolCall, UserMessage};
    use tempfile::TempDir;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::broadcast;
    use tokio::time::timeout;

    #[derive(Default)]
    struct Journal(Mutex<Vec<SeqEntry>>);

    #[async_trait::async_trait]
    impl JobHost for Journal {
        fn results_ready(&self) {}
        async fn journal(&self, entry: Entry) -> Result<(), crate::Error> {
            let mut entries = lock(&self.0);
            let seq = entries.len() as u64;
            entries.push(SeqEntry { seq, entry });
            Ok(())
        }
        fn mail_changed(&self, _: u32) {}
        fn tree_changed(&self) {}
    }

    fn registry(entries: &[SeqEntry], host: &Arc<Journal>) -> (Arc<JobRegistry>, Vec<JobId>) {
        let host: Arc<dyn JobHost> = host.clone();
        let (jobs, unfinished) = JobRegistry::new(
            "owner".into(),
            1,
            entries,
            Arc::default(),
            broadcast::channel(16).0,
        );
        jobs.attach(Arc::downgrade(&host));
        (jobs, unfinished)
    }

    fn fixture() -> (TempDir, Arc<Journal>, CommandRunner) {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(Journal::default());
        let runner = CommandRunner {
            jobs: registry(&[], &host).0,
            artifacts: Arc::new(Artifacts::new(dir.path().to_path_buf())),
            supervisor: ProcessSupervisor::new(Supervision::ProcessGroup),
            ci: false,
            promote_after: Duration::ZERO,
        };
        (dir, host, runner)
    }

    fn context() -> ToolContext {
        ToolContext {
            call: Arc::new(ToolCall {
                id: "script".into(),
                name: "codemode".into(),
                arguments: Default::default(),
                arguments_error: None,
                thought_signature: None,
            }),
            cancel: CancellationToken::new(),
            updates: ToolUpdates::sink(),
            steer: SteerSignal::never(),
            invocation: ToolInvocationId(7),
            parent: None,
            calls: axo_agent::Calls::none(),
        }
    }

    fn request(run_async: bool) -> TaskRequest {
        TaskRequest {
            label: "print('hello')".into(),
            run_async,
            promote_after: Duration::ZERO,
            timeout: None,
        }
    }

    fn file(path: &str) -> FileRecord {
        FileRecord {
            ledger: LedgerRecord {
                path: path.into(),
                sha256: "hash".into(),
                access: FileAccess::Read,
            },
            change: None,
        }
    }

    fn text(content: &[UserBlock]) -> &str {
        content.last().and_then(UserBlock::as_text).unwrap()
    }

    fn id() -> JobId {
        JobId::parse("bg_1").unwrap()
    }

    async fn bounded<T>(future: impl Future<Output = T>) -> T {
        timeout(Duration::from_secs(5), future)
            .await
            .expect("task made no progress")
    }

    async fn closed(jobs: &JobRegistry) {
        let mut changed = jobs.subscribe();
        bounded(async {
            while jobs.open() {
                changed.changed().await.unwrap();
            }
        })
        .await;
    }

    async fn admit(jobs: &JobRegistry, host: &Journal, cx: &ToolContext, expected: Promotion) {
        let mut receipts = jobs.receipts(&MessageOrigin::Tool(cx.invocation));
        let start = receipts.start.take().unwrap();
        assert!(
            matches!(&start, Entry::JobStart { script: true, promotion: Some(cause), .. } if *cause == expected)
        );
        host.journal(start).await.unwrap();
        host.journal(Entry::Message {
            base: journal::base(),
            message: user().message,
            submission_id: None,
            admitted_jobs: std::mem::take(&mut receipts.admitted_jobs),
            delivered_jobs: Vec::new(),
        })
        .await
        .unwrap();
        jobs.settle(receipts, true).await;
    }

    #[tokio::test]
    async fn foreground() {
        let (_dir, host, runner) = fixture();
        let cx = context();
        let record = file("foreground.txt");
        let out = bounded(runner.run_task(&cx, request(false), |scope| {
            let record = record.clone();
            async move {
                lock(&scope.progress).files.push(record);
                TaskEnd {
                    text: "hello".into(),
                    ..TaskEnd::default()
                }
            }
        }))
        .await
        .unwrap();
        assert_eq!(text(&out.content), "hello");
        assert_eq!(file_records_in(out.details.as_ref()), vec![record]);
        assert_eq!(
            out.details.unwrap()["exit"],
            json!({ "type": "exited", "code": 0 })
        );
        assert!(
            runner
                .jobs
                .receipts(&MessageOrigin::Tool(cx.invocation))
                .start
                .is_none()
        );
        assert!(lock(&host.0).is_empty());
        assert!(runner.jobs.lock().jobs.is_empty());
    }

    #[tokio::test]
    async fn async_delivers_files_even_if_work_finishes_before_admission() {
        let (_dir, host, runner) = fixture();
        let cx = context();
        let finish = Arc::new(Notify::new());
        let record = file("background.txt");
        let out = bounded(runner.run_task(&cx, request(true), |mut scope| {
            let finish = finish.clone();
            let record = record.clone();
            async move {
                finish.notified().await;
                assert_eq!(scope.job, "bg_1");
                assert_eq!(scope.artifact, 0);
                scope.log.write_all(b"finished").await.unwrap();
                scope.log.flush().await.unwrap();
                lock(&scope.progress).files.push(record);
                TaskEnd {
                    text: "finished".into(),
                    ..TaskEnd::default()
                }
            }
        }))
        .await
        .unwrap();
        assert!(text(&out.content).contains("Script is still running in the background as bg_1"));
        assert_eq!(out.details.unwrap()["fullOutput"], "artifact://0");
        assert!(matches!(
            runner.jobs.list()[0].kind,
            JobKind::Script {
                promotion: Some(Promotion::Async),
                ..
            }
        ));
        finish.notify_one();
        closed(&runner.jobs).await;
        admit(&runner.jobs, &host, &cx, Promotion::Async).await;
        let entries = lock(&host.0).clone();
        assert!(
            matches!(&entries[2].entry, Entry::JobEnd { files, .. } if files == std::slice::from_ref(&record))
        );
        assert_eq!(
            std::fs::read(runner.artifacts.find(0).unwrap()).unwrap(),
            b"finished"
        );
        let replay_host = Arc::new(Journal::default());
        let (reopened, unfinished) = registry(&entries, &replay_host);
        assert!(unfinished.is_empty());
        for jobs in [&runner.jobs, &reopened] {
            let delivery = jobs.delivery_message().unwrap();
            let AgentMessage::Custom(message) = &delivery.message else {
                panic!("not a custom message")
            };
            assert_eq!(message.custom_type, JOB_RESULT);
            assert!(text(&message.content).ends_with("finished"));
            assert_eq!(
                file_records_in(message.details.as_ref()),
                vec![record.clone()]
            );
            let ledger = FileLedger::default();
            ledger.restore(&[delivery.message]);
            assert_eq!(
                ledger.recorded(std::path::Path::new(&record.ledger.path)),
                Some("hash".into())
            );
            let receipts = jobs.receipts(&MessageOrigin::Input(delivery.id));
            jobs.settle(receipts, true).await;
            assert!(jobs.delivery_message().is_none());
        }
    }

    async fn stopped(scope: TaskScope) -> TaskEnd {
        scope.cancel.cancelled().await;
        TaskEnd {
            text: "stopping".into(),
            ..TaskEnd::default()
        }
    }

    #[tokio::test]
    async fn timer_promotes() {
        let (_dir, host, runner) = fixture();
        let cx = context();
        let request = TaskRequest {
            promote_after: Duration::from_millis(1),
            ..request(false)
        };
        bounded(runner.run_task(&cx, request, stopped))
            .await
            .unwrap();
        admit(&runner.jobs, &host, &cx, Promotion::Timer).await;
        bounded(runner.jobs.shutdown()).await;
    }

    #[tokio::test]
    async fn host_promotes_and_reports_progress() {
        let (_dir, host, runner) = fixture();
        let cx = context();
        let started = Arc::new(Notify::new());
        let record = file("before-promotion.txt");
        let run = runner.run_task(&cx, request(false), |scope| {
            let started = started.clone();
            let record = record.clone();
            async move {
                *lock(&scope.progress) = TaskProgress {
                    text: "hello so far".into(),
                    files: vec![record],
                };
                started.notify_one();
                stopped(scope).await
            }
        });
        let out = bounded(async {
            let (out, ()) = tokio::join!(run, async {
                started.notified().await;
                assert!(runner.jobs.background(&cx.call.id));
            });
            out.unwrap()
        })
        .await;
        assert!(text(&out.content).starts_with("hello so far"));
        assert_eq!(file_records_in(out.details.as_ref()), vec![record]);
        admit(&runner.jobs, &host, &cx, Promotion::Host).await;
        assert!(!runner.jobs.background(&cx.call.id));
        assert!(runner.jobs.kill(id(), Killer::Model));
        closed(&runner.jobs).await;
        let entries = lock(&host.0);
        assert!(matches!(&entries[2].entry, Entry::JobEnd { files, .. } if files.is_empty()));
    }

    // Obtain a real, agent-owned steering signal without exposing a new test-only API.
    struct CaptureContext(Mutex<Option<oneshot::Sender<ToolContext>>>, Value);

    #[async_trait::async_trait]
    impl Tool for CaptureContext {
        fn name(&self) -> &str {
            "codemode"
        }
        fn description(&self) -> &str {
            "Capture the tool context"
        }
        fn parameters(&self) -> &Value {
            &self.1
        }
        async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
            let _ = lock(&self.0).take().unwrap().send(cx);
            Ok(ToolOutput::default())
        }
    }

    fn user() -> axo_agent::QueuedMessage {
        AgentMessage::Llm(Message::User(UserMessage {
            content: vec![UserBlock::Text {
                text: "hello".into(),
            }],
            timestamp: 0,
        }))
        .into()
    }

    #[tokio::test]
    async fn steer_promotes() {
        let (_dir, host, runner) = fixture();
        let api = ScriptedApi::new(vec![
            ScriptedTurn::Reply(reply_tool_calls(&[("script", "codemode", json!({}))])),
            ScriptedTurn::Reply(reply_text("done")),
        ]);
        let mut config = AgentConfig::new(Arc::new(api), scripted_model());
        let (send, recv) = oneshot::channel();
        config.tools = vec![Arc::new(CaptureContext(
            Mutex::new(Some(send)),
            json!({ "type": "object" }),
        ))];
        let agent = Agent::new(config);
        bounded(agent.prompt(vec![user()])).await.unwrap();
        let cx = recv.await.unwrap();
        let started = Notify::new();
        bounded(async {
            let (out, ()) = tokio::join!(
                runner.run_task(&cx, request(false), |scope| {
                    started.notify_one();
                    stopped(scope)
                }),
                async {
                    started.notified().await;
                    agent.steer(user());
                }
            );
            out.unwrap();
        })
        .await;
        admit(&runner.jobs, &host, &cx, Promotion::Steer).await;
        bounded(runner.jobs.shutdown()).await;
    }

    #[tokio::test]
    async fn kill_stops_script() {
        let (_dir, host, runner) = fixture();
        let cx = context();
        bounded(runner.run_task(&cx, request(true), stopped))
            .await
            .unwrap();
        admit(&runner.jobs, &host, &cx, Promotion::Async).await;
        assert!(runner.jobs.kill(id(), Killer::Model));
        closed(&runner.jobs).await;
        let entries = lock(&host.0);
        assert!(matches!(&entries[2].entry, Entry::JobEnd {
            exit: Some(CommandExit::Killed { by: Killer::Model }), text, is_error: true, ..
        } if text == "stopping\n\nScript was stopped (kill bg_1)"));
    }

    #[tokio::test]
    async fn abort_before_promotion() {
        let (_dir, host, runner) = fixture();
        let cx = context();
        let started = Notify::new();
        bounded(async {
            let (out, ()) = tokio::join!(
                runner.run_task(&cx, request(false), |scope| {
                    started.notify_one();
                    stopped(scope)
                }),
                async {
                    started.notified().await;
                    cx.cancel.cancel();
                }
            );
            assert_eq!(out.unwrap_err().0, "stopping\n\nScript aborted");
        })
        .await;
        assert!(runner.jobs.lock().jobs.is_empty());
        assert!(lock(&host.0).is_empty());
    }

    #[tokio::test]
    async fn shutdown_awaits_script_and_reopen_reconciles_it() {
        let (_dir, host, runner) = fixture();
        let cx = context();
        let (cancelled, cancelling) = oneshot::channel();
        let (finish, finishing) = oneshot::channel();
        bounded(runner.run_task(&cx, request(true), |scope| async move {
            scope.cancel.cancelled().await;
            cancelled.send(()).unwrap();
            finishing.await.unwrap();
            TaskEnd::default()
        }))
        .await
        .unwrap();
        let nested = |n: u32, root: &str, record: &FileRecord| Entry::NestedToolCall {
            base: journal::base(),
            root_call_id: root.into(),
            call_id: format!("{root}/{n}"),
            name: "read".into(),
            arguments: Default::default(),
            result: "read".into(),
            is_error: false,
            duration_ms: None,
            details: Some(crate::file_details(std::slice::from_ref(record))),
            admitted_jobs: Vec::new(),
            delivered_jobs: Vec::new(),
        };
        host.journal(nested(1, "script", &file("before.txt")))
            .await
            .unwrap();
        admit(&runner.jobs, &host, &cx, Promotion::Async).await;
        let recovered_files = vec![file("first.txt"), file("second.txt")];
        host.journal(nested(2, "script", &recovered_files[0]))
            .await
            .unwrap();
        host.journal(nested(1, "other", &file("unrelated.txt")))
            .await
            .unwrap();
        host.journal(nested(3, "script", &recovered_files[1]))
            .await
            .unwrap();
        let shutdown = runner.jobs.shutdown();
        tokio::pin!(shutdown);
        bounded(async {
            tokio::select! {
                result = cancelling => result.unwrap(),
                () = &mut shutdown => panic!("shutdown did not wait for script cleanup"),
            }
        })
        .await;
        finish.send(()).unwrap();
        bounded(shutdown).await;
        let entries = lock(&host.0).clone();
        assert_eq!(
            entries.len(),
            6,
            "shutdown leaves the journal without a job_end"
        );

        let host = Arc::new(Journal::default());
        let (reopened, unfinished) = registry(&entries, &host);
        assert_eq!(unfinished, vec![id()]);
        reopened
            .reconcile(unfinished, &entries, &runner.artifacts, &*host)
            .await
            .unwrap();
        let result = "Script was interrupted when the session closed; it may have partially run. Output so far: artifact://0";
        assert!(matches!(&lock(&host.0)[0].entry, Entry::JobEnd {
            exit: Some(CommandExit::Interrupted), text, is_error: true, files, ..
        } if text == result && files == &recovered_files));
        let (job, text) = reopened.get(id()).unwrap();
        assert!(matches!(
            job.kind,
            JobKind::Script {
                exit: Some(CommandExit::Interrupted),
                ..
            }
        ));
        assert_eq!(text.as_deref(), Some(result));
    }
}
