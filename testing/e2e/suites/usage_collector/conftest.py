"""E2E fixtures for the usage-collector gear.

This module owns its own server AND its own TimescaleDB container, because the
gear's storage plugin connects and migrates a real TimescaleDB at init and
fails hard when it cannot. It therefore gates on E2E_BINARY and skips when
unset, exactly like mini_chat: `make e2e-local` builds a binary WITHOUT the
usage-collector features, so running there would start a second server and a
container for routes that binary does not serve.

Run it with: make e2e-usage-collector
"""

from __future__ import annotations

import json
import os
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path

import httpx
import pytest

from lib.orchestrator import GearTestEnv
from lib.sidecars import DockerUnavailable, TimescaleDbSidecar, require_docker

# ── Constants ─────────────────────────────────────────────────────────────

HERE = Path(__file__).resolve().parent
PROJECT_ROOT = Path(__file__).resolve().parents[4]
CONFIG = HERE / "config.yaml"
PORT_PLACEHOLDER = "__E2E_TS_PORT__"

SERVER_PORT = 8088
API_BASE = "/usage-collector/v1"
REGISTRY_API_BASE = "/types-registry/v1"
REQUEST_TIMEOUT = 5.0

# Must match this suite's config.yaml.
TENANT_A = "a0000000-0000-4000-8000-00000000000a"
TENANT_B = "a0000000-0000-4000-8000-00000000000b"
TOKEN_A = "e2e-uc-token-tenant-a"
TOKEN_B = "e2e-uc-token-tenant-b"

# The abstract base every meter derives from
# (usage_collector_sdk::USAGE_RECORD_BASE_TYPE). A meter is a derived TYPE of
# it with exactly one further segment, so both ids end in `~`; there is no
# instance id anywhere in this model.
GTS_BASE = "gts.cf.core.uc.usage_record.v1~"

# The published base declaration, posted to types-registry by `make_meter`.
# Read off disk rather than restated here: it is the normative document (it
# carries `x-gts-traits-schema`, which is what makes a meter's traits
# admissible), and a trimmed copy in this file would be a second source of
# truth that drifts.
BASE_SCHEMA_PATH = (
    PROJECT_ROOT / "gears" / "system" / "usage-collector" / "docs" / "schemas"
    / "usage_record.v1.schema.json"
)


def unique_suffix() -> str:
    """Short lowercase-alphanumeric token, legal inside a GTS segment."""
    return uuid.uuid4().hex[:8]


def meter_type_id(suffix: str) -> str:
    """Build a derived meter TYPE id — one segment past the base, `~`-ended.

    Segment grammar is vendor.package.namespace.type.v<major>; `_` is a legal
    namespace. `MeterTypeId::new` rejects a missing terminator and rejects a
    second derivation segment, so both halves of the shape are load-bearing.
    """
    return f"{GTS_BASE}cf.uc_e2e._.usage_{suffix}.v1~"


def covered_period(
    *,
    ends_ago: timedelta = timedelta(minutes=1),
    length: timedelta = timedelta(minutes=5),
) -> tuple[str, str]:
    """An RFC3339 (window_start, window_end) covered period.

    The live route admits an entry whose period ENDS within
    [now - 48h, now + 5min] (`domain/covered_period.rs`), and reads nothing
    else — not `window_start`, not the length, not the arrival instant. The
    default lands the end a minute in the past, clear of both bounds.
    """
    end = datetime.now(timezone.utc) - ends_ago
    start = end - length
    return _rfc3339(start), _rfc3339(end)


def selection_range(hours: int = 72) -> tuple[str, str]:
    """An RFC3339 (from, to) pair for the mandatory read-path range.

    Selection is on `window_end` and is lower-inclusive / upper-exclusive
    (`cpt-cf-usage-collector-adr-window-end-selection`). Both read paths
    REQUIRE the range: `GET /records` takes it as the `from` / `to` query
    parameters, `POST /records/aggregate` as `time_range` in its body. It is
    not a `$filter` conjunct on either — naming a period bound in a predicate
    is a reserved-field 400.

    72 hours back covers the live route's whole 48-hour past tolerance, so a
    test may backdate a period anywhere the live route still accepts it and
    still find its own rows.
    """
    now = datetime.now(timezone.utc)
    return _rfc3339(now - timedelta(hours=hours)), _rfc3339(now + timedelta(hours=1))


def _rfc3339(moment: datetime) -> str:
    """Whole-second RFC3339 with an explicit offset.

    The offset is mandatory on every timestamp this gear parses: both bounds
    deserialize through `time::serde::rfc3339`, and an offset-less stamp would
    attribute usage to whatever offset the server happened to assume.
    """
    return moment.strftime("%Y-%m-%dT%H:%M:%SZ")


# ── Environment gate ──────────────────────────────────────────────────────

# Set by the CI step to forbid the two skips below, mirroring
# `RG_PG_REQUIRE_DOCKER` in ci.yml: "Fail if Docker is unreachable instead of
# letting the suite skip itself into a green step that asserted nothing."
#
# It matters more here than it does there. This suite's step is the only job in
# CI that compiles the TimescaleDB storage plugin into a server, so a silent
# skip is not one lane going quiet - it is the plugin's only end-to-end
# exercise going quiet, in the same green tick.
#
# Off by default, because a developer without Docker should still get a skip
# rather than a failure from a `make e2e-local` sweep. Only the step that owns
# the guarantee sets it.
REQUIRE_DOCKER_ENV = "UC_E2E_REQUIRE_DOCKER"


def _refuse(reason: str) -> None:
    """Fail if the caller demanded this suite run; skip otherwise.

    Not a swap of two callables with one argument list: `pytest.skip` takes
    `allow_module_level` and `pytest.fail` takes `pytrace`, and passing the
    wrong one is a `TypeError` inside a session fixture - which pytest reports
    as an error, not as the refusal it was meant to be.
    """
    if os.environ.get(REQUIRE_DOCKER_ENV):
        pytest.fail(reason, pytrace=False)
    pytest.skip(reason, allow_module_level=True)


@pytest.fixture(scope="session", autouse=True)
def _require_dedicated_binary():
    if not os.environ.get("E2E_BINARY"):
        _refuse("E2E_BINARY not set — run these tests via: make e2e-usage-collector")
    # `require_docker` rather than `skip_without_docker`: the shared helper
    # skips unconditionally, and the choice between skipping and failing is
    # this suite's to make. That leaves the helper with no callers at all -
    # this was its last one - and it is kept rather than deleted because
    # changing shared harness code to suit one suite is the larger move. Its
    # own docstring records that it is uncalled, so nobody has to grep to find
    # out.
    try:
        require_docker()
    except DockerUnavailable as exc:
        _refuse(f"Docker required for this suite: {exc}")


def pytest_collection_modifyitems(items):
    """Stop charging harness startup to the per-test budget in THIS package.

    `pytest_collection_modifyitems` is a global hook: pytest invokes every
    registered conftest's implementation once with the FULL collected item
    list — including tests from every other gear suite under `testing/e2e`.
    This conftest must filter to items under `HERE` (this directory) before
    touching them, or it would change pytest-timeout's behaviour for unrelated
    suites too.

    The problem: pytest-timeout charges FIXTURE SETUP against whichever test
    triggers it. The session-scoped `test_env` starts a TimescaleDB container
    (image pull included) and waits up to 90s for the server to become
    healthy, so the first test to run would blow pytest.ini's 10s budget
    through no fault of its own.

    The fix is `func_only`, NOT a bigger number. `func_only=True` moves
    pytest-timeout's timer from the whole runtest protocol to the call phase
    alone, and passing no timeout value leaves pytest.ini's 10s in force
    (pytest_timeout._get_item_settings falls back to the ini value whenever
    the marker omits one). So every test here keeps the repo-wide 10s hard
    kill on its own body — `docs/toolkit_unified_system/13_e2e_testing.md`
    §3: "If a test exceeds 10s, it is broken, not slow" — while the harness's
    startup cost is simply not counted. Raising the number instead would let
    a genuinely hung test burn the raised budget.

    Session setup keeps its own bounds and its own diagnostics: READY_TIMEOUT
    (60s) and DOCKER_TIMEOUT (120s) in `lib/sidecars.py`, `health_timeout`
    (90s) in the orchestrator. Those report *what* stalled; a pytest-timeout
    kill mid-setup only dumps stacks.

    An explicit @pytest.mark.timeout on a test wins — that is how the
    container lifecycle tests keep their own longer budget, and those do their
    waiting inside the test body where the timer belongs.

    Both sides of the path comparison are `.resolve()`d. pytest canonicalises
    neither `item.fspath` nor a conftest's `__file__`, so comparing them raw
    also works today — but resolving only ONE side silently matches nothing
    when the checkout path contains a symlink, which would drop the marker and
    hand the first test a 10s budget it cannot meet. Symmetry is the invariant
    here; keep both `.resolve()` calls or neither.
    """
    for item in items:
        if HERE not in Path(str(item.fspath)).resolve().parents:
            continue
        if item.get_closest_marker("timeout") is None:
            item.add_marker(pytest.mark.timeout(func_only=True))


# ── Test environment ──────────────────────────────────────────────────────

def _patch_config(config_text: str, env) -> str:
    """Substitute the sidecar's mapped port into the plugin DSN.

    The orchestrator calls this AFTER sidecars start, so the port is known.
    """
    for sidecar in env.sidecars:
        if sidecar.name == "timescaledb":
            return config_text.replace(PORT_PLACEHOLDER, sidecar.dsn_port)
    raise RuntimeError("timescaledb sidecar missing from GearTestEnv.sidecars")


@pytest.fixture(scope="session")
def gear_test_env() -> GearTestEnv:
    return GearTestEnv(
        config_path=CONFIG,
        config_patch=_patch_config,
        port=SERVER_PORT,
        health_path="/healthz",
        health_timeout=90,
        env={"RUST_LOG": os.environ.get(
            "RUST_LOG",
            "info,cf_gears_usage_collector=debug,"
            "cf_gears_timescaledb_usage_collector_plugin=debug",
        )},
        sidecars=[TimescaleDbSidecar()],
        log_suffix="usage-collector",
    )


# ── HTTP helpers ──────────────────────────────────────────────────────────

@pytest.fixture
def api(test_env):
    """Async client factory bound to the running server, per tenant token."""
    def _client(token: str = TOKEN_A) -> httpx.AsyncClient:
        return httpx.AsyncClient(
            base_url=f"{test_env.base_url}{API_BASE}",
            headers={"Authorization": f"Bearer {token}"},
            timeout=REQUEST_TIMEOUT,
        )
    return _client


@pytest.fixture
def registry_api(test_env):
    """Async client factory bound to the types-registry gear's v1 surface.

    A second base URL rather than a second server: meters are declarations
    owned by `types-registry`, and usage-collector resolves them through it
    (`infra/types_registry_source.rs`). v1 deliberately — `get_type_schema`,
    the method usage-collector calls, reads the in-memory v1 repository, so a
    v2 submission would register successfully and still be invisible here.
    """
    def _client(token: str = TOKEN_A) -> httpx.AsyncClient:
        return httpx.AsyncClient(
            base_url=f"{test_env.base_url}{REGISTRY_API_BASE}",
            headers={"Authorization": f"Bearer {token}"},
            timeout=REQUEST_TIMEOUT,
        )
    return _client


async def _register_entities(registry_api, entities: list[dict]) -> None:
    """POST GTS documents to types-registry and assert every one landed.

    The batch answers 200 with a per-item `results` list even when items
    failed, so the summary is what has to be asserted: a check on the status
    code alone would pass while nothing was registered.
    """
    async with registry_api() as client:
        resp = await client.post("/entities", json={"entities": entities})
    assert resp.status_code == 200, f"registration failed: {resp.status_code} {resp.text}"
    body = resp.json()
    assert body["summary"]["failed"] == 0, f"registration reported failures: {body}"
    assert body["summary"]["succeeded"] == len(entities), body


@pytest.fixture
def make_meter(registry_api):
    """Declare a meter through the GTS registry and return its type id.

    The replacement for the deleted `/usage-types` catalog: a meter is a
    derived GTS TYPE, and its metering semantics live in `x-gts-traits` on
    that type. `aggregation_fold` is what the aggregate path resolves - the
    request carries no fold - and the closed `metadata` surface is what
    decides which keys an entry may carry and which dimensions are groupable
    and filterable.

    A fresh meter id per call, which is what keeps two tests' rows apart: a
    read path is scoped by `gts_type_id`, so distinct meters cannot see each
    other's entries however much their covered periods overlap.

    The base type is posted alongside every meter rather than once in a
    session fixture. Nothing seeds it — usage-collector declares no
    `#[gts_type_schema]` for `gts.cf.core.uc.usage_record.v1~`, and no shipped
    config registers it — and types-registry refuses a child whose parent is
    unknown, so it has to be registered before the first meter. Re-posting it
    is free: an identical document resolves to the stored one and returns ok
    (`in_memory_repo.rs`, `existing.content == *entity`).

    `x-gts-traits` sits at the TOP LEVEL of the meter, not inside `allOf`:
    `extract_traits` reads it there and nowhere else. The closed `metadata`
    subschema is the opposite — either placement is collected, and it is in
    the `allOf` branch here to match the published example meter.
    """
    base_document = json.loads(BASE_SCHEMA_PATH.read_text())

    async def _create(
        fold: str = "SUM",
        canonical_unit: str = "byte-hours",
        metadata_keys: tuple[str, ...] = ("region",),
    ) -> str:
        meter_id = meter_type_id(unique_suffix())
        meter_document = {
            "$id": f"gts://{meter_id}",
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": f"E2E meter {meter_id}",
            "x-gts-traits": {
                "aggregation_fold": fold,
                "canonical_unit": canonical_unit,
                # Far past anything this suite writes, so no retention
                # arithmetic can interact with a test's own rows.
                "retention": "P400D",
            },
            "x-gts-final": True,
            "allOf": [
                {"$ref": f"gts://{GTS_BASE}"},
                {
                    "properties": {
                        "metadata": {
                            "type": "object",
                            "additionalProperties": False,
                            "properties": {
                                key: {"type": "string"} for key in metadata_keys
                            },
                        }
                    }
                },
            ],
        }
        await _register_entities(registry_api, [base_document])
        await _register_entities(registry_api, [meter_document])
        return meter_id
    return _create


def record_payload(
    gts_type_id: str,
    *,
    tenant_id: str = TENANT_A,
    value: str = "1",
    resource_id: str = "res-1",
    idempotency_key: str | None = None,
    period: tuple[str, str] | None = None,
    metadata: dict[str, str] | None = None,
    invalidates: str | None = None,
    reason_code: str | None = None,
) -> dict:
    """One CreateUsageRecordRequest. `value` is a STRING on the wire.

    `invalidates` / `reason_code` are both-or-neither and are what make a
    submission a withdrawal: there is no caller-supplied discriminator, so a
    marker cannot disagree with the payload it marks. Both are omitted
    entirely when unset — the shape is `deny_unknown_fields` and a `null`
    would be a different thing from an absent key.
    """
    window_start, window_end = period or covered_period()
    payload = {
        "gts_type_id": gts_type_id,
        "tenant_id": tenant_id,
        "resource_ref": {"resource_id": resource_id, "resource_type": "compute.vm"},
        "value": value,
        "idempotency_key": idempotency_key or f"e2e-idem-{unique_suffix()}",
        "window_start": window_start,
        "window_end": window_end,
        "metadata": metadata or {},
    }
    if invalidates is not None:
        payload["invalidates"] = invalidates
        payload["reason_code"] = reason_code or "E2E_CORRECTION"
    return payload


def rejected_records(response: httpx.Response) -> list[dict]:
    """Assert every per-record outcome is `rejected`, return the Problem bodies.

    The batch path answers 207 when any entry was refused, and the refusal is
    the per-record `error`, not the status line. Returning the Problems keeps
    a caller from having to know the envelope to assert on the reason.
    """
    assert response.status_code == 207, (
        f"expected 207, got {response.status_code}: {response.text}"
    )
    results = response.json()["results"]
    # `!= "rejected"`, not `== "accepted"`: two wire tags exist today, and the
    # fail-closed spelling is the one that would also catch a third.
    unrejected = [r for r in results if r["outcome"] != "rejected"]
    assert not unrejected, f"records not rejected: {unrejected}"
    return [r["error"] for r in results]


def accepted_records(response: httpx.Response) -> list[dict]:
    """Assert every per-record outcome is `accepted`, return the record bodies.

    POST /records is a BATCH endpoint: it answers 200 when every entry was
    admitted and 207 when any was rejected, with the real outcome per record.
    Asserting only on the status code would pass while every record was
    rejected.

    `CreateUsageRecordResultDto` has exactly two tags, `accepted` and
    `rejected`. A deduplicated resubmission is an `accepted` carrying the
    already-persisted row - it is not a third outcome, and nothing on the wire
    distinguishes it from a first submission.
    """
    assert response.status_code == 200, f"expected 200, got {response.status_code}: {response.text}"
    results = response.json()["results"]
    # Same fail-closed spelling as `rejected_records`, and the same reason.
    unaccepted = [r for r in results if r["outcome"] != "accepted"]
    assert not unaccepted, f"records not accepted: {unaccepted}"
    return [r["record"] for r in results]
