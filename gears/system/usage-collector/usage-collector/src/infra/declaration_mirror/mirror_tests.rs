//! Adapter tests for [`DbDeclarationMirror`], against an in-memory `SQLite`
//! database with this module's own migration applied.
//!
//! `SQLite` rather than a container: the mirror's SQL is one `ON CONFLICT`
//! upsert and one primary-key read, both engine-agnostic, and the pg lanes
//! are untouched by this slice. The idiom (`connect_db("sqlite::memory:")` +
//! `run_migrations_for_testing`) is `account-management`'s, in
//! `gear_tests.rs::migrated_sqlite`.

use serde_json::json;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::{ConnectOpts, DBProvider, Db, connect_db};
use usage_collector_sdk::MeterTypeId;
use uuid::Uuid;

use crate::domain::ports::declaration_mirror::DeclarationMirror;

use super::{DbDeclarationMirror, migrations};

const METER: &str = "gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1~";
// A second, distinct meter — same base type, different derivation segment —
// so a test can tell "the row for THIS id" apart from "a row". Reused from
// `domain/type_resolver/resolver_tests.rs`'s own second meter id.
const METER_B: &str = "gts.cf.core.uc.usage_record.v1~example.metering._.network_egress.v1~";

fn meter_id() -> MeterTypeId {
    MeterTypeId::new(METER).expect("valid meter id")
}

fn meter_id_b() -> MeterTypeId {
    MeterTypeId::new(METER_B).expect("valid meter id")
}

async fn migrated_sqlite() -> Db {
    let db = connect_db(
        "sqlite::memory:",
        ConnectOpts {
            // What actually isolates each test's database from the others
            // is NOT this pool size: `sqlite::memory:` sets
            // `shared_cache = true` and rewrites the filename to a
            // *named* `file:sqlx-in-memory-{seqno}`, where `seqno` is a
            // process-wide atomic counter bumped once per `connect_db`
            // call (`sqlx-sqlite-0.9.0/src/options/parse.rs:14-24`). Every
            // connection opened by ONE pool therefore shares one
            // named in-memory database — a multi-connection pool here
            // would still see its own migration — and it is the per-call
            // `seqno`, not `max_conns`, that keeps this test's database
            // from colliding with the next test's. `max_conns: Some(1)`
            // is harmless belt-and-braces, not the isolation mechanism;
            // kept because it costs nothing.
            max_conns: Some(1),
            min_conns: Some(1),
            ..Default::default()
        },
    )
    .await
    .expect("connect in-memory SQLite");
    run_migrations_for_testing(&db, migrations())
        .await
        .expect("apply the declaration-mirror migration");
    db
}

async fn mirror() -> DbDeclarationMirror {
    DbDeclarationMirror::new(DBProvider::new(migrated_sqlite().await))
}

#[tokio::test]
async fn an_absent_row_reads_as_none() {
    let mirror = mirror().await;
    assert_eq!(
        mirror.read(&meter_id()).await.expect("read succeeds"),
        None,
        "an unmirrored meter must read as None, not as an error"
    );
}

#[tokio::test]
async fn an_upserted_document_reads_back_verbatim() {
    let mirror = mirror().await;
    let document = json!({ "$id": "gts://example", "x-gts-traits": { "aggregation_fold": "SUM" } });

    mirror
        .upsert(&meter_id(), Uuid::from_u128(100), &document)
        .await
        .expect("first write succeeds");

    assert_eq!(
        mirror.read(&meter_id()).await.expect("read succeeds"),
        Some(document),
        "the mirror must return the document it was given, unchanged"
    );
}

#[tokio::test]
async fn distinct_meters_each_read_back_their_own_document_not_the_others() {
    // Every other test in this file reaches the table with exactly one
    // meter id, so none of them can tell a `read` keyed correctly on its
    // argument apart from a `read` that returns whatever row the table
    // happens to hold — `an_upserted_document_reads_back_verbatim`
    // especially cannot see this, since its own `upsert` and `read` agree
    // on the same id regardless of whether `read` even looks at it. Two
    // distinct meters with two distinct documents is the minimum case that
    // can observe a mis-keyed read.
    let mirror = mirror().await;
    let doc_a = json!({ "meter": "a", "v": 1 });
    let doc_b = json!({ "meter": "b", "v": 2 });

    mirror
        .upsert(&meter_id(), Uuid::from_u128(101), &doc_a)
        .await
        .expect("write meter A");
    mirror
        .upsert(&meter_id_b(), Uuid::from_u128(102), &doc_b)
        .await
        .expect("write meter B");

    assert_eq!(
        mirror.read(&meter_id()).await.expect("read meter A"),
        Some(doc_a),
        "meter A must read back its own document, not meter B's"
    );
    assert_eq!(
        mirror.read(&meter_id_b()).await.expect("read meter B"),
        Some(doc_b),
        "meter B must read back its own document, not meter A's"
    );
}

#[tokio::test]
async fn a_second_write_moves_last_seen_and_leaves_first_seen() {
    // `DbConn` implements only `DBRunner`, not sea-orm's `ConnectionTrait`
    // (`libs/toolkit-db/src/secure/runner.rs`'s `impl DBRunner for
    // DbConn<'_>`, with no matching `ConnectionTrait` impl): the mirror
    // entity is `#[secure(unrestricted)]`, so even this test's own
    // inspection of the row goes through `.secure().scope_with(...)`
    // rather than a bare sea-orm call, exactly as the adapter under test
    // does.
    use sea_orm::EntityTrait;
    use toolkit_db::secure::SecureEntityExt;
    use toolkit_security::AccessScope;

    let db = migrated_sqlite().await;
    let provider = DBProvider::new(db);
    let mirror = DbDeclarationMirror::new(provider.clone());

    mirror
        .upsert(&meter_id(), Uuid::from_u128(103), &json!({ "v": 1 }))
        .await
        .expect("first write");
    let first = super::entity::Entity::find_by_id(METER.to_owned())
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&provider.conn().expect("conn"))
        .await
        .expect("query")
        .expect("row exists after the first write");

    // A real elapsed interval, so the two timestamps cannot coincide by
    // clock granularity and make the assertions below pass for the wrong
    // reason.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    mirror
        .upsert(&meter_id(), Uuid::from_u128(103), &json!({ "v": 2 }))
        .await
        .expect("second write");
    let second = super::entity::Entity::find_by_id(METER.to_owned())
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&provider.conn().expect("conn"))
        .await
        .expect("query")
        .expect("row still exists after the second write");

    assert_eq!(
        second.first_seen_at, first.first_seen_at,
        "first_seen_at must be set once; the upsert's DO UPDATE must not name it"
    );
    assert!(
        second.last_seen_at > first.last_seen_at,
        "last_seen_at must move on every write: {} is not after {}",
        second.last_seen_at,
        first.last_seen_at
    );

    // Slice 8b final fix round, item 1.4 -- a Task 2 deferred minor, now
    // closed because it was confirmed to survive on BOTH engines. Every
    // guard above (and its Postgres counterpart in
    // `tests/postgres_declaration_mirror.rs`) compares the two reads TO EACH
    // OTHER -- `second.first_seen_at == first.first_seen_at` -- and never
    // against the write's own clock, so setting `first_seen_at` to
    // `OffsetDateTime::UNIX_EPOCH` in `upsert` left the whole --lib lane
    // green AND the Postgres test passing: a mirror that stamps every row
    // 1970-01-01T00:00:00Z satisfied every gate in the slice on both
    // engines. These two assertions give the column's VALUE a subject.
    //
    // A one-minute window rather than an exact instant: the stamp is taken
    // inside `upsert` from its own `OffsetDateTime::now_utc()`, which this
    // test cannot name, and the column round-trips through the engine's own
    // timestamp representation. A minute is far tighter than any constant a
    // mutation would reach for and far looser than any scheduling or
    // precision noise this test can suffer.
    let now = time::OffsetDateTime::now_utc();
    assert!(
        (now - first.first_seen_at).abs() < time::Duration::minutes(1),
        "first_seen_at must carry the write's OWN clock reading, not a \
         constant: {} is not within a minute of {now}",
        first.first_seen_at
    );
    assert!(
        first.first_seen_at <= first.last_seen_at,
        "the two columns are set from one `now` on the insert, so first_seen_at \
         can never be after last_seen_at: {} is after {}",
        first.first_seen_at,
        first.last_seen_at
    );
    assert_eq!(
        second.document,
        json!({ "v": 2 }).to_string(),
        "the document must be replaced, so the row tracks the registry"
    );
}

#[tokio::test]
async fn a_row_whose_stored_text_is_not_json_surfaces_as_an_error_not_a_panic() {
    // Same `DbConn` vs. `ConnectionTrait` reasoning as the previous test:
    // the seed insert below goes through `.secure().scope_unchecked(...)`
    // rather than a bare sea-orm call.
    use sea_orm::{ActiveValue, EntityTrait};
    use toolkit_db::secure::SecureInsertExt;
    use toolkit_security::AccessScope;

    let db = migrated_sqlite().await;
    let provider = DBProvider::new(db);

    // Written past the adapter deliberately: no adapter path can produce
    // this, and the point is that a row an older schema (or a hand edit)
    // left behind cannot panic the resolver's cold path.
    let now = time::OffsetDateTime::now_utc();
    super::entity::Entity::insert(super::entity::ActiveModel {
        gts_type_id: ActiveValue::Set(METER.to_owned()),
        gts_type_uuid: ActiveValue::Set(Uuid::from_u128(104)),
        document: ActiveValue::Set("{not json".to_owned()),
        first_seen_at: ActiveValue::Set(now),
        last_seen_at: ActiveValue::Set(now),
    })
    .secure()
    .scope_unchecked(&AccessScope::allow_all())
    .expect("scope accepted for an unrestricted entity")
    .exec(&provider.conn().expect("conn"))
    .await
    .expect("seed the corrupt row");

    let err = DbDeclarationMirror::new(provider)
        .read(&meter_id())
        .await
        .expect_err("unparseable stored text must be an error");
    assert!(
        err.to_string().contains("is not valid JSON"),
        "the error must name the defect so an operator can find the row: {err}"
    );
}

#[tokio::test]
async fn an_upsert_stores_the_registry_reference_it_is_given() {
    let db = migrated_sqlite().await;
    let mirror = DbDeclarationMirror::new(DBProvider::new(db));
    let reference = Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888);

    mirror
        .upsert(&meter_id(), reference, &json!({"a": 1}))
        .await
        .expect("first write");

    let stored = mirror.resolve_id(reference).await.expect("read back");
    assert_eq!(
        stored.map(|id| id.as_str().to_owned()).as_deref(),
        Some(METER),
        "the reverse read must answer with the identifier the upsert was given, \
         keyed by the registry reference it was given"
    );
}

#[tokio::test]
async fn a_second_upsert_never_moves_the_registry_reference() {
    let db = migrated_sqlite().await;
    let mirror = DbDeclarationMirror::new(DBProvider::new(db));
    let first = Uuid::from_u128(1);
    let second = Uuid::from_u128(2);

    mirror
        .upsert(&meter_id(), first, &json!({"v": 1}))
        .await
        .expect("first write");
    mirror
        .upsert(&meter_id(), second, &json!({"v": 2}))
        .await
        .expect("second write");

    // Review Focus 5: nothing couples the identifier to the reference, so the
    // port stores what it is given — and `ON CONFLICT DO UPDATE` must not name
    // this column, exactly as it must not name `first_seen_at`.
    assert_eq!(
        mirror
            .resolve_id(first)
            .await
            .expect("read first")
            .map(|id| id.as_str().to_owned())
            .as_deref(),
        Some(METER),
        "the reference set on insert must survive every later write"
    );
    assert!(
        mirror
            .resolve_id(second)
            .await
            .expect("read second")
            .is_none(),
        "a later write must not rebind the row to a new reference"
    );
    assert_eq!(
        mirror.read(&meter_id()).await.expect("read doc"),
        Some(json!({"v": 2})),
        "the document must still be updated by the second write"
    );
}

#[tokio::test]
async fn the_migration_is_idempotent_on_sqlite() {
    // Review Focus 1: a second boot re-runs the migration set.
    //
    // Fix round 1, Important 1: `MigrationAddTypeUuid::up` DROPS the table,
    // so "no error" alone is not idempotence -- a runner that wrongly
    // re-applied it on a second boot would silently wipe the mirror and
    // this test would still pass. Binding the result and asserting
    // `applied == 0` / `skipped == 2` (the same shape
    // `tests/postgres_declaration_mirror.rs` already asserts on Postgres)
    // is what actually distinguishes "no-op" from "re-ran and happened not
    // to error".
    let db = migrated_sqlite().await;
    let second_boot = run_migrations_for_testing(&db, migrations())
        .await
        .expect("a second application of the migration set is a no-op");
    assert_eq!(
        second_boot.applied, 0,
        "a second boot must apply nothing new -- any non-zero count here means \
         a migration (possibly the destructive drop-and-recreate) re-ran"
    );
    assert_eq!(
        second_boot.skipped, 2,
        "a second boot must skip both already-applied migrations"
    );
}

#[tokio::test]
async fn resolve_id_answers_none_for_an_unknown_reference() {
    let db = migrated_sqlite().await;
    let mirror = DbDeclarationMirror::new(DBProvider::new(db.clone()));

    let found = mirror
        .resolve_id(Uuid::from_u128(0xdead_beef))
        .await
        .expect("an unknown reference is not an error");
    assert!(
        found.is_none(),
        "an unmirrored reference must resolve to None, not Some(..): {found:?}"
    );
}

#[tokio::test]
async fn resolve_id_reports_a_row_whose_identifier_no_longer_validates() {
    // Review Focus 3. The row is written behind the port's back, because the
    // port cannot construct an invalid identifier.
    //
    // Seeded through the entity's own `ActiveModel` rather than the raw SQL
    // this test was originally specified with: `toolkit_db::secure::DbConn`
    // implements only `DBRunner`, not sea-orm's `ConnectionTrait` (see the
    // reasoning beside the JSON-corruption test above), so there is no
    // `execute_unprepared` reachable from this crate at all, over either
    // `Db` or `DBProvider`. `gts_type_id` is a plain `String` column on
    // `entity::Model` -- nothing at the entity layer validates it -- so an
    // `ActiveModel` insert reaches the same corrupt state the raw SQL would
    // have, through a path this crate can actually compile.
    use sea_orm::{ActiveValue, EntityTrait};
    use toolkit_db::secure::SecureInsertExt;
    use toolkit_security::AccessScope;

    let db = migrated_sqlite().await;
    let provider = DBProvider::new(db);
    let reference = Uuid::from_u128(7);
    let now = time::OffsetDateTime::now_utc();

    super::entity::Entity::insert(super::entity::ActiveModel {
        gts_type_id: ActiveValue::Set("not-a-gts-id".to_owned()),
        gts_type_uuid: ActiveValue::Set(reference),
        document: ActiveValue::Set("{}".to_owned()),
        first_seen_at: ActiveValue::Set(now),
        last_seen_at: ActiveValue::Set(now),
    })
    .secure()
    .scope_unchecked(&AccessScope::allow_all())
    .expect("scope accepted for an unrestricted entity")
    .exec(&provider.conn().expect("conn"))
    .await
    .expect("seed a corrupt row");

    let mirror = DbDeclarationMirror::new(provider);
    let err = mirror
        .resolve_id(reference)
        .await
        .expect_err("a corrupt row is a defect, not an absent row");
    assert!(
        err.to_string().contains("not-a-gts-id"),
        "the error must name the row: {err}"
    );
}

#[tokio::test]
async fn two_identifiers_cannot_share_one_reference() {
    // Review Focus 4, SQLite side. Task 6 pins the same on Postgres.
    let db = migrated_sqlite().await;
    let mirror = DbDeclarationMirror::new(DBProvider::new(db.clone()));
    let shared = Uuid::from_u128(9);

    mirror
        .upsert(&meter_id(), shared, &json!({}))
        .await
        .expect("first identifier claims the reference");
    let second = mirror.upsert(&meter_id_b(), shared, &json!({})).await;

    let err =
        second.expect_err("the unique index must refuse a second identifier under one reference");
    // Not the index's own name (`uq_usage_collector__declaration_mirror_type_uuid`):
    // verified against the actual driver, `SQLite`'s own UNIQUE-violation
    // message names the table and column it tripped on
    // (`UNIQUE constraint failed: usage_collector__declaration_mirror.gts_type_uuid`),
    // never the index identifier, so asserting the index name here would pin
    // a string this engine never produces. `gts_type_uuid` is specific
    // enough on its own to rule out a coincidental failure for an unrelated
    // reason — this table carries exactly one unique constraint, and it is
    // on that column.
    assert!(
        err.to_string().contains("gts_type_uuid"),
        "the failure must name the column whose uniqueness it tripped, ruling out a \
         coincidental failure for an unrelated reason: {err}"
    );
}
