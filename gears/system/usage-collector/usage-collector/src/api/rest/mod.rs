//! REST surface for usage-collector. Layout mirrors `resource-group`:
//! `dto` (wire shapes), `handlers/` (axum, one file per resource family),
//! `routes/` (`OperationBuilder`, one file per resource family). This
//! generation has no published contract document; the specification it
//! serves is emitted at runtime by `OpenApiRegistryImpl`.

pub mod dto;
pub mod handlers;
pub mod routes;
