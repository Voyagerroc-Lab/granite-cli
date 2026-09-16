# Issue #129: Recommended Configurations for `setup --auto` (and the wizard)

## Context

`granite-cli setup --auto` currently over-enables. Root cause, traced directly
in code:

- Every `Capability` declares exactly **one** global `ModelRequirement` via
  `Capability::dependencies()`. `AgentModelCapability`
  (`src/capabilities/agent_model.rs:84-92`) only requires
  `ModelFunction::Chat + ToolCalling` — no size/quality bar, and no awareness
  of which launcher it's being bound into.
- `Revaluator::for_models` (`src/commands/setup.rs:515`) admits *any* catalog
  model whose metadata satisfies *any* selected capability's requirement.
- `SetupCommands::find_model_for_capability` (`src/commands/setup.rs:1533`)
  picks the **first** model in an unordered `HashSet<String>` that satisfies
  *any* of a capability's requirements.

So `claude` — which merely *supports* the `AgentModel` binding per
`LAUNCHER_REGISTRY` — gets happily wired to whatever Chat+ToolCalling model
happens to be configured, e.g. `granite4.2:3b` in Ollama, because nothing
distinguishes "`claude` needs a strong model" from "`bob` will take anything."

Issue #129 asks for a well-known, portable "Recommended Configuration" schema
that says, per `launcher`: which `capabilities` can be auto-enabled, and what
model constraints apply per capability — shipped built-in under `resources/`
(compiled into the binary) and user-authorable/shareable at runtime. The goal
is to make `setup --auto` conservative-by-default: only wire up combinations
someone has actually vetted, while still letting a human override via the
existing manual paths.

This plan is the result of an extended design discussion; every structural
choice below reflects an explicit decision already made with the user (not
just a default I picked), noted inline as "(decided: ...)".

## Scope boundary: `SetupCommands::run` only

Confirmed against a rebase pulling in PR #123 and #128 (`src/config/validation.rs`,
`src/commands/shared/remediation.rs`), which added dangling-reference
validation/remediation for already-configured instances.

- **No conflict, no change needed there.** `validation.rs`'s `RefKind`/
  `Validatable` walk (4 kinds: Launcher/Capability/Model/Provider) covers
  references *between configured instances* via the existing, unmodified
  `Dependency` (confirmed: `dependency_refs` in `validation.rs` reads
  `Dependency::Model{config_key,...}` exactly as it exists today).
  `RecommendedConfiguration` only ever references **registry type keys**
  (`capability_type`, catalog `model_id`) for *initial selection* — it's never
  itself a configured instance, nothing points at it, and it doesn't belong in
  `RefKind`. `configure_all`'s output is unaffected either way and stays
  cleanly validatable regardless of which logic picked the model.
- **Deliberately out of scope** (decided, after surfacing this explicitly):
  `CapabilityCommands::setup`, `ModelCommands::setup`, `LauncherCommands::setup`,
  `ProviderCommands::setup` (`src/commands/capability.rs:149`,
  `src/commands/model.rs:607`, etc.) — including `resolve_model_dependency`
  (`src/commands/capability.rs:313`) — and therefore `remediation.rs`'s
  "Reconfigure" fix, which calls straight into those. These are fundamentally
  different from `SetupCommands::run`: they gate only on what's *possible*
  (the capability's plain `ModelRequirement` against currently-configured
  instances, plus a manual "Configure a new model..." escape hatch) — always
  have, always should. `SetupCommands::run` (`run_auto` and the discovery
  wizard) is the "get me something good with minimal effort" entrypoint that
  already encodes recommendations, not just possibility (hardware fit,
  probing provider health endpoints) — `RecommendedConfiguration` extends
  that existing recommendation logic. It does not, and should not, reach the
  manual/expert commands. **No changes to `capability.rs`, `model.rs`,
  `launcher.rs`, `provider.rs`, `validation.rs`, or `remediation.rs` are part
  of this issue.**

## Final schema

```rust
// New module: src/recommended_config/mod.rs

/// One launcher's curated auto-setup recommendation. `launcher` is the
/// `LAUNCHER_REGISTRY` key (e.g. "claude"), or the reserved wildcard "*"
/// applied to any launcher without its own specific entry.
/// (decided: new first-class entity, not bolted onto `Dependency::Model`;
/// wildcard supported; whole-launcher-replace merge between built-in/user.)
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecommendedConfiguration {
    pub launcher: String,
    pub capabilities: Vec<RecommendedCapability>,
}

/// One capability this launcher can auto-enable, and how to fill each of its
/// model dependency slots.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecommendedCapability {
    /// CAPABILITY_REGISTRY key (e.g. "agent-model").
    pub capability: String,
    /// Keyed by the capability's own `Dependency::Model.config_key` (e.g.
    /// "model_id") -- lets a capability with more than one model dependency
    /// (present or future) get independent recommendations per slot.
    /// (decided: config_key is the right differentiator -- it's already how
    /// the capability's own config addresses which model fills which role.)
    pub models: HashMap<String, RecommendedModelSet>,
}

/// A slot's requirements plus its ordered list of admissible candidates.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecommendedModelSet {
    /// Minimum useful *achieved* context length for this slot, independent
    /// of which candidate is chosen (e.g. agent-model inherently needs more
    /// headroom than a lightweight capability). Checked against the
    /// *effective* context after hardware fit (ContextFit::Partial(n) -> n),
    /// not the model's native max -- this is what makes it meaningful on
    /// weak hardware where even the smallest model/quant only partially fits.
    /// (decided: this is a property of the capability/slot, not of any one
    /// candidate model.)
    pub min_context_length: Option<u64>,
    /// Ordered by preference; first available + admitted candidate wins.
    /// (decided: list order = preference, no separate "preferred" flag.)
    pub models: Vec<RecommendedModel>,
}

/// One candidate model and which of its variants are acceptable.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecommendedModel {
    pub model: StringMatch,
    /// Per-model precision allow-list (e.g. "Q4_K_M", "IQ4_XS"); empty = any
    /// variant. (decided: explicit inclusion list, NOT a quantization
    /// "floor" -- precision methods aren't linearly ordered, e.g. IQ2 may be
    /// fine where Q2_K_M isn't. Similarly `model` is matched by catalog
    /// model_id directly rather than family/version/size, since declared
    /// size can diverge from real size, e.g. granite-4.2-30b is ~29B.)
    pub variant_precisions: Vec<StringMatch>,
}

/// Exact match by default; explicit regex escape hatch so a plain version-
/// like string (e.g. "4.2") is never silently misinterpreted as a pattern.
/// YAML: a bare string -> Exact; `{regex: "..."}` -> Regex. Requires adding
/// the `regex` crate (decided: add it now).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum StringMatch {
    Exact(String),
    Regex { regex: String },
}

impl StringMatch {
    pub fn matches(&self, s: &str) -> bool {
        match self {
            StringMatch::Exact(v) => v == s,
            StringMatch::Regex { regex } => regex::Regex::new(regex)
                .map(|re| re.is_match(s))
                .unwrap_or(false), // invalid pattern -> warn at load time, never matches
        }
    }
}
```

Note `supported_functions` deliberately does **not** appear anywhere in this
schema (decided: it's implied by which models get listed — if a model lacks
`Thinking`, don't list it in a config that needs `Thinking`).

## Loading and merging

- **Built-in**: `resources/recommended_configs/*.yaml`, one file per launcher
  (`claude.yaml`, `bob.yaml`, `goose.yaml`, `hermes.yaml`, `openclaw.yaml`,
  `opencode.yaml`, `pi.yaml` — the current `src/launchers/` set), plus a
  reserved `default.yaml` holding `launcher: "*"` for capabilities that don't
  need per-launcher differentiation (e.g. `mcp`, `vision-mcp`,
  `sub-agent*`). (decided: one file per launcher; wildcard included in v1.)
- Extend `build.rs` (which already codegens `resources/models.yaml` at
  `cargo:rerun-if-changed`-tracked build time) to also walk
  `resources/recommended_configs/*.yaml` and emit
  `include_str!("../resources/recommended_configs/<f>.yaml")` entries into a
  generated `pub const RECOMMENDED_CONFIG_SOURCES: &[(&str, &str)]` (no need
  for models.yaml's full per-item struct codegen — this is plain data, so a
  `LazyLock<Vec<RecommendedConfiguration>>` in `src/recommended_config/mod.rs`
  just `serde_yaml::from_str`s each embedded string once at first use.
- **User-authored**: reuse the existing `Config::load_dir` machinery
  (`src/config/mod.rs:408`) exactly as models/providers/capabilities/launchers
  already do — add a 5th `recommended_configs_dir()`, wire it into
  `ensure_directories()` and `Config::new()`, and add
  `pub recommended_configs: HashMap<String, RecommendedConfiguration>` to
  `Config`. This requires implementing the (crate-private) `ConfigId` (returns
  `self.launcher.clone()`) and `ConfigPathTranslator` (no-op — no filesystem
  paths embedded) traits for `RecommendedConfiguration`, alongside the
  existing impls in `src/config/mod.rs`. Files live at
  `GRANITE_CLI_HOME/config/recommended_configs/<launcher>.yaml`.
- **Merge precedence** (decided): a user file for a launcher key **fully
  replaces** the built-in entry for that same key (whole-launcher replace,
  simplest unambiguous semantics) — this applies independently to `"*"` too.
  Then, per launcher `L`, the *effective* capability set is the union of
  `L`'s own entry and the `"*"` entry, with `L`'s entry for a given
  `capability` key winning over `"*"`'s entry for that same key when both
  define it. Implement as one helper:
  `fn effective_recommended_config(cfg: &Config, launcher_type: &str) -> Vec<RecommendedCapability>`.

## Algorithm change in `src/commands/setup.rs`

Replace `Revaluator::for_capabilities`/`for_models`/`for_providers`,
`find_model_for_capability`, and `admits_for_recommendation`'s role in
selection with one resolution pass, reused by **both** `run_auto` and the
interactive wizard (decided: gate both, not just `--auto`).

```rust
/// Resolve every model slot a capability declares, in order, against its
/// recommended config. `None` if any *required* slot can't be resolved.
fn resolve_capability_models(
    cap_type: &str,
    rec_cap: &RecommendedCapability,
    hardware: &HardwareProfile,
    healthy_providers: &[...],
) -> Option<HashMap<String, ResolvedModelSlot>> { ... }

/// Try each candidate in `set.models`, in order; first fully-satisfying one
/// wins. "Satisfying" = at least one variant passes `variant_precisions`,
/// its hardware fit is not `ContextFit::None`, the *effective* context
/// (native for Full fit, partial amount for Partial fit) clears
/// `set.min_context_length`, and some healthy configured provider can
/// actually run that model+variant.
fn resolve_model_set(
    set: &RecommendedModelSet,
    hardware: &HardwareProfile,
    healthy_providers: &[...],
) -> Option<ResolvedModelSlot> { ... }
```

`resolve_model_set` reuses the existing hardware-fit ranking in `best_variant`
(`src/commands/setup.rs`) refactored to accept a pre-filtered variant slice
rather than always `model.variants`, so ranking logic isn't duplicated.
`StringMatch::Exact` resolves to a single catalog id lookup; `StringMatch::Regex`
iterates `MODEL_REGISTRY` entries in catalog order, testing each id.

- `run_auto`: for each *detected* launcher, look up its effective recommended
  config (strict gate — decided: **no entry means nothing auto-enabled for
  that launcher**, not a permissive fallback); for each `RecommendedCapability`,
  call `resolve_capability_models`; capabilities that resolve get auto-enabled
  with the resolved model/variant/provider bindings, feeding directly into the
  existing `configure_all` (unchanged plumbing from here on).
- `run_wizard`: its `select_capabilities`/`select_models`/`select_providers`/
  `select_variants` phases source their candidate sets from the same
  resolution pass instead of `Revaluator`, so a human sees only
  recommended-config-backed choices by default. Preserve the existing
  `DiscoveryResult.all_model_candidates` "choose a different model" escape
  hatch for advanced/manual override — this feature tightens the *default*
  path, it doesn't remove existing manual flexibility.

## Files touched

- New: `src/recommended_config/mod.rs` (schema, `StringMatch`, built-in
  static, merge helper).
- New: `resources/recommended_configs/{claude,bob,goose,hermes,openclaw,opencode,pi,default}.yaml`.
- `build.rs`: extend to embed `resources/recommended_configs/*.yaml`.
- `Cargo.toml`: add `regex` dependency.
- `src/config/mod.rs`: new `recommended_configs` field/dir, `ConfigId`/
  `ConfigPathTranslator` impls.
- `src/commands/setup.rs`: replace `Revaluator`/`find_model_for_capability`/
  `admits_for_recommendation`'s selection role with the new resolution pass;
  update `run_auto` and the wizard's selection phases; refactor `best_variant`
  to accept a variant slice.
- Existing tests referencing `Revaluator`, `find_model_for_capability`, or
  `admits_for_recommendation` (e.g.
  `configure_all_enables_only_capabilities_a_launcher_supports`,
  `src/commands/setup.rs:2480`) get rewritten against the new resolution
  function, using a test-injectable recommended-config set (mirroring the
  existing `TestConfigHome` pattern) instead of the real built-ins.

## Testing / verification

- Unit tests in `src/recommended_config/mod.rs`: `StringMatch::matches`
  (exact, regex, and the footgun case — a plain version string like `"4.2"`
  must NOT be treated as a pattern); merge precedence (user whole-launcher
  replace; wildcard ∪ specific with specific winning per-capability).
- Unit tests for `resolve_model_set`: ordered fallback (first candidate
  unavailable → falls through), `variant_precisions` filtering,
  `min_context_length` gated against *achieved* (not native) context.
- Regression test in `src/commands/setup.rs` directly encoding the original
  bug report: with a test-injected recommended config for `claude` listing
  only an 8B-class model, assert a test fixture equivalent to
  `granite4.2:3b` does **not** get wired to `claude`'s `agent-model`, and a
  qualifying model does.
- `cargo build` (verifies the `build.rs` change compiles and embeds the new
  resources), `cargo test`, then a manual run of `granite-cli setup --auto`
  against a real/mock Ollama with only a small model pulled (confirm `claude`
  is skipped), then with a recommended model pulled (confirm it's wired up).
