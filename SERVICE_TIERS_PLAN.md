# Service tiers (`fast` / `ultrafast` / `flex`) for axo

## Context

Some providers serve the same model faster (or cheaper and slower) when a request asks for a service tier. axo cannot request one today; the only trace is a hardcoded Codex price bump (`apply_service_tier_pricing` in `crates/axo-ai/src/api/codex.rs`). The goal: a tier can be chosen in any model selector (`codex/gpt-6.1-terra:low:fast`, `@coder:fast`, `:fast`), through a typed session method and a `/tier` command, it is sent to the provider, hosts can read which tiers every model supports, and each assistant message records the tier that actually served it.

Agreed behavior, all of it binding:

- Tier names: `standard` (default, no tier), `flex`, `fast`, `ultrafast`. There is no `priority` name in axo; adapters translate to the provider's wire word.
- A chosen tier the current model does not support is ignored at request time (sent as standard). The choice itself stays stored, so it applies again on a model that has it.
- A tier is never spread implicitly. By default it resets to `standard` when the model changes and is not inherited by child sessions. Two user-only settings widen that: `serviceTier.onModelChange: "keep"` and `subagents.serviceTier: "inherit"`.
- Selecting the model the session is already on, without a tier suffix, leaves the tier alone. `:standard` turns it off explicitly.
- Reopening a session restores its recorded tier.
- Compaction summary requests always run at standard.
- 429s on fast keep using the existing retry in `api/http.rs`; axo never downgrades fast to standard on its own. No new error marker: a failed message already carries `failure.status` and will carry the tier it was sent with.
- Supported tiers and price multipliers come from data (models.dev, the Codex account model list, `models.jsonc`), never from model-name checks.

Order the steps below as written; each leaves the workspace compiling. Steps 1–5 are `axo-ai`, 6 is `axo-agent`, 7–13 are `axo`.

## Approach

### 1. `ServiceTier` type (`crates/axo-ai/src/types.rs`, next to `ThinkingLevel`)

Add, mirroring `ThinkingLevel`'s derives exactly (`Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, specta::Type`, `#[serde(rename_all = "lowercase")]`):

```rust
pub enum ServiceTier { #[default] Standard, Flex, Fast, Ultrafast }

impl ServiceTier {
    pub const ALL: [ServiceTier; 4] = [Standard, Flex, Fast, Ultrafast];
    pub fn as_str(self) -> &'static str;          // "standard" | "flex" | "fast" | "ultrafast"
    pub fn is_standard(&self) -> bool;
}
impl std::str::FromStr for ServiceTier { type Err = String; } // Err: format!("unknown service tier: {text}")
```

Re-export it from `axo-ai`'s `lib.rs` wherever `ThinkingLevel` is re-exported, and from `axo` wherever `axo` re-exports `ThinkingLevel`.

### 2. Model data: which tiers a model has, and their price multiplier

`crates/axo-ai/src/model.rs`, `Model`: add after `cost`

```rust
/// Service tiers this model can be requested with → price multiplier over `cost`.
/// Never contains `Standard`.
#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
pub service_tiers: BTreeMap<ServiceTier, f64>,
```

and on `impl Model`:

```rust
/// `tier` when this model can be sent it, else `Standard`. Anthropic Messages has one
/// speed switch, so only `Fast` is sendable there.
pub fn effective_tier(&self, tier: ServiceTier) -> ServiceTier
```

Returns `tier` iff `service_tiers` contains it and (`api != ApiKind::AnthropicMessages` or `tier == Fast`); otherwise `Standard`. This is the single support check: adapters, the session and hosts all use it.

`Model` has no `Default`; add `service_tiers: BTreeMap::new()` to every literal: `crates/axo-ai/src/testing.rs:22`, `src/api/completions.rs:1467`, `src/models/mod.rs:960`, `src/models/rules.rs:244,413`, `tests/provider_safety.rs:35`, `tests/providers.rs:49`, `tests/streaming.rs:14`, `tests/transform.rs:11` (rules.rs literals get real values in steps 4–5).

### 3. Request option, wire mapping, served tier and cost

`crates/axo-ai/src/api/mod.rs`:

- `SimpleStreamOptions`: add `pub service_tier: ServiceTier` (the *chosen* tier; adapters apply `Model::effective_tier`).
- Two crate-private helpers:
  - `fn openai_service_tier(tier: ServiceTier) -> Option<&'static str>`: `Flex → "flex"`, `Fast → "priority"`, `Ultrafast → "ultrafast"`, `Standard → None`.
  - `fn served_tier(echo: &str) -> ServiceTier`: `"priority" | "fast" → Fast`, `"flex" → Flex`, `"ultrafast" → Ultrafast`, anything else (`"default"`, `"auto"`, `"standard"`, `"scale"`) `→ Standard`.

`crates/axo-ai/src/types.rs`, `AssistantMessage`: add after `thinking_level`

```rust
/// Tier that served this response: the provider's report, else the tier the request asked for.
#[serde(default, skip_serializing_if = "ServiceTier::is_standard")]
pub service_tier: ServiceTier,
```

Add `service_tier: ServiceTier::Standard` to every `AssistantMessage { .. }` literal (find them with `grep -rn "provider_thinking_level:" crates`; the compiler lists the rest).

`crates/axo-ai/src/cost.rs`: change the signature to `calculate_cost(model: &Model, usage: &mut Usage, tier: ServiceTier) -> UsageCost`. After computing the four components and before `total`, multiply each by `model.service_tiers.get(&tier).copied().unwrap_or(1.0)` (raw map lookup: this is the *served* tier).

Every adapter does the same three things: (a) compute `let tier = model.effective_tier(opts.simple.service_tier)` and set `reply.message.service_tier = tier` before sending, so a failed request still records what it asked for; (b) put the tier on the wire; (c) when the response reports a tier, overwrite `message.service_tier` with `served_tier(echo)` before pricing, and pass `message.service_tier` to `calculate_cost`.

- **Codex** (`api/codex.rs`): in `build_request_body`, after the `temperature` block: `if let Some(name) = openai_service_tier(model.effective_tier(opts.simple.service_tier)) { body["service_tier"] = json!(name); }`. Set `reply.message.service_tier` where the run creates `ResponsesStream::new(model)` (~line 235). **Delete** `apply_service_tier_pricing` and its call in `map_codex_event` (lines 297–303, 378–392).
- **Responses stream** (`api/responses.rs`, `finalize`, ~line 698–717; only Codex uses it): before the `calculate_cost` call add `if let Some(echo) = response.get("service_tier").and_then(Value::as_str) { reply.message.service_tier = served_tier(echo); }`, then call `calculate_cost(self.model, &mut reply.message.usage, tier)` with the tier copied out first.
- **Chat Completions** (`api/completions.rs`): in `build_params` next to the `vllm_priority` insert, `params.insert("service_tier", name)` under the same `openai_service_tier(effective)` condition. In the chunk handler (~line 1242), before the `usage` handling: read top-level `chunk["service_tier"]` as a string and overwrite `self.reply.message.service_tier`. `parse_usage` gains a `tier: ServiceTier` parameter forwarded to `calculate_cost`; both callers pass `self.reply.message.service_tier`.
- **Anthropic** (`api/anthropic.rs`): `RequestOptions` gains `fast: bool`, set in `request_options` (in `base`, so every return path carries it) to `model.effective_tier(simple.service_tier) == ServiceTier::Fast`. `build_params`: after the `temperature` block, `if options.fast { params["speed"] = json!("fast"); }`. `beta_features`, inside the `None =>` branch: `if options.fast { add(FAST_MODE_BETA); }` with a new `const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";` beside the other beta constants (sent for every fast request; Opus 5 requires it and GA models accept it). In `run`, set `reply.message.service_tier` from `options.fast`. In `handle`: in both `message_start` (`message["usage"]["speed"]`) and `message_delta` (`event["usage"]["speed"]`), when the field is a string overwrite `output.service_tier = served_tier(speed)`; `update_cost` gains a `tier: ServiceTier` parameter (copy `output.service_tier` into a local before borrowing `output.usage`).

### 4. Tiers from the models.dev catalog (`crates/axo-ai/src/models/catalog.rs`, `rules.rs`)

`DevCost`: add `pub multipliers: Vec<Value>` (the struct is `#[serde(default)]`).

`rules.rs`: add

```rust
/// The tier a catalog `service_tier` name means on `api`, when axo knows its wire form.
fn tier_named(api: ApiKind, name: &str) -> Option<ServiceTier>
/// Tier multipliers from `cost.multipliers`, else derived from per-tier input prices.
fn catalog_tiers(cost: &DevCost, api: ApiKind, today: &str) -> BTreeMap<ServiceTier, f64>
```

- `tier_named`: for `ApiKind::AnthropicMessages` only `"fast" → Fast`. For the other two kinds `"priority" → Fast`, `"flex" → Flex`, `"ultrafast" → Ultrafast`. Everything else (`"standby"`, `"fast"` on Chat Completions, `"priority"` on Messages) is `None`: the wire value for those is unknown, so the model does not get the tier.
- `catalog_tiers`, pass 1 over `cost.multipliers` (first match per tier wins): `when` must be an object containing `"service_tier": {"equals": <string>}`; the only other key allowed is `"date"`, an object with exactly one of `"lt"` / `"gte"` holding a `YYYY-MM-DD` string, which applies iff `today < lt` / `today >= gte` (plain string comparison). Any other key in `when` skips the entry. `factor` is either a number or an object whose `text` is a number; any other shape skips the entry. Skip non-finite or `<= 0` factors.
- Pass 2 over `cost.text.input.overrides`, only for tiers still missing: entries whose `when` is exactly `{"service_tier": {"equals": <string>}}` (single key) with a numeric `price`, when the base input price is `> 0`: multiplier = `price / base input price`. `// ponytail: input-price ratio stands in for the whole table; store per-tier rates if cost display for these gateways matters`.
- `today`: computed once in `map_catalog` as `jiff::Timestamp::now().strftime("%Y-%m-%d").to_string()` (UTC; `jiff` is already an `axo-ai` dependency) and passed down. `catalog_model` gains a `today: &str` parameter and sets `service_tiers: catalog_tiers(&dev.cost, api, today)`.
- `CatalogModels.openai` becomes `HashMap<String, (Cost, bool, BTreeMap<ServiceTier, f64>)>`; the `openai` branch of `map_catalog` fills the third element with `catalog_tiers(&dev.cost, ApiKind::OpenAiCodexResponses, today)`. Update the two destructurings in `codex_model` (`Some((cost, ..))`, `Some(&(_, off, _))`).
- Fix the doc comment above `catalog_cost` (it says service-tier overrides are ignored): they are ignored *by `catalog_cost`* and read by `catalog_tiers`.

### 5. Tiers for Codex models and `models.jsonc`

`crates/axo-ai/src/models/codex.rs`:

- `Row`: add `#[serde(default)] service_tiers: Vec<RowTier>` with `#[derive(Deserialize)] struct RowTier { id: String }`, and `#[serde(default)] additional_speed_tiers: Vec<String>`.
- `CodexModel`: add `#[serde(default)] pub tiers: Vec<String>` (default keeps existing `codex-models.json` caches readable). In `discover`, fill it with the `service_tiers` ids, plus `"priority"` when `additional_speed_tiers` contains `"fast"` and it is not already listed.

`rules.rs`, `codex_model`: add a `tiers: &[String]` parameter (pass `&model.tiers` for discovered models, `&[]` for `CODEX_FLOOR` rows). `service_tiers` = for each id, `tier_named(ApiKind::OpenAiCodexResponses, id)` → multiplier from the matching `openai` catalog row's tier map, else `1.0`. A tier the account list does not name is not offered, including `flex`.

`crates/axo-ai/src/models/config.rs`: `ModelPatch` gains `#[serde(default)] pub service_tiers: Option<BTreeMap<ServiceTier, Option<f64>>>` (JSON key `serviceTiers`). In `apply`, after the `cost` block: a number inserts/overwrites, `null` removes (same per-key merge as `thinking_level_map`). Return `Err(Error::Config(..))` with message `"serviceTiers: \"standard\" has no multiplier"` for a `Standard` key and `"serviceTiers: multipliers must be positive numbers"` for a non-finite or `<= 0` value.

### 6. Agent loop (`crates/axo-agent`)

- `agent.rs`: `AgentConfig` gains `pub service_tier: ServiceTier` (`AgentConfig::new` sets `Standard`); `State` gains `pub(crate) service_tier: ServiceTier`, copied in `Agent::new`; add `/// Takes effect from the next provider request.` `pub fn set_service_tier(&self, t: ServiceTier)` beside `set_thinking`.
- `run.rs`: `fn model(&self)` (~line 939) returns `(Model, ThinkingLevel, ServiceTier)`; `stream_turn` puts it in `SimpleStreamOptions { thinking, service_tier, session_id, .. }`. Update the other callers of `self.model()` to ignore the third element. The loop does not stamp the message; the adapter already did.

### 7. Settings (`crates/axo/src/config.rs`)

```rust
// Settings: new field, JSON key `serviceTier`
pub service_tier: ServiceTierSettings,

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize, specta::Type)]
#[serde(rename_all = "camelCase", default)]
pub struct ServiceTierSettings { pub on_model_change: TierOnModelChange }

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum TierOnModelChange { #[default] Reset, Keep }

// SubagentSettings: new field, JSON key `subagents.serviceTier`
pub service_tier: SubagentTier,

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum SubagentTier { #[default] Standard, Inherit }
```

Add the fields to the `Default` impls of `Settings` and `SubagentSettings`. Add `"serviceTier"` to `USER_ONLY_KEYS` (~line 580); `subagents` is already there. Both are user-only because they raise spending.

### 8. Selector grammar (`crates/axo/src/config.rs`)

`ModelChoice` gains `pub tier: Option<ServiceTier>` (doc: "The service tier, if the selector names one; `Standard` when it says `:standard`"). `Cascade` gains `pub tier: Option<ServiceTier>`; `first_model` adds `cascade.tier = cascade.tier.or(choice.tier);` beside the thinking line.

A selector may end in up to two `:suffix` segments in either order: at most one thinking level and at most one tier word. Add a private helper:

```rust
#[derive(Default)]
struct Suffixes { thinking: Option<ThinkingLevel>, tier: Option<ServiceTier> }
impl Suffixes { fn push(&mut self, segment: &str) -> Result<(), String> }
```

`push` parses the segment as `ThinkingLevel`, else as `ServiceTier`; errors are `"more than one thinking level"`, `"more than one service tier"`, and `format!("unknown thinking level or service tier: {segment}")`.

- `plain`:
  1. Starts with `:` → split the rest on `:`, `push` each segment; a `push` error becomes `Error::InvalidModelSelector { selector, message }`. Returns `model: None`.
  2. Exact `models.find(selector)` → model with no suffixes (keeps ids like `ollama/qwen3:8b` intact).
  3. Otherwise peel from the right, at most twice: `rsplit_once(':')` on the remaining prefix, `push` the suffix (a `push` error stops peeling), then `models.find(prefix)`; the first hit returns that model with the collected suffixes.
  4. `Err(Error::ModelNotFound(selector))`.
- `resolve_model` role branch: `rest.split(':')`; the first piece is the role name, every further piece goes through `push` (error → `InvalidModelSelector` with the full selector). Then `choice.thinking = suffixes.thinking.or(choice.thinking); choice.tier = suffixes.tier.or(choice.tier);`.

If an existing test pins the old `unknown thinking level: …` wording for a bad selector suffix, update that assertion to the new message.

### 9. Journal and projection (`crates/axo/src/store`)

- `entry.rs`: add `Entry::ServiceTierChange { #[serde(flatten)] base: EntryBase, service_tier: ServiceTier }` beside `ThinkingLevelChange` (serializes as `"type": "service_tier_change"`, field `serviceTier`); add it to both `base()` and `base_mut()` match lists. `ModeRestore` gains `#[serde(default)] pub service_tier: ServiceTier`.
- `project.rs`: `Projection` gains `pub tier: Option<ServiceTier>`; `track` gains `Entry::ServiceTierChange { service_tier, .. } => { self.tier = Some(*service_tier); self.mode_restore = None; }`. `ModelChange` does **not** touch `tier`: a reset is always journaled as its own entry (step 10).
- `session/journal.rs` (~line 275): add `Entry::ServiceTierChange { .. }` to the list beside `ThinkingLevelChange` that does not bump the context revision.

### 10. Session selection (`crates/axo/src/session/mod.rs`)

`Selection` gains `tier: ServiceTier` — the **chosen** tier. `SessionSetup` gains `tier: ServiceTier`; `Session::start` puts it in the initial `Selection` (~line 2194) and in `AgentConfig { service_tier: tier, .. }` (~line 2249).

Add on `SessionHooks`:

```rust
/// The chosen tier after moving from `current` to `model`: a tier named by the selector
/// wins; otherwise it survives only on the same model or under `serviceTier.onModelChange: "keep"`.
fn next_tier(&self, current: &Selection, model: &Model, explicit: Option<ServiceTier>) -> ServiceTier
```

= `explicit.unwrap_or(..)` where the fallback is `current.tier` if (`current.model.provider == model.provider && current.model.id == model.id`) or `self.inherited.config.settings.service_tier.on_model_change == TierOnModelChange::Keep`, else `ServiceTier::Standard`.

Change these, keeping their locking and append-then-apply order:

- `model_entries(&self, current, model, thinking: Option<ThinkingLevel>, tier: Option<ServiceTier>) -> (Vec<Entry>, ThinkingLevel, ServiceTier)`: compute `next_tier`; push `Entry::ServiceTierChange` when it differs from `current.tier`.
- `apply_selection(&self, agent, model, level, tier)`: also `agent.set_service_tier(tier)` and store `tier` in the new `Selection`.
- `select_model(&self, agent, model, thinking, tier: Option<ServiceTier>)`.
- New `select_tier(&self, agent, tier: ServiceTier) -> Result<ServiceTier, Error>`, shaped like `select_thinking`: take `selection_order`; if `tier != current.tier` append `Entry::ServiceTierChange`, then `agent.set_service_tier(tier)` and `selection.send_modify`; return `current.model.effective_tier(tier)`.
- `select_choice`: `Some(model)` → `select_model(agent, model, choice.thinking, choice.tier)`. `None` → run `select_thinking` if `choice.thinking` is set and `select_tier` if `choice.tier` is set (remove the `expect`).
- `follow_registry`: pass `current.tier` through to `apply_selection` unchanged (a refreshed model that lost the tier just makes it ineffective).
- `record_selection(&self, model, thinking, tier: Option<ServiceTier>, agent)`: push `Entry::ServiceTierChange` when `tier.unwrap_or_default() != selection.tier`. Callers: `Session::start` (~line 2299, pass `projection.tier`) and `record_current_selection` (~line 1385, pass `view.projection.tier`).
- `set_agent` (~line 2834): `model_entries(&current, &model, choice.thinking, choice.tier)` and carry the tier into `apply_selection`.
- `set_model`: `select_model(&agent, model, None, None)`.
- `fork` (~line 3177 and ~3308): destructure and pass `tier` into the fork's `SessionSetup`.
- `session/maintenance.rs:195`: `let Selection { model, thinking, .. } = self.selection();` (summaries stay standard via `SimpleStreamOptions::default()`).

Public surface, in the `#[expose(block = SESSION_CORE, object = "session")] impl Session` block beside `thinking` / `set_thinking`, each with `#[ext]`:

```rust
/// The chosen service tier; it applies only on models that support it.
pub fn service_tier(&self) -> ServiceTier
/// The tier requests are sent with on the current model.
pub fn effective_service_tier(&self) -> ServiceTier        // selection.model.effective_tier(selection.tier)
/// Chooses the service tier from the next request; returns the tier the current model applies.
pub async fn set_service_tier(&self, tier: ServiceTier) -> Result<ServiceTier, Error>
```

`set_service_tier` uses the same `detached(async move { hooks.select_tier(&agent, tier).await })` form as `set_thinking`. `LiveSnapshot` gains `pub service_tier: ServiceTier` (chosen), filled in `attach` beside `thinking`.

### 11. Session start, children, modes

- **Root start** (`crates/axo/src/axo.rs`, ~line 471–496): `let mut tier_suffix = cascade.tier;`, and in the default-model fallback `tier_suffix = tier_suffix.or(choice.tier);`. After `model` is known:
  `let same = projection.model.as_deref() == Some(format!("{}/{}", model.provider, model.id).as_str());`
  `let tier = tier_suffix.unwrap_or(if same || settings.service_tier.on_model_change == TierOnModelChange::Keep { projection.tier.unwrap_or_default() } else { ServiceTier::Standard });`
  Pass `tier` in `SessionSetup`. No `SessionOptions` field is added: a host passes the tier in `SessionOptions::model` or calls `set_service_tier`.
- **Child revival** (`session/children.rs`, ~line 292): `tier: projection.tier.unwrap_or_default()`.
- **Spawn** (`session/spawn.rs`, `resolve`): the `select` closure also returns `let tier = cascade.tier.unwrap_or(match settings.subagents.service_tier { SubagentTier::Inherit => parent.tier, SubagentTier::Standard => ServiceTier::Standard });`. Thread it through `Resolved { tier, .. }` and `launch` into `SessionSetup`. `SpawnContext` gains `pub service_tier: ServiceTier` (after `thinking`), filled in `resolve`; add the field to the one other literal, `crates/axo-extensions/tests/bridge/sessions.rs:878`.
- **Modes** (`session/modes.rs`):
  - `mode_target` returns `(Model, ThinkingLevel, ServiceTier, Option<ModeRestore>)`; `let tier = self.next_tier(base, &model, choice.as_ref().and_then(|c| c.tier));` (read `choice.tier` before `choice` is consumed). The `restore` condition adds `|| base.tier != tier`; `ModeRestore { .., service_tier: base.tier }`.
  - `restore_base`: `Selection { model, thinking: restore.thinking_level, tier: restore.service_tier }`.
  - `switch_live(&self, current, model, level, tier)`: model changed → `apply_selection(&agent, model, level, tier)`; otherwise update thinking as today and, when `current.tier != tier`, `agent.set_service_tier(tier)` plus `selection.send_modify`.
  - `select_mode_locked`: model changed → `model_entries(&current, &model, Some(level), Some(tier)).0`; otherwise push `ThinkingLevelChange` when the level differs **and** `ServiceTierChange` when the tier differs (replace the `else if` chain with two independent checks).
  - `rebase_mode`: carry the 4-tuple; the error fallback is `(base.model.clone(), base.thinking, base.tier, None)`; add `&& live.tier == tier` to the `live` comparison.

### 12. `/tier` command and one-off requests

- `crates/axo/src/commands.rs`: add `Command::Tier { tier: ServiceTier }`; `struct TierArgs { #[schemars(schema_with = "tier_schema")] tier: ServiceTier }` with `#[derive(Deserialize, JsonSchema)] #[serde(deny_unknown_fields)]`; `fn tier_schema` copying `thinking_schema` over `ServiceTier::ALL`; `fn decode_tier`; and a `Builtin { name: "tier", description: "Set the service tier from the next request", policy: CommandPolicy::Alongside, parameters: LazyLock::new(parameters::<TierArgs>), decode: decode_tier }` after `thinking` (`BUILTINS` becomes `[Builtin; 6]`). Update the `ModelArgs` doc comment to mention the tier suffix.
- `session/mod.rs` `run_command`: `Command::Tier { tier } => self.set_service_tier(tier).await.map(drop),`.
- `crates/axo/src/models.rs`: `CompletionRequest` gains `/// Overrides the selector's tier suffix; standard when neither names one.` `#[serde(default)] pub service_tier: Option<ServiceTier>`; in `Axo::stream`, `service_tier: request.service_tier.or(choice.tier).unwrap_or_default()`.

### 13. Tests (three, each guarding billing-relevant behavior)

1. `crates/axo-ai/src/models/rules.rs` unit test `catalog_tiers_read_multipliers_and_price_lists`: call `catalog_tiers` with `today = "2026-11-22"` on hand-built `DevCost` JSON and assert: `{"when":{"service_tier":{"equals":"priority"}},"factor":{"text":2}}` on `OpenAiCompletions` → `{Fast: 2.0}`; the pair `date lt 2027-03-01 → 3` / `date gte 2027-03-01 → 6` for `ultrafast` → `3.0` (and `6.0` with `today = "2027-03-01"`); `"fast"` on `AnthropicMessages` → `Fast`, on `OpenAiCompletions` → absent; an entry with an extra `region` key → absent; base input `4`, input override `{"when":{"service_tier":{"equals":"priority"}},"price":10}` with no multiplier → `{Fast: 2.5}`.
2. `crates/axo-ai/tests/provider_safety.rs`, new test `service_tiers_reach_the_wire_and_the_bill`, using that file's `server(status, headers, body)` (returns the captured request head and JSON body) and `request_with` helpers; extend `request_with` (or add a sibling) so the test can pass a `Model` with non-zero `cost` and `service_tiers: {Fast: 2.0}` and `service_tier: Fast` in the options. Serve a 200 `text/event-stream` body per adapter, shaped like the SSE bodies the file's existing SSE tests use. Assert: the request body field (`service_tier: "priority"` for Codex and Completions; `speed: "fast"` for Anthropic, with `fast-mode-2026-02-01` in the `anthropic-beta` request header); a response echoing the tier (`response.service_tier: "priority"`, chunk `service_tier: "priority"`, `usage.speed: "fast"`) yields `message.service_tier == Fast` and exactly twice the `usage.cost.total` of the same response echoing standard; a Codex response echoing `"service_tier": "default"` yields `Standard` and undoubled cost; and with `service_tiers` empty no tier field is sent at all.
3. `crates/axo/tests/agents.rs`, new test `service_tier_follows_selectors_and_settings`, modeled on `model_selectors_expand_roles_and_thinking` (models are declared as JSON, so give two of them `"serviceTiers": {"fast": 2}` and leave one without). Assert via `host.api.requests()[i].options.service_tier` and the session getters:
   - `create_session(model: "test/a:low:fast")` → request has `Fast` and thinking `Low`; `session.effective_service_tier() == Fast`.
   - `Command::Model { "test/a" }` (same model) → still `Fast`. `Command::Model { "test/b" }` → `Standard`. `Command::Tier { Fast }` then `Command::Model { ":standard" }` → `Standard`.
   - On the model without tiers: `set_service_tier(Fast)` returns `Standard`, `service_tier() == Fast`, `effective_service_tier() == Standard`.
   - Reopen the session (same `open_session` pattern as `tests/session.rs` ~line 834–929) → recorded tier restored.
   - With settings `{"serviceTier": {"onModelChange": "keep"}}`: `test/a:fast` then `Command::Model { "test/b" }` → still `Fast`.
   - Spawn a child with no selector: `Standard` by default; `Fast` with `{"subagents": {"serviceTier": "inherit"}}`.

## Critical files & anchors

- `crates/axo/src/session/mod.rs` — `Selection` (~714), `record_selection` / `select_model` / `model_entries` / `apply_selection` / `select_thinking` / `select_choice` (~1199–1382): the tier mirrors the thinking level through every one of these.
- `crates/axo/src/session/modes.rs` — `mode_target`, `switch_live`, `select_mode_locked`, `rebase_mode` (~65–245): the only place tier, model and thinking are saved and restored as a unit.
- `crates/axo/src/config.rs` — `resolve_model` / `plain` (~225–291): the selector grammar.
- `crates/axo-ai/src/models/rules.rs` — `catalog_model`, `map_catalog`, `codex_model`: where tier data enters `Model`.
- `crates/axo-ai/src/api/anthropic.rs` — `request_options`, `build_params`, `beta_features`, `handle`: the one adapter with a different wire form.

## Verification

Run inside `nix develop` from `/repo/rust/axo`:

```bash
cargo check --workspace --all-targets
cargo test -p axo-ai catalog_tiers_read_multipliers_and_price_lists
cargo test -p axo-ai --test provider_safety service_tiers_reach_the_wire_and_the_bill
cargo test -p axo --test agents service_tier_follows_selectors_and_settings
cargo test -p axo --test ext_surface
UPDATE_BINDINGS=1 cargo test -p axo-extensions --test bindings
UPDATE_BINDINGS=1 cargo test -p axo --test ext_reference
cargo test -p axo --test doc_identifiers
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The two `UPDATE_BINDINGS` runs regenerate `packages/axo-extension-sdk/bindings.ts` and `crates/axo/docs/extension-api.md`; afterwards `bindings.ts` must contain `export type ServiceTier = "standard" | "flex" | "fast" | "ultrafast";`, `serviceTiers` on `Model`, and `setServiceTier` on the session interface.

Behavior documentation is part of done for this repo (`crates/axo/docs/` is compiled into the binary): `configuration.md` gains the `serviceTier` and `subagents.serviceTier` rows, the tier suffix and reset/keep rules in "Model selectors", and `serviceTiers` in the `ModelPatch` field list; `doc_identifiers` must still pass.

Live check (needs the user's Codex login; not part of `cargo test`): run `crates/axo-ai/examples/stream.rs` against an `openai-codex` model with `service_tier: ServiceTier::Fast` added to its `SimpleStreamOptions` literal (line ~80) and confirm the final message prints `service_tier: Fast`. This is the check for the two unverified items below.

## Assumptions & contingencies

- **Codex model-list field shape is unverified** (taken from `openai/codex` `codex-rs/protocol/src/openai_models.rs`: `service_tiers: [{id, name, description}]`, fast id `"priority"`, deprecated `additional_speed_tiers: ["fast"]`). If the live response differs, adapt only `Row`/`discover` in `models/codex.rs` so `CodexModel.tiers` still holds wire ids; nothing else changes.
- **Codex fast over HTTP/SSE is unverified.** If the live check shows `service_tier: "default"` echoed for every fast request, the feature still ships (the served tier is recorded truthfully and cost is not doubled); report it to the user rather than adding a WebSocket transport.
- **Anthropic beta header on GA models.** If Anthropic rejects `fast-mode-2026-02-01` on a model where fast is GA, add `Compat::fast_mode_beta: Option<bool>` and send the header only when it is `Some(true)`, set by a `models.jsonc` override.
- **Mythos 5.2** supports fast but the catalog has no tier entry for it; it needs `"modelOverrides": {"claude-mythos-5-2": {"serviceTiers": {"fast": 2}}}` in `models.jsonc`. Not hardcoded.
- The `openai` API-key provider is not served by axo (catalog marks its models Responses-only), so its tiers matter only as the price source for Codex models.
- A date-conditioned multiplier is evaluated when the catalog is mapped; a process running across the switch date uses the old value until the next catalog refresh.
