//! Injection-safe filter translation: a validated `FilterNode<F>` becomes a
//! parameterized `PostgreSQL` `WHERE` fragment plus an ordered bind list.
//!
//! Identifiers come only from the closed allowlist ([`record_column`]); every
//! value is bound as `$N` through
//! [`crate::infra::storage::query::bind::odata_value_to_bind`], never
//! interpolated.
//!
//! # Verified `toolkit-odata` / SDK API (Task E1)
//!
//! - Converter: `toolkit_odata::filter::convert_expr_to_filter_node::<F>(expr:
//!   &toolkit_odata::ast::Expr) -> Result<FilterNode<F>, FilterError>` — takes
//!   `&Expr` (the AST node, e.g. from `ODataQuery::filter()`), NOT
//!   `&ODataQuery`. (`parse_odata_filter::<F>(&str)` is the string entry point.)
//! - `FilterNode<F>` variants: `Binary { field: F, op: FilterOp, value:
//!   ODataValue }`, `InList { field: F, values: Vec<ODataValue> }`, `Composite
//!   { op: FilterOp, children: Vec<FilterNode<F>> }`, `Not(Box<FilterNode<F>>)`.
//! - `FilterOp` variants: `Eq, Ne, Gt, Ge, Lt, Le, In, Contains, StartsWith,
//!   EndsWith, And, Or`.
//! - `FilterField` (`toolkit_odata::filter::FilterField`): `const FIELDS:
//!   &'static [Self]`, `fn name(&self) -> &'static str`, `fn kind(&self) ->
//!   FieldKind`, `fn from_name(name: &str) -> Option<Self>`. `name()` returns
//!   the macro field's snake-case name — for `UsageRecordFilterField` (the
//!   `UsageRecordQuery` shape in `usage-collector-sdk/src/models.rs`) those are
//!   exactly `"id"`, `"window_start"`, `"window_end"`, `"tenant_id"`,
//!   `"resource_id"`, `"resource_type"`, `"subject_id"`, `"subject_type"`,
//!   `"invalidates"`, `"entry_type"`, `"origin"`. The identity column
//!   allowlist below relies on that, and covers all eleven.
//! - `ODataValue` path: `toolkit_odata::filter::ODataValue` is a `pub use` of
//!   `toolkit_odata::ast::Value`. Variants: `Null`, `Bool(bool)`,
//!   `Number(bigdecimal::BigDecimal)`, `Uuid(uuid::Uuid)`,
//!   `DateTime(chrono::DateTime<chrono::Utc>)`, `Date(chrono::NaiveDate)`,
//!   `Time(chrono::NaiveTime)`, `String(String)`.
//! - `UsageRecordFilterField` is an SDK re-export
//!   (`UsageRecordQueryFilterField`, `#[derive(ODataFilterable)]`-generated).
//!   Tests build it via
//!   `<UsageRecordFilterField as FilterField>::from_name("entry_type")`.
//! - `MeterTypeId`: `new(impl Into<String>) -> Result<Self,
//!   UsageCollectorError>` (validated); reads back via `as_str()`, or through
//!   its `AsRef<str>`, which forwards to it. It replaced the retired
//!   `UsageTypeGtsId` this list used to name.
//!   `ResourceRef::new(resource_id, resource_type) -> Result<_,
//!   _>`; `SubjectRef::new(subject_id, Option<subject_type>) -> Result<_, _>`;
//!   `MetadataKey::new(impl Into<String>) -> Result<_, _>`;
//!   `IdempotencyKey::new(impl Into<String>) -> Result<_, _>`.

use toolkit_odata::ast;
use toolkit_odata::filter::{FilterField, FilterNode, FilterOp, convert_expr_to_filter_node};
use usage_collector_sdk::UsageRecordFilterField;

pub use super::bind::{SqlBind, bind_one, bind_one_query, odata_value_to_bind};
pub use toolkit_odata::filter::ODataValue;

/// Closed allowlist mapping a `usage_records` filter-field name to its column.
///
/// The map is the identity (field name == column name); the closed `match` is
/// the security boundary — only these eleven identifiers can ever reach the SQL
/// string. `gts_type_id` is intentionally absent: it is a typed parameter on
/// the SPI, not a `$filter` field. The covered period is likewise not
/// filterable — it arrives as `time_range` — but its columns *are* mapped, for
/// the reason below.
///
/// The set is the published eight (`usage-collector-v1.yaml:440`) plus `id`,
/// which the filterable schema carries so a caller can pin one entry and so the
/// canonical cursor tiebreaker resolves, plus `window_start` and `window_end`.
/// Those last two are reserved on `$filter` but sit in
/// [`usage_collector_sdk::KEYSET_SAFE_RECORD_FIELDS`], and `window_end` must
/// resolve for the canonical `(window_end, id)` keyset to render at all.
///
/// `entry_type` resolves to the stored generated column
/// (`CASE WHEN invalidates IS NULL THEN 'record' ELSE 'invalidation' END`),
/// which is why the field is filterable here at all: the SDK stores no such
/// attribute and its value hook cannot carry one.
#[must_use]
pub fn record_column(field_name: &str) -> Option<&'static str> {
    // Declaration order of `UsageRecordQuery`, so the module doc's list above
    // and this match can be checked against the SDK side by side.
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
        _ => None,
    }
}

/// Bind accumulator + placeholder counter for a single SQL statement.
///
/// `next` is the next `$N` index to emit; `binds` is the ordered list of
/// values to apply (via [`bind_one`]) in `$1, $2, …` order. Callers seed the
/// start index so a filter fragment can follow leading binds (e.g. a `gts_id`
/// bound at `$1`).
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
/// scope and the caller's `$filter` (the Query Gateway composes them "so the
/// result can only narrow", so what arrives is one expression). Three
/// transcriptions of these four lines would be three chances to get the
/// security boundary wrong in different ways.
///
/// **Two gates, in this order.** [`convert_expr_to_filter_node`] resolves each
/// identifier against [`UsageRecordFilterField`], the SDK's filterable schema —
/// fixing the vocabulary a scope may name in one place — and
/// [`translate_record_filter`] resolves it again against the closed
/// [`record_column`] allowlist and binds every value as `$N`. No identifier
/// reaches the SQL string from caller input either way.
///
/// **The result is parenthesized, and that is the point of the return being a
/// fragment rather than a clause vector.** Callers conjoin it — the point
/// lookup after `id = $1`, the collection paths inside a `clauses.join(" AND
/// ")` — and `AND` binds tighter than `OR`, so an unparenthesized `A OR B`
/// conjoined after another predicate `P` reads as `(P AND A) OR B`: every row
/// matching `B`, whatever `P` said. A multi-constraint grant compiles to
/// exactly that shape through the host gear's
/// `authz::scope_to_odata_filter` — a left-nested `or` chain of tenant-pinned
/// conjunctions — and nothing downstream can tell how many constraints the PDP
/// returned, so the wrap is unconditional. The recursive walker below already
/// parenthesizes a `Composite`, and `composite_or_joins_children_with_or_inside_parens`
/// pins that; the wrap here means a caller never has to know it, and never has
/// to re-check it when the translator grows a node kind.
///
/// # Errors
///
/// Returns `invalid read predicate: …` when either gate refuses — an identifier
/// off the schema or off the allowlist, an operator SQL cannot express, an
/// empty `IN` list, or a value that cannot be bound. The prefix names neither
/// half on purpose: on the collection paths the two are composed into one
/// expression before this function sees it, so which of them is malformed is
/// not knowable at this layer.
///
/// **A caller must propagate it.** A scope that fails to translate and is
/// dropped instead leaves the read unscoped, which turns a translation failure
/// into an authorization bypass; there is deliberately no "renders to nothing"
/// success here to drop. Nor is recovering and continuing without the scope
/// sound even if it were permitted: this is **not atomic on failure** — a
/// partially-walked `Composite` has already pushed its binds onto `ctx`, so
/// `ctx` is only usable by a caller that abandons the whole statement.
pub fn translate_scope(scope: &ast::Expr, ctx: &mut SqlCtx) -> Result<String, String> {
    let node = convert_expr_to_filter_node::<UsageRecordFilterField>(scope)
        .map_err(|e| format!("invalid read predicate: {e}"))?;
    let fragment =
        translate_record_filter(&node, ctx).map_err(|e| format!("invalid read predicate: {e}"))?;
    Ok(format!("({fragment})"))
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
            let n = ctx.push(odata_value_to_bind(value)?);
            Ok(format!("{column} {operator} ${n}"))
        }
        FilterNode::InList { field, values } => {
            let column = col(field.name())
                .ok_or_else(|| format!("field not allowlisted: {}", field.name()))?;
            if values.is_empty() {
                return Err("IN list must not be empty".to_owned());
            }
            let placeholders = values
                .iter()
                .map(|v| Ok(format!("${}", ctx.push(odata_value_to_bind(v)?))))
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
