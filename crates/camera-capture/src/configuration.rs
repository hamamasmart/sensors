//! TOML configuration for the capture loop.
//!
//! Config path comes from `CAMERA_CONFIG` (default `cameras.toml`, relative to
//! the current working directory). The process runs on a Raspberry Pi on the
//! same network as the cameras, so camera URIs are plain `http://<lan-ip>:port`
//! while `server_url` points at the cloud `server`.

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Configuration {
    /// Seconds between full passes over every camera.
    pub interval_secs: u64,
    /// Base URL of the cloud `server` (HTTPS). The `/cameras/images` route is
    /// appended to it.
    pub server_url: String,
    /// Bearer token sent as `Authorization: Bearer <token>` to the server.
    pub auth_token: String,
    pub cameras: Vec<CameraConfig>,
}

#[derive(Debug, Deserialize)]
pub struct CameraConfig {
    /// Device host root, e.g. `http://192.168.1.50:8000`. The `service_path` is
    /// appended to reach the device service.
    pub uri: String,
    pub username: String,
    pub password: String,
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
        toml::from_str(&contents).context("Failed to parse camera config as TOML")
    }
}
