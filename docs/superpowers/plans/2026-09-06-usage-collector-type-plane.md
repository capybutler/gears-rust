# Usage Collector Type Plane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move usage-type ownership out of the usage-collector gear into `types-registry`, so a meter is a GTS type declaration whose `x-gts-traits` carry its aggregation fold, canonical unit and metadata surface.

**Architecture:** A new `TypeResolver` domain component resolves a `MeterTypeId` to a `ResolvedDeclaration` through a narrow `DeclarationSource` port over the `TypesRegistryClient` already on `ClientHub`, backed by a TTL cache that serves stale on registry error and fails closed when nothing is cached. Consumers flip to the resolver one path at a time — aggregate, then ingest, then the identifier newtype — so the catalog stays in place and the workspace stays green until the final task deletes it.

**Tech Stack:** Rust, `async_trait`, ToolKit (`OperationBuilder`, `ClientHub`, `GtsPluginSelector`), `toolkit-odata`, `types-registry-sdk`, `gts` 0.12, `jsonschema` 0.40, `rust_decimal`, `bigdecimal`, `cargo nextest`.

**Spec:** `docs/superpowers/specs/2026-09-06-usage-collector-gateway-registry-owned-typing-design.md`

---

## Orientation for the implementer

You are working in `gears/system/usage-collector/`, which holds four crates:

| Path | Package | Role |
| --- | --- | --- |
| `usage-collector-sdk/` | `cf-gears-usage-collector-sdk` | Public models, consumer trait, plugin SPI trait, errors |
| `usage-collector/` | `cf-gears-usage-collector` | The gear: domain service, REST surface, plugin host |
| `plugins/noop-usage-collector-plugin/` | `cf-gears-noop-usage-collector-plugin` | SPI implementation that persists nothing |
| `plugins/timescaledb-usage-collector-plugin/` | `cf-gears-timescaledb-usage-collector-plugin` | Unwired in Task 1. Do not edit. |

Conventions this codebase uses, which you must follow:

- **Tests live in a sibling file**, not an inline `mod tests`. A module `foo.rs` puts its tests in `foo_tests.rs` and ends with:
  ```rust
  #[cfg(test)]
  #[cfg_attr(coverage_nightly, coverage(off))]
  #[path = "foo_tests.rs"]
  mod foo_tests;
  ```
- **Run tests with** `cargo nextest run -p <package>`. The whole gear is
  `make test GEAR=usage-collector`.
- **Lint with** `cargo clippy --all-targets --all-features`. It is deny-warnings in CI.
- **Format with** `cargo +nightly fmt` (the repo pins a nightly rustfmt via `rust-toolchain.toml`).
- Newtypes validate in `new()` and route `Deserialize` through it, so a wire
  payload cannot bypass the invariant. Copy the shape of `ResourceRef` in
  `usage-collector-sdk/src/models.rs`.
- Commit messages are Conventional Commits with a `Signed-off-by` trailer.

**Do not** consult `gears/system/usage-collector/docs/DECOMPOSITION.md` or
`gears/system/usage-collector/docs/features/*.md`. They were not reworked with
the model and describe the catalog this plan deletes. The current documents are
`docs/DESIGN.md`, `docs/ADR/*.md`, `docs/PRD.md`,
`docs/usage-collector-v1.yaml` and `docs/schemas/*.json`.

## File structure

**Created:**

| File | Responsibility |
| --- | --- |
| `usage-collector/src/domain/type_resolver/mod.rs` | `TypeResolver` — cache, single-flight, stale-on-error, fail-closed |
| `usage-collector/src/domain/type_resolver/declaration.rs` | `ResolvedDeclaration` and its parse from `GtsTypeSchema` |
| `usage-collector/src/domain/type_resolver/metadata.rs` | `CompiledMetadataSchema` — compile once, validate per entry, expose declared keys |
| `usage-collector/src/domain/ports/declarations.rs` | `DeclarationSource` port |
| `usage-collector/src/infra/types_registry_source.rs` | `DeclarationSource` adapter over `ClientHub` |
| plus one `*_tests.rs` sibling per file above | |

**Modified:** `usage-collector-sdk/src/{models,api,plugin_api,lib,gts}.rs`,
`usage-collector/src/{config,module}.rs`,
`usage-collector/src/domain/{service,query,validation,authz,mod}.rs`,
`usage-collector/src/domain/ports/{mod,metrics}.rs`,
`usage-collector/src/api/rest/{dto,routes/mod,routes/usage_types,routes/usage_records,handlers/mod,handlers/usage_types,handlers/usage_records}.rs`,
`plugins/noop-usage-collector-plugin/src/plugin.rs`, root `Cargo.toml`,
`Makefile`, `apps/cf-gears-example-server/{Cargo.toml,src/registered_gears.rs}`,
`testing/e2e/suites/usage_collector/{config,e2e}.yaml`.

The resolver is a directory module rather than one file because its three
concerns — caching policy, trait parsing, metadata compilation — are
independently testable and would otherwise produce one file well over 600 lines.

---

## Task 1: Unwire the TimescaleDB plugin

The plugin implements the SPI this plan reshapes. Rather than port it now, take
it out of the build and leave every file on disk untouched, so nothing
references a crate that no longer compiles.

**Files:**
- Modify: `Cargo.toml:142`
- Modify: `Cargo.lock` (regenerated — include it in the commit)
- Modify: `apps/cf-gears-example-server/Cargo.toml:52,123`
- Modify: `apps/cf-gears-example-server/src/registered_gears.rs:101-102`
- Modify: `Makefile:27,669,718-722` and the `ci:` prerequisite list
- Modify: `.github/workflows/ci.yml`
- Modify: `.github/workflows/e2e.yml`
- Modify: `testing/e2e/suites/usage_collector/e2e.yaml`
- Modify: `testing/e2e/suites/usage_collector/config.yaml`

- [ ] **Step 1: Remove the workspace member**

In `Cargo.toml`, delete this line:

```toml
    "gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin",
```

- [ ] **Step 2: Remove the example-server feature and dependency**

In `apps/cf-gears-example-server/Cargo.toml`, delete both lines:

```toml
timescaledb-usage-collector = ["dep:timescaledb_usage_collector_plugin"]
```

```toml
timescaledb_usage_collector_plugin = { package = "cf-gears-timescaledb-usage-collector-plugin", path = "../../gears/system/usage-collector/plugins/timescaledb-usage-collector-plugin", optional = true }
```

- [ ] **Step 3: Remove the example-server link reference**

In `apps/cf-gears-example-server/src/registered_gears.rs`, delete:

```rust
#[cfg(feature = "timescaledb-usage-collector")]
use timescaledb_usage_collector_plugin as _;
```

- [ ] **Step 4: Remove the Makefile target and feature exclude**

On line 27, drop `timescaledb-usage-collector` from the exclude list so it reads:

```make
EXAMPLE_SERVER_FEATURE_EXCLUDES ?= default fips k8s otel oop-example
```

Delete `test-usage-collector-pg` from the `.PHONY` list on line 669, and delete
the whole target (the comment block plus recipe around lines 718-722):

```make
## Run TimescaleDB usage-collector plugin integration tests (Docker required;
## the suite spins up its own timescale/timescaledb container via testcontainers)
test-usage-collector-pg: install-tools
	cargo nextest run -p cf-gears-timescaledb-usage-collector-plugin --features postgres
```

- [ ] **Step 5: Disable the usage-collector E2E suite**

The suite asserts persistence round-trips. With the only persisting plugin
unwired it would run against the noop backend, which stores nothing, so every
round-trip assertion fails for a reason unrelated to the code under test.
Disable the suite for the duration of the rewrite rather than leaving it red.

`tools/scripts/run_e2e.py` reads each manifest with a plain `yaml.safe_load`
and recognizes no disable key, so there is no in-manifest way to switch a suite
off. The step that invokes it is the only real control.

In `.github/workflows/e2e.yml`, delete the step and its comment:

```yaml
      # Self-managed suite (launcher: pytest): starts its own server plus a
      # TimescaleDB container, so it needs a reachable Docker daemon.
      - name: Run usage-collector E2E tests (TimescaleDB container)
        run: make e2e-usage-collector
```

In `testing/e2e/suites/usage_collector/e2e.yaml`, remove the
`- timescaledb-usage-collector` feature line and record why the suite is no
longer wired, without claiming a mechanism the runner does not have:

```yaml
# NOT RUN IN CI during the type-plane rewrite: the step that invoked this
# suite has been removed from .github/workflows/e2e.yml. The suite asserts
# persistence round-trips, and the only persisting plugin is unwired from
# the build (Task 1 of this plan). `make e2e-usage-collector` still runs it
# locally and is expected to fail until the TimescaleDB plugin is ported.
suite: usage-collector
features:
  - usage-collector
  - static-tenants
  - static-authn
  - static-authz
```

Do **not** teach `run_e2e.py` a `disabled:` key. It is shared tooling behind
every suite, and a new manifest key is well outside this task.

In `testing/e2e/suites/usage_collector/config.yaml`, delete the whole
`timescaledb-usage-collector-plugin:` block — the key and every indented line
under it, to the end of the block.

`.github/workflows/ci.yml` also invokes the Makefile target deleted in Step 4.
Delete that step too, or CI fails with "No rule to make target":

```yaml
      - name: Test timescaledb usage-collector plugin (pg integration)
        run: make test-usage-collector-pg
```

The `ci:` Makefile target names `test-usage-collector-pg` as a prerequisite as
well. Remove it from that list.

- [ ] **Step 6: Verify the workspace builds without it**

Run: `cargo check --workspace --all-targets`
Expected: success, and no mention of `cf-gears-timescaledb-usage-collector-plugin`.

Then confirm the crate really is out of the graph:

Run: `cargo metadata --format-version 1 --no-deps | grep -c timescaledb-usage-collector`
Expected: `0`

- [ ] **Step 7: Verify the gear's own tests still pass**

Run: `cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk -p cf-gears-noop-usage-collector-plugin`
Expected: PASS. Nothing in this task touched gear code.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Makefile apps/cf-gears-example-server testing/e2e/suites/usage_collector
git commit -s -m "build(usage-collector): unwire the timescaledb plugin

The plugin implements the storage SPI that the type-plane rewrite
reshapes. Take it out of the build so nothing references a crate that
stops compiling, and leave every source file on disk untouched.

Also disables the usage-collector E2E suite: it asserts persistence
round-trips, and the noop backend it would fall back to stores nothing."
```

---

## Task 2: Add `AggregationFold` to the SDK

Additive. `AggregationOp` and `AggregationSpec` stay until Task 11, so
everything keeps compiling.

The declared fold set is `SUM`, `COUNT`, `MAX`, `MIN`, `LATEST`
(`docs/schemas/usage_record.v1.schema.json`, `x-gts-traits-schema`). Note this
differs from today's `AggregationOp`: `AVG` is not a fold this gear serves, and
`LATEST` is new.

**Files:**
- Modify: `usage-collector-sdk/src/models.rs`
- Modify: `usage-collector-sdk/src/lib.rs`
- Test: `usage-collector-sdk/src/models_tests.rs`

- [ ] **Step 1: Write the failing tests**

Append to `usage-collector-sdk/src/models_tests.rs`:

```rust
#[test]
fn aggregation_fold_serde_round_trips_screaming_case() {
    for (fold, wire) in [
        (AggregationFold::Sum, "\"SUM\""),
        (AggregationFold::Count, "\"COUNT\""),
        (AggregationFold::Max, "\"MAX\""),
        (AggregationFold::Min, "\"MIN\""),
        (AggregationFold::Latest, "\"LATEST\""),
    ] {
        assert_eq!(serde_json::to_string(&fold).unwrap(), wire);
        assert_eq!(
            serde_json::from_str::<AggregationFold>(wire).unwrap(),
            fold
        );
    }
}

#[test]
fn aggregation_fold_rejects_avg() {
    // AVG was an AggregationOp. It is not a declared fold: a declaration
    // naming it must fail resolution rather than silently pick another.
    assert!(serde_json::from_str::<AggregationFold>("\"AVG\"").is_err());
    assert!("AVG".parse::<AggregationFold>().is_err());
}

#[test]
fn aggregation_fold_from_str_matches_the_wire_shape() {
    assert_eq!("SUM".parse::<AggregationFold>().unwrap(), AggregationFold::Sum);
    assert_eq!(
        "LATEST".parse::<AggregationFold>().unwrap(),
        AggregationFold::Latest
    );
    // Case-sensitive on purpose: the enum in the trait schema is upper case,
    // and accepting "sum" would admit a declaration the registry rejects.
    assert!("sum".parse::<AggregationFold>().is_err());
}
```

Add `AggregationFold` to the `use usage_collector_sdk::...` (or `use super::*`)
list at the top of `models_tests.rs` if that file imports names explicitly.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p cf-gears-usage-collector-sdk -E 'test(aggregation_fold)'`
Expected: FAIL to compile — `cannot find type AggregationFold in this scope`.

- [ ] **Step 3: Implement `AggregationFold`**

Add to `usage-collector-sdk/src/models.rs`, next to `AggregationOp`:

```rust
// ---------------------------------------------------------------------------
// Declared aggregation fold
// ---------------------------------------------------------------------------

/// The single aggregation a meter declares, read from its GTS type
/// declaration's `x-gts-traits.aggregation_fold`.
///
/// This is never a request parameter. The aggregate path serves the declared
/// fold and no other, so no class of request is well-formed and semantically
/// wrong. The set is closed: adding a fold is an additive change, removing one
/// is breaking.
///
/// `SUM` is the only fold yielding a chargeable period quantity. A meter whose
/// consumption is naturally a level is pre-integrated at the emitter into an
/// accrued quantity and declared `SUM`; this gear integrates on no path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AggregationFold {
    /// Total of the selected quantities.
    #[serde(rename = "SUM")]
    Sum,
    /// Count of selected entries. Under this fold a quantity means nothing:
    /// one record is one event.
    #[serde(rename = "COUNT")]
    Count,
    /// Greatest selected quantity.
    #[serde(rename = "MAX")]
    Max,
    /// Least selected quantity.
    #[serde(rename = "MIN")]
    Min,
    /// The quantity of the entry with the greatest `window_end`, ties broken
    /// by the greatest `acceptance_sequence`.
    #[serde(rename = "LATEST")]
    Latest,
}

impl AggregationFold {
    /// The wire spelling, identical to the trait-schema enum member.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sum => "SUM",
            Self::Count => "COUNT",
            Self::Max => "MAX",
            Self::Min => "MIN",
            Self::Latest => "LATEST",
        }
    }
}

impl std::fmt::Display for AggregationFold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AggregationFold {
    type Err = UsageCollectorError;

    /// Mirrors the serde wire shape without paying a `serde_json::Value`
    /// allocation per call. Case-sensitive: the trait schema's enum is upper
    /// case, so accepting another casing would admit a declaration
    /// `types-registry` rejects.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "SUM" => Ok(Self::Sum),
            "COUNT" => Ok(Self::Count),
            "MAX" => Ok(Self::Max),
            "MIN" => Ok(Self::Min),
            "LATEST" => Ok(Self::Latest),
            other => Err(UsageCollectorError::invalid_aggregation_fold(other)),
        }
    }
}
```

- [ ] **Step 4: Add the error constructor**

In `usage-collector-sdk/src/error.rs`, next to the existing
`invalid_usage_kind` constructor, add:

```rust
    /// An `aggregation_fold` outside the declared set. Raised when a
    /// resolved declaration names a fold this major version does not serve.
    #[must_use]
    pub fn invalid_aggregation_fold(value: &str) -> Self {
        Self::invalid_argument_with_reason(
            "aggregation_fold",
            format!(
                "`{value}` is not a declared aggregation fold \
                 (expected one of SUM, COUNT, MAX, MIN, LATEST)"
            ),
            ValidationReason::Validation,
        )
    }
```

Read the neighbouring constructors first and match whichever helper they use to
build an `InvalidArgument` with a field violation — the exact helper name may
differ from `invalid_argument_with_reason`. Do not invent a new error shape.

- [ ] **Step 5: Export it**

In `usage-collector-sdk/src/lib.rs`, add `AggregationFold` to the
`pub use models::{...}` list, keeping the list alphabetically ordered.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo nextest run -p cf-gears-usage-collector-sdk -E 'test(aggregation_fold)'`
Expected: PASS, 3 tests.

- [ ] **Step 7: Verify nothing else broke**

Run: `cargo nextest run -p cf-gears-usage-collector-sdk`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add gears/system/usage-collector/usage-collector-sdk
git commit -s -m "feat(usage-collector-sdk): add the declared AggregationFold

One meter declares one fold in its GTS type declaration, so the closed
set is SUM, COUNT, MAX, MIN and LATEST. AVG is not among them and LATEST
is new, which is why this is a new type rather than a rename of
AggregationOp. Both coexist until the catalog is removed."
```

---

## Task 3: Add `MeterTypeId` to the SDK

Additive, alongside `UsageTypeGtsId`. Task 10 swaps the use sites.

The old newtype wraps `gts::GtsInstanceId`. A meter is a GTS **type**, so this
one wraps `gts::GtsTypeId` and enforces the pattern from
`docs/schemas/usage_record.v1.schema.json`:

```
^gts\.cf\.core\.uc\.usage_record\.v1~[^\x00-\x1F\x7F~]+~$
```

That is the base type plus exactly one derivation segment. The
control-character exclusion is load-bearing: ADR-0007's identifier derivation
concatenates this value under a `0x1F` separator, and a control character in it
would let two distinct dedup identities collapse to one pre-image.

**Files:**
- Modify: `usage-collector-sdk/src/models.rs`
- Modify: `usage-collector-sdk/src/lib.rs`
- Test: `usage-collector-sdk/src/models_tests.rs`

- [ ] **Step 1: Write the failing tests**

Append to `usage-collector-sdk/src/models_tests.rs`:

```rust
const VALID_METER: &str =
    "gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1~";

#[test]
fn meter_type_id_accepts_a_single_derivation_of_the_base() {
    let id = MeterTypeId::new(VALID_METER).expect("valid meter type id");
    assert_eq!(id.as_str(), VALID_METER);
}

#[test]
fn meter_type_id_rejects_the_bare_base_type() {
    // The base is abstract. A meter must add exactly one segment.
    assert!(MeterTypeId::new("gts.cf.core.uc.usage_record.v1~").is_err());
}

#[test]
fn meter_type_id_rejects_a_type_outside_the_base() {
    assert!(MeterTypeId::new("gts.cf.core.uc.usage_type.v1~foo.bar._.baz.v1~").is_err());
}

#[test]
fn meter_type_id_rejects_a_missing_terminator() {
    // No trailing `~` makes it an instance id, not a type id.
    assert!(
        MeterTypeId::new("gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1")
            .is_err()
    );
}

#[test]
fn meter_type_id_rejects_control_characters() {
    // ADR-0007 concatenates this value under a 0x1F separator, so a control
    // character would break the injectivity the identifier derivation needs.
    let with_us = "gts.cf.core.uc.usage_record.v1~exa\u{1F}mple._.m.v1~";
    assert!(MeterTypeId::new(with_us).is_err());
    let with_del = "gts.cf.core.uc.usage_record.v1~exa\u{7F}mple._.m.v1~";
    assert!(MeterTypeId::new(with_del).is_err());
}

#[test]
fn meter_type_id_rejects_an_over_long_identifier() {
    let long = format!(
        "gts.cf.core.uc.usage_record.v1~{}.v1~",
        "a".repeat(600)
    );
    assert!(MeterTypeId::new(long).is_err());
}

#[test]
fn meter_type_id_deserialize_routes_through_validation() {
    let bad = serde_json::json!("gts.cf.core.uc.usage_record.v1~");
    assert!(serde_json::from_value::<MeterTypeId>(bad).is_err());

    let good = serde_json::json!(VALID_METER);
    let parsed: MeterTypeId = serde_json::from_value(good).expect("valid");
    assert_eq!(parsed.as_str(), VALID_METER);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p cf-gears-usage-collector-sdk -E 'test(meter_type_id)'`
Expected: FAIL to compile — `cannot find type MeterTypeId in this scope`.

- [ ] **Step 3: Implement `MeterTypeId`**

Add to `usage-collector-sdk/src/models.rs`. Read `UsageTypeGtsId` (around line
448) first and mirror its `Serialize` / `Deserialize` / `AsRef` / `Display`
shape:

```rust
// ---------------------------------------------------------------------------
// MeterTypeId
// ---------------------------------------------------------------------------

/// The GTS base type every meter derives from.
pub const USAGE_RECORD_BASE_TYPE: &str = "gts.cf.core.uc.usage_record.v1~";

/// Ceiling on the wire length of a meter type identifier, from
/// `docs/schemas/usage_record.v1.schema.json`.
const MAX_METER_TYPE_ID_LEN: usize = 512;

/// Reference to the GTS type declaration a ledger entry is metered against.
///
/// A meter is a derived **type** of [`USAGE_RECORD_BASE_TYPE`] with exactly
/// one further segment — not an instance — so this wraps [`GtsTypeId`] rather
/// than `GtsInstanceId`. The declaration it names is owned by
/// `types-registry`; this gear resolves it and mints none.
///
/// The gear infers no metering meaning from the shape of the identifier. Fold,
/// canonical unit and metadata surface come from the resolved declaration
/// alone.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct MeterTypeId(GtsTypeId);

impl MeterTypeId {
    /// Creates a [`MeterTypeId`] after validating it against the base type's
    /// published pattern.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorError::InvalidArgument`] when the value is
    /// longer than 512 bytes, is not a `~`-terminated GTS type identifier,
    /// does not derive from [`USAGE_RECORD_BASE_TYPE`] with exactly one
    /// further segment, or carries an ASCII control character.
    pub fn new(value: impl Into<String>) -> Result<Self, UsageCollectorError> {
        let raw = value.into();

        if raw.len() > MAX_METER_TYPE_ID_LEN {
            return Err(UsageCollectorError::invalid_meter_type_id(
                &raw,
                "must be at most 512 bytes",
            ));
        }

        // Checked before the GTS parse so the diagnostic names the real
        // problem: the identifier derivation of ADR-0007 concatenates this
        // value under a 0x1F separator, and a control character there would
        // let two dedup identities share one pre-image.
        if raw.chars().any(|c| c.is_ascii_control() || c == '\u{7F}') {
            return Err(UsageCollectorError::invalid_meter_type_id(
                &raw,
                "must not contain ASCII control characters",
            ));
        }

        let Some(suffix) = raw.strip_prefix(USAGE_RECORD_BASE_TYPE) else {
            return Err(UsageCollectorError::invalid_meter_type_id(
                &raw,
                "must derive from `gts.cf.core.uc.usage_record.v1~`",
            ));
        };

        // Exactly one further segment: non-empty, `~`-terminated, and with no
        // interior `~` that would make it two.
        let Some(segment) = suffix.strip_suffix('~') else {
            return Err(UsageCollectorError::invalid_meter_type_id(
                &raw,
                "must end with `~`",
            ));
        };
        if segment.is_empty() || segment.contains('~') {
            return Err(UsageCollectorError::invalid_meter_type_id(
                &raw,
                "must add exactly one derivation segment to the base type",
            ));
        }

        let parsed = GtsTypeId::try_new(&raw).map_err(|e| {
            UsageCollectorError::invalid_meter_type_id(&raw, &e.to_string())
        })?;

        Ok(Self(parsed))
    }

    /// Borrows the wire string, terminator included.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }

    /// Borrows the underlying GTS type identifier.
    #[must_use]
    pub fn as_gts(&self) -> &GtsTypeId {
        &self.0
    }
}

impl AsRef<str> for MeterTypeId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for MeterTypeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MeterTypeId {
    type Err = UsageCollectorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl<'de> Deserialize<'de> for MeterTypeId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        MeterTypeId::new(raw).map_err(serde::de::Error::custom)
    }
}
```

Add `GtsTypeId` to the `use gts::{...}` import at the top of `models.rs`.

Check `GtsTypeId::try_new`'s real name and signature in the `gts` crate before
writing this — if the constructor differs, adapt the call rather than the
validation rules.

- [ ] **Step 4: Add the error constructor**

In `usage-collector-sdk/src/error.rs`, following the shape of the neighbouring
constructors:

```rust
    /// A `gts_type_id` that is not a well-formed meter type reference.
    #[must_use]
    pub fn invalid_meter_type_id(value: &str, why: &str) -> Self {
        Self::invalid_argument_with_reason(
            "gts_type_id",
            format!("`{value}` is not a valid meter type id: {why}"),
            ValidationReason::InvalidBaseGtsId,
        )
    }
```

`ValidationReason::InvalidBaseGtsId` already exists — reuse it rather than
adding a variant.

- [ ] **Step 5: Export it**

In `usage-collector-sdk/src/lib.rs`, add `MeterTypeId` and
`USAGE_RECORD_BASE_TYPE` to the `pub use models::{...}` list.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo nextest run -p cf-gears-usage-collector-sdk -E 'test(meter_type_id)'`
Expected: PASS, 7 tests.

- [ ] **Step 7: Commit**

```bash
git add gears/system/usage-collector/usage-collector-sdk
git commit -s -m "feat(usage-collector-sdk): add MeterTypeId

A meter is a derived GTS type of gts.cf.core.uc.usage_record.v1~ with
exactly one further segment, so the reference wraps GtsTypeId rather
than the GtsInstanceId that UsageTypeGtsId wraps.

Control characters are rejected because the ADR-0007 identifier
derivation concatenates this value under a 0x1F separator."
```

---

## Task 4: `DeclarationSource` port and `ResolvedDeclaration`

The resolver depends on a one-method port it owns, not on the 14-method
`TypesRegistryClient`. That keeps its tests to a trivial fake and matches the
`domain/ports/` shape the gear already uses for metrics.

**Files:**
- Create: `usage-collector/src/domain/ports/declarations.rs`
- Create: `usage-collector/src/domain/type_resolver/mod.rs`
- Create: `usage-collector/src/domain/type_resolver/declaration.rs`
- Create: `usage-collector/src/domain/type_resolver/declaration_tests.rs`
- Modify: `usage-collector/src/domain/ports/mod.rs`
- Modify: `usage-collector/src/domain/mod.rs`

- [ ] **Step 1: Write the failing tests**

Create `usage-collector/src/domain/type_resolver/declaration_tests.rs`:

```rust
use serde_json::json;
use std::sync::Arc;

use types_registry_sdk::GtsTypeSchema;
use usage_collector_sdk::{AggregationFold, MeterTypeId};

use super::ResolvedDeclaration;

const METER: &str = "gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1~";
const BASE: &str = "gts.cf.core.uc.usage_record.v1~";

/// Builds a base + derived chain shaped like docs/schemas/*.json, so these
/// tests exercise the same trait-merge path production does.
fn schema_with_traits(traits: serde_json::Value) -> GtsTypeSchema {
    let base = GtsTypeSchema::try_new(
        BASE.parse().expect("base type id"),
        json!({
            "type": "object",
            "x-gts-abstract": true,
            "properties": {
                "metadata": { "type": "object", "additionalProperties": { "type": "string" } }
            }
        }),
        None,
        None,
    )
    .expect("base schema");

    GtsTypeSchema::try_new(
        METER.parse().expect("meter type id"),
        json!({
            "allOf": [
                { "$ref": format!("gts://{BASE}") },
                { "x-gts-traits": traits }
            ]
        }),
        None,
        Some(Arc::new(base)),
    )
    .expect("derived schema")
}

#[test]
fn parses_every_declared_trait() {
    let schema = schema_with_traits(json!({
        "aggregation_fold": "SUM",
        "canonical_unit": "byte-hours",
        "retention": "P125D",
        "nominal_sampling_interval": "PT1H"
    }));

    let decl = ResolvedDeclaration::from_schema(
        MeterTypeId::new(METER).unwrap(),
        &schema,
    )
    .expect("declaration parses");

    assert_eq!(decl.aggregation_fold, AggregationFold::Sum);
    assert_eq!(decl.canonical_unit, "byte-hours");
    assert_eq!(decl.nominal_sampling_interval.as_deref(), Some("PT1H"));
}

#[test]
fn nominal_sampling_interval_is_optional() {
    let schema = schema_with_traits(json!({
        "aggregation_fold": "COUNT",
        "canonical_unit": "count",
        "retention": "P400D"
    }));

    let decl =
        ResolvedDeclaration::from_schema(MeterTypeId::new(METER).unwrap(), &schema).unwrap();

    assert_eq!(decl.aggregation_fold, AggregationFold::Count);
    assert!(decl.nominal_sampling_interval.is_none());
}

#[test]
fn rejects_a_declaration_binding_no_unit() {
    // DESIGN 3.2: "Rejects an entry whose type binds no unit."
    let schema = schema_with_traits(json!({
        "aggregation_fold": "SUM",
        "retention": "P125D"
    }));

    let err = ResolvedDeclaration::from_schema(MeterTypeId::new(METER).unwrap(), &schema)
        .expect_err("a declaration with no canonical_unit must not resolve");
    assert!(
        err.to_string().contains("canonical_unit"),
        "diagnostic must name the missing trait, got: {err}"
    );
}

#[test]
fn rejects_a_declaration_with_no_fold() {
    let schema = schema_with_traits(json!({
        "canonical_unit": "bytes",
        "retention": "P125D"
    }));

    let err = ResolvedDeclaration::from_schema(MeterTypeId::new(METER).unwrap(), &schema)
        .expect_err("a declaration with no aggregation_fold must not resolve");
    assert!(err.to_string().contains("aggregation_fold"));
}

#[test]
fn rejects_an_unknown_fold_rather_than_substituting_one() {
    // 2.2 constraint-plugin-contract-stability: a fold the gear does not
    // implement is an error, never a substitution.
    let schema = schema_with_traits(json!({
        "aggregation_fold": "AVG",
        "canonical_unit": "bytes",
        "retention": "P125D"
    }));

    assert!(
        ResolvedDeclaration::from_schema(MeterTypeId::new(METER).unwrap(), &schema).is_err()
    );
}

#[test]
fn rejects_a_schema_carrying_no_traits_at_all() {
    let base = GtsTypeSchema::try_new(
        BASE.parse().expect("base type id"),
        json!({ "type": "object" }),
        None,
        None,
    )
    .expect("base schema");

    assert!(
        ResolvedDeclaration::from_schema(MeterTypeId::new(METER).unwrap(), &base).is_err()
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p cf-gears-usage-collector -E 'test(declaration_tests)'`
Expected: FAIL to compile — the module does not exist.

- [ ] **Step 3: Create the port**

Create `usage-collector/src/domain/ports/declarations.rs`:

```rust
//! Port for reading GTS type declarations.
//!
//! The Type Resolver depends on this one method rather than on the whole
//! `TypesRegistryClient`, so the resolver's caching policy can be tested
//! against a trivial fake and the registry adapter stays in `infra`.

use async_trait::async_trait;
use types_registry_sdk::GtsTypeSchema;
use usage_collector_sdk::MeterTypeId;

use crate::domain::error::DomainError;

/// Reads a meter's type declaration from its system of record.
#[async_trait]
pub trait DeclarationSource: Send + Sync + 'static {
    /// Fetches the type schema for `id`.
    ///
    /// # Errors
    ///
    /// - [`DomainError::DeclarationNotFound`] when the registry gives a
    ///   definite not-found answer. This is a resolvable fact and the
    ///   resolver caches nothing for it.
    /// - [`DomainError::TypesRegistryUnavailable`] for any other failure.
    ///   The resolver may serve a stale cached declaration for this, because
    ///   it cannot tell an unavailable registry from a slow one.
    async fn fetch(&self, id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError>;
}
```

Add `pub mod declarations;` to `usage-collector/src/domain/ports/mod.rs`.

Check `DomainError`'s existing variants in `usage-collector/src/domain/error.rs`
before writing this. `TypesRegistryUnavailable` already exists (it is
constructed in `Service::resolve_plugin`). If `UsageTypeNotFound` does not
exist under that name, use whichever `NotFound` variant the enum already
carries — do not add a variant in this task.

- [ ] **Step 4: Implement `ResolvedDeclaration`**

Create `usage-collector/src/domain/type_resolver/declaration.rs`:

```rust
//! The declaration attributes the write and read paths read off a meter.

use std::sync::Arc;

use types_registry_sdk::GtsTypeSchema;
use usage_collector_sdk::{AggregationFold, MeterTypeId};

use crate::domain::error::DomainError;
use crate::domain::type_resolver::metadata::CompiledMetadataSchema;

/// A meter's declaration, resolved from `types-registry` and cached.
///
/// Fold, canonical unit and metadata surface are immutable for the life of a
/// GTS type, which is what makes resolving them at read time safe: the gear
/// never pins them onto an accepted entry.
///
/// `retention` is deliberately absent. The storage plugin reads it from
/// `types-registry` itself, because the plugin is what applies it
/// (DESIGN 3.3, plugin obligations).
#[derive(Debug, Clone)]
pub struct ResolvedDeclaration {
    /// The meter this declaration describes.
    pub gts_type_id: MeterTypeId,
    /// The single fold the aggregate path serves for this meter.
    pub aggregation_fold: AggregationFold,
    /// The unit quantities travel and persist in. No path converts or scales.
    pub canonical_unit: String,
    /// The closed metadata surface, compiled once per declaration.
    pub metadata_schema: Arc<CompiledMetadataSchema>,
    /// Informational only. The gear exposes it and MUST NOT act on it.
    pub nominal_sampling_interval: Option<String>,
}

impl ResolvedDeclaration {
    /// Parses a declaration out of a registered type schema.
    ///
    /// Reads `x-gts-traits` merged across the inheritance chain, and the
    /// `metadata` property likewise merged, so a trait or a metadata
    /// constraint declared on an ancestor is honoured.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] when a mandatory trait is missing, when the
    /// fold is outside the set this major version serves, or when the
    /// metadata subschema does not compile. Every case fails closed: the gear
    /// never substitutes a default for a declared attribute.
    pub fn from_schema(
        gts_type_id: MeterTypeId,
        schema: &GtsTypeSchema,
    ) -> Result<Self, DomainError> {
        let traits = schema.effective_traits();

        let fold_raw = traits
            .get("aggregation_fold")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                DomainError::declaration_incomplete(
                    &gts_type_id,
                    "declares no `aggregation_fold`",
                )
            })?;
        let aggregation_fold: AggregationFold = fold_raw
            .parse()
            .map_err(|_| {
                DomainError::declaration_incomplete(
                    &gts_type_id,
                    &format!(
                        "declares `aggregation_fold: {fold_raw}`, which this \
                         major version does not serve"
                    ),
                )
            })?;

        let canonical_unit = traits
            .get("canonical_unit")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                DomainError::declaration_incomplete(
                    &gts_type_id,
                    "declares no `canonical_unit`",
                )
            })?
            .to_owned();

        let nominal_sampling_interval = traits
            .get("nominal_sampling_interval")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);

        let metadata_schema =
            Arc::new(CompiledMetadataSchema::compile(&gts_type_id, schema)?);

        Ok(Self {
            gts_type_id,
            aggregation_fold,
            canonical_unit,
            metadata_schema,
            nominal_sampling_interval,
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "declaration_tests.rs"]
mod declaration_tests;
```

- [ ] **Step 5: Add the `DomainError` constructor**

In `usage-collector/src/domain/error.rs`, add a constructor that produces the
gear's existing `NotFound`-class variant with a diagnostic naming the meter.
Read the enum first and follow its established shape:

```rust
    /// A declaration that resolved but does not carry what a meter needs.
    ///
    /// Fails closed: the gear never substitutes a default for a declared
    /// attribute (DESIGN 3.1, fail-closed resolution).
    #[must_use]
    pub fn declaration_incomplete(id: &MeterTypeId, why: &str) -> Self {
        Self::invalid_argument(format!(
            "GTS type `{id}` {why}"
        ))
    }

    /// `types-registry` has no declaration under this identifier.
    ///
    /// Distinct from an unavailable registry: this is a definite answer, and
    /// the resolver acts on it rather than riding it out on a stale entry.
    #[must_use]
    pub fn declaration_not_found(id: &MeterTypeId) -> Self {
        Self::DeclarationNotFound {
            gts_type_id: id.as_str().to_owned(),
        }
    }
```

Add the variant it constructs to the `DomainError` enum:

```rust
    /// The referenced GTS type declaration does not resolve.
    #[error("GTS type `{gts_type_id}` is not declared")]
    DeclarationNotFound {
        /// The unresolvable meter reference.
        gts_type_id: String,
    },
```

Map it to `UsageCollectorError::NotFound` wherever `DomainError` is lifted
(`infra/sdk_error_mapping.rs`), matching how the existing not-found variant is
lifted. DESIGN §3.3 puts an unresolvable GTS type on `NotFound`/404.

- [ ] **Step 6: Create the module and its metadata half**

Create `usage-collector/src/domain/type_resolver/mod.rs`:

```rust
//! Type Resolver — resolves a meter's declaration from `types-registry`.
//!
//! Resolution sits on the ingestion hot path, and `types-registry` publishes
//! no latency obligation of its own, so a per-entry registry call would make
//! this gear's ingestion NFRs contingent on a second gear. A local cache of
//! resolved declarations keeps those obligations self-contained.

mod declaration;
mod metadata;

pub use declaration::ResolvedDeclaration;
pub use metadata::CompiledMetadataSchema;
```

Add `pub mod type_resolver;` to `usage-collector/src/domain/mod.rs`.

Create `usage-collector/src/domain/type_resolver/metadata.rs` with the
key-extraction half only. Task 5 adds compilation and validation to this same
file; this version exists so Task 4 ends green.

```rust
//! The closed metadata surface a meter declares.
//!
//! Task 5 adds schema compilation and per-entry validation here.

use std::collections::BTreeSet;

use serde_json::Value;
use types_registry_sdk::GtsTypeSchema;
use usage_collector_sdk::MeterTypeId;

use crate::domain::error::DomainError;

/// A meter's `metadata` subschema.
#[derive(Debug)]
pub struct CompiledMetadataSchema {
    declared_keys: BTreeSet<String>,
}

impl CompiledMetadataSchema {
    /// Reads the declared property names off the `metadata` property merged
    /// across the schema chain.
    ///
    /// # Errors
    ///
    /// Infallible today. Task 5 makes it fail on a subschema that does not
    /// compile, and the signature carries the `Result` from the start so that
    /// change is not a breaking one for callers.
    pub fn compile(
        _id: &MeterTypeId,
        schema: &GtsTypeSchema,
    ) -> Result<Self, DomainError> {
        let merged = schema.effective_properties();
        let declared_keys = merged
            .get("metadata")
            .and_then(|m| m.get("properties"))
            .and_then(Value::as_object)
            .map(|props| props.keys().cloned().collect())
            .unwrap_or_default();
        Ok(Self { declared_keys })
    }

    /// The declared property names. Declared equals groupable and
    /// equality-filterable on both read paths.
    #[must_use]
    pub fn declared_keys(&self) -> &BTreeSet<String> {
        &self.declared_keys
    }
}
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo nextest run -p cf-gears-usage-collector -E 'test(declaration_tests)'`
Expected: PASS, 6 tests.

- [ ] **Step 8: Verify the whole gear still builds**

Run: `cargo nextest run -p cf-gears-usage-collector`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add gears/system/usage-collector/usage-collector/src
git commit -s -m "feat(usage-collector): parse a meter declaration from its GTS schema

ResolvedDeclaration reads aggregation_fold, canonical_unit and the
optional nominal_sampling_interval off x-gts-traits merged across the
inheritance chain. Retention is deliberately not carried: the storage
plugin reads it from types-registry itself, because the plugin applies
it.

Every missing or unserved attribute fails closed. The gear substitutes
no default for a declared attribute."
```

---

## Task 5: Compile and enforce the closed metadata surface

The base type admits any string-valued key; a derived meter closes the set with
`additionalProperties: false`. The two intersect to declared-keys-only.
`gears/system/resource-group/resource-group/src/domain/validation.rs:170-195`
does the same job against a GTS schema — read it before writing this.

**Files:**
- Modify: `usage-collector/src/domain/type_resolver/metadata.rs`
- Create: `usage-collector/src/domain/type_resolver/metadata_tests.rs`
- Modify: `usage-collector/Cargo.toml`

- [ ] **Step 1: Add the dependency**

In `usage-collector/Cargo.toml`, under `[dependencies]`:

```toml
jsonschema = { workspace = true }
```

- [ ] **Step 2: Write the failing tests**

Create `usage-collector/src/domain/type_resolver/metadata_tests.rs`:

```rust
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

use types_registry_sdk::GtsTypeSchema;
use usage_collector_sdk::MeterTypeId;

use super::CompiledMetadataSchema;

const METER: &str = "gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1~";
const BASE: &str = "gts.cf.core.uc.usage_record.v1~";

fn meter_id() -> MeterTypeId {
    MeterTypeId::new(METER).expect("valid meter id")
}

/// Mirrors docs/schemas/example.stored_volume.v1.schema.json: the base admits
/// any string value, the derived type closes the key set.
fn stored_volume_schema() -> GtsTypeSchema {
    let base = GtsTypeSchema::try_new(
        BASE.parse().unwrap(),
        json!({
            "type": "object",
            "properties": {
                "metadata": {
                    "type": "object",
                    "additionalProperties": { "type": "string" }
                }
            }
        }),
        None,
        None,
    )
    .unwrap();

    GtsTypeSchema::try_new(
        METER.parse().unwrap(),
        json!({
            "allOf": [
                { "$ref": format!("gts://{BASE}") },
                {
                    "properties": {
                        "metadata": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "region": { "type": "string", "minLength": 1, "maxLength": 64 },
                                "storage_class": { "type": "string", "minLength": 1, "maxLength": 64 }
                            }
                        }
                    }
                }
            ]
        }),
        None,
        Some(Arc::new(base)),
    )
    .unwrap()
}

fn metadata(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[test]
fn declared_keys_are_exactly_the_derived_properties() {
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    let keys: Vec<&str> = compiled.declared_keys().iter().map(String::as_str).collect();
    assert_eq!(keys, vec!["region", "storage_class"]);
}

#[test]
fn accepts_metadata_using_only_declared_keys() {
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    compiled
        .validate(&metadata(&[("region", "eu-west-1"), ("storage_class", "cold")]))
        .expect("declared keys accepted");
}

#[test]
fn accepts_a_subset_of_declared_keys() {
    // The derived schema declares no `required`, so a subset is well-formed.
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    compiled
        .validate(&metadata(&[("region", "eu-west-1")]))
        .expect("subset accepted");
}

#[test]
fn accepts_empty_metadata() {
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    compiled.validate(&BTreeMap::new()).expect("empty accepted");
}

#[test]
fn rejects_an_undeclared_key_and_names_it() {
    // 3.1 closed metadata shape: no free-form remainder, no escape hatch.
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    let err = compiled
        .validate(&metadata(&[("region", "eu-west-1"), ("tier", "gold")]))
        .expect_err("an undeclared key must be rejected before persistence");
    assert!(
        err.to_string().contains("tier"),
        "diagnostic must name the offending key, got: {err}"
    );
}

#[test]
fn rejects_a_value_violating_a_declared_constraint() {
    // The subschema constrains minLength, so an empty value is not merely an
    // odd string: it is outside the declared surface.
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &stored_volume_schema()).unwrap();
    assert!(compiled.validate(&metadata(&[("region", "")])).is_err());
}

#[test]
fn a_meter_declaring_no_metadata_property_admits_no_keys() {
    // A meter that declares no metadata surface has an empty one. Admitting
    // arbitrary keys would reopen the closed shape the base only half-closes.
    let base = GtsTypeSchema::try_new(
        BASE.parse().unwrap(),
        json!({ "type": "object" }),
        None,
        None,
    )
    .unwrap();
    let compiled = CompiledMetadataSchema::compile(&meter_id(), &base).unwrap();

    assert!(compiled.declared_keys().is_empty());
    compiled.validate(&BTreeMap::new()).expect("empty accepted");
    assert!(compiled.validate(&metadata(&[("anything", "x")])).is_err());
}

#[test]
fn rejects_a_metadata_subschema_that_does_not_compile() {
    let base = GtsTypeSchema::try_new(
        BASE.parse().unwrap(),
        json!({
            "type": "object",
            "properties": {
                "metadata": { "type": "object", "properties": { "x": { "type": 42 } } }
            }
        }),
        None,
        None,
    )
    .unwrap();

    assert!(CompiledMetadataSchema::compile(&meter_id(), &base).is_err());
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo nextest run -p cf-gears-usage-collector -E 'test(metadata_tests)'`
Expected: FAIL to compile — `no method named validate`.

- [ ] **Step 4: Implement the compiled schema**

Replace `usage-collector/src/domain/type_resolver/metadata.rs` entirely:

```rust
//! The closed metadata surface a meter declares.
//!
//! The base type admits any key with a string value. A derived meter closes
//! the set by declaring its properties with `additionalProperties: false`, and
//! the two constraints intersect to declared-keys-only. Compiling the
//! subschema once per declaration keeps the per-entry cost to a validation
//! pass rather than a parse plus a compile.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use types_registry_sdk::GtsTypeSchema;
use usage_collector_sdk::MeterTypeId;

use crate::domain::error::DomainError;

/// A meter's `metadata` subschema, compiled for repeated validation.
pub struct CompiledMetadataSchema {
    validator: jsonschema::Validator,
    declared_keys: BTreeSet<String>,
}

impl std::fmt::Debug for CompiledMetadataSchema {
    /// `jsonschema::Validator` is not `Debug`, and the compiled program is not
    /// useful in a log line anyway. The declared surface is.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledMetadataSchema")
            .field("declared_keys", &self.declared_keys)
            .finish_non_exhaustive()
    }
}

impl CompiledMetadataSchema {
    /// Compiles the `metadata` property merged across the schema chain.
    ///
    /// **Closure is enforced in code, not delegated to the subschema.**
    /// `GtsTypeSchema::effective_properties` resolves a key by *override*,
    /// not by intersection — "this schema wins on key collisions; parent
    /// fills in inherited keys". The base type always declares an open
    /// `metadata` (`additionalProperties: {"type": "string"}`), so a meter
    /// that does not supply its own closing override inherits that open
    /// definition verbatim, and a lookup for the key is never absent. A
    /// default that only fires when the key is missing entirely is therefore
    /// dead code against every real declaration.
    ///
    /// So `validate` checks `metadata.keys() ⊆ declared_keys()` itself. The
    /// invariant then holds by construction, whether or not a schema author
    /// remembered `additionalProperties: false`, and the admissible key set
    /// is exactly the one the query surface gates filtering and grouping on
    /// (Task 12). The compiled validator still enforces per-value
    /// constraints such as `minLength`.
    ///
    /// A meter declaring no `metadata` properties therefore has an empty
    /// surface and admits no keys at all — fail-closed, and actionable:
    /// the author sees a rejection naming the key rather than silently
    /// getting an open extension surface.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] when the subschema is not a valid JSON Schema.
    pub fn compile(
        id: &MeterTypeId,
        schema: &GtsTypeSchema,
    ) -> Result<Self, DomainError> {
        let merged = schema.effective_properties();

        let subschema: Value = match merged.get("metadata") {
            Some(v) => v.clone(),
            None => serde_json::json!({
                "type": "object",
                "additionalProperties": false
            }),
        };

        let declared_keys = subschema
            .get("properties")
            .and_then(Value::as_object)
            .map(|props| props.keys().cloned().collect())
            .unwrap_or_default();

        let validator = jsonschema::validator_for(&subschema).map_err(|e| {
            DomainError::declaration_incomplete(
                id,
                &format!("declares a metadata schema that does not compile: {e}"),
            )
        })?;

        Ok(Self {
            validator,
            declared_keys,
        })
    }

    /// The declared property names.
    ///
    /// Declared equals queryable: every one of these is groupable and
    /// equality-filterable on both read paths, recomputed per request.
    #[must_use]
    pub fn declared_keys(&self) -> &BTreeSet<String> {
        &self.declared_keys
    }

    /// Validates an entry's metadata against the declared surface.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] naming every violation, so a caller correcting
    /// a payload sees all of them at once rather than one per round-trip.
    pub fn validate(
        &self,
        metadata: &BTreeMap<String, String>,
    ) -> Result<(), DomainError> {
        let instance = serde_json::to_value(metadata).map_err(|e| {
            DomainError::internal(format!("metadata is not serializable: {e}"))
        })?;

        let violations: Vec<String> = self
            .validator
            .iter_errors(&instance)
            .map(|e| e.to_string())
            .collect();

        if violations.is_empty() {
            return Ok(());
        }

        Err(DomainError::invalid_metadata(violations.join("; ")))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "metadata_tests.rs"]
mod metadata_tests;
```

Add `mod metadata;` and `pub use metadata::CompiledMetadataSchema;` to
`type_resolver/mod.rs` if Task 4 left them out.

- [ ] **Step 5: Add the `DomainError` constructors**

In `usage-collector/src/domain/error.rs`, add `invalid_metadata` if no
equivalent exists. The gear already has metadata validation reasons
(`ValidationReason::MetadataValidation`, `UnknownMetadataKey`) — reuse
`MetadataValidation`:

```rust
    /// Metadata outside the meter's declared closed surface.
    #[must_use]
    pub fn invalid_metadata(detail: impl Into<String>) -> Self {
        Self::invalid_argument_with_reason(
            "metadata",
            detail.into(),
            ValidationReason::MetadataValidation,
        )
    }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo nextest run -p cf-gears-usage-collector -E 'test(metadata_tests)'`
Expected: PASS, 8 tests.

- [ ] **Step 7: Commit**

```bash
git add gears/system/usage-collector
git commit -s -m "feat(usage-collector): compile a meter's closed metadata surface

The base type admits any string-valued key and a derived meter closes
the set; the two intersect to declared-keys-only. The subschema is
compiled once per declaration rather than per entry.

A meter declaring no metadata property gets an empty closed surface,
not an open one: defaulting to open would let an undeclared key reach
storage."
```

---

## Task 6: The `TypeResolver` cache

Cache policy is the whole point of this component, so it gets its own tests
against a fake `DeclarationSource` whose behaviour each test controls.

Required behaviour, from DESIGN §3.2 and §3.5:

- Miss populates from the source; hit serves without a second call.
- Concurrent misses on one key make **one** source call (single-flight).
- Past the TTL an entry refreshes.
- Source **error** past the TTL serves the **stale** entry: a registry outage
  degrades new-type introduction, not ingestion of existing types.
- Source error with nothing cached **fails closed**.
- A definite **not-found** fails closed and is not cached, so a type declared a
  moment later resolves without waiting out a negative TTL.

**Files:**
- Modify: `usage-collector/src/domain/type_resolver/mod.rs`
- Create: `usage-collector/src/domain/type_resolver/resolver_tests.rs`

- [ ] **Step 1: Write the failing tests**

Create `usage-collector/src/domain/type_resolver/resolver_tests.rs`:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;
use types_registry_sdk::GtsTypeSchema;
use usage_collector_sdk::{AggregationFold, MeterTypeId};

use crate::domain::error::DomainError;
use crate::domain::ports::declarations::DeclarationSource;

use super::{TypeResolver, TypeResolverConfig};

const METER: &str = "gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1~";
const BASE: &str = "gts.cf.core.uc.usage_record.v1~";

fn meter_id() -> MeterTypeId {
    MeterTypeId::new(METER).expect("valid meter id")
}

fn schema(unit: &str) -> GtsTypeSchema {
    let base = GtsTypeSchema::try_new(
        BASE.parse().unwrap(),
        json!({ "type": "object" }),
        None,
        None,
    )
    .unwrap();
    GtsTypeSchema::try_new(
        METER.parse().unwrap(),
        json!({
            "allOf": [
                { "$ref": format!("gts://{BASE}") },
                { "x-gts-traits": {
                    "aggregation_fold": "SUM",
                    "canonical_unit": unit,
                    "retention": "P125D"
                }}
            ]
        }),
        None,
        Some(Arc::new(base)),
    )
    .unwrap()
}

/// Scripted source: each call pops the next outcome, and the last one repeats.
struct FakeSource {
    outcomes: Mutex<Vec<Result<GtsTypeSchema, DomainError>>>,
    calls: AtomicUsize,
    delay: Option<Duration>,
}

impl FakeSource {
    fn new(outcomes: Vec<Result<GtsTypeSchema, DomainError>>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes),
            calls: AtomicUsize::new(0),
            delay: None,
        })
    }

    fn slow(outcomes: Vec<Result<GtsTypeSchema, DomainError>>, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes),
            calls: AtomicUsize::new(0),
            delay: Some(delay),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl DeclarationSource for FakeSource {
    async fn fetch(&self, _id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(d) = self.delay {
            tokio::time::sleep(d).await;
        }
        let mut outcomes = self.outcomes.lock().await;
        if outcomes.len() > 1 {
            outcomes.remove(0)
        } else {
            match outcomes.first() {
                Some(Ok(s)) => Ok(s.clone()),
                Some(Err(e)) => Err(e.clone()),
                None => Err(DomainError::internal("FakeSource exhausted")),
            }
        }
    }
}

fn cfg(ttl: Duration) -> TypeResolverConfig {
    TypeResolverConfig {
        ttl,
        capacity: 64,
    }
}

#[tokio::test]
async fn a_miss_populates_and_a_hit_serves_from_cache() {
    let source = FakeSource::new(vec![Ok(schema("bytes"))]);
    let resolver = TypeResolver::new(source.clone(), cfg(Duration::from_secs(300)));

    let first = resolver.resolve(&meter_id()).await.expect("resolves");
    assert_eq!(first.aggregation_fold, AggregationFold::Sum);
    assert_eq!(first.canonical_unit, "bytes");

    let second = resolver.resolve(&meter_id()).await.expect("resolves");
    assert_eq!(second.canonical_unit, "bytes");

    assert_eq!(source.calls(), 1, "a cache hit must not reach the registry");
}

#[tokio::test]
async fn concurrent_misses_make_one_source_call() {
    // Without single-flight a cold key under load fans a burst of identical
    // reads at types-registry, which is exactly the hot-path coupling the
    // cache exists to prevent.
    let source = FakeSource::slow(vec![Ok(schema("bytes"))], Duration::from_millis(50));
    let resolver = Arc::new(TypeResolver::new(source.clone(), cfg(Duration::from_secs(300))));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let r = resolver.clone();
        handles.push(tokio::spawn(async move { r.resolve(&meter_id()).await }));
    }
    for h in handles {
        h.await.expect("task joins").expect("resolves");
    }

    assert_eq!(source.calls(), 1, "single-flight must collapse concurrent misses");
}

#[tokio::test]
async fn an_entry_refreshes_past_the_ttl() {
    let source = FakeSource::new(vec![Ok(schema("bytes")), Ok(schema("count"))]);
    let resolver = TypeResolver::new(source.clone(), cfg(Duration::from_millis(20)));

    let first = resolver.resolve(&meter_id()).await.unwrap();
    assert_eq!(first.canonical_unit, "bytes");

    tokio::time::sleep(Duration::from_millis(40)).await;

    let second = resolver.resolve(&meter_id()).await.unwrap();
    assert_eq!(second.canonical_unit, "count", "past the TTL the entry refreshes");
    assert_eq!(source.calls(), 2);
}

#[tokio::test]
async fn a_registry_error_past_the_ttl_serves_the_stale_entry() {
    // DESIGN 3.5: cached declarations stay usable while the registry is
    // unreachable, so an outage degrades new-type introduction rather than
    // ingestion of existing types.
    let source = FakeSource::new(vec![
        Ok(schema("bytes")),
        Err(DomainError::TypesRegistryUnavailable("connect refused".into())),
    ]);
    let resolver = TypeResolver::new(source.clone(), cfg(Duration::from_millis(20)));

    resolver.resolve(&meter_id()).await.expect("first resolve");

    tokio::time::sleep(Duration::from_millis(40)).await;

    let stale = resolver
        .resolve(&meter_id())
        .await
        .expect("a stale entry must still serve while the registry is down");
    assert_eq!(stale.canonical_unit, "bytes");
}

#[tokio::test]
async fn a_registry_error_with_nothing_cached_fails_closed() {
    let source = FakeSource::new(vec![Err(DomainError::TypesRegistryUnavailable(
        "connect refused".into(),
    ))]);
    let resolver = TypeResolver::new(source, cfg(Duration::from_secs(300)));

    assert!(
        resolver.resolve(&meter_id()).await.is_err(),
        "with nothing cached the resolver must fail closed, never admit unvalidated"
    );
}

#[tokio::test]
async fn a_not_found_fails_closed_and_is_not_cached() {
    // Caching a negative answer would make a type declared a moment later
    // unusable until the entry expired.
    let source = FakeSource::new(vec![
        Err(DomainError::declaration_not_found(&meter_id())),
        Ok(schema("bytes")),
    ]);
    let resolver = TypeResolver::new(source.clone(), cfg(Duration::from_secs(300)));

    assert!(resolver.resolve(&meter_id()).await.is_err());

    let after = resolver
        .resolve(&meter_id())
        .await
        .expect("a freshly declared type resolves without waiting out a negative TTL");
    assert_eq!(after.canonical_unit, "bytes");
    assert_eq!(source.calls(), 2);
}

#[tokio::test]
async fn an_undeclared_attribute_fails_closed_and_is_not_cached_as_success() {
    let bad = GtsTypeSchema::try_new(
        BASE.parse().unwrap(),
        json!({ "type": "object" }),
        None,
        None,
    )
    .unwrap();
    let source = FakeSource::new(vec![Ok(bad), Ok(schema("bytes"))]);
    let resolver = TypeResolver::new(source.clone(), cfg(Duration::from_secs(300)));

    assert!(resolver.resolve(&meter_id()).await.is_err());
    assert!(resolver.resolve(&meter_id()).await.is_ok());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p cf-gears-usage-collector -E 'test(resolver_tests)'`
Expected: FAIL to compile — `cannot find type TypeResolver`.

- [ ] **Step 3: Implement the resolver**

Replace `usage-collector/src/domain/type_resolver/mod.rs`:

```rust
//! Type Resolver — resolves a meter's declaration from `types-registry`.
//!
//! Resolution sits on the ingestion hot path, and `types-registry` publishes
//! no latency obligation of its own, so a per-entry registry call would make
//! this gear's ingestion NFRs contingent on a second gear's availability and
//! latency. A local cache of resolved declarations keeps those obligations
//! self-contained.
//!
//! Fold, canonical unit and metadata surface are immutable for a type's life,
//! so a cached entry cannot silently change meaning. Only additions and
//! withdrawals propagate. The TTL is nonetheless load-bearing rather than a
//! convenience: `types-registry` is moving to a model where a major-only GTS
//! identifier names a mutable entity, and a no-expiry cache would not survive
//! it.

mod declaration;
mod metadata;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, RwLock};
use usage_collector_sdk::MeterTypeId;

use crate::domain::error::DomainError;
use crate::domain::ports::declarations::DeclarationSource;

pub use declaration::ResolvedDeclaration;
pub use metadata::CompiledMetadataSchema;

/// Cache policy for the [`TypeResolver`].
#[derive(Debug, Clone, Copy)]
pub struct TypeResolverConfig {
    /// How long a resolved declaration is served before it refreshes.
    pub ttl: Duration,
    /// Ceiling on cached declarations. One entry per meter, not per entry.
    pub capacity: usize,
}

/// One cached declaration plus the instant it was fetched.
#[derive(Clone)]
struct CacheEntry {
    declaration: Arc<ResolvedDeclaration>,
    fetched_at: Instant,
}

/// Resolves `gts_type_id` references to their declarations, fail-closed.
pub struct TypeResolver {
    source: Arc<dyn DeclarationSource>,
    config: TypeResolverConfig,
    entries: RwLock<HashMap<MeterTypeId, CacheEntry>>,
    /// One in-flight fetch per key. Collapses concurrent misses so a cold key
    /// under load does not fan a burst of identical reads at the registry.
    inflight: Mutex<HashMap<MeterTypeId, Arc<Mutex<()>>>>,
}

impl TypeResolver {
    /// Creates a resolver over `source`.
    #[must_use]
    pub fn new(source: Arc<dyn DeclarationSource>, config: TypeResolverConfig) -> Self {
        Self {
            source,
            config,
            entries: RwLock::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// Resolves `id` to its declaration.
    ///
    /// Serves a fresh cached entry directly. Past the TTL it refetches, and
    /// falls back to the stale entry when the source is unavailable. With
    /// nothing cached it fails closed.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError`] when the declaration does not resolve and no
    /// cached entry can stand in. The gear never admits an entry whose type
    /// it could not validate against.
    pub async fn resolve(
        &self,
        id: &MeterTypeId,
    ) -> Result<Arc<ResolvedDeclaration>, DomainError> {
        if let Some(entry) = self.fresh_entry(id).await {
            return Ok(entry);
        }

        // Serialize the fetch per key. The guard is taken before the second
        // freshness check so a waiter that arrives during a fetch observes the
        // populated entry rather than issuing its own.
        let gate = {
            let mut inflight = self.inflight.lock().await;
            Arc::clone(
                inflight
                    .entry(id.clone())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let _held = gate.lock().await;

        if let Some(entry) = self.fresh_entry(id).await {
            return Ok(entry);
        }

        match self.source.fetch(id).await {
            Ok(schema) => {
                let declaration =
                    Arc::new(ResolvedDeclaration::from_schema(id.clone(), &schema)?);
                self.store(id.clone(), Arc::clone(&declaration)).await;
                Ok(declaration)
            }
            Err(e) if e.is_declaration_not_found() => {
                // Not cached: a type declared a moment later must resolve
                // without waiting out a negative TTL.
                Err(e)
            }
            Err(e) => match self.stale_entry(id).await {
                Some(stale) => {
                    tracing::warn!(
                        gts_type_id = %id,
                        error = %e,
                        "serving a stale declaration: types-registry is unavailable"
                    );
                    Ok(stale)
                }
                None => Err(e),
            },
        }
    }

    /// A cached entry within the TTL.
    async fn fresh_entry(&self, id: &MeterTypeId) -> Option<Arc<ResolvedDeclaration>> {
        let entries = self.entries.read().await;
        entries.get(id).and_then(|e| {
            (e.fetched_at.elapsed() < self.config.ttl).then(|| Arc::clone(&e.declaration))
        })
    }

    /// A cached entry of any age.
    async fn stale_entry(&self, id: &MeterTypeId) -> Option<Arc<ResolvedDeclaration>> {
        let entries = self.entries.read().await;
        entries.get(id).map(|e| Arc::clone(&e.declaration))
    }

    async fn store(&self, id: MeterTypeId, declaration: Arc<ResolvedDeclaration>) {
        let mut entries = self.entries.write().await;

        // Capacity is a ceiling on distinct meters, which grows slowly. Evict
        // the oldest rather than pulling in an LRU crate for a map this shape.
        if entries.len() >= self.config.capacity && !entries.contains_key(&id) {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, e)| e.fetched_at)
                .map(|(k, _)| k.clone())
            {
                entries.remove(&oldest);
            }
        }

        entries.insert(
            id,
            CacheEntry {
                declaration,
                fetched_at: Instant::now(),
            },
        );
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "resolver_tests.rs"]
mod resolver_tests;
```


- [ ] **Step 4: Add the error helpers**

In `usage-collector/src/domain/error.rs`:

```rust
    /// Whether this is a definite not-found answer from `types-registry`,
    /// rather than a failure that leaves the question open.
    ///
    /// The distinction is load-bearing: a definite answer is a fact the
    /// resolver acts on, while an unavailable registry is a condition it can
    /// ride out on a stale entry.
    #[must_use]
    pub fn is_declaration_not_found(&self) -> bool {
        matches!(self, Self::UsageTypeNotFound { .. })
    }
```

Match the real variant name in the enum. If the gear's `NotFound` is a single
variant carrying a resource type, compare against the meter resource instead.
Also confirm `DomainError` derives `Clone` — the `FakeSource` in the tests
clones a stored error. If it does not, add `#[derive(Clone)]`, or have
`FakeSource` rebuild the error per call instead.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p cf-gears-usage-collector -E 'test(resolver_tests)'`
Expected: PASS, 7 tests.

- [ ] **Step 6: Verify no clippy regressions**

Run: `cargo clippy -p cf-gears-usage-collector --all-targets --all-features`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add gears/system/usage-collector/usage-collector/src
git commit -s -m "feat(usage-collector): add the Type Resolver cache

Serves resolved declarations from a TTL cache with single-flight
population, so a cold key under load makes one registry call rather
than a burst.

A registry error past the TTL serves the stale entry: an outage should
degrade the introduction of new types, not the ingestion of existing
ones. With nothing cached it fails closed. A definite not-found is not
cached, so a type declared a moment later resolves immediately."
```

---

## Task 7: Registry adapter, configuration, and wiring

**Files:**
- Create: `usage-collector/src/infra/types_registry_source.rs`
- Create: `usage-collector/src/infra/types_registry_source_tests.rs`
- Modify: `usage-collector/src/infra/mod.rs`
- Modify: `usage-collector/src/config.rs`
- Modify: `usage-collector/src/config_tests.rs`
- Modify: `usage-collector/src/domain/service.rs`
- Modify: `usage-collector/src/module.rs`

- [ ] **Step 1: Write the failing config test**

Append to `usage-collector/src/config_tests.rs`:

```rust
#[test]
fn type_cache_defaults_are_applied_when_absent() {
    let cfg: UsageCollectorConfig = toml::from_str("").expect("empty config parses");
    assert_eq!(cfg.type_cache_ttl_secs, 300);
    assert_eq!(cfg.type_cache_capacity, 10_000);
}

#[test]
fn type_cache_knobs_are_overridable() {
    let cfg: UsageCollectorConfig = toml::from_str(
        r#"
        type_cache_ttl_secs = 60
        type_cache_capacity = 500
        "#,
    )
    .expect("config parses");
    assert_eq!(cfg.type_cache_ttl_secs, 60);
    assert_eq!(cfg.type_cache_capacity, 500);
}

#[test]
fn a_zero_ttl_is_rejected() {
    // A zero TTL turns every ingestion into a registry round-trip, which is
    // the coupling the cache exists to remove.
    let cfg: UsageCollectorConfig =
        toml::from_str("type_cache_ttl_secs = 0").expect("config parses");
    assert!(cfg.validate().is_err());
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p cf-gears-usage-collector -E 'test(type_cache)'`
Expected: FAIL — no field `type_cache_ttl_secs`.

- [ ] **Step 3: Extend the config**

In `usage-collector/src/config.rs`, add to `UsageCollectorConfig`:

```rust
    /// How long a resolved GTS type declaration is served before the Type
    /// Resolver refreshes it, in seconds.
    ///
    /// Fold, unit and metadata surface are immutable for a type's life, so
    /// this is not a correctness window for them. It bounds how long a
    /// withdrawn declaration keeps resolving, and it is what keeps the cache
    /// honest once `types-registry` admits mutable major-only identifiers.
    pub type_cache_ttl_secs: u64,

    /// Ceiling on cached declarations. One entry per meter, not per entry,
    /// so realistic deployments sit far below the default.
    pub type_cache_capacity: usize,

    /// Cap on an entry's serialized metadata map, in bytes.
    ///
    /// DESIGN 3.1 makes the cap per deployment. It is enforced alongside the
    /// declared-shape check, not instead of it: a payload can sit inside the
    /// cap and still carry an undeclared key.
    pub metadata_size_cap_bytes: usize,
```

In `Default`:

```rust
            type_cache_ttl_secs: 300,
            type_cache_capacity: 10_000,
            metadata_size_cap_bytes: 8192,
```

If `usage-collector/src/domain/validation.rs` already enforces a hard-coded
metadata size cap, replace that constant with this configured value in Task 9
rather than leaving two caps in the codebase.

Add a validator (or extend the existing one):

```rust
impl UsageCollectorConfig {
    /// Checks the configuration is internally coherent.
    ///
    /// # Errors
    ///
    /// Returns a message naming the offending key.
    pub fn validate(&self) -> Result<(), String> {
        if self.type_cache_ttl_secs == 0 {
            return Err(
                "[usage_collector].type_cache_ttl_secs must be greater than 0: \
                 a zero TTL makes every ingestion a types-registry round-trip"
                    .to_owned(),
            );
        }
        if self.type_cache_capacity == 0 {
            return Err(
                "[usage_collector].type_cache_capacity must be greater than 0".to_owned(),
            );
        }
        Ok(())
    }
}
```

Call `cfg.validate()` in `module.rs` right after `ctx.config_or_default()?` and
fail `Gear::init` on error.

- [ ] **Step 4: Write the failing adapter test**

Create `usage-collector/src/infra/types_registry_source_tests.rs`:

```rust
use std::sync::Arc;

use serde_json::json;
use toolkit::client_hub::ClientHub;
use types_registry_sdk::testing::MockTypesRegistryClient;
use types_registry_sdk::{GtsTypeSchema, TypesRegistryClient};
use usage_collector_sdk::MeterTypeId;

use crate::domain::ports::declarations::DeclarationSource;

use super::TypesRegistryDeclarationSource;

const METER: &str = "gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1~";

fn meter_id() -> MeterTypeId {
    MeterTypeId::new(METER).unwrap()
}

fn registered_schema() -> GtsTypeSchema {
    let base = GtsTypeSchema::try_new(
        "gts.cf.core.uc.usage_record.v1~".parse().unwrap(),
        json!({ "type": "object" }),
        None,
        None,
    )
    .unwrap();
    GtsTypeSchema::try_new(
        METER.parse().unwrap(),
        json!({
            "allOf": [
                { "$ref": "gts://gts.cf.core.uc.usage_record.v1~" },
                { "x-gts-traits": {
                    "aggregation_fold": "SUM",
                    "canonical_unit": "bytes",
                    "retention": "P125D"
                }}
            ]
        }),
        None,
        Some(Arc::new(base)),
    )
    .unwrap()
}

fn hub_with(client: MockTypesRegistryClient) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::default());
    hub.register::<dyn TypesRegistryClient>(Arc::new(client));
    hub
}

#[tokio::test]
async fn fetches_a_registered_schema() {
    let hub = hub_with(MockTypesRegistryClient::new().with_type_schemas([registered_schema()]));
    let source = TypesRegistryDeclarationSource::new(hub);

    let schema = source.fetch(&meter_id()).await.expect("fetches");
    assert_eq!(schema.type_id.as_ref(), METER);
}

#[tokio::test]
async fn an_unregistered_type_is_a_definite_not_found() {
    let hub = hub_with(MockTypesRegistryClient::new());
    let source = TypesRegistryDeclarationSource::new(hub);

    let err = source.fetch(&meter_id()).await.expect_err("not registered");
    assert!(
        err.is_declaration_not_found(),
        "an unregistered type must be a definite answer so the resolver does \
         not serve a stale entry for it, got: {err:?}"
    );
}

#[tokio::test]
async fn a_missing_registry_client_is_not_a_not_found() {
    // No TypesRegistryClient on the hub is an availability problem, not a
    // statement that the type does not exist.
    let source = TypesRegistryDeclarationSource::new(Arc::new(ClientHub::default()));

    let err = source.fetch(&meter_id()).await.expect_err("no client");
    assert!(!err.is_declaration_not_found());
}
```

Confirm `ClientHub::default()` and `register` match the real API by reading
`Service::resolve_plugin` in `domain/service.rs`, which already does a
`hub.get::<dyn TypesRegistryClient>()`.

- [ ] **Step 5: Implement the adapter**

Create `usage-collector/src/infra/types_registry_source.rs`:

```rust
//! `DeclarationSource` adapter over the `types-registry` SDK client.
//!
//! Resolving the client per call rather than holding it keeps the gear's
//! startup free of a `types-registry` dependency, matching how the plugin
//! binding resolves lazily on first dispatch.

use std::sync::Arc;

use async_trait::async_trait;
use toolkit::client_hub::ClientHub;
use types_registry_sdk::{GtsTypeSchema, TypesRegistryClient};
use usage_collector_sdk::MeterTypeId;

use crate::domain::error::DomainError;
use crate::domain::ports::declarations::DeclarationSource;

/// Reads declarations from `types-registry` through `ClientHub`.
pub struct TypesRegistryDeclarationSource {
    hub: Arc<ClientHub>,
}

impl TypesRegistryDeclarationSource {
    /// Creates a source over `hub`.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>) -> Self {
        Self { hub }
    }
}

#[async_trait]
impl DeclarationSource for TypesRegistryDeclarationSource {
    async fn fetch(&self, id: &MeterTypeId) -> Result<GtsTypeSchema, DomainError> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| DomainError::TypesRegistryUnavailable(e.to_string()))?;

        registry
            .get_type_schema(id.as_str())
            .await
            .map_err(|e| map_registry_error(id, e))
    }
}

/// Classifies a registry failure into a definite not-found or an
/// availability problem.
///
/// The resolver treats the two differently: it fails closed on the first and
/// may serve a stale declaration for the second, so a misclassification here
/// either hides a withdrawn type or turns an outage into an ingestion stop.
fn map_registry_error(
    id: &MeterTypeId,
    err: toolkit_canonical_errors::CanonicalError,
) -> DomainError {
    if err.is_not_found() {
        DomainError::declaration_not_found(id)
    } else {
        DomainError::TypesRegistryUnavailable(err.to_string())
    }
}
```

`CanonicalError`'s not-found predicate may be spelled differently — read
`libs/toolkit-canonical-errors` and use whatever the type actually offers
(a `code()`/`status()` comparison is fine). Getting this classification right
is the point of the test in Step 4.

Add `pub mod types_registry_source;` to `usage-collector/src/infra/mod.rs` and
the test module hook at the bottom of the new file.

- [ ] **Step 6: Wire the resolver into `Service`**

In `usage-collector/src/domain/service.rs`:

- Add a field: `type_resolver: Arc<TypeResolver>`.
- Extend `Service::new` and `Service::new_with_metrics` to build it:

```rust
        let source = Arc::new(TypesRegistryDeclarationSource::new(Arc::clone(&hub)));
        let type_resolver = Arc::new(TypeResolver::new(
            source,
            TypeResolverConfig {
                ttl: Duration::from_secs(type_cache_ttl_secs),
                capacity: type_cache_capacity,
            },
        ));
```

- Both constructors take the two new config values. Update `module.rs` to pass
  `cfg.type_cache_ttl_secs` and `cfg.type_cache_capacity`.
- Add a test-only constructor so `service_tests.rs` can inject a fake source:

```rust
    /// Test-only: build a `Service` over a caller-supplied declaration source.
    #[cfg(test)]
    pub(crate) fn new_with_declaration_source(
        hub: Arc<ClientHub>,
        vendor: String,
        enforcer: PolicyEnforcer,
        source: Arc<dyn DeclarationSource>,
    ) -> Self {
        // ... same as new_with_metrics, but with the given source
    }
```

- [ ] **Step 7: Run the tests**

Run: `cargo nextest run -p cf-gears-usage-collector`
Expected: PASS. The resolver is constructed but not yet consulted, so no
existing behaviour changes.

- [ ] **Step 8: Commit**

```bash
git add gears/system/usage-collector/usage-collector
git commit -s -m "feat(usage-collector): wire the Type Resolver into the service

Adds the types-registry adapter behind the DeclarationSource port and
the two cache knobs on [usage_collector]. The adapter classifies a
definite not-found apart from an availability failure, which is the
distinction the resolver's stale-serving depends on.

The resolver is constructed but not yet consulted; the next tasks flip
the read and write paths onto it."
```

---

## Task 8: Serve the declared fold on the aggregate path

The first behavioural flip. The catalog stays, so the tree stays green.

**Files:**
- Modify: `usage-collector-sdk/src/api.rs`
- Modify: `usage-collector-sdk/src/plugin_api.rs`
- Modify: `plugins/noop-usage-collector-plugin/src/plugin.rs`
- Modify: `usage-collector/src/domain/service.rs`
- Modify: `usage-collector/src/domain/local_client.rs`
- Modify: `usage-collector/src/api/rest/{dto.rs,handlers/usage_records.rs,routes/usage_records.rs}`
- Test: `usage-collector/src/domain/service_tests.rs`

- [ ] **Step 1: Write the failing tests**

Append to `usage-collector/src/domain/service_tests.rs`. Follow the existing
fixtures in `domain/test_support.rs` for building a `Service` and a
`SecurityContext`:

```rust
#[tokio::test]
async fn aggregate_serves_the_fold_the_declaration_names() {
    // The request carries no aggregation parameter. Whatever fold reaches the
    // plugin must have come from the resolved declaration.
    let source = fake_declaration_source_with_fold("MAX");
    let (svc, spy) = service_with_recording_plugin(source).await;

    svc.query_aggregated_usage_records(
        &ctx(),
        meter_id(),
        &ODataQuery::default(),
        &[],
        &[],
    )
    .await
    .expect("aggregates");

    assert_eq!(spy.last_fold(), Some(AggregationFold::Max));
}

#[tokio::test]
async fn aggregate_fails_closed_when_the_type_does_not_resolve() {
    let source = fake_declaration_source_not_found();
    let (svc, spy) = service_with_recording_plugin(source).await;

    let err = svc
        .query_aggregated_usage_records(&ctx(), meter_id(), &ODataQuery::default(), &[], &[])
        .await
        .expect_err("an unresolvable type must not reach the plugin");

    assert!(matches!(err, UsageCollectorError::NotFound { .. }));
    assert_eq!(
        spy.calls(),
        0,
        "the plugin must not be dispatched for an unresolvable type"
    );
}
```

Add these helpers to `domain/test_support.rs`, alongside the existing plugin
spies, with exactly these signatures — Tasks 9, 12 and 13 call them too:

```rust
/// A `DeclarationSource` that always resolves to a meter declaring `fold`,
/// unit `bytes` and no metadata properties.
pub(crate) fn fake_declaration_source_with_fold(fold: &str) -> Arc<dyn DeclarationSource>;

/// A `DeclarationSource` resolving to a meter whose metadata surface declares
/// exactly `keys`.
pub(crate) fn fake_declaration_source_with_metadata(
    keys: &[&str],
) -> Arc<dyn DeclarationSource>;

/// A `DeclarationSource` that always answers a definite not-found.
pub(crate) fn fake_declaration_source_not_found() -> Arc<dyn DeclarationSource>;

/// A `DeclarationSource` counting its calls, so a batch test can assert one
/// resolution per distinct meter rather than one per record.
pub(crate) fn fake_declaration_source_counting() -> Arc<CountingDeclarationSource>;

/// Builds a `Service` over `source` and a plugin spy that records every
/// dispatch.
pub(crate) async fn service_with_recording_plugin(
    source: Arc<dyn DeclarationSource>,
) -> (Service, Arc<RecordingPlugin>);
```

`RecordingPlugin` implements `UsageCollectorPluginV1`, counts calls through
`calls() -> usize`, and captures the aggregate `fold` argument through
`last_fold() -> Option<AggregationFold>`. `CountingDeclarationSource` exposes
`fetch_calls() -> usize`. Build both on the existing spy in `test_support.rs`
rather than adding a second spy type.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo nextest run -p cf-gears-usage-collector -E 'test(aggregate_serves_the_fold) + test(aggregate_fails_closed)'`
Expected: FAIL to compile — the signature still takes an `AggregationSpec`.

- [ ] **Step 3: Change the SDK trait**

In `usage-collector-sdk/src/api.rs`, replace the `aggregation: AggregationSpec`
parameter with `group_by: &[AggregationDimension]`:

```rust
    /// Aggregated query over one meter.
    ///
    /// Carries no aggregation parameter: the fold is resolved from the
    /// queried type's declaration, so no request is well-formed and
    /// semantically wrong. A withdrawn record and its invalidation each
    /// contribute nothing.
    async fn query_aggregated_usage_records(
        &self,
        ctx: &SecurityContext,
        gts_id: UsageTypeGtsId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorError>;
```

`gts_id` stays `UsageTypeGtsId` until Task 10 — change one thing at a time.

- [ ] **Step 4: Change the SPI**

In `usage-collector-sdk/src/plugin_api.rs`:

```rust
    /// Compute the given fold over the authorized scope.
    ///
    /// The fold arrives as a parameter: declarations never reach the SPI.
    async fn query_aggregated_usage_records(
        &self,
        gts_id: UsageTypeGtsId,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError>;
```

- [ ] **Step 5: Update the noop plugin**

In `plugins/noop-usage-collector-plugin/src/plugin.rs`, change the signature to
match, keeping the empty-result body. Update `plugin_tests.rs` call sites.

- [ ] **Step 6: Update the service**

In `Service::query_aggregated_usage_records`:

- Delete the `require_op_allowed_for_kind` call and the `get_usage_type`
  lookup that fed it.
- Resolve the declaration and take the fold from it:

```rust
        let declaration = self.type_resolver.resolve(&gts_id_as_meter).await?;
        // ... existing PDP + scope composition ...
        plugin
            .query_aggregated_usage_records(
                gts_id,
                declaration.aggregation_fold,
                &composed,
                metadata_filter,
                group_by,
            )
            .await
```

Resolution must run before plugin dispatch, so an unresolvable type never
reaches the SPI.

- [ ] **Step 7: Update the REST surface**

In `usage-collector/src/api/rest/dto.rs`, delete the `aggregation` field from
the aggregate request DTO and keep `group_by`. Update
`handlers/usage_records.rs` to pass `group_by` through. Update the OpenAPI
schema annotation on the DTO to match `AggregationRequest` in
`docs/usage-collector-v1.yaml:1045`.

Also update `domain/local_client.rs`, which forwards the trait method.

- [ ] **Step 8: Delete the now-dead kind check**

Remove `require_op_allowed_for_kind` from `usage-collector/src/domain/query.rs`
and its tests from `query_tests.rs`. Remove `AggregationOp::is_allowed_for`
from the SDK. `AggregationOp` and `AggregationSpec` themselves stay until
Task 11.

- [ ] **Step 9: Run the tests**

Run: `cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk -p cf-gears-noop-usage-collector-plugin`
Expected: PASS. Existing aggregate tests need their call sites updated; delete
any test asserting a caller-chosen op is rejected for a kind — that rule is
gone, not relocated.

- [ ] **Step 10: Commit**

```bash
git add gears/system/usage-collector
git commit -s -m "feat(usage-collector): serve the declared fold on the aggregate path

The aggregate request no longer carries an aggregation parameter. The
fold is resolved from the queried meter's declaration and pushed to the
plugin, so a caller cannot select one and no request is well-formed and
semantically wrong.

Drops the kind/op compatibility check with it: there is no kind left to
check against, and the fold can no longer disagree with the meter."
```

---

## Task 9: Validate ingestion against the declaration

**Files:**
- Modify: `usage-collector/src/domain/service.rs`
- Modify: `usage-collector/src/domain/validation.rs`
- Test: `usage-collector/src/domain/{service_tests,validation_tests}.rs`

- [ ] **Step 1: Write the failing tests**

Append to `usage-collector/src/domain/service_tests.rs`:

```rust
#[tokio::test]
async fn ingestion_rejects_an_undeclared_metadata_key() {
    let source = fake_declaration_source_with_metadata(&["region"]);
    let (svc, spy) = service_with_recording_plugin(source).await;

    let mut record = valid_create_record();
    record.metadata.insert(
        MetadataKey::new("tier").unwrap(),
        "gold".to_owned(),
    );

    let err = svc
        .create_usage_record(&ctx(), record)
        .await
        .expect_err("an undeclared key must be rejected before persistence");
    assert!(err.to_string().contains("tier"));
    assert_eq!(spy.calls(), 0, "a rejected entry must not reach the plugin");
}

#[tokio::test]
async fn ingestion_accepts_declared_metadata_keys() {
    let source = fake_declaration_source_with_metadata(&["region"]);
    let (svc, spy) = service_with_recording_plugin(source).await;

    let mut record = valid_create_record();
    record.metadata.insert(
        MetadataKey::new("region").unwrap(),
        "eu-west-1".to_owned(),
    );

    svc.create_usage_record(&ctx(), record).await.expect("accepted");
    assert_eq!(spy.calls(), 1);
}

#[tokio::test]
async fn ingestion_fails_closed_when_the_type_does_not_resolve() {
    // 2.1 fail-closed: an unresolvable reference is rejected, never admitted
    // unvalidated to protect ingestion availability.
    let source = fake_declaration_source_not_found();
    let (svc, spy) = service_with_recording_plugin(source).await;

    assert!(svc.create_usage_record(&ctx(), valid_create_record()).await.is_err());
    assert_eq!(spy.calls(), 0);
}

#[tokio::test]
async fn a_batch_resolves_each_distinct_type_once() {
    // The resolver fan-out replaces the catalog fan-out: one resolution per
    // distinct gts_id, not one per record.
    let source = fake_declaration_source_counting();
    let (svc, _spy) = service_with_recording_plugin(source.clone()).await;

    let records = vec![valid_create_record(), valid_create_record(), valid_create_record()];
    svc.create_usage_records(&ctx(), records).await.expect("batch accepted");

    assert_eq!(source.fetch_calls(), 1, "three records of one type resolve once");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo nextest run -p cf-gears-usage-collector -E 'test(ingestion_rejects) + test(ingestion_accepts) + test(ingestion_fails_closed) + test(a_batch_resolves)'`
Expected: FAIL — the catalog path still validates metadata.

- [ ] **Step 3: Replace the catalog pre-pass with the resolver**

In `usage-collector/src/domain/service.rs`:

- In `create_usage_records_inner`, replace the `CatalogCache` /
  `get_usage_type` fan-out with a resolver fan-out over distinct `gts_id`s,
  keeping the same bounded concurrency (`CATALOG_FANOUT_CONCURRENCY`, renamed
  to `TYPE_RESOLUTION_FANOUT_CONCURRENCY`).
- Change the cache type:

```rust
/// Cached resolution per distinct meter, lifted into [`DomainError`] so one
/// outcome projects to every record sharing the type without re-resolving.
type DeclarationCache = HashMap<UsageTypeGtsId, Result<Arc<ResolvedDeclaration>, DomainError>>;
```

- In `create_usage_record_inner`, resolve and validate against the
  declaration rather than the `UsageType`.

- [ ] **Step 4: Replace metadata validation**

In `usage-collector/src/domain/validation.rs`, replace
`validate_submit_record_metadata`'s `UsageType`-driven body with a call to
`declaration.metadata_schema.validate(&record.metadata)`. Keep the
size-cap check. Delete the `UsageKind`-driven branches of
`validate_record_semantics` — with no kind there is no gauge/counter rule
left, and the compensation rules go in Task 11 anyway.

- [ ] **Step 5: Run the tests**

Run: `cargo nextest run -p cf-gears-usage-collector`
Expected: PASS. Update or delete existing metadata tests that build a
`UsageType`; the ones asserting an undeclared key is rejected should be kept
and repointed at the declaration.

- [ ] **Step 6: Commit**

```bash
git add gears/system/usage-collector/usage-collector
git commit -s -m "feat(usage-collector): validate ingestion against the declaration

Metadata is checked against the meter's compiled closed surface and the
size cap, and an unresolvable type is rejected before any plugin write.
The per-distinct-type resolver fan-out replaces the catalog fan-out at
the same bounded concurrency.

Removes the UsageKind-driven semantics branches: there is no kind to
drive them."
```

---

## Task 10: Swap `UsageTypeGtsId` for `MeterTypeId`

Mechanical but wide. Do it as one commit so no surface is left half-swapped.

**Files:** every file naming `UsageTypeGtsId` —
`usage-collector-sdk/src/{models,api,plugin_api,id,error,lib}.rs`,
`usage-collector/src/domain/{service,query,validation,authz,local_client}.rs`,
`usage-collector/src/api/rest/{dto,handlers/*,routes/*}.rs`,
`plugins/noop-usage-collector-plugin/src/plugin.rs`, and their `*_tests.rs`.

- [ ] **Step 1: Find every use site**

Run: `rg -n 'UsageTypeGtsId' gears/system/usage-collector apps/`
Expected: a list to work through. Record the count before you start.

**Close the length window while you are here.** Task 8 introduced a
temporary `meter_type_id_of` bridge in `domain/service.rs` converting
`UsageTypeGtsId` to `MeterTypeId` by appending `~`. The two types carry
different length ceilings — `gts-id` caps a whole identifier at 1024 bytes and
`MeterTypeId` at 512, from the JSON schema's `maxLength`. So an identifier can
pass `UsageTypeGtsId::new` and then fail `MeterTypeId::new`, which today
surfaces as a 500 on the aggregate path rather than a 400 at the boundary.

Making `MeterTypeId` the parameter type removes that window by construction:
validation moves to the wire boundary and a too-long identifier is rejected
once, as a validation error. Delete `meter_type_id_of` with the swap, and add
a test that an over-long identifier is rejected as `InvalidArgument` rather
than reaching a handler.

- [ ] **Step 2: Write a failing test pinning the wire name**

In `usage-collector/src/api/rest/dto_tests.rs`:

```rust
#[test]
fn record_dto_names_the_type_reference_gts_type_id() {
    // usage-collector-v1.yaml renamed gts_id to gts_type_id on every shape.
    let json = serde_json::to_value(sample_usage_record_dto()).unwrap();
    assert!(json.get("gts_type_id").is_some(), "wire field is gts_type_id");
    assert!(json.get("gts_id").is_none(), "the old name must be gone");
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo nextest run -p cf-gears-usage-collector -E 'test(record_dto_names_the_type_reference)'`
Expected: FAIL — `gts_id` is still the field name.

- [ ] **Step 4: Swap the type and the field name**

Replace `UsageTypeGtsId` with `MeterTypeId` at every site, and rename the
struct field and its serde name from `gts_id` to `gts_type_id` on
`UsageRecord`, `CreateUsageRecord`, `UsageRecordQuery` and every DTO.

`UsageType` still carries a `gts_id` — leave it; the whole type goes in
Task 11.

Update `derive_usage_record_id`'s parameter type in
`usage-collector-sdk/src/id.rs`. The derivation inputs do not change in this
task, only the type of the reference.

- [ ] **Step 5: Run everything**

Run: `cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk -p cf-gears-noop-usage-collector-plugin`
Expected: PASS.

Run: `rg -n 'UsageTypeGtsId' gears/system/usage-collector --glob '!**/plugins/timescaledb-*/**'`
Expected: hits only inside `UsageType` and its own tests.

- [ ] **Step 6: Commit**

```bash
git add gears/system/usage-collector
git commit -s -m "refactor(usage-collector)!: reference meters by MeterTypeId

A meter is a derived GTS type, not an instance, so the reference on
every entry, query and DTO becomes MeterTypeId and the wire field
becomes gts_type_id, matching usage-collector-v1.yaml.

BREAKING CHANGE: the gts_id field is renamed to gts_type_id on every
REST and SDK shape."
```

---

## Task 11: Delete the usage-type catalog

The last task of the slice. Everything the catalog owned is now served by the
resolver, so it comes out in one piece.

**Files:**
- Modify: `usage-collector-sdk/src/{models,api,plugin_api,gts,lib,error,reason}.rs`
- Delete: `usage-collector/src/api/rest/routes/usage_types.rs`
- Delete: `usage-collector/src/api/rest/routes/usage_types_tests.rs`
- Delete: `usage-collector/src/api/rest/handlers/usage_types.rs`
- Delete: `usage-collector/src/api/rest/handlers/usage_types_tests.rs`
- Modify: `usage-collector/src/api/rest/{dto,routes/mod,handlers/mod}.rs`
- Modify: `usage-collector/src/domain/{service,authz,local_client,validation}.rs`
- Modify: `usage-collector/src/domain/ports/metrics.rs`, `src/infra/metrics.rs`
- Modify: `usage-collector/src/module.rs`
- Modify: `plugins/noop-usage-collector-plugin/src/plugin.rs`

- [ ] **Step 1: Write the failing test**

In `usage-collector/src/api/rest/routes/registration_tests.rs`:

```rust
#[test]
fn no_usage_type_route_is_registered() {
    // 2.2 constraint-no-type-catalog: this gear exposes no usage-type
    // endpoint at all, read or write. Declarations are a types-registry
    // surface.
    let registry = build_test_registry();
    let paths: Vec<String> = registry.registered_paths();

    assert!(
        !paths.iter().any(|p| p.contains("usage-types")),
        "a usage-type route survived: {paths:?}"
    );
}
```

Match `build_test_registry` and `registered_paths` to whatever the existing
tests in that file use to enumerate registered routes.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p cf-gears-usage-collector -E 'test(no_usage_type_route)'`
Expected: FAIL — four usage-type routes are registered.

- [ ] **Step 3: Delete the REST surface**

Delete `routes/usage_types.rs`, `routes/usage_types_tests.rs`,
`handlers/usage_types.rs`, `handlers/usage_types_tests.rs`. Remove
`mod usage_types;` from `routes/mod.rs` and `handlers/mod.rs`, and the
`register_usage_type_routes` call from `register_api_routes`. Remove every
usage-type DTO from `dto.rs`.

- [ ] **Step 4: Delete the SDK surface**

From `usage-collector-sdk/src/api.rs`: `create_usage_type`, `get_usage_type`,
`list_usage_types`, `delete_usage_type`.

From `usage-collector-sdk/src/plugin_api.rs`: the same four.

From `usage-collector-sdk/src/models.rs`: `UsageType`, `UsageTypeQuery`,
`UsageTypeFilterField`, `UsageTypeGtsId`, `UsageKind`, `AggregationOp`,
`AggregationSpec`, `is_keyset_safe_type_field`.

From `usage-collector-sdk/src/gts.rs`: `USAGE_TYPE_RESOURCE`.

From `usage-collector-sdk/src/lib.rs`: every corresponding `pub use`, and the
stale doc-comment naming `UsageType` and `AggregationOp`.

From `usage-collector-sdk/src/reason.rs`: `GaugeCompensationRejected`,
`OpNotAllowedForKind`, `UsageTypeReferenced`. Keep the compensation-related
`ConflictReason` variants for now — Task 4 of the record-model slice removes
them with `corrects_id`.

From `usage-collector-sdk/src/error.rs`: `UsageTypeNotFound` on the **plugin**
error enum, `invalid_usage_kind`, and any usage-type constructor. Keep the
gear-side `DomainError` not-found path the resolver uses.

- [ ] **Step 5: Delete the service and authz surface**

From `domain/service.rs`: the four usage-type methods, `CatalogCache` if any
remnant survives Task 9, and the three
`USAGE_TYPES_GAUGE_REFRESH_TIMEOUT` / `_PAGE_LIMIT` / `_MAX_PAGES` constants
plus the gauge-refresh loop that reads them. Remove the refresh call from
`module.rs`'s `serve`.

From `domain/authz.rs`: the whole `usage_type` module — its `RESOURCE` and
`actions`.

From `domain/local_client.rs`: the four forwarding methods.

- [ ] **Step 6: Delete the metrics**

From `domain/ports/metrics.rs`: `UsageTypeOp`, `UsageTypeErrorCategory`, the
usage-type counter methods on the trait, and the catalog-size gauge. Mirror
the deletions in `infra/metrics.rs` and its tests.

Add the resolution instruments DESIGN §3.11.5 requires in their place:

```rust
    /// Records one type-resolution outcome.
    fn record_type_resolution(&self, outcome: TypeResolutionOutcome);
```

with

```rust
/// Outcome of one Type Resolver call, for the failure and staleness
/// instruments DESIGN 3.11.5 requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeResolutionOutcome {
    /// Served from a fresh cache entry.
    CacheHit,
    /// Fetched from `types-registry`.
    Fetched,
    /// Served from a cached entry past its TTL because the registry failed.
    ServedStale,
    /// The type does not resolve. The operation is rejected.
    FailedClosed,
}
```

Call it from `TypeResolver::resolve` on each of the four paths.

- [ ] **Step 7: Delete the noop plugin's catalog methods**

Remove the four from `plugins/noop-usage-collector-plugin/src/plugin.rs` and
their tests.

- [ ] **Step 8: Strip obsolete traceability annotations**

Run: `rg -n '@cpt-(begin|end|dod|flow):.*usage-type-lifecycle' gears/system/usage-collector/usage-collector gears/system/usage-collector/usage-collector-sdk gears/system/usage-collector/plugins/noop-usage-collector-plugin`

Delete every marker the search returns, and the same for
`...-event-deactivation-...` markers on code this task removes. Leave markers
whose ID still appears in `docs/DESIGN.md` or `docs/PRD.md`. Verify a marker
before keeping it:

Run: `rg -n '<the-id>' gears/system/usage-collector/docs/DESIGN.md gears/system/usage-collector/docs/PRD.md`

Do not invent replacement IDs.

- [ ] **Step 9: Run everything**

Run: `cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk -p cf-gears-noop-usage-collector-plugin`
Expected: PASS.

Run: `rg -n 'UsageType\b|UsageKind|AggregationOp|AggregationSpec|usage-types' gears/system/usage-collector --glob '!**/plugins/timescaledb-*/**' --glob '!**/docs/**'`
Expected: no hits.

Run: `cargo clippy --workspace --all-targets --all-features`
Expected: no warnings.

- [ ] **Step 10: Commit**

```bash
git add -A gears/system/usage-collector
git commit -s -m "feat(usage-collector)!: remove the usage-type catalog

types-registry owns every type declaration, so this gear exposes no
usage-type operation on any surface and keeps no second catalog that
would have to be kept in step. The Type Resolver now serves every fold,
unit and metadata-surface read the catalog used to.

Replaces the catalog-size gauge and the usage-type counters with the
type-resolution failure and staleness instruments of DESIGN 3.11.5.

Drops traceability markers naming feature IDs the reworked DESIGN
deleted.

BREAKING CHANGE: the /usage-collector/v1/usage-types endpoints, the four
usage-type SDK methods, the four usage-type SPI methods, and the
UsageType, UsageKind, AggregationOp and AggregationSpec types are gone."
```

---

## Task 12: Declared metadata keys as the query filter and grouping surface

`CompiledMetadataSchema::declared_keys()` exists but nothing reads it yet.
Spec §3.11: the admissible filter and `group_by` set is the eight fixed fields
plus the queried type's declared metadata keys, **recomputed per request**, so
a freshly declared property is usable on the next request without a restart.

**Files:**
- Modify: `usage-collector/src/domain/query.rs`
- Modify: `usage-collector/src/domain/service.rs`
- Test: `usage-collector/src/domain/query_tests.rs`

- [ ] **Step 1: Write the failing tests**

Append to `usage-collector/src/domain/query_tests.rs`:

```rust
use std::collections::BTreeSet;

fn declared(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|k| (*k).to_owned()).collect()
}

#[test]
fn a_declared_metadata_key_is_groupable() {
    let dims = [AggregationDimension::Metadata(
        MetadataKey::new("region").unwrap(),
    )];
    require_dimensions_declared(&dims, &declared(&["region", "storage_class"]))
        .expect("a declared key is groupable");
}

#[test]
fn an_undeclared_metadata_key_is_rejected_and_named() {
    let dims = [AggregationDimension::Metadata(
        MetadataKey::new("tier").unwrap(),
    )];
    let err = require_dimensions_declared(&dims, &declared(&["region"]))
        .expect_err("an undeclared key must not be groupable");
    assert!(err.to_string().contains("tier"));
}

#[test]
fn the_fixed_dimensions_need_no_declaration() {
    let dims = [
        AggregationDimension::TenantId,
        AggregationDimension::ResourceId,
    ];
    require_dimensions_declared(&dims, &declared(&[])).expect("fixed fields always admissible");
}

#[test]
fn gts_type_id_is_reserved_as_a_filter_field() {
    // 3.11: it travels as a typed parameter, so any predicate touching it is
    // rejected rather than silently honored.
    let filter = ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("gts_type_id".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::String("x".to_owned()))),
    );
    assert!(reject_reserved_filter_fields(&filter).is_err());
}

#[test]
fn the_window_bounds_are_not_filterable() {
    for field in ["window_start", "window_end"] {
        let filter = ast::Expr::Compare(
            Box::new(ast::Expr::Identifier(field.to_owned())),
            ast::CompareOperator::Eq,
            Box::new(ast::Expr::Value(ast::Value::String("x".to_owned()))),
        );
        assert!(
            reject_reserved_filter_fields(&filter).is_err(),
            "{field} must not be filterable: the time range is a first-class parameter"
        );
    }
}
```

`window_start` / `window_end` do not exist as fields until Slice 3. Until then
those two assertions guard names the query surface must never admit, which is
exactly what a reservation test is for — keep them.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo nextest run -p cf-gears-usage-collector -E 'test(declared) + test(reserved) + test(filterable)'`
Expected: FAIL to compile — neither helper exists.

- [ ] **Step 3: Implement the admissibility checks**

Add to `usage-collector/src/domain/query.rs`:

```rust
/// Field names a caller may never name in a `$filter`.
///
/// `gts_type_id` travels as a typed parameter and the covered period as a
/// typed time range, so a predicate over either would express a second,
/// possibly contradictory, constraint on something already fixed.
const RESERVED_FILTER_FIELDS: &[&str] = &["gts_type_id", "window_start", "window_end"];

/// Rejects a filter naming a reserved field.
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] naming the field.
pub(crate) fn reject_reserved_filter_fields(
    filter: &ast::Expr,
) -> Result<(), UsageCollectorError> {
    // Walk the whole AST: a reserved identifier nested under an `or` is just
    // as much a constraint as one at the top level.
    match filter {
        ast::Expr::Identifier(name) if RESERVED_FILTER_FIELDS.contains(&name.as_str()) => {
            Err(UsageCollectorError::reserved_filter_field(name))
        }
        ast::Expr::Identifier(_) | ast::Expr::Value(_) => Ok(()),
        ast::Expr::Not(inner) => reject_reserved_filter_fields(inner),
        ast::Expr::And(l, r) | ast::Expr::Or(l, r) => {
            reject_reserved_filter_fields(l)?;
            reject_reserved_filter_fields(r)
        }
        ast::Expr::Compare(l, _, r) => {
            reject_reserved_filter_fields(l)?;
            reject_reserved_filter_fields(r)
        }
        ast::Expr::In(l, items) => {
            reject_reserved_filter_fields(l)?;
            items.iter().try_for_each(reject_reserved_filter_fields)
        }
        ast::Expr::Function(_, args) => {
            args.iter().try_for_each(reject_reserved_filter_fields)
        }
    }
}

/// Checks every grouping dimension is a fixed field or a declared metadata
/// property.
///
/// `declared_keys` is read from the resolved declaration per request, so a
/// property declared a moment ago is usable on the next call.
///
/// # Errors
///
/// Returns [`UsageCollectorError::InvalidArgument`] naming the first
/// undeclared property.
pub(crate) fn require_dimensions_declared(
    dimensions: &[AggregationDimension],
    declared_keys: &BTreeSet<String>,
) -> Result<(), UsageCollectorError> {
    for dim in dimensions {
        if let AggregationDimension::Metadata(key) = dim {
            if !declared_keys.contains(key.as_str()) {
                return Err(UsageCollectorError::undeclared_metadata_dimension(
                    key.as_str(),
                ));
            }
        }
    }
    Ok(())
}
```

Add the two error constructors to `usage-collector-sdk/src/error.rs` following
the existing shape, reusing `ValidationReason::UnknownMetadataKey` for the
second and `ValidationReason::Validation` for the first.

- [ ] **Step 4: Call them from both read paths**

In `Service::query_aggregated_usage_records` and `Service::list_usage_records`,
after resolving the declaration and before composing the scope:

```rust
        if let Some(filter) = query.filter() {
            reject_reserved_filter_fields(filter)?;
        }
        require_dimensions_declared(
            group_by,
            declaration.metadata_schema.declared_keys(),
        )?;
```

`list_usage_records` takes no `group_by`; call only the filter check there.

- [ ] **Step 5: Run the tests**

Run: `cargo nextest run -p cf-gears-usage-collector`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add gears/system/usage-collector
git commit -s -m "feat(usage-collector): gate the query surface on the declaration

A grouping dimension must be a fixed field or a property the queried
meter's declaration declares, recomputed per request so a freshly
declared property is usable on the next call.

Reserves gts_type_id and both window bounds as filter fields: each
travels as a typed parameter, so a predicate over one would express a
second and possibly contradictory constraint on something already
fixed."
```

---

## Task 13: Compile the PDP scope into the point lookup

Spec §3.5's second SPI shape change. Not typing work, but it edits the same SPI
signature block Task 8 touched, so doing it inside this slice avoids a second
breaking SPI revision for one parameter.

Today `get_usage_record(id)` reads without a scope, and the service filters
afterwards or not at all. DESIGN §3.3 requires the compiled scope to reach the
plugin so a row outside it is never returned — the point lookup reports
`NotFound` rather than acting as an existence oracle.

**Files:**
- Modify: `usage-collector-sdk/src/plugin_api.rs`
- Modify: `plugins/noop-usage-collector-plugin/src/plugin.rs`
- Modify: `usage-collector/src/domain/service.rs`
- Test: `usage-collector/src/domain/service_tests.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn the_point_lookup_passes_the_compiled_scope_to_the_plugin() {
    // DESIGN 3.3: the read runs under the compiled scope, so an entry outside
    // it is NotFound and this surface is not an existence oracle.
    let source = fake_declaration_source_with_fold("SUM");
    let (svc, spy) = service_with_recording_plugin(source).await;

    let _ = svc.get_usage_record(&ctx(), Uuid::new_v4()).await;

    assert!(
        spy.last_get_scope().is_some(),
        "the plugin must receive a compiled scope, not an unscoped read"
    );
}
```

Add `last_get_scope() -> Option<String>` to `RecordingPlugin`, storing the
scope expression's `Debug` rendering.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo nextest run -p cf-gears-usage-collector -E 'test(the_point_lookup_passes)'`
Expected: FAIL to compile — no `last_get_scope`, and the SPI takes no scope.

- [ ] **Step 3: Change the SPI**

```rust
    /// Read one entry by identifier, with its correction linkage.
    ///
    /// `scope` is the compiled PDP scope, projected into a `toolkit_odata`
    /// filter. A row outside it is not returned.
    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
    ) -> Result<UsageRecord, UsageCollectorPluginError>;
```

- [ ] **Step 4: Update the noop plugin and the service**

Noop keeps returning `UsageRecordNotFound`, with the parameter named `_scope`.

In `Service::get_usage_record`, build the expression with the existing
`authz::scope_to_odata_filter(&scope)` — the same helper
`compose_query_with_scope` uses — and pass it through. The point lookup carries
no caller filter, so the compiled scope is the whole filter.

- [ ] **Step 5: Run everything**

Run: `cargo nextest run -p cf-gears-usage-collector -p cf-gears-usage-collector-sdk -p cf-gears-noop-usage-collector-plugin`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add gears/system/usage-collector
git commit -s -m "feat(usage-collector)!: read the point lookup under the compiled scope

The PDP scope now reaches the plugin on get_usage_record, so an entry
outside the caller's scope is never returned and the surface reports
NotFound rather than acting as an existence oracle.

BREAKING CHANGE: UsageCollectorPluginV1::get_usage_record takes the
compiled scope expression."
```

---

## Remaining slices

These are the record-model half of the approved spec. Each gets its own
detailed plan, written against the code as it stands once the slice before it
lands — writing their steps now would mean guessing at a codebase shape that
does not exist yet.

### Slice 3: Time model

`created_at` becomes `[window_start, window_end)`. Identity derivation moves to
the ADR-0007 5-tuple with the fixed 27-character timestamp form, rejecting
sub-microsecond precision and a `60` second rather than truncating. `TimeRange`
becomes a typed parameter on both read paths. Selection becomes
`from <= window_end < to` everywhere. `require_bounded_time_window` and
`ValidationReason::MissingTimeWindow` come out.

Tasks: SDK window fields and validation; the new derivation with golden
vectors; `TimeRange` on the SDK trait and SPI; period-end selection in the
query composer; the keyset moves to `(window_end, id)`; REST parameters.

### Slice 4: Correction model

`status`, `corrects_id` and `deactivate_usage_record` come out. `invalidates`,
`reason_code` and the derived `entry_type` go in, with the invalidation rules:
both-or-neither, resolvable target, target is itself a record, at most one
invalidation, faithful copy with a per-field diagnostic, quantity echoed not
negated. `UsageRecordStatus` and the remaining `CorrectsId*` conflict reasons
come out; `AlreadyInvalidated` goes in.

Tasks: SDK shape; the faithful-copy comparator; the target pre-check fan-out;
plugin SPI conflict variants; withdrawal exclusion on the aggregate path;
REST DTOs.

### Slice 5: Origin and backfill

`RecordOrigin` stamped from the path. `backfill_usage_records` on the SDK trait
and `POST /records/backfill`. The live path's two-sided window bound and the
backfill window with its elevated-authorization rule beyond it. The four new
config keys.

Tasks: origin stamping; the live bounds with `FUTURE_WINDOW` / `PAST_WINDOW`
and the message naming the backfill route; the backfill route and its authz
action; workload isolation.

### Slice 6: Errors and the contract gate

Final reason-vocabulary pass. `UsageCollectorPluginError` narrowed to the six
variants of DESIGN §3.3. The six drift tests in
`api/rest/routes/openapi_contract_tests.rs` re-enabled against an explicit
implemented-operation set, with `/feed` and `/reconciliation` named in a
not-yet-implemented list. The §3.3 plugin contract suite scaffolded in
`usage-collector-sdk` against the noop plugin, without
`feed-snapshot-and-replay`.

**Add a scope-enforcement case to that suite.** Task 13's review found that no
test anywhere — service-level or plugin-contract-level — exercises a plugin
*filtering out* a real stored row that fails a non-trivial compiled scope.
Every test double either ignores the `scope` argument or is not-found by
construction.

That matters because Task 13 retired the point lookup's in-process per-record
attribution check, per DESIGN §3.2 and §3.3, which put GET on the same
scope-as-filter posture as the raw and aggregate paths. The "exists but not
yours reads as `NotFound`" guarantee now rests entirely on each storage plugin
intersecting the filter it is handed. That is the same trust boundary the other
two read paths already operate under — but GET previously had a second layer
and no longer does.

The case: a plugin MUST NOT return a row that fails its own scope filter,
exercised against a test double that genuinely filters rather than one that
ignores the argument. One case closes the gap for all three read paths at once.

## Out of scope for every slice above

The usage feed, reconciliation, the ADR-0015 declaration mirror, ingestion
quotas (`fr-rate-limiting`, not implemented before this work either), the
TimescaleDB plugin port, and refreshing `docs/DECOMPOSITION.md` and
`docs/features/*.md`. See §7 of the spec.
