//! One receiver: its pairings, the link state of each device, and battery
//! readings driven by the devices' own events.
//!
//! - Start: unique id, firmware, notification flags, then device arrival,
//!   which reports every pairing with its link state without contacting the
//!   devices. Codenames come from the receiver.
//! - Link up: feature discovery and identity once per device, then one
//!   battery read. Later link-ups (and 0x1D4B reconnects) wait a moment for
//!   the report the device sends by itself and read only if none comes.
//! - Battery changes arrive as events; nothing is polled unless
//!   `battery_refresh` is set, and then only connected devices are read.
//! - Link down: `connected` turns off and the values stay. A sleeping device
//!   is never asked anything.
//! - Backlight (opt-in): read once at discovery, then kept up to date by the
//!   device's 0x1982 events; Home Assistant's changes are written while the
//!   device is connected.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use deskhand_common::peripherals::{
    Backlight, BacklightCommand, ChargeState, Control, Controller, Link, PairedSlot,
    PeripheralReading, PeripheralSink, ReceiverReading,
};
use deskhand_core::EntityId;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::adapter::{
    AdapterError, Battery, DeviceEvent, DeviceLink, Notice, ReceiverLink, kind_name,
};

const MANUFACTURER: &str = "Logitech";

/// Timings, shortened by tests.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    /// After a link-up, how long to wait for the device's own battery report.
    pub wake_read: Duration,
    /// A failed discovery or read is retried after this (doubling, while the
    /// device stays connected).
    pub retry: Duration,
    pub retry_max: Duration,
    /// Upper bound for one discovery or read.
    pub job_timeout: Duration,
    /// Overrides `battery_refresh` (tests).
    pub refresh: Option<Duration>,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            wake_read: Duration::from_secs(3),
            retry: Duration::from_secs(30),
            retry_max: Duration::from_secs(600),
            job_timeout: Duration::from_secs(30),
            refresh: None,
        }
    }
}

#[derive(Debug)]
struct Slot {
    wpid: u16,
    kind: u8,
    encrypted: bool,
    online: bool,
    codename: Option<String>,
    /// The receiver's unit id for the device (the kernel's serial).
    unit_id: Option<String>,
    /// Fixed once reported, so a later name cannot move the entities.
    key: Option<EntityId>,
    device: Option<Arc<DeviceLink>>,
    battery: Option<Battery>,
    read_at: Option<SystemTime>,
    backlight: Option<Backlight>,
    /// Routes Home Assistant's commands for this slot to the task.
    controller: Controller,
    /// A discovery or read is running.
    busy: bool,
    /// When to read (or discover) unless the device reports first.
    due: Option<Instant>,
    failures: u32,
}

impl Slot {
    fn new(wpid: u16, kind: u8, encrypted: bool, controller: Controller) -> Self {
        Self {
            wpid,
            kind,
            encrypted,
            online: false,
            codename: None,
            unit_id: None,
            key: None,
            device: None,
            battery: None,
            read_at: None,
            backlight: None,
            controller,
            busy: false,
            due: None,
            failures: 0,
        }
    }

    fn name(&self) -> Option<&str> {
        self.device
            .as_ref()
            .and_then(|d| d.identity.name.as_deref())
            .or(self.codename.as_deref())
    }
}

type Discovery = (DeviceLink, Option<Battery>, Option<Backlight>);

enum Job {
    Discovered(Result<Discovery, AdapterError>),
    Read(Result<Option<Battery>, AdapterError>),
    /// A backlight write, and the state read back.
    Backlight(Result<Option<Backlight>, AdapterError>),
}

pub(crate) struct ReceiverTask {
    pub link: Arc<ReceiverLink>,
    pub notices: mpsc::UnboundedReceiver<Notice>,
    pub node: String,
    pub kernel_receiver: bool,
    pub sink: PeripheralSink,
    pub timing: Timing,
}

struct State {
    link: Arc<ReceiverLink>,
    sink: PeripheralSink,
    timing: Timing,
    key: EntityId,
    reading: ReceiverReading,
    slots: BTreeMap<u8, Slot>,
    jobs: JoinSet<(u8, Job)>,
    commands: mpsc::UnboundedSender<(u8, BacklightCommand)>,
}

impl ReceiverTask {
    /// Runs until cancelled or the receiver is gone. Its devices are left
    /// reported as disconnected, so a replugged receiver picks them up again.
    pub async fn run(
        self,
        cancel: CancellationToken,
        ready: oneshot::Sender<()>,
    ) -> Result<(), AdapterError> {
        let Self {
            link,
            mut notices,
            node,
            kernel_receiver,
            sink,
            timing,
        } = self;
        let unique_id = link.unique_id().await?;
        let firmware = link.firmware().await;
        if !kernel_receiver {
            match link.enable_notifications().await {
                Ok(true) => tracing::debug!(%node, "receiver notifications enabled"),
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(%node, error = %e, "could not enable receiver notifications")
                }
            }
        }
        let kind = link.kind();
        let key = EntityId::slugify(&format!("{}_{unique_id}", kind.key_prefix()));
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let mut state = State {
            reading: ReceiverReading {
                key: key.clone(),
                name: kind.name().to_owned(),
                manufacturer: Some(MANUFACTURER.to_owned()),
                model: Some(kind.name().to_owned()),
                serial: Some(unique_id),
                firmware,
                slots: Vec::new(),
            },
            key,
            link,
            sink,
            timing,
            slots: BTreeMap::new(),
            jobs: JoinSet::new(),
            commands,
        };

        // Device arrival: the 0x41 notices are queued before the ack.
        state.link.trigger_arrival().await?;
        while let Ok(n) = notices.try_recv() {
            state.notice(n).await;
        }
        state.report_receiver();
        let slots: Vec<u8> = state.slots.keys().copied().collect();
        for slot in slots {
            state.report(slot);
        }
        tracing::info!(
            %node,
            receiver = %state.key,
            devices = state.slots.len(),
            "HID++ receiver ready"
        );
        let _ = ready.send(());

        let refresh = state.timing.refresh.or(state.sink.battery_refresh());
        let mut tick = refresh.map(|every| tokio::time::interval_at(Instant::now() + every, every));
        let result = loop {
            let due = state.slots.values().filter_map(|s| s.due).min();
            tokio::select! {
                () = cancel.cancelled() => break Ok(()),
                n = notices.recv() => match n {
                    Some(n) => state.notice(n).await,
                    None => break Err(AdapterError::Gone),
                },
                Some((slot, cmd)) = command_rx.recv() => state.command(slot, cmd),
                Some(done) = state.jobs.join_next(), if !state.jobs.is_empty() => {
                    if let Ok((slot, job)) = done
                        && let Err(AdapterError::Gone) = state.finished(slot, job)
                    {
                        break Err(AdapterError::Gone);
                    }
                }
                () = sleep_until(due), if due.is_some() => state.start_due(),
                _ = tick_next(&mut tick), if tick.is_some() => state.refresh(),
            }
            if !state.link.is_connected() {
                break Err(AdapterError::Gone);
            }
        };
        state.jobs.abort_all();
        // Gone or stopping: the devices keep their values, disconnected.
        for slot in state.slots.values_mut() {
            slot.online = false;
        }
        let slots: Vec<u8> = state.slots.keys().copied().collect();
        for slot in slots {
            state.report(slot);
        }
        state.report_receiver();
        result
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn tick_next(tick: &mut Option<tokio::time::Interval>) {
    match tick {
        Some(t) => {
            t.tick().await;
        }
        None => std::future::pending().await,
    }
}

impl State {
    async fn notice(&mut self, n: Notice) {
        match n {
            Notice::Connection {
                slot,
                wpid,
                kind,
                online,
                encrypted,
            } => {
                let known = self.slots.get(&slot).is_some_and(|s| s.wpid == wpid);
                if !known {
                    // New pairing, or another device paired into the slot.
                    if let Some(old) = self.slots.remove(&slot)
                        && let Some(key) = old.key
                    {
                        self.sink.remove(&key);
                    }
                    let tx = self.commands.clone();
                    let controller = Controller::new(move |cmd| {
                        let _ = tx.send((slot, cmd));
                    });
                    let mut s = Slot::new(wpid, kind, encrypted, controller);
                    s.codename = self.link.codename(slot).await;
                    s.unit_id = self.link.unit_id(slot).await;
                    self.slots.insert(slot, s);
                }
                let Some(s) = self.slots.get_mut(&slot) else {
                    return;
                };
                s.encrypted = encrypted;
                let was = s.online;
                s.online = online;
                if online && (!was || !known) {
                    self.link_up(slot);
                } else if !online {
                    s.due = None;
                }
                if was != online || !known {
                    tracing::debug!(receiver = %self.key, slot, online, "HID++ link");
                    self.report(slot);
                    self.report_receiver();
                }
            }
            Notice::Unpaired { slot } => {
                if let Some(s) = self.slots.remove(&slot) {
                    tracing::info!(receiver = %self.key, slot, "HID++ device unpaired");
                    if let Some(key) = s.key {
                        self.sink.remove(&key);
                    }
                    self.report_receiver();
                }
            }
            Notice::Event {
                slot,
                feature_index,
                function,
                payload,
            } => {
                let Some(s) = self.slots.get_mut(&slot) else {
                    return;
                };
                let Some(device) = &s.device else {
                    return;
                };
                let event = device.decode(feature_index, function, &payload);
                // Events mean the device is awake, even if a 0x41 was missed.
                let woke = !s.online;
                s.online = true;
                match event {
                    Some(DeviceEvent::Battery(b)) => {
                        s.battery = Some(b);
                        s.read_at = Some(SystemTime::now());
                        s.due = None;
                        self.report(slot);
                    }
                    Some(DeviceEvent::Backlight(change)) => {
                        if let Some(b) = &mut s.backlight {
                            change.apply(b);
                            self.report(slot);
                        } else if woke {
                            self.report(slot);
                        }
                    }
                    Some(DeviceEvent::Reconnected) => {
                        if s.due.is_none() && !s.busy {
                            s.due = Some(Instant::now() + self.timing.wake_read);
                        }
                        if woke {
                            self.report(slot);
                        }
                    }
                    None if woke => self.report(slot),
                    None => {}
                }
                if woke {
                    self.report_receiver();
                }
            }
        }
    }

    /// A device came online: discover it once, else wait for its own report.
    fn link_up(&mut self, slot: u8) {
        let Some(s) = self.slots.get_mut(&slot) else {
            return;
        };
        if s.busy {
            return;
        }
        if s.device.is_none() {
            s.due = None;
            self.start(slot);
        } else if s.due.is_none() {
            s.due = Some(Instant::now() + self.timing.wake_read);
        }
    }

    fn start(&mut self, slot: u8) {
        let Some(s) = self.slots.get_mut(&slot) else {
            return;
        };
        if !s.online || s.busy {
            return;
        }
        s.busy = true;
        s.due = None;
        let timeout = self.timing.job_timeout;
        match s.device.clone() {
            None => {
                let link = self.link.clone();
                let backlight = self.sink.wants(Control::Backlight);
                self.jobs.spawn(async move {
                    let job = async {
                        let device = link.device(slot, backlight).await?;
                        let battery = device.read_battery().await?;
                        // The backlight is extra: a failed read leaves it out.
                        let backlight = match device.read_backlight().await {
                            Ok(b) => b,
                            Err(AdapterError::Gone) => return Err(AdapterError::Gone),
                            Err(e) => {
                                tracing::debug!(slot, error = %e, "backlight not read");
                                None
                            }
                        };
                        Ok((device, battery, backlight))
                    };
                    let r = tokio::time::timeout(timeout, job)
                        .await
                        .unwrap_or(Err(AdapterError::Timeout));
                    (slot, Job::Discovered(r))
                });
            }
            Some(device) => {
                self.jobs.spawn(async move {
                    let r = tokio::time::timeout(timeout, device.read_battery())
                        .await
                        .unwrap_or(Err(AdapterError::Timeout));
                    (slot, Job::Read(r))
                });
            }
        }
    }

    /// A backlight change from Home Assistant: written only to a connected,
    /// discovered device with a backlight.
    fn command(&mut self, slot: u8, cmd: BacklightCommand) {
        let Some(s) = self.slots.get(&slot) else {
            return;
        };
        let device = s
            .device
            .clone()
            .filter(|_| s.online && s.backlight.is_some());
        let Some(device) = device else {
            tracing::info!(receiver = %self.key, slot, ?cmd, "device asleep; backlight command ignored");
            return;
        };
        let timeout = self.timing.job_timeout;
        self.jobs.spawn(async move {
            let r = tokio::time::timeout(timeout, device.set_backlight(cmd))
                .await
                .unwrap_or(Err(AdapterError::Timeout));
            (slot, Job::Backlight(r))
        });
    }

    fn start_due(&mut self) {
        let now = Instant::now();
        let due: Vec<u8> = self
            .slots
            .iter()
            .filter(|(_, s)| s.due.is_some_and(|d| d <= now))
            .map(|(slot, _)| *slot)
            .collect();
        for slot in due {
            if let Some(s) = self.slots.get_mut(&slot) {
                s.due = None;
            }
            self.start(slot);
        }
    }

    /// `battery_refresh`: read every connected device once.
    fn refresh(&mut self) {
        let online: Vec<u8> = self
            .slots
            .iter()
            .filter(|(_, s)| s.online && s.device.is_some())
            .map(|(slot, _)| *slot)
            .collect();
        for slot in online {
            self.start(slot);
        }
    }

    fn finished(&mut self, slot: u8, job: Job) -> Result<(), AdapterError> {
        let Some(s) = self.slots.get_mut(&slot) else {
            return Ok(());
        };
        let result = match job {
            Job::Backlight(r) => {
                match r {
                    Ok(b) => s.backlight = b.or(s.backlight),
                    Err(AdapterError::Gone) => return Err(AdapterError::Gone),
                    Err(e) => {
                        tracing::warn!(receiver = %self.key, slot, error = %e, "backlight not changed")
                    }
                }
                // Also after a failure: Home Assistant shows the real state again.
                self.report(slot);
                return Ok(());
            }
            Job::Discovered(Ok((device, battery, backlight))) => {
                tracing::info!(
                    receiver = %self.key,
                    slot,
                    name = device.identity.name.as_deref().unwrap_or("?"),
                    battery = ?device.battery,
                    backlight = backlight.is_some(),
                    "HID++ device discovered"
                );
                s.device = Some(Arc::new(device));
                s.backlight = backlight;
                Ok(battery)
            }
            Job::Read(Ok(battery)) => Ok(battery),
            Job::Discovered(Err(e)) | Job::Read(Err(e)) => Err(e),
        };
        s.busy = false;
        match result {
            Ok(battery) => {
                s.failures = 0;
                if battery.is_some() {
                    s.battery = battery;
                    s.read_at = Some(SystemTime::now());
                }
                self.report(slot);
                self.report_receiver();
                Ok(())
            }
            Err(AdapterError::Gone) => Err(AdapterError::Gone),
            Err(e) => {
                s.failures += 1;
                if s.online {
                    let wait = self
                        .timing
                        .retry
                        .saturating_mul(1 << (s.failures - 1).min(10))
                        .min(self.timing.retry_max);
                    s.due = Some(Instant::now() + wait);
                    tracing::debug!(receiver = %self.key, slot, error = %e, retry_in = ?wait, "HID++ device not read");
                } else {
                    tracing::debug!(receiver = %self.key, slot, error = %e, "HID++ device went to sleep while being read");
                }
                Ok(())
            }
        }
    }

    /// Reports the device in `slot` once it has a name to key it by.
    fn report(&mut self, slot: u8) {
        let receiver = self.key.clone();
        let Some(s) = self.slots.get_mut(&slot) else {
            return;
        };
        if s.key.is_none() {
            // The receiver's codename is there even while the device sleeps,
            // so it keys the device; the device's own name is the fallback.
            let name = s.codename.as_deref().or(s.name());
            s.key = name.map(|n| PeripheralReading::key_for(n, "", ""));
        }
        let (Some(key), Some(name)) = (s.key.clone(), s.name()) else {
            return;
        };
        let identity = s
            .device
            .as_ref()
            .map(|d| d.identity.clone())
            .unwrap_or_default();
        let b = s.battery;
        self.sink.update(PeripheralReading {
            kind: kind_name(s.kind).map(str::to_owned),
            manufacturer: Some(MANUFACTURER.to_owned()),
            model: Some(name.to_owned()),
            model_id: Some(format!("{:04X}", s.wpid)),
            serial: identity.serial,
            alt_serials: s.unit_id.iter().cloned().collect(),
            firmware: identity.firmware,
            link: Link {
                receiver: Some(receiver),
                slot: Some(slot),
                encrypted: Some(s.encrypted),
            },
            percentage: b.and_then(|b| b.percentage),
            estimated: b.is_some_and(|b| b.estimated),
            level: b.and_then(|b| b.level),
            state: b.map_or(ChargeState::Unknown, |b| b.state),
            low: b.and_then(|b| b.low),
            connected: s.online,
            read_at: s.read_at,
            backlight: s.backlight,
            controller: s.backlight.is_some().then(|| s.controller.clone()),
            ..PeripheralReading::new(key, name)
        });
    }

    fn report_receiver(&mut self) {
        self.reading.slots = self
            .slots
            .iter()
            .map(|(slot, s)| PairedSlot {
                slot: *slot,
                name: s
                    .name()
                    .map_or_else(|| format!("{:04X}", s.wpid), str::to_owned),
                model_id: Some(format!("{:04X}", s.wpid)),
                online: s.online,
            })
            .collect();
        self.sink.receiver(self.reading.clone());
    }
}
