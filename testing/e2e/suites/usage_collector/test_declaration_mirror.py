"""E2E coverage for the DESIGN §3.7 declaration mirror.

The mirror is the one piece of this gear's durable state that is NOT in the
storage plugin's TimescaleDB: a small SQLite table under the server's
`home_dir` holding the raw GTS document of every meter the Type Resolver has
resolved, so a meter survives a `types-registry` restart
(`cpt-cf-usage-collector-adr-declaration-rehydration`). Every run of this
suite has been writing real rows into that table, and nothing read them back:
a live write path with no assertion over it anywhere in the e2e lane. Measured
that way too — with the mirror write stubbed out of the gear entirely, all
thirteen tests that predate this module still passed.

**The instrument is Python's `sqlite3`, opened `mode=ro`.** There is no
`sqlite3` binary on this machine, and read-only is not merely tidy: the table
belongs to the gear, and a suite that could write it could manufacture the
very rows it asserts about.

Three tests, one per claim the table makes:

- the INSERT arm carries the declaration that was registered, which this
  suite has been reaching all along without looking at it;
- the table holds nothing but meters THIS run declared, which is the
  observable form of `conftest.py`'s `fresh_declaration_mirror` having taken
  the file's lifecycle over from nobody; and
- the `ON CONFLICT DO UPDATE` arm, which nothing had ever executed in a
  booted gear. Reaching it needs one meter resolved twice with a
  declaration-cache miss in between — see `config.yaml`'s
  `type_cache_capacity` for how that miss is arranged.
"""

import json
import re
import sqlite3
from pathlib import Path

from .conftest import (
    MIRROR_DB,
    TYPE_CACHE_CAPACITY,
    accepted_records,
    meter_document,
    record_payload,
)

# The mirror's own timestamp spelling: whole-second RFC3339 in UTC with an
# optional fraction, as `sea-orm` stores an `OffsetDateTime` into the table's
# `timestamp_with_timezone_text` column. Matched rather than assumed, so a
# change in that spelling fails loudly here instead of silently defeating the
# comparison below.
_STAMP = re.compile(r"(?P<whole>\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})(?:\.(?P<frac>\d+))?Z")


def _instant(stamp: str) -> tuple[str, int]:
    """`stamp` as a correctly ORDERED value.

    Not a string comparison, and not for pedantry: the mirror's fractions are
    written at whatever width the nanosecond value needs (8 and 9 digits both
    appear in a real table), and comparing those as text gets a pair like
    `...:05.5Z` / `...:05.5004Z` backwards — `Z` sorts above `0`, so the
    earlier instant would read as the later one. Padding the fraction to a
    fixed nanosecond width removes that; the whole-seconds half is already
    fixed-width and UTC, so it orders correctly as text.
    """
    matched = _STAMP.fullmatch(stamp)
    assert matched, f"not a mirror timestamp: {stamp!r}"
    return matched["whole"], int((matched["frac"] or "").ljust(9, "0"))


def _mirror_rows(db: Path) -> dict[str, tuple[str, str, str]]:
    """Every mirror row, keyed by meter id: (document, first_seen, last_seen).

    The gear holds this database open with WAL enabled while the suite runs,
    so this reads through a second connection rather than waiting for the
    server to stop — which is the only way the `ON CONFLICT` test can compare
    a row against its own earlier state at all.
    """
    assert db.exists(), f"the gear should have created {db}"
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        return {
            gts_type_id: (document, first_seen_at, last_seen_at)
            for gts_type_id, document, first_seen_at, last_seen_at in conn.execute(
                "SELECT gts_type_id, document, first_seen_at, last_seen_at "
                "FROM usage_collector__declaration_mirror"
            )
        }
    finally:
        conn.close()


async def _ingest(api, meter_id: str) -> None:
    """Submit one admissible record, which is what makes the gear RESOLVE
    `meter_id` — and resolving is what writes the mirror row. Nothing on the
    wire asks for a mirror write; it is a side effect of the resolution the
    ingest path performs on its way to validating the entry.
    """
    async with api() as client:
        accepted_records(
            await client.post("/records", json={"records": [record_payload(meter_id)]})
        )


async def test_the_mirror_holds_the_declaration_the_gear_resolved(api, make_meter):
    """The row the gear wrote carries the declaration that was registered.

    By equality against the document `make_meter` posted, not by spot checks:
    the mirror's contract is that a restore can re-register this row verbatim
    (`TypeResolver::restore_document`), so anything less than the whole
    document is not what the row is for. Note the key is the BARE meter id
    while the document's own `$id` carries the `gts://` prefix — the two
    spellings sit in one row, and the equality below pins both.
    """
    meter_id = await make_meter()
    await _ingest(api, meter_id)

    rows = _mirror_rows(MIRROR_DB)
    assert meter_id in rows, (
        f"a resolved meter should have been mirrored; table holds {sorted(rows)}"
    )
    document, first_seen_at, last_seen_at = rows[meter_id]

    assert json.loads(document) == meter_document(meter_id)
    # A first insert writes both stamps from one `now`, so this is equality,
    # not ordering — and it is what makes the second test's `>` meaningful.
    assert first_seen_at == last_seen_at


async def test_the_mirror_carries_nothing_over_from_an_earlier_run(
    api, make_meter, declared_meters
):
    """The table holds only meters THIS session declared.

    Two things at once, and both are the point. The gear mirrors a meter it
    resolved and nothing else — the abstract base type every meter derives
    from is registered at link time and never resolved through the Type
    Resolver, so it must not be in here. And the file the gear appends to was
    emptied before the server booted: a row naming an id this session never
    minted is a row from a previous run, which is the state this table was
    actually in before `conftest.py`'s `fresh_declaration_mirror` took it over
    (120 rows spanning 18 hours, measured).

    Order-independent, so it does not matter where this lands in the suite:
    the subset holds at every point in a run, since `declared_meters` only
    ever grows and the gear can only mirror what it has already resolved.
    """
    meter_id = await make_meter()
    await _ingest(api, meter_id)

    rows = _mirror_rows(MIRROR_DB)
    assert meter_id in rows, "the meter this test resolved should be mirrored"
    assert set(rows) <= declared_meters, (
        "the mirror holds meters this session never declared: "
        f"{sorted(set(rows) - declared_meters)}"
    )


async def test_a_second_resolution_of_one_meter_updates_last_seen(
    api, make_meter, meter_budget
):
    """The `ON CONFLICT DO UPDATE` arm, executed in a booted gear.

    `DbDeclarationMirror::upsert` is one statement whose conflict action names
    `document` and `last_seen_at` and never `first_seen_at`, so "first seen
    once, last seen on every write" is structural rather than a branch. This
    is the end-to-end proof of that: one meter, two resolutions, with
    `TYPE_CACHE_CAPACITY` other meters resolved in between to evict it from
    the declaration cache — because a cached meter is never re-resolved and
    the conflict would never be reached.

    The only test in this suite that declares more than one meter, which is
    why it is also the only one that spends `meter_budget`.
    """
    meter_id = await make_meter()
    await _ingest(api, meter_id)
    _, inserted_first_seen, inserted_last_seen = _mirror_rows(MIRROR_DB)[meter_id]

    # Evicting this test's own meter is the point of this test, so the extra
    # meters are budgeted for explicitly — see `conftest.MeterBudget`, which
    # fails any OTHER test that declares more than one.
    meter_budget.allow_more(TYPE_CACHE_CAPACITY)
    for _ in range(TYPE_CACHE_CAPACITY):
        await _ingest(api, await make_meter())

    await _ingest(api, meter_id)
    document, first_seen_at, last_seen_at = _mirror_rows(MIRROR_DB)[meter_id]

    assert first_seen_at == inserted_first_seen, (
        "first_seen_at is not in the DO UPDATE set, so it must survive byte for byte"
    )
    assert _instant(last_seen_at) > _instant(inserted_last_seen), (
        "last_seen_at must advance on the second write — equal stamps would mean "
        "the second resolution never reached the mirror at all"
    )
    # The same declaration, rewritten: `document` IS in the DO UPDATE set, so
    # this pins that the update carries the row forward rather than damaging it.
    assert json.loads(document) == meter_document(meter_id)
