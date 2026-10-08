//! Postgres-only: the declaration mirror's migration and its one `ON
//! CONFLICT` upsert, executed against the backend a production deployment
//! actually binds.
//!
//! DESIGN §3.7 calls the mirror's SQL engine-agnostic (one upsert, one
//! primary-key read), but until this suite existed that claim had never been
//! checked against Postgres itself: the adapter's own unit tests
//! (`src/infra/declaration_mirror/mirror_tests.rs`) run on
//! `sqlite::memory:`, the e2e suite binds `engine: "sqlite"`
//! (`testing/e2e/suites/usage_collector/config.yaml`), and every other
//! `postgres_*`-named test under this gear's tree belongs to the
//! `TimescaleDB` storage plugin, on the plugin's own tables, never this
//! gear's. A Postgres deployment would otherwise be the first execution of
//! this DDL and this `ON CONFLICT DO UPDATE` anywhere.
//!
//! The migration's `up()` already carries `.if_not_exists()`
//! (`src/infra/declaration_mirror/migrations.rs`) — precisely the guard
//! `gears/bss/ledger`'s known second-boot crash-loop lacked — so this
//! suite is checking an UNTESTED engine, not a known defect. It pins that:
//! applying the migration twice (two boots against the same database) is a
//! clean no-op; the upsert's `ON CONFLICT DO UPDATE` path — never reachable
//! on a fresh `sqlite::memory:` database used only once per test, the way
//! `mirror_tests.rs`'s `a_second_write_moves_last_seen_and_leaves_first_seen`
//! exercises it — actually updates in place on Postgres rather than erroring
//! on the duplicate primary key; that the upsert's own structural contract
//! (`first_seen_at` set once, `last_seen_at` moved on every write — the
//! entity's whole design claim, and the reason this is one `ON CONFLICT DO
//! UPDATE` statement rather than a read-then-branch) holds on Postgres, the
//! one engine a production deployment actually binds, not just on `SQLite`;
//! and that the migration's DDL produced the column shapes DESIGN §3.7
//! actually specifies, not merely "some shape that didn't error".
//!
//! **Fix round 2 note.** The first version of this suite (round 1) proved
//! only that an insert works and a second upsert replaces the document —
//! `DeclarationMirror::read` returns the document alone, with no
//! `first_seen_at` / `last_seen_at`, so nothing through that port can reach
//! the one property that matters most about this table. The row-level
//! assertions below reach `first_seen_at` / `last_seen_at` directly, by raw
//! SQL against a SEPARATE plain connection
//! (`sea_orm::Database::connect`, not through `DbDeclarationMirror`'s own
//! `DBProvider`) — the same split ledger's own `postgres_migration_idempotency.rs`
//! uses for its inspection queries. This is a deliberate departure from
//! `entity::Entity::find_by_id(...)` (what `mirror_tests.rs`'s `SQLite`
//! version uses): that entity model is a private module
//! (`src/infra/declaration_mirror/entity.rs`, `mod entity;` with no `pub`),
//! unreachable from an external `tests/*.rs` integration binary by Rust's
//! own privacy rules — verified by trying it first. Raw SQL against the
//! column names DESIGN §3.7 and `entity.rs` both name is the mechanism
//! actually available here, and it reaches the identical two columns.
//!
//! Idiom adopted from `gears/bss/ledger/ledger/tests/postgres_migration_idempotency.rs`,
//! not invented.
//!
//! Ignored by default (needs Docker); run with:
//! `cargo test -p cf-gears-usage-collector --test postgres_declaration_mirror -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)]

use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use serde_json::json;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use usage_collector::domain::ports::declaration_mirror::DeclarationMirror;
use usage_collector::infra::declaration_mirror::{DbDeclarationMirror, migrations};
use usage_collector_sdk::MeterTypeId;
use uuid::Uuid;

const METER: &str = "gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1~";
const TABLE: &str = "usage_collector__declaration_mirror";

fn meter_id() -> MeterTypeId {
    MeterTypeId::new(METER).expect("valid meter id")
}

fn pg(sql: impl Into<String>) -> Statement {
    Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.into())
}

/// Reads `first_seen_at` / `last_seen_at` for `METER` directly, bypassing
/// `DeclarationMirror::read` entirely (see the module doc on why: that port
/// returns the document alone).
async fn row_timestamps(conn: &DatabaseConnection) -> (OffsetDateTime, OffsetDateTime) {
    let row = conn
        .query_one_raw(pg(format!(
            "SELECT first_seen_at, last_seen_at FROM {TABLE} WHERE gts_type_id = '{METER}'"
        )))
        .await
        .expect("query row timestamps")
        .expect("row must exist after an upsert");
    let first_seen_at: OffsetDateTime = row.try_get("", "first_seen_at").expect("first_seen_at");
    let last_seen_at: OffsetDateTime = row.try_get("", "last_seen_at").expect("last_seen_at");
    (first_seen_at, last_seen_at)
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn migration_is_idempotent_and_the_upsert_round_trips_through_on_conflict() {
    let container = test_containers::postgres()
        .start()
        .await
        .expect("start postgres container");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("mapped host port");
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let db = connect_db(&url, ConnectOpts::default())
        .await
        .expect("connect to postgres");

    // Boot 1: a fresh database applies the mirror's two migrations (the
    // original table, then the drop-and-recreate that adds the registry
    // reference).
    let r1 = run_migrations_for_testing(&db, migrations())
        .await
        .expect("boot 1 migration must succeed");
    assert_eq!(
        r1.applied, 2,
        "boot 1 applies both declaration-mirror migrations"
    );
    assert_eq!(r1.skipped, 0, "nothing can be skipped on a fresh database");

    // Boot 2, same connection, same migration set: must be a clean no-op --
    // the `.if_not_exists()` guard this migration's `up()` carries is what
    // makes this true rather than a second CREATE TABLE erroring.
    let r2 = run_migrations_for_testing(&db, migrations())
        .await
        .expect("boot 2 migration must be a clean no-op, not a crash");
    assert_eq!(r2.applied, 0, "boot 2 applies nothing new");
    assert_eq!(
        r2.skipped, 2,
        "boot 2 skips both already-applied migrations"
    );

    // A second, independent plain connection for inspection queries only --
    // never used for a write the adapter itself would make; see the module
    // doc for why this split, not `entity::Entity`, is what is reachable
    // from this file.
    let inspect = Database::connect(&url)
        .await
        .expect("connect a second plain connection for inspection");

    // The migration's DDL must have produced the column shapes DESIGN §3.7
    // actually specifies -- not merely "some shape that didn't error".
    // Asserted as a roster (name, data_type, is_nullable), not a count, so a
    // narrowed or widened column is named in the failure rather than hidden
    // behind a passing length check.
    let columns = inspect
        .query_all_raw(pg(format!(
            "SELECT column_name, data_type, is_nullable FROM information_schema.columns \
             WHERE table_name = '{TABLE}' ORDER BY column_name"
        )))
        .await
        .expect("query column shapes");
    let shapes: Vec<(String, String, String)> = columns
        .into_iter()
        .map(|row| {
            (
                row.try_get::<String>("", "column_name")
                    .expect("column_name"),
                row.try_get::<String>("", "data_type").expect("data_type"),
                row.try_get::<String>("", "is_nullable")
                    .expect("is_nullable"),
            )
        })
        .collect();
    assert_eq!(
        shapes,
        vec![
            ("document".to_owned(), "text".to_owned(), "NO".to_owned()),
            (
                "first_seen_at".to_owned(),
                "timestamp with time zone".to_owned(),
                "NO".to_owned()
            ),
            ("gts_type_id".to_owned(), "text".to_owned(), "NO".to_owned()),
            (
                "gts_type_uuid".to_owned(),
                "uuid".to_owned(),
                "NO".to_owned()
            ),
            (
                "last_seen_at".to_owned(),
                "timestamp with time zone".to_owned(),
                "NO".to_owned()
            ),
        ],
        "the migration must produce exactly these five NOT NULL columns, \
         with first_seen_at/last_seen_at as a real timestamptz and \
         gts_type_uuid as a real uuid, on Postgres"
    );

    // One upsert round trip, including the ON CONFLICT DO UPDATE path this
    // table's SQL is built around (`src/infra/declaration_mirror/mod.rs`'s
    // `upsert`): the first write inserts, the second updates the existing
    // row in place rather than failing on the duplicate primary key.
    let mirror = DbDeclarationMirror::new(DBProvider::new(db));
    let type_uuid = Uuid::from_u128(1);
    let doc_v1 = json!({ "v": 1 });
    mirror
        .upsert(&meter_id(), type_uuid, &doc_v1)
        .await
        .expect("first write (insert)");
    assert_eq!(
        mirror.read(&meter_id()).await.expect("read after insert"),
        Some(doc_v1),
        "the mirror must return exactly the document it was given"
    );
    let (first_seen_1, last_seen_1) = row_timestamps(&inspect).await;

    // A real elapsed interval, so the two timestamps cannot coincide by
    // clock/column-precision granularity and make the assertions below pass
    // for the wrong reason -- same reasoning as `mirror_tests.rs`'s own
    // SQLite version of this test.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    let doc_v2 = json!({ "v": 2 });
    mirror
        .upsert(&meter_id(), type_uuid, &doc_v2)
        .await
        .expect("second write must go through ON CONFLICT DO UPDATE, not error");
    assert_eq!(
        mirror.read(&meter_id()).await.expect("read after update"),
        Some(doc_v2),
        "the second write must replace the document via Postgres's own \
         ON CONFLICT DO UPDATE path, not an application-level read-then-write"
    );
    let (first_seen_2, last_seen_2) = row_timestamps(&inspect).await;

    // The structural contract the `ON CONFLICT DO UPDATE` statement is
    // built around, on the one engine a production deployment actually
    // binds: `first_seen_at` is named only in the INSERT branch, never in
    // `DO UPDATE`, so it cannot move; `last_seen_at` IS named in `DO
    // UPDATE`, so it must move on every write.
    assert_eq!(
        first_seen_2, first_seen_1,
        "first_seen_at must be set once on Postgres too; the upsert's \
         DO UPDATE clause must not name it"
    );
    assert!(
        last_seen_2 > last_seen_1,
        "last_seen_at must move on every write, on Postgres too: {last_seen_2} \
         is not after {last_seen_1}"
    );

    // Slice 8b final fix round, item 1.4 -- the Postgres half. The guard
    // above compares the two reads TO EACH OTHER (`first_seen_2 ==
    // first_seen_1`) and never against the write's own clock, so setting
    // `first_seen_at` to `OffsetDateTime::UNIX_EPOCH` in `upsert` left this
    // suite passing as well as the whole --lib lane: a mirror that stamps
    // every row 1970-01-01T00:00:00Z satisfied every gate in the slice on
    // BOTH engines. These two assertions give the column's VALUE a subject
    // on the engine a production deployment actually binds, where the stamp
    // also has to survive the `timestamptz` round trip the column-shape
    // roster above pins.
    //
    // A one-minute window rather than an exact instant, same as the SQLite
    // counterpart in `src/infra/declaration_mirror/mirror_tests.rs`: the
    // stamp comes from `upsert`'s own `OffsetDateTime::now_utc()`, which
    // this test cannot name.
    let now = OffsetDateTime::now_utc();
    assert!(
        (now - first_seen_1).abs() < time::Duration::minutes(1),
        "first_seen_at must carry the write's OWN clock reading on Postgres, \
         not a constant: {first_seen_1} is not within a minute of {now}"
    );
    assert!(
        first_seen_1 <= last_seen_1,
        "the two columns are set from one `now` on the insert, so first_seen_at \
         can never be after last_seen_at: {first_seen_1} is after {last_seen_1}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_reference_column_and_its_unique_index_exist_and_hold() {
    let container = test_containers::postgres()
        .start()
        .await
        .expect("start postgres container");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("mapped host port");
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let db = connect_db(&url, ConnectOpts::default())
        .await
        .expect("connect postgres");
    run_migrations_for_testing(&db, migrations())
        .await
        .expect("apply the mirror migrations");
    // Review Focus 1, Postgres side: a second boot is a clean no-op.
    run_migrations_for_testing(&db, migrations())
        .await
        .expect("a second application is a no-op");

    let inspect = Database::connect(&url)
        .await
        .expect("inspection connection");

    let index: String = inspect
        .query_one_raw(pg(format!(
            "SELECT indexdef FROM pg_indexes WHERE tablename = '{TABLE}' \
             AND indexname = 'uq_usage_collector__declaration_mirror_type_uuid'"
        )))
        .await
        .expect("query index")
        .expect("the unique index must exist")
        .try_get("", "indexdef")
        .expect("indexdef");
    assert!(
        index.contains("UNIQUE") && index.contains("gts_type_uuid"),
        "the index must be unique on the reference: {index}"
    );

    // Review Focus 4, Postgres side.
    let mirror = DbDeclarationMirror::new(DBProvider::new(db.clone()));
    let shared = uuid::Uuid::from_u128(42);
    mirror
        .upsert(&meter_id(), shared, &json!({}))
        .await
        .expect("first identifier claims the reference");
    let second_meter =
        MeterTypeId::new("gts.cf.core.uc.usage_record.v1~example.metering._.network_egress.v1~")
            .expect("second meter id");
    assert!(
        mirror
            .upsert(&second_meter, shared, &json!({}))
            .await
            .is_err(),
        "the unique index must refuse a second identifier under one reference"
    );

    // And the reverse read round-trips on the engine production binds.
    assert_eq!(
        mirror
            .resolve_id(shared)
            .await
            .expect("reverse read")
            .map(|id| id.as_str().to_owned())
            .as_deref(),
        Some(METER),
        "the reverse read must answer with the identifier that first claimed the \
         reference, on the engine a production deployment actually binds"
    );
}
