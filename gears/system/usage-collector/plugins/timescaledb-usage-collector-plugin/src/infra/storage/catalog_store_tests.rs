use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tokio_util::sync::CancellationToken;

use super::{PgCatalogStore, RefreshOutcome};
use crate::infra::metrics::Metrics;

/// A catalog store over a lazy pool (no connection opened) with a caller-chosen
/// cancellation token. The tiny acquire timeout keeps an accidental DB touch
/// from hanging the test.
fn lazy_store(cancel: CancellationToken) -> PgCatalogStore {
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(50))
        .connect_lazy("postgres://user:pass@localhost/db")
        .expect("a syntactically valid DSN yields a lazy pool without connecting");
    PgCatalogStore::new(pool.clone(), Arc::new(Metrics::new(pool)), cancel)
}

#[tokio::test]
async fn refresh_short_circuits_when_token_already_cancelled() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    let store = lazy_store(cancel);

    // With a biased select and an already-cancelled token, the cancel arm wins
    // before the count future is polled: no query is issued, no connection is
    // checked out.
    let outcome = store.refresh_catalog_size_cancellable().await;
    assert_eq!(outcome, RefreshOutcome::Cancelled);
}

#[tokio::test]
async fn list_rejects_backward_cursor() {
    use toolkit_odata::{CursorV1, ODataQuery, SortDir};
    use usage_collector_sdk::UsageCollectorPluginError;

    use crate::domain::ports::CatalogStore;

    let store = lazy_store(CancellationToken::new());

    // A backward cursor with a matching (unset) filter hash: only the direction
    // guard can reject it. Without the guard the request would page forward over
    // the lazy pool and fail with a backend error instead.
    let query = ODataQuery::new().with_cursor(CursorV1 {
        k: vec!["gts.some.type.v1".to_owned()],
        o: SortDir::Asc,
        s: "+gts_id".to_owned(),
        f: None,
        d: "bwd".to_owned(),
    });

    let err = store
        .list(&query)
        .await
        .expect_err("a backward cursor must be rejected before any DB access");

    match err {
        UsageCollectorPluginError::Internal(msg) => {
            assert!(msg.contains("direction"), "unexpected error message: {msg}");
        }
        other => panic!("expected an Internal direction error, got {other:?}"),
    }
}

#[tokio::test]
async fn refresh_runs_the_query_when_not_cancelled() {
    // A live (never-cancelled) token: the refresh actually attempts the count.
    // Over the lazy pool the connect fails after the 50ms acquire timeout and is
    // logged at warn, but the `Ran` arm proves the query branch was taken rather
    // than short-circuited.
    let store = lazy_store(CancellationToken::new());

    let outcome = store.refresh_catalog_size_cancellable().await;
    assert_eq!(outcome, RefreshOutcome::Ran);
}

#[tokio::test]
async fn burst_refresh_requests_coalesce_into_at_most_one_trailing_run() {
    use std::sync::atomic::Ordering;

    // A live token keeps the single background worker alive to drain signals.
    let store = lazy_store(CancellationToken::new());

    // Fire a burst of mutation signals synchronously (as a flurry of create /
    // delete would). `notify_one` collapses them: the worker runs the count at
    // most once while busy plus one trailing run — never once per signal, which
    // is the per-mutation `tokio::spawn` fan-out this change removes.
    for _ in 0..32 {
        store.request_catalog_size_refresh();
    }

    // Each count fails fast over the lazy pool (50ms acquire timeout); give the
    // worker ample real time to drain. A regression to per-signal spawning would
    // drive the counter toward 32.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let runs = store.refresh_runs.load(Ordering::SeqCst);
    assert!(
        runs <= 5,
        "32 burst signals must coalesce to a handful of runs, got {runs} \
         (per-signal spawning would approach 32)"
    );
}
