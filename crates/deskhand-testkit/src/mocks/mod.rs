//! Mock D-Bus services. Each `start(&TestBus)` claims the real service's
//! well-known name on the matching private bus.

pub mod bluez;
pub mod hardware;
pub mod logind;
pub mod mpris;
pub mod notifications;
pub mod screensaver;
pub mod systemd;

pub use bluez::MockBluez;
pub use hardware::{MockGameMode, MockNetworkManager, MockPowerProfiles, MockUPower, UPowerDevice};
pub use logind::MockLogind;
pub use mpris::MockMpris;
pub use notifications::MockNotifications;
pub use screensaver::MockScreenSaver;
pub use systemd::MockSystemd;
