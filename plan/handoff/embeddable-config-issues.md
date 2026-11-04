# Issue: fix axo's embedded configuration ownership

Status: open. Written 2026-11-04.

Refines [embeddable-config.md](embeddable-config.md). This is input to the plan workflow, not an approved API design. The original remains background; this brief supersedes its requirement to migrate Axome. Deepen the ownership and behavioral contract, not the feature scope.

## Outcome

Make axo genuinely embeddable: a host can own configuration names, locations and acquisition without losing axo's merge and trust semantics or triggering implicit axo-specific configuration I/O. Fix axo and its in-repository examples/tests only. Axome integration and migration are separate work, not implementation tasks or acceptance gates here.

Preserving the existing file-backed behavior is the conservative baseline. Its packaging, default opt-in behavior and API compatibility policy are planning decisions below, not prerequisites already settled by this brief.

Success is not a renamed directory, a collection of new traits, or a clean grep. It is a demonstrated separation: the same semantic inputs produce the same permitted behavior through either adapter, and embedding does not cause implicit reads or writes to axo-owned configuration locations.

## Open discussion for the plan workflow

The previous brief promoted several assumptions into requirements. Discuss these before selecting an implementation; record the decision, tradeoff and resulting acceptance criteria. The alternatives are prompts, not an exhaustive menu. Do not silently treat the first option or the original handoff's trait sketch as approved.

### Product assumptions

- **Who needs legacy file discovery?** I assumed axo must continue to supply the current standalone-style layout. Decide whether that remains a first-class convenience offered by this library or belongs in a separate host adapter. Do not assume a standalone/ACP product must be built here.
- **What must remain compatible?** I assumed exact preservation of diagnostics, validation order, project-file toggles and snapshot behavior, while also assuming a breaking API cutover without shims. Characterize current behavior, then distinguish intended guarantees from accidents. Decide the compatibility policy for axo's public API and file adapter; external consumer migration remains separate regardless.
- **What is an embedding default?** I assumed filesystem discovery must require explicit adapter selection. The requirement is an embedding route with no implicit legacy configuration I/O; discuss whether the ordinary builder is that route or a clearly separate convenience constructor retains file-backed defaults. Also decide which missing resources default, disable a feature or produce an error.
- **How much policy does a host own?** I assumed axo always computes project access and resolves command/environment-valued selectors. Axo's existing trust rules must remain reusable and must not be bypassed by moving acquisition out of files. Discuss whether hosts can also supply explicit access or already-resolved values, which guarantees apply to each route, and who is responsible for execution. A trusted host is not the same thing as trusted project data.

### Solution assumptions

- **What is the smallest useful seam?** I assumed host-provided sources with raw layers. Compare supplied layers plus a core evaluator, callbacks, and source traits against actual call sites. Do not require a generic source framework, a trait per resource, or a particular module/crate/feature layout. Preserve origin and authority information wherever axo evaluates project contributions.
- **Who controls sampling and refresh?** I assumed file and host adapters should share current lifecycle behavior. Discuss host-supplied snapshots versus axo-invoked sources, when user policy is sampled, and how session creation/opening and closed-session scheme resolution stay consistent. No hot-reload feature is requested. Tests should assert the chosen contract, not incidental call counts.
- **Which resources belong to this cut?** I assumed settings, prompts, credentials, skills, stores, docs and caches all need the same adapter treatment. Inventory all implicit dependencies, but reuse existing injection seams and distinguish configuration acquisition from explicit runtime storage. Decide what must change to make the embedding route independent; do not redesign a working storage API solely for uniformity.
- **What does “no project read” mean at the host boundary?** I assumed axo invokes a project source and can prevent that invocation under `Ignore`. That is appropriate for lazy acquisition, but a host may already hold project data. Require axo to do no project acquisition or value resolution under `Ignore`, and not apply project contributions; define what the host is responsible for before handing over values. Do not claim axo can prevent arbitrary prior host I/O.

Fixed boundaries: this work is in axo, project data cannot elevate its own authority, the embedding route must not fall back to product-specific configuration discovery, and the result needs offline executable evidence. The sections below describe baseline behavior to investigate and candidate proof obligations; adjust them explicitly to the planning decisions above rather than treating every proposed mechanism as mandatory.

## Start with the behavior, not the proposed interface

Before choosing public APIs, characterize the current implementation from primary evidence:

- `crates/axo/src/config.rs`: `Settings::load`, `read_settings_value`, `deep_merge`, `ProjectSettings::access_for`, defaults and directory discovery.
- `crates/axo/src/axo.rs`: `AxoBuilder::build`, `Axo::settings_for`, session creation/opening and closed-session scheme resolution. Trace their callees rather than stopping at builder injection points.
- Prompt and compaction override loading, skills/context discovery, credential/catalog loading, session storage, documentation materialization and dev-shell caches. Follow reads and writes through startup and later session operations.
- `crates/axo/tests/settings.rs` and the relevant session/context/compaction tests: use raw fixture inputs, expected values, diagnostics and observed side effects.
- An axo-local integration fixture that embeds axo through public APIs with host-owned inputs and resources. It must exercise a genuine non-legacy route, not a helper that quietly supplies an `AxoDir`. No Axome repository inspection, edits, builds or tests are required.

Use `rg` and symbol/call-site tracing to produce a compact boundary table in the change description. For each existing input/resource, record its reader, lifecycle, authority, default/fallback, observable I/O, and proposed owner. Cite source symbols and executable tests. Include indirect fallback reads, not just literal path matches.

No separate architecture study or generalized source framework is required. This evidence should directly determine the smallest implementation.

## Resolve the contract before moving code

### 1. Preserve layer semantics and provenance

Separate acquisition from validation, policy filtering, merging and value resolution. Establish their current order with characterization tests before extracting them.

Required cases include:

- Missing and empty inputs, JSONC/BOM handling, top-level null versus a non-object, unknown keys, and invalid typed values.
- Recursive object merge, replacement of arrays/scalars, explicit null versus absent fields, and ordered project-path rules.
- Independent layer validation: a malformed user value must not disappear because a project overrides it. Today an invalid project value can fail validation even when that key would subsequently be filtered; preserve that behavior unless an explicit behavioral change is agreed.
- Default context-file contributions: keep standalone defaults without retaining an axo-specific layout in core defaults.
- Filtering before command resolution, environment expansion, and error attribution to the originating layer.

A source must not have to invent a filesystem path to report an error. Preserve origin information for host diagnostics while retaining existing file-adapter diagnostics. Do not silently normalize distinctions such as missing, empty and explicitly disabled prompt content.

### 2. Keep authority in the core, not in an adapter's goodwill

Document the trust boundary: hosts and their source implementations are trusted integration code; project-provided data is not. The host labels origins, and axo enforces policy on project contributions. This is not a sandbox for malicious host code.

Specify and test:

- How effective access is obtained from user policy and an explicit session override, before project acquisition. Project data cannot promote its own access or set user-only fields.
- Under `Ignore`, axo does not acquire, parse, resolve or apply project configuration. If the API uses lazy sources, test with one that fails if project acquisition is invoked. If the host supplies already-acquired layers, test non-application and absence of axo-side resolution; document that prior host I/O is outside this guarantee.
- Under `Read`, project `!command` model selectors and dev-shell activation cannot cause execution. Filtering must precede resolution; assert absence of execution, not only the final settings value.
- Under `Trust`, permitted project contributions still work. User-only keys remain user-only at every project access level.
- Characterize existing project-file toggles and decide their mapping or retirement under the agreed compatibility policy. Keep adapter-specific names out of the host-neutral core contract.

Do not adopt the original `SettingsSource::layers` sketch if it makes forbidden project reads unavoidable or lets adapters merge away authority information.

### 3. Make lifecycle behavior explicit

For each source, identify whether it is sampled at host construction, session creation/opening, prompt/compaction construction, or another existing boundary. State what an already-running session observes after its source changes.

In particular, characterize the current distinction between process-wide models/user policy and settings reloaded for sessions. Choose and document lifecycle semantics rather than accidentally inheriting or changing them while removing `settings_from_dir`.

Exercise two project roots through one host, opening a stored session, and closed-session scheme resolution. Settings and access must use the correct project root/cwd and must not leak from another session. Document any deliberate lifecycle differences between file-backed and host-supplied inputs.

### 4. Draw the ownership boundary precisely

Core accepts semantic values and explicit host-supplied resources. It does not discover configuration files or choose product-specific names. Paths remain legitimate for cwd, skills roots, stores and other explicitly filesystem-backed resources; this is not a filesystem-free agent rewrite.

Isolate standalone discovery, layout and file acquisition from the embedding route using the packaging chosen during planning. Audit the existing settings, models/credentials, prompts, skills roots, stores, docs materialization and caches already named in the original issue. Change only the seams needed to eliminate implicit layout dependencies. Reuse existing seams such as credential and session stores where they suffice; do not create a trait for every field.

An embedder that does not select this adapter must not silently fall back to it when an input is absent or fails. Specify whether each absent input has a semantic default, disables a feature or produces an error. Exercise deferred paths such as compaction and docs access, not only `build()`.

## Implementation and proof

1. **Capture the baseline first.** Add focused characterization fixtures against the old loader and relevant lifecycle paths. Record expected values, warnings/errors and side effects before replacing it. Tests must be capable of detecting a regression, not just blessing the new implementation.
2. **Implement the agreed ownership boundary.** Keep reusable merge/trust policy in one core implementation. Feed equivalent semantic layers through an in-memory host source and the standalone file adapter. Assert parity against the characterized expectations; agreement between two wrappers around the same broken evaluator is not sufficient.
3. **Use adversarial, table-driven cases.** Cross access levels with absent/valid/invalid layers, user-only overrides, executable selectors, project disabling and session overrides. Use recording sources for call order/counts, fail-on-read project sources for `Ignore`, and harmless execution sentinels for forbidden commands. Keep all tests offline and deterministic.
4. **Prove absence of implicit fallback.** Run an isolated embedding fixture with host-owned resources and no standalone adapter. Place conflicting or fail-on-access fixtures at legacy locations; unset or redirect relevant environment variables in a child process rather than racing global environment changes in parallel tests. Assert successful operation, unchanged sentinels and no legacy directory creation. Supplement with a Linux filesystem trace, such as `strace -f -e trace=%file`, for attempted reads that leave no files behind; use this as inspection evidence, not a mandatory portable test dependency.
5. **Prove the public embedding route inside axo.** Add an offline integration test using public APIs, host-owned settings layers and resources, and no legacy directory dependency. Exercise per-project policy and the relevant deferred paths. Migrate axo's own examples/tests and document how an external host can adopt the API; do not modify or validate Axome, or implement `/plan` or model-role features.
6. **Update living documentation in the same change.** Update `crates/axo/docs/`, relevant README material, `plan/design.md`, and the Track 2/5 source-provision language in `plan/plan.md`. Update repository instructions only where actually affected. Describe semantics host-neutrally; document legacy layout only as the standalone adapter's choices.

Use `axo_ai::testing::ScriptedApi`, `crates/axo/tests/common/mod.rs`, and `tempfile::tempdir()` where applicable. No API keys, network calls or live-model evidence belong in acceptance tests.

## Acceptance: evidence beyond author judgment

The change must supply a concise requirement-to-test mapping and reproducible commands showing:

- **Semantic preservation:** baseline fixtures still pass for the standalone adapter, including diagnostic origins, validation order, prompt fallback/empty overrides, and trust restrictions. Any intentional difference is identified explicitly, not hidden by regenerated expectations.
- **Adapter independence:** equivalent in-memory and file layers yield equivalent effective settings and permitted effects. Host origin labels work without fictitious filenames.
- **Negative guarantees:** ignored project inputs cause no axo-side acquisition or resolution and are not applied; lazy project sources, if used, are never invoked under `Ignore`; rejected executable values never run; omitted embedding inputs never trigger legacy discovery. Demonstrate that the corresponding tests fail when their guard is deliberately bypassed, then restore it.
- **Lifecycle correctness:** one host supports separate project layers across create/open/scheme paths without cross-project leakage, according to the documented sampling contract.
- **Public-API embedding:** an axo-local offline integration test uses host-owned configuration and resources without requiring a legacy axo directory. It proves axo's embedding capability, not an external product's completed integration.
- **Structural separation:** audit product-specific layout references with `rg` and inspect dependency direction. Core code and runtime guides do not prescribe legacy names; adapter code/docs and explicit regression fixtures may name them. Historical handoffs are not runtime policy and need not be rewritten to satisfy a blanket grep.

Run inside `nix develop`:

```bash
cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo test -p axo --test doc_identifiers
```

Report unavailable checks as unavailable, not as passing. No Axome build or test is part of acceptance.

## Scope limits

This is axo's configuration-ownership fix. Axome and other external consumers are separate work; do not edit them or make their migration a dependency of completion. Do not add subagents, extensions, MCP, hot reload, a new configuration format, a plugin framework, a benchmark project, or a standalone binary. Future agent/extension plans should refer to host-provided inputs, not gain implementations here.

Leave `axo://` renaming and prompt identity to a follow-on recorded in `plan/plan.md`; do not let branding work drive the source API. Preserve ecosystem context/skill conventions as distinct from axo-specific layout. Follow all repository invariants, including crate dependency direction, offline tests, mutation guards and subprocess containment.
