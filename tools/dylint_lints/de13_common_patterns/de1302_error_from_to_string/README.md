# DE1302 — No `.to_string()` in Error Conversion Impls

## Rule

This lint flags `.to_string()` calls inside `fn from()` (or `fn try_from()`)
bodies when they appear in:

- `impl From<X> for Y`
- `impl TryFrom<X> for Y`

where the source type `X`, the target type `Y`, or the `TryFrom::Error`
associated type implements `std::error::Error`. Both syntactic forms are
caught:

- Method-call form: `e.to_string()`
- UFCS form: `ToString::to_string(&e)` and `<E as ToString>::to_string(&e)`

Closure bodies are also walked (e.g. `.map(|e| e.to_string())` inside a From
body).

## Rationale

`From` impls on error types exist primarily to power the `?` operator: when
a function returns `Result<T, AppError>` and the caller writes
`db_query()?`, Rust desugars that to `db_query().map_err(AppError::from)`.
Whatever your `From<DatabaseError> for AppError` does is what every `?` in
the codebase will do.

Calling `e.to_string()` on an error inside that conversion converts the
error to a `String` and **discards the original**. The result is a new error
that:

- Has no `.source()` — the chain is broken; callers can't follow back to the
  root cause.
- Cannot be `.downcast_ref::<ConcreteErr>()` to recover the underlying type.
- Loses structured metadata (error codes, fields, retry hints, etc.).
- Is missing the information `tracing`, alerting, and bug-report tooling
  rely on.

For most conversions, you have better options that preserve the chain:

- **`thiserror`'s `#[from]`** auto-derives a `From` impl that stores the
  source error directly. The variant's first field becomes the source.
- **`#[error(transparent)]`** delegates `Display` / `source()` to a wrapped
  inner error — the variant disappears from messages but is still reachable
  via `.source()`.
- **`#[source]`** marks a field as the chain source without auto-generating
  the `From` impl. Use this when you want a custom variant constructor
  (`Internal { msg: String, #[source] source: SomeError }`) but still want
  `.source()` to work.
- **Box the source**: `Internal(Box<dyn std::error::Error + Send + Sync + 'static>)`
  with a manual `From` that calls `.into()` (no stringification). The
  `Send + Sync + 'static` bound is what async runtimes (tokio, async-std)
  and error-reporting libraries need to move errors across tasks.
- **Match-and-forward**: pattern-match the source variants and map them to
  shape-preserving target variants.

## Gating

The lint is type-driven, not name-based. It only walks a body when **at
least one of**:

- `source_ty` implements `std::error::Error`, **or**
- `target_ty` implements `std::error::Error`, **or**
- (TryFrom only) the `type Error` associated type implements
  `std::error::Error`.

Inside the body, `.to_string()` is only flagged when:

- The receiver type equals the source parameter type **and** the source type
  implements `Error`, **or**
- The receiver type equals the `TryFrom::Error` associated type.

Any other receiver — `&str`, `String`, `Uuid`, an unrelated error fetched for
logging — is left alone. This prevents false positives like `impl From<u32>
for MyErr` flagging `n.to_string()`.

## Examples

### Bad — chain destroyed

```rust
impl From<DatabaseError> for AppError {
    fn from(e: DatabaseError) -> Self {
        AppError::Internal(e.to_string()) // chain lost
    }
}
```

```rust
impl TryFrom<DatabaseError> for AppError {
    type Error = ConversionError;

    fn try_from(e: DatabaseError) -> Result<Self, Self::Error> {
        Ok(AppError::Internal(e.to_string())) // chain lost
    }
}
```

```rust
// UFCS form — same problem.
impl From<DatabaseError> for AppError {
    fn from(e: DatabaseError) -> Self {
        AppError::Internal(ToString::to_string(&e))
    }
}
```

```rust
// Inside a closure inside a From body — also caught.
impl From<DatabaseError> for AppError {
    fn from(e: DatabaseError) -> Self {
        let render = |x: &DatabaseError| x.to_string();
        AppError::Internal(render(&e))
    }
}
```

### Good — chain preserved

```rust
// thiserror #[from] — the cleanest path.
#[derive(thiserror::Error, Debug)]
enum AppError {
    #[error(transparent)]
    Database(#[from] DatabaseError),
    #[error("internal: {0}")]
    Internal(String),
}
```

```rust
// Manual From that stores the source directly.
impl From<DatabaseError> for AppError {
    fn from(e: DatabaseError) -> Self {
        AppError::Database(e) // chain preserved via #[error(transparent)] / source()
    }
}
```

```rust
// Custom variant shape with `#[source]` — when `#[from]` doesn't fit but you
// still want `.source()` to walk into the underlying error.
#[derive(thiserror::Error, Debug)]
enum AppError {
    #[error("internal: {msg}")]
    Internal {
        msg: String,
        #[source]
        source: DatabaseError,
    },
}

impl From<DatabaseError> for AppError {
    fn from(e: DatabaseError) -> Self {
        AppError::Internal {
            msg: "database operation failed".into(),
            source: e,
        }
    }
}
```

```rust
// Boxed source variant — keeps a single Internal bucket while preserving
// `.source()`. anyhow::Error already implements Into<Box<dyn Error + Send + Sync>>.
#[derive(thiserror::Error, Debug)]
enum AppError {
    #[error(transparent)]
    Unexpected(Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::Unexpected(e.into()) // no .to_string(), chain preserved
    }
}
```

### Not flagged (intentional)

```rust
// Source type is not an Error — stringifying a u32 has no chain to lose.
impl From<u32> for AppError {
    fn from(n: u32) -> Self {
        AppError::Internal(n.to_string()) // OK
    }
}
```

```rust
// Stringifying an unrelated error inside a From body for logging context.
// The returned error preserves the actual source.
impl From<DatabaseError> for AppError {
    fn from(e: DatabaseError) -> Self {
        let other_err = build_some_unrelated_error();
        AppError {
            context: other_err.to_string(), // OK — recv is not the source type
            source: e,
        }
    }
}
```

```rust
// Format-arg macros (format!, write!, panic!, tracing::*) are NOT caught —
// they construct strings via Display::fmt, not ToString::to_string. See the
// "Known gaps" section.
impl From<DatabaseError> for AppError {
    fn from(e: DatabaseError) -> Self {
        AppError::Internal(format!("db: {e}")) // NOT flagged today
    }
}
```

## Macro behavior

| Source                           | Treatment                |
| -------------------------------- | ------------------------ |
| `macro_rules!` / bang proc-macro | **Checked.** A macro that expands to `.to_string()` on the source error is just as much a chain-loss pattern as inline code. |
| Attribute macros (`#[attr]`)     | Skipped — assumed to be third-party codegen the user can't easily change. |
| Derive macros                    | Skipped — same reason. |
| Compiler desugarings (`?`, etc.) | Skipped — synthetic, not user intent. |

## Known gaps

- **`format!("...{err}")` / `write!` / `panic!`** — these macros destroy the
  chain identically (they go through `Display::fmt` rather than
  `ToString::to_string`), but DE1302 doesn't see them. Catching this needs
  `format_args!`-level inspection; tracked as a follow-up.
- **Logging-only stringification** — `tracing::error!(error = %err)` inside a
  conversion body is not flagged; the receiver-type tightening rules out
  side-channel `.to_string()` calls.

## Configuration

The lint level is **deny** by default. To silence a known site, prefer fixing
the conversion to preserve the chain. When the underlying error type's shape
truly forbids that (e.g. an SDK boundary that exposes only opaque
`Internal(String)`), use a targeted allow with a `TODO(DE1302)` and a brief
note explaining what the proper fix would require:

```rust
// TODO(DE1302): `Internal` only carries a String; extend to hold a boxed
// source so `.source()` returns the original error, then remove this allow.
#[allow(unknown_lints, de1302_error_from_to_string)]
impl From<SomeError> for MyError {
    fn from(e: SomeError) -> Self {
        Self::Internal(e.to_string())
    }
}
```

If several adjacent conversions share the same TODO, group them in a small
inner module so the attribute appears once:

```rust
// TODO(DE1302): see above.
#[allow(unknown_lints, de1302_error_from_to_string)]
mod error_froms {
    use super::MyError;

    impl From<A> for MyError { fn from(e: A) -> Self { Self::Internal(e.to_string()) } }
    impl From<B> for MyError { fn from(e: B) -> Self { Self::Internal(e.to_string()) } }
    impl From<C> for MyError { fn from(e: C) -> Self { Self::Internal(e.to_string()) } }
}
```

The `unknown_lints` allow lets the attribute compile under regular `cargo
check` / `cargo clippy` runs that don't load the dylint driver.

## UI Tests

This lint includes UI tests covering:

- Method-call positive case (`bad_from_to_string.rs`)
- UFCS positive case (`bad_ufcs_to_string.rs`)
- Closure body recursion (`bad_closure_to_string.rs`)
- `TryFrom` with Error source (`bad_tryfrom_to_string.rs`)
- `TryFrom` whose only Error is the assoc type (`bad_tryfrom_assoc_error.rs`)
- `macro_rules!` expansion still flagged (`bad_macro_rules.rs`)
- `From<u32>` with non-Error source — not flagged (`good_from_u32.rs`)
- Stringifying an unrelated error inside a From body — not flagged
  (`good_unrelated_error.rs`)
- `#[from]`-style and source-preserving conversions — not flagged
  (`good_from_preserve.rs`)

## See Also

- [thiserror](https://crates.io/crates/thiserror) — derive macro for typed
  errors with `#[from]` / `#[source]` / `#[error(transparent)]`.
- [anyhow](https://crates.io/crates/anyhow) — opaque error type that
  preserves chain via `.source()` and `Into<Box<dyn Error + Send + Sync>>`.
- [`std::error::Error::source`](https://doc.rust-lang.org/std/error/trait.Error.html#method.source) — the chain navigation method.
- [Error handling in Rust](https://doc.rust-lang.org/book/ch09-00-error-handling.html)
