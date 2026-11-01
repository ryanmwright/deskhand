//! `deskhand` capability: facts about Deskhand itself.

use std::time::SystemTime;

use deskhand_core::entity::SensorClass;
use deskhand_core::{Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};

use crate::time::rfc3339;

/// Publishes the running version and when Deskhand started.
#[derive(Debug, Default)]
pub struct DeskhandInfo;

impl DeskhandInfo {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Integration for DeskhandInfo {
    fn id(&self) -> &'static str {
        "deskhand.info"
    }

    fn capability(&self) -> &'static str {
        "deskhand"
    }

    fn description(&self) -> &'static str {
        "Deskhand version and start time"
    }

    async fn probe(&self) -> Probe {
        Probe::available()
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let version = ctx.sensor(
            EntitySpec::sensor(
                EntityId::from_static("deskhand_version"),
                "Deskhand version",
            )
            .icon("mdi:information-outline")
            .diagnostic(),
        );
        let started = ctx.sensor(
            EntitySpec::sensor(
                EntityId::from_static("deskhand_started"),
                "Deskhand started",
            )
            .sensor_class(SensorClass::Timestamp)
            .diagnostic(),
        );
        version.set(deskhand_core::VERSION);
        started.set(rfc3339(SystemTime::now()));
        ctx.ready();
        ctx.cancelled().cancelled().await;
        Ok(())
    }
}
