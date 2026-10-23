# Configuration & Directories

`axo` loads configuration from the user directory (`~/.axo/`, overridden by `AXO_DIR`) and, when permitted by the user's project access settings, the project directory (`<cwd>/.axo/`) and `.agents/`. Hosts embedding `axo` in Rust can also pass `Settings`, `Models`, `Streamer`, and `SessionStore` directly via `Axo::builder()` without reading config files.

## Directory Layout

### User directory (`~/.axo/` or `$AXO_DIR`)

Resolved by `AxoDir::from_env()` (`AXO_DIR` when set and non-empty, else `$HOME/.axo`):

| Path | Method on `AxoDir` | Purpose |
| :--- | :--- | :--- |
| `settings.jsonc` | `settings_path()` | User settings (model, thinking level, project access rules, context files, dev shells, `ask` timeout, compaction) |
| `models.jsonc` | `models_path()` | Custom providers, API keys/commands, base URLs, and per-model patches |
| `auth.json` | `auth_path()` | Machine-written OAuth and API key credentials (`FileCredentialStore`, mode `0600`, locked during refresh) |
| `AGENTS.md` | `root().join("AGENTS.md")` | User-wide instructions loaded at the start of `project_context` |
| `prompts/<NAME>.md` | `prompt_path(name)` | Prompt overrides: `SYSTEM.md`, `WORK.md`, `PROSE.md`, `SUMMARY.md`, `COMPACTION.md`, `COMPACTION_UPDATE.md`, `COMPACTION_SPLIT_TURN.md`, `BRANCH_SUMMARY.md` |
| `skills/` | `skills_dir()` | User skills (`<skill>/SKILL.md`) |
| `sessions/` | `sessions_dir()` | Default `JsonlStore` root (`--<encoded-cwd>--/<timestamp>_<uuidv7>.jsonl`), per-session `<session>/bash/` spool directories, and `<session>/context/` maintenance artifacts |
| `cache/` | `cache_dir()` | Cached `models.dev` catalog (`cache/models.json`), bundled documentation (`cache/docs/`, via `docs_dir()` / `docs_path()`), and captured Nix dev-shell environments (`cache/devshell/`) |

When `homeAgentsDir` is `true` in `~/.axo/settings.jsonc`, `axo` also reads `~/.agents/AGENTS.md` (before `~/.axo/AGENTS.md`) and `~/.agents/skills/` (after `~/.axo/skills/`).

### Project directories (`<cwd>/.axo/` and `.agents/`)

| Path | Access required | Gate in `project.files` | Purpose |
| :--- | :--- | :--- | :--- |
| `<cwd>/.axo/settings.jsonc` | `read` (`trust` for code-running keys) | `axoDir: true` | Project settings merged on top of user settings |
| `<cwd>/.axo/prompts/<NAME>.md` | `read` | `axoDir: true` | Project overrides for `SYSTEM.md`, `WORK.md`, `PROSE.md`, and summarizer prompts |
| `.agents/skills/` | `read` | `skills: true` | Project skills discovered from `cwd` up to the git root |
| `AGENTS.md`, `CLAUDE.md`, … | `read` | `contextFiles` map | Context files loaded from the git root to `cwd` at startup and on demand in subdirectories |

## `settings.jsonc` (`axo::Settings`)

Settings files use JSON with Comments (JSONC), parsed with `jsonc-parser` (UTF-8 BOM, comments, and trailing commas are accepted). Each file is validated individually before merging so syntax or type errors identify the offending path. User and allowed project objects deep-merge key by key with the project winning; arrays replace. Unknown keys are ignored.

```jsonc
{
  "defaultModel": "anthropic/claude-sonnet-4-5",
  "defaultThinkingLevel": "medium",
  "extendedContext": false,
  "homeAgentsDir": false,
  "steeringMode": "one-at-a-time",
  "followUpMode": "one-at-a-time",
  "nestedContextFiles": true,
  "contextFiles": {
    "AGENTS.md": true,
    "AGENTS.local.md": true,
    "CLAUDE.md": true,
    "CLAUDE.local.md": true,
    ".agents/AGENTS.md": true,
    ".axo/AGENTS.md": true
  },
  "bash": {
    "devShell": "off",
    "devShellRef": ".#default",
    "devShellWatch": [],
    "devShellTimeoutMs": 600000,
    "ci": true
  },
  "ask": {
    "timeoutSecs": null
  },
  "compaction": {
    "enabled": true,
    "maxContext": 400000,
    "reserveTokens": null,
    "keepRecentTokens": null,
    "summaryRequest": "live"
  },
  "project": {
    "files": {
      "axoDir": true,
      "skills": true
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
| `defaultModel` | `default_model` | `None` | User or project | Model selector (`provider/id` or `id`). Accepts `$ENV` and `!command` (see [Config values](#config-values)); a `!command` value in project settings requires `ProjectAccess::Trust` |
| `defaultThinkingLevel` | `default_thinking_level` | `"medium"` | User or project | `"off"`, `"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`, or `"max"` (clamped to the active model via `clamp_level`) |
| `extendedContext` | `extended_context` | `false` | User only | When `false`, catalog and built-in model context windows are capped at the lowest `cost.tiers` threshold (`CostTier`). Ignored with a warning in project settings because `Models` is loaded once per `Axo` instance |
| `homeAgentsDir` | `home_agents_dir` | `false` | User only | Also load `~/.agents/AGENTS.md` and `~/.agents/skills/`. Ignored with a warning in project settings |
| `steeringMode` | `steering_mode` | `"one-at-a-time"` | User or project | `QueueMode::OneAtATime` (`"one-at-a-time"`) or `QueueMode::All` (`"all"`) |
| `followUpMode` | `follow_up_mode` | `"one-at-a-time"` | User or project | `QueueMode::OneAtATime` (`"one-at-a-time"`) or `QueueMode::All` (`"all"`) |
| `contextFiles` | `context_files` | 6 built-in names | User or project | Ordered map of relative paths to booleans merged onto the built-in default map |
| `nestedContextFiles` | `nested_context_files` | `true` | User or project | Attach unseen subdirectory context files onto `read`, `write`, and `edit` tool results |

### Project access (`project` / `axo::ProjectSettings`)

The `project` block is **user-only** (`~/.axo/settings.jsonc`). If present in `<cwd>/.axo/settings.jsonc`, it is stripped before merging and logged as a warning.

- `project.files.axoDir` (`true`): read `<cwd>/.axo/settings.jsonc` and `<cwd>/.axo/prompts/`.
- `project.files.skills` (`true`): read project `.agents/skills/`.
- `project.access` (`"read"`): default `ProjectAccess` when no glob in `project.paths` matches the project root (the git root, or `cwd` outside a repository).
- `project.paths` (`{}`): ordered map of globs over the project root to `"ignore"`, `"read"`, or `"trust"`. Evaluated in map order with the last matching pattern winning. `~/` expands to `$HOME` (skipped if `HOME` is unset), and `/` is matched literally (`literal_separator(true)`).

| `ProjectAccess` | Effect |
| :--- | :--- |
| `Ignore` (`"ignore"`) | Loads no project settings, prompt overrides, context files, or skills |
| `Read` (`"read"`) | Loads enabled markdown context files, skills, prompt overrides, and safe settings; strips `bash.devShell` and `!command` `defaultModel` from project settings with a warning. Dev shells remain disabled even if enabled in user settings |
| `Trust` (`"trust"`) | Permits all project settings, including `!command` `defaultModel`, and lets `bash.devShell` take effect regardless of which settings file enabled it |

A Rust host can override the resolved access level per session via `SessionOptions::project_access`.

### Execution environment (`bash` / `axo::BashSettings`)

| Key | Rust field | Default | Meaning |
| :--- | :--- | :--- | :--- |
| `bash.devShell` | `dev_shell` | `"off"` | `DevShellMode::Off` (`"off"`), `Direnv` (`"direnv"`), or `Nix` (`"nix"`). Takes effect only at `Trust`, whether enabled in user or project settings |
| `bash.devShellRef` | `dev_shell_ref` | `".#default"` | Flake output reference appended to the flake directory when capturing with `nix print-dev-env` |
| `bash.devShellWatch` | `dev_shell_watch` | `[]` | Additional globs relative to the flake root whose mtime/size changes trigger recapture |
| `bash.devShellTimeoutMs` | `dev_shell_timeout_ms` | `600000` | Timeout in milliseconds for `nix print-dev-env` or `direnv export json` |
| `bash.ci` | `ci` | `true` | Set `CI=true` in the command environment |

In `DevShellMode::Nix`, `DevShellEnv` finds the nearest ancestor containing `flake.nix` (stopping at the git root), captures the environment via `nix print-dev-env --json` with a GC root under `<AXO_DIR>/cache/devshell/<sha256>/`, and watches `flake.nix`, `flake.lock`, git-tracked `*.nix` files, and `devShellWatch`. Concurrent `bash` calls share a single in-flight capture per key. If capture fails, commands fall back to the host environment and append a capture failure notice to the tool result once.

### Question tool (`ask` / `axo::AskSettings`)

| Key | Rust field | Default | Meaning |
| :--- | :--- | :--- | :--- |
| `ask.timeoutSecs` | `timeout_secs` | `None` | Optional timeout in seconds for `ask` tool calls. When elapsed, questions with a `recommended` option select that option automatically; otherwise the call returns a timeout message |

### Compaction (`compaction` / `axo::CompactionSettings`)

| Key | Rust field | Default | Meaning |
| :--- | :--- | :--- | :--- |
| `compaction.enabled` | `enabled` | `true` | Enable automatic threshold maintenance and one-shot overflow recovery. Manual `Session::compact()` and `Session::shake()` work even when `false` |
| `compaction.maxContext` | `max_context` | `Some(400_000)` | Cap on the effective context window `W`; explicit `null` in JSONC clears the cap (`None`) |
| `compaction.reserveTokens` | `reserve_tokens` | `None` | Explicit token reserve; when `null` or omitted, derived as `min(min(max_tokens, 32768) + 4096, W / 4)` |
| `compaction.keepRecentTokens` | `keep_recent_tokens` | `None` | Explicit retained-tail and user-carryover budget; when `null` or omitted, derived as `min(20000, trigger / 4)` |
| `compaction.summaryRequest` | `summary_request` | `"live"` | `SummaryRequest::Live` (`"live"`) or `SummaryRequest::Separate` (`"separate"`) |

## `models.jsonc` (`axo_ai::ModelsConfig`) & Credentials

`Models::load` combines the built-in catalog, the cached or fetched `models.dev` catalog (`<AXO_DIR>/cache/models.json`), and `<AXO_DIR>/models.jsonc`:

```jsonc
{
  "providers": {
    "zebes": {
      "name": "Zebes Local",
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

- **`ApiKind` (`api`):** `"anthropic-messages"`, `"openai-completions"`, `"openai-responses"`, or `"openai-codex-responses"`.
- **Auto-discovery:** If a custom `"openai-completions"` provider omits `models` (`None`) and supplies `baseUrl`, `Models::load` queries `<baseUrl>/models` to populate its model list.
- **Price-tier window caps:** For catalog and built-in models with `cost.tiers`, `extendedContext: false` caps `context_window` at the lowest positive `up_to` tier threshold. Models defined in `models.jsonc` or patched via `modelOverrides.contextWindow` keep their explicit window.
- **Credentials (`auth.json` and environment):** `Models` resolves credentials in order:
  1. `ProviderConfig::api_key` in `models.jsonc` (a [config value](#config-values): `!command`, `$ENV`, or literal).
  2. Stored credentials in `<AXO_DIR>/auth.json` (`Credential::ApiKey` or `Credential::OAuth`, refreshed under a per-provider lock and persisted with mode `0600`).
  3. Provider environment variables (`ANTHROPIC_API_KEY`, `ANTHROPIC_OAUTH_TOKEN`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `GROQ_API_KEY`, `XAI_API_KEY`, `OPENROUTER_API_KEY`, `MISTRAL_API_KEY`, `DEEPSEEK_API_KEY`, `CEREBRAS_API_KEY`, `TOGETHER_API_KEY`, `FIREWORKS_API_KEY`, `PERPLEXITY_API_KEY`, `COHERE_API_KEY`).

## Config values

Three kinds of value are resolved by `axo_ai::resolve_config_value` instead of being used as written: `apiKey` and `headers` values in `models.jsonc` (on providers, models, and `modelOverrides`), and `defaultModel` in settings.

| Form | Result |
| :--- | :--- |
| `!command` | Runs `bash -c command` and uses its trimmed stdout. Only a leading `!` counts |
| `$NAME` / `${NAME}` | Replaced by the environment variable; can appear anywhere in the value (`"Bearer $TOKEN"`) |
| `$$` / `$!` | A literal `$` / `!` (start a literal value that begins with `!` with `$!`) |
| anything else | Used as written |

```jsonc
{
  "providers": {
    "zebes": {
      "api": "openai-completions",
      "baseUrl": "http://zebes:8080/v1",
      "apiKey": "!pass show zebes/api-key",
      "headers": { "X-Team": "$ZEBES_TEAM" }
    }
  }
}
```

- **When:** `apiKey` and `headers` resolve when a request needs them, and also while `Models::load` discovers a provider's models. `defaultModel` resolves once when settings load.
- **Caching:** each distinct `!command` runs at most once per process; its output (or failure) is reused until the process restarts, so a rotated secret needs a restart.
- **Failures:** a command that cannot start, exits non-zero, prints nothing, or runs past 10 seconds is a `Config` error, as is an unset or empty environment variable. stdin is closed and stderr is discarded.
- **Trust:** a `!command` runs code. A project `defaultModel` starting with `!` is stripped below `Trust` (see [Project access](#project-access-project--axoprojectsettings)); `models.jsonc` is user-only.
