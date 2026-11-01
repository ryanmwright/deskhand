//! The `Integration` trait: one source of entities (idle detection, audio,
//! MPRIS, ...). Backends provide several candidate integrations per
//! capability; the supervisor probes them and runs the first available one.

use crate::config::ConfigError;
use crate::ctx::Ctx;

/// Result of probing whether an integration can work on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Available { detail: Option<String> },
    Unavailable { reason: String },
}

impl Probe {
    pub fn available() -> Self {
        Self::Available { detail: None }
    }

    pub fn available_with(detail: impl Into<String>) -> Self {
        Self::Available {
            detail: Some(detail.into()),
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }

    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }
}

/// Why an integration stopped running.
#[derive(Debug, thiserror::Error)]
pub enum IntegrationError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("{0}")]
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl IntegrationError {
    pub fn msg(msg: impl Into<String>) -> Self {
        Self::Other(msg.into().into())
    }

    pub fn other(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Other(Box::new(err))
    }
}

/// A source of entities.
///
/// `run` registers entities through the [`Ctx`], keeps them updated and
/// returns only when cancelled (`Ok`), when it has nothing left to do (`Ok`),
/// or on failure (`Err`, which makes the supervisor restart it with backoff).
/// `run` may be called again after it returns, so keep per-run state inside it.
#[async_trait::async_trait]
pub trait Integration: Send + Sync + 'static {
    /// Unique id, for example `idle.wayland`.
    fn id(&self) -> &'static str;
    /// Capability this provides, for example `idle`. Candidates for the same
    /// capability are alternatives; only one runs.
    fn capability(&self) -> &'static str;
    /// One line for `deskhand doctor`.
    fn description(&self) -> &'static str;
    /// Cheap check whether this can work here (service present, protocol advertised, ...).
    async fn probe(&self) -> Probe;
    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError>;
}
