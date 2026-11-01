//! `metrics` capability: system metrics from `sysinfo`.
//!
//! CPU, load, memory, swap, disk usage per configured mount point, CPU
//! temperature, network throughput, GPU (AMD and Intel through sysfs,
//! NVIDIA through NVML, all via sysinfo) and boot time. Entities are only
//! registered for values the first sample could read, so a machine without
//! temperature sensors simply has no temperature entity.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use deskhand_core::ctx::Sensor;
use deskhand_core::entity::{SensorClass, StateClass};
use deskhand_core::{Attributes, Ctx, EntityId, EntitySpec, Integration, IntegrationError, Probe};
use serde::Deserialize;
use serde_json::json;
use sysinfo::{Components, CpuRefreshKind, Disks, Gpus, Networks, RefreshKind, System};

use crate::time::rfc3339;

/// `[integrations.metrics]` options.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct MetricsOptions {
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    /// Mount points to report usage for.
    pub disks: Vec<PathBuf>,
    pub network: bool,
    /// Interfaces to count. Empty means every interface except loopback,
    /// containers, bridges and VPN tunnels (their traffic is counted twice).
    pub network_interfaces: Vec<String>,
    pub temperature: bool,
    pub gpu: bool,
}

impl Default for MetricsOptions {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30),
            disks: vec![PathBuf::from("/")],
            network: true,
            network_interfaces: Vec::new(),
            temperature: true,
            gpu: true,
        }
    }
}

/// Interface name prefixes skipped by default.
const VIRTUAL_INTERFACES: &[&str] = &[
    "lo",
    "veth",
    "docker",
    "br-",
    "virbr",
    "podman",
    "cni",
    "flannel",
    "vnet",
    "tun",
    "tap",
    "wg",
    "tailscale",
    "zt",
];

fn counted_interface(name: &str, configured: &[String]) -> bool {
    if configured.is_empty() {
        !VIRTUAL_INTERFACES.iter().any(|p| name.starts_with(p))
    } else {
        configured.iter().any(|c| c == name)
    }
}

/// Picks the component that best represents the CPU package temperature.
fn cpu_temperature_label<'a>(labels: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    const PREFERRED: &[&str] = &[
        "package id 0",
        "tctl",
        "tdie",
        "cpu_thermal",
        "cpu temp",
        "coretemp",
        "k10temp",
        "cpu",
    ];
    let labels: Vec<&str> = labels.collect();
    PREFERRED.iter().find_map(|want| {
        labels
            .iter()
            .find(|l| l.to_ascii_lowercase().contains(want))
            .copied()
    })
}

/// Entity id for a mount point: `/` → `disk_root`, `/home` → `disk_home`.
fn disk_id(mount: &Path) -> EntityId {
    let text = mount.to_string_lossy();
    let trimmed = text.trim_matches('/');
    if trimmed.is_empty() {
        EntityId::from_static("disk_root")
    } else {
        EntityId::from_static("disk").join(trimmed)
    }
}

fn round(value: f64, digits: i32) -> f64 {
    let f = 10f64.powi(digits);
    (value * f).round() / f
}

fn percent(used: u64, total: u64) -> Option<f64> {
    (total > 0).then(|| round(used as f64 * 100.0 / total as f64, 1))
}

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// One reading of everything.
#[derive(Debug, Clone, Default, PartialEq)]
struct Sample {
    cpu_percent: Option<f64>,
    load: Option<[f64; 3]>,
    memory_percent: Option<f64>,
    memory_used_gib: Option<f64>,
    memory_total_gib: f64,
    swap_percent: Option<f64>,
    disks: BTreeMap<PathBuf, DiskSample>,
    cpu_temperature: Option<f64>,
    /// kB/s since the previous sample.
    network: Option<(f64, f64)>,
    gpu: Option<GpuSample>,
    boot_time: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
struct DiskSample {
    percent: f64,
    total_gib: f64,
    free_gib: f64,
    file_system: String,
}

#[derive(Debug, Clone, PartialEq)]
struct GpuSample {
    usage: Option<f64>,
    memory_percent: Option<f64>,
    temperature: Option<f64>,
    vendor: String,
    model: String,
}

/// Owns the sysinfo handles between samples.
struct Sampler {
    opts: MetricsOptions,
    system: System,
    disks: Option<Disks>,
    networks: Option<Networks>,
    last_network: Option<Instant>,
    components: Option<Components>,
    gpus: Option<Gpus>,
}

impl Sampler {
    fn new(opts: MetricsOptions) -> Result<Self, sysinfo::Error> {
        let system = System::new_with_specifics(
            RefreshKind::nothing().with_cpu(CpuRefreshKind::nothing().with_usage()),
        )?;
        let disks = Disks::new_with_refreshed_list().ok();
        let networks = if opts.network {
            Networks::new_with_refreshed_list().ok()
        } else {
            None
        };
        let components = if opts.temperature || opts.gpu {
            Components::new_with_refreshed_list().ok()
        } else {
            None
        };
        let gpus = if opts.gpu {
            Gpus::new_with_refreshed_list()
                .ok()
                .filter(|g| g.into_iter().next().is_some())
        } else {
            None
        };
        Ok(Self {
            opts,
            system,
            disks,
            networks,
            last_network: Some(Instant::now()),
            components,
            gpus,
        })
    }

    fn sample(&mut self) -> Sample {
        let mut s = Sample::default();

        self.system.refresh_cpu_usage();
        s.cpu_percent = Some(round(f64::from(self.system.global_cpu_usage()), 1));
        s.load = System::load_average()
            .ok()
            .map(|l| [l.one, l.five, l.fifteen].map(|v| round(v, 2)));

        self.system.refresh_memory();
        let (total, used) = (self.system.total_memory(), self.system.used_memory());
        s.memory_percent = percent(used, total);
        s.memory_used_gib = (total > 0).then(|| round(used as f64 / GIB, 2));
        s.memory_total_gib = round(total as f64 / GIB, 2);
        s.swap_percent = percent(self.system.used_swap(), self.system.total_swap());

        if let Some(disks) = &mut self.disks {
            disks.refresh(true);
            for want in &self.opts.disks {
                if let Some(d) = disks.into_iter().find(|d| d.mount_point() == want) {
                    let total = d.total_space();
                    let free = d.available_space();
                    if let Some(p) = percent(total.saturating_sub(free), total) {
                        s.disks.insert(
                            want.clone(),
                            DiskSample {
                                percent: p,
                                total_gib: round(total as f64 / GIB, 1),
                                free_gib: round(free as f64 / GIB, 1),
                                file_system: d.file_system().to_string_lossy().into_owned(),
                            },
                        );
                    }
                }
            }
        }

        if let Some(components) = &mut self.components {
            components.refresh(false);
            if self.opts.temperature {
                let components = &*components;
                let label = cpu_temperature_label(components.into_iter().map(|c| c.label()));
                s.cpu_temperature = label.and_then(|label| {
                    components
                        .into_iter()
                        .find(|c| c.label() == label)
                        .and_then(|c| c.temperature())
                        .map(|t| round(f64::from(t), 1))
                });
            }
        }

        if let Some(networks) = &mut self.networks {
            networks.refresh(true);
            let now = Instant::now();
            let elapsed = self
                .last_network
                .replace(now)
                .map_or(0.0, |t| now.duration_since(t).as_secs_f64());
            let (rx, tx) = networks
                .into_iter()
                .filter(|(name, _)| {
                    counted_interface(&name.to_string_lossy(), &self.opts.network_interfaces)
                })
                .fold((0u64, 0u64), |(rx, tx), (_, n)| {
                    (rx + n.received(), tx + n.transmitted())
                });
            if elapsed > 0.5 {
                s.network = Some((
                    round(rx as f64 / 1000.0 / elapsed, 1),
                    round(tx as f64 / 1000.0 / elapsed, 1),
                ));
            }
        }

        if let Some(gpus) = &mut self.gpus {
            gpus.refresh(false);
            // The first GPU with a usage reading (the discrete one on hybrid laptops).
            let gpu = gpus
                .into_iter()
                .find(|g| g.usage().is_some())
                .or_else(|| gpus.into_iter().next());
            if let Some(g) = gpu {
                let vendor = g.vendor().unwrap_or("unknown").to_owned();
                let temperature = self.components.as_ref().and_then(|cs| {
                    let driver = match vendor.to_ascii_lowercase() {
                        v if v.contains("amd") || v.contains("advanced micro") => "amdgpu",
                        v if v.contains("nvidia") => "nvidia",
                        v if v.contains("intel") => "i915",
                        _ => return None,
                    };
                    cs.into_iter()
                        .find(|c| c.label().to_ascii_lowercase().contains(driver))
                        .and_then(|c| c.temperature())
                        .map(|t| round(f64::from(t), 1))
                });
                s.gpu = Some(GpuSample {
                    usage: g.usage().map(|u| round(f64::from(u), 1)),
                    memory_percent: match (g.used_memory(), g.total_memory()) {
                        (Some(used), Some(total)) => percent(used, total),
                        _ => None,
                    },
                    temperature,
                    vendor,
                    model: g.model().unwrap_or("unknown").to_owned(),
                });
            }
        }

        s.boot_time = System::boot_time().ok().filter(|b| *b > 0);
        s
    }
}

/// Handles registered from the first sample.
#[derive(Default)]
struct Entities {
    cpu: Option<Sensor>,
    load: Vec<Sensor>,
    memory: Option<Sensor>,
    memory_used: Option<Sensor>,
    swap: Option<Sensor>,
    disks: BTreeMap<PathBuf, Sensor>,
    cpu_temperature: Option<Sensor>,
    network: Option<(Sensor, Sensor)>,
    gpu: Option<Sensor>,
    gpu_memory: Option<Sensor>,
    gpu_temperature: Option<Sensor>,
    boot_time: Option<Sensor>,
}

fn pct_spec(id: &'static str, name: &str) -> EntitySpec {
    EntitySpec::sensor(EntityId::from_static(id), name)
        .unit("%")
        .state_class(StateClass::Measurement)
        .precision(1)
}

fn temp_spec(id: &'static str, name: &str) -> EntitySpec {
    EntitySpec::sensor(EntityId::from_static(id), name)
        .sensor_class(SensorClass::Temperature)
        .unit("°C")
        .state_class(StateClass::Measurement)
        .precision(0)
}

impl Entities {
    fn register(ctx: &Ctx, s: &Sample) -> Self {
        let mut e = Self::default();
        if s.cpu_percent.is_some() {
            e.cpu = Some(ctx.sensor(pct_spec("cpu_percent", "CPU").icon("mdi:cpu-64-bit")));
        }
        if s.load.is_some() {
            for (id, name, enabled) in [
                ("load_1m", "Load (1m)", true),
                ("load_5m", "Load (5m)", false),
                ("load_15m", "Load (15m)", false),
            ] {
                let mut spec = EntitySpec::sensor(EntityId::from_static(id), name)
                    .state_class(StateClass::Measurement)
                    .precision(2)
                    .icon("mdi:gauge");
                if !enabled {
                    spec = spec.disabled_by_default();
                }
                e.load.push(ctx.sensor(spec));
            }
        }
        if s.memory_percent.is_some() {
            e.memory = Some(ctx.sensor(pct_spec("memory_percent", "Memory").icon("mdi:memory")));
            e.memory_used = Some(
                ctx.sensor(
                    EntitySpec::sensor(EntityId::from_static("memory_used"), "Memory used")
                        .sensor_class(SensorClass::DataSize)
                        .unit("GiB")
                        .state_class(StateClass::Measurement)
                        .precision(1)
                        .icon("mdi:memory")
                        .disabled_by_default(),
                ),
            );
        }
        if s.swap_percent.is_some() {
            e.swap = Some(
                ctx.sensor(
                    pct_spec("swap_percent", "Swap")
                        .icon("mdi:harddisk")
                        .disabled_by_default(),
                ),
            );
        }
        for mount in s.disks.keys() {
            let id = disk_id(mount);
            let spec = EntitySpec::sensor(id, format!("Disk {}", mount.display()))
                .unit("%")
                .state_class(StateClass::Measurement)
                .precision(1)
                .icon("mdi:harddisk");
            e.disks.insert(mount.clone(), ctx.sensor(spec));
        }
        if s.cpu_temperature.is_some() {
            e.cpu_temperature = Some(ctx.sensor(temp_spec("cpu_temperature", "CPU temperature")));
        }
        if s.network.is_some() {
            let spec = |id: &'static str, name: &str, icon: &str| {
                EntitySpec::sensor(EntityId::from_static(id), name)
                    .sensor_class(SensorClass::DataRate)
                    .unit("kB/s")
                    .state_class(StateClass::Measurement)
                    .precision(0)
                    .icon(icon)
            };
            e.network = Some((
                ctx.sensor(spec(
                    "network_rx",
                    "Network download",
                    "mdi:download-network",
                )),
                ctx.sensor(spec("network_tx", "Network upload", "mdi:upload-network")),
            ));
        }
        if let Some(gpu) = &s.gpu {
            if gpu.usage.is_some() {
                e.gpu = Some(ctx.sensor(pct_spec("gpu_percent", "GPU").icon("mdi:expansion-card")));
            }
            if gpu.memory_percent.is_some() {
                e.gpu_memory = Some(ctx.sensor(
                    pct_spec("gpu_memory_percent", "GPU memory").icon("mdi:expansion-card"),
                ));
            }
            if gpu.temperature.is_some() {
                e.gpu_temperature =
                    Some(ctx.sensor(temp_spec("gpu_temperature", "GPU temperature")));
            }
        }
        if s.boot_time.is_some() {
            e.boot_time = Some(
                ctx.sensor(
                    EntitySpec::sensor(EntityId::from_static("boot_time"), "Last boot")
                        .sensor_class(SensorClass::Timestamp)
                        .icon("mdi:restart")
                        .diagnostic(),
                ),
            );
        }
        e
    }

    fn publish(&self, s: &Sample) {
        fn set(handle: &Option<Sensor>, value: Option<f64>) {
            if let Some(h) = handle {
                h.set(value);
            }
        }
        set(&self.cpu, s.cpu_percent);
        if let Some(load) = s.load {
            for (h, v) in self.load.iter().zip(load) {
                h.set(v);
            }
        }
        set(&self.memory, s.memory_percent);
        if let Some(h) = &self.memory_used {
            let mut attrs = Attributes::new();
            attrs.insert("total_gib".into(), json!(s.memory_total_gib));
            h.set_attributes(attrs);
            h.set(s.memory_used_gib);
        }
        set(&self.swap, s.swap_percent);
        for (mount, h) in &self.disks {
            match s.disks.get(mount) {
                Some(d) => {
                    let mut attrs = Attributes::new();
                    attrs.insert("mount_point".into(), json!(mount));
                    attrs.insert("total_gib".into(), json!(d.total_gib));
                    attrs.insert("free_gib".into(), json!(d.free_gib));
                    attrs.insert("file_system".into(), json!(d.file_system));
                    h.set_attributes(attrs);
                    h.set(d.percent);
                }
                None => h.set_unknown(),
            }
        }
        set(&self.cpu_temperature, s.cpu_temperature);
        if let Some((rx, tx)) = &self.network {
            rx.set(s.network.map(|n| n.0));
            tx.set(s.network.map(|n| n.1));
        }
        let gpu = s.gpu.as_ref();
        set(&self.gpu, gpu.and_then(|g| g.usage));
        set(&self.gpu_memory, gpu.and_then(|g| g.memory_percent));
        set(&self.gpu_temperature, gpu.and_then(|g| g.temperature));
        if let (Some(h), Some(g)) = (&self.gpu, gpu) {
            let mut attrs = Attributes::new();
            attrs.insert("vendor".into(), json!(g.vendor));
            attrs.insert("model".into(), json!(g.model));
            h.set_attributes(attrs);
        }
        if let (Some(h), Some(boot)) = (&self.boot_time, s.boot_time) {
            h.set(rfc3339(UNIX_EPOCH + Duration::from_secs(boot)));
        }
    }
}

/// Publishes system metrics every `interval`.
#[derive(Debug, Default)]
pub struct Metrics;

impl Metrics {
    pub fn new() -> Self {
        Self
    }
}

/// Samples on a dedicated thread: sysinfo's NVML handle is not `Send`, and
/// reading sysfs/procfs blocks. The thread stops as soon as `stop` is dropped.
fn spawn_sampler(
    opts: MetricsOptions,
    samples: tokio::sync::mpsc::Sender<Result<Sample, String>>,
    stop: std::sync::mpsc::Receiver<()>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("deskhand-metrics".into())
        .spawn(move || {
            let interval = opts.interval;
            let mut sampler = match Sampler::new(opts) {
                Ok(s) => s,
                Err(e) => {
                    let _ = samples.blocking_send(Err(e.to_string()));
                    return;
                }
            };
            // CPU usage and network rates need two readings some time apart.
            let mut wait = sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(600));
            loop {
                match stop.recv_timeout(wait) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    _ => return,
                }
                if samples.blocking_send(Ok(sampler.sample())).is_err() {
                    return;
                }
                wait = interval;
            }
        })
        .map(drop)
}

#[async_trait::async_trait]
impl Integration for Metrics {
    fn id(&self) -> &'static str {
        "metrics.sysinfo"
    }

    fn capability(&self) -> &'static str {
        "metrics"
    }

    fn description(&self) -> &'static str {
        "CPU, memory, disk, temperature, network and GPU metrics"
    }

    async fn probe(&self) -> Probe {
        match System::new() {
            Ok(_) => Probe::available(),
            Err(e) => Probe::unavailable(format!("sysinfo: {e}")),
        }
    }

    async fn run(&self, ctx: Ctx) -> Result<(), IntegrationError> {
        let opts: MetricsOptions = ctx.config()?;
        if opts.interval < Duration::from_secs(1) {
            return Err(IntegrationError::msg(
                "integrations.metrics.interval must be at least 1s",
            ));
        }
        let wanted_disks = opts.disks.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let (_stop, stop_rx) = std::sync::mpsc::channel();
        spawn_sampler(opts, tx, stop_rx).map_err(IntegrationError::other)?;

        let first = match rx.recv().await {
            Some(Ok(s)) => s,
            Some(Err(e)) => return Err(IntegrationError::msg(format!("sysinfo: {e}"))),
            None => return Err(IntegrationError::msg("metrics thread exited")),
        };
        for mount in &wanted_disks {
            if !first.disks.contains_key(mount) {
                tracing::warn!(mount = %mount.display(), "integrations.metrics.disks: no such mount point");
            }
        }
        let entities = Entities::register(&ctx, &first);
        entities.publish(&first);
        ctx.ready();

        let cancel = ctx.cancelled();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                s = rx.recv() => match s {
                    Some(Ok(s)) => entities.publish(&s),
                    _ => return Err(IntegrationError::msg("metrics thread exited")),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_filter() {
        assert!(counted_interface("enp3s0", &[]));
        assert!(counted_interface("wlan0", &[]));
        assert!(!counted_interface("lo", &[]));
        assert!(!counted_interface("veth12ab", &[]));
        assert!(!counted_interface("wg0", &[]));
        assert!(!counted_interface("enp3s0", &["wlan0".into()]));
        assert!(counted_interface("wg0", &["wg0".into()]));
    }

    #[test]
    fn temperature_label_choice() {
        let labels = [
            "acpitz temp1",
            "k10temp Tctl",
            "amdgpu edge",
            "nvme Composite",
        ];
        assert_eq!(
            cpu_temperature_label(labels.into_iter()),
            Some("k10temp Tctl")
        );
        let intel = ["coretemp Core 0", "coretemp Package id 0"];
        assert_eq!(
            cpu_temperature_label(intel.into_iter()),
            Some("coretemp Package id 0")
        );
        assert_eq!(cpu_temperature_label(["nvme Composite"].into_iter()), None);
    }

    #[test]
    fn disk_ids() {
        assert_eq!(disk_id(Path::new("/")).as_str(), "disk_root");
        assert_eq!(disk_id(Path::new("/home")).as_str(), "disk_home");
        assert_eq!(
            disk_id(Path::new("/mnt/Data 2")).as_str(),
            "disk_mnt_data_2"
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(percent(1, 3), Some(33.3));
        assert_eq!(percent(1, 0), None);
        assert_eq!(round(1.23456, 2), 1.23);
    }

    #[test]
    fn options_parse() {
        let config = deskhand_core::Config::from_toml_str(
            "[integrations.metrics]\ninterval = '1m'\ndisks = ['/', '/home']\ngpu = false",
            None,
        )
        .unwrap();
        let parsed = config
            .integration("metrics")
            .unwrap()
            .options::<MetricsOptions>()
            .unwrap();
        assert!(parsed.unknown_keys.is_empty());
        assert_eq!(parsed.value.interval, Duration::from_secs(60));
        assert_eq!(parsed.value.disks.len(), 2);
        assert!(!parsed.value.gpu);
    }

    #[test]
    fn real_sample_has_basics() {
        let Ok(mut sampler) = Sampler::new(MetricsOptions::default()) else {
            return; // unsupported OS
        };
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let s = sampler.sample();
        assert!(s.cpu_percent.is_some_and(|c| (0.0..=100.0).contains(&c)));
        assert!(s.memory_percent.is_some());
        assert!(s.boot_time.is_some());
    }
}
