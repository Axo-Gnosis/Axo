#![cfg(feature = "codemode")]

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering::Relaxed},
        mpsc,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use axo::script::{Limits, ScriptEnd, ScriptHost, StoreChanges, run};
use axo_ai::JsonMap;
use serde_json::{Value, json};
use tokio::{
    sync::Notify,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;

const SLACK: Duration = Duration::from_secs(3);

#[derive(Default)]
struct Host {
    output: Mutex<Vec<String>>,
    started: Notify,
    stop: CancellationToken,
    active: AtomicUsize,
    peak: AtomicUsize,
    finished: AtomicUsize,
    abandoned: AtomicUsize,
}

#[async_trait]
impl ScriptHost for Host {
    async fn call(&self, tool: &str, arguments: JsonMap) -> Result<Value, String> {
        self.peak
            .fetch_max(self.active.fetch_add(1, Relaxed) + 1, Relaxed);
        self.started.notify_one();
        let result = match tool {
            "no" => Err("no".into()),
            "hang" => {
                self.stop.cancelled().await;
                Err("stopped".into())
            }
            "wait" => {
                sleep(Duration::from_secs(2)).await;
                Ok(json!(true))
            }
            "big" => Ok(json!(vec![0; 50_000])),
            "yield" => {
                let i = arguments["i"].as_u64().unwrap();
                sleep(Duration::from_millis((173 * i + 17) % 5)).await;
                Ok(json!(i))
            }
            "echo" => Ok(Value::Object(arguments)),
            other => panic!("unexpected call {other}"),
        };
        self.active.fetch_sub(1, Relaxed);
        self.finished.fetch_add(1, Relaxed);
        result
    }

    fn search(&self, query: &str) -> Value {
        json!([query, "echo"])
    }

    fn describe(&self, name: &str) -> Result<Value, String> {
        match name {
            "echo" => Ok(json!({"name": name})),
            _ => Err("Tool not found".into()),
        }
    }

    fn output(&self, text: &str) {
        self.output.lock().unwrap().push(text.into());
    }

    fn abandon(&self) {
        self.abandoned.fetch_add(1, Relaxed);
        self.stop.cancel();
    }
}

fn limits() -> Limits {
    Limits {
        memory_bytes: 8 << 20,
        cpu: Duration::from_millis(350),
        ..Limits::default()
    }
}

async fn eval(code: &str, limits: Limits) -> (ScriptEnd, Arc<Host>) {
    let host = Arc::new(Host::default());
    let end = timeout(
        SLACK,
        run(
            code,
            BTreeMap::new(),
            host.clone(),
            limits,
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap();
    (end, host)
}

fn assert_ok(end: ScriptEnd, host: &Host, expected: &[&str]) {
    assert!(end.error.is_none(), "{end:?}");
    assert_eq!(*host.output.lock().unwrap(), expected);
}

fn assert_error(end: ScriptEnd, contains: &str) {
    let error = end.error.expect("script unexpectedly succeeded");
    assert!(
        error.message.contains(contains),
        "{error:?}, expected {contains:?}"
    );
}

#[tokio::test]
async fn heap_cap_and_single_huge_allocations() {
    let (end, _) = eval(
        "const a=[]; while(true) a.push(new Uint8Array(1<<20).fill(1))",
        limits(),
    )
    .await;
    assert_error(end, "Script ran out of memory (8 MiB limit)");
    for code in [
        r#""x".repeat(2**30)"#,
        "new Array(2**31-1).fill(0)",
        "new ArrayBuffer(2**32)",
        r#"JSON.parse("[" + "0,".repeat(1e8) + "0]")"#,
    ] {
        let (end, _) = eval(code, limits()).await;
        assert!(end.error.is_some(), "{code}");
    }
    let (end, _) = eval(
        "return 1",
        Limits {
            memory_bytes: 1,
            ..limits()
        },
    )
    .await;
    assert_error(end, "Script ran out of memory");
    let (end, host) = eval("return 2", limits()).await;
    assert_ok(end, &host, &["2"]);
}

#[tokio::test]
async fn deadline_cannot_be_caught_and_ends_an_oom_loop_and_regex() {
    let limits = Limits {
        cpu: Duration::from_millis(35),
        ..limits()
    };
    for code in [
        "while(true){}",
        "while(true){ try { new ArrayBuffer(9<<20) } catch {} }",
        "try { while(true){} } catch {print('caught')} finally {print('finally')}",
        r#"/(a+)+$/.test("a".repeat(40)+"b")"#,
    ] {
        let (end, host) = eval(code, limits.clone()).await;
        // Other tests can hold all worker slots; only time spent executing counts here.
        assert!(
            end.cpu >= limits.cpu && end.cpu < Duration::from_secs(1),
            "{code}: {:?}",
            end.cpu
        );
        assert_error(end, "Script exceeded its CPU limit of 0.035 s");
        assert!(host.output.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn cancel_while_spinning_or_awaiting_the_host() {
    for code in ["print('ready'); while(true){}", "await tools.hang()"] {
        let host = Arc::new(Host::default());
        let cancel = CancellationToken::new();
        let mut script = std::pin::pin!(run(
            code,
            BTreeMap::new(),
            host.clone(),
            Limits::default(),
            cancel.clone()
        ));
        tokio::select! {
            end = &mut script => panic!("script stopped before cancellation: {end:?}"),
            ready = timeout(SLACK, async {
                if code.contains("hang") { host.started.notified().await; }
                else {
                    while host.output.lock().unwrap().is_empty() { sleep(Duration::from_millis(2)).await; }
                }
            }) => ready.unwrap(),
        }
        let start = Instant::now();
        std::thread::spawn(move || cancel.cancel()).join().unwrap();
        assert_error(
            timeout(Duration::from_secs(1), script).await.unwrap(),
            "Script cancelled",
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        let called = usize::from(code.contains("hang"));
        assert_eq!(host.abandoned.load(Relaxed), called);
        assert_eq!(host.finished.load(Relaxed), called);
    }
}

#[tokio::test]
async fn standard_builtins_within_the_heap_budget() {
    let (end, host) = eval(
        r#"
      print(new Array(80_000).fill(0).indexOf(1));
      const a = Array.from({length: 10_000}, (_, i) => (Math.imul(i, 2654435761) >>> 0) % 1000);
      a.sort();
      return [a[0], a.at(-1)];
    "#,
        limits(),
    )
    .await;
    assert_ok(end, &host, &["-1", "[\n  0,\n  999\n]"]);
}

#[tokio::test]
async fn stack_guards_and_deep_promise_jobs() {
    let limits = Limits {
        stack_bytes: 256 << 10,
        ..limits()
    };
    for code in [
        "function f(){return f()+1} f()",
        "let a={}; for(let n=0;n<8_000;n++) a={a}; JSON.stringify(a)",
        r#"JSON.parse("[".repeat(1e6))"#,
        r#"eval("(".repeat(1e6))"#,
        "const a = { get n() {return f()} }; function f(){print(a)}; f()",
    ] {
        let (end, _) = eval(code, limits.clone()).await;
        assert_error(end, "Maximum call stack size exceeded");
    }
    let (end, host) = eval(
        "let p=Promise.resolve(0); for(let i=0;i<8000;i++) p=p.then(n=>n+1); return p",
        limits,
    )
    .await;
    assert_ok(end, &host, &["8000"]);
}

#[tokio::test]
async fn async_fanout_is_bounded_and_results_stay_in_order() {
    let (end, host) = eval(
        r#"
      const results = await Promise.all(Array.from({length: 200}, (_, i) => tools.yield({i})));
      if (!results.every((x, i) => x === i)) throw new Error('mismatched result');
      return results.length;
    "#,
        Limits {
            max_in_flight: 4,
            ..limits()
        },
    )
    .await;
    assert_eq!(end.calls, 200);
    assert_ok(end, &host, &["200"]);
    assert_eq!(host.peak.load(Relaxed), 4);
    assert_eq!(host.finished.load(Relaxed), 200);
    assert_eq!(host.abandoned.load(Relaxed), 0);
}

#[tokio::test]
async fn host_reject_is_catchable_and_uncaught_reject_keeps_the_call_site() {
    let (end, host) = eval(
        "try { await tools.no(); } catch(e) { return e instanceof Error && e.message; }",
        limits(),
    )
    .await;
    assert_ok(end, &host, &["no"]);
    let (end, _) = eval("\nawait Promise.all([tools.no()]);", limits()).await;
    let error = end.error.unwrap();
    assert_eq!(error.message, "no");
    assert_eq!(error.line, Some(2));
    assert!(error.stack.unwrap().contains("axo-script.js:2"));
}

#[tokio::test]
async fn orphan_rejections_and_failed_jobs_report_lines() {
    for code in [
        "\n\nthrow new Error('boom')",
        "Promise.resolve().then(() => {\n\nthrow new Error('boom'); }); return 1",
        "queueMicrotask(() => {\n\nthrow new Error('boom'); }); return 1",
    ] {
        let (end, _) = eval(code, limits()).await;
        let error = end.error.unwrap();
        assert_eq!(error.message, "boom");
        assert_eq!(error.line, Some(3));
        assert!(error.stack.unwrap().contains("axo-script.js:3"));
    }
    let (end, host) = eval(
        "const p = Promise.reject(new Error('handled')); await p.catch(() => {}); return 1",
        limits(),
    )
    .await;
    assert_ok(end, &host, &["1"]);
}

#[tokio::test]
async fn no_ambient_capability_or_forgeable_bridge() {
    let (end, host) = eval(r#"
      const root = new Function('return this')();
      for (const name of ['require', 'process', 'fetch', 'setTimeout', 'std', 'os', '__settle', 'send']) {
        if (typeof root[name] !== 'undefined') throw new Error(name);
      }
      try { await import('x'); throw new Error('module loaded'); }
      catch (err) { if (err.message === 'module loaded') throw err; }
      try { root.tools = {echo: () => 'forged'}; } catch {}
      try { Array.prototype.toJSON = () => 'forged'; } catch {}
      try { JSON.parse = () => 'forged'; } catch {}
      try { Map.prototype.get = () => 'forged'; } catch {}
      const value = await tools['echo']({ v: [1,2] });
      if (value.v.join(',') !== '1,2') throw new Error('forged bridge');
      for (const args of [null, [], new Date(), 'string']) {
        try { await tools.echo(args); throw new Error('invalid arguments accepted'); }
        catch (err) { if (!err.message.includes('plain object')) throw err; }
      }
      console.debug('search', (await searchTools('ex'))[0]);
      print('describe', (await describeTool('echo')).name);
      return await tools.echo();
    "#, limits()).await;
    assert_eq!(end.calls, 4);
    assert_ok(end, &host, &["search ex", "describe echo", "{}"]);
}

#[tokio::test]
async fn call_cap_and_cpu_excludes_host_wait() {
    let (end, host) = eval(
        r#"
      for (let i=0; i<1000; i++) await tools.echo();
      try { await tools.echo(); } catch (err) { return err.message; }
    "#,
        Limits {
            cpu: Duration::from_secs(1),
            ..limits()
        },
    )
    .await;
    assert_eq!(end.calls, 1000);
    assert_ok(end, &host, &["Tool call limit reached (1000)"]);
    let limits = Limits {
        cpu: Duration::from_millis(100),
        ..limits()
    };
    let (end, host) = eval("return await tools.wait()", limits).await;
    assert!(end.cpu < Duration::from_millis(100));
    assert_ok(end, &host, &["true"]);
}

#[tokio::test]
async fn returning_stops_and_drains_started_calls() {
    let (end, host) = eval("tools.hang(); return 1;", limits()).await;
    assert_ok(end, &host, &["1"]);
    assert_eq!(host.peak.load(Relaxed), 1);
    assert_eq!(host.abandoned.load(Relaxed), 1);
    assert_eq!(host.finished.load(Relaxed), 1);

    let (end, host) = eval(
        "tools.hang(); tools.hang(); return (await searchTools('lookup'))[0];",
        Limits {
            max_in_flight: 1,
            ..limits()
        },
    )
    .await;
    assert_ok(end, &host, &["lookup"]);
    assert_eq!(host.abandoned.load(Relaxed), 1);
    assert_eq!(
        host.finished.load(Relaxed),
        1,
        "queued call started during shutdown"
    );
}

#[tokio::test]
async fn store_is_bounded_and_commit_is_transactional() {
    let initial = BTreeMap::from([
        ("keep".into(), json!({"v": 5})),
        ("delete".into(), json!(1)),
    ]);
    let host = Arc::new(Host::default());
    let end = run(r#"
      const keep = load('keep');
      keep.v = 99;
      if (load('keep').v !== 5) throw new Error('store aliased');
      store('delete', undefined);
      store('temp', 1);
      store('temp', undefined);
      if (load('delete') !== undefined || load('temp') !== undefined) throw new Error('not deleted');
      store('new', [load('keep').v, true]);
    "#, initial, host.clone(), limits(), CancellationToken::new()).await;
    assert!(end.error.is_none(), "{end:?}");
    assert_eq!(
        end.store.set,
        BTreeMap::from([("new".into(), json!([5, true]))])
    );
    assert_eq!(end.store.delete, ["delete"]);
    for (code, error) in [
        ("store('ok', 1); throw new Error('no commit')", "no commit"),
        (
            "store('big', 'x'.repeat(300<<10))",
            "Store value limit exceeded",
        ),
        ("store('bad', () => {})", "Value is not serializable"),
    ] {
        let (end, _) = eval(code, limits()).await;
        assert_eq!(end.store, StoreChanges::default());
        assert_error(end, error);
    }
    let (end, _) = eval(
        "store('key', 'string')",
        Limits {
            store_total_bytes: 9,
            ..limits()
        },
    )
    .await;
    assert_error(end, "Store total limit exceeded");
}

#[tokio::test]
async fn result_that_cannot_fit_in_the_vm_rejects() {
    let (end, host) = eval("try { await tools.big() } catch (err) { return err.message; } throw new Error('oversized result resolved');", Limits { memory_bytes: 640 << 10, ..limits() }).await;
    assert_ok(end, &host, &["Result too large"]);
}

#[tokio::test]
async fn output_limits_apply_to_print_and_return_in_utf8_bytes() {
    let (end, host) = eval(
        "while (true) console.log('hello', {x: 1})",
        Limits {
            max_output_items: 2,
            ..limits()
        },
    )
    .await;
    assert_error(end, "Output limit exceeded");
    assert_eq!(
        *host.output.lock().unwrap(),
        ["hello {\n  \"x\": 1\n}", "hello {\n  \"x\": 1\n}"]
    );
    let (end, host) = eval(
        "print('é'); return 'é';",
        Limits {
            max_output_bytes: 3,
            ..limits()
        },
    )
    .await;
    assert_error(end, "Output limit exceeded");
    assert_eq!(*host.output.lock().unwrap(), ["é"]);
}

// A gated native callback models a builtin that cannot poll the interrupt hook.
// Unlike a big sort, it gives us a deterministic way to hold all four slots.
#[tokio::test]
async fn cancelled_non_cooperative_worker_still_holds_its_slot() {
    struct Gate {
        started: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    #[async_trait]
    impl ScriptHost for Gate {
        async fn call(&self, _: &str, _: JsonMap) -> Result<Value, String> {
            unreachable!()
        }
        fn search(&self, _: &str) -> Value {
            unreachable!()
        }
        fn describe(&self, _: &str) -> Result<Value, String> {
            unreachable!()
        }
        fn output(&self, _: &str) {
            self.started.send(()).unwrap();
            self.release.lock().unwrap().recv_timeout(SLACK).unwrap();
        }
        fn abandon(&self) {
            unreachable!()
        }
    }
    let (start, started) = mpsc::channel();
    let mut workers = Vec::new();
    for _ in 0..4 {
        let (release, rx) = mpsc::channel();
        let host = Arc::new(Gate {
            started: start.clone(),
            release: Mutex::new(rx),
        });
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let task = tokio::spawn(async move {
            run(
                "print('blocked');",
                BTreeMap::new(),
                host,
                Limits::default(),
                token,
            )
            .await
        });
        workers.push((release, cancel, task));
    }
    tokio::task::spawn_blocking(move || (0..4).for_each(|_| started.recv_timeout(SLACK).unwrap()))
        .await
        .unwrap();
    workers[0].1.cancel();
    let waiting = eval("return 5", limits());
    let mut waiting = std::pin::pin!(waiting);
    assert!(
        timeout(Duration::from_millis(35), &mut waiting)
            .await
            .is_err()
    );
    workers[0].0.send(()).unwrap();
    let (end, host) = timeout(SLACK, waiting).await.unwrap();
    assert_ok(end, &host, &["5"]);
    let (_, _, cancelled) = workers.remove(0);
    assert_error(cancelled.await.unwrap(), "Script cancelled");
    for (release, _, task) in workers {
        release.send(()).unwrap();
        assert!(task.await.unwrap().error.is_none());
    }
}
