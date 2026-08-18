//! RealiteQ (Topco) webhook handler.
//!
//! Each incoming value change becomes a measurement for a `topco`-provider
//! sensor whose `external_id` is the node `path`, reusing the shared sensor
//! upsert / measurement insert that [`crate::handlers`] exposes so no
//! webhook-specific SQL lives here.

use std::collections::HashMap;

use axum::{
    Form, Json,
    extract::{FromRequest, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::PgPool;
use tracing::warn;

use api_types::Measurement;

use super::{ApiError, bad_request, insert_measurements_db, upsert_sensor_db};

/// The `provider` value recorded for sensors fed by RealiteQ (Topco) webhooks.
/// Stored as plain text alongside the existing `phytech` / `tera` providers.
const TOPCO_PROVIDER: &str = "topco";

/// A single RealiteQ node value change. The same fields arrive whether the
/// change is sent on its own (unbuffered mode, form-urlencoded) or batched in an
/// array (buffered mode, JSON), so one `Deserialize` type covers both.
///
/// Field meanings (from the RealiteQ webhook spec):
///   path              — the node's path, e.g. /icex1/r1500 (used as external_id)
///   raw_stamp         — UTC unix epoch (seconds); used as measured_at
///   value             — the node's formatted value; here configured to emit
///                       just the measurement unit, which is stored on the sensor
///   unformatted_value — raw numeric string representation of the current value
///   datatype          — 0=string,1=integer,2=floating point,3=boolean
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TopcoValueChange {
    path: String,
    raw_stamp: i64,
    unformatted_value: String,
    #[serde(rename = "value")]
    measurement_unit: String,
    datatype: String,
}

/// Webhook body normalized to a flat list of value changes.
///
/// RealiteQ sends either a single change as `application/x-www-form-urlencoded`
/// (unbuffered) or a JSON array (buffered). Rather than reading bytes and
/// dispatching by hand, this delegates to axum's `Form` / `Json` extractors
/// based on the request's Content-Type, so each mode is parsed by the
/// extractor built for it.
pub struct TopcoPayload(Vec<TopcoValueChange>);

impl<S> FromRequest<S> for TopcoPayload
where
    S: Send + Sync,
    Json<Vec<TopcoValueChange>>: FromRequest<S>,
    Form<TopcoValueChange>: FromRequest<S>,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let is_json = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("application/json"));

        if is_json {
            let Json(changes) = Json::<Vec<TopcoValueChange>>::from_request(req, state)
                .await
                .map_err(|rejection| rejection.into_response())?;
            Ok(TopcoPayload(changes))
        } else {
            let Form(change) = Form::<TopcoValueChange>::from_request(req, state)
                .await
                .map_err(|rejection| rejection.into_response())?;
            Ok(TopcoPayload(vec![change]))
        }
    }
}

/// `POST /webhooks/topco` — receive a RealiteQ (Topco) remote update.
///
/// Each change becomes a measurement for a `topco`-provider sensor whose
/// `external_id` is the node `path`, reusing the same sensor upsert and
/// measurement insert the scraper uses. The measurement timestamp comes from
/// `raw_stamp` (UTC unix seconds). Non-numeric values are logged and skipped,
/// not rejected — RealiteQ does not retry failed requests, so a 200 is returned
/// regardless of per-change parse issues (only a malformed body or a DB error
/// yields 4xx/5xx).
pub async fn topco_webhook(
    State(state): State<crate::handlers::AppState>,
    TopcoPayload(changes): TopcoPayload,
) -> Result<StatusCode, ApiError> {
    if changes.is_empty() {
        return Ok(StatusCode::OK);
    }

    store_topco_changes(&state.pool, &changes).await?;
    Ok(StatusCode::OK)
}

/// Persist a batch of RealiteQ value changes, grouping them by node `path` so
/// each sensor is upserted once and its measurements inserted in one batch.
/// Both writes reuse [`upsert_sensor_db`] / [`insert_measurements_db`], so no
/// webhook-specific SQL is maintained here.
async fn store_topco_changes(pool: &PgPool, changes: &[TopcoValueChange]) -> Result<(), ApiError> {
    // Group measurements by node path (external_id), parsing each value. The
    // RealiteQ `value` field carries the node's measurement unit (the node is
    // formatted to emit the unit, not the reading), so it is captured once per
    // node and stored on the sensor.
    let mut groups: HashMap<String, (String, Vec<Measurement>)> = HashMap::new();
    for change in changes {
        let Some(value) = parse_topco_value(change) else {
            warn!(
                path = %change.path,
                datatype = change.datatype,
                measurement_unit = %change.measurement_unit,
                "skipping non-numeric topco value"
            );
            continue;
        };

        let measured_at =
            DateTime::<Utc>::from_timestamp(change.raw_stamp, 0).ok_or_else(|| {
                bad_request(format!(
                    "raw_stamp is not a valid epoch timestamp: {}",
                    change.raw_stamp
                ))
            })?;

        let group = groups
            .entry(change.path.clone())
            .or_insert_with(|| (change.measurement_unit.clone(), Vec::new()));
        group.1.push(Measurement::number(value, measured_at));
    }

    for (path, (measurement_unit, measurements)) in groups {
        let sensor_id = upsert_sensor_db(
            pool,
            &path,
            TOPCO_PROVIDER,
            None,
            Some(&measurement_unit),
            None,
            None,
            api_types::ResponseType::Numeric.as_db_str(),
        )
        .await?;
        insert_measurements_db(pool, sensor_id, &measurements).await?;
    }

    Ok(())
}

/// Decode a RealiteQ value change's current value to an `f64` for storage.
///
/// Prefers `unformatted_value` (the raw numeric form) and falls back to the
/// formatted `value`. Boolean values map to 1.0 / 0.0; integers, floats, and
/// even string-typed values are attempted as a numeric parse. Returns `None`
/// when nothing numeric can be extracted, in which case the caller skips it.
fn parse_topco_value(change: &TopcoValueChange) -> Option<f64> {
    if change.unformatted_value.is_empty() {
        return None;
    }
    let raw = &change.unformatted_value;
    match change.datatype.as_str() {
        // boolean(3)
        "3" => match raw.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "1.0" => Some(1.0),
            "false" | "0" | "no" | "0.0" => Some(0.0),
            _ => None,
        },
        // integer(1), floating point(2), string(0): best-effort numeric parse.
        _ => raw.parse::<f64>().ok(),
    }
}
