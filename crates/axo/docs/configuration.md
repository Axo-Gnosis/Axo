# Configuration

`axo` is a library: it owns no configuration directory, no configuration file names, and no environment variable that locates either. It owns configuration **formats and semantics**: the settings schema, validation, merging, trust filtering, and value resolution, all in one evaluator (`EffectiveConfig::resolve`). Hosts own **names, locations, and acquisition**: they hand axo raw layers through the `ConfigSource` trait. axo does own the cross-tool `.agents` conventions (`~/.agents/AGENTS.md`, `~/.agents/skills`, project `.agents/skills`), each behind a user-only switch: `homeAgentsDir` (default `false`) and `project.files.skills`. `Models`, the `SessionStore`, and the cache directory are host-supplied too (see [Host-owned resources](#host-owned-resources)), so `Axo::builder(models, store)` and `AxoBuilder::build` perform no configuration I/O. Command supervision (`Supervision`) is host-only as well: it has no settings key and is set only through `AxoBuilder::supervision` (see [Command supervision](#command-supervision-axosupervision)).

## Configuration source (`axo::ConfigSource`)

A host implements `ConfigSource` and passes it to `AxoBuilder::config(Arc<dyn ConfigSource>)`. Both methods are blocking and run on the caller's async task.

| Method | Signature | Contract |
| :--- | :--- | :--- |
| `user` | `fn user(&self) -> Result<UserConfig, Error>` | Called once per `Axo::create_session`, `Axo::open_session`, and `Axo::schemes`, before anything else |
| `project` | `fn project(&self, cwd: &Path) -> Result<ConfigLayer, Error>` | The project contribution for a session in `cwd`; the project root is `axo::context::project_root(cwd)` (the git root, or `cwd` outside a repository). Called at most once per such call, after `user`, and only when the effective access is `Read` or `Trust` and the user's `project.files.config` is `true` |

An error from either method fails that `create_session`, `open_session`, or `schemes` call as returned. Hosts report an unreadable path as `Error::Io { path, source }`, invalid content they parse (for example malformed JSONC) as `Error::Config { origin, message }`, and any other acquisition failure as `Error::Host(Box::new(err))`, which carries the host's own error.

| Type | Fields | Meaning |
| :--- | :--- | :--- |
| `ConfigLayer` | `origin: String`, `settings: Option<Value>`, `prompts: IndexMap<String, PromptOverride>`, `agent_roots: Vec<PathBuf>` | One raw, unvalidated contribution. `origin` names the layer in errors and warnings (a path, a URL, `"host defaults"`, …). `settings` is the [settings](#settings-axosettings) object; `None` and `Some(Value::Null)` both mean no settings. `prompts` is keyed by a `PROMPT_NAMES` entry: a missing key means no override, an empty text is an override. Agent roots are explicit directories, honored on the project layer only at `Trust` |
| `PromptOverride` | `origin: String`, `text: String` | One prompt override as the host holds it; `text` is untrimmed, axo trims it |
| `UserConfig` | `layer: ConfigLayer`, `skill_roots: Vec<PathBuf>`, `context_files: Vec<PathBuf>` | The user's layer plus the user's filesystem resources: skill roots searched after project `.agents/skills` and before `~/.agents/skills`, and context files loaded after `~/.agents/AGENTS.md` and before the project walk (missing files are skipped) |
| `EffectiveConfig` | `access: ProjectAccess`, `settings: Settings`, `prompts: IndexMap<String, PromptOverride>`, `skill_roots: Vec<PathBuf>`, `context_files: Vec<PathBuf>`, `agent_roots: Vec<PathBuf>` | What one session runs with: the effective access, merged and filtered settings (`defaultModel` still unresolved; see `EffectiveConfig::default_model`), trimmed prompt overrides (the project's wins per name), user skill roots and context files, and agent roots (trusted project roots first, then user roots) |

`PROMPT_NAMES` lists the overridable prompts: `SYSTEM`, `WORK`, `PROSE` (system prompt sections; an empty override disables the section) and `SUMMARY`, `COMPACTION`, `COMPACTION_UPDATE`, `COMPACTION_SPLIT_TURN`, `BRANCH_SUMMARY` (summarizer prompts; an empty override records a `Diagnostic` and keeps the bundled prompt). See [`prompts-and-context.md`](prompts-and-context.md).

### Evaluation (`EffectiveConfig::resolve`)

`EffectiveConfig::resolve(source: &dyn ConfigSource, cwd: &Path, access: Option<ProjectAccess>) -> Result<EffectiveConfig, Error>` is the single evaluator. `Axo` calls it with `SessionOptions::project_access` as `access`; hosts may call it directly, for example to preview what a session would get.

1. **Sample the user layer:** `source.user()`.
2. **Validate the user settings on their own:** `None` or `null` is no settings; a non-object is `Error::Config { origin, message: "expected a JSON object" }`; a value that does not deserialize as `Settings` is `Error::Config` with the layer's `origin` and the deserializer's message. The valid object is merged over the defaults.
3. **Validate the user prompt overrides:** a name outside `PROMPT_NAMES` is `Error::Config` with that override's `origin` and the message `unknown prompt override "<name>"`; texts are trimmed.
4. **Derive access:** an explicit `access` wins; otherwise the `project` policy of this same user sample decides via `ProjectSettings::access_for(project_root(cwd))` (see [Project access](#project-access-project--axoprojectsettings)). A project layer can never raise its own access.
5. **Apply the project layer unless access is `Ignore` or the user's `project.files.config` is `false`:** call `source.project(cwd)` and validate its settings as in step 2 before any filtering, so an invalid value fails with the project's `origin` even for a key that would be stripped. Strip the user-only keys `project`, `homeAgentsDir`, and `modelRoles`, logging a `tracing` warning carrying the layer's `origin` for each stripped key. The rest deep-merges over the user settings: objects merge key by key with the project winning, arrays and scalars replace. Project prompt overrides (validated as in step 3) replace the user's per name, at `Read` and `Trust`. Project agent roots apply only at `Trust`; at `Read`, non-empty roots are ignored with a warning. At `Ignore`, or with `project.files.config: false`, `project` is not called and no project contribution applies.
6. **Deserialize the merged object** into `Settings`; an error is attributed to the last applied origin (the project's when project settings were merged, else the user's). Below `Trust`, any enabled `bash.devShell` resolves to `"off"`, regardless of which layer enabled it, with one warning naming that setting's winning origin.

`defaultModel` is not resolved here: `EffectiveConfig::default_model` expands its `$VAR` at model selection and rejects values starting with `!`, only when neither `SessionOptions::model`, the session's recorded model, nor the root agent's model selects one (see [Config values](#config-values)).

Unknown keys are ignored in every layer.

### Lifecycle

- **Sampling:** every `Axo::create_session`, `Axo::open_session`, and `Axo::schemes` call samples `user()` afresh and computes access from that same sample, so a policy or settings change applies to the next session start without rebuilding `Axo`.
- **Per session:** a session keeps the `EffectiveConfig` it started with. Settings, prompt overrides (including `SYSTEM`, `WORK`, and `PROSE` when the system prompt is rebuilt for a new date or dev-shell note), skill roots, and context-file paths are fixed at create or open.
- **Forks:** `Session::fork` inherits the parent's resolved `EffectiveConfig` and never calls the source.
- **Build:** `Axo::builder` and `AxoBuilder::build` call no source method.

### Trust boundary

Hosts are trusted integration code; project layers are untrusted data. axo enforces the rules above on whatever `ConfigSource::project` returns: it never calls `project` at `Ignore`, never lets a project layer set user-only keys or its own access, and below `Trust` resolves `bash.devShell` to `"off"` even when enabled in user settings. Config values starting with `!` are rejected at every access level; they never run code. I/O the host performs before or while producing a layer (for example reading a project file inside `user`, or computing `project` eagerly) is outside axo's guarantee: a host that must not touch an ignored project must not read it on its own.

### Absent inputs

| Absent input | Effect |
| :--- | :--- |
| config (`AxoBuilder::config` unset) | Default settings and no project layer: `user` is an empty `UserConfig`, `project` an empty `ConfigLayer` |
| `cache_dir` (`AxoBuilder::cache_dir` unset) | No `docs` system prompt section and no `axo://` scheme; Nix dev shells (`bash.devShell` `nix`) fall back to the host environment with a warning |
| models / store | Compile-time required: `Axo::builder(models: Arc<Models>, store: Arc<dyn SessionStore>)` |

## Host-owned resources

| Resource | Supplied through | How axo uses it |
| :--- | :--- | :--- |
| Models and credentials | `Axo::builder(models, ..)` with an `Arc<Models>` | The host builds `Models` with `Models::load(ModelsOptions { cache_dir, extended_context, config, credentials, http_client })`; see [Models & Credentials](#models-axo_aimodelsconfig--credentials) |
| Session store | `Axo::builder(.., store)` with an `Arc<dyn SessionStore>` | `JsonlStore::new(root)` writes `<root>/--<encoded-cwd>--/<timestamp>_<uuidv7>.jsonl`, per-session `<session>/artifacts/` bash and maintenance artifacts (`artifact://<n>`), and `<session>/local/` (`local://<path>`): model scratch files and content handed over in prompts (see [Prompt Parts](prompts-and-context.md#prompt-parts-cratesaxosrcinputrs)). Forks copy both directories |
| Cache directory | `AxoBuilder::cache_dir(PathBuf)` | Bundled documentation materialized under `<cache_dir>/docs/` (the `docs` section and `axo://` URLs) and captured Nix dev-shell environments under `<cache_dir>/devshell/`. The `models.dev` catalog cache is separate: it lives wherever the host points `ModelsOptions::cache_dir` |
| Harness name | `AxoBuilder::name(name) -> Result<Self, Error>` | Defaults to `axo`; names the docs scheme and section and the bundled `SYSTEM`/`SUMMARY` self-references. Names must match `^[a-z][a-z0-9+.-]+$`; `artifact`, `file`, `skill`, `local`, `proc`, and `agent` are reserved. Invalid or reserved names return `Error::Scheme` here; `build()` stays infallible |
| User skills and context files | `UserConfig::skill_roots`, `UserConfig::context_files` | Skill roots (`<root>/<skill>/SKILL.md`) and user-wide instruction files, in the order given |
| Agent definitions | `ConfigLayer::agent_roots` | Explicit roots recursively searched for frontmatter-bearing `*.md` files; trusted project roots precede user roots. No implicit `.agents/` discovery. See [Agent definitions](prompts-and-context.md#agent-definitions) |

With `homeAgentsDir: true` in the user layer, axo also reads the ecosystem files `~/.agents/AGENTS.md` (before `UserConfig::context_files`) and `~/.agents/skills/` (after `UserConfig::skill_roots`).

The `drive` and `memory` examples ship their own host under an explicit, required `--dir <dir>` (no default, no environment variable): a JSONC `settings.jsonc` user layer (whose `extendedContext` boolean the example itself reads for `ModelsOptions::extended_context`), `prompts/<NAME>.md` overrides, `skills/`, `AGENTS.md`, `models.jsonc`, `auth.json`, `sessions/`, and `cache/` (both `cache_dir`s). That layout belongs to the examples, not to axo.

### Project resources

| Resource | Access required | Gate | Purpose |
| :--- | :--- | :--- | :--- |
| Project layer (`ConfigSource::project`) | `read` (`trust` for code-running keys) | `project.files.config` | Project settings merged on top of user settings, and project prompt overrides |
| `.agents/skills/` | `read` | `project.files.skills` | Project skills discovered from `cwd` up to the git root |
| `AGENTS.md`, `CLAUDE.md`, … | `read` | `contextFiles` map | Context files loaded from the git root to `cwd` at startup and on demand in subdirectories |

## Settings (`axo::Settings`)

A layer's `settings` is a JSON object (`serde_json::Value`); how a host stores or produces it is the host's business. Each layer is validated on its own before merging, so a type error names the offending layer's `origin`. User and allowed project objects deep-merge key by key with the project winning; arrays replace. Unknown keys are ignored. Every field has a default:

```json
{
  "defaultModel": "anthropic/claude-sonnet-4-5",
  "modelRoles": {},
  "defaultThinkingLevel": "medium",
  "homeAgentsDir": false,
  "steeringMode": "one-at-a-time",
  "followUpMode": "one-at-a-time",
  "nestedContextFiles": true,
  "contextFiles": {
    "AGENTS.md": true,
    "AGENTS.local.md": true,
    "CLAUDE.md": true,
    "CLAUDE.local.md": true,
    ".agents/AGENTS.md": true
  },
  "bash": {
    "devShell": "off",
    "devShellRef": ".#default",
    "devShellWatch": [],
    "devShellTimeoutMs": 600000,
    "ci": true,
    "promoteAfterMs": 60000
  },
  "ask": {
    "timeoutMs": null
  },
  "compaction": {
    "enabled": true,
    "maxContext": 400000,
    "reserveTokens": null,
    "keepRecentTokens": null,
    "summaryRequest": "live"
  },
  "retry": {
    "maxRetries": 3,
    "baseDelayMs": 2000,
    "maxDelayMs": 60000
  },
  "project": {
    "files": {
      "skills": true,
      "config": true
    },
    "access": "read",
    "paths": {
      "/repo/**": "trust",
      "~/scratch/**": "ignore"
    }
  }
}
```

### Top-level keys

| Key | Rust field | Default | Scope | Meaning |
| :--- | :--- | :--- | :--- | :--- |
| `defaultModel` | `default_model` | `None` | User or project | A [model selector](#model-selectors). Accepts `$ENV` (see [Config values](#config-values)); resolved by `EffectiveConfig::default_model` only when a session falls back to it and references `$NAME`/`${NAME}` (`$$`/`$!` escapes alone leave it unchanged). Values starting with `!` are rejected when selected |
| `defaultThinkingLevel` | `default_thinking_level` | `"medium"` | User or project | `"off"`, `"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`, or `"max"` (clamped to the active model via `clamp_level`) |
| `modelRoles` | `model_roles` | `{}` | User only | Role name → selector (`provider/model[:thinking]` or `:thinking`); expanded by `@role`. A role cannot name another role. Ignored with a warning in project settings |
| `homeAgentsDir` | `home_agents_dir` | `false` | User only | Also load `~/.agents/AGENTS.md` and `~/.agents/skills/`. Ignored with a warning in project settings |
| `steeringMode` | `steering_mode` | `"one-at-a-time"` | User or project | `QueueMode::OneAtATime` (`"one-at-a-time"`) or `QueueMode::All` (`"all"`) |
| `followUpMode` | `follow_up_mode` | `"one-at-a-time"` | User or project | `QueueMode::OneAtATime` (`"one-at-a-time"`) or `QueueMode::All` (`"all"`) |
| `contextFiles` | `context_files` | 5 built-in names | User or project | Ordered map of relative paths to booleans merged onto the built-in default map |
| `nestedContextFiles` | `nested_context_files` | `true` | User or project | Attach unseen subdirectory context files onto `read`, `write`, and `edit` tool results |

### Model selectors

Model strings in `SessionOptions::model`, `defaultModel`, agent definitions, and `/model` accept `provider/model`, `@role`, either with `:thinking`, or `:thinking` alone. A bare model ID is not looked up. Exact model matches are tried before splitting a final thinking suffix, so IDs such as `ollama/qwen3:8b` remain intact.

For example, user settings `"modelRoles": {"bard": "anthropic/claude-sonnet-4-5:high"}` make `@bard:low` choose that model with low thinking. The selector's suffix overrides the role value's suffix. An unknown role is `Error::UnknownModelRole`; a role naming another role, or an invalid thinking-only or role suffix, is `Error::InvalidModelSelector`. An unmatched plain selector is `Error::ModelNotFound`. Hosts can call `Settings::resolve_model(&Models, selector)` to obtain a `ModelChoice` containing a model, a thinking level, or both.

At session start, model sources are `SessionOptions::model`, the session's last model, the root agent's model, then `defaultModel`. Thinking-only selectors continue to the next source; the first model wins, and later sources are not consulted. Thinking uses explicit `SessionOptions::thinking`, then the first selector thinking level encountered through the winning model, then the recorded thinking level, then `defaultThinkingLevel`, clamped to the model. If no source supplies a model, start returns `Error::NoModel`.

`/model` accepts the same selectors through `Session::run_command`: a thinking-only selector keeps the current model, and a model selector without a thinking suffix preserves the current thinking level (clamped to the new model). `Session::set_model` still takes a resolved `Model`, not a string.

### Project access (`project` / `axo::ProjectSettings`)

The `project` block is **user-only**: it is honored only in the user layer. In a project layer it is stripped before merging and logged as a warning, so a project can never choose its own access.

- `project.files.skills` (`true`): read project `.agents/skills/`.
- `project.files.config` (`true`): request the host's project layer (`ConfigSource::project`): its settings and prompt overrides. `false` keeps project access for context files and skills while never calling `project`.
- `project.access` (`"read"`): default `ProjectAccess` when no glob in `project.paths` matches the project root (the git root, or `cwd` outside a repository).
- `project.paths` (`{}`): ordered map of globs over the project root to `"ignore"`, `"read"`, or `"trust"`. Evaluated in map order with the last matching pattern winning. `~/` expands to `$HOME` (skipped if `HOME` is unset), and `/` is matched literally (`literal_separator(true)`).

| `ProjectAccess` | Effect |
| :--- | :--- |
| `Ignore` (`"ignore"`) | `ConfigSource::project` is not called. Loads no project settings, prompt overrides, context files, or skills; `bash.devShell` resolves to `"off"` |
| `Read` (`"read"`) | Applies project settings and prompt overrides, and loads enabled markdown context files and skills. Below `Trust`, `bash.devShell` resolves to `"off"` with a warning naming the winning origin, even if enabled in user settings |
| `Trust` (`"trust"`) | Permits project settings and lets `bash.devShell` take effect regardless of which layer enabled it. Config values starting with `!` remain unsupported |

A Rust host can override the resolved access level per session via `SessionOptions::project_access`; it takes precedence over the user policy.

### Execution environment (`bash` / `axo::BashSettings`)

| Key | Rust field | Default | Meaning |
| :--- | :--- | :--- | :--- |
| `bash.devShell` | `dev_shell` | `"off"` | `DevShellMode::Off` (`"off"`), `Direnv` (`"direnv"`), or `Nix` (`"nix"`). Takes effect only at `Trust`, whether enabled in user or project settings |
| `bash.devShellRef` | `dev_shell_ref` | `".#default"` | Flake output reference appended to the flake directory when capturing with `nix print-dev-env` |
| `bash.devShellWatch` | `dev_shell_watch` | `[]` | Additional globs relative to the flake root whose mtime/size changes trigger recapture |
| `bash.devShellTimeoutMs` | `dev_shell_timeout_ms` | `600000` | Timeout in milliseconds for `nix print-dev-env` or `direnv export json` |
| `bash.ci` | `ci` | `true` | Set `CI=true` in the command environment |
| `bash.promoteAfterMs` | `promote_after_ms` | `60000` | Promote a still-running command to a background job after this many milliseconds; `0` disables timed promotion, but explicit async execution and pending steering still promote. Applies to `bash` and host tools using `SessionTools::run_command` |

In `DevShellMode::Nix`, `DevShellEnv` finds the nearest ancestor containing `flake.nix` (stopping at the git root), captures the environment by evaluating `nix print-dev-env` output in a clean `bash` (GC root `--profile` at `<cache_dir>/devshell/<hash>/gcroot` under the host's `AxoBuilder::cache_dir`, where `<hash>` is a 16-hex-digit key of flake dir, ref and system), and watches `flake.nix`, `flake.lock`, git-tracked `*.nix` files, and `devShellWatch`. Concurrent `bash` calls share a single in-flight capture per key. If capture fails, commands fall back to the host environment and prefix the next `bash` result with a capture failure notice (once per session).

Nix mode needs a cache directory: without `AxoBuilder::cache_dir` it logs `bash.devShell "nix" needs a cache directory (AxoBuilder::cache_dir); using the host environment` and uses `StaticEnv` with the host environment snapshot. Direnv mode needs none.

In `DevShellMode::Direnv`, `DevShellEnv` searches for the nearest `.envrc` from `cwd` up to the filesystem root `/` and executes `direnv export json` bounded by `devShellTimeoutMs`. An unallowed or failing `.envrc`, or a missing `direnv` executable, falls back silently to the host environment. Lookups and exports are cached for 5 s.

Every subprocess `DevShellEnv` runs (`direnv export json`, `git ls-files`, `nix print-dev-env`, and the capture `bash` that sources the dev-shell script) goes through the session's `ProcessSupervisor` in its own container (a process group or, in cgroup mode, a job cgroup), as an internal foreground capture: it is never a background job, never promotable, and never listed under `proc://`. `devShellTimeoutMs` kills the whole container, not just the direct child, and the capture `bash` container is killed as soon as the environment dump is read, so services or traps started by `shellHook` die with it. A supervision failure (cgroup mode without a usable root or a supervisor that does not report ready) is handled like any other capture failure: resolution falls back to the host environment (with the capture failure notice in `Nix` mode). Environment resolution always finishes before the `bash` tool starts its command, so the background-promotion timer and steer promotion only begin once the command runs; `Session::abort` still cancels a capture in progress.

### Command supervision (`axo::Supervision`)

Every local command, `bash` tool calls and dev-shell capture alike, runs in its own container through the session's `ProcessSupervisor`. The host picks the container kind with the `Supervision` enum; there are two modes and no third: no PID namespaces, no `unsafe` or in-process fork path, and no silent downgrade from one mode to the other.

`Supervision` is host configuration, not a settings key; no configuration layer can change it. Hosts pass it to `AxoBuilder::supervision`; without one, `Supervision::default()` (`ProcessGroup`) applies:

| Variant | Fields | Meaning and limits |
| :--- | :--- | :--- |
| `ProcessGroup` (default) | none | Each command leads its own process group. Stopping or timing out a `bash` command sends the group SIGTERM, then SIGKILL after 1 s; dropping an unclosed `Process` (or an `output` deadline) sends SIGKILL immediately. A best-effort `sh` guard kills the group if the host process dies. Needs no helper executable, no delegation, and no privileges. Descendants that leave the group (`setsid`, double fork) are not tracked: they survive the job and are not listed under `proc://` |
| `Cgroup { program, args, cgroup_root }` | `program: PathBuf`, `args: Vec<OsString>`, `cgroup_root: Option<PathBuf>` | Each command gets its own cgroup v2 job directory, so every descendant, detached or not, stays contained and killable. axo runs the host-provided supervisor command `program args… guard <dir>` and `program args… enter <dir> <command…>`; the host must route those arguments to `axo::exec::supervisor::run`. A bare `program` name is resolved on axo's own `PATH` at spawn. `cgroup_root` is an existing delegated cgroup v2 directory under which job cgroups are created; `None` uses axo's own cgroup, read from the `0::` line of `/proc/self/cgroup` under `/sys/fs/cgroup`. Fails closed when unavailable |

No supervisor binary is distributed: the supervisor is the host's own executable (or any executable the host controls) with a routing prefix of the host's choosing. The route must be the first thing `main` does, synchronously, before any runtime, threads, or host setup:

```rust,no_run
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use axo::JsonlStore;
use axo::{Axo, Supervision};
use axo_ai::{FileCredentialStore, Models, ModelsConfig, ModelsOptions};

const SUPERVISOR_ROUTE: &str = "__axo-supervisor";

fn main() -> ExitCode {
    // axo spawned this process as a guard or entry: hand over before anything else.
    if std::env::args_os().nth(1).as_deref() == Some(OsStr::new(SUPERVISOR_ROUTE)) {
        return axo::exec::supervisor::run(std::env::args_os().skip(2));
    }
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async {
        let state = PathBuf::from("/var/lib/my-host");
        let models = Models::load(ModelsOptions {
            http_client: None,
            cache_dir: None,
            extended_context: false,
            config: ModelsConfig::default(),
            credentials: Arc::new(FileCredentialStore::new(state.join("credentials.json"))),
        })
        .await
        .expect("models");
        let store = Arc::new(JsonlStore::new(state.join("sessions")));
        let supervision = Supervision::Cgroup {
            program: std::env::current_exe().expect("current executable"),
            args: vec![OsString::from(SUPERVISOR_ROUTE)],
            cgroup_root: None,
        };
        let _axo = Axo::builder(Arc::new(models), store)
            .supervision(supervision)
            .build();
        // ... run sessions ...
    });
    ExitCode::SUCCESS
}
```

Hosts that keep the default need no routing at all: `Axo::builder(models, store).build()` runs commands in process groups.

- **Delegated cgroup (cgroup mode only):** the root must be a cgroup v2 directory the axo process can create subdirectories in, with `cgroup.kill` available (Linux 5.14+). Under systemd that means running the host in a unit or scope with `Delegate=yes` (for example `systemd-run --user --scope -p Delegate=yes …`), or creating a delegated subtree and passing it as `cgroup_root`. Systemd only provides the delegation; axo never calls systemd at runtime and needs no systemd API.
- **When it is checked:** `AxoBuilder::build` is synchronous and infallible, so supervision never fails it. In cgroup mode, the first command probes the root by creating and removing `axo-probe-<uuidv7>` and checking for `cgroup.kill`; the result, success or failure, is cached for the lifetime of that `Axo`. Process-group mode has nothing to probe.
- **Cgroup root:** whether explicit (`cgroup_root`) or derived from `/proc/self/cgroup`, the root is canonicalized (symlinks resolved) before the probe and must lie under `/sys/fs/cgroup`; one that does not fails with the reason `it is not inside the cgroup v2 hierarchy at /sys/fs/cgroup`.
- **Failures (cgroup mode only):** an unusable root reports `Command supervision unavailable: cannot create a job cgroup under {root}: {err}. Run the host in a delegated cgroup (systemd: Delegate=yes), set cgroup_root in Supervision::Cgroup, or use Supervision::ProcessGroup.` (where `{err}` is the OS error, `/proc/self/cgroup names no cgroup v2 path`, `it is not inside the cgroup v2 hierarchy at /sys/fs/cgroup`, or `it has no cgroup.kill (cgroup v2 on Linux 5.14+ required)`); a supervisor that cannot start reports `Command supervisor {program} failed to start: {err}`; one that starts but does not report `ready` within 5 s reports `Command supervisor {program} did not report ready; the host must route these arguments to axo::exec::supervisor::run`. The `bash` tool returns the message as its error and runs nothing; dev-shell resolution falls back as described above. axo never switches to process groups on its own.
- **Per command, process-group mode:** the command starts with `process_group(0)`, so its pgid is its pid. The guard is `sh -c 'read _; kill -KILL -- -"$0"' <pgid>` holding a pipe from axo, started in its own process group as well: if axo dies, the pipe closes and the guard kills the group; when the job closes normally axo kills the guard first. The guard is best effort (if `sh` cannot start, a warning is logged and the command still runs), and losing the guard never kills the job. Because neither the command nor the guard shares the host's foreground process group, a Ctrl-C or SIGTSTP at the host's terminal reaches no job and no watcher.
- **Per command, cgroup mode:** each command gets `<root>/axo-job-<uuidv7>`. A `guard` process stays in axo's own cgroup and watches axo through a pidfd while axo watches it; whichever survives the other writes `cgroup.kill` and removes the directory, so detached (`setsid`, double-forked, `nohup`) descendants never outlive both. Only the simultaneous death of axo and the guardian leaves a job behind. The command itself starts as `enter`, which joins the job cgroup, verifies the move, and then `exec`s the program. The guardian and the `enter` root each start in their own process group (`process_group(0)`), like the command in process-group mode, so terminal signals aimed at the host reach neither. The supervisor process inherits the host's own (trusted) launch environment, which a wrapped host executable may need to start at all; the command environment travels over `enter`'s stdin and is applied only after cgroup entry, at `exec`, so nothing in the job environment (such as `LD_PRELOAD`) runs before the process is inside its cgroup. `enter` exits 125 when it cannot join the cgroup, 127 when the program is not found, and 126 for other `exec` failures.
- **Descendants:** in process-group mode, in-group descendants stay supervised, drained to the command's artifact, and inspectable and killable through `proc://` (see [Background Jobs](prompts-and-context.md#background-jobs-cratesaxosrcjobsrs)); `setsid` escapees need cgroup mode. When the host is PID 1 or a child subreaper, a job's leftover processes are reparented to it; axo reaps those zombies itself (a non-blocking `waitpgid` on the job's group) so the container empties and the job can close instead of hanging on them.

### Question tool (`ask` / `axo::AskSettings`)

| Key | Rust field | Default | Meaning |
| :--- | :--- | :--- | :--- |
| `ask.timeoutMs` | `timeout_ms` | `None` | Optional timeout in milliseconds for `ask` tool calls. When it elapses, each question auto-selects its `recommended` option (or the first option if none is valid), and the answer is marked `(auto-selected after timeout)` |

### Compaction (`compaction` / `axo::CompactionSettings`)

| Key | Rust field | Default | Meaning |
| :--- | :--- | :--- | :--- |
| `compaction.enabled` | `enabled` | `true` | Enable automatic threshold maintenance and one-shot overflow recovery. Manual `Session::compact()` and `Session::shake()` work even when `false` |
| `compaction.maxContext` | `max_context` | `Some(400_000)` | Cap on the effective context window `W`; an explicit `null` clears the cap (`None`) |
| `compaction.reserveTokens` | `reserve_tokens` | `None` | Explicit token reserve; when `null` or omitted, derived (see [Window and trigger math](sessions-and-compaction.md#window-and-trigger-math-limitsresolve)) |
| `compaction.keepRecentTokens` | `keep_recent_tokens` | `None` | Explicit retained-tail and user-carryover budget; when `null` or omitted, derived (see [Window and trigger math](sessions-and-compaction.md#window-and-trigger-math-limitsresolve)) |
| `compaction.summaryRequest` | `summary_request` | `"live"` | `SummaryRequest::Live` (`"live"`) or `SummaryRequest::Separate` (`"separate"`) |

### Transient retries (`retry` / `axo::RetrySettings`)

These settings govern both failed turns and summary requests; they do not change the fixed request-level transport retries.

| Key | Rust field | Default | Meaning |
| :--- | :--- | :--- | :--- |
| `retry.maxRetries` | `max_retries` | `3` | Maximum retries after the initial attempt; `0` disables turn and summary retries |
| `retry.baseDelayMs` | `base_delay_ms` | `2000` | Exponential backoff base in milliseconds, doubled for each retry when no server delay is supplied; an unrepresentable delay stops retrying |
| `retry.maxDelayMs` | `max_delay_ms` | `60000` | Maximum accepted server-requested delay in milliseconds; a larger hint stops retrying rather than being clamped |


## Models (`axo_ai::ModelsConfig`) & Credentials

The host builds `Models` and passes it to `Axo::builder`; axo never loads models itself. `Models::load(ModelsOptions { cache_dir, extended_context, config, credentials, http_client })` combines the built-in catalog, the `models.dev` catalog (cached as `<cache_dir>/models.dev.json` and fetched when needed; `cache_dir: None` runs without the catalog and does no catalog I/O), and the host's `ModelsConfig`. `ModelsOptions::http_client` optionally supplies the HTTP client shared by catalog fetches, discovery, adapters, and OAuth refresh. Without one, the default client sends `User-Agent: axo/<version>` and has no read or global request timeout; a supplied client's policies take precedence. A host that keeps its `ModelsConfig` as JSONC text can parse it with `ModelsConfig::parse_jsonc` (UTF-8 BOM, comments, and trailing commas accepted; empty text is the default config). Its JSON shape:

```json
{
  "providers": {
    "zebes": {
      "api": "openai-completions",
      "baseUrl": "http://zebes:8080/v1",
      "apiKey": "local",
      "models": [
        {
          "id": "qwen3.8-27b",
          "name": "Qwen 3.8 27B",
          "contextWindow": 65536,
          "maxTokens": 16384,
          "reasoning": true
        }
      ]
    },
    "anthropic": {
      "modelOverrides": {
        "claude-sonnet-4-5": {
          "contextWindow": 200000
        }
      }
    }
  }
}
```

- **`ApiKind` (`api`):** `"anthropic-messages"`, `"openai-completions"`, or `"openai-codex-responses"`.
- **Auto-discovery:** If a custom `"openai-completions"` provider omits `models` (`None`) and supplies `baseUrl`, `Models::load` queries `<baseUrl>/models` to populate its model list.
- **Price-tier window caps:** For catalog and built-in models with `cost.tiers`, `ModelsOptions::extended_context` set to `false` caps `context_window` at the lowest `inputTokensAbove` threshold in `cost.tiers`. Models defined in the `ModelsConfig` or patched via `modelOverrides.contextWindow` keep their explicit window. `Models` is built once per host, so this is a host choice, not a settings key.
- **Credentials (credential store and environment):** `Models` resolves credentials in order:
  1. `ProviderConfig::api_key` in the `ModelsConfig` (a [config value](#config-values): `$ENV` or literal).
  2. Stored credentials in `ModelsOptions::credentials` (`Credential::ApiKey` or `Credential::Oauth`). `FileCredentialStore::new(path)` keeps them in one JSON file the host names, replaced by rename with mode `0600`; every write, including a Codex OAuth refresh, holds an exclusive lock on the `<path>.lock` sidecar.
  3. The provider's `env` variable names from the models.dev catalog (for example `$ANTHROPIC_API_KEY`).
  4. Custom providers absent from the catalog may run without an API key, including those using `"anthropic-messages"`. There is no per-request API-key override; `Models::auth(&model)` uses the same provider-owned resolution.

## Config values

Three kinds of value are resolved by `axo_ai::resolve_config_value` instead of being used as written: `apiKey` and `headers` values in the `ModelsConfig` (on providers, models, and `modelOverrides`), and `defaultModel` in settings.

| Form | Result |
| :--- | :--- |
| Leading `!` | Rejected with a config error; axo never executes command config values |
| `$NAME` / `${NAME}` | Replaced by the environment variable; can appear anywhere in the value (`"Bearer $TOKEN"`) |
| `$$` / `$!` | A literal `$` / `!` (start a literal value that begins with `!` with `$!`) |
| anything else | Used as written |

```json
{
  "providers": {
    "zebes": {
      "api": "openai-completions",
      "baseUrl": "http://zebes:8080/v1",
      "apiKey": "$ZEBES_API_KEY",
      "headers": { "X-Team": "$ZEBES_TEAM" }
    }
  }
}
```

- **When:** `apiKey` and `headers` resolve when a request needs them, and also while `Models::load` discovers a provider's models. `defaultModel` resolves through `EffectiveConfig::default_model` only when a session create or open falls back to it (no `SessionOptions::model` and no recorded model); `Axo::schemes` never resolves it.
- **No command cache:** config-value resolution performs environment expansion only and caches no command results.
- **Failures:** a leading `!`, or an unset or empty environment variable, is a config error.
- **Host-resolved values:** if a host obtains configuration by executing code, it must do so itself before handing the resulting values to axo.
