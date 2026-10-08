//! The declaration mirror's storage adapter, entity and migration.
//!
//! **This module is the whole of the mirror's footprint in the gear, and it is
//! temporary.** `cpt-cf-usage-collector-adr-declaration-rehydration` statement 7
//! retires the mirror and the restore when `types-registry` gains persistent
//! storage on this gear's resolution path, and
//! `cpt-cf-usage-collector-dod-declaration-mirror-restore` requires the table to
//! be *"reachable from one place in the code, so its removal is a single
//! deletion"*. Deleting this directory, the temporary ports it implements, and
//! `TypeResolver::rehydrate` is that removal.
//!
//! The mirror carries one unique index beyond its primary key, on the
//! registry reference, because `MeterReverseResolver` reads the reverse
//! direction. See `migrations.rs`'s module doc for why that index is unique.
//! Do not add a third speculatively.
//
// @cpt-dod:cpt-cf-usage-collector-dod-declaration-mirror-restore:p1
//
// This module realizes the DoD's SINGLE-PLACE-REACHABILITY clause — "The table
// MUST be reachable from one place in the code, so its removal is a single
// deletion when the registry gains persistent storage" — the one clause no
// single struct or method can carry, being a claim about where the table is
// *not* named. Every production naming is inside this directory
// (`entity.rs`'s `table_name`, `migrations.rs`'s `iden`); see `entity.rs`'s
// module doc.
//
// The other clauses are marked where they are realized: the table's shape on
// `entity::Model`, the row write and read on `DbDeclarationMirror::upsert` /
// `::read` below, and the behavioural ones in `TypeResolver::rehydrate` and its
// helpers.

mod entity;
mod migrations;

use async_trait::async_trait;
use sea_orm::{ActiveValue, ColumnTrait, EntityTrait, QueryFilter};
use sea_orm_migration::MigrationTrait;
use serde_json::Value;
use time::OffsetDateTime;
use toolkit_db::secure::{SecureEntityExt, SecureInsertExt, SecureOnConflict};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::AccessScope;
use usage_collector_sdk::MeterTypeId;
use uuid::Uuid;

use crate::domain::ports::declaration_mirror::{DeclarationMirror, MirrorError};

/// The mirror's migrations, for `Gear::migrations`.
#[must_use]
pub fn migrations() -> Vec<Box<dyn MigrationTrait>> {
    vec![
        Box::new(migrations::Migration),
        Box::new(migrations::MigrationAddTypeUuid),
    ]
}

/// Reads and writes the declaration mirror over a `toolkit-db` provider.
pub struct DbDeclarationMirror {
    db: DBProvider<DbError>,
}

impl DbDeclarationMirror {
    /// Creates an adapter over `db`.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// The scope every mirror statement runs under.
    ///
    /// `allow_all` is correct here rather than permissive: the entity is
    /// `#[secure(unrestricted)]` because declarations are platform-global
    /// (DESIGN §3.7), and there is **no caller** on this path to scope against —
    /// the Type Resolver writes from a registry read, not from a request.
    /// `AccessScope::allow_all`'s own doc states the distinction: *"This
    /// represents a legitimate PDP decision with no row-level filtering. Not a
    /// bypass — it's a valid authorization outcome."*
    ///
    /// `types-registry`'s `domain/registry_service.rs::scope()` makes the
    /// identical call for a different reason — a maturity-ceiling posture
    /// expected to narrow later. This call site's reason does not narrow: there
    /// is no caller on this path under any maturity level.
    fn scope() -> AccessScope {
        AccessScope::allow_all()
    }
}

#[async_trait]
impl DeclarationMirror for DbDeclarationMirror {
    // @cpt-algo:cpt-cf-usage-collector-algo-mirror-and-restore:p1
    //
    // This method realizes mirror-mode's write step alone (`inst-mirror-write`
    // — "TRY upsert the row ... setting first-seen once and last-seen on every
    // write"). Counting a write failure and swallowing it rather than rejecting
    // the operation (`inst-mirror-catch` / `inst-mirror-swallow`) is the Type
    // Resolver's job against this method's `Err`.
    async fn upsert(
        &self,
        id: &MeterTypeId,
        type_uuid: Uuid,
        document: &Value,
    ) -> Result<(), MirrorError> {
        let now = OffsetDateTime::now_utc();
        let row = entity::ActiveModel {
            gts_type_id: ActiveValue::Set(id.as_str().to_owned()),
            gts_type_uuid: ActiveValue::Set(type_uuid),
            document: ActiveValue::Set(document.to_string()),
            first_seen_at: ActiveValue::Set(now),
            last_seen_at: ActiveValue::Set(now),
        };

        // ONE statement, so "first-seen once, last-seen on every write" is
        // structural rather than a branch: `DO UPDATE` names `document` and
        // `last_seen_at` and NEVER `first_seen_at`, so the inserted value for
        // that column survives every later write. A read-then-branch
        // implementation would have a race this does not.
        let mut on_conflict =
            SecureOnConflict::<entity::Entity>::columns([entity::Column::GtsTypeId]);
        on_conflict
            .inner_mut()
            .update_columns([entity::Column::Document, entity::Column::LastSeenAt]);

        let scope = Self::scope();
        let conn = self
            .db
            .conn()
            .map_err(|e| MirrorError::new(format!("mirror connection unavailable: {e}")))?;

        entity::Entity::insert(row)
            .secure()
            .scope_unchecked(&scope)
            .map_err(|e| MirrorError::new(format!("mirror scope rejected the write: {e}")))?
            .on_conflict(on_conflict)
            .exec(&conn)
            .await
            .map_err(|e| MirrorError::new(format!("mirror write failed: {e}")))?;

        Ok(())
    }

    // @cpt-algo:cpt-cf-usage-collector-algo-mirror-and-restore:p1
    //
    // This method realizes restore-mode's row-read step alone
    // (`inst-restore-read` — "Read the row for the identifier. Where none
    // exists, RETURN no declaration"). Every other restore-mode step —
    // `inst-restore-definite`, `inst-restore-no-lifecycle`,
    // `inst-restore-register`, `inst-restore-catch` / `inst-restore-fail` —
    // lands in `TypeResolver::rehydrate` / `with_rehydration`.
    async fn read(&self, id: &MeterTypeId) -> Result<Option<Value>, MirrorError> {
        let scope = Self::scope();
        let conn = self
            .db
            .conn()
            .map_err(|e| MirrorError::new(format!("mirror connection unavailable: {e}")))?;

        let row = entity::Entity::find_by_id(id.as_str().to_owned())
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await
            .map_err(|e| MirrorError::new(format!("mirror read failed: {e}")))?;

        match row {
            None => Ok(None),
            Some(row) => serde_json::from_str(&row.document)
                .map(Some)
                // Named rather than swallowed: a row whose text will not
                // parse is a defect an operator has to find, and the caller
                // cannot tell it from an absent row unless this says so.
                .map_err(|e| {
                    MirrorError::new(format!("mirrored document for {id} is not valid JSON: {e}"))
                }),
        }
    }

    async fn resolve_id(&self, type_uuid: Uuid) -> Result<Option<MeterTypeId>, MirrorError> {
        let scope = Self::scope();
        let conn = self
            .db
            .conn()
            .map_err(|e| MirrorError::new(format!("mirror connection unavailable: {e}")))?;

        let row = entity::Entity::find()
            .filter(entity::Column::GtsTypeUuid.eq(type_uuid))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await
            .map_err(|e| MirrorError::new(format!("mirror reverse read failed: {e}")))?;

        match row {
            None => Ok(None),
            Some(row) => {
                let gts_type_id = row.gts_type_id;
                MeterTypeId::new(gts_type_id.as_str())
                    .map(Some)
                    .map_err(|e| {
                        MirrorError::new(format!(
                            "mirrored identifier `{gts_type_id}` for reference {type_uuid} is \
                             not a valid meter type id: {e}"
                        ))
                    })
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "mirror_tests.rs"]
mod mirror_tests;
