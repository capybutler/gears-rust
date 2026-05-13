use std::sync::Arc;
use std::time::Duration;

use axum::extract::Path;
use axum::response::sse::KeepAlive;
use axum::response::{IntoResponse, Response, Sse};
use axum::{Extension, Json};
use modkit::api::canonical_prelude::*;
use modkit_security::SecurityContext;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, info, warn};
use utoipa::ToSchema;

use super::messages::SseRelay;
use crate::api::rest::error::MiniChatChatError;
use crate::domain::stream_events::StreamEvent;
use crate::infra::db::entity::chat_turn::TurnState;
use crate::module::AppServices;

// ════════════════════════════════════════════════════════════════════════════
// GET turn status
// ════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct TurnStatusResponse {
    request_id: uuid::Uuid,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assistant_message_id: Option<uuid::Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: time::OffsetDateTime,
}

fn map_turn_state(state: &TurnState) -> &'static str {
    match state {
        TurnState::Running => "running",
        TurnState::Completed => "done",
        TurnState::Failed => "error",
        TurnState::Cancelled => "cancelled",
    }
}

/// GET /mini-chat/v1/chats/{id}/turns/{request_id}
#[tracing::instrument(skip(svc, ctx), fields(chat_id = %chat_id, turn_request_id = %request_id))]
pub(crate) async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((chat_id, request_id)): Path<(uuid::Uuid, uuid::Uuid)>,
) -> ApiResult<Json<TurnStatusResponse>> {
    let turn = svc
        .turns
        .get(&ctx, chat_id, request_id)
        .await
        .map_err(CanonicalError::from)?;

    Ok(Json(TurnStatusResponse {
        request_id: turn.request_id,
        state: map_turn_state(&turn.state).to_owned(),
        error_code: turn.error_code.clone(),
        assistant_message_id: turn.assistant_message_id,
        updated_at: turn.updated_at,
    }))
}

// ════════════════════════════════════════════════════════════════════════════
// DELETE turn
// ════════════════════════════════════════════════════════════════════════════

/// DELETE /mini-chat/v1/chats/{id}/turns/{request_id}
#[tracing::instrument(skip(svc, ctx), fields(chat_id = %chat_id, turn_request_id = %request_id))]
pub(crate) async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((chat_id, request_id)): Path<(uuid::Uuid, uuid::Uuid)>,
) -> ApiResult<impl IntoResponse> {
    svc.turns
        .delete(&ctx, chat_id, request_id)
        .await
        .map_err(CanonicalError::from)?;

    Ok(no_content().into_response())
}

// ════════════════════════════════════════════════════════════════════════════
// POST retry turn
// ════════════════════════════════════════════════════════════════════════════

/// POST /mini-chat/v1/chats/{id}/turns/{request_id}/retry
#[tracing::instrument(skip(svc, ctx), fields(chat_id = %chat_id, turn_request_id = %request_id))]
pub(crate) async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((chat_id, request_id)): Path<(uuid::Uuid, uuid::Uuid)>,
) -> Response {
    let mutation = match svc.turns.retry(&ctx, chat_id, request_id).await {
        Ok(m) => m,
        Err(e) => return CanonicalError::from(e).into_response(),
    };

    start_mutation_stream(&svc, ctx, chat_id, mutation).await
}

// ════════════════════════════════════════════════════════════════════════════
// PATCH edit turn
// ════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize, ToSchema)]
pub struct EditTurnRequest {
    pub content: String,
}

impl modkit::api::api_dto::RequestApiDto for EditTurnRequest {}

/// PATCH /mini-chat/v1/chats/{id}/turns/{request_id}
#[tracing::instrument(skip(svc, ctx, body), fields(chat_id = %chat_id, turn_request_id = %request_id))]
pub(crate) async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((chat_id, request_id)): Path<(uuid::Uuid, uuid::Uuid)>,
    Json(body): Json<EditTurnRequest>,
) -> Response {
    if body.content.trim().is_empty() {
        return MiniChatChatError::invalid_argument()
            .with_field_violation("content", "Edit content must not be empty", "EMPTY_CONTENT")
            .create()
            .into_response();
    }

    let mutation = match svc
        .turns
        .edit(&ctx, chat_id, request_id, body.content)
        .await
    {
        Ok(m) => m,
        Err(e) => return CanonicalError::from(e).into_response(),
    };

    start_mutation_stream(&svc, ctx, chat_id, mutation).await
}

// ════════════════════════════════════════════════════════════════════════════
// Shared helpers
// ════════════════════════════════════════════════════════════════════════════

#[allow(clippy::cognitive_complexity)]
async fn start_mutation_stream(
    svc: &AppServices,
    ctx: SecurityContext,
    chat_id: uuid::Uuid,
    mutation: crate::domain::service::MutationResult,
) -> Response {
    let chat_model = mutation.chat_model.clone();
    let resolved = match svc
        .models
        .resolve_model(ctx.subject_id(), Some(mutation.chat_model))
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, model = %chat_model, "model resolution failed for mutation stream");
            return CanonicalError::from(e).into_response();
        }
    };

    let capacity = svc.stream.channel_capacity();
    let ping_secs = svc.stream.ping_interval_secs();
    let (tx, rx) = mpsc::channel::<StreamEvent>(capacity);
    let cancel = CancellationToken::new();

    info!(
        chat_id = %chat_id,
        new_request_id = %mutation.new_request_id,
        model = %resolved.model_id,
        "starting mutation SSE stream"
    );

    let provider_handle = match svc
        .stream
        .run_stream_for_mutation(
            ctx,
            chat_id,
            mutation.new_request_id,
            mutation.new_turn_id,
            mutation.user_content,
            resolved,
            mutation.web_search_enabled,
            mutation.snapshot_boundary,
            cancel.clone(),
            tx,
        )
        .await
    {
        Ok(handle) => handle,
        Err(e) => return CanonicalError::from(e).into_response(),
    };

    let monitor_span = tracing::Span::current();
    tokio::spawn(
        async move {
            if let Err(e) = provider_handle.await {
                tracing::error!(error = ?e, "provider task panicked");
            }
        }
        .instrument(monitor_span),
    );

    let relay = SseRelay::new(rx, cancel, ping_secs);
    Sse::new(relay)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
        .into_response()
}
