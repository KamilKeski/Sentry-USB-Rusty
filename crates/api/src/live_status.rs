//! `live_status` WebSocket push: `/api/status`, `/api/drives/status` and
//! `/api/system/ble-latest-sample` in one event,
//! `{"status": ..., "drive": ..., "car": ...}`, each exactly as its endpoint
//! returns it (`car` is `{"ts": null}` with no car data, and left out when
//! the sample couldn't be built so clients keep their last one), so clients
//! can stop polling them while connected.
//!
//! Nothing is computed while no WebSocket client is connected. A client gets
//! a copy as soon as it connects, then whenever any part changes (checked
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

/// Car-sample ages, derived from the clock on every read. A new reading
/// changes the sample's `ts` or values; the heartbeat refreshes the ages.
const VOLATILE_CAR_KEYS: &[&str] = &[
    "seconds_ago",
    "field_secs_ago",
    "shift_state_seconds_ago",
    "body_controller_seconds_ago",
];

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
            let car = crate::ble::latest_car_sample(&state).await;

            let fingerprint = fingerprint(&status, &drive, car.as_ref());
            let due = greeting
                || match &last_sent {
                    Some((sent, at)) => *sent != fingerprint || at.elapsed() >= HEARTBEAT_INTERVAL,
                    None => true,
                };
            if due {
                let mut event = serde_json::json!({ "status": status, "drive": drive });
                if let Some(car) = car {
                    event["car"] = car;
                }
                state.hub.broadcast("live_status", event);
                last_sent = Some((fingerprint, Instant::now()));
            }
        }
    });
}

/// The parts of a snapshot whose change is worth an immediate push.
fn fingerprint(
    status: &serde_json::Value,
    drive: &serde_json::Value,
    car: Option<&serde_json::Value>,
) -> serde_json::Value {
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
        car.map(|car| without(car, VOLATILE_CAR_KEYS)),
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
        let car = json!({"ts": 900, "seconds_ago": 4, "battery_pct": 80.0, "shift_state": "Drive",
            "shift_state_seconds_ago": 3, "body_controller_seconds_ago": 1,
            "field_secs_ago": {"battery_pct": 20, "tire_fl": 7200}});
        let later_status = json!({"uptime": "1h 1m", "cpu_temp": "51000", "drives_active": "yes"});
        let later_drive = json!({"running": true, "process_current": 3, "process_sampled_at": 102});
        let later_car = json!({"ts": 900, "seconds_ago": 6, "battery_pct": 80.0, "shift_state": "Drive",
            "shift_state_seconds_ago": 5, "body_controller_seconds_ago": 3,
            "field_secs_ago": {"battery_pct": 22, "tire_fl": 7202}});
        assert_eq!(
            fingerprint(&status, &drive, Some(&car)),
            fingerprint(&later_status, &later_drive, Some(&later_car)),
        );
    }

    #[test]
    fn progress_and_state_changes_count() {
        let status = json!({"uptime": "1h", "drives_active": "yes"});
        let drive = json!({"running": true, "process_current": 3});
        assert_ne!(
            fingerprint(&status, &drive, None),
            fingerprint(&status, &json!({"running": true, "process_current": 4}), None),
        );
        assert_ne!(
            fingerprint(&status, &drive, None),
            fingerprint(&json!({"uptime": "1h", "drives_active": "no"}), &drive, None),
        );
    }

    #[test]
    fn car_readings_count_as_changes() {
        let status = json!({"drives_active": "yes"});
        let drive = json!({"running": false});
        let car = json!({"ts": 900, "seconds_ago": 4, "shift_state": "Drive", "battery_pct": 80.0});
        let fp = |car: serde_json::Value| fingerprint(&status, &drive, Some(&car));
        assert_ne!(fp(car.clone()), fp(json!({"ts": 905, "seconds_ago": 0, "shift_state": "Drive", "battery_pct": 80.0})));
        assert_ne!(fp(car.clone()), fp(json!({"ts": 900, "seconds_ago": 4, "shift_state": "Park", "battery_pct": 80.0})));
        assert_ne!(fp(car.clone()), fp(json!({"ts": 900, "seconds_ago": 4, "shift_state": null, "battery_pct": 80.0})));
        assert_ne!(fp(car), fp(json!({"ts": null})));
    }
}
