use anyhow::Context;
use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use futures::StreamExt;
use serde::{Deserialize, Deserializer};
use serde::de::Error;
use sqlx::{PgPool, QueryBuilder};
use tokio::time::{Duration, sleep};
use tracing::{error, info, warn};

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

// ── Sensor / measurement types (unchanged) ──────────────────────────────────

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
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
pub struct MeasurementResponse {
    pub measurements: Option<Vec<Measurement>>,
}

#[derive(Debug, Deserialize)]
pub struct Measurement {
    pub value: f64,
    #[serde(deserialize_with = "deserialize_israel_standard_time_milliseconds")]
    pub time: DateTime<Utc>,
}

fn deserialize_israel_standard_time_milliseconds<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
    let milliseconds: i64 = Deserialize::deserialize(d)?;
    let naive_time = DateTime::from_timestamp_millis(milliseconds)
        .ok_or(D::Error::custom("Invalid timestamp"))?
        .naive_utc();
    let ist_time: DateTime<FixedOffset> = ISRAEL_STANDARD_TIMEZONE.from_local_datetime(&naive_time).unwrap();
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

pub async fn run_scrape(pool: &PgPool, email: &str, password: &str) -> anyhow::Result<()> {
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
        if let Err(e) = scrape_project(&jwt_client, pool, project.id).await {
            error!(project_id = project.id, "Failed to scrape project: {:?}", e);
        }
    }

    sqlx::query!("INSERT INTO scrapes DEFAULT VALUES")
        .execute(pool)
        .await?;

    Ok(())
}

// ── Per-project sensor scraping (unchanged) ─────────────────────────────────

async fn scrape_project(
    client: &reqwest::Client,
    pool: &PgPool,
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
            if let Err(e) = scrape_sensor(client, pool, &base_url, source, &sensor_id).await {
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
) -> anyhow::Result<Option<Vec<Measurement>>> {
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

async fn scrape_sensor(
    client: &reqwest::Client,
    pool: &PgPool,
    base_url: &str,
    source: &MeasurementSource,
    sensor_id: &str,
) -> anyhow::Result<()> {
    // Upsert sensor
    let internal_sensor_id = sqlx::query_scalar!(
        r#"
        INSERT INTO sensors (external_id, provider, category, measurement_unit, depth_value, depth_unit)
        VALUES ($1, 'phytech', $2, $3, $4, $5)
        ON CONFLICT (external_id, provider) DO UPDATE SET
            category = EXCLUDED.category,
            measurement_unit = EXCLUDED.measurement_unit,
            depth_value = EXCLUDED.depth_value,
            depth_unit = EXCLUDED.depth_unit
        RETURNING sensor_id as "sensor_id!"
        "#,
        sensor_id,
        source.category,
        source.measurement_unit,
        source.depth_value,
        source.depth_unit
    )
    .fetch_one(pool)
    .await?;

    // Find the latest measurement we already have for this sensor
    let last_measured_at: Option<DateTime<Utc>> = sqlx::query_scalar!(
        r#"
        SELECT MAX(measured_at) as "max_measured_at"
        FROM measurements
        WHERE sensor_id = $1
        "#,
        internal_sensor_id
    )
    .fetch_one(pool)
    .await?;

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
    let new_measurements: Vec<&Measurement> = measurements
        .iter()
        .filter(|m| {
            match last_measured_at {
                Some(last) => m.time > last,
                None => true, // no existing data, take everything
            }
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

    // Batch insert using sqlx::QueryBuilder mapping up to 10k parameters (~3k rows per insert)
    let scale_factor = match get_scale_factor(&source.category, source.measurement_unit.as_deref()) {
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

    for chunk in new_measurements.chunks(1000) {
        let mut query_builder: QueryBuilder<'_, sqlx::Postgres> =
            QueryBuilder::new("INSERT INTO measurements (sensor_id, value, measured_at) ");

        query_builder.push_values(chunk, |mut b, m| {
            b.push_bind(&internal_sensor_id)
                .push_bind(m.value * scale_factor)
                .push_bind(m.time);
        });

        query_builder.push(" ON CONFLICT (sensor_id, measured_at) DO NOTHING");

        query_builder.build().execute(pool).await?;
    }

    Ok(())
}
