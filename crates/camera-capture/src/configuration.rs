//! TOML configuration for the capture loop.
//!
//! Config path comes from `CAMERA_CONFIG` (default `cameras.toml`, relative to
//! the current working directory). The process runs on a Raspberry Pi on the
//! same network as the cameras, so camera URIs are plain `http://<lan-ip>:port`
//! while `server_url` points at the cloud `server`.

use std::collections::HashMap;

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Configuration {
    /// Base URL of the cloud `server` (HTTPS). The `/cameras/images` route is
    /// appended to it.
    pub server_url: String,
    /// Bearer token sent as `Authorization: Bearer <token>` to the server.
    pub auth_token: String,
    /// Site latitude in degrees, [-90, 90]. Shared by every camera that opts
    /// into `daylight_only`.
    pub latitude: f64,
    /// Site longitude in degrees, [-180, 180]. Shared by every camera that
    /// opts into `daylight_only`.
    pub longitude: f64,
    /// Vision-LLM prompts defined once by `prompt_id` and referenced from
    /// locations via `prompt_ids`. Keeping them here (rather than restating the
    /// same prompt at every site) lets many locations share one prompt. Each
    /// `prompt_id` must be unique; locations resolve their `prompt_ids` against
    /// this table at load time. Empty (the default) means no location can opt
    /// into analysis.
    #[serde(default)]
    pub prompts: Vec<api_types::AnalysisPrompt>,
    pub cameras: Vec<CameraConfig>,
}

#[derive(Debug, Deserialize)]
pub struct CameraConfig {
    /// Device host root, e.g. `http://192.168.1.50:8000`. The `service_path` is
    /// appended to reach the device service.
    pub uri: String,
    pub username: String,
    pub password: String,
    /// Seconds between captures of this camera. Each camera keeps its own
    /// cadence, independent of the others.
    pub interval_secs: u64,
    /// When true, this camera's ticks outside the daylight window
    /// (sunrise → sunset, adjusted by `daylight_margin_mins`) are skipped
    /// entirely — no moves, snapshots, or uploads. The site location comes
    /// from the global `latitude` / `longitude`, which must be set when any
    /// camera opts in here.
    #[serde(default)]
    pub daylight_only: bool,
    /// Minutes shaved off each edge of this camera's daylight window: the
    /// capture window becomes `[sunrise + margin, sunset - margin]`. A positive
    /// value drops the twilight edges where frames would be too dark; negative
    /// extends into twilight. Defaults to 0.
    #[serde(default = "default_daylight_margin")]
    pub daylight_margin_mins: i64,
    /// `"any"` (default) | `"digest"` | `"usernametoken"`.
    #[serde(default)]
    pub auth_type: Option<String>,
    /// ONVIF device-service path appended to `uri`.
    #[serde(default = "default_service_path")]
    pub service_path: String,
    /// Media profile token to drive; if omitted the first profile is used.
    #[serde(default)]
    pub profile_token: Option<String>,
    /// Compensate for a skewed camera clock (breaks WS-Security). Off by default.
    #[serde(default)]
    pub fix_time: bool,
    /// Seconds to wait after a PTZ move before grabbing the snapshot.
    #[serde(default = "default_settle")]
    pub settle_secs: f64,
    pub locations: Vec<LocationConfig>,
}

fn default_service_path() -> String {
    "onvif/device_service".to_string()
}

fn default_settle() -> f64 {
    2.0
}

fn default_daylight_margin() -> i64 {
    0
}

#[derive(Debug, Deserialize)]
pub struct LocationConfig {
    /// Identifier sent to the server as `camera_id`; the stored S3 key is built
    /// from it.
    pub camera_id: String,
    /// `preset = "<token>"` to recall a saved preset.
    #[serde(default)]
    pub preset: Option<String>,
    /// Absolute normalized target: pan/tilt in [-1, 1], zoom in [0, 1]. All
    /// three must be set together (and `preset` left unset) for an absolute move.
    #[serde(default)]
    pub pan: Option<f64>,
    #[serde(default)]
    pub tilt: Option<f64>,
    #[serde(default)]
    pub zoom: Option<f64>,
    /// `prompt_id`s of the top-level `[[prompts]]` to run against this
    /// location's images on upload. Resolved into [`prompts`] at load time;
    /// an unknown id fails fast rather than at capture. Empty (the default)
    /// uploads the image only, with no analysis. Each resolved prompt becomes
    /// a derived sensor keyed `{camera_id}_{prompt_id}` under the
    /// `image-analysis` provider.
    #[serde(default)]
    pub prompt_ids: Vec<String>,
    /// The prompts resolved from [`prompt_ids`] against the top-level
    /// `[[prompts]]` table, populated by `Configuration::validate`. Not read
    /// from TOML; sent as the `prompts` multipart part (a JSON array of
    /// `AnalysisPrompt`) on `POST /cameras/images`.
    #[serde(skip)]
    pub prompts: Vec<api_types::AnalysisPrompt>,
}

/// A resolved movement target, derived from a `LocationConfig`.
#[derive(Debug, Clone)]
pub enum LocationTarget {
    Preset { preset: String },
    Absolute { pan: f64, tilt: f64, zoom: f64 },
}

impl LocationConfig {
    /// Resolve the location into either a preset recall or an absolute move,
    /// rejecting configs that specify neither (or both).
    pub fn target(&self) -> anyhow::Result<LocationTarget> {
        if let Some(preset) = &self.preset {
            if self.pan.is_some() || self.tilt.is_some() || self.zoom.is_some() {
                anyhow::bail!(
                    "location `{}` sets both `preset` and pan/tilt/zoom — choose one",
                    self.camera_id
                );
            }
            return Ok(LocationTarget::Preset {
                preset: preset.clone(),
            });
        }
        match (self.pan, self.tilt, self.zoom) {
            (Some(pan), Some(tilt), Some(zoom)) => Ok(LocationTarget::Absolute { pan, tilt, zoom }),
            _ => anyhow::bail!(
                "location `{}` must set either `preset` or all of `pan`, `tilt`, `zoom`",
                self.camera_id
            ),
        }
    }
}

impl Configuration {
    pub fn load() -> anyhow::Result<Self> {
        let path = std::env::var("CAMERA_CONFIG").unwrap_or_else(|_| "cameras.toml".to_string());
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read camera config from {path}"))?;
        let mut config: Configuration =
            toml::from_str(&contents).context("Failed to parse camera config as TOML")?;
        config.validate()?;
        Ok(config)
    }

    /// Cross-field checks that TOML deserialization can't express on its own,
    /// plus resolution of each location's `prompt_ids` into the prompts sent on
    /// upload.
    fn validate(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.latitude.abs() <= 90.0,
            "latitude must be in [-90, 90], got {}",
            self.latitude
        );
        anyhow::ensure!(
            self.longitude.abs() <= 180.0,
            "longitude must be in [-180, 180], got {}",
            self.longitude
        );

        // Index the top-level prompts by prompt_id so locations can reference
        // them by id. Duplicate ids are ambiguous, so reject them up front.
        let mut by_id: HashMap<&str, &api_types::AnalysisPrompt> = HashMap::new();
        for p in &self.prompts {
            if by_id.insert(p.prompt_id.as_str(), p).is_some() {
                anyhow::bail!(
                    "duplicate prompt_id `{}` in top-level [[prompts]]",
                    p.prompt_id
                );
            }
        }

        // Resolve each location's prompt_ids into the prompts actually sent on
        // upload. Unknown ids fail fast at load time rather than surfacing as a
        // missing `prompts` part at capture.
        for cam in &mut self.cameras {
            for loc in &mut cam.locations {
                loc.prompts.clear();
                for id in &loc.prompt_ids {
                    let p = by_id.get(id.as_str()).with_context(|| {
                        format!(
                            "location `{}` references unknown prompt_id `{}` \
                             (not defined in any [[prompts]] table)",
                            loc.camera_id, id
                        )
                    })?;
                    loc.prompts.push((*p).clone());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
server_url = "https://example.test"
auth_token = "tok"
latitude = 32.0853
longitude = 34.7818

[[cameras]]
uri = "http://192.168.0.226:8080"
username = "admin"
password = "admin"
interval_secs = 300
daylight_only = true
daylight_margin_mins = 15

  [[cameras.locations]]
  camera_id = "yard-north"
  pan = 0.0
  tilt = 0.0
  zoom = 0.5
"#;

    #[test]
    fn parses_per_camera_daylight_and_interval() {
        let mut config: Configuration = toml::from_str(SAMPLE).unwrap();
        config.validate().unwrap();
        let cam = &config.cameras[0];
        assert_eq!(cam.interval_secs, 300);
        assert!(cam.daylight_only);
        assert_eq!(cam.daylight_margin_mins, 15);
        assert_eq!(config.latitude, 32.0853);
        assert_eq!(config.longitude, 34.7818);
    }

    #[test]
    fn missing_coords_fails_to_parse() {
        // latitude/longitude are required fields, so omitting them must fail
        // at deserialization (before validation even runs).
        let bad = r#"
server_url = "https://example.test"
auth_token = "tok"

[[cameras]]
uri = "http://x"
username = "a"
password = "b"
interval_secs = 60

  [[cameras.locations]]
  camera_id = "c"
  pan = 0.0
  tilt = 0.0
  zoom = 0.0
"#;
        assert!(toml::from_str::<Configuration>(bad).is_err());
    }

    #[test]
    fn out_of_range_coords_rejected_even_without_daylight() {
        // Coordinates are always required and validated, regardless of whether
        // any camera opts into daylight gating.
        let bad = r#"
server_url = "https://example.test"
auth_token = "tok"
latitude = 200.0
longitude = 0.0

[[cameras]]
uri = "http://x"
username = "a"
password = "b"
interval_secs = 60

  [[cameras.locations]]
  camera_id = "c"
  pan = 0.0
  tilt = 0.0
  zoom = 0.0
"#;
        let mut config: Configuration = toml::from_str(bad).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn resolves_prompt_ids_to_top_level_prompts() {
        // Prompts are defined once at the top level and referenced by id from
        // many locations, instead of restated at each site.
        let sample = r#"
server_url = "https://example.test"
auth_token = "tok"
latitude = 32.0853
longitude = 34.7818

[[prompts]]
prompt_id = "green-cover"
prompt_text = "What percentage of the frame is green canopy? Reply with a single number."
model = "google/gemini-2.5-flash"
measurement_unit = "%"

[[prompts]]
prompt_id = "color"
prompt_text = "Reply as `r,g,b`."
model = "google/gemini-2.5-flash"
response_type = "text"

[[cameras]]
uri = "http://192.168.0.226:8080"
username = "admin"
password = "admin"
interval_secs = 300

  [[cameras.locations]]
  camera_id = "yard-north"
  pan = 0.0
  tilt = 0.0
  zoom = 0.5
  prompt_ids = ["green-cover", "color"]

  [[cameras.locations]]
  camera_id = "yard-south"
  pan = 0.0
  tilt = 0.0
  zoom = 0.5
  prompt_ids = ["green-cover"]
"#;
        let mut config: Configuration = toml::from_str(sample).unwrap();
        config.validate().unwrap();

        let north = &config.cameras[0].locations[0];
        assert_eq!(north.camera_id, "yard-north");
        assert_eq!(north.prompts.len(), 2);
        assert_eq!(north.prompts[0].prompt_id, "green-cover");
        assert_eq!(north.prompts[0].measurement_unit.as_deref(), Some("%"));
        assert_eq!(
            north.prompts[0].response_type,
            api_types::ResponseType::Numeric
        );
        assert_eq!(north.prompts[1].prompt_id, "color");
        assert!(north.prompts[1].measurement_unit.is_none());
        assert_eq!(
            north.prompts[1].response_type,
            api_types::ResponseType::Text
        );

        // The second location shares the same `green-cover` prompt without
        // restating it — the whole point of the indirection.
        let south = &config.cameras[0].locations[1];
        assert_eq!(south.prompts.len(), 1);
        assert_eq!(south.prompts[0].prompt_id, "green-cover");
    }

    #[test]
    fn unknown_prompt_id_rejected_at_load() {
        let sample = r#"
server_url = "https://example.test"
auth_token = "tok"
latitude = 0.0
longitude = 0.0

[[prompts]]
prompt_id = "green-cover"
prompt_text = "x"
model = "google/gemini-2.5-flash"

[[cameras]]
uri = "http://x"
username = "a"
password = "b"
interval_secs = 60

  [[cameras.locations]]
  camera_id = "c"
  pan = 0.0
  tilt = 0.0
  zoom = 0.0
  prompt_ids = ["missing"]
"#;
        let mut config: Configuration = toml::from_str(sample).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("unknown prompt_id `missing`"), "got: {err}");
        assert!(err.contains("`c`"), "got: {err}");
    }

    #[test]
    fn duplicate_prompt_id_rejected_at_load() {
        let sample = r#"
server_url = "https://example.test"
auth_token = "tok"
latitude = 0.0
longitude = 0.0

[[prompts]]
prompt_id = "green-cover"
prompt_text = "x"
model = "google/gemini-2.5-flash"

[[prompts]]
prompt_id = "green-cover"
prompt_text = "y"
model = "google/gemini-2.5-flash"

[[cameras]]
uri = "http://x"
username = "a"
password = "b"
interval_secs = 60

  [[cameras.locations]]
  camera_id = "c"
  pan = 0.0
  tilt = 0.0
  zoom = 0.0
"#;
        let mut config: Configuration = toml::from_str(sample).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("duplicate prompt_id `green-cover`"),
            "got: {err}"
        );
    }

    #[test]
    fn prompts_default_to_empty() {
        // A location without `prompt_ids` resolves to no prompts — the image
        // is uploaded without analysis, exactly as before the feature existed.
        let sample = r#"
server_url = "https://example.test"
auth_token = "tok"
latitude = 0.0
longitude = 0.0

[[cameras]]
uri = "http://x"
username = "a"
password = "b"
interval_secs = 60

  [[cameras.locations]]
  camera_id = "c"
  pan = 0.0
  tilt = 0.0
  zoom = 0.0
"#;
        let mut config: Configuration = toml::from_str(sample).unwrap();
        config.validate().unwrap();
        assert!(config.cameras[0].locations[0].prompts.is_empty());
    }
}
