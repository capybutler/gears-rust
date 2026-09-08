"""Integration-seam E2E tests for the usage-collector gear.

Each test targets exactly one seam that only manifests over real HTTP against
real TimescaleDB. Storage SQL, aggregation internals, domain validation and
DTO conversion are covered by unit tests and by
`make test-usage-collector-pg` — not here.

QUARANTINED — this module does not run. See `pytestmark` below.
"""

from datetime import datetime
from decimal import Decimal

import pytest

from .conftest import accepted_records, record_payload, window_filter

# ── Quarantine ────────────────────────────────────────────────────────────
# Every test here is skipped, for two reasons that are worth keeping apart.
#
# 1. PRE-EXISTING, and it already covered the whole module. Each test takes
#    the `make_usage_type` fixture, which POSTs `/usage-types`. The usage-type
#    catalog and its routes were deleted before the correction-model slice —
#    `src/api/rest/routes/` has had no usage-types module since — so every
#    test in this file has been failing at setup independently of anything
#    below. `test_list_usage_types_includes_created`, `test_get_usage_type`
#    and `test_delete_usage_type_referenced_by_record_is_rejected` are that
#    breakage and nothing else: they test the deleted catalog directly.
#
#    The catalog is not the only pre-existing cause, and a rewrite owes the
#    others too. `test_ingest_and_read_record_roundtrip` also asserts
#    `body["gts_id"]` and `body["created_at"]`; the time-model slice before
#    this one retired both — the type reference is `gts_type_id` and the
#    record carries a `window_start` / `window_end` covered period instead
#    of a `created_at` instant. `record_payload` in `conftest.py` submits
#    the same two retired keys, and `window_filter()` there builds a
#    `created_at ge … and created_at lt …` `$filter` that every read test
#    passes — a shape the gear now rejects outright, because the covered
#    period is a typed `from` / `to` parameter and a predicate naming a
#    period bound is a reserved-field `400`.
#
# 2. ADDED BY THE CORRECTION-MODEL SLICE, which replaced the mutate-in-place
#    correction model (a `status` latch plus `POST /records/{id}/deactivate`)
#    with an append-only one: a correction is an ordinary ingested entry
#    carrying `invalidates` + `reason_code`, and `entry_type` is derived from
#    `invalidates` rather than stored. Two tests pin vocabulary that slice
#    deleted, and each carries a note in place below:
#      - `test_ingest_and_read_record_roundtrip` asserts
#        `body["status"] == "active"`; `UsageRecord` carries no `status`, and
#        the response DTO now projects `entry_type`.
#      - `test_deactivate_is_monotonic` asserts the route (deleted), the
#        `inactive` field value (deleted) and the `ALREADY_INACTIVE` reason
#        code (deleted). Its whole premise — that a correction flips a latch
#        on the target — is the model that was replaced.
#
# Rewriting is deliberately NOT done here: it needs a running deployment plus
# a TimescaleDB plugin on the append-only model to verify against, and the
# plugin under `plugins/timescaledb-usage-collector-plugin/` is still on the
# pre-slice-2 model. A rewrite owes: a replacement for `make_usage_type` that
# declares a meter through the GTS registry instead of the deleted catalog;
# `status`/`entry_type` repointed; and `test_deactivate_is_monotonic` replaced
# by an invalidation test that submits a faithful copy carrying `invalidates`
# and asserts the second attempt is `409 ALREADY_INVALIDATED` — which is now
# the storage plugin's atomic obligation, not the gateway's.
pytestmark = pytest.mark.skip(
    reason="quarantined: usage-type catalog endpoints deleted (pre-existing), and "
    "the deactivation surface plus the `status` field were deleted by the "
    "append-only correction-model slice; needs a running deployment to rewrite"
)


# Quarantine reason 2 (see the module header): `status` no longer exists on
# the record or on the wire. The seam this test guards — a `Decimal` value
# crossing as a string and a timestamptz round-trip — is still worth having;
# only the `status` assertion needs repointing, to the derived `entry_type`.
async def test_ingest_and_read_record_roundtrip(api, make_usage_type):
    """Seam: handler <-> JSON wire format <-> PostgreSQL round-trip.

    A Decimal `value` crosses as a string and comes back byte-identical, and
    an RFC3339 `created_at` survives the timestamptz round-trip.
    """
    gts_id, _ = await make_usage_type()
    payload = record_payload(gts_id, value="42.5")

    async with api() as client:
        created = accepted_records(
            await client.post("/records", json={"records": [payload]})
        )
        assert len(created) == 1
        record_id = created[0]["id"]

        fetched = await client.get(f"/records/{record_id}")

    assert fetched.status_code == 200, fetched.text
    body = fetched.json()
    assert body["id"] == record_id
    assert body["value"] == "42.5"
    assert body["gts_id"] == gts_id
    assert body["tenant_id"] == payload["tenant_id"]
    assert body["status"] == "active"
    assert body["resource_ref"]["resource_id"] == "res-1"
    assert datetime.fromisoformat(body["created_at"].replace("Z", "+00:00")) == \
        datetime.fromisoformat(payload["created_at"].replace("Z", "+00:00"))


async def test_list_records_odata_filter_and_cursor(api, make_usage_type):
    """Seam: OData $filter -> SQL, and the keyset cursor codec over HTTP.

    Following a cursor resends the IDENTICAL gts_id/$filter/limit — the
    toolkit rejects a cursor replayed under a different filter or order.
    `limit` is the page-size parameter: the toolkit's OData extractor
    (`ODataParams` in libs/toolkit/src/api/odata.rs) is what binds it to
    `ODataQuery.limit`.
    """
    gts_id, _ = await make_usage_type()
    async with api() as client:
        created = accepted_records(await client.post("/records", json={"records": [
            record_payload(gts_id, resource_id="res-a", value="1"),
            record_payload(gts_id, resource_id="res-b", value="2"),
        ]}))
        assert len(created) == 2
        all_ids = {r["id"] for r in created}

        params = {"gts_id": gts_id, "$filter": window_filter(), "limit": 1}

        first = await client.get("/records", params=params)
        assert first.status_code == 200, first.text
        page_one = first.json()
        assert len(page_one["items"]) == 1
        cursor = page_one["page_info"]["next_cursor"]
        assert cursor, "a second page exists, so next_cursor must be set"

        second = await client.get("/records", params={**params, "cursor": cursor})

    assert second.status_code == 200, second.text
    page_two = second.json()
    assert len(page_two["items"]) == 1

    seen = {page_one["items"][0]["id"], page_two["items"][0]["id"]}
    assert seen == all_ids, "the two pages must be disjoint and cover both records"


async def test_aggregate_groups_by_resource(api, make_usage_type):
    """Seam: aggregation request -> SQL GROUP BY -> PDP-scoped result.

    `sum` requires a counter usage type (it is a 400 on a gauge). group_by
    carries closed dimensions as bare snake_case strings.
    """
    gts_id, _ = await make_usage_type(kind="counter")
    async with api() as client:
        accepted_records(await client.post("/records", json={"records": [
            record_payload(gts_id, resource_id="res-a", value="10"),
            record_payload(gts_id, resource_id="res-a", value="5"),
            record_payload(gts_id, resource_id="res-b", value="7"),
        ]}))

        resp = await client.post(
            "/records/aggregate",
            params={"gts_id": gts_id, "$filter": window_filter()},
            json={"op": "sum", "group_by": ["resource_id"]},
        )

    assert resp.status_code == 200, resp.text
    buckets = resp.json()["buckets"]
    # `value` may arrive as a JSON string or number; Decimal(str(...)) accepts both.
    sums = {b["key"][0]: Decimal(str(b["value"])) for b in buckets}
    assert sums == {"res-a": Decimal("15"), "res-b": Decimal("7")}


# Quarantine reason 2 (see the module header): this test has no successor
# assertion to repoint to. Deactivation was not renamed — the whole
# mutate-the-target mechanism it describes was replaced by appending a second
# entry, so the route, the `inactive` value and `ALREADY_INACTIVE` are all
# gone. A rewrite is a new test, not an edit of this one.
async def test_deactivate_is_monotonic(api, make_usage_type):
    """Seam: deactivation is one-way, enforced at the storage transaction.

    First call flips active -> inactive (204). The second is REJECTED with 409
    `ALREADY_INACTIVE` — the store reads the row FOR UPDATE and refuses an
    already-inactive target. It is not an idempotent no-op (ADR-0005).
    """
    gts_id, _ = await make_usage_type()
    async with api() as client:
        created = accepted_records(
            await client.post("/records", json={"records": [record_payload(gts_id)]})
        )
        record_id = created[0]["id"]

        first = await client.post(f"/records/{record_id}/deactivate")
        assert first.status_code == 204, first.text

        after = await client.get(f"/records/{record_id}")
        assert after.status_code == 200, after.text
        assert after.json()["status"] == "inactive"

        second = await client.post(f"/records/{record_id}/deactivate")

    assert second.status_code == 409, f"expected 409, got {second.status_code}: {second.text}"
    assert second.headers["content-type"].startswith("application/problem+json")
    problem = second.json()
    assert problem["status"] == 409
    assert problem["context"]["reason"] == "ALREADY_INACTIVE"


async def test_list_usage_types_includes_created(api, make_usage_type):
    """Seam: catalog list projects plugin-owned rows onto the wire.

    `limit` is the page-size parameter here too: this endpoint shares the
    toolkit `OData` extractor with `/records`. A large `limit` keeps this
    assertion stable as the session accumulates usage types across tests;
    1000 is the documented ceiling and the plugin's clamp.
    """
    gts_id, metadata_key = await make_usage_type()
    async with api() as client:
        resp = await client.get("/usage-types", params={"limit": 1000})

    assert resp.status_code == 200, resp.text
    by_id = {item["gts_id"]: item for item in resp.json()["items"]}
    assert gts_id in by_id, f"{gts_id} missing from the catalog listing"
    assert by_id[gts_id]["kind"] == "counter"
    assert by_id[gts_id]["metadata_fields"] == [metadata_key]


async def test_get_usage_type(api, make_usage_type):
    """Seam: single catalog read by GTS id in the path.

    The id contains `~` and `.`; this also proves the path segment survives
    routing without mangling.
    """
    gts_id, metadata_key = await make_usage_type()
    async with api() as client:
        resp = await client.get(f"/usage-types/{gts_id}")

    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["gts_id"] == gts_id
    assert body["kind"] == "counter"
    assert body["metadata_fields"] == [metadata_key]


async def test_delete_usage_type_referenced_by_record_is_rejected(api, make_usage_type):
    """Seam: real PostgreSQL FK ON DELETE RESTRICT surfaced as HTTP 409.

    This is the canonical PostgreSQL-only seam: SQLite's default FK behaviour
    differs, and no unit test can observe the constraint. An unreferenced type
    deletes cleanly (204), a referenced one is refused (409).
    """
    referenced_id, _ = await make_usage_type()
    unreferenced_id, _ = await make_usage_type()

    async with api() as client:
        accepted_records(await client.post(
            "/records", json={"records": [record_payload(referenced_id)]}
        ))

        blocked = await client.delete(f"/usage-types/{referenced_id}")
        allowed = await client.delete(f"/usage-types/{unreferenced_id}")

    assert blocked.status_code == 409, (
        f"a referenced usage type must not be deletable, got "
        f"{blocked.status_code}: {blocked.text}"
    )
    assert blocked.headers["content-type"].startswith("application/problem+json")
    # Reason code from ConflictReason::UsageTypeReferenced (usage-collector-sdk/src/reason.rs),
    # constructed by UsageCollectorError::usage_type_referenced (usage-collector-sdk/src/error.rs)
    # and lifted onto `context.reason` in usage-collector/src/infra/sdk_error_mapping.rs. Asserting
    # on it (not just the status code) keeps this test tied to the real FK RESTRICT seam: an
    # application-level pre-check returning a bare 409 for a different reason would stay green
    # on `status_code == 409` alone.
    assert blocked.json()["context"]["reason"] == "USAGE_TYPE_REFERENCED"
    assert allowed.status_code == 204, allowed.text
