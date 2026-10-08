//! The mirror's migrations. Temporary with the module.
//!
//! **Two indexes: the primary key and one unique index on the registry
//! reference.** The reverse direction — reference to identifier — is read by
//! `MeterReverseResolver`, so the mirror now has a second reader beyond the
//! restore path. The index is unique rather than plain because it is an
//! assertion as much as an access path: a `MeterTypeId` cannot carry an
//! explicit UUID tail, so `GtsId::to_uuid` is always its injective `UUIDv5`
//! branch, and if that ever stops holding, this constraint prevents a second
//! identifier from silently rebinding a stored reference — it does not
//! guarantee the mirror answers correctly even then: if the residual ever
//! materialised, the second identifier's mirror write simply fails, that
//! failure is swallowed and counted on the generic write-failure counter,
//! and tier 2 keeps serving the pre-existing row. Do not add a third index
//! speculatively.

use sea_orm_migration::prelude::*;

/// Column identifiers, so the DDL below names each column once per
/// statement rather than repeating a string literal five times in `up`.
///
/// **This enum's guard against `entity.rs` is a runtime one, not a compile-time
/// one, for `GtsTypeId`/`GtsTypeUuid`/`Document`/`FirstSeenAt`/`LastSeenAt`.**
/// These five variants are local to this file; `entity.rs`'s column spellings
/// come from its own struct field names via `DeriveEntityModel`, a wholly
/// separate derive with no shared symbol between the two. Renaming one of
/// these five variants compiles clean — the mismatch surfaces only when a
/// test runs the migration and then queries the entity against it (`table …
/// has no column named …`). What IS a genuine single spelling is `Table`: it
/// carries an
/// explicit `#[sea_orm(iden = "...")]` naming the exact string
/// `entity.rs`'s `#[sea_orm(table_name = "usage_collector__declaration_mirror")]`
/// also carries, so the table name itself has one source either way, even
/// though the two cannot be compared by the compiler.
#[derive(DeriveIden)]
enum DeclarationMirror {
    #[sea_orm(iden = "usage_collector__declaration_mirror")]
    Table,
    GtsTypeId,
    GtsTypeUuid,
    Document,
    FirstSeenAt,
    LastSeenAt,
}

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &'static str {
        "m20261004_000001_declaration_mirror"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(DeclarationMirror::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(DeclarationMirror::GtsTypeId)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(DeclarationMirror::Document)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DeclarationMirror::FirstSeenAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DeclarationMirror::LastSeenAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(DeclarationMirror::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await
    }
}

/// Adds the registry reference and its unique index.
///
/// **Drops and recreates rather than altering.** `SQLite` refuses
/// `ALTER TABLE ... ADD COLUMN ... NOT NULL` with no default even against an
/// empty table, and this gear runs the mirror on `SQLite` (e2e) as well as
/// Postgres (production), so an `ALTER` would pass one lane and fail the
/// other. The rows are discarded deliberately: the mirror is a recovery cache
/// rewritten on every successful registry read, so each meter's row returns
/// within one `type_cache_ttl_secs` of that meter next carrying traffic, and
/// a backfill would mean deriving the reference inside a migration — the one
/// thing `cpt-cf-types-registry-adr-storage-identity-query-model` bars this
/// gear from doing.
///
/// **`down` is not this migration's inverse.** It deliberately returns to "no
/// table" rather than recreating migration 1's original (reference-less)
/// shape, so rolling back `MigrationAddTypeUuid` alone, without also rolling
/// back migration 1, leaves migration 1 recorded as applied with no table —
/// safe only when the two are rolled back together.
pub struct MigrationAddTypeUuid;

impl MigrationName for MigrationAddTypeUuid {
    fn name(&self) -> &'static str {
        "m20261005_000002_declaration_mirror_type_uuid"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for MigrationAddTypeUuid {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(DeclarationMirror::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(DeclarationMirror::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(DeclarationMirror::GtsTypeId)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(DeclarationMirror::GtsTypeUuid)
                            .uuid()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DeclarationMirror::Document)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DeclarationMirror::FirstSeenAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(DeclarationMirror::LastSeenAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("uq_usage_collector__declaration_mirror_type_uuid")
                    .table(DeclarationMirror::Table)
                    .col(DeclarationMirror::GtsTypeUuid)
                    .unique()
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(DeclarationMirror::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await
    }
}
