//! `live_status` WebSocket push: `/api/status` and `/api/drives/status` in
//! one event, `{"status": ..., "drive": ...}`, each exactly as its endpoint
//! returns it, so clients can stop polling both while connected.
//!
//! Nothing is computed while no WebSocket client is connected. A client gets
//! a copy as soon as it connects, then whenever either part changes (checked
//! every 2 s), and a heartbeat copy at least every 15 s.

use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;

use crate::router::AppState;

const CHECK_INTERVAL: Duration = Duration::from_secs(2);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// `/api/status` fields that move on every sample (counters, sensors,
/// clocks). Left out of change detection; the heartbeat carries them.
const VOLATILE_STATUS_KEYS: &[&str] = &[
    "cpu_temp",
    "uptime",
    "cam_last_write_secs",
    "wifi_strength",
    "wifi_signal_dbm",
    "supply_voltage",
    "fan_speed",
    "wifi_rx_bps",
    "wifi_tx_bps",
    "ether_rx_bps",
    "ether_tx_bps",
    "wifi_sample_age_ms",
    "ether_sample_age_ms",
    "sampled_at",
];

/// `/api/drives/status` fields that tick without progress changing.
const VOLATILE_DRIVE_KEYS: &[&str] = &["process_sampled_at"];

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut last_sent: Option<(serde_json::Value, Instant)> = None;
        loop {
            let greeting = tokio::select! {
                _ = state.hub.client_connected() => true,
                _ = tokio::time::sleep(CHECK_INTERVAL) => false,
            };
            if !state.hub.has_clients() {
                last_sent = None;
                continue;
            }
            let (code, status) = crate::status::get_status(State(state.clone())).await;
            if code != StatusCode::OK {
                continue;
            }
            let (_, drive) = crate::drives_handler::processing_status(State(state.clone())).await;
            let (status, drive) = (status.0, drive.0);

            let fingerprint = fingerprint(&status, &drive);
            let due = greeting
                || match &last_sent {
                    Some((sent, at)) => *sent != fingerprint || at.elapsed() >= HEARTBEAT_INTERVAL,
                    None => true,
                };
            if due {
                state.hub.broadcast(
                    "live_status",
                    serde_json::json!({ "status": status, "drive": drive }),
                );
                last_sent = Some((fingerprint, Instant::now()));
            }
        }
    });
}

/// The parts of a snapshot whose change is worth an immediate push.
fn fingerprint(status: &serde_json::Value, drive: &serde_json::Value) -> serde_json::Value {
    let without = |value: &serde_json::Value, keys: &[&str]| {
        let mut value = value.clone();
        if let Some(map) = value.as_object_mut() {
            for key in keys {
                map.remove(*key);
            }
        }
        value
    };
    serde_json::json!([
        without(status, VOLATILE_STATUS_KEYS),
        without(drive, VOLATILE_DRIVE_KEYS),
    ])
}

#[cfg(test)]
mod tests {
    use super::fingerprint;
    use serde_json::json;

    #[test]
    fn ticking_fields_do_not_count_as_changes() {
        let status = json!({"uptime": "1h", "cpu_temp": "50000", "drives_active": "yes"});
        let drive = json!({"running": true, "process_current": 3, "process_sampled_at": 100});
        let later_status = json!({"uptime": "1h 1m", "cpu_temp": "51000", "drives_active": "yes"});
        let later_drive = json!({"running": true, "process_current": 3, "process_sampled_at": 102});
        assert_eq!(fingerprint(&status, &drive), fingerprint(&later_status, &later_drive));
    }

    #[test]
    fn progress_and_state_changes_count() {
        let status = json!({"uptime": "1h", "drives_active": "yes"});
        let drive = json!({"running": true, "process_current": 3});
        assert_ne!(
            fingerprint(&status, &drive),
            fingerprint(&status, &json!({"running": true, "process_current": 4})),
        );
        assert_ne!(
            fingerprint(&status, &drive),
            fingerprint(&json!({"uptime": "1h", "drives_active": "no"}), &drive),
        );
    }
}
