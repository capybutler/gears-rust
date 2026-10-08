//! Injection-safe filter translation: a validated `FilterNode<F>` becomes a
//! parameterized `PostgreSQL` `WHERE` fragment plus an ordered bind list.
//!
//! Identifiers come only from the closed allowlist ([`record_column`]); every
//! value is bound as `$N` through
//! [`crate::infra::storage::query::bind::odata_value_to_bind`], never
//! interpolated.
//!
//! `convert_expr_to_filter_node::<F>` takes the `ast::Expr` a read path holds
//! (`parse_odata_filter::<F>(&str)` is the string entry point) and resolves each
//! identifier against `UsageRecordFilterField`, the SDK's filterable schema —
//! whose `name()`s are the snake-case fields of the `UsageRecordQuery` shape in
//! `usage-collector-sdk/src/models.rs`. The allowlist below covers those and
//! `accepted_at`, which is on no `UsageRecordFilterField` variant at all: it is
//! admissible only as an `$orderby` key, never a `$filter` field.
//!
//! `gts_type_uuid` needs no newtype here — it reaches these builders as a bare
//! [`Uuid`](uuid::Uuid) and binds as one.

use std::collections::BTreeSet;

use toolkit_odata::ast;
use toolkit_odata::filter::{
    FieldKind, FilterField, FilterNode, FilterOp, convert_expr_to_filter_node,
};
use usage_collector_sdk::UsageRecordFilterField;

use crate::infra::storage::record_store::ENTRY_TYPE_ENUM;

pub use super::bind::{SqlBind, bind_one, bind_one_query, odata_value_to_bind};
pub use toolkit_odata::filter::ODataValue;

/// Closed allowlist mapping a `usage_records` filter-field name to its column.
///
/// The map is the identity (field name == column name); the closed `match` is
/// the security boundary — only the identifiers it names can ever reach the SQL
/// string. `gts_type_uuid` is intentionally absent: it is a typed parameter on
/// the SPI, not a `$filter` field. The covered period is likewise not
/// filterable — it arrives as `time_range` — but its columns *are* mapped, for
/// the reason below.
///
/// The set is [`usage_collector_sdk::PUBLISHED_FILTER_FIELDS`] (transcribed
/// from `usage-collector-v1.yaml`'s `Filter` parameter description) plus `id`,
/// which the filterable schema carries so a caller can pin one entry and so the
/// canonical cursor tiebreaker resolves, plus `window_start` and `window_end`.
/// Those last two are reserved on `$filter` but sit in
/// [`usage_collector_sdk::KEYSET_SAFE_RECORD_FIELDS`], and `window_end` must
/// resolve for the canonical `(window_end, id)` keyset to render at all.
/// `accepted_at` is the odd one out: on neither the published `$filter` set nor
/// `UsageRecordFilterField`, admissible as an `$orderby` key only, so this is
/// its sole resolution site — reached through `render_order_by` and never
/// through the `$filter` translation path.
///
/// `entry_type` resolves to the ledger's written `usage_entry_type` column,
/// which is why the field is filterable here at all: the SDK stores no such
/// attribute and its value hook cannot carry one. It is the one column whose
/// comparison value needs a cast — see `bind_cast` below.
///
/// **This map also feeds `render_order_by`, where an `ORDER BY entry_type`
/// would sort by the enum's declaration order — but nothing can ask for one.**
/// `entry_type` is absent from both
/// [`usage_collector_sdk::KEYSET_SAFE_RECORD_FIELDS`] and
/// `usage-collector-v1.yaml`'s `RecordOrderKey`, on the enum's own ground: its
/// value is a function of the optional `invalidates`, so the storage contract
/// carries no such attribute in its own right. The gear refuses a caller
/// `$orderby` naming a field off that list with a `400` (`domain::query`'s
/// shared `keyset_defect` check, pinned by `usage_collector_sdk`'s
/// `models_tests::the_order_key_refusal_names_the_derived_ground_alongside_the_optional_one`),
/// so an order this function ever renders for `entry_type` is a host breach.
#[must_use]
pub fn record_column(field_name: &str) -> Option<&'static str> {
    // Declaration order of `UsageRecordQuery`, so this match can be checked
    // against the SDK side by side.
    match field_name {
        "id" => Some("id"),
        "window_start" => Some("window_start"),
        "window_end" => Some("window_end"),
        "tenant_id" => Some("tenant_id"),
        "resource_id" => Some("resource_id"),
        "resource_type" => Some("resource_type"),
        "subject_id" => Some("subject_id"),
        "subject_type" => Some("subject_type"),
        "invalidates" => Some("invalidates"),
        "entry_type" => Some("entry_type"),
        "origin" => Some("origin"),
        "accepted_at" => Some("accepted_at"),
        _ => None,
    }
}

/// Resolves an admissible `$orderby` field to the [`FieldKind`] its keyset
/// boundary value binds as — [`record_column`]'s sibling for
/// `keyset_predicate`'s `kind` parameter, which [`cursor_key_to_bind`](super::keyset::cursor_key_to_bind)
/// needs to know how to parse a cursor's boundary string back into a typed
/// bind.
///
/// **Not the same vocabulary as [`record_column`].** Most of
/// [`usage_collector_sdk::KEYSET_SAFE_RECORD_FIELDS`] are also
/// [`UsageRecordFilterField`] variants, so the fall-through resolves their kind
/// from the filterable schema. `accepted_at` is the exception: an admissible
/// order key with no `UsageRecordFilterField` variant at all (see
/// [`record_column`]'s doc), so the fall-through has no kind for it and
/// `keyset_predicate` would refuse every continuation ordered by it with
/// `"keyset field has no known kind"` — a 500 on a caller's second page after a
/// clean first one. That is this function's whole reason to exist rather than
/// being inlined at its one call site. `DateTimeUtc` is its kind for the same
/// reason `window_start` and `window_end` are: the keyset binds it back from
/// the RFC 3339 string `record_row_key` renders.
///
/// `every_keyset_safe_field_resolves_a_kind` iterates
/// `KEYSET_SAFE_RECORD_FIELDS` against this function, so a new keyset-safe
/// field without an arm here fails that test rather than waiting for a
/// continuation to 500 in production.
#[must_use]
pub fn record_field_kind(field_name: &str) -> Option<FieldKind> {
    match field_name {
        "accepted_at" => Some(FieldKind::DateTimeUtc),
        other => <UsageRecordFilterField as FilterField>::from_name(other).map(|f| f.kind()),
    }
}

/// Bind accumulator + placeholder counter for a single SQL statement.
///
/// `next` is the next `$N` index to emit; `binds` is the ordered list of values
/// to apply (via [`bind_one`]) in `$1, $2, …` order. Callers seed the start
/// index so a filter fragment can follow a bind applied outside the counter —
/// the point lookup's `id` at `$1` is the one such caller. The collection paths
/// seed at 1 and push their leading meter value through this counter too.
pub struct SqlCtx {
    next: usize,
    /// Accumulated binds in placeholder order. Crate-visible: read only by the
    /// in-crate record store and the query tests — never by an external
    /// consumer.
    pub(crate) binds: Vec<SqlBind>,
}

impl SqlCtx {
    /// Create a context whose first emitted placeholder is `$start`.
    #[must_use]
    pub fn new(start: usize) -> Self {
        Self {
            next: start,
            binds: Vec::new(),
        }
    }

    /// Append a bind and return the `$N` index it occupies. Crate-visible so
    /// the keyset helper accumulates binds in the same ordered context.
    pub(crate) fn push(&mut self, b: SqlBind) -> usize {
        let n = self.next;
        self.next += 1;
        self.binds.push(b);
        n
    }
}

/// The type a bound comparison value must be cast to before it can be compared
/// with `column`, or `None` where the bind's own type already serves.
///
/// Every value this translator binds reaches `PostgreSQL` as a [`SqlBind`],
/// and `entry_type` is the one allowlisted column no variant of that enum
/// types: the column is a `PostgreSQL` enum, the bind is `text`, and no
/// `usage_entry_type = text` operator exists, so an uncast comparison is
/// refused outright (`42883`). The cast goes on the **literal** rather than on
/// the column, per this plugin's DESIGN §3.7 — `$filter=entry_type eq
/// 'invalidation'` "compares against it directly, the literal casting to the
/// enum" — which also keeps [`record_column`]'s output the bare column name
/// every other part of this module assumes it is.
///
/// A value that is not one of the enum's labels is therefore rejected by
/// `PostgreSQL` (`22P02`) rather than selecting nothing, which is the cost of
/// comparing against the column directly. **At this layer the class that
/// reaches the caller is an internal error, not an invalid-argument**: `22P02`
/// is neither `23505` nor transient, so
/// [`super::super::error::classify_db`] leaves it in `Other` and
/// [`super::super::error::map_sqlx_err`] returns `Internal("database error")`.
/// That is pinned at the error-class level by
/// `a_non_label_entry_type_filter_still_answers_an_internal_error` in
/// `tests/records_query_integration_pg.rs`; the cast mechanism itself — that
/// the literal, and only the literal on `entry_type`, carries the cast — is
/// pinned by the `--lib` unit tests in `translate_tests.rs`
/// (`only_the_enum_column_casts_its_bound_literal` among them).
///
/// It is not this module's defect to fix: `UsageCollectorPluginError` has no
/// invalid-argument variant, so a guard added here would still surface as
/// `Internal`. A misspelled literal in a well-formed request is refused one
/// layer up by `usage-collector`'s `domain::query::reject_off_label_literals`,
/// which checks it against
/// [`EntryType::wire_labels`](usage_collector_sdk::EntryType::wire_labels) /
/// [`RecordOrigin::wire_labels`](usage_collector_sdk::RecordOrigin::wire_labels)
/// before this function's cast is reached. A non-conforming host that skipped
/// that gate still gets this `Internal`.
///
/// Keyed on the resolved column rather than the field name, so it is the
/// spelling that actually reaches the SQL that decides. The type name is
/// [`ENTRY_TYPE_ENUM`], not a second spelling of it.
fn bind_cast(column: &str) -> Option<&'static str> {
    match column {
        "entry_type" => Some(ENTRY_TYPE_ENUM),
        _ => None,
    }
}

/// [`bind_cast`] rendered as the SQL suffix a placeholder carries, or empty.
fn bind_cast_suffix(column: &str) -> String {
    bind_cast(column).map_or_else(String::new, |ty| format!("::{ty}"))
}

/// Map a comparison [`FilterOp`] to its SQL operator.
///
/// # Errors
///
/// Returns an error string for non-comparison operators (`In` / `Contains` /
/// `StartsWith` / `EndsWith` / `And` / `Or`): the SPI filter fields are
/// exact-match, so `LIKE`-family operators are out of scope, and the composite
/// / membership operators are handled structurally by the translators.
fn op_sql(op: FilterOp) -> Result<&'static str, String> {
    match op {
        FilterOp::Eq => Ok("="),
        FilterOp::Ne => Ok("<>"),
        FilterOp::Gt => Ok(">"),
        FilterOp::Ge => Ok(">="),
        FilterOp::Lt => Ok("<"),
        FilterOp::Le => Ok("<="),
        other => Err(format!("unsupported operator: {other:?}")),
    }
}

/// Translate a compiled PDP scope into a **self-delimiting** parameterized
/// `WHERE` fragment, pushing each value onto `ctx` as a bind.
///
/// This is the whole road from the `ast::Expr` a read path is handed to the SQL
/// it may conjoin, and every read path takes it: the point lookup with the
/// scope alone, `list` and `aggregate` with the gateway's composition of that
/// scope and the caller's `$filter`. One copy, because a transcription per path
/// would be a chance per path to get the security boundary wrong.
///
/// **Two gates, in this order.** [`convert_expr_to_filter_node`] resolves each
/// identifier against [`UsageRecordFilterField`], the SDK's filterable schema,
/// and [`translate_record_filter`] resolves it again against the closed
/// [`record_column`] allowlist and binds every value as `$N`. No identifier
/// reaches the SQL string from caller input either way.
///
/// **The result is parenthesized**, which is why the return is a fragment
/// rather than a clause vector. Callers conjoin it, and `AND` binds tighter
/// than `OR`, so an unparenthesized `A OR B` conjoined after a predicate `P`
/// would read as `(P AND A) OR B`: every row matching `B`, whatever `P` said. A
/// multi-constraint grant compiles to exactly that shape through
/// `authz::scope_to_odata_filter`, and nothing downstream can tell how many
/// constraints the PDP returned, so the wrap is unconditional.
///
/// # Errors
///
/// Returns `invalid read predicate: …` when either gate refuses — an identifier
/// off the schema or off the allowlist, an operator SQL cannot express, an
/// empty `IN` list, or a value that cannot be bound. The prefix names neither
/// half on purpose: on the collection paths the two are composed into one
/// expression before this function sees it.
///
/// **A caller must propagate it.** A scope that fails to translate and is
/// dropped leaves the read unscoped, turning a translation failure into an
/// authorization bypass; there is deliberately no "renders to nothing" success
/// to drop. This is also **not atomic on failure** — a partially-walked
/// `Composite` has already pushed its binds onto `ctx`, so `ctx` is only usable
/// by a caller that abandons the whole statement.
pub fn translate_scope(scope: &ast::Expr, ctx: &mut SqlCtx) -> Result<String, String> {
    let node = convert_expr_to_filter_node::<UsageRecordFilterField>(scope)
        .map_err(|e| format!("invalid read predicate: {e}"))?;
    let fragment =
        translate_record_filter(&node, ctx).map_err(|e| format!("invalid read predicate: {e}"))?;
    Ok(format!("({fragment})"))
}

/// Every filter-field name `expr` references, resolved through the same
/// schema gate [`translate_scope`] applies first.
///
/// The aggregate path uses it to decide whether a query's predicate can be
/// applied to rollup rows, which it can only when every name is a rollup grain
/// column. It is the whole of that decision's view of the filter, so it walks
/// every node kind, including under `not`.
///
/// # Errors
///
/// Returns `invalid read predicate: …` when an identifier is off the schema,
/// the same refusal [`translate_scope`] would give.
pub fn filter_fields(expr: &ast::Expr) -> Result<BTreeSet<&'static str>, String> {
    let node = convert_expr_to_filter_node::<UsageRecordFilterField>(expr)
        .map_err(|e| format!("invalid read predicate: {e}"))?;
    let mut names = BTreeSet::new();
    collect_filter_fields(&node, &mut names);
    Ok(names)
}

fn collect_filter_fields<F: FilterField>(node: &FilterNode<F>, out: &mut BTreeSet<&'static str>) {
    match node {
        FilterNode::Binary { field, .. } | FilterNode::InList { field, .. } => {
            out.insert(field.name());
        }
        FilterNode::Composite { children, .. } => {
            for child in children {
                collect_filter_fields(child, out);
            }
        }
        FilterNode::Not(inner) => collect_filter_fields(inner, out),
    }
}

/// Translate a `usage_records` filter node into a parameterized `WHERE`
/// fragment, pushing each value onto `ctx` as a bind.
///
/// Identifiers resolve through [`record_column`]; an unmapped field is an
/// error (never interpolated). Values resolve through
/// [`odata_value_to_bind`].
///
/// **Returns the walker's fragment as-is** — parenthesized only for a
/// `Composite`. To translate a compiled scope, or a composed `$filter`, for
/// conjoining with another predicate, use [`translate_scope`]: a bare `A OR B`
/// pushed into a `clauses.join(" AND ")` reads as `(P AND A) OR B`.
///
/// # Errors
///
/// Returns an error string when a field is not on the allowlist, an operator is
/// unsupported, a composite carries a non-`And`/`Or` operator, or a value
/// cannot be converted to a bind.
pub fn translate_record_filter<F: FilterField>(
    node: &FilterNode<F>,
    ctx: &mut SqlCtx,
) -> Result<String, String> {
    translate_filter(node, ctx, record_column)
}

/// Recursive walker over the filter AST. No identifier reaches the SQL from
/// caller input: every column name is resolved through `col`, a closed
/// allowlist, and every value is bound as `$N`, so injection safety holds for
/// any AST shape this walks.
fn translate_filter<F: FilterField>(
    node: &FilterNode<F>,
    ctx: &mut SqlCtx,
    col: fn(&str) -> Option<&'static str>,
) -> Result<String, String> {
    match node {
        FilterNode::Binary { field, op, value } => {
            let column = col(field.name())
                .ok_or_else(|| format!("field not allowlisted: {}", field.name()))?;
            let operator = op_sql(*op)?;
            let cast = bind_cast_suffix(column);
            let n = ctx.push(odata_value_to_bind(value)?);
            Ok(format!("{column} {operator} ${n}{cast}"))
        }
        FilterNode::InList { field, values } => {
            let column = col(field.name())
                .ok_or_else(|| format!("field not allowlisted: {}", field.name()))?;
            if values.is_empty() {
                return Err("IN list must not be empty".to_owned());
            }
            let cast = bind_cast_suffix(column);
            let placeholders = values
                .iter()
                .map(|v| Ok(format!("${}{cast}", ctx.push(odata_value_to_bind(v)?))))
                .collect::<Result<Vec<_>, String>>()?;
            Ok(format!("{column} IN ({})", placeholders.join(", ")))
        }
        FilterNode::Composite { op, children } => {
            let joiner = match op {
                FilterOp::And => " AND ",
                FilterOp::Or => " OR ",
                other => return Err(format!("invalid composite operator: {other:?}")),
            };
            let parts = children
                .iter()
                .map(|child| translate_filter(child, ctx, col))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("({})", parts.join(joiner)))
        }
        FilterNode::Not(inner) => Ok(format!("NOT ({})", translate_filter(inner, ctx, col)?)),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "translate_tests.rs"]
mod translate_tests;
