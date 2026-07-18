//! Phytech scraper.
//!
//! This crate is DB-less: it scrapes the Phytech API, applies provider-specific value
//! scaling, and pushes sensors + measurements to the `server` crate over HTTP. All DB
//! writes happen there.

use anyhow::Context;
use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use futures::StreamExt;
use serde::{Deserialize, Deserializer};
use serde::de::Error;
use tokio::time::{Duration, sleep};
use tracing::{error, info, warn};

use api_types::{
    InsertMeasurementsRequest, Measurement, UpsertSensorRequest, UpsertSensorResponse,
};

const ISRAEL_STANDARD_TIMEZONE: FixedOffset = FixedOffset::east_opt(2 * 3600).unwrap();

// ── Plot / Project discovery types ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PlotsResponse {
    plots: Vec<Plot>,
}

#[derive(Debug, Deserialize)]
struct Plot {
    id: i32,
    plot_name: String,
}

#[derive(Debug, Deserialize)]
struct Project {
    id: i32,
    plot_id: i32,
    name: String,
    state: String,
}

// ── Sensor / measurement types (Phytech API response) ───────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct MeasurementSource {
    pub sensor_id: Option<String>,
    pub title: String,
    pub category: String,
    pub measurement_unit: Option<String>,
    pub measurement_time_unit: Option<String>,
    pub measurement_calc_type: Option<String>,
    pub depth_value: Option<f64>,
    pub depth_unit: Option<String>,
}

fn get_scale_factor(category: &str, unit: Option<&str>) -> Option<f64> {
    match (category.to_uppercase().as_str(), unit) {
        ("CLIMATE", Some("C")) => Some(0.1),
        ("CLIMATE", Some("%")) => Some(0.1),
        ("PLANT", _) => Some(1.0),
        ("SOIL", Some("%")) => Some(0.05),
        ("SOIL", Some("C")) => Some(0.1),
        ("SOIL", Some("cBar")) => Some(0.1),
        ("IRRIGATION", Some("kPa")) => Some(2.0),
        ("FRUIT", Some("mm")) => Some(0.001),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct MeasurementResponse {
    measurements: Option<Vec<PhytechMeasurement>>,
}

/// A measurement as returned by Phytech's API (timestamp is Israel standard time,
/// interpreted as IST then converted to UTC).
#[derive(Debug, Deserialize)]
struct PhytechMeasurement {
    value: f64,
    #[serde(deserialize_with = "deserialize_israel_standard_time_milliseconds")]
    time: DateTime<Utc>,
}

fn deserialize_israel_standard_time_milliseconds<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<DateTime<Utc>, D::Error> {
    let milliseconds: i64 = Deserialize::deserialize(d)?;
    let naive_time = DateTime::from_timestamp_millis(milliseconds)
        .ok_or(D::Error::custom("Invalid timestamp"))?
        .naive_utc();
    let ist_time: DateTime<FixedOffset> =
        ISRAEL_STANDARD_TIMEZONE.from_local_datetime(&naive_time).unwrap();
    Ok(ist_time.with_timezone(&Utc))
}

#[derive(Debug, Deserialize)]
struct User {
    jwt_token: String,
}

#[derive(Debug, Deserialize)]
struct SignInResponse {
    api_token: String,
    user: User,
}

async fn sign_in(email: &str, password: &str) -> anyhow::Result<SignInResponse> {
    let client = reqwest::Client::new();
    let resp: SignInResponse = client
        .post("https://api.phytech.com/users/sign_in")
        .json(&serde_json::json!({
            "user": {
                "email": email,
                "mfa_method": "",
                "mfa_token": "",
                "password": password,
            }
        }))
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp)
}

// ── Discovery helpers ───────────────────────────────────────────────────────

async fn fetch_plots(client: &reqwest::Client) -> anyhow::Result<Vec<Plot>> {
    info!("Fetching all installed plots");
    let resp: PlotsResponse = client
        .get("https://api.phytech.com/api/v2/plots?statuses[]=installed")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    info!(count = resp.plots.len(), "Discovered plots");
    for p in &resp.plots {
        info!(plot_id = p.id, name = p.plot_name, "  plot");
    }
    Ok(resp.plots)
}

async fn fetch_projects(
    client: &reqwest::Client,
    plot_ids: &[i32],
) -> anyhow::Result<Vec<Project>> {
    let query_params: String = plot_ids
        .iter()
        .map(|id| format!("plot_ids[]={}", id))
        .collect::<Vec<_>>()
        .join("&");

    let url = format!("https://api.phytech.com/api/v2/projects?{}", query_params);
    info!("Fetching projects for {} plots", plot_ids.len());

    let projects: Vec<Project> = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    info!(count = projects.len(), "Discovered projects");
    for p in &projects {
        info!(
            project_id = p.id,
            plot_id = p.plot_id,
            name = p.name,
            state = p.state,
            "  project"
        );
    }
    Ok(projects)
}

// ── Main entry point ────────────────────────────────────────────────────────

pub async fn run_scrape(
    server_url: &str,
    email: &str,
    password: &str,
    server_auth_token: &str,
) -> anyhow::Result<()> {
    let mut server_headers = reqwest::header::HeaderMap::new();
    server_headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {}", server_auth_token)
            .parse()
            .context("Invalid server auth token format")?,
    );
    let server_client = reqwest::ClientBuilder::new()
        .default_headers(server_headers)
        .build()
        .context("Failed to build server HTTP client")?;

    let sign_in_response = sign_in(email, password)
        .await
        .context("Failed to sign in")?;
    let api_token = sign_in_response.api_token;
    let jwt = sign_in_response.user.jwt_token;
    let mut api_token_headers = reqwest::header::HeaderMap::new();
    api_token_headers.insert(
        "Authorization",
        format!("Token token={}", api_token)
            .parse()
            .context("Invalid API token format")?,
    );
    let api_token_client = reqwest::ClientBuilder::new()
        .default_headers(api_token_headers)
        .build()
        .context("Failed to build HTTP client")?;
    // 1. Discover plots (uses api_token for api.phytech.com)
    let plots = fetch_plots(&api_token_client)
        .await
        .context("Failed to fetch plots")?;
    let plot_ids: Vec<i32> = plots.iter().map(|p| p.id).collect();

    if plot_ids.is_empty() {
        warn!("No installed plots found — nothing to scrape");
        return Ok(());
    }

    // 2. Discover projects for those plots (uses api_token for api.phytech.com)
    let projects = fetch_projects(&api_token_client, &plot_ids)
        .await
        .context("Failed to fetch projects")?;

    info!(total = projects.len(), "Discovered projects to scrape");

    // 3. Scrape each project (uses JWT for japi.phytech.com)
    let mut jwt_headers = reqwest::header::HeaderMap::new();
    jwt_headers.insert("Authorization", jwt.parse().context("Invalid JWT format")?);
    let jwt_client = reqwest::ClientBuilder::new()
        .default_headers(jwt_headers)
        .build()
        .context("Failed to build HTTP client")?;

    for project in &projects {
        if let Err(e) =
            scrape_project(&jwt_client, &server_client, server_url, project.id).await
        {
            error!(project_id = project.id, "Failed to scrape project: {:?}", e);
        }
    }

    Ok(())
}

// ── Per-project sensor scraping ──────────────────────────────────────────────

async fn scrape_project(
    client: &reqwest::Client,
    server_client: &reqwest::Client,
    server_url: &str,
    project_id: i32,
) -> anyhow::Result<()> {
    info!(project_id, "Scraping project — discovering sensors");

    let url = format!(
        "https://japi.phytech.com/api/v3/web/projects/{}/report_measurements",
        project_id
    );
    let sources: Vec<MeasurementSource> = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // Filter to only RAW measurement sources that have a sensor_id
    let raw_sources: Vec<&MeasurementSource> = sources
        .iter()
        .filter(|s| s.sensor_id.is_some() && s.measurement_calc_type.as_deref() == Some("RAW"))
        .collect();

    info!(
        project_id,
        sensor_count = raw_sources.len(),
        "Found RAW sensor sources"
    );

    // Process up to 10 sensors concurrently
    let fetches = futures::stream::iter(raw_sources.into_iter().map(|source| {
        let sensor_id = source.sensor_id.clone().unwrap();
        let base_url = url.clone();
        async move {
            if let Err(e) =
                scrape_sensor(client, server_client, server_url, &base_url, source, &sensor_id)
                    .await
            {
                warn!(sensor_id, "Failed to scrape sensor after retries: {:?}", e);
            }
        }
    }))
    .buffer_unordered(10);

    fetches.collect::<Vec<()>>().await;

    Ok(())
}

async fn fetch_measurements_with_retry(
    client: &reqwest::Client,
    url: &str,
    sensor_id: &str,
) -> anyhow::Result<Option<Vec<PhytechMeasurement>>> {
    const MAX_RETRIES: u32 = 3;
    let mut attempt = 0;

    loop {
        attempt += 1;
        match client.get(url).send().await {
            Ok(resp) => match resp.error_for_status() {
                Ok(resp) => {
                    let parsed: MeasurementResponse = resp.json().await?;
                    return Ok(parsed.measurements);
                }
                Err(e) => {
                    if attempt >= MAX_RETRIES {
                        return Err(e.into());
                    }
                    warn!(sensor_id, attempt, "API returned error, retrying...");
                }
            },
            Err(e) => {
                if attempt >= MAX_RETRIES {
                    return Err(e.into());
                }
                warn!(sensor_id, attempt, "Request failed, retrying...");
            }
        }
        sleep(Duration::from_millis(500 * attempt as u64)).await;
    }
}

async fn upsert_sensor(
    server_client: &reqwest::Client,
    server_url: &str,
    source: &MeasurementSource,
    sensor_id: &str,
) -> anyhow::Result<UpsertSensorResponse> {
    let resp = server_client
        .post(format!("{}/sensors", server_url))
        .json(&UpsertSensorRequest {
            external_id: sensor_id.to_string(),
            provider: "phytech".to_string(),
            category: source.category.clone(),
            measurement_unit: source.measurement_unit.clone(),
            depth_value: source.depth_value,
            depth_unit: source.depth_unit.clone(),
        })
        .send()
        .await
        .context("Failed to upsert sensor")?
        .error_for_status()
        .context("Server rejected sensor upsert")?
        .json::<UpsertSensorResponse>()
        .await
        .context("Failed to parse sensor upsert response")?;
    Ok(resp)
}

async fn scrape_sensor(
    client: &reqwest::Client,
    server_client: &reqwest::Client,
    server_url: &str,
    base_url: &str,
    source: &MeasurementSource,
    sensor_id: &str,
) -> anyhow::Result<()> {
    // Upsert sensor and learn its internal id + the latest measurement we have.
    let upsert_response = upsert_sensor(server_client, server_url, source, sensor_id).await?;
    let internal_sensor_id = upsert_response.sensor_id;

    // Fetch measurements from the API
    let measurements_url = format!(
        "{}?measurement_source_id={}&measurement_source_type=SENSOR&measurement_calc_type={}&measurement_time_unit={}",
        base_url,
        sensor_id,
        source.measurement_calc_type.as_deref().unwrap_or(""),
        source.measurement_time_unit.as_deref().unwrap_or("")
    );

    let measurements =
        match fetch_measurements_with_retry(client, &measurements_url, sensor_id).await? {
            Some(m) => m,
            None => {
                info!(sensor_id, "No measurements returned from API");
                return Ok(());
            }
        };

    // Filter to only new measurements (after the last one we stored)
    let new_measurements: Vec<&PhytechMeasurement> = measurements
        .iter()
        .filter(|m| match upsert_response.last_measured_at {
            Some(last) => m.time > last,
            None => true, // no existing data, take everything
        })
        .collect();

    if new_measurements.is_empty() {
        info!(sensor_id, "No new measurements to insert");
        return Ok(());
    }

    info!(
        sensor_id,
        total = measurements.len(),
        new = new_measurements.len(),
        "Inserting new measurements"
    );

    let scale_factor = match get_scale_factor(&source.category, source.measurement_unit.as_deref())
    {
        Some(f) => f,
        None => {
            error!(
                sensor_id,
                category = %source.category,
                unit = ?source.measurement_unit,
                "Unknown category and unit combination for sensor"
            );
            return Ok(());
        }
    };

    // Map to the API contract, applying the provider-specific scale factor, and push in
    // 1000-row batches so each request stays well under the Postgres parameter limit.
    let new_measurements: Vec<Measurement> = new_measurements
        .iter()
        .map(|m| Measurement {
            value: m.value * scale_factor,
            measured_at: m.time,
        })
        .collect();

    let insert_url = format!("{}/sensors/{}/measurements", server_url, internal_sensor_id);
    for chunk in new_measurements.chunks(1000) {
        let response = server_client
            .post(&insert_url)
            .json(&InsertMeasurementsRequest {
                measurements: chunk.to_vec(),
            })
            .send()
            .await
            .context("Failed to insert measurements")?
            .error_for_status()
            .context("Server rejected measurements insert")?;
        // Drain the body so the connection can be reused.
        let _ = response.bytes().await;
    }

    Ok(())
}
