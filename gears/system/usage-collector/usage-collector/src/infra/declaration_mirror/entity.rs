//! The declaration mirror's sea-orm model — **the authoritative naming of
//! the mirror table: the one every production read and write resolves
//! through.**
//!
//! The production code sites that name the table are two `sea_orm` attributes,
//! both inside this directory: this file's `#[sea_orm(table_name = ..)]` below
//! and `migrations.rs`'s `#[sea_orm(iden = ..)]`. The only other naming is
//! `TABLE` in `tests/postgres_declaration_mirror.rs`, this bridge's own
//! integration test, which the same deletion takes.
//!
//! `cpt-cf-usage-collector-dod-declaration-mirror-restore` asks that the table be
//! "reachable from one place in the code, so its removal is a single deletion",
//! which holds at **module** granularity: every production naming is inside
//! `infra/declaration_mirror/`.
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
/// **`#[secure(unrestricted)]`, deliberately.** DESIGN §3.7: *"Declarations
/// are platform-global, so it is not tenant-scoped, and it holds one row per
/// meter rather than per entry."* There is no caller on this path and no
/// principal to scope against — the Type Resolver writes it from a registry
/// read, not from a request — so there is no row-level filter to enforce.
///
/// The same annotation `types-registry`'s own `types_registry__entity` model
/// carries, but **not for the same reason**: that one records a
/// platform-maturity ceiling with a stated upgrade path. This model's reason is
/// structural and does not retire on that upgrade — there is no caller on this
/// path at all, ever, so there is no principal for any PDP to decide about.
///
/// `document` is **text**, not a JSON column: types-registry stores the same
/// payload as `raw_schema: String`, documented *"The authored document as
/// submitted, canonical UTF-8 text"*. Copying that decision keeps the mirror
/// engine-agnostic across the `sqlite` and `pg` features without a second
/// opinion about JSON column support.
// @cpt-dod:cpt-cf-usage-collector-dod-declaration-mirror-restore:p1
//
// `Model` realizes the table-shape clause (one row per resolved meter, carrying
// the identifier, the document as registered, and first-seen/last-seen
// timestamps) and this module's share of single-place reachability. The
// behavioural clauses — rewrite-on-every-successful-read,
// restore-only-on-definite-not-found, a failed write counted and non-rejecting,
// a failed restore rejecting — are `DbDeclarationMirror`'s (`upsert`/`read`) and
// the Type Resolver's.
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
    /// Set on insert and never on conflict-update, the same discipline
    /// `first_seen_at` relies on and for a stronger reason: the value is a
    /// function of the primary key, so a write that moved it would be
    /// describing a different meter.
    pub gts_type_uuid: Uuid,
    /// The declaration document as `types-registry` returned it.
    pub document: String,
    /// Set once, by the upsert's shape — `ON CONFLICT ... DO UPDATE` never
    /// names this column, so no branch can set it twice and no race can
    /// reorder a read-then-write.
    pub first_seen_at: OffsetDateTime,
    /// Moved on every successful registry read, cold miss and refresh alike.
    ///
    /// **Not moved by a restore — though a restore does perform a registry
    /// read**: `TypeResolver::restore_document` re-fetches after registering the
    /// document back, because a locally rebuilt `GtsTypeSchema` cannot carry the
    /// inheritance chain `effective_traits` walks. What that re-fetch returns is
    /// the document the row **already holds**, the row's own document being what
    /// the restore just registered. `document` is therefore unchanged and
    /// `first_seen_at` never updatable, so this column is the only one a write
    /// could move — and ADR statement 2 ties it to a registry read that *changes*
    /// what the registry knows. Stamping it on a replay would report it as a
    /// fresh confirmation and hide how long the type has been absent upstream.
    ///
    /// The omission is structural rather than a run-time comparison:
    /// `restore_document` never calls `mirror_document`, so no write is
    /// attempted on that path. Pinned by
    /// `resolver_tests::a_successful_restore_writes_nothing_to_the_mirror`.
    pub last_seen_at: OffsetDateTime,
}

/// No relations: *"No entry references it and it enforces no referential
/// integrity"* (DESIGN §3.7).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
