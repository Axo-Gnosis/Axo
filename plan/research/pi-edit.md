# pi-edit anatomy (omp v18.12.2, `/nix/store/lb6rqm446k7xw01z0lmp2mjz73hkpl4p-source`)

Paths below are relative to that omp source tree unless noted. Dependency numbers come from real `cargo tree`/`cargo build` runs on a copy in `/tmp/axo-research-PiEditAnatomy/` (Linux x86_64 host, nixpkgs cargo/rustc 1.98, `-e normal`, unique packages, root excluded).

## 0. Correction to axo docs
`docs/core.md:223` and `docs/pi/context.md:426` say hashline reads put `N#TAG` on every line. **That is wrong.** The tag is per file, not per line:
```
[src/lib.rs#49F6]          ← one header per file: path + 4-hex snapshot tag of the whole file
1:fn a() {}                ← N:TEXT rows; no per-line hash
2:fn b() {
```
`core.md:229` asks whether pi-edit handles EOL itself. It does: `FileRead::persist` restores the BOM and line endings (`crates/pi-edit/src/files.rs:47-60`; `text.rs:28-66`).

## 1. The hashline protocol

### Read output
- Header: `[path#TAG]` (`format.rs:97-99`). TS shows a cwd-relative path, or a `~`-shortened absolute path outside the workspace (`packages/coding-agent/src/tools/read-format.ts:55-68`).
- Rows: `N:line` (`format.rs:108-110`). `format_numbered_lines` numbers every LF-split row, including a final empty row after a trailing newline (`format.rs:119-130`; my smoke run printed `7:` for a 6-line file).
- Read can also emit elision rows (`N-M: …`) and truncation notices (`[Showing …]`, `[… N ln elided; re-read needed ranges …]`). The edit parser recognizes these and ignores them if a model pastes them back (`prefixes.rs:14-52`).
- **TAG:** xxh32 (seed 0) of the LF-normalized, BOM-stripped text, with trailing space, tab and CR removed from each line. It is masked to 16 bits and printed as `{:04X}` (`store.rs:74-95`). So the tag is a 65,536-value snapshot ID, not a security hash.
- **Side effect of reading:** every display calls `EditStore::record(path, text, seen_lines)` (TS: `read-format.ts:78`, `read.ts:1651-1665`, `read.ts:2499-2667`; `write-content.ts:104`; ast-grep and file mentions do the same).
  - The store keeps up to 4 versions per path, 256 paths, 64 MiB total, and skips files over 4 MiB (`store.rs:14-25`).
  - It also keeps the set of line numbers the model actually saw (`seen_lines`, parsed back from the displayed body by `^[ *]?(\d+)(?:-(\d+))?:`, `store.rs:102-123`; `packages/coding-agent/src/edit/store.ts:30-50`).

### Edit request
- The tool schema is just `{ input: string }` (`packages/coding-agent/src/edit/schemas.ts:39-41`).
- For providers with custom-format tools there is also a Lark grammar (`crates/pi-edit/grammars/hashline.lark`, exposed via `lib.rs:60-66`).
- The model-facing prompt is `prompts/hashline.md`, 31 lines. It is the same text this harness uses.

Grammar (`hashline.lark:1-27`, constants in `format.rs:7-52`):
```
[PATH#TAG]                       section header (TAG required: [0-9A-F]{4})
PUT N.=M:  + body rows           replace inclusive range (N.=N for one line)
PUT N*:    + body                replace syntactic block starting at line N
PUT <N: / PUT >N: / >$ / <1      insert before/after line, EOF, BOF
PUT >N*:                         insert after the block opened at N
CUT N.=M [@reg] / CUT N* [@reg]  delete + capture into anonymous or named register
PUT <N @reg / PUT N.=M @reg      paste register (no body)
REM                              delete file;  MV DEST  rename after edits
```
- Body rows start with `+`; a lone `+` is an empty line. Line numbers always refer to the original file, never to positions shifted by earlier hunks.
- The parser lowers `PUT a.=b:` into per-line `Insert`/`Delete` edits (`types.rs:44-91`, `parser.rs`).
- It tolerates many model mistakes and turns them into warnings instead of errors (`parser.rs:15-20`): diff `-` rows, bare bodies, `*** Begin Patch` envelopes, apply_patch path noise (`input.rs:25-32`), and pasted `N:` prefixes.
- Named registers persist across calls in the store; anonymous ones last for one call (`store.rs:42-72`).

### Staging and stale-anchor behavior
`patcher::stage_patch` (`patcher.rs:592-736`) handles each section; `apply_with_recovery` (`patcher.rs:425-500`) decides what happens to stale anchors. In order:
1. **Tag matches the live file:** apply. If `enforce_seen_lines` is on (TS turns it on), every anchor must be a line the model was shown (`assert_seen_lines`, `patcher.rs:349-423`).
   - **Shift rescue:** unseen anchors are still accepted if they correspond to lines an older retained version displayed, and the payload contains the current content of both ends of the span (`patcher.rs:127-347`).
   - Otherwise the edit is rejected. The error reveals up to 40 of the unseen lines and records them as seen, so an immediate retry works.
2. **Tag mismatch, but every edit is a pure head/tail insert:** apply anyway, with a drift warning (`patcher.rs:473-483`).
3. **Tag mismatch, but the tag is a retained older snapshot:** `recovery::try_recover` (`recovery.rs:367-409`).
   - Builds a Myers line map from old to new text (`recovery.rs:65-83`, via pi-diff).
   - Requires each anchor's neighbors to map with the same offset, with stricter checks for duplicated lines (`recovery.rs:124-188`).
   - Requires the tree-sitter enclosing-construct chain to be unchanged at the remapped anchors (`recovery.rs:330-364`).
   - If all checks pass it applies, with a "recovered/remapped" warning.
4. **Otherwise:** a mismatch error (`mismatch.rs:75-119`).
   - If the tag is known for this file: "file changed between read and edit … copy the [path#newhash] header from that edit's response or re-read".
   - If not: "hash #X is not from this session", plus which other paths this session issued that tag for.
   - Both variants include the current text around the anchored lines.

Further behavior:
- **Path recovery:** if the authored path doesn't exist, rebind the section to the stored snapshot with the same tag and file name (`patcher.rs:524-555`). Separately, unique-suffix search finds the file under cwd (`path_policy.rs:171-212`, using pi-walker).
- **No-op guard:** the 3rd identical byte-for-byte no-op payload escalates to an error (`store.rs:420-431`, `patcher.rs:715-734`).
- **After a successful write:** the session records the written text and returns a fresh `[path#NEWTAG]` header. Seen lines carry over through the diff (`session.rs:332-393`).
- **Writes are host-owned:** an async `EditWriter` trait (`session.rs:72-75`). This matches axo's per-file mutation lock.

### What tree-sitter (pi-ast) is used for
| Use | Where | Without it |
|---|---|---|
| `N*` block ops: span of the outermost named node starting on line N | `hashline/block.rs:301-327` → `pi_ast::block::block_range_at` (`pi-ast/src/block.rs:1-9,60`) | `*` ops fail with "no block" |
| Suggesting the nearest block when `N*` misses (scans 64 lines) | `block.rs:58-102` | Plainer error |
| Enriching invalid-range errors with the proven block | `patcher.rs:47-50` | Plainer error |
| Boundary repair: does the result parse, picking among repaired variants, duplicated opener/closer echoes, landing inserts after an opener, "edit broke parse" warning | `apply.rs:280-297,432-584,624-728,792-973,1116-1158` via `syntax::{parses_cleanly,node_chain,enclosing_boundaries}` (`syntax.rs:47-111`) | Edits apply as written; the heuristics that need a parse switch off |
| Stale-recovery safety: enclosing construct must match | `recovery.rs:310-364` | Positional checks only |
| Block-aware context in the response diff | `diff_string.rs:5,473` | Plain context |
| "parse regressed" warning after a write | `session.rs:393,548-552` | No warning |

Grammar coverage:
- **Linked natively:** 18 languages (C, C++, CSS, Go, Python, Rust, Bash, Java, JS, JSON, TS, TSX, YAML, Markdown, TOML, Diff, Regex, HTML).
- **Wasm only:** 37 others (C#, Kotlin, Nix, PHP, Ruby, Swift, Lua, Zig, …) (`pi-ast/src/language/mod.rs:163-225`).
- Wasm grammars are **downloaded at runtime by the TS host** from `github.com/stencil-hq/wasm-grammars` releases (`packages/coding-agent/src/utils/grammars.ts:20`) into a directory set with `set_grammar_dir` (`pi-ast/src/language/grammar.rs:131-139`; wired at `crates/pi-natives/src/grammars.rs:42-45`).
- If no directory is set, the 37 wasm languages report `NotInstalled` and get treated as "no tree" (`grammar.rs:19-22,165-178`). **So in a Rust host with no downloader, wasmtime is linked but never used.**

## 2. pi-edit module map (lines: total / non-test code before `#[cfg(test)]`)
pi-edit has 23,927 lines in `src` plus 6,550 in `tests/`.

**Hashline core** (`modes/hashline/`, 7,426 total, about 6.8k code):

| Module | Lines | Role |
|---|---|---|
| apply | 1159 | apply edits + syntax-guided boundary repairs |
| messages | 1079/914 | model-facing text, byte-identical to TS (`lib.rs:19-20`) |
| patcher | 1142/737 | staging, seen-line guard, rescue, mismatch |
| parser | 854 | |
| tokenizer | 616 | |
| input | 460 | section split, envelope noise |
| recovery | 409 | |
| block | 327 | tree-sitter block ops |
| clipboard | 258 | registers |
| preview | 263/251 | streaming diff preview |
| prefixes | 167 | |
| mod | 165 | |
| types | 160 | |
| format | 132 | |
| mismatch | 124 | |
| syntax | 111 | pi-ast cache wrapper |

**Shared infrastructure** (about 5.2k code; hashline needs all of it):

| Module | Lines | Role |
|---|---|---|
| diff_string | 1547/1228 | numbered diffs; uses pi-ast and pi-diff |
| path_policy | 1071/674 | cwd/URL resolution, plan-mode refusal, auto-generated guard, suffix recovery via pi-walker |
| stream_json | 772/624 | partial-JSON repair for streaming previews |
| store | 768/472 | |
| notebook | 618/482 | `.ipynb` edited as cell text |
| session | 604/586 | streaming previews, apply, `EditWriter` |
| text | 480/438 | EOL, BOM, JS-trim; NFC via xutf |
| files | 310 | read cache, persist |
| engine | 226 | `ModeEngine` trait, `StagedFile` |
| error | 80 | |
| lib | 67 | |
| modes/mod | 54 | |

**Other modes, not needed for hashline** (about 9.4k code):

| Module | Lines |
|---|---|
| `sloppy/` | 5,340 |
| `patch` | 1,954 |
| `fuzzy` | 1,790 (used only by replace, patch, apply_patch, sloppy) |
| `apply_patch` | 541 |
| `replace` | 279 |

The modes are coupled only through `EditMode`, `engine_for` (`modes/mod.rs:12-29`) and the two `allow_fuzzy`/`fuzzy_threshold` fields in `SessionConfig` (`session.rs:28-29`). I found no hashline or shared module that references `fuzzy`, `sloppy` or `patch`. A hashline-only cut is mechanically clean.

**pi-ast** (6,145 lines total):

| Module | Lines | Used by pi-edit? |
|---|---|---|
| block | 1348/415 | yes |
| summary | 1413/915 | `parse_code_status` |
| parse_cache | 1195/590 | yes |
| language/mod | 929/913 | ast-grep-core `Language` impls for 55 languages + alias map |
| grammar | 356/198 | wasm loading |
| wasm_grammars | 361 | generated table |
| parsers | 59 | |
| ops | 477/412 | **no** (ast-grep search/rewrite for the `ast_grep`/`ast_edit` tools) |

**pi-diff:** 1,019 lines, zero dependencies. Myers diff, interning, hunks.

## 3. Dependencies (direct deps; "crates" = unique transitive packages, root excluded)

### pi-edit's direct deps (`crates/pi-edit/Cargo.toml:13-24`)
| Dep | What | pi-edit use | Crates |
|---|---|---|---|
| pi-ast | omp tree-sitter wrapper | syntax.rs:8, block.rs:10, diff_string.rs:5, session.rs:550 | **119** (55 with `wasm` off) |
| pi-walker | omp parallel gitignore-aware walker | `path_policy.rs:181-191` only (unique-suffix path recovery) | **50** (via pi-vfs → tokio full + tokio-util full) |
| pi-diff | omp Myers diff | diff_string.rs:249-381, recovery.rs:69, patcher.rs:174-177, session.rs:465-468 | 0 |
| regex | regex engine | header/prefix/closer patterns throughout (mod.rs:88-94, prefixes.rs:7-18, apply.rs:26) | 4 |
| serde_json | JSON | stream_json.rs (tool-arg repair), notebook.rs | 9 |
| serde | derive | (via pi-ast types) | 7 |
| parking_lot | Mutex | store.rs:10 | 13 |
| async-trait | async trait macro | `EditWriter`, session.rs:9,72 | 4 (proc-macro) |
| thiserror | error derive | error.rs:10, notebook.rs:13 | 5 |
| xxhash-rust | xxh32/xxh64 | tag (store.rs:12,79-95), cache keys | 0 |
| xutf | SIMD UTF and Unicode normalization (same author as omp, MIT) | **only** NFC in `text::normalize_unicode` (text.rs:8,289), used by fuzzy/sloppy, **not hashline** | 0, but **requires nightly** (`#![feature(portable_simd)]`, xutf-1.5.0 `src/lib.rs:62`) |

### Deps of the pi-* crates
| Crate → dep | What / use | Crates |
|---|---|---|
| pi-ast → tree-sitter 0.25 `features=["wasm"]` (`pi-ast/Cargo.toml:21`) | parser runtime; `wasm` pulls `wasmtime-c-api-impl` 29 | **90** with wasm (wasmtime alone 83), **7** without |
| pi-ast → ast-grep-core 0.39 | `Language`/`Doc` traits that `SupportLang` is built on (`language/mod.rs:13-19`); pattern matching in ops.rs | 93 with wasm, **16** without |
| pi-ast → 17 tree-sitter-* grammars | native C parsers (cc build dependency) | 1 each (+cc build) |
| pi-ast → phf 0.14 | alias→language map (`language/mod.rs:753`) | 9 |
| pi-ast → globset, ignore | ops.rs only (ast-grep file walking) | 7, 13 |
| pi-ast → anyhow, serde, xxhash-rust | errors, option structs, parse cache key | small |
| pi-walker → pi-vfs, globset, ignore, rayon, parking_lot, libc | walker | 50 total |
| pi-vfs → tokio (full), tokio-util (full), async-trait, parking_lot, libc, xattr | async VFS | 33 |
| pi-diff | none | 0 |

### Measured totals (`cargo tree -p pi-edit -e normal`)
| Configuration | Crates (normal) | Normal + build |
|---|---|---|
| pi-edit as shipped | **150** | 156 |
| tree-sitter `wasm` off | **98** | 101 |
| pi-ast removed | 66 | — |
| pi-ast and pi-walker removed | **31** | — |

The last configuration is just regex, serde, serde_json, parking_lot, thiserror, async-trait, xutf, xxhash-rust and pi-diff.

### Measured build and binary cost
- Smoke binary: parse a `PUT 2*:` block op on a Rust file, resolve it via tree-sitter, apply it. Output was correct in both variants.
- Release build, thin LTO, stripped, 16 jobs, cold target dir:

| Variant | Release build time | Binary size |
|---|---|---|
| **no-wasm** | 29 s | **14.5 MB** |
| **wasm** | 77 s | **20.4 MB** |

- Debug build of `-p pi-edit`: no-wasm 31 s, wasm 52 s; target dirs 559 MB vs 810 MB.
- Most of the 14.5 MB is grammar parse tables from 17 native grammars [INFERENCE from crate composition; not separately measured].

## 4. Disabling tree-sitter `wasm`, and pi-edit without pi-ast
- **There is no feature flag.** `wasm` is hard-coded in `pi-ast/Cargo.toml:21`.
  - Removing it breaks exactly 3 sites in `pi-ast/src/language/grammar.rs`: the `WasmStore`/`wasmtime::Engine` import (line 33), `Language::is_wasm` (221) and `Parser::set_wasm_store` (231). Confirmed with `cargo check`.
  - A roughly 10-line patch fixes it: `WasmGrammar::load` returns `NotInstalled`, and `parse` always uses a plain `Parser`. With that, `cargo check -p pi-edit` and a release build pass. That patch is in `/tmp/axo-research-PiEditAnatomy/omp`.
  - Done properly it would be a `wasm` cargo feature with `#[cfg]` on about 4 items, about 20 lines.
  - **What you lose:** the 37 wasm-only languages get no syntax features (block ops, parse-guided repair, recovery context). That is exactly what a host without a grammar downloader gets anyway.
- **pi-edit without pi-ast:** not possible as shipped (hard dependency). A patch would touch 4 files:
  - `hashline/syntax.rs`: three functions return empty/false.
  - `hashline/block.rs`: `native_block_resolver` returns `None`.
  - `diff_string.rs:473`: no boundaries.
  - `session.rs:550`: `source_parses` returns false.
  - About 30–40 lines behind a `syntax` feature. Hashline still works, but **without `N*` block ops** and without the parse-guided repairs and checks listed in §1.

## 5. Integration facts
- **Not on crates.io.** `pi-edit`, `pi-ast`, `pi-diff`, `pi-walker` and `pi-vfs` all return "does not exist" from the crates.io API.
  - They live only in `github.com/can1357/oh-my-pi` (`Cargo.toml:33`), so using them means a git dependency.
  - The GitHub API reports the repo at about 700 MB (`size: 719142` KB). Cargo clones the whole repo for a git dependency.
  - The upstream crate version is the workspace version, 18.12.2, so it moves with omp releases.
- **License:** MIT (`Cargo.toml:30`, "Stencil Labs, Inc."). xutf is MIT too.
- **Edition and toolchain:**
  - Edition 2024. No `rust-version` field.
  - The workspace pins `nightly-2026-10-06` (`rust-toolchain.toml`).
  - pi-edit's own code uses no `#![feature]`.
  - **xutf forces nightly:** stable 1.98 fails with E0554. I built with `RUSTC_BOOTSTRAP=1`; a git dependent would need the same, or nightly.
- **`[patch.crates-io]` does not carry over to dependents** (Cargo applies patches only at the root workspace). A git dependent gets upstream `tree-sitter-go` 0.25.0, without omp's Go 1.26 `new(expr)` fix (`Cargo.toml:46-48`).
- **N-API:** pi-edit has none.
  - Its manifest has no napi crates. `crates/pi-natives/src/edit.rs` is a separate binding layer (`#[napi]` wrappers around `pi_edit::{Session, EditStore, …}`, lines 36-963).
  - pi-ast, pi-diff, pi-walker and pi-vfs are also napi-free.
  - The API is "internal but `pub`": it is shaped for pi-natives (a `Session` driven by streamed JSON argument deltas plus `EditWriter`/URL-resolver callbacks). It is not a stable library contract.
- **What axo must supply:**
  - Call `EditStore::record` from `read`/`grep`/`write` with the seen lines.
  - Format reads with `[path#TAG]` plus `N:` rows.
  - Implement `EditWriter`, and feed `Session` with `set_args_json` or deltas.
  - Optionally a URL resolver.
  - pi-natives' `edit.rs` (about 960 lines including napi structs) is the reference for this glue.

## 6. Options for axo
| | What | Upfront effort | Crates / build | Ongoing | What we lose |
|---|---|---|---|---|---|
| **(a) Git dependency as-is** | `pi-edit = { git = ".../oh-my-pi", rev = "…" }` | About 0.5–1 day of glue (read formatting, store calls, `EditWriter`, tool wrapper; about 300–600 lines) | 150 crates, wasmtime, +6 MB binary, about 2.5× release build time on these crates; **nightly or `RUSTC_BOOTSTRAP`**; about 700 MB clone | Rev bumps against an unstable internal API; omp ships fast | Nothing functional. Wasm grammars are inert unless we also write a downloader. We carry 4 unused modes (about 9.4k lines) |
| **(b) Thin fork / `[patch]` with feature flags** | Fork omp. Add a `pi-ast/wasm` feature (about 20 lines), an optional `pi-walker` in pi-edit (`recover_missing` via `ignore`, or off; about 20 lines), optionally a `syntax` feature (about 40 lines). Point axo's `[patch]` or git dependency at the fork | About 1 day plus (a)'s glue | 98 crates (no wasm) → about 50–60 with no walker (needs pi-ast to drop globset/ignore, which only `ops.rs` uses) → 31 with no pi-ast; 14.5 MB smoke binary | Small rebase per bump. Could be upstreamed to can1357, which would make it zero | 37 wasm languages lose syntax features. Still nightly unless xutf is also replaced |
| **(c) Vendor a hashline-only subset** (MIT notice kept) | Copy `modes/hashline` (7.4k), the shared modules (5.2k code; drop fuzzy and xutf; NFC is only needed by other modes), pi-diff (1k), and a pi-ast subset (block, summary, parse_cache, language, grammar without wasm and without `ops.rs`: about 3.1k code). Cut `engine_for`/`EditMode` to hashline only | About 2–3 days of surgery plus glue; mechanical, since cross-mode coupling is only `engine_for`/`EditMode`/fuzzy config | About 30 + grammar crates + ast-grep-core (16) ≈ 70; **stable toolchain** [INFERENCE: xutf is the only nightly blocker found]; about 15 MB | We own about 15–17k lines (plus about 5k of tests worth keeping); upstream heuristic and error-text fixes must be cherry-picked by hand | Nothing for hashline. Other modes and wasm languages dropped deliberately |
| **(d) Our own implementation** | Header and tag (any 4-hex hash), `N:` read format, tokenizer and parser for PUT/CUT/REM/MV/gaps/ranges/registers (about 600), apply (about 300), snapshot store, version retention and diff-remap recovery using `similar` (about 400), mismatch and seen-line messages (about 300), optional `N*` via the tree-sitter crate directly (about 300) | About 1.5–2.5k lines; about 1–2 weeks to match core behavior | About 30 crates without tree-sitter | Fully ours, small | omp's accumulated eval-driven robustness: echo/indent/landing boundary repair, parse-guided variant choice, seen-line guard with shift rescue, tag-path and suffix path recovery, no-op loop guard, streaming previews, notebooks, tolerant parsing of malformed payloads, and the training-aligned error strings. Their success-rate value is unmeasured [INFERENCE] |

## Recommendation for axo
1. **Don't take (a).** It costs wasmtime (+6 MB, about 2.5× build time) for grammars a Rust host never downloads, forces a nightly toolchain through xutf for a feature hashline doesn't use, and means a 700 MB git dependency on an internal, fast-moving API.
2. **Prefer (c), a vendored hashline-only subset, as axo's owned code.**
   - The cut is clean: the only cross-mode coupling is `engine_for`/`EditMode`/fuzzy config.
   - It keeps every piece of hashline behavior, including the syntax-aware repairs and recovery, which are the expensive, eval-tuned parts that (d) would throw away.
   - It drops about 9.4k lines of unused modes, wasmtime, pi-walker/tokio and xutf, so the stable toolchain should work.
   - It matches Toph's preference for owning code without re-deriving omp's heuristics.
   - Keep the MIT notice and record the upstream commit so fixes can be cherry-picked.
3. **If speed matters more than ownership right now, use (b) as a bridge:** a fork with the `wasm` and `pi-walker` feature flags, offered upstream. It costs about 1 day, and moving to (c) later is cheap.
4. **Choose (d) only if** we decide `N*` block ops and parse-guided repair are not worth tree-sitter at all. Even then, porting `parser`/`tokenizer`/`recovery` from omp beats rewriting them.
5. **Either way,** fix `core.md`/`context.md`: the tag is per file (`[path#TAG]`), not per line, and pi-edit already restores BOM and EOL.

Scratch artifacts:
- `/tmp/axo-research-PiEditAnatomy/omp`: no-wasm patched copy.
- `omp-wasm/`: pristine copy.
- `smoke-{nowasm,wasm}/`: smoke binaries.
- `count.sh`: crate counter.
