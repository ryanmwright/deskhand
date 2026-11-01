//! Mock D-Bus services. Each `start(&TestBus)` claims the real service's
//! well-known name on the matching private bus.

pub mod logind;
pub mod screensaver;

pub use logind::MockLogind;
pub use screensaver::MockScreenSaver;
