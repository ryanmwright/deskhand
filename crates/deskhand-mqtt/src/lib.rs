//! MQTT transport for Deskhand.
//!
//! Implements the core `Transport` trait using `rumqttc`: topic layout, Home
//! Assistant device-based discovery, birth/LWT handling, stale entity cleanup
//! and the optional `mqtt_media_player` compatibility payload. See
//! `docs/v1-plan.md` §1 "MQTT and HA discovery".
