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
    TENANT_A,
    accepted_records,
    covered_period,
    record_payload,
    rejected_records,
    selection_range,
    unique_suffix,
    withdrawal_of,
)


async def test_ingest_and_read_record_roundtrip(api, make_meter):
    """Seam: handler <-> JSON wire format <-> PostgreSQL round-trip.

    A Decimal `quantity` crosses as a string and comes back byte-identical, both
    covered-period bounds survive the timestamptz round-trip, and the read
    projection reports the kind the submission declared. The two `entry_type`s
    are not one field making a round trip: on the request it is required and
    caller-supplied, and on the response it is projected from the absent
    `invalidates` rather than read back from a stored column. `origin` is the
    one value here the server assigns outright — the route that admitted the
    entry.
    """
    meter_id = await make_meter()
    period = covered_period()
    payload = record_payload(meter_id, quantity="42.5", period=period)

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
    assert body["quantity"] == "42.5"
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
            record_payload(meter_id, resource_id="res-a", quantity="1"),
            record_payload(meter_id, resource_id="res-b", quantity="2"),
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


async def test_feed_pages_and_resumes_on_its_own_cursor(api, make_meter):
    """Seam: the gateway's CursorV1 <-> the plugin's FeedPosition, over HTTP.

    The one composition nothing else anywhere exercises. An earlier slice
    built the plugin's `FeedPosition`; this slice built the gateway that mints
    it into a `CursorV1` and recovers it again. Every gear-side feed test runs
    against an in-memory stub that hands back a position the test chose, and
    the plugin's integration suite never sees the gateway — so a position the
    plugin cannot interpret, or a subscription fingerprint the resume request
    recomputes differently, is invisible to both.

    Resuming resends the IDENTICAL `gts_type_id` set. A feed cursor binds the
    subscription as a raw cursor binds its filter, so a second page requested
    under a different set is refused `400` (`FILTER_MISMATCH`) rather than
    served.

    `limit` is 1, so the second page's existence is not a guess: DESIGN §3.2
    requires a next cursor on every page of a LIVE read, short pages included,
    which is also why `next_cursor` is asserted on both pages here rather than
    only on the first.

    A fresh meter per test is what makes the disjointness assertion exact —
    the subscription is the only narrowing the feed admits, so no other
    test's entries can land in these pages.
    """
    meter_id = await make_meter()
    async with api() as client:
        created = accepted_records(await client.post("/records", json={"records": [
            record_payload(meter_id, resource_id="res-a", quantity="1"),
            record_payload(meter_id, resource_id="res-b", quantity="2"),
        ]}))
        assert len(created) == 2
        all_ids = {r["id"] for r in created}

        params = {"gts_type_id": meter_id, "limit": 1}

        first = await client.get("/feed", params=params)
        assert first.status_code == 200, first.text
        page_one = first.json()
        assert len(page_one["entries"]) == 1, page_one
        cursor = page_one["page_info"]["next_cursor"]
        assert cursor, "a live read carries a next cursor on every page"

        second = await client.get("/feed", params={**params, "cursor": cursor})

    assert second.status_code == 200, second.text
    page_two = second.json()
    assert len(page_two["entries"]) == 1, page_two
    assert page_two["page_info"]["next_cursor"], (
        "still a live read: the second page carries a cursor too"
    )
    # `prev_cursor` is fixed at null by the contract — the feed reads forward
    # only — and it is a field `FeedPageDto` satisfies by construction rather
    # than by echoing anything, so nothing else would catch it acquiring a
    # value.
    assert page_two["page_info"]["prev_cursor"] is None, page_two["page_info"]

    seen = {page_one["entries"][0]["id"], page_two["entries"][0]["id"]}
    assert seen == all_ids, "the two pages must be disjoint and cover both entries"


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
            record_payload(meter_id, resource_id="res-a", quantity="10",
                           metadata={"region": "eu"}),
            record_payload(meter_id, resource_id="res-a", quantity="5",
                           metadata={"region": "us"}),
            record_payload(meter_id, resource_id="res-b", quantity="7",
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
    """Seam: at most one invalidation per target, as a dedup outcome.

    A withdrawal repeats its target's tenant, GTS type, idempotency key and
    covered period, and declares `entry_type = invalidation`, which fixes all
    six identity components: every withdrawal of one target lands on one
    identity, and a second one is an ordinary collision on it (DESIGN §3.1
    "At most one invalidation"). Under the same reason code it is an exact
    retry: accepted, answering with the stored invalidation. Under another
    reason code it is refused per record inside a 207 as
    `ALREADY_INVALIDATED`, whose context names the invalidation in place and
    its reason code — the reason code is not an identity input, so on a
    faithful copy it is the only field left that can differ.

    No submission here names a target. The gateway derives the target's `id`
    from the withdrawal's own identity inputs with `entry_type` set to
    `record`, so asserting the returned `invalidates` against the target's id
    is what proves that derivation crossed real HTTP and found the right
    record.
    """
    meter_id = await make_meter()
    target_payload = record_payload(meter_id, quantity="3", period=covered_period())
    withdrawal_payload = withdrawal_of(target_payload, reason_code="E2E_CORRECTION")

    async with api() as client:
        created = accepted_records(await client.post("/records", json={
            "records": [target_payload]
        }))
        target_id = created[0]["id"]

        withdrawal = accepted_records(await client.post("/records", json={
            "records": [withdrawal_payload]
        }))
        assert withdrawal[0]["entry_type"] == "invalidation"
        assert withdrawal[0]["invalidates"] == target_id
        assert withdrawal[0]["reason_code"] == "E2E_CORRECTION"
        # The entry type is the sixth identity input, and it is the only one
        # the two entries do not share — without it the withdrawal would
        # derive its target's own id and be absorbed as a retry of it.
        assert withdrawal[0]["id"] != target_id, withdrawal[0]

        retried = accepted_records(await client.post("/records", json={
            "records": [withdrawal_payload]
        }))

        second = await client.post("/records", json={
            "records": [
                withdrawal_of(target_payload, reason_code="E2E_CORRECTION_SECOND")
            ]
        })

        # The target is untouched by any attempt: an invalidation appends.
        target = await client.get(f"/records/{target_id}")

    assert retried[0]["id"] == withdrawal[0]["id"], retried
    assert retried[0]["accepted_at"] == withdrawal[0]["accepted_at"], (
        "an absorbed retry answers with the stored invalidation"
    )

    rejected = rejected_records(second)
    assert len(rejected) == 1, rejected
    problem = rejected[0]
    assert problem["status"] == 409, problem
    assert problem["context"]["reason"] == "ALREADY_INVALIDATED", problem
    assert problem["context"]["invalidated_by"] == withdrawal[0]["id"], problem
    assert problem["context"]["reason_code"] == "E2E_CORRECTION", problem

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
    payload = record_payload(meter_id, quantity="8", period=period)

    async with api() as client:
        live = await client.post("/records", json={"records": [payload]})
        imported = accepted_records(
            await client.post("/records/backfill", json={"records": [payload]})
        )

    # The reason code, not just the class — the standard this file sets on the
    # invalidation test above. Any of a dozen validation faults is a 400, so a
    # status-only assertion would stay green if the past bound stopped firing
    # and something else rejected the payload instead.
    refusal = rejected_records(live)[0]
    assert refusal["status"] == 400, refusal
    violations = refusal["context"]["field_violations"]
    assert [(v["field"], v["reason"]) for v in violations] == [
        ("window_end", "PAST_WINDOW")
    ], refusal

    assert imported[0]["origin"] == "backfill"
    # The period is carried verbatim, not clamped to the live route's bound:
    # the route exists for exactly the periods that bound refuses.
    assert datetime.fromisoformat(imported[0]["window_end"].replace("Z", "+00:00")) == \
        datetime.fromisoformat(period[1].replace("Z", "+00:00"))


async def test_reconciliation_reports_figures_and_refuses_a_reserved_granularity(
    api, make_meter
):
    """Seam: the seventh registered route, and the only one with no process-level
    coverage until this slice.

    `GET /reconciliation` is a thin REST pass-through over
    `Service::get_reconciliation_metadata`, which authorizes, resolves the
    meter's declared fold, and dispatches one `RecordStore::reconciliation`
    call the plugin answers with two real SQL statements against
    TimescaleDB — none of which any other E2E test in this suite reaches.
    `test_aggregate_folds_by_resource_and_declared_metadata` above proves the
    aggregate path crosses HTTP; this proves the fourth read path does too,
    with its own wire shape (`accepted_count`/`quantity_summary`, not a page)
    and its own admission rule (the reserved reporting granularities).

    A SUM meter is used so the summary takes the `accrued_sum` branch, the
    simpler of the two to assert exactly. Both records share one covered
    period and quantities that sum to a value with no floating-point
    ambiguity, so `accrued_sum` can be compared as a plain string.
    """
    meter_id = await make_meter(fold="SUM")
    period = covered_period()
    frm, to = selection_range()

    async with api() as client:
        created = accepted_records(await client.post("/records", json={"records": [
            record_payload(meter_id, quantity="3", period=period),
            record_payload(meter_id, quantity="5", period=period),
        ]}))
        assert len(created) == 2

        recon = await client.get("/reconciliation", params={
            "scope": "tenant_gts_type",
            "gts_type_id": meter_id,
            "tenant_id": TENANT_A,
            "from": frm,
            "to": to,
        })

        # A reserved caller granularity: rejected before anything is
        # dispatched, naming the `scope` parameter rather than answering an
        # empty or misleading figure
        # (`cpt-cf-usage-collector-dod-reconciliation-caller-scopes-reserved`).
        reserved = await client.get("/reconciliation", params={
            "scope": "caller",
            "gts_type_id": meter_id,
            "tenant_id": TENANT_A,
            "from": frm,
            "to": to,
        })

    assert recon.status_code == 200, recon.text
    body = recon.json()
    assert body["scope"] == {"tenant_id": TENANT_A, "gts_type_id": meter_id}, body
    assert body["accepted_count"] == 2, body
    assert body["quantity_summary"] == {"accrued_sum": "8"}, body
    # Both watermarks are populated: the covered period the two records share
    # falls inside the requested range, so neither watermark is the absent
    # case a scope with no entries at all would report.
    assert body["accepted_at_watermark"] is not None, body
    assert body["window_end_watermark"] is not None, body

    assert reserved.status_code == 400, reserved.text
    violations = reserved.json()["context"]["field_violations"]
    assert [v["field"] for v in violations] == ["scope"], reserved.json()
    assert "caller" in violations[0]["description"], violations[0]
