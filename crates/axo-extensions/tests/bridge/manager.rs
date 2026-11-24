//! Configuration, discovery and winner publication through the public `Extensions` API.
//! Nothing here needs a runner to start; most cases run with Bun absent.

use std::path::{Path, PathBuf};

use axo_ai::{Error, Models};
use axo_extensions::{
    BridgeError, ExtensionEvent, ExtensionPhase, ExtensionProvider, Extensions, Hidden,
    ProblemKind, ProviderCollision, ProviderOrigin, inspect_package,
};

use super::{
    Case, Host, Http, assert_unavailable, base_url, enabled, find, phase, register_native, stream,
};

pub const CASES: &[Case] = &[
    case!(absent_bun_keeps_claims_names_the_extension_and_spares_natives),
    case!(invalid_input_changes_nothing),
    case!(last_enabled_wins_and_collisions_are_reported),
    case!(unadmitted_and_duplicate_packages_are_diagnosed),
    case!(rescan_retains_last_good_claims_until_deliberate_removal),
    case!(shutdown_removes_the_layer_and_closes_admission),
];

fn collision(provider: &str, winner: &str, hidden: Hidden) -> ProviderCollision {
    ProviderCollision {
        provider: provider.into(),
        winner: winner.into(),
        hidden,
    }
}

/// A request to `provider` fails before any HTTP, naming `ext`.
async fn fails_named(models: &Models, provider: &str, ext: &str, http: &Http) {
    let before = http.requests().len();
    let model = find(models, provider).unwrap_or_else(|| panic!("{provider} is not listed"));
    assert_unavailable(&stream(models, &model).await, ext);
    assert!(matches!(
        models.auth(&model).await,
        Err(Error::ProviderUnavailable { provider: p, reason }) if p == provider && reason.contains(ext)
    ));
    assert_eq!(
        http.requests().len(),
        before,
        "an unavailable provider sent HTTP"
    );
}

async fn absent_bun_keeps_claims_names_the_extension_and_spares_natives() {
    let host = Host::new().await;
    let http = Http::start();
    let native = Http::start();
    register_native(&host.models, "lab", &native.url);
    register_native(&host.models, "unrelated", &native.url);
    host.package("root", "lab-provider", &[("lab", &http.url, true, true)]);
    let list = enabled(&[("lab-provider", None)]);

    for bun in [None, Some(host.path("ctl")), Some(host.path("missing-bun"))] {
        // Absent, a directory, or a missing file: all Unavailable, nothing started.
        let extensions = host.manager_with(host.options_with(bun));
        extensions.configure(&list).await.unwrap();
        let status = phase(&extensions, "lab-provider");
        assert_eq!(status.phase, ExtensionPhase::Unavailable, "{status:?}");
        let problem = status.problem.as_ref().unwrap();
        assert_eq!(problem.kind, ProblemKind::Bun, "{status:?}");
        assert!(problem.message.contains("Bun"), "{status:?}");

        // The declared override is listed and wins over the native registration...
        assert_eq!(base_url(&host.models, "lab").unwrap(), http.url);
        assert!(
            host.models
                .providers()
                .iter()
                .any(|p| p.id == "lab" && p.login)
        );
        assert_eq!(
            extensions.collisions(),
            [collision(
                "lab",
                "lab-provider",
                Hidden::Host(ProviderOrigin::Registered)
            )]
        );
        // ...yet never falls through to it: the request names the extension.
        fails_named(&host.models, "lab", "lab-provider", &http).await;
        assert!(
            native.requests().is_empty(),
            "the hidden native provider was used"
        );
        // Login is gated before any runner start too.
        assert!(matches!(
            host.models.login("lab", &NoUi).await,
            Err(Error::ProviderUnavailable { .. })
        ));

        // An unrelated native provider is untouched.
        let unrelated = find(&host.models, "unrelated").unwrap();
        stream(&host.models, &unrelated).await;
        assert_eq!(native.requests().len(), 1);
        native.clear();

        extensions.shutdown().await;
        assert_eq!(base_url(&host.models, "lab").unwrap(), native.url);
    }

    // A live manager picks up Bun once the host supplies it, without being rebuilt.
    let extensions = host.manager_with(host.options_with(None));
    extensions.configure(&list).await.unwrap();
    let status = phase(&extensions, "lab-provider");
    assert_eq!(status.problem.map(|p| p.kind), Some(ProblemKind::Bun));
    let executable = std::env::current_exe().unwrap();
    let mut relative = std::env::current_dir()
        .unwrap()
        .strip_prefix("/")
        .unwrap()
        .components()
        .map(|_| "..")
        .collect::<PathBuf>();
    relative.push(executable.strip_prefix("/").unwrap());
    assert!(relative.is_relative() && relative.is_file());
    let expected = format!("bun {} is not an absolute path", relative.display());
    assert!(matches!(
        Extensions::new(host.axo.clone(), host.options_with(Some(relative.clone()))),
        Err(BridgeError::InvalidOptions(message)) if message == expected
    ));
    assert!(matches!(
        extensions.set_bun(Some(relative)).await,
        Err(BridgeError::InvalidOptions(message)) if message == expected
    ));
    extensions.set_bun(Some(executable)).await.unwrap();
    let status = phase(&extensions, "lab-provider");
    assert_eq!((status.phase, status.problem), (ExtensionPhase::Cold, None));
    extensions.shutdown().await;
    assert!(host.pids().is_empty(), "no runner may start without Bun");
}

async fn invalid_input_changes_nothing() {
    let host = Host::new().await;
    let http = Http::start();
    host.package("root", "alpha", &[("pa", &http.url, false, false)]);
    host.package("root", "beta", &[("pb", &http.url, false, false)]);
    let extensions = host.manager_with(host.options_with(None));
    extensions
        .configure(&enabled(&[("alpha", None)]))
        .await
        .unwrap();

    let invalid = [
        enabled(&[("beta", None), ("beta", None)]),
        enabled(&[("Beta", None)]),
        enabled(&[("..", None)]),
        enabled(&[("a/b/c", None)]),
        enabled(&[("", None)]),
        enabled(&[("beta", Some("Group"))]),
        enabled(&[("beta", Some(""))]),
    ];
    for list in &invalid {
        assert!(
            matches!(
                extensions.configure(list).await,
                Err(BridgeError::InvalidConfig(_))
            ),
            "{list:?}"
        );
        assert!(find(&host.models, "pa").is_some());
        assert!(find(&host.models, "pb").is_none());
        assert_eq!(extensions.status().len(), 1);
    }

    // Relative host paths are invalid options.
    let mut options = host.options_with(None);
    options.roots = vec!["relative".into()];
    assert!(matches!(
        Extensions::new(host.axo.clone(), options),
        Err(BridgeError::InvalidOptions(message)) if message == "root relative is not an absolute path"
    ));
    assert!(find(&host.models, "pb").is_none());
    assert!(matches!(
        extensions.retry("nobody").await,
        Err(BridgeError::UnknownExtension(_))
    ));
}

async fn last_enabled_wins_and_collisions_are_reported() {
    let host = Host::new().await;
    let a = Http::start();
    let b = Http::start();
    host.package(
        "root",
        "alpha",
        &[
            ("shared", &a.url, false, false),
            ("pa", &a.url, false, false),
        ],
    );
    host.package("root", "beta", &[("shared", &b.url, false, false)]);
    let extensions = host.manager_with(host.options_with(None));
    let mut events = extensions.subscribe();

    extensions
        .configure(&enabled(&[("alpha", None), ("beta", None)]))
        .await
        .unwrap();
    assert_eq!(base_url(&host.models, "shared").unwrap(), b.url);
    assert_eq!(
        extensions.collisions(),
        [collision(
            "shared",
            "beta",
            Hidden::Extension("alpha".into())
        )]
    );
    // The shadowed extension is still reported, in enabled order.
    let ids: Vec<_> = extensions.status().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, ["alpha", "beta"]);
    assert_eq!(
        extensions.providers(),
        [
            ExtensionProvider {
                provider: "pa".into(),
                extension: "alpha".into()
            },
            ExtensionProvider {
                provider: "shared".into(),
                extension: "beta".into()
            },
        ]
    );
    let mut shadowed = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let ExtensionEvent::Shadowed(collision) = event {
            shadowed.push(collision);
        }
    }
    assert_eq!(
        shadowed,
        [collision(
            "shared",
            "beta",
            Hidden::Extension("alpha".into())
        )]
    );

    // Priority follows list order.
    extensions
        .configure(&enabled(&[("beta", None), ("alpha", None)]))
        .await
        .unwrap();
    assert_eq!(base_url(&host.models, "shared").unwrap(), a.url);
    assert_eq!(
        extensions.collisions(),
        [collision(
            "shared",
            "alpha",
            Hidden::Extension("beta".into())
        )]
    );

    // Deliberate removal reveals the next claimant; removing all removes the provider.
    extensions
        .configure(&enabled(&[("beta", None)]))
        .await
        .unwrap();
    assert_eq!(base_url(&host.models, "shared").unwrap(), b.url);
    assert!(find(&host.models, "pa").is_none());
    assert!(extensions.collisions().is_empty());
    extensions.configure(&[]).await.unwrap();
    assert!(find(&host.models, "shared").is_none());
    assert!(extensions.status().is_empty());
}

async fn unadmitted_and_duplicate_packages_are_diagnosed() {
    let host = Host::new().await;
    let one = Http::start();
    let two = Http::start();
    let native = Http::start();
    register_native(&host.models, "lab", &native.url);
    // Present in both roots: the first root wins, and the duplicate is diagnosed.
    host.package("root", "dup", &[("pd", &one.url, false, false)]);
    host.package("root2", "dup", &[("pd", &two.url, false, false)]);
    // Malformed first-ever manifests: not admitted, no guessed overrides.
    host.write_package("root", "broken", "{ not json");
    host.write_package(
        "root",
        "mismatch",
        &serde_json::json!({"id": "other", "entry": "index.ts", "providers": []}).to_string(),
    );
    host.write_package(
        "root",
        "escape",
        &serde_json::json!({"id": "escape", "entry": "../dup/index.ts", "providers": []})
            .to_string(),
    );
    host.package("root", "good", &[("pg", &one.url, false, false)]);
    host.write_package(
        "root2",
        "decl",
        &serde_json::json!({
            "id": "decl", "entry": "index.ts",
            "tools": [{"name": "probe", "description": "Probe", "parameters": {"type": "object"}}],
            "commands": [{"name": "hello", "description": "Say hello"}],
            "sections": [{"name": "rules", "file": "rules.md"}],
        })
        .to_string(),
    );
    std::fs::write(host.path("root2/decl/rules.md"), "Rules.").unwrap();

    let extensions = host.manager_with(host.options_with(None));
    // Packages are readable from disk before anything is enabled or run.
    assert_eq!(
        extensions.authored().await,
        ["broken", "decl", "dup", "escape", "good", "mismatch"]
    );
    let decl = extensions.inspect("decl").await.unwrap();
    assert_eq!(decl.entry_error, None);
    assert_eq!(decl.session.tools[0].name, "probe");
    assert_eq!(decl.session.commands[0].name, "hello");
    assert_eq!(decl.sections, [("rules".into(), "Rules.".into())]);
    let dup = extensions.inspect("dup").await.unwrap();
    assert_eq!(dup.package, host.path("root/dup"));
    assert_eq!(dup.providers[0].id, "pd");
    assert!(!dup.diagnostics.is_empty());
    assert!(
        extensions
            .inspect("escape")
            .await
            .unwrap()
            .entry_error
            .is_some()
    );
    for (id, kind) in [
        ("ghost", ProblemKind::NotFound),
        ("broken", ProblemKind::Invalid),
        ("../dup", ProblemKind::Invalid),
    ] {
        assert_eq!(extensions.inspect(id).await.unwrap_err().kind, kind, "{id}");
    }
    let package = inspect_package(&host.path("root2/decl")).await.unwrap();
    assert_eq!(
        (package.id, package.package),
        ("decl".into(), host.path("root2/decl"))
    );
    assert_eq!(
        inspect_package(Path::new("root2/decl"))
            .await
            .unwrap_err()
            .kind,
        ProblemKind::Invalid
    );
    let mut events = extensions.subscribe();
    let list = enabled(&[
        ("dup", None),
        ("ghost", None),
        ("broken", None),
        ("mismatch", None),
        ("escape", None),
        ("good", None),
    ]);
    extensions.configure(&list).await.unwrap();

    assert_eq!(base_url(&host.models, "pd").unwrap(), one.url);
    let diagnostics = extensions.diagnostics();
    assert!(
        diagnostics.iter().any(|d| d.id == "dup"),
        "duplicate roots not diagnosed: {diagnostics:?}"
    );
    let mut announced = false;
    while let Ok(event) = events.try_recv() {
        announced |= matches!(event, ExtensionEvent::Diagnostic(d) if d.id == "dup");
    }
    assert!(announced, "no Diagnostic event");

    for id in ["ghost", "broken", "mismatch", "escape"] {
        let status = phase(&extensions, id);
        assert_eq!(status.phase, ExtensionPhase::Unavailable, "{status:?}");
        let kind = status.problem.map(|problem| problem.kind);
        let expected = if id == "ghost" {
            ProblemKind::NotFound
        } else {
            ProblemKind::Invalid
        };
        assert_eq!(kind, Some(expected), "{id}");
    }
    // Siblings stay admitted; the native provider stays visible.
    assert!(find(&host.models, "pg").is_some());
    assert_eq!(base_url(&host.models, "lab").unwrap(), native.url);
    assert!(extensions.collisions().is_empty());
}

async fn rescan_retains_last_good_claims_until_deliberate_removal() {
    let host = Host::new().await;
    let http = Http::start();
    let native = Http::start();
    register_native(&host.models, "lab", &native.url);
    host.package("root", "lab-provider", &[("lab", &http.url, false, true)]);
    let extensions = host.manager();
    let list = enabled(&[("lab-provider", None)]);
    extensions.configure(&list).await.unwrap();
    assert_eq!(
        phase(&extensions, "lab-provider").phase,
        ExtensionPhase::Cold
    );

    // A failed rescan keeps the last accepted claim, reserved but unavailable.
    host.write_package("root", "lab-provider", "{ broken");
    extensions.configure(&list).await.unwrap();
    assert_eq!(base_url(&host.models, "lab").unwrap(), http.url);
    let status = phase(&extensions, "lab-provider");
    assert_eq!(status.phase, ExtensionPhase::Unavailable, "{status:?}");
    fails_named(&host.models, "lab", "lab-provider", &http).await;
    assert!(native.requests().is_empty());
    // Removing the manifest entirely is still only a failed refresh.
    std::fs::remove_dir_all(host.path("root").join("lab-provider")).unwrap();
    extensions.configure(&list).await.unwrap();
    assert_eq!(base_url(&host.models, "lab").unwrap(), http.url);
    fails_named(&host.models, "lab", "lab-provider", &http).await;
    assert!(host.pids().is_empty());

    // A corrected (changed) declaration is adopted and runs.
    let fixed = Http::start();
    host.package("root", "lab-provider", &[("lab", &fixed.url, false, true)]);
    extensions.configure(&list).await.unwrap();
    assert_eq!(base_url(&host.models, "lab").unwrap(), fixed.url);
    assert_eq!(
        phase(&extensions, "lab-provider").phase,
        ExtensionPhase::Cold
    );
    let model = find(&host.models, "lab").unwrap();
    stream(&host.models, &model).await;
    assert_eq!(fixed.requests().len(), 1);
    assert!(fixed.requests()[0].0.starts_with("POST /rewritten "));

    // Deliberate removal reveals the exact earlier native registration.
    extensions.configure(&[]).await.unwrap();
    assert_eq!(base_url(&host.models, "lab").unwrap(), native.url);
    extensions.shutdown().await;
}

async fn shutdown_removes_the_layer_and_closes_admission() {
    let host = Host::new().await;
    let http = Http::start();
    let native = Http::start();
    register_native(&host.models, "lab", &native.url);
    host.package("root", "lab-provider", &[("lab", &http.url, false, true)]);
    let extensions = host.manager();
    let list = enabled(&[("lab-provider", None)]);
    extensions.configure(&list).await.unwrap();
    let model = find(&host.models, "lab").unwrap();
    stream(&host.models, &model).await;
    let pid = host.pids()[0];
    assert!(super::alive(pid));

    extensions.shutdown().await;
    // Reaped before `shutdown` returned, and the native provider is back.
    assert!(!super::alive(pid), "shutdown left the runner running");
    assert_eq!(base_url(&host.models, "lab").unwrap(), native.url);
    // New requests go to the revealed native winner.
    stream(&host.models, &find(&host.models, "lab").unwrap()).await;
    assert_eq!(native.requests().len(), 1);
    assert!(matches!(
        extensions.configure(&list).await,
        Err(BridgeError::ShutDown)
    ));
    assert!(matches!(
        extensions.retry("lab-provider").await,
        Err(BridgeError::ShutDown)
    ));
    extensions.shutdown().await;
    assert_eq!(http.requests().len(), 1);
}

/// A login UI that must never be reached.
struct NoUi;

#[async_trait::async_trait]
impl axo_ai::LoginInteraction for NoUi {
    async fn open_url(&self, _: &str, _: Option<&str>) {
        unreachable!()
    }
    async fn device_code(&self, _: &axo_ai::DeviceCode) {
        unreachable!()
    }
    async fn prompt(&self, _: &axo_ai::LoginPrompt) -> Result<String, Error> {
        unreachable!()
    }
    async fn select(&self, _: &axo_ai::LoginSelect) -> Result<String, Error> {
        unreachable!()
    }
    async fn progress(&self, _: &str) {
        unreachable!()
    }
}
