"""AuthZ and idempotency E2E tests for the usage-collector gear.

These exercise the real PDP pipeline: static-authz returns a row scope clamped
to the caller's own tenant, and the gear matches each record's attribution
tuple against that scope. Tenants A and B are siblings under one root, so
neither is in the other's subtree.
"""

from .conftest import (
    TENANT_B,
    TOKEN_B,
    accepted_records,
    covered_period,
    record_payload,
    rejected_records,
    selection_range,
    unique_suffix,
)


async def test_cross_tenant_ingest_denied(api, make_meter):
    """Seam: per-record attribution gate vs. the PDP-returned scope.

    Tenant A's token submits a record attributed to tenant B. Ingest is a BATCH
    path that authorizes per record (grouped by attribution tuple), so the
    denial arrives as 207 with a `rejected` entry — NOT a top-level 403.
    """
    meter_id = await make_meter()
    async with api() as client:
        resp = await client.post("/records", json={"records": [
            record_payload(meter_id, tenant_id=TENANT_B),
        ]})

    # `error` carries the full canonical Problem verbatim (api/rest/dto.rs
    # CreateUsageRecordResultDto::Rejected), not just a bare message — assert on
    # its `status` being the forbidden class so a record rejected for an
    # unrelated reason (undeclared meter, validation failure, storage error)
    # cannot masquerade as the attribution gate having fired.
    problems = rejected_records(resp)
    assert len(problems) == 1
    assert problems[0]["status"] == 403, problems[0]


async def test_cross_tenant_read_no_existence_leak(api, make_meter):
    """Seam: a foreign reader gets 404, not 403 — no existence leak.

    403 would confirm the record exists. The single-record read path fails at
    the top level (unlike batch ingest).
    """
    meter_id = await make_meter()
    async with api() as client:
        created = accepted_records(
            await client.post("/records", json={"records": [record_payload(meter_id)]})
        )
        record_id = created[0]["id"]

    async with api(TOKEN_B) as foreign:
        resp = await foreign.get(f"/records/{record_id}")

    assert resp.status_code == 404, (
        f"a foreign tenant must not learn the record exists, got "
        f"{resp.status_code}: {resp.text}"
    )


async def test_idempotent_repost_returns_same_record(api, make_meter):
    """Seam: dedup on (tenant_id, gts_type_id, idempotency_key, period).

    An identical re-POST is deduplicated, not duplicated: same record id back,
    and one row visible in the listing. The covered period is part of the dedup
    identity (`cpt-cf-usage-collector-adr-record-identity-derivation`), so it is
    pinned here rather than regenerated per call — a second `covered_period()`
    would move the window bounds and make the two submissions distinct entries
    rather than a retry.
    """
    meter_id = await make_meter()
    payload = record_payload(
        meter_id,
        idempotency_key=f"e2e-idem-fixed-{unique_suffix()}",
        period=covered_period(),
    )
    frm, to = selection_range()

    async with api() as client:
        first = accepted_records(await client.post("/records", json={"records": [payload]}))
        second = accepted_records(await client.post("/records", json={"records": [payload]}))

        listing = await client.get("/records", params={
            "gts_type_id": meter_id, "from": frm, "to": to, "limit": 1000,
        })

    assert first[0]["id"] == second[0]["id"], "re-POST must dedup onto the same record"
    assert listing.status_code == 200, listing.text
    ids = [item["id"] for item in listing.json()["items"]]
    assert ids == [first[0]["id"]], f"expected exactly one row, saw {ids}"
