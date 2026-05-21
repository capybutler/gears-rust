<!--
Created:  2026-05-14 by Constructor Tech
Updated:  2026-05-20 by Constructor Tech
-->
# Feature: Module Scaffold


<!-- toc -->

- [Feature Status](#feature-status)
- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [ModKit Module Registration](#modkit-module-registration)
  - [Smoke Test — Module Loads](#smoke-test--module-loads)
- [4. States (CDSL)](#4-states-cdsl)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Cargo Crate Wired Into Workspace](#cargo-crate-wired-into-workspace)
  - [ModKit Module Declaration](#modkit-module-declaration)
  - [Layer Skeleton Directories](#layer-skeleton-directories)
  - [Baseline DomainError Enum](#baseline-domainerror-enum)
  - [Module Registers In Example Server](#module-registers-in-example-server)
  - [Smoke Test](#smoke-test)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Non-Functional Considerations](#7-non-functional-considerations)

<!-- /toc -->

## Feature Status

- [ ] `p1` - **ID**: `cpt-cf-serverless-runtime-featstatus-module-scaffold`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-serverless-runtime-feature-module-scaffold`

## 1. Feature Context

### 1.1 Overview

Bootstrap the `serverless-runtime` host crate as a ModKit module: Cargo.toml + `src/lib.rs` with `#[modkit::module]`, root-workspace wiring, skeleton layer directories (`api/`, `domain/`, `infra/`), a baseline `DomainError` enum, and a smoke test that confirms the module registers in `cyberware-example-server`. This feature is the **foundation** for every other host feature — no upstream feature dependencies, no SDK or plugin dependencies at compile time.

### 1.2 Purpose

Provide the minimum host-crate surface that ModKit recognizes, so subsequent features (Function Registry, REST surface, etc.) have a place to live and a known module-lifecycle entry point. Establishes the layer convention (`api/`, `domain/`, `infra/`) used by every later feature.

**Requirements**: `cpt-cf-serverless-runtime-nfr-composition-deps`

**Principles**: `cpt-cf-serverless-runtime-principle-pluggable-adapters`

### 1.3 Actors

Not applicable — this feature has no user-facing actor flows. It is infrastructure: a Cargo crate, a `#[modkit::module]` registration, and a smoke test. Subsequent features (F-02 Function Registry onward) introduce the Application Developer / Tenant Admin / Platform Operator interactions described in PRD §2.

### 1.4 References

- **PRD**: [PRD.md](../../../docs/PRD.md)
- **Design**: [DESIGN.md](../../../docs/DESIGN.md)
- **Decomposition**: [DECOMPOSITION.md §2.1](../DECOMPOSITION.md) — `cpt-cf-serverless-runtime-feature-module-scaffold`
- **ADR**: [ADR-0005 Thin Host Module, Fat Runtime Plugins](../../../docs/ADR/0005-cpt-cf-serverless-runtime-adr-thin-host.md)
- **ModKit reference**: `docs/modkit_unified_system/01_overview.md`, `docs/modkit_unified_system/02_module_layout_and_sdk_pattern.md`, `docs/modkit_unified_system/08_lifecycle_stateful_tasks.md`
- **Dependencies**: None (foundation feature)

## 2. Actor Flows (CDSL)

Not applicable — see §1.3. No actor-triggered flows belong in a pure scaffold feature. End-to-end user flows begin in F-02 Function Registry and later features.

## 3. Processes / Business Logic (CDSL)

### ModKit Module Registration

- [ ] `p1` - **ID**: `cpt-cf-serverless-runtime-algo-module-scaffold-modkit-registration`

**Input**: ModKit module-orchestrator startup (host-process boot).

**Output**: `serverless-runtime` module registered with ModKit, lifecycle hooks wired, ClientHub slot reserved.

**Steps**:

1. [ ] - `p1` - Declare `#[modkit::module]` on the host crate's module struct in `src/lib.rs` - `inst-declare-modkit-module`
2. [ ] - `p1` - Implement the ModKit `Module` trait stub (no clients registered yet in this feature) - `inst-impl-module-trait`
3. [ ] - `p1` - Register the module in the root `cyberware-example-server` composition root - `inst-register-in-example-server`
4. [ ] - `p1` - **RETURN** module-registration result via ModKit lifecycle - `inst-return-registration`

### Smoke Test — Module Loads

- [ ] `p1` - **ID**: `cpt-cf-serverless-runtime-algo-module-scaffold-smoke-test`

**Input**: Test binary spawning `cyberware-example-server` with the new module included.

**Output**: Assertion that the module registers without panic and exposes a non-empty module identity.

**Steps**:

1. [ ] - `p1` - Boot `cyberware-example-server` test harness with `serverless-runtime` module included - `inst-boot-test-harness`
2. [ ] - `p1` - Query ModKit's directory/registry for the `serverless-runtime` module identity - `inst-query-registry`
3. [ ] - `p1` - **IF** module is absent OR registration emitted an error - `inst-check-presence`
   1. [ ] - `p1` - **RETURN** test FAIL with the diagnostic message - `inst-fail`
4. [ ] - `p1` - **RETURN** test PASS - `inst-pass`

## 4. States (CDSL)

Not applicable — the scaffold itself has no entity lifecycle. Module lifecycle is fully owned by ModKit's `Module` trait and is not redefined here.

## 5. Definitions of Done

### Cargo Crate Wired Into Workspace

- [ ] `p1` - **ID**: `cpt-cf-serverless-runtime-dod-module-scaffold-cargo-wired`

The system **MUST** add `modules/serverless-runtime/serverless-runtime/Cargo.toml` as a member of the root Cargo workspace, with the crate name `serverless-runtime` and dependencies limited to ModKit core, the SDK crate (`serverless-runtime-sdk`), and any other crates already required by the ModKit `#[modkit::module]` macro. **MUST NOT** depend on any plugin crate at compile time (per `cpt-cf-serverless-runtime-adr-thin-host`).

**Implements**:
- `cpt-cf-serverless-runtime-algo-module-scaffold-modkit-registration`

**Touches**:
- Files: `modules/serverless-runtime/serverless-runtime/Cargo.toml`, root `Cargo.toml`

### ModKit Module Declaration

- [ ] `p1` - **ID**: `cpt-cf-serverless-runtime-dod-module-scaffold-modkit-declaration`

The system **MUST** declare the module struct in `src/lib.rs` annotated with `#[modkit::module]` and implementing the ModKit `Module` trait at the level required for registration. The implementation **MUST** be minimal — no client registrations, no lifecycle background tasks — those belong to later features.

**Implements**:
- `cpt-cf-serverless-runtime-algo-module-scaffold-modkit-registration`

**Touches**:
- Files: `modules/serverless-runtime/serverless-runtime/src/lib.rs`

### Layer Skeleton Directories

- [ ] `p1` - **ID**: `cpt-cf-serverless-runtime-dod-module-scaffold-layer-dirs`

The system **MUST** create `src/api/`, `src/domain/`, and `src/infra/` directories with at least an empty `mod.rs` (or equivalent) each, so subsequent features have a known layer to place code in. Layer split follows ModKit unified system docs (`02_module_layout_and_sdk_pattern.md`).

**Touches**:
- Files: `modules/serverless-runtime/serverless-runtime/src/api/mod.rs`, `src/domain/mod.rs`, `src/infra/mod.rs`

### Baseline DomainError Enum

- [ ] `p1` - **ID**: `cpt-cf-serverless-runtime-dod-module-scaffold-domain-error`

The system **MUST** define a module-level `DomainError` enum stub in `src/domain/mod.rs` (or `src/domain/error.rs`) with at minimum a `Internal(String)` variant, deriving `Debug` + `thiserror::Error`. This is the placeholder for subsequent feature-specific error variants. RFC-9457 Problem mapping is **out of scope** of this DoD — that belongs to F-08.

**Touches**:
- Files: `modules/serverless-runtime/serverless-runtime/src/domain/error.rs`

### Module Registers In Example Server

- [ ] `p1` - **ID**: `cpt-cf-serverless-runtime-dod-module-scaffold-register-in-example`

The system **MUST** register the `serverless-runtime` module in `cyberware-example-server`'s composition root so it boots as part of the platform. The registration **MUST** not require any plugin or SDK-trait implementation to be present.

**Implements**:
- `cpt-cf-serverless-runtime-algo-module-scaffold-modkit-registration`

**Touches**:
- Files: `cyberware-example-server` composition root (path per existing ModKit modules' precedent)

### Smoke Test

- [ ] `p1` - **ID**: `cpt-cf-serverless-runtime-dod-module-scaffold-smoke-test`

The system **MUST** include an integration smoke test that boots a test instance of `cyberware-example-server` and asserts the `serverless-runtime` module is registered in ModKit's directory without errors. The test **MUST** run under `cargo test -p serverless-runtime` (or the equivalent workspace test target) and gate the MVP PR.

**Implements**:
- `cpt-cf-serverless-runtime-algo-module-scaffold-smoke-test`

**Touches**:
- Files: `modules/serverless-runtime/serverless-runtime/tests/smoke.rs` (or equivalent)

## 6. Acceptance Criteria

- [ ] `cargo build -p serverless-runtime` succeeds from a clean workspace state.
- [ ] `cargo clippy -p serverless-runtime -- -D warnings` succeeds.
- [ ] `cargo fmt --check` succeeds.
- [ ] The host crate has zero compile-time dependency on any plugin crate (verifiable via `cargo tree -p serverless-runtime --no-default-features`).
- [ ] `cyberware-example-server` boots with the `serverless-runtime` module included and the module appears in ModKit's runtime directory.
- [ ] The smoke test (`cpt-cf-serverless-runtime-dod-module-scaffold-smoke-test`) passes locally and in CI.
- [ ] Layer directories (`api/`, `domain/`, `infra/`) exist with at minimum empty `mod.rs` placeholders.
- [ ] `DomainError` enum stub is present in `src/domain/` and used as the host crate's `Result<_, DomainError>` return alias.
- [ ] No `TODO`, `TBD`, or `FIXME` markers in the scaffold code (acceptable in commit body or PR description but not in committed source).

## 7. Non-Functional Considerations

- **KISS** (per project `CLAUDE.md`): keep the scaffold minimal — no premature abstractions, no anticipated extension points beyond the three layer directories. Subsequent features add what they need when they need it.
- **`cpt-cf-serverless-runtime-nfr-composition-deps`**: This feature is the foundation for the NFR — wiring the host crate into the workspace without coupling it to plugins or downstream modules establishes the composition pattern that later features must preserve.
- **Cross-crate boundary**: The SDK crate (`serverless-runtime-sdk`, docs at `modules/serverless-runtime/serverless-sdk/`) is the only external dependency the scaffold pulls in at the module level. Plugin crates are forbidden at compile time per `cpt-cf-serverless-runtime-adr-thin-host`.
- **Not in scope here** (covered by later features): REST endpoints (F-03), SeaORM entities (F-02/F-03), plugin dispatch (F-04), audit / error mapping (F-08), tenant policy (F-07). The scaffold deliberately ships with none of those.
