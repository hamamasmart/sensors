//! ONVIF device discovery, PTZ movement, and snapshot-URI resolution.
//!
//! Mirrors `onvif/examples/camera.rs`: build the device-mgmt client at the
//! device host root + service path, call `GetServices`, then construct a SOAP
//! client per advertised service we care about (media, ptz). The cameras are
//! reached over plain HTTP on the LAN.

use anyhow::Context;
use url::Url;

use onvif::soap::client::{AuthType, Client, ClientBuilder, Credentials};

use schema::devicemgmt::{self, GetServices};
use schema::onvif::ReferenceToken;

use crate::configuration::{CameraConfig, LocationTarget};

/// The per-service SOAP clients a camera exposes, resolved once per tick.
pub struct CameraClients {
    /// Required for snapshot URIs.
    pub media: Client,
    /// Required when a location needs movement.
    pub ptz: Option<Client>,
    /// Resolved media profile token (configured or the first advertised).
    pub profile_token: String,
}

impl CameraClients {
    /// Move the PTZ node to `target` on this camera's profile.
    pub async fn move_to(&self, target: &LocationTarget) -> anyhow::Result<()> {
        let ptz = self
            .ptz
            .as_ref()
            .context("camera advertises no PTZ service but a location requires movement")?;
        let profile_token = ReferenceToken(self.profile_token.clone());
        match target {
            LocationTarget::Preset { preset } => {
                schema::ptz::goto_preset(
                    ptz,
                    &schema::ptz::GotoPreset {
                        profile_token,
                        preset_token: ReferenceToken(preset.clone()),
                        speed: None,
                    },
                )
                .await
                .context("GotoPreset failed")?;
            }
            LocationTarget::Absolute { pan, tilt, zoom } => {
                schema::ptz::absolute_move(
                    ptz,
                    &schema::ptz::AbsoluteMove {
                        profile_token,
                        position: schema::onvif::Ptzvector {
                            pan_tilt: Some(schema::onvif::Vector2D {
                                x: *pan,
                                y: *tilt,
                                space: None,
                            }),
                            zoom: Some(schema::onvif::Vector1D {
                                x: *zoom,
                                space: None,
                            }),
                        },
                        speed: None,
                    },
                )
                .await
                .context("AbsoluteMove failed")?;
            }
        }
        Ok(())
    }

    /// Ask the media service for the still-snapshot URI for this profile.
    pub async fn snapshot_uri(&self) -> anyhow::Result<String> {
        let resp = schema::media::get_snapshot_uri(
            &self.media,
            &schema::media::GetSnapshotUri {
                profile_token: ReferenceToken(self.profile_token.clone()),
            },
        )
        .await
        .context("GetSnapshotUri failed")?;
        Ok(resp.media_uri.uri)
    }
}

/// Discover the camera's media + ptz services and resolve the profile token.
///
/// Best-effort clock-skew compensation: when `fix_time` is set, read the
/// device clock via `GetSystemDateAndTime` (typically unauthenticated) and pass
/// the gap to the other clients so WS-Security timestamps validate.
pub async fn discover(cam: &CameraConfig) -> anyhow::Result<CameraClients> {
    let base_uri =
        Url::parse(&cam.uri).with_context(|| format!("invalid camera uri `{}`", cam.uri))?;
    let devicemgmt_uri = base_uri
        .join(&cam.service_path)
        .with_context(|| format!("invalid service path `{}`", cam.service_path))?;

    let creds = Credentials {
        username: cam.username.clone(),
        password: cam.password.clone(),
    };
    let auth_type = parse_auth_type(cam.auth_type.as_deref());

    let mut devicemgmt = ClientBuilder::new(&devicemgmt_uri)
        .credentials(Some(creds.clone()))
        .auth_type(auth_type.clone())
        .build();

    let time_gap = if cam.fix_time {
        resolve_time_gap(&devicemgmt).await
    } else {
        None
    };
    if let Some(gap) = time_gap {
        devicemgmt.set_fix_time_gap(Some(gap));
    }

    let services = devicemgmt::get_services(
        &devicemgmt,
        &GetServices {
            include_capability: false,
        },
    )
    .await
    .context("GetServices failed")?;

    let mut media: Option<Client> = None;
    let mut ptz: Option<Client> = None;
    for svc in &services.service {
        let url = match Url::parse(&svc.x_addr) {
            Ok(u) => u,
            Err(_) => continue,
        };
        let client = ClientBuilder::new(&url)
            .credentials(Some(creds.clone()))
            .auth_type(auth_type.clone())
            .fix_time_gap(time_gap)
            .build();
        match svc.namespace.as_str() {
            "http://www.onvif.org/ver10/media/wsdl" => media = Some(client),
            "http://www.onvif.org/ver20/ptz/wsdl" => ptz = Some(client),
            _ => {}
        }
    }

    let media = media.context("camera does not advertise a media service")?;

    let profile_token = match &cam.profile_token {
        Some(t) => t.clone(),
        None => {
            let profiles = schema::media::get_profiles(&media, &Default::default())
                .await
                .context("GetProfiles failed")?;
            profiles
                .profiles
                .into_iter()
                .next()
                .map(|p| p.token.0)
                .context("camera has no media profiles")?
        }
    };

    Ok(CameraClients {
        media,
        ptz,
        profile_token,
    })
}

/// Read the device's UTC clock and return the gap `device_time - pc_time` when
/// it's more than a minute off; `None` otherwise (or if the call fails / device
/// omits the value).
async fn resolve_time_gap(devicemgmt: &Client) -> Option<chrono::Duration> {
    let resp = schema::devicemgmt::get_system_date_and_time(devicemgmt, &Default::default())
        .await
        .ok()?;
    let dt = resp.system_date_and_time.utc_date_time?;
    let pc_time = chrono::Utc::now();
    let date = &dt.date;
    let t = &dt.time;
    let device_time = chrono::NaiveDate::from_ymd_opt(date.year, date.month as _, date.day as _)?
        .and_hms_opt(t.hour as _, t.minute as _, t.second as _)?
        .and_utc();
    let diff = device_time - pc_time;
    (diff.num_seconds().abs() > 60).then_some(diff)
}

fn parse_auth_type(s: Option<&str>) -> AuthType {
    match s.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("digest") => AuthType::Digest,
        Some("usernametoken") => AuthType::UsernameToken,
        _ => AuthType::Any,
    }
}
