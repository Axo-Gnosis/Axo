# The shell tool: pi vs omp, and what to do for a fish user

Sources: pi v1.3.0 at `/tmp/pi-src` (abbreviated `PI/` = `packages/coding-agent/src/`), omp at `/nix/store/lb6r…-source` (abbreviated `OMP/` = `packages/coding-agent/src/`, `CR/` = `crates/`). Experiments ran on this machine: NixOS, fish 4.9.3, bash 5.3.15, home-manager. Scratch output is in `/tmp/axo-research-ShellToolStudy/`.

## 1. pi's bash tool (what axo's M3 currently ports)

Checked against `plan/pi/context.md` §bash (lines 275-301) and the source.

- **Schema.** `{command, timeout?}`, where `timeout` is in seconds and has no default (`PI/core/tools/bash.ts:40-43`). The snippet is "Execute bash commands (ls, grep, find, etc.)" (`:45-48`).
- **Shell.** The `shellPath` setting if it exists, otherwise `/bin/bash`, otherwise `bash` on PATH, otherwise `sh`. Args are `-c <cmd>` (`PI/utils/shell.ts:86-139`).
  - **pi never looks at `$SHELL`.** With no `shellPath`, a fish user gets bash.
  - **NixOS has no `/bin/bash`** (checked: only `/bin/sh` exists, and it links to bash-interactive), so pi falls through to `bash` on PATH.
- **Env.** `process.env` with `~/.pi/agent/bin` prepended to PATH (`shell.ts:171-183`). The tool gets whatever environment the process was launched with. No login shell is involved and no rc file is sourced.
- **Process model.** One fresh process per call, in its own process group (`detached:true`), with stdin ignored.
  - **Kill.** Abort and timeout send SIGKILL to the group, with **no TERM grace** (`shell.ts:220-251`).
  - **Wait.** After exit, output is drained until it has been idle for 100 ms.
- **Output.** stdout and stderr are merged. After 50 KB or 2000 lines the raw bytes spool to `$TMPDIR/pi-bash-*.log`. The model sees the tail: the last 2000 lines or 50 KB. There is no ANSI stripping on the tool path.
- **Results.** A non-zero exit returns `isError` plus "Command exited with code N". Abort and timeout throw.
- **Not in pi:** persistent state, `cwd` param, background jobs, interceptor, PTY, non-interactive env hardening, per-line cap.

## 2. omp's bash tool in depth

### 2.1 Architecture: it does not run bash

- **Commands run inside omp's own process.** Every model-issued command goes to an embedded **brush** shell, a bash-compatible shell written in Rust (brush-core 0.5.0, MIT). It is vendored and patched (`Cargo.toml:35-39`; brush-parser is patched for heredocs inside `$(...)`). The executor header says "Uses brush-core via native bindings" (`OMP/exec/bash-executor.ts:1-5`). `procmgr.ts:141-148` adds: "bash tool commands always execute in the embedded brush-core shell. The resolved binary only serves the spawn-a-shell paths (interactive PTY sessions, ACP client terminals, SHELL env)".
- **Many builtins run in-process.** `crates/pi-builtins` reimplements a large share of coreutils plus `rg`, `fd`, `sed`, `jq` (via jaq) and `ps` as builtins: 150 files, about 112k lines. Disabling them requires `PI_DISABLE_UUTILS_BUILTINS` (`docs/bash-tool-runtime.md:150`).
- **Size.** brush-core is about 27.7k lines, brush-parser about 10.1k, and `pi-shell` about 42k, including the output "minimizer" (`CR/pi-shell/src/minimizer*`, adapted from rtk-ai).
- **Why they did it** (from the docs and comments):
  1. Windows without Git Bash.
  2. `scheme://` internal URLs working inside redirects, globs and `cd` through an injected filesystem (`bash-tool-runtime.md:34`).
  3. Persistent per-session state without having to parse output for sentinel markers.
  4. Pid tracking and kill control.
  5. Output minimizing.
- **What it costs:**
  - **Compatibility gaps.** One example: aliases expand by whitespace split (brush issue #57), so the rc snapshot has to drop any alias containing `( | & ; < >` (`OMP/utils/shell-snapshot.ts:31-74`).
  - **The bundled jq is not jq.** It is jaq, with a long list of differences from jq 1.8 (`bash-tool-runtime.md:152-154`).
  - **Maintenance.** omp carries a fork of brush.

### 2.2 Feature by feature

- **Schema.** `command`, `timeout?`, `cwd?`, `pty?`, and two optional groups (`OMP/tools/bash.ts:327-371`):
  - with `async.enabled`: `async?`;
  - with `launch.enabled`: `name?` (≤48 chars) and `ready{log?,port?,host?,timeout?}`.
  - **No `env` param.**
  - **The prompt is compressed and assertive** (`OMP/prompts/tools/bash.md:1-8`): "Persistent shell…", "`cwd`, not `cd`", "No `head`/`tail`/redirection; output trunc… full result at `artifact://<id>`", "…move to background job `bg_N`… NEVER poll."
- **cwd.** A `cwd` param resolves against the session cwd. A leading single-line `cd X && …` is extracted into `cwd` when the model didn't pass one (`bash.ts:983-994`). Every call runs at the session cwd (or the given `cwd`, `:1052`), so **`cd` never persists between calls**.
- **Persistent state.** One brush `Shell` per session (`bash-executor.ts:206-210`, `:669-690`). Variables, functions and options persist between calls. cwd does not.
  - **Parallel calls get a throwaway shell.** When the session shell is busy, an overlapping call runs in a one-shot shell and gets the notice "Ran in a temporary shell… nothing set here persists" (`bash.ts:447-449`). So the model can never fully rely on persistence.
  - **Failures reset the shell.** A timeout, cancel or failed run quarantines the shell, and the next call starts fresh (`bash-tool-runtime.md:144`).
- **rc snapshot.** If the configured shell is bash or zsh, omp sources the user's `.bashrc`/`.zshrc` once and dumps functions, aliases and options into a file brush sources (`shell-snapshot.ts:1-7`, `:106-111`). This matches Claude Code's approach. For a fish user it does nothing: the shell isn't bash, so `.profile` is the only fallback.
- **Environment.** Layered from lowest to highest priority:
  1. the process env filtered of the launch project's dotenv values;
  2. omp's additions `SHELL`, `GIT_EDITOR=true`, `GPG_TTY`, `OMPCODE=1`, `CLAUDECODE=1`, `CI=true` (`packages/utils/src/procmgr.ts:43-55`);
  3. a **non-interactive hardening table** (`OMP/exec/non-interactive-env.ts:6-53`): about 45 vars, including PAGER/GIT_PAGER/GH_PAGER=cat, `TERM=dumb`, `NO_COLOR=1`, `EDITOR=true`, `GIT_TERMINAL_PROMPT=0`, `SSH_ASKPASS=false`, `PIP_NO_INPUT`, `npm_config_yes`, `DEBIAN_FRONTEND`, `CARGO_TERM_PROGRESS_WHEN=never`;
  4. the **direnv/devenv diff** for the cwd, cached per `.envrc`, plus `unset -v` for variables the `.envrc` removes (`bash-executor.ts:174-204`; NixShellStudy covers this part).
- **Timeout and kill.**
  - **Timeout.** The default is **300 s**. `0` disables it, and other values are clamped to 1-3600 (`bash-tool-runtime.md:89`).
  - **Kill.** Cancel sends SIGTERM to the owned process groups and their descendants, waits up to 1 s, then sends SIGKILL (`:189`; `CR/pi-shell/src/process.rs:848-885`). omp refuses to signal its own process group (`process.rs:1018-1027`). Each command runs in a new process group (`CR/pi-shell/src/shell.rs:1367`). `nohup x &` double-forks out of the tree.
- **Async and background jobs, and how results come back.**
  - **Explicit.** `async:true` starts a managed job and immediately returns "running, job id".
  - **Automatic.** `bash.autoBackground` is on by default with a **60 s threshold**. A foreground call races completion against the threshold, abort, and the turn's **steering signal** (`OMP/async/auto-background.ts:16-74`). If the threshold wins, the job is promoted and the tool returns the output so far plus "backgrounded as bg_N". If a queued user message arrives, it backgrounds early so the message can be injected (`bash.ts:1185-1249`).
  - **Delivery.** On completion, `AsyncJobManager` queues an `async-result` delivery to the owning session's sink. It is injected as a **follow-up message** that wakes the agent, with retry, dead-lettering and retention (`OMP/async/job-manager.ts:151-163`, `:1102-1191`). The model can also block with `wait` or read `proc://`.
  - **Cost.** job-manager.ts alone is 1,317 lines.
- **Named services.** `name` plus `ready` (log regex or TCP port) starts a broker-owned PTY process that is replaced on reuse, persists across turns, and is inspected, fed and killed via `proc://<name>` (`bash-tool-runtime.md:112-124`).
- **PTY mode.** `pty:true` runs the command in a real PTY rendered in a TUI overlay (xterm-headless; keys are forwarded; `esc` kills it). It is only available with a UI, otherwise it falls back with a notice (`OMP/tools/bash-pty-selection.ts`, `bash-interactive.ts:19-60`). The non-interactive hardening is removed and `TERM=xterm-256color` is set. Its purpose is sudo prompts, interactive installers, TUIs.
- **ACP client terminal.** If the ACP client advertises `terminal`, foreground non-PTY commands run in the editor's terminal via `terminal/create`. omp wraps the line in `[shell, -l, -c, line]` because ACP spawns argv directly (`bash.ts:140-166`).
- **Interceptor** (`OMP/tools/bash-interceptor.ts`, rules in `OMP/exec/settings.ts:17-78`).
  - **Default.** **Off** (`bashInterceptor.enabled: false`).
  - **What it matches.** Regex rules against the whole command, then against flat `&&`/`;`/`|` fragments with leading `VAR=` stripped. Stages that receive piped stdin are excluded. A rule blocks only if the suggested tool is present. It blocks:
    - `cat`/`head`/`tail` → read;
    - `grep`/`rg` → grep;
    - `find`/`fd` with `-name` → glob;
    - `sed -i`/`perl -i` → edit;
    - `echo`/`cat <<` redirection into a file → write;
    - `nohup` or a trailing `&`, dev servers, `tail -f`, `--watch` → bash with `name`.
  - **First-hand evidence:** this research session runs under it. It blocked `grep -n … | head`, `cat Cargo.toml`, and a legitimate `find … | xargs wc -l` line count. Each block cost a turn to re-route. It does push the model toward the dedicated tools, but it misfires on read-only pipelines.
- **Approval patterns.** Allow/deny/prompt globs, plus `CRITICAL_BASH_PATTERNS`: `rm -rf /`, `curl|sh`, `mkfs`, fork bomb and similar (`bash.ts:185-230`). Matching is per segment. Allow rules never cover compound syntax. The docs say outright that this is "not containment" (`bash-tool-runtime.md:48`).
- **Worktree rewrite.** `git worktree add …` is rewritten to `omp worktree add` for clone-first worktrees (`OMP/tools/bash-worktree-rewrite.ts:69-88`). This is omp-specific.
- **Output** (`OutputSink`, `bash-tool-runtime.md:210-245`):
  - **Inline budget.** 50 KB total, split as a **20 KB head plus a rolling tail** with a middle-elision marker.
  - **Line cap.** **768 bytes per line**, after which the line is ellipsis-truncated.
  - **Cleaning.** Text is sanitized and Kitty/Sixel images are extracted.
  - **Artifact.** The full stream goes to `artifact://<id>`, capped at 16 MB as head plus tail. There is no line cap.
  - **Minimizer.** It rewrites verbose tool output and keeps the raw text as a separate artifact.
  - **Notices.** Live previews stream the sink's own view. "Read artifact://<id> for full output" is appended automatically.
- **Errors.** A non-zero exit returns an error result with `exitCode`. A timeout returns an error result with `timedOut`. Only cancellation throws (`bash-tool-runtime.md:255-270`).

### 2.3 pi vs omp, and what axo should do

Cost is a rough estimate of Rust lines for axo.

| # | omp difference from pi | Why omp does it | axo | Cost |
|---|---|---|---|---|
| 1 | Embedded brush plus in-process coreutils instead of spawning bash | Windows, `scheme://` paths inside the shell, pid control, persistence | **Skip.** Linux/Nix-first. Real bash is the dialect models trained on. 150k+ lines of fork to carry. URL args can be translated at the exec seam later. | huge |
| 2 | Persistent per-session shell (vars/functions; cwd not persisted) | Fewer re-exports and re-sourced setup | **Skip.** Parallel calls lose it anyway. It conflicts with `Replay::Never` recovery and adds hidden state. A stateless call is easier to reason about at 3am. | 400+ |
| 3 | `cwd` param plus leading-`cd` extraction | Model ergonomics, approval matching | **Skip for M3.** `cd x && …` works in a fresh process. Revisit if approvals arrive. | 40 |
| 4 | Default timeout 300 s (pi has none) | Hung commands | **Keep axo's decision** (no default). Note that it becomes safe in practice once #5 exists. | 0 |
| 5 | Auto-background at 60 s plus `async` plus follow-up delivery plus `wait` | Long builds and tests don't block the turn; steering stays responsive | **Later**, with the `wait` tool. **Shape the seam now** (see the end of this report). | 600-900 |
| 6 | Named services (`name`/`ready`, `proc://`) | Dev servers and watchers stay observable | **Later**, after #5. Axome may own services instead. | 500+ |
| 7 | SIGTERM, 1 s grace, then SIGKILL (pi: SIGKILL at once) | Lets cargo/tests/servers clean up locks and temp files | **Adopt in M3.** | ~15 |
| 8 | Non-interactive env table (pagers=cat, EDITOR=true, GIT_TERMINAL_PROMPT=0, SSH_ASKPASS, npm/pip/cargo flags, `CI=true`, `NO_COLOR`, `TERM=dumb`) | Stops hangs on prompts and pagers and noisy progress bars | **Adopt in M3** as a static table under the process env. `CI=true` behind a setting, since it changes some tools' behavior. | ~60 |
| 9 | Inline output = 20 KB head plus tail (pi: tail only) | The first compiler error is usually at the top | **Adopt in M3.** This revises core.md's "bash keeps the tail". Spool unchanged. | ~40 |
| 10 | Per-line cap of 768 B | Minified JS or JSON lines eat the budget | **Adopt in M3.** | ~20 |
| 11 | ANSI/CR sanitizing on the tool path (pi: raw) | Clean context | **Adopt in M3.** Strip CSI/OSC and fold `\r` progress lines. | ~40 |
| 12 | Interceptor (off by default) | Push the model to the dedicated tools | **Skip.** axo's grep/find are on by default and the prompt can say so. It misfires on pipelines (seen first-hand). Maybe later as an opt-in. | 150 |
| 13 | Approval patterns and critical-command list | Policy, not sandboxing | **Later**, with an approval system. Not in M1-M6. | 300 |
| 14 | PTY mode with TUI overlay | sudo, interactive installers | **Skip.** axo has no TUI. The ACP terminal covers the need. | — |
| 15 | Route to the ACP client's `terminal/create` | Command visible in the editor | **Later (M6+).** The exec seam makes it a backend. Wrap as `bash -c line`. | 150 |
| 16 | rc snapshot (aliases/functions from `.bashrc`) | Users' bash aliases | **Skip.** Useless for fish users and brittle. | — |
| 17 | direnv/devenv preflight | Project tools on PATH | See NixShellStudy. **Later or opt-in.** | 200 |
| 18 | Output minimizer (rtk filters) | Token savings | **Skip.** Lossy, large, and the gains are unproven here. | large |
| 19 | Worktree rewrite, git guard, gh-cache invalidation | omp features | **Skip.** | — |
| 20 | Non-zero exit and timeout as error *results*; only cancel throws (pi throws on timeout) | Consistent results | **Already axo** (`is_error`, core.md:168). | 0 |

## 3. Fish

### 3.1 How the harnesses treat a fish login shell

- **pi.** It ignores `$SHELL` entirely and always uses bash, unless `shellPath` is set.
  - Setting `shellPath=/…/fish` would run `fish -c <cmd>` while the tool is still called `bash` and described as "Execute bash commands" (`PI/utils/shell.ts:26-28`, `:86-93`). That is a guaranteed dialect mismatch.
  - The env is simply inherited from the process.
- **omp.** The tool *always* speaks bash, because it runs brush.
  - **`$SHELL` is accepted only if it contains `bash` or `zsh`.** Otherwise omp falls back to `bash`, then `sh` (`procmgr.ts:245-259`). A fish `$SHELL` is ignored for tool calls.
  - **Fish is used only for the user's `!` commands** (`useUserShell`), on a headless PTY, with `-l` stripped for fish so `status is-login` blocks don't fire on every command (`OMP/exec/bash-executor.ts:387-412`, `:542-553`).
  - **Elsewhere omp treats fish as a hazard.** The approval classifier marks fish as "unknown quoting" (`bash-tool-runtime.md:42`). The `ssh://` code wraps remote snippets in an explicit shell, because a fish login shell can't parse `if [ … ]; then` (`OMP/ssh/utils.ts:31-35`).
- **Claude Code.** Same picker, bash/zsh only ([anthropics/claude-code#7490](https://github.com/anthropics/claude-code/issues/7490), open, 38 comments). It also **puts `Shell: fish` in the system prompt**. The model then writes fish syntax, which fails in zsh ([caillou.ch, 2026-08-20](https://caillou.ch/blog/2026-08-20-claude-code-bash-tool-runs-zsh/)). Lesson for axo: **never advertise the login shell in the prompt if it isn't the shell that runs.**

### 3.2 What breaks when bash habits meet fish (measured, fish 4.9.3)

I ran 30 common bash idioms through `fish --no-config -c` and `bash -c` (script in the scratch dir).

- **Works in both (13):** `&&`/`||`, `$(…)`, `VAR=x cmd`, `export X=y`, `[ … ]`, `2>&1`, `&>`, `;`, brace expansion `{a,b}`, `~`, `cd x && pwd`, double-quoted `$HOME`, glob with `2>/dev/null`.
- **Hard fail (14):** `[[ ]]`, `if …; then … fi`, `for …; do … done`, `while read`, **heredoc `<<EOF`**, here-string `<<<`, `$?`, `${X}`, `X=1; …` (bare assignment), `$((1+2))`, `(subshell)`, `f() { … }`, `case … esac`, process substitution `<(…)`.
- **Silently wrong (3):**
  - backticks print literally;
  - `set -euo pipefail` *erases variables* and exits 0;
  - `$PIPESTATUS` is empty.
- **Evidence about models.** I found no benchmark. The anecdotes all point the same way:
  - "Every AI I've used tries to make heredoc" ([fish-shell#12759](https://github.com/fish-shell/fish-shell/issues/12759); the maintainers refuse to add heredocs);
  - the CC#7490 thread;
  - the blog above, where the model "retried in bash until something stuck".
- **[INFERENCE]** Training data and the agent tool traces are overwhelmingly bash, and Anthropic's own trained-in tool is named `bash`. Expect noticeably more failed calls in fish, mostly on heredocs, loops and conditionals, which are exactly what agents use to write files and run checks.

### 3.3 Does Toph's environment even differ between bash and fish?

On this machine, home-manager manages both shells.

- **Clean login envs are nearly identical.** `env -i HOME=… bash -l` and `env -i HOME=… fish -l` each give 85 vars. Fish adds only two PATH entries (a python3 and grc from `config.fish`) and `PYTHONNOUSERSITE` (scratch: `cleanbashl`, `cleanfishl`).
- **Capture cost.** `fish -l -c 'env -0'` takes about 100 ms. `bash -l` takes about 3 ms.
- **An inherited PATH (e.g. from `nix develop`) survives both `bash -l` and `fish -l`** (tested with `PATH=/devshell/bin:$PATH`).

So for Toph the env problem is small. It only matters when axo isn't started from a terminal: Axome launched from the desktop, or as a systemd user service, gets a minimal env.

### 3.4 Options

- **(a) Real bash, inheriting the user's environment.** The model writes the dialect it knows best, which is the dialect the tool says it runs.
  - **Launched from a fish terminal or `nix develop`:** inheriting the process env is already right. That is what pi does, and it costs nothing.
  - **Launched headless:** the *host* supplies the env. Axome can capture it once with `$SHELL -l -c 'env -0'`, about 100 ms per process, cached. axo exposes this as an opt-in setting such as `shellEnv: "inherit" | "login"`; the field name is illustrative.
  - **Never capture by default.** Capturing would override a `nix develop` env with the login env.
- **(b) Run fish and call the tool `fish`.** This respects the user's shell, but:
  - it loses the trained-in `bash` tool prior;
  - heredocs, loops and `[[ ]]` fail (14/30 above);
  - there are silent semantic traps (`set -e`);
  - the model also needs fish-specific prompt guidance.

  Rejected.
- **(c) A generic `shell` tool with the dialect in the description.** This keeps the ambiguity the CC thread complains about ("if it did respect your shell wouldn't you want the tool to be called something else?" — @rekram1-node in #7490). Models default to bash habits whatever the name. Rejected.

## Recommendation for axo

**Fish and the tool name (M3):**

- **Option (a).** The tool is named **`bash`** and always runs real GNU bash.
- **Resolving bash.** Use `shellPath` if set (documented as "must be bash-compatible"), otherwise `bash` on PATH, otherwise `/bin/bash`, otherwise `sh`. Prefer PATH over `/bin/bash`: NixOS has no `/bin/bash`, and inside `nix develop` PATH bash is the project's.
- **Never read `$SHELL`.** Never put the user's login shell in the system prompt. The tool description should say "Runs in bash (not your login shell)".
- **Env.** Inherit the process env by default. Add an opt-in login-env capture via `$SHELL -l -c 'env -0'`, run once and cached, for hosts like Axome that start headless. Cost: about 40 lines, which could land in M3 or right after.
- **If fish is ever needed,** the model can run `fish -c '…'` inside bash.
- **Persistent scripts Axo writes for Toph stay fish.** That is a prompt/AGENTS.md rule, unrelated to the tool's runtime.

**Adopt in the M3 first set** (about 175 lines on top of the pi port):

- #7 SIGTERM → 1 s → SIGKILL on the group;
- #8 the non-interactive env table under the process env;
- #9 head (20 KB) plus tail inline, with the full spool unchanged. Update core.md's "keeps the tail";
- #10 the 768 B per-line cap;
- #11 ANSI/CR sanitizing.

Keep pi's schema `{command, timeout?}`, no default timeout, stateless processes, error *results* for exit and timeout, and a session-dir spool.

**Supervision correction (2026-11-03):** the original recommendation here used `PR_SET_PDEATHSIG`. Actual axo instead has an external pipe-EOF process-group guard. Measured probes show that group escapes survive it, while PDEATHSIG can kill a child when its spawning Tokio blocking thread expires even though the host process remains alive. The running-handle, steering and durable-delivery seams below are still unbuilt, not capabilities supplied by the current follow-up queue. See [`track3-supervision.md`](track3-supervision.md) for the source discrepancies, measurements and lifecycle constraints.

**Later:**

- #5 auto-background/async plus `wait`;
- #6 named services;
- #15 the ACP terminal backend;
- #13 approvals;
- #17 direnv (per NixShellStudy).

**Skip:** brush/in-process coreutils, the persistent shell, the interceptor, PTY, the rc snapshot, the minimizer, and the omp-specific rewrites.

**Core seams needed now (M2/M3), so that #5, #6 and #15 don't need a retrofit:**

1. **The exec seam returns a running handle,** not a finished future: output stream/spool, `wait()`, `kill(grace)`, pid/pgid. A session-level process registry owns it, not the tool-call future. Auto-backgrounding then just means "stop awaiting, keep the handle, register a job". ACP terminal and SSH become alternative handle backends.
2. **A background completion is a submission.** Use the existing follow-up queue, as core.md:326 already intends, with a distinct kind (`job_result{job_id}`) so `wait` can consume it and steering can interrupt a foreground wait. That is omp's "steer → background early" behaviour (`OMP/tools/bash.ts:1239-1245`), and it falls out of the loop's steering signal.
3. **The env comes from the host** through the exec seam (`SessionOptions`/exec-backend env), so Axome can inject a captured login env without axo reading dotfiles.
4. **Durability.** A running job's spool path and job id are recorded in the session log. Recovery reports "bg_N interrupted, partial output at …", the same way `Replay::Never` reports an interrupted bash call today.
