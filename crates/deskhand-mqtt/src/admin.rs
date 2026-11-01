//! One-shot broker operations for the CLI: `deskhand doctor` (can we
//! connect, is a daemon online, what is published) and `deskhand uninstall`
//! (remove every retained message Deskhand left on the broker).
//!
//! The admin client uses its own client id and no last will, so it never
//! disturbs a running daemon's session or availability.

use std::collections::BTreeMap;
use std::time::Duration;

use bytes::Bytes;
use rumqttc::{
    AsyncClient, ConnectReturnCode, ConnectionError, Event, EventLoop, Outgoing, Packet, QoS,
};
use tokio::time::Instant;

use crate::config::{MqttConfig, MqttConfigError};
use crate::topics::Topics;

/// Errors from the admin client, worded for people.
#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error(transparent)]
    Config(#[from] MqttConfigError),
    #[error("{0}")]
    Connect(String),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("connection lost: {0}")]
    Lost(String),
}

/// A readable explanation of a failed connection attempt.
pub fn describe_connection_error(e: &ConnectionError) -> String {
    match e {
        ConnectionError::ConnectionRefused(code) => match code {
            ConnectReturnCode::BadUserNamePassword => {
                "the broker rejected the username or password".into()
            }
            ConnectReturnCode::NotAuthorized => {
                "the broker refused the connection: not authorised (check the username and the broker's ACL)".into()
            }
            ConnectReturnCode::BadClientId => "the broker rejected the client id".into(),
            ConnectReturnCode::ServiceUnavailable => "the broker is unavailable".into(),
            ConnectReturnCode::RefusedProtocolVersion => {
                "the broker does not speak MQTT 3.1.1".into()
            }
            ConnectReturnCode::Success => "connection refused".into(),
        },
        ConnectionError::Io(io) => match io.kind() {
            std::io::ErrorKind::ConnectionRefused => {
                "connection refused: nothing is listening on that host and port".into()
            }
            std::io::ErrorKind::TimedOut => "the connection timed out".into(),
            _ if io.to_string().contains("failed to lookup") => {
                format!("cannot resolve the host name ({io})")
            }
            _ => format!("network error: {io}"),
        },
        ConnectionError::Tls(tls) => format!("TLS error: {tls}"),
        ConnectionError::NetworkTimeout => "the connection timed out".into(),
        other => other.to_string(),
    }
}

/// A short-lived connection for inspection and cleanup.
pub struct AdminClient {
    client: AsyncClient,
    eventloop: EventLoop,
    topics: Topics,
}

impl std::fmt::Debug for AdminClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminClient")
            .field("node_id", &self.topics.node_id())
            .finish_non_exhaustive()
    }
}

impl AdminClient {
    /// Connects and waits for the broker's CONNACK.
    pub async fn connect(
        config: &MqttConfig,
        node_id: &str,
        timeout: Duration,
    ) -> Result<Self, AdminError> {
        config.validate()?;
        let password = config.resolve_password()?;
        let tls = if config.tls {
            Some(config.tls_config()?)
        } else {
            None
        };
        let client_id = format!("deskhand-{node_id}-admin-{}", std::process::id());
        let options = config.mqtt_options(client_id, password, tls);
        let (client, mut eventloop) = AsyncClient::new(options, 1024);
        let connack = async {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => return Ok(()),
                    Ok(_) => {}
                    Err(e) => return Err(AdminError::Connect(describe_connection_error(&e))),
                }
            }
        };
        tokio::time::timeout(timeout, connack)
            .await
            .map_err(|_| AdminError::Timeout(timeout))??;
        Ok(Self {
            client,
            eventloop,
            topics: Topics::new(&config.topic_prefix, &config.discovery_prefix, node_id),
        })
    }

    pub fn topics(&self) -> &Topics {
        &self.topics
    }

    /// Every topic a Deskhand daemon for this node may have retained.
    pub fn owned_filters(&self) -> Vec<String> {
        vec![
            self.topics.device_config(),
            self.topics.media_player_config(),
            self.topics.base_filter(),
        ]
    }

    /// Subscribes to `filters` and collects the retained messages the broker
    /// sends back, until nothing new arrives for `quiet` (or `max` passes).
    /// Empty payloads (already deleted) are skipped.
    pub async fn read_retained(
        &mut self,
        filters: &[String],
        quiet: Duration,
        max: Duration,
    ) -> Result<BTreeMap<String, Bytes>, AdminError> {
        for f in filters {
            self.client
                .subscribe(f, QoS::AtLeastOnce)
                .await
                .map_err(|e| AdminError::Lost(e.to_string()))?;
        }
        let mut retained = BTreeMap::new();
        let mut pending_subacks = filters.len();
        let hard_deadline = Instant::now() + max;
        let mut quiet_deadline = Instant::now() + quiet;
        loop {
            let deadline = if pending_subacks == 0 {
                quiet_deadline.min(hard_deadline)
            } else {
                hard_deadline
            };
            let ev = match tokio::time::timeout_at(deadline, self.eventloop.poll()).await {
                Err(_) if pending_subacks > 0 => return Err(AdminError::Timeout(max)),
                Err(_) => break,
                Ok(Err(e)) => return Err(AdminError::Lost(describe_connection_error(&e))),
                Ok(Ok(ev)) => ev,
            };
            match ev {
                Event::Incoming(Packet::SubAck(_)) => {
                    pending_subacks = pending_subacks.saturating_sub(1);
                    quiet_deadline = Instant::now() + quiet;
                }
                Event::Incoming(Packet::Publish(p)) if p.retain => {
                    if !p.payload.is_empty() {
                        retained.insert(p.topic, p.payload);
                    }
                    quiet_deadline = Instant::now() + quiet;
                }
                _ => {}
            }
        }
        for f in filters {
            let _ = self.client.unsubscribe(f).await;
        }
        Ok(retained)
    }

    /// Deletes retained messages by publishing empty retained payloads, and
    /// waits until the broker has acknowledged every one.
    pub async fn clear(&mut self, topics: &[String], timeout: Duration) -> Result<(), AdminError> {
        // HA removes the device when its discovery document goes, so clear
        // that first; states published afterwards cannot recreate entities.
        let mut ordered: Vec<&String> = topics.iter().collect();
        ordered.sort_by_key(|t| !t.ends_with("/config"));
        let mut outstanding = 0usize;
        let deadline = Instant::now() + timeout;
        for t in ordered {
            // `try_publish` + polling: the request queue is drained by `poll`,
            // so a long list must not block on a full queue.
            loop {
                match self
                    .client
                    .try_publish(t.as_str(), QoS::AtLeastOnce, true, Vec::new())
                {
                    Ok(()) => break,
                    Err(_) => self.poll_once(deadline, &mut outstanding).await?,
                }
            }
            outstanding += 1;
        }
        while outstanding > 0 {
            self.poll_once(deadline, &mut outstanding).await?;
        }
        Ok(())
    }

    async fn poll_once(
        &mut self,
        deadline: Instant,
        outstanding: &mut usize,
    ) -> Result<(), AdminError> {
        match tokio::time::timeout_at(deadline, self.eventloop.poll()).await {
            Err(_) => Err(AdminError::Timeout(
                deadline.saturating_duration_since(Instant::now()),
            )),
            Ok(Err(e)) => Err(AdminError::Lost(describe_connection_error(&e))),
            Ok(Ok(Event::Incoming(Packet::PubAck(_)))) => {
                *outstanding = outstanding.saturating_sub(1);
                Ok(())
            }
            Ok(Ok(_)) => Ok(()),
        }
    }

    /// Disconnects cleanly (no last will is configured, so nothing is published).
    pub async fn disconnect(mut self) {
        if self.client.try_disconnect().is_err() {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match self.eventloop.poll().await {
                    Ok(Event::Outgoing(Outgoing::Disconnect)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readable_errors() {
        let e = ConnectionError::ConnectionRefused(ConnectReturnCode::BadUserNamePassword);
        assert!(describe_connection_error(&e).contains("username or password"));
        let e = ConnectionError::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        assert!(describe_connection_error(&e).contains("nothing is listening"));
    }

    #[tokio::test]
    async fn refused_connection_is_reported() {
        // Port 1 on localhost: nothing listens there.
        let config = MqttConfig {
            host: "127.0.0.1".into(),
            port: Some(1),
            ..MqttConfig::default()
        };
        let err = AdminClient::connect(&config, "node", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("refused"), "{err}");
    }
}
