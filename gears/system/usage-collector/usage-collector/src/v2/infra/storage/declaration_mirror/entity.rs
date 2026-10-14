//! The declaration mirror's sea-orm model — the authoritative naming of the
//! mirror table every production read and write resolves through. Every
//! production naming of the table is inside
//! `infra/storage/declaration_mirror/`, so its removal is a single deletion.
//!
//! Temporary, with the rest of this module:
//! `cpt-cf-usage-collector-adr-declaration-rehydration` statement 7 deletes
//! the mirror when `types-registry` gains persistent storage on this gear's
//! resolution path, and this directory is that deletion.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

/// One row per resolved meter.
///
/// **`#[secure(unrestricted)]`, deliberately.** Declarations are
/// platform-global, so the table is not tenant-scoped (DESIGN §3.7). There is
/// no caller on this path and no principal to scope against — the Type
/// Resolver writes it from a registry read, not from a request — so there is
/// no row-level filter to enforce, ever.
///
/// `document` is **text**, not a JSON column, mirroring types-registry's own
/// `raw_schema: String`: it keeps the mirror engine-agnostic across the
/// `sqlite` and `pg` features without a second opinion about JSON column
/// support.
// @cpt-dod:cpt-cf-usage-collector-dod-declaration-mirror-restore:p1
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "usage_collector__declaration_mirror")]
#[secure(unrestricted)]
pub struct Model {
    /// The meter's `gts_type_id`. The whole key: declarations are
    /// platform-global.
    #[sea_orm(primary_key, auto_increment = false)]
    pub gts_type_id: String,
    /// The registry reference `types-registry` issued for this meter.
    ///
    /// Set on insert and never on conflict-update: the value is a function of
    /// the primary key, so a write that moved it would be describing a
    /// different meter.
    pub gts_type_uuid: Uuid,
    /// The declaration document as `types-registry` returned it.
    pub document: String,
    /// Set once, by the upsert's shape — `ON CONFLICT ... DO UPDATE` never
    /// names this column.
    pub first_seen_at: OffsetDateTime,
    /// Moved on every successful registry read, cold miss and refresh alike.
    ///
    /// **Not moved by a restore**, though a restore does re-fetch from the
    /// registry: that re-fetch returns the document the row already holds, so
    /// stamping this column would report a replay as a fresh confirmation and
    /// hide how long the type has been absent upstream. The omission is
    /// structural — `restore_document` never calls `mirror_document`, so no
    /// write is attempted on that path.
    pub last_seen_at: OffsetDateTime,
}

/// No relations: no entry references the mirror and it enforces no
/// referential integrity (DESIGN §3.7).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
