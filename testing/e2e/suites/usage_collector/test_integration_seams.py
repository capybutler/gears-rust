"""Integration-seam E2E tests for the usage-collector gear.

Each test targets exactly one seam that only manifests over real HTTP against
real TimescaleDB. Storage SQL, aggregation internals, domain validation and
DTO conversion are covered by unit tests and by
`make test-usage-collector-pg` — not here.

What is here that the plugin's own pg lane cannot reach: the meter comes from
`types-registry` over the wire rather than from a fixture, the covered-period
range crosses as query parameters or a request body rather than as a typed
struct, and the decimal quantity crosses as a JSON string.
"""

from datetime import datetime, timedelta
from decimal import Decimal

from .conftest import (
    GTS_BASE,
    accepted_records,
    covered_period,
    record_payload,
    rejected_records,
    selection_range,
    unique_suffix,
)


async def test_ingest_and_read_record_roundtrip(api, make_meter):
    """Seam: handler <-> JSON wire format <-> PostgreSQL round-trip.

    A Decimal `value` crosses as a string and comes back byte-identical, both
    covered-period bounds survive the timestamptz round-trip, and the two
    server-derived projections arrive: `origin` names the route that admitted
    the entry and `entry_type` is derived from the absent `invalidates`
    rather than stored.
    """
    meter_id = await make_meter()
    period = covered_period()
    payload = record_payload(meter_id, value="42.5", period=period)

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
    assert body["gts_type_id"] == meter_id
    assert body["tenant_id"] == payload["tenant_id"]
    assert body["resource_ref"]["resource_id"] == "res-1"
    assert body["entry_type"] == "record"
    assert body["origin"] == "live"
    for bound, submitted in zip(("window_start", "window_end"), period):
        assert datetime.fromisoformat(body[bound].replace("Z", "+00:00")) == \
            datetime.fromisoformat(submitted.replace("Z", "+00:00")), bound


async def test_list_records_range_filter_and_cursor(api, make_meter):
    """Seam: the covered-period range -> SQL, and the keyset cursor over HTTP.

    Following a cursor resends the IDENTICAL gts_type_id/range/limit — the
    toolkit rejects a cursor replayed under a different filter or order.
    `limit` is the page-size parameter: the toolkit's OData extractor
    (`ODataParams` in libs/toolkit/src/api/odata.rs) is what binds it to
    `ODataQuery.limit`, under both its own spelling and `$top`.

    A fresh meter per test is what makes the two-page assertion exact: the
    read path is scoped by `gts_type_id`, so no other test's entries can land
    in this listing however far the ranges overlap.
    """
    meter_id = await make_meter()
    frm, to = selection_range()
    async with api() as client:
        created = accepted_records(await client.post("/records", json={"records": [
            record_payload(meter_id, resource_id="res-a", value="1"),
            record_payload(meter_id, resource_id="res-b", value="2"),
        ]}))
        assert len(created) == 2
        all_ids = {r["id"] for r in created}

        params = {"gts_type_id": meter_id, "from": frm, "to": to, "limit": 1}

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


async def test_aggregate_folds_by_resource_and_declared_metadata(api, make_meter):
    """Seam: aggregation request -> SQL GROUP BY -> PDP-scoped result.

    The request carries NO fold. `SUM` is resolved from the meter's
    `x-gts-traits.aggregation_fold`, so this test also proves the declaration
    reached the gear through types-registry rather than through a fixture.
    The range travels in the body here (`time_range`), not as query
    parameters, because this path has one.

    `group_by` mixes both spellings: a closed dimension is a bare snake_case
    string, and the metadata form carries the key inline as
    `{"metadata": "<key>"}`. `region` is groupable only because the meter's
    schema declares it — the closed surface is what makes a metadata key both
    submittable and groupable, and an undeclared key is refused at ingest
    rather than silently dropped.
    """
    meter_id = await make_meter(fold="SUM", metadata_keys=("region",))
    frm, to = selection_range()
    async with api() as client:
        accepted_records(await client.post("/records", json={"records": [
            record_payload(meter_id, resource_id="res-a", value="10",
                           metadata={"region": "eu"}),
            record_payload(meter_id, resource_id="res-a", value="5",
                           metadata={"region": "us"}),
            record_payload(meter_id, resource_id="res-b", value="7",
                           metadata={"region": "eu"}),
        ]}))

        resp = await client.post(
            "/records/aggregate",
            params={"gts_type_id": meter_id},
            json={
                "time_range": {"from": frm, "to": to},
                "group_by": ["resource_id", {"metadata": "region"}],
            },
        )

    assert resp.status_code == 200, resp.text
    buckets = resp.json()["buckets"]
    # `value` may arrive as a JSON string or number; Decimal(str(...)) accepts both.
    sums = {tuple(b["key"]): Decimal(str(b["value"])) for b in buckets}
    assert sums == {
        ("res-a", "eu"): Decimal("10"),
        ("res-a", "us"): Decimal("5"),
        ("res-b", "eu"): Decimal("7"),
    }


async def test_invalidation_is_admitted_at_most_once(api, make_meter):
    """Seam: at-most-one invalidation per target, enforced at the store.

    A correction is an ordinary ingested entry carrying `invalidates` plus
    `reason_code`; nothing about the target row is mutated. The second
    withdrawal of one target is REJECTED with 409 `ALREADY_INVALIDATED`. The
    partial unique index on `(tenant_id, invalidates, window_end)` is what
    refuses it, inside the insert's own transaction — the gateway takes no
    pre-read for this and could not: a pre-read cannot exclude a concurrent
    second submission, which is the case the rule exists for.

    The refusal is TOP-LEVEL, not a per-record entry inside a 207, and that
    is the backend's statement granularity showing through: the index refuses
    the whole multi-row insert and the transaction rolls back, so the call
    has no partial outcome to report per entry
    (`record_store.rs`, `map_insert_error` / `create_batch_inner`).

    The two withdrawals differ in `idempotency_key`. An identical resubmission
    would be absorbed by dedup on the 5-tuple and answer 200 with the same
    row, which is a different seam and would leave this one untested.
    """
    meter_id = await make_meter()
    period = covered_period()
    async with api() as client:
        created = accepted_records(await client.post("/records", json={
            "records": [record_payload(meter_id, value="3", period=period)]
        }))
        target_id = created[0]["id"]

        withdrawal = accepted_records(await client.post("/records", json={
            "records": [record_payload(
                meter_id, value="3", period=period, invalidates=target_id,
            )]
        }))
        assert withdrawal[0]["entry_type"] == "invalidation"
        assert withdrawal[0]["invalidates"] == target_id
        assert withdrawal[0]["reason_code"] == "E2E_CORRECTION"

        second = await client.post("/records", json={
            "records": [record_payload(
                meter_id, value="3", period=period, invalidates=target_id,
            )]
        })

        # The target is untouched by either attempt: an invalidation appends,
        # and there is no latch on the row it withdraws to flip.
        target = await client.get(f"/records/{target_id}")

    assert second.status_code == 409, f"expected 409, got {second.status_code}: {second.text}"
    assert second.headers["content-type"].startswith("application/problem+json")
    problem = second.json()
    assert problem["status"] == 409
    # The reason code, not just the class: a 409 for an idempotency conflict
    # would otherwise pass here and leave the at-most-one rule untested.
    assert problem["context"]["reason"] == "ALREADY_INVALIDATED", problem

    assert target.status_code == 200, target.text
    assert target.json()["entry_type"] == "record"


async def test_undeclared_meter_is_rejected_per_record(api, make_meter):
    """Seam: type resolution against types-registry, over the wire.

    A syntactically valid meter id that was never declared is a 404 — the gear
    holds no catalog of its own, so this is types-registry answering not-found
    and the resolver failing closed on it. It arrives per record inside a 207,
    alongside an entry against a declared meter that is accepted in the same
    batch: one unresolvable type does not fail the batch.
    """
    declared_id = await make_meter()
    undeclared_id = f"{GTS_BASE}cf.uc_e2e._.never_declared_{unique_suffix()}.v1~"

    async with api() as client:
        resp = await client.post("/records", json={"records": [
            record_payload(declared_id),
            record_payload(undeclared_id),
        ]})

    assert resp.status_code == 207, f"expected 207, got {resp.status_code}: {resp.text}"
    results = resp.json()["results"]
    assert [r["index"] for r in results] == [0, 1], results
    assert results[0]["outcome"] == "accepted", results[0]
    assert results[1]["outcome"] == "rejected", results[1]
    # `NotFoundReason` has no slot on the wire, so the class is the status and
    # the cause is the detail; asserting the status alone would not tell an
    # undeclared meter apart from a missing record.
    assert results[1]["error"]["status"] == 404, results[1]["error"]
    assert undeclared_id in results[1]["error"]["detail"], results[1]["error"]


async def test_backfill_admits_a_period_the_live_path_refuses(api, make_meter):
    """Seam: the two ingestion routes disagree about one covered period.

    The live route admits an entry whose period ENDS within 48 hours of now;
    the backfill route exists for exactly the periods that bound refuses, and
    stamps what it admits `origin: backfill`. The same payload is submitted to
    both, so the routes are the only difference between the two outcomes.

    A week back is past the live bound and well inside the 90-day backfill
    window, so the entry is authorized against the ordinary `create` action
    and needs no elevated permission.
    """
    meter_id = await make_meter()
    period = covered_period(ends_ago=timedelta(days=7))
    payload = record_payload(meter_id, value="8", period=period)

    async with api() as client:
        live = await client.post("/records", json={"records": [payload]})
        imported = accepted_records(
            await client.post("/records/backfill", json={"records": [payload]})
        )

    assert rejected_records(live)[0]["status"] == 400, live.text
    assert imported[0]["origin"] == "backfill"
    # The period is carried verbatim, not clamped to the live route's bound:
    # the route exists for exactly the periods that bound refuses.
    assert datetime.fromisoformat(imported[0]["window_end"].replace("Z", "+00:00")) == \
        datetime.fromisoformat(period[1].replace("Z", "+00:00"))
