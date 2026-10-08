//! Domain output ports for the usage-collector module.
//!
//! Ports are the domain-layer contracts that infra adapters implement,
//! keeping the domain free of transport / vendor types (`OTel`, HTTP, …).

pub mod declaration_mirror;
pub mod declarations;
pub mod metrics;

pub use declaration_mirror::{DeclarationMirror, MirrorError, NoopDeclarationMirror};
pub use declarations::{
    DeclarationRegistrar, DeclarationSource, UnavailableDeclarationRegistrar,
    UnavailableDeclarationSource,
};
pub use metrics::{
    AuthzDecision, IngestRequestErrorCategory, IngestRequestOutcome, NoopMetrics, PdpFailureCause,
    PdpOp, PluginErrorCategory, PluginOp, QueryErrorCategory, QueryKind, RecordErrorCategory,
    RecordOutcome, RequestOutcome, TypeResolutionOutcome, UsageCollectorMetrics,
};
