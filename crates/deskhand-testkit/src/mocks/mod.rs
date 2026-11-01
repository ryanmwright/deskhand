//! Mock D-Bus services. Each `start(&TestBus)` claims the real service's
//! well-known name on the matching private bus.

pub mod bluez;
pub mod hardware;
pub mod kwin;
pub mod logind;
pub mod mpris;
pub mod notifications;
pub mod plasma;
pub mod portal;
pub mod screensaver;
pub mod systemd;

pub use bluez::MockBluez;
pub use hardware::{MockGameMode, MockNetworkManager, MockPowerProfiles, MockUPower, UPowerDevice};
pub use kwin::MockKWin;
pub use logind::MockLogind;
pub use mpris::MockMpris;
pub use notifications::MockNotifications;
pub use plasma::{MockKGlobalAccel, MockScreenBrightness, MockShutdown};
pub use portal::MockPortal;
pub use screensaver::MockScreenSaver;
pub use systemd::MockSystemd;
