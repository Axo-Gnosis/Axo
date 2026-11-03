# Running agent commands inside Nix dev shells — study for axo

Scratch work: `/tmp/axo-research-NixShellStudy/` (scratch flake, `bench*.sh`, `capture.sh` prototype). Machine: 24 cores, Nix 2.34.9, direnv 2.37.1, nix-direnv 3.2.0 (Home Manager). Nothing under /repo was edited. Side effect to know: the timing runs against `/repo/rust/axome` ran its shellHook, and that hook runs `git config --local core.hooksPath .githooks`. The value was already `.githooks`, and it still is.

## 1. What pi and omp do today

- **pi (v1.3.0): no dev-shell support.** "Nix" in pi only means installing pi through its own flake. pi does provide the general hooks that dev-shell support would plug into:
  - `BashOperations.exec(command, cwd, {onData, signal, timeout, env})`, a replaceable backend (`/tmp/pi-src/packages/coding-agent/src/core/tools/bash.ts:75-94`).
  - `BashSpawnHook = (ctx: {command, cwd, env}) => BashSpawnContext`, which runs before every spawn (`bash.ts:178-211`).
  - The `shellCommandPrefix` setting, which is prepended to every command (`settings-schema.ts:272`, applied at `bash.ts:268`).
  - A dev shell could be bolted on through the spawn hook (an extension), but nothing ships.
- **omp: direnv only, and solid.** It has no flake or `print-dev-env` support.
  - `src/exec/direnv.ts` walks up to the nearest `.envrc` (`:10-24`), with a 5 s per-directory cache (`:60-67`).
  - It runs `direnv export json` and parses `{VAR: value|null}` into set/unset (`:33-52`).
  - It **respects `direnv allow`**: a blocked or denied `.envrc` is never run (`:262-296`, settings text at `settings.ts:165-177`).
  - **Warm path:** it keeps the *loaded* env and `DIRENV_*` state. Each call re-runs `direnv export json` from the loaded env, which prints an empty diff when nothing changed (`:345-356`). It also stats `.envrc` and every direnv watch path with nanosecond precision (`:95-153`). If those stats are unchanged and the last verification was under 5 s ago, it skips spawning direnv at all (`:68-70`, `:346-349`).
  - The base env strips `DIRENV_*` and is checksummed, so a process-env change forces a cold reload (`:177-214`).
  - It drains stderr and stdout concurrently, because a cold devenv load can fill the pipe (`:236-242`).
  - **Merging:** `applyDirenvPreflight` puts direnv's `set` under the caller's env. For variables direnv *unsets*, it prepends `unset -v A B; ` to the command, gated by a variable-name regex (`bash-executor.ts:174-204`).
  - **Settings:** `bash.direnv: auto|off` and `bash.direnvLoadTimeoutMs` (30 s). The load is also clamped to the command timeout (`bash-executor.ts:184-187`). Services and terminals skip direnv (`docs/bash-tool-runtime.md:117,156-172`).
  - **Consequence for Toph:** omp only helps repos that have an `.envrc`. Neither `/repo/rust/axome` nor `/repo/rust/axo` has one.
- **Axome already captures dev shells, per folder** (`src-crates/axome-core/src/services/nix.rs`):
  - It detects `flake.nix`, then `shell.nix`, then `default.nix` (`:9`).
  - It runs `nix develop <dir> --no-write-lock-file --command env -0` from a clean base env, with a 600 s timeout (`:221-231`; `axome-protocol/src/settings.rs:13-16`), or `nix-shell --run 'env -0'`.
  - It keeps only the variables that differ from the host (`:293-296`) and stores them in SQLite (`folder_nix_envs`). Re-evaluation is **manual**, through the `NixEvaluate` RPC; I found no automatic invalidation.
  - `with_nix` merges PATH as dev shell first, then host. It never overrides `HOST_AUTHORITATIVE_VARS` (HOME, USER, SHELL, LANG, DISPLAY, SSH_AUTH_SOCK, …), and it sets `__NIXOS_SET_ENVIRONMENT_DONE=1` (`services/env.rs:9-24,130-151`).
  - **Axome bug found:** its capture keeps `TMPDIR`/`NIX_BUILD_TOP=/tmp/nix-shell.XXXX`, which are not in the host-authoritative list. The directory survives `nix develop`'s exit (measured below), but tmpfiles eventually deletes it, so a capture that lives long enough points `TMPDIR` at a missing directory. [INFERENCE for the tmpfiles part. 159 leaked `nix-shell.*` directories exist on this box, the oldest about 9.4 days old, which is consistent with a 10-day cleanup.]

## 2. Measurements

Done with hyperfine (`-N`, 8–15 runs, means). The scratch flake is a mkShell with jq, ripgrep, cargo and a shellHook that prints, exports a variable and sets an EXIT trap. Axome's flake is a real GUI/Rust/Bun dev shell with 132 variables and 106 bash functions.

| Technique | Scratch flake | Axome flake |
|---|---|---|
| Very first `nix develop -c true` (fetch inputs + substitute cargo/jq/rg) | ~19 s wall | n/a (already warm) |
| `nix develop -c true`, eval cache warm | **120 ms** | **464 ms** |
| `nix print-dev-env --json`, warm | 110 ms | 441 ms (first call: 597 ms) |
| Same, dirty tree with unchanged content | 119 / 109 ms (still a cache hit in Nix 2.34) | — |
| `flake.nix` edited before every run (re-evaluation, nothing to build) | 513 ms | — (expect more; nixpkgs eval dominates) |
| `nix develop <saved --profile> -c true` (skips evaluation; still runs the hook) | 69 ms | — |
| nix-direnv: first `direnv export json` | 293 ms (env **empty**, see pitfalls) / 846 ms without the trap | — |
| nix-direnv: cache hit from a clean env (sources the cached `.rc`) | 53 ms | — |
| `direnv export json` from the already-loaded env, nothing changed (omp's warm path) | 6.0 ms | — |
| nix-direnv after a `flake.nix` edit | 669 ms | — |
| **axo-style capture once**: `print-dev-env` → eval in `bash --noprofile --norc` → `env -0` (`capture.sh`) | 130 ms | ~450 ms [INFERENCE: 441 + ~10] |
| Per command: `bash -c true` with the host env (baseline) | 2.1 ms | |
| Per command: `env -i <~75 captured vars> bash -c jq` | 5.6 ms (includes running jq) | |
| Per command: `source` the print-dev-env script (re-runs the hook and defines 98 functions) | 7.5 ms | |

What this means per session: with 200 bash calls in axome, wrapping each in `nix develop -c` adds about **93 s** of overhead and runs the shellHook 200 times. Capturing once costs about 0.45 s plus about 3 ms per call. Where the warm `nix develop` time goes: 110 ms is a floor (Nix startup, flake lock, eval cache). Axome's extra ~330 ms grows with repository size, because Nix has to fingerprint and copy the git working tree. [INFERENCE: I did not profile the split.]

## 3. Technique comparison

| Technique | Cost per command | Invalidation | Hook semantics | Fit for an agent |
|---|---|---|---|---|
| `nix develop --command` on every call | 0.1–0.5 s and up | Always correct (Nix eval cache keyed on the tree) | Hook runs **every call**. Leaks one `/tmp/nix-shell.*` directory per call (measured) | Simple but slow; the hook repeating is a real problem |
| `nix print-dev-env --json` once, use only `variables` | ~0 after capture | Yours to implement | **Misses everything the hook exports.** Axome's `LD_LIBRARY_PATH`, `LIBCLANG_PATH`, `BUN_INSTALL` and EGL/GBM paths are only set in its shellHook (verified absent from the JSON). Also includes `HOME=/homeless-shelter`, `TMPDIR=/build`, `SHELL`=nix bash, `TZ=UTC` | **Wrong on its own** |
| `print-dev-env` (bash script) evaluated once in bash, then `env -0` and diff (Axome-like, nix-direnv-like) | ~3 ms | Yours to implement | Hook runs **once per capture**, exactly as in Toph's terminal | **Best for flakes** |
| `nix develop --profile P` once, then `nix develop P -c` | 69 ms | Manual | Hook every call | Middle ground; no reason to pick it |
| Long-lived shell inside `nix develop`, commands sent through stdin | ~0 | Manual | Hook once | Leaks state between calls (cd, exports). Conflicts with axo's process group per command, parallel tools and replay. Reject |
| direnv + nix-direnv (`direnv export json`) | 6 ms warm (omp path), 53 ms clean | nix-direnv mtimes on `flake.nix`, `flake.lock`, `devshell.toml`, `.envrc`, direnvrc, plus user `watch_file` | Hook once per load; respects `direnv allow` | **Best when the repo has `.envrc`**; uses the user's existing trust setup |
| Legacy `nix-shell` / `shell.nix` (`--run`) | seconds; no eval cache [INFERENCE] | Manual | Hook every call | Capture once, same as flakes (`nix-shell --run 'env -0'`, as Axome does) |
| devenv (`devenv shell`, `use devenv`) | It has its own incremental eval cache, claimed "sub 100ms" ([github.com/cachix/devenv](https://github.com/cachix/devenv)) | devenv's | Starts processes/services only through `devenv up`, not the shell [INFERENCE] | Cover it through direnv (`use devenv`), as omp does; no special path |
| lorri | Background daemon that rebuilds on change | Daemon | — | Superseded in practice by nix-direnv [INFERENCE]; skip |

## 4. Invalidation and correctness pitfalls (with evidence)

- **When to recapture.** Recapture when `flake.nix`, `flake.lock` or `.envrc` change. Also recapture when **any imported `.nix` file** changes. nix-direnv misses that case: it only watches `flake.nix`, `flake.lock` and `devshell.toml` (`~/.config/direnv/lib/hm-nix-direnv.sh:319-326`). Axome's dev shell lives in `nix/devshell.nix`, so editing it would *not* invalidate nix-direnv without a `watch_file` line. Axome has 11 tracked `*.nix` files (`git ls-files '*.nix'`).
  - Cheap and correct enough: at capture time, record `stat(mtime_ns, size)` for `flake.nix`, `flake.lock`, `.envrc` and every git-tracked `*.nix` under the flake directory, plus user globs. Re-stat them per call; that costs microseconds.
  - Nix's own eval cache is the precise truth (the tree fingerprint), but asking it costs 110–440 ms. Use it only for an explicit reload.
  - Inputs that are not `.nix` (`readFile ./rust-toolchain.toml`, `fromTOML Cargo.toml`) need a `watch` setting or a manual reload.
- **Garbage collection.** A cached environment points at store paths. After `nix-collect-garbage`, PATH entries dangle. nix-direnv guards against this with `print-dev-env --profile` GC roots (`hm-nix-direnv.sh:213-235,371`), and axo should do the same: keep a profile symlink in its cache directory.
- **Hooks that print.** The hook's output goes to stderr in `nix develop -c` (observed); send it to a log during capture. **Hooks that set an EXIT trap break direnv.** The scratch hook `trap 'echo bye' EXIT` replaced direnv's own dump-at-exit trap, so `direnv export json` returned **only `DIRENV_*` keys**. It failed silently (measured: 4 keys versus 68 without the trap). In axo's own capture, dump the env *before* exit to a dedicated file or file descriptor (or put markers around it, as Axome does at `env.rs:208-209`), so trap output cannot corrupt it.
- **Hooks with side effects** (git config, starting services, `exec $SHELL`, prompts): capture with `stdin=/dev/null`, its own process group that is killed after the dump, and a timeout. Note in the docs that services the hook starts with `setsid` will survive. `nix develop -c` repeats these side effects on every call.
- **Variables to drop or restore from the host:**
  - **Restore** `TMP`, `TMPDIR`, `TEMP`, `TEMPDIR`, `NIX_BUILD_TOP`, `terminfo` (nix-direnv does this at `hm-nix-direnv.sh:172-199`), and `rm -rf` the `nix-shell.*` directory. The prototype confirms that `TMPDIR` otherwise points at the directory you just removed.
  - **Host wins** for Axome's list: HOME, USER, LOGNAME, SHELL (the dev shell sets it to the Nix store bash), LANG/LC_*, DISPLAY/WAYLAND, XDG_RUNTIME_DIR, DBUS, SSH_AUTH_SOCK, and TERM.
  - Keep `SOURCE_DATE_EPOCH=315532800` and `IN_NIX_SHELL=impure`, for parity with `nix develop` and nix-direnv, which both keep them.
- **PATH order.** The dev shell comes first, then the host. `print-dev-env` already appends the saved outer PATH (`env.sh:2147`). After diffing, recompute it as `devshell_entries ++ host_PATH` at spawn time, so later host PATH changes still apply. Deduplicate as Axome's `merge_paths` does.
- **NixOS login shells clobber PATH.** Measured: `bash -lc` puts `/run/wrappers/bin` first and drops the dev-shell priority. Plain `bash -c` and `fish -c` keep it. Never use `-l`, and set `__NIXOS_SET_ENVIRONMENT_DONE=1` (Axome's fix) for any user-shell path.
- **Impure env / baseline.** Capture from a **clean, fixed baseline** (Axome's `environment.base`, omp's `cleanSpawnEnv`). If axo itself started inside some *other* dev shell, which happens when a dev build of Axome runs inside axome's own shell, the inherited `LD_LIBRARY_PATH` and similar would leak in. Axome's `AXOME_ORIG_*` restore exists for this reason (`env.rs:29-48`). axo should take its base env from the host and never re-read `std::env` per call.
- **Monorepos and subdirectories.** `nix develop` run from `sub/deeper` resolves `.` to the nearest `flake.nix` above it (verified). axo should key its cache on the *resolved* flake directory plus the attribute, find the nearest flake up to the git root, and allow a per-project override. Nested flakes mean that the nearest one wins.
- **Picking `devShells.<system>.<name>`.** The default is `.#default`, and `nix develop .#other` works (verified). Make the attribute a project setting, never a model choice.
- **Lock files.** Always pass `--no-write-lock-file`. An agent's environment probe must never rewrite `flake.lock` (Axome does this too).
- **Trust.** Evaluating a flake and running its shellHook executes repository code. direnv's allow list is a real trust gate, and omp respects it. axo has no trust gate yet (`core.md:374`), and `!command` config values already execute project-controlled code (`core.md:340`). So automatic flake capture is no worse than the status quo. It should still be a *user*-level opt-in, not something a project file can enable.

## 5. Proposed seam for axo

**Where it lives:**
- The seam goes in **core `axo` (`axo::exec`)**, next to the bash tool. The design already promises "exec seams" for this (`core.md:317`).
- The dev-shell provider is a **compiled-in Rust module** (`axo::exec::devshell`), not an extension. It runs before every bash call and has to cost microseconds. The Bun extension host is the wrong place for that, and the logic is pure process plumbing.
- Extensions can still replace the provider later, the same way pi's `spawnHook` allows.

```rust
// axo::exec
pub type EnvMap = BTreeMap<OsString, OsString>;

/// What the environment a command runs with is. Resolved per call, cheap when warm.
#[async_trait]
pub trait ExecEnv: Send + Sync {
    async fn resolve(&self, cwd: &Path, cancel: &CancellationToken,
                     progress: &dyn Fn(&str)) -> Resolved;
}
pub struct Resolved {
    pub env: Arc<EnvMap>,         // full env; spawn does env_clear().envs(...) — no `unset -v` prefix hack
    pub notice: Option<String>,   // e.g. "dev shell failed to load (…tail…); using host env" — shown once in the tool result
}
pub struct HostEnv(Arc<EnvMap>);                  // default: base snapshot taken at startup
pub struct StaticEnv(Arc<EnvMap>);                // hosts (Axome) hand in their own per-folder env
pub struct DevShellEnv { base: Arc<EnvMap>, cfg: DevShellConfig, cache: Mutex<HashMap<Key, Entry>> }
// SessionOptions { exec_env: Option<Arc<dyn ExecEnv>>, .. }  — None = built from settings
```

- **Returning a full map** instead of an overlay removes omp's `unset -v` command rewriting. The warm path is: stat the watch list, then clone an `Arc`. Spawning with `env_clear().envs(map)` allocates nothing beyond what the spawn already needs.
- **`DevShellEnv` resolution order:**
  1. An allowed `.envrc`: use direnv. Port omp's warm path, which is "loaded env + empty diff ⇒ reuse", plus stat guard within a TTL.
  2. Otherwise, a `flake.nix` (or `shell.nix`): use the capture.
  3. Otherwise, the host env.
- **Flake capture steps:**
  1. Run `nix print-dev-env --no-write-lock-file --profile <cache>/gcroot <ref>`.
  2. Evaluate the result in `bash --noprofile --norc` with a clean baseline, `stdin=/dev/null` and its own process group.
  3. Dump the env to a file descriptor before exit, then `rm -rf $NIX_BUILD_TOP`.
  4. Diff against the baseline and drop or restore the variables listed in §4.
  5. Store it in memory and in `~/.axo/cache/devshell/<hash(flake_dir, ref, system)>/{env.json, watches.json, gcroot}`. That makes a restarted ACP process warm, and it never writes `.direnv/` into the repo.
- **Concurrency:** use one in-flight capture per key (single-flight). Parallel bash calls wait on it, and the wait reports `progress("preparing Nix dev shell…")` as a `ToolUpdate`.
- **Durability:** a capture is idempotent, so it needs no special handling.
- **Timeouts and failures:** the default timeout is 600 s (Axome's number). A cold build can be legitimate, so it is not clamped to the command timeout; omp's clamp would make a cold build fail repeatedly. If capture fails, run with the host env and include `notice` once, with the stderr tail.
- **Invalidation:** use the stat watch list from §4. A `reload` lives on the session API (`session.reload_exec_env()`); the ACP binary exposes it as a slash command later.
- **What the model sees:** one stable line in the `cwd` system-prompt section, which keeps the prompt cache-stable and contains no hashes:
  `Shell commands run inside the Nix dev shell of <flake_dir> (<ref>); its tools are already on PATH, so don't wrap commands in "nix develop".`
  Failures and reloads appear only as tool-result notices, never as prompt edits. Don't list packages; the model can run `which`.
- **Configuration** (`settings.jsonc`, deep-merged, project wins except where noted):
  ```jsonc
  "bash": {
    "devShell": "direnv",          // "off" | "direnv" | "auto" (direnv, else flake/shell.nix). Shipped default "direnv"; Toph sets "auto" globally.
                                   // Enabling flake capture is honored only from the user file (trust); a project may lower it to "off".
    "devShellRef": ".#default",    // project-level: attribute/path, e.g. ".#rust" or "path/to/dir#ci"
    "devShellWatch": ["rust-toolchain.toml"], // extra invalidation globs relative to the flake dir
    "devShellTimeoutMs": 600000
  }
  ```
- **fish and bash:**
  - The tool runs `bash -c`, as pi does, and the env map works the same for any shell, so fish needs nothing special.
  - bash functions from `print-dev-env` (`genericBuild`, `runHook`; 98 in scratch, 106 in axome) are deliberately **not** carried over. Agents need binaries and variables, not stdenv phases. If someone ever needs them, sourcing the script per call costs 7.5 ms.
  - Never use login shells. Always set `__NIXOS_SET_ENVIRONMENT_DONE=1`. If axo later runs Toph's fish for `!` commands, pass it the same env map.
- **Axome integration:** Axome already has its own per-folder capture and UI. When it embeds axo, it passes `StaticEnv(folder_env(...))`, or a small adapter that re-reads its DB, and leaves axo's provider off, so the environment is never captured twice. Later, Axome could switch to `DevShellEnv` to get automatic invalidation and GC roots, and to fix its TMPDIR issue.

**Estimated cost (Rust):**
- Seam (`ExecEnv`, `HostEnv`, `StaticEnv`, bash wiring, notice plumbing): about 60–90 lines.
- direnv provider (an omp port: find `.envrc`, export, warm path, allow/deny): about 150–200 lines.
- Flake/shell.nix capture with diff, var policy, GC root, single-flight, disk cache and watch list: about 300–400 lines.
- Prompt line, settings and reload API: about 40 lines.
- Checks: one regression check for diff/merge (PATH order, restored TMPDIR, host-authoritative vars, unsets), plus a smoke run against the scratch flake (hook export captured, EXIT trap harmless, edit triggers recapture).
- **Total: about 550–750 lines.** Roughly 1–1.5 days of agent time [INFERENCE].

## Recommendation for axo

1. **Now (core seam, M3):** add `ExecEnv`, `HostEnv` and `StaticEnv` to `axo::exec`, plus `SessionOptions.exec_env`, and have the bash tool spawn with `env_clear().envs(resolved)` using a base snapshot taken at startup. This costs about 80 lines and closes off the omp-style `unset -v` rewriting and pi's `commandPrefix` hack. Without it, M3's acceptance test ("runs tests in a real repo") in Toph's repos only passes if axo itself is started inside `nix develop`.
2. **Also M3, if it fits; otherwise first thing after M3 and before M6:** build `DevShellEnv` with **flake capture first**. Toph's repos (axome, axo) have flakes and no `.envrc`, so omp's direnv-only approach would do nothing for him. Use the capture-once design in §5, which measured about 130–450 ms per capture and about 3 ms per call (versus 120–464 ms per call with `nix develop -c`). Recapture on the stat watch list, keep a GC-root profile, and restore TMP* and host-authoritative variables.
3. **Next:** add the direnv path, ported from omp's warm-path logic, for `.envrc` and devenv repos. It is about 200 lines, and it is the safe default for a shipped `"direnv"` setting because it respects `direnv allow`.
4. **Reject:** running `nix develop --command` on every call (slow, repeats hook side effects, leaks temp directories), using the `print-dev-env --json` variables alone (misses hook exports, which breaks axome), a persistent stdin shell, and lorri.
5. **For Axome (outside axo's scope, worth telling Toph):** its `nix develop -c env -0` capture freezes `TMPDIR=/tmp/nix-shell.*`, has no GC root and no automatic invalidation. When embedding axo, pass Axome's env through `StaticEnv`; later, consider switching to `DevShellEnv`.
