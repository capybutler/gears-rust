"""E2E fixtures for the usage-collector gear.

This module owns its own server AND its own TimescaleDB container, because the
gear's storage plugin connects and migrates a real TimescaleDB at init and
fails hard when it cannot. That is what makes the suite `launcher: pytest`
(`e2e.yaml`), and the unscoped `make e2e-local` therefore never collects it:
`run_e2e.py::discover_launcher_test_paths` hands pytest the paths of
`launcher: e2e-launcher` suites only.

The E2E_BINARY gate below catches a direct pytest invocation that went round
`run_e2e.py`. It is needed even though `config/e2e-features.txt` DOES list
`usage-collector`: what that shared binary omits is
`timescaledb-usage-collector`, so it would serve these routes off the noop
backend and assert nothing about storage.

Run it with: make e2e-usage-collector
"""

from __future__ import annotations

import os
import uuid
from copy import deepcopy
from datetime import datetime, timedelta, timezone
from pathlib import Path

import httpx
import pytest
import yaml

from lib.orchestrator import GearTestEnv
from lib.sidecars import DockerUnavailable, TimescaleDbSidecar, require_docker

# ── Constants ─────────────────────────────────────────────────────────────

HERE = Path(__file__).resolve().parent
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

# This suite does NOT carry a copy of, or a path to, the base declaration.
# The gear registers it at link time from its own `#[gts_type_schema]`
# (`usage-collector/src/gts/usage_record.rs`), so the document types-registry
# serves for this id is the macro's emission. Posting the checked-in
# `docs/schemas/usage_record.v1.schema.json` beside it would be a second
# source of truth, and types-registry rejects a non-identical re-post of an
# existing id — see `make_meter`.


# ── Values read back out of config.yaml ───────────────────────────────────

# Parsed once, at import. Read back rather than restated, so a rename in
# config.yaml cannot leave the fixtures below operating on a path nothing
# writes or a cache ceiling nothing enforces — the two failures that would be
# silent, since both would still produce a suite that passes while asserting
# less than it claims.
#
# `yaml` is a DECLARED dependency of this lane, which is the whole of what
# makes this import safe: `testing/e2e/requirements.txt` lists PyYAML (as does
# `testing/requirements.txt`), and `make py-env` installs both into the one
# `.venv` that every e2e target then runs pytest under — `tools/scripts/run_e2e.py`
# already imports yaml at module level under that same interpreter.
#
# Note what does NOT make it safe, because the obvious answer is wrong:
# `_require_dedicated_binary` below guards nothing here. It is an autouse
# FIXTURE, so it runs after collection, while this import runs DURING it — an
# interpreter without PyYAML fails on this line before any fixture, skip gate
# or E2E_BINARY check is ever consulted. A runtime gate cannot stand in for a
# declared dependency; the requirements files are the guarantee.
_CONFIG = yaml.safe_load(CONFIG.read_text())

# The declaration mirror's SQLite file (DESIGN §3.7). `toolkit-db` puts a
# gear's SQLite file at `<server.home_dir>/<gear name>/<database.file>` —
# `DbManager::build_for_gear` joins the gear name onto `home_dir` and hands
# that to `finalize_sqlite_paths` (`libs/toolkit-db/src/manager.rs`) — so the
# path is assembled here from the same three pieces rather than written out
# as one literal. It is NOT under the repo: a repo-rooted search for it
# returns nothing.
MIRROR_DB = (
    Path(_CONFIG["server"]["home_dir"]).expanduser()
    / "usage-collector"
    / _CONFIG["gears"]["usage-collector"]["database"]["file"]
)

# The declaration cache's ceiling, as this suite configures it. Resolving this
# many OTHER meters is what guarantees a given meter has been evicted, whatever
# the cache already held: `TypeResolver::store` evicts the least recently
# resolved entry whenever a NEW key arrives at a full map, so after this many
# fresh keys the map holds those keys and nothing else.
TYPE_CACHE_CAPACITY = _CONFIG["gears"]["usage-collector"]["config"]["type_cache_capacity"]


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
    kill on its own body — `docs/toolkit_unified_system/13_e2e_testing.md`,
    "Anti-Flaking Practices" §3: "If a test exceeds 10s, it is broken, not
    slow" — while the harness's startup cost is simply not counted. Raising
    the number instead would let a genuinely hung test burn the raised budget.

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

# SQLite's two sidecar files for a WAL database (`WAL: "true"` on the
# `sqlite_uc_e2e` server in config.yaml). Most of the mirror's live rows sit
# in the `-wal` rather than the database file between checkpoints — measured
# here at 910 KB of WAL beside a 40 KB database — so they are removed with it.
#
# Hygiene rather than correctness, and stated that way because the stronger
# claim is false: removing only the database and leaving these two behind was
# measured, and SQLite does NOT replay the orphaned WAL into the database it
# then creates at that path (the old table is simply not there). What removing
# them buys is that "the mirror is gone" stays true of everything on disk,
# instead of leaving two files whose names advertise state that no longer
# exists.
_SQLITE_WAL_SIDECARS = ("-wal", "-shm")


@pytest.fixture(scope="session")
def fresh_declaration_mirror():
    """Owns the declaration mirror file's lifecycle for this suite.

    Nothing else does. The gear creates the file at startup (its `database:`
    block in config.yaml, migrated at init) and never prunes it, the
    orchestrator only knows about the server process and the sidecar
    container, and the file lives under `~/.cf-gears`, outside the repo —
    so before this fixture existed it simply accumulated. Measured on this
    machine before the change: 120 rows whose `first_seen_at` spanned 18
    hours, several whole suite runs deep.

    **The removal is on SETUP, before the server boots — deliberately not a
    teardown.** A teardown covers the paths where it is asked to run: a
    passing run, and a failing one. It covers neither a `SIGKILL` nor a
    harness crash, and those are exactly the runs that leave the mess. A
    setup-side removal covers every one of them uniformly, because it does
    not care how the previous run ended, and it is what the invariant the
    tests actually need is stated against: **the server always boots onto an
    empty mirror**, so a meter id seen twice in the table is two resolutions
    within THIS run rather than two runs meeting.

    What it deliberately does not do is remove the file afterwards. The file
    a failing run leaves behind is the evidence for diagnosing it — a
    teardown would delete exactly the rows an investigator wants — and the
    next run removes it regardless. The cost is one run's worth of rows
    (about a dozen) resident between runs, which is bounded; what this
    replaces was unbounded.

    Requested by `gear_test_env` rather than marked `autouse`, so the
    ordering is a data dependency instead of a convention: pytest builds
    `gear_test_env` before `test_env`'s body starts the server, so the
    removal cannot race the process that recreates the file.
    """
    sidecars = (MIRROR_DB.with_name(MIRROR_DB.name + s) for s in _SQLITE_WAL_SIDECARS)
    for path in (MIRROR_DB, *sidecars):
        path.unlink(missing_ok=True)
    yield MIRROR_DB


def _patch_config(config_text: str, env) -> str:
    """Substitute the sidecar's mapped port into the plugin DSN.

    The orchestrator calls this AFTER sidecars start, so the port is known.
    """
    for sidecar in env.sidecars:
        if sidecar.name == "timescaledb":
            return config_text.replace(PORT_PLACEHOLDER, sidecar.dsn_port)
    raise RuntimeError("timescaledb sidecar missing from GearTestEnv.sidecars")


@pytest.fixture(scope="session")
def gear_test_env(fresh_declaration_mirror) -> GearTestEnv:
    # `fresh_declaration_mirror` is requested for its ORDERING, not its value:
    # it empties the mirror file, and depending on it here is what puts that
    # removal before `test_env` starts the server that recreates it. Dropping
    # the argument as unused would silently put the two back in any order.
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


def meter_document(
    meter_id: str,
    fold: str = "SUM",
    canonical_unit: str = "byte-hours",
    metadata_keys: tuple[str, ...] = ("region",),
) -> dict:
    """The GTS document `make_meter` posts for `meter_id`.

    A module-level function rather than a literal inside the fixture, so a
    test can rebuild the exact document that was declared and compare
    something against it by equality — `test_declaration_mirror.py` does,
    against the copy the gear mirrored. The fixture still returns only the
    id, so every call site that predates this change is untouched.
    """
    return {
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
                        "properties": {key: {"type": "string"} for key in metadata_keys},
                    }
                }
            },
        ],
    }


@pytest.fixture(scope="session")
def declared_meters() -> set[str]:
    """Every meter id `make_meter` has minted so far in THIS session.

    Session-scoped and accumulating, which is what makes it a usable bound on
    the declaration mirror's contents: the mirror is written only for a meter
    the gear actually resolved, and every resolvable meter in this suite comes
    from `make_meter`, so the table's keys must be a subset of this set at any
    point in the run. `test_declaration_mirror.py` asserts exactly that, and
    it is the assertion that would catch `fresh_declaration_mirror` failing to
    do its job — a row from an earlier run names an id this session never
    minted.
    """
    return set()


class MeterBudget:
    """How many meters one test may declare, and how many it did.

    The `type_cache_capacity: 2` in config.yaml is safe only because of an
    invariant nothing used to state: **a test declares one meter.** That is
    what keeps a two-entry declaration cache from changing what the suite
    measures — one meter per test means a test can never evict its OWN meter
    and silently re-resolve it, which would rewrite that meter's mirror row
    and turn a first insert into an update. The invariant held when it was
    read off all sixteen tests by hand; this makes it hold by failing.

    Deliberately a budget that can be RAISED rather than a flat ban, because
    one test must exceed it: driving the mirror's `ON CONFLICT DO UPDATE` arm
    is exactly "declare enough meters to evict your own". That test says so
    with [`allow_more`], in the test body, next to the loop that spends it —
    so exceeding the budget stays a thing a test states out loud rather than
    a thing it does by accident.
    """

    def __init__(self) -> None:
        self.allowed = 1
        self.minted: list[str] = []

    def allow_more(self, extra: int) -> None:
        """Raise this test's budget by `extra` meters."""
        self.allowed += extra


@pytest.fixture
def meter_budget() -> MeterBudget:
    """Enforces [`MeterBudget`] at the end of each test that declares a meter.

    The check runs in teardown rather than inside `make_meter`, so the failure
    names the whole of what the test did (five declared against one budgeted)
    instead of refusing the second call before the test's own assertions have
    had their say. A teardown failure is reported as an ERROR on that test;
    the rest of the suite is unaffected, which is right — the thing that broke
    is one test's relationship with the cache ceiling, not the gear.
    """
    budget = MeterBudget()
    yield budget
    assert len(budget.minted) <= budget.allowed, (
        f"this test declared {len(budget.minted)} meters but budgeted "
        f"{budget.allowed}: {sorted(budget.minted)}.\n"
        "This suite runs the gear with a deliberately tiny declaration cache "
        f"(type_cache_capacity={TYPE_CACHE_CAPACITY} in config.yaml), so a test "
        "that declares more meters than that can evict its own earlier meter "
        "and re-resolve it — which rewrites that meter's mirror row and makes "
        "a first insert indistinguishable from an update. If the extra meters "
        "are the point, say so with `meter_budget.allow_more(n)`; otherwise "
        "use one meter."
    )


@pytest.fixture
def make_meter(registry_api, declared_meters, meter_budget):
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

    The base type is NOT posted here. The gear seeds it through the
    process-wide `toolkit-gts` link-time inventory: it declares
    `#[gts_type_schema(type_id = gts_id!("cf.core.uc.usage_record.v1~"),
    base = true, gts_abstract = true)]`
    (`usage-collector/src/gts/usage_record.rs`), so
    `gts.cf.core.uc.usage_record.v1~` is registered before this suite's
    server finishes starting and the parent of every meter below is
    guaranteed. types-registry refuses a child whose parent is unknown, and
    that is satisfied by the gear, not by this fixture.

    Posting it here is not merely redundant, it FAILS: types-registry's
    re-post path admits an existing id only when the submitted document is
    byte-identical to the stored one (`in_memory_repo.rs`,
    `existing.content == *entity`), and the macro's emission is not
    byte-identical to the hand-authored contract document in
    `docs/schemas/` — `Option<String>` emits `"type": ["string", "null"]`
    where the contract writes `"string"`. An earlier revision of this
    fixture posted that file, and **every test that requested this fixture
    failed** with
    `Entity already exists: gts.cf.core.uc.usage_record.v1~` (ruling G31) —
    exactly the tests that request it and no others, which is why the
    `test_sidecar_contract.py` pair went on passing: they take no
    `make_meter` and post nothing.
    Forcing the two documents to agree byte-for-byte would couple a macro's
    output format to a checked-in file and break on any `schemars` change.

    `x-gts-traits` sits at the TOP LEVEL of the meter, not inside `allOf`:
    `extract_traits` reads it there and nowhere else. The closed `metadata`
    subschema is the opposite — either placement is collected, and it is in
    the `allOf` branch here to match the published example meter.
    """
    async def _create(
        fold: str = "SUM",
        canonical_unit: str = "byte-hours",
        metadata_keys: tuple[str, ...] = ("region",),
    ) -> str:
        meter_id = meter_type_id(unique_suffix())
        await _register_entities(
            registry_api,
            [meter_document(meter_id, fold, canonical_unit, metadata_keys)],
        )
        declared_meters.add(meter_id)
        meter_budget.minted.append(meter_id)
        return meter_id
    return _create


def record_payload(
    gts_type_id: str,
    *,
    tenant_id: str = TENANT_A,
    quantity: str = "1",
    resource_id: str = "res-1",
    idempotency_key: str | None = None,
    period: tuple[str, str] | None = None,
    metadata: dict[str, str] | None = None,
) -> dict:
    """One `record` CreateUsageRecordRequest. `quantity` is a STRING on the wire.

    `entry_type` is the required, caller-supplied discriminator between the
    request schema's two branches, and it has no default
    (`usage-collector-v1.yaml` `EntryType`): a submission that omits it is
    refused rather than read as a `record`. This factory only builds
    measurements; the withdrawal of one is `withdrawal_of(payload, ...)`,
    which copies the measurement rather than rebuilding it.

    `idempotency_key` is caller-supplied and required on BOTH entry kinds,
    and it is one of the six components of the dedup identity, so a fresh
    random one per call is what keeps two tests' submissions from
    deduplicating onto each other. A test that needs two submissions to be
    one entry pins both the key and the covered period.
    """
    window_start, window_end = period or covered_period()
    return {
        "entry_type": "record",
        "gts_type_id": gts_type_id,
        "tenant_id": tenant_id,
        "resource_ref": {"resource_id": resource_id, "resource_type": "compute.vm"},
        "quantity": quantity,
        "window_start": window_start,
        "window_end": window_end,
        "metadata": metadata or {},
        "idempotency_key": idempotency_key or f"e2e-idem-{unique_suffix()}",
    }


def withdrawal_of(target: dict, *, reason_code: str) -> dict:
    """The invalidation that withdraws the record `target` submitted.

    A copy of the submitted payload rather than a second construction of one,
    because that is literally what the contract asks for: an invalidation
    repeats its target's tenant, GTS type, resource, subject, covered period,
    quantity, metadata and idempotency key, and departs in exactly two
    caller-supplied fields, `entry_type` and `reason_code` (DESIGN.md §3.1,
    "Faithful copy"). A rebuilt copy that drifted in any compared field would
    be refused naming that field, so a test about something else would fail
    for a reason it never meant to exercise. `reason_code` has no default
    here for the same reason `entry_type` has none on the wire: the two are
    the whole of what a withdrawal states, and neither should be guessable.

    A deep copy, so the two payloads share no nested `resource_ref` or
    `metadata` object and a caller may edit either in place.

    The target is NOT named. `invalidates` is server-derived and a submitted
    one is refused as an unknown field; the gateway derives the target's `id`
    from this submission's own identity inputs with `entry_type` set to
    `record`, looks it up converged-only, and stamps what it resolved
    (DESIGN.md §3.1, "Target resolution"). Repeating the target's key is
    therefore not a formality — it is how the target is found.
    """
    payload = deepcopy(target)
    payload["entry_type"] = "invalidation"
    payload["reason_code"] = reason_code
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
