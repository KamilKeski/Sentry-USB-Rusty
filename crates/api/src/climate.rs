//! Cabin climate controls sent over BLE. Like the charge limit, these may
//! target a sleeping car: `sentryusb-ble-action` wakes it before sending.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use std::time::Duration;

use crate::router::AppState;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClimateActionRequest {
    action: String,
    value: Option<f64>,
}

/// Map a request to its BLE verb. Setpoints match the verb parser: 15.0–28.0
/// °C with at most one decimal place.
fn climate_action_verb(req: &ClimateActionRequest) -> Result<String, String> {
    match req.action.as_str() {
        "on" => Ok("climate-on".into()),
        "off" => Ok("climate-off".into()),
        "setTemp" => {
            let value = req.value.ok_or_else(|| "temperature value is required".to_string())?;
            if !value.is_finite() || !(15.0..=28.0).contains(&value) {
                return Err("temperature must be between 15.0 and 28.0 °C".into());
            }
            let tenths = value * 10.0;
            if (tenths - tenths.round()).abs() > 1e-6 {
                return Err("temperature allows at most one decimal place".into());
            }
            Ok(format!("climate-set-temp:{value:.1}"))
        }
        _ => Err("action must be on, off, or setTemp".into()),
    }
}

/// POST /api/climate/action, gated on BLE health only (the car may be asleep).
pub async fn climate_action(
    State(state): State<AppState>,
    Json(req): Json<ClimateActionRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let verb = match climate_action_verb(&req) {
        Ok(verb) => verb,
        Err(message) => return crate::json_error(StatusCode::BAD_REQUEST, &message),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let store = state.drives.store.clone();
    let health_ok = match tokio::task::spawn_blocking(move || {
        store.with_read_conn(|conn| -> rusqlite::Result<bool> {
            Ok(crate::ble::wake_controls_health_is_ok(conn, now))
        })
    })
    .await
    {
        Ok(Ok(ok)) => ok,
        Ok(Err(e)) => {
            return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("BLE health read failed: {e}"));
        }
        Err(e) => {
            return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("BLE health task failed: {e}"));
        }
    };
    if !health_ok {
        return crate::json_error(
            StatusCode::CONFLICT,
            "climate controls require a healthy authenticated BLE connection",
        );
    }

    match sentryusb_shell::run_with_timeout(
        Duration::from_secs(30),
        "/root/bin/sentryusb-ble-action",
        &[verb.as_str()],
    )
    .await
    {
        Ok(_) => {
            crate::ble::invalidate_latest_sample();
            let _ = sentryusb_shell::run(
                "systemctl",
                &["kill", "-s", "SIGUSR1", "sentryusb-telemetry"],
            )
            .await;
            (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
        }
        Err(e) => crate::json_error(
            StatusCode::BAD_GATEWAY,
            &format!("climate command failed: {e}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(action: &str, value: Option<f64>) -> ClimateActionRequest {
        ClimateActionRequest { action: action.into(), value }
    }

    #[test]
    fn climate_actions_map_to_ble_verbs() {
        assert_eq!(climate_action_verb(&req("on", None)).unwrap(), "climate-on");
        assert_eq!(climate_action_verb(&req("off", Some(20.0))).unwrap(), "climate-off");
        assert_eq!(climate_action_verb(&req("setTemp", Some(21.5))).unwrap(), "climate-set-temp:21.5");
        assert_eq!(climate_action_verb(&req("setTemp", Some(20.0))).unwrap(), "climate-set-temp:20.0");
        assert!(climate_action_verb(&req("defrost", None)).is_err());
    }

    #[test]
    fn set_temp_validates_range_and_precision() {
        assert!(climate_action_verb(&req("setTemp", None)).is_err());
        assert!(climate_action_verb(&req("setTemp", Some(15.0))).is_ok());
        assert!(climate_action_verb(&req("setTemp", Some(28.0))).is_ok());
        assert!(climate_action_verb(&req("setTemp", Some(14.9))).is_err());
        assert!(climate_action_verb(&req("setTemp", Some(28.1))).is_err());
        assert!(climate_action_verb(&req("setTemp", Some(21.25))).is_err());
        assert!(climate_action_verb(&req("setTemp", Some(f64::NAN))).is_err());
    }
}
