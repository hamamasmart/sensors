//! Build-time configuration codegen.
//!
//! Parses `config.toml` into owned `*Toml` mirror structs (the runtime `&'static str`/`&'static [u8]`
//! fields in `src/config.rs` cannot be deserialized from TOML, so the types are duplicated: owned
//! here, borrowed there). Validates the result, hex-decodes the PSK, and emits `DEFAULT_CONFIG` /
//! `DEFAULT_SENSORS` as Rust `static`s into `OUT_DIR/config.rs` via `quote`.

use anyhow::{Context, Result, bail};
use proc_macro2::TokenStream;
use quote::quote;
use serde::Deserialize;
use std::{env, fs, path::PathBuf};

// ---------------------------------------------------------------------------
// Owned TOML mirror types (deserialized from config.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ConfigFile {
    provider: String,
    poll_interval_secs: u64,
    wifi: WifiConfigToml,
    server: ServerConfigToml,
    modbus: ModbusConfigToml,
    sensors: Vec<SensorDefinitionToml>,
}

#[derive(Debug, Deserialize)]
struct WifiConfigToml {
    ssid: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct ServerConfigToml {
    host: String,
    port: u16,
    auth_token: String,
    /// Transport security, parsed from the required `[server.tls]` table.
    #[serde(default)]
    tls: TomlTlsConfig,
}

/// TLS configuration parsed from `[server.tls]` in `config.toml`.
///
/// Internally tagged by `type`: `type = "none"` (plain HTTP) or
/// `type = "psk"` (HTTPS with TLS-PSK, carrying `identity` + `psk`).
#[derive(Debug, Default, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum TomlTlsConfig {
    #[default]
    None,
    Psk {
        identity: String,
        psk: String,
    },
}

#[derive(Debug, Deserialize)]
struct ModbusConfigToml {
    baud_rate: u32,
    timeout_ms: u64,
    turnaround_delay_ms: u64,
}

#[derive(Debug, Deserialize)]
struct SensorDefinitionToml {
    external_id: String,
    category: String,
    measurement_unit: Option<String>,
    depth_value: Option<f64>,
    depth_unit: Option<String>,
    slave_address: u8,
    function: TomlModbusFunction,
    register_address: u16,
    register_count: u16,
    scaling: TomlScaling,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TomlModbusFunction {
    #[serde(
        alias = "ReadHoldingRegisters",
        alias = "holding_registers",
        alias = "holding",
        alias = "0x03",
        alias = "3"
    )]
    ReadHoldingRegisters,
    #[serde(
        alias = "ReadInputRegisters",
        alias = "input_registers",
        alias = "input",
        alias = "0x04",
        alias = "4"
    )]
    ReadInputRegisters,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TomlScaling {
    Typed {
        #[serde(rename = "type")]
        scaling_type: String,
        factor: Option<f64>,
    },
    EnumStyle(TomlScalingEnum),
    Named(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TomlScalingEnum {
    #[serde(alias = "UnsignedScaled", alias = "unsigned_scaled")]
    UnsignedScaled(f64),
    #[serde(alias = "SignedScaled", alias = "signed_scaled")]
    SignedScaled(f64),
    #[serde(alias = "Raw", alias = "raw")]
    Raw,
    #[serde(
        alias = "Float32Be",
        alias = "float32_be",
        alias = "float32be",
        alias = "Float32BE"
    )]
    Float32Be,
}

// ---------------------------------------------------------------------------
// Codegen helpers (TOML value → runtime token stream)
// ---------------------------------------------------------------------------

/// Render a byte slice as a `&[u8]` array-literal token stream (e.g. `&[0xA1u8, 0xB2u8]`).
fn byte_array_tokens(bytes: &[u8]) -> TokenStream {
    let items = bytes.iter().map(|b| quote! { #b });
    quote! { &[ #( #items ),* ] }
}

/// Map a [`TomlModbusFunction`] to its runtime `ModbusFunction` constructor.
fn function_tokens(function: &TomlModbusFunction) -> TokenStream {
    match function {
        TomlModbusFunction::ReadHoldingRegisters => quote! { ModbusFunction::ReadHoldingRegisters },
        TomlModbusFunction::ReadInputRegisters => quote! { ModbusFunction::ReadInputRegisters },
    }
}

/// Map a [`TomlScaling`] to its runtime `RegisterScaling` constructor.
fn scaling_tokens(scaling: &TomlScaling) -> Result<TokenStream> {
    match scaling {
        TomlScaling::Typed {
            scaling_type,
            factor,
        } => match scaling_type.to_lowercase().as_str() {
            "unsignedscaled" | "unsigned_scaled" => {
                let f = factor
                    .ok_or_else(|| anyhow::anyhow!("UnsignedScaled scaling requires a `factor`"))?;
                Ok(quote! { RegisterScaling::UnsignedScaled(#f) })
            }
            "signedscaled" | "signed_scaled" => {
                let f = factor
                    .ok_or_else(|| anyhow::anyhow!("SignedScaled scaling requires a `factor`"))?;
                Ok(quote! { RegisterScaling::SignedScaled(#f) })
            }
            "raw" => Ok(quote! { RegisterScaling::Raw }),
            "float32be" | "float32_be" => Ok(quote! { RegisterScaling::Float32Be }),
            other => bail!("unknown scaling type: '{other}'"),
        },
        TomlScaling::EnumStyle(TomlScalingEnum::UnsignedScaled(v)) => {
            Ok(quote! { RegisterScaling::UnsignedScaled(#v) })
        }
        TomlScaling::EnumStyle(TomlScalingEnum::SignedScaled(v)) => {
            Ok(quote! { RegisterScaling::SignedScaled(#v) })
        }
        TomlScaling::EnumStyle(TomlScalingEnum::Raw) => Ok(quote! { RegisterScaling::Raw }),
        TomlScaling::EnumStyle(TomlScalingEnum::Float32Be) => {
            Ok(quote! { RegisterScaling::Float32Be })
        }
        TomlScaling::Named(name) => match name.to_lowercase().as_str() {
            "raw" => Ok(quote! { RegisterScaling::Raw }),
            "float32be" | "float32_be" => Ok(quote! { RegisterScaling::Float32Be }),
            other => bail!("unknown scaling name: '{other}'"),
        },
    }
}

/// Whether a [`TomlScaling`] resolves to the two-register `Float32Be` variant.
fn scaling_is_float32be(scaling: &TomlScaling) -> bool {
    match scaling {
        TomlScaling::Typed { scaling_type, .. } => {
            let s = scaling_type.to_lowercase();
            s == "float32be" || s == "float32_be"
        }
        TomlScaling::EnumStyle(TomlScalingEnum::Float32Be) => true,
        TomlScaling::Named(name) => {
            let s = name.to_lowercase();
            s == "float32be" || s == "float32_be"
        }
        _ => false,
    }
}

/// Emit a `SensorDefinition` literal for one sensor.
fn sensor_tokens(s: &SensorDefinitionToml) -> Result<TokenStream> {
    let external_id = &s.external_id;
    let category = &s.category;
    let measurement_unit = match &s.measurement_unit {
        Some(u) => quote! { Some(#u) },
        None => quote! { None },
    };
    let depth_value = match s.depth_value {
        Some(v) => quote! { Some(#v) },
        None => quote! { None },
    };
    let depth_unit = match &s.depth_unit {
        Some(u) => quote! { Some(#u) },
        None => quote! { None },
    };
    let slave_address = s.slave_address;
    let function = function_tokens(&s.function);
    let register_address = s.register_address;
    let register_count = s.register_count;
    let scaling = scaling_tokens(&s.scaling)?;

    Ok(quote! {
        SensorDefinition {
            external_id: #external_id,
            category: #category,
            measurement_unit: #measurement_unit,
            depth_value: #depth_value,
            depth_unit: #depth_unit,
            slave_address: #slave_address,
            function: #function,
            register_address: #register_address,
            register_count: #register_count,
            scaling: #scaling,
        }
    })
}

/// Map a [`TomlTlsConfig`] to its runtime `TlsConfig` constructor, hex-decoding the PSK.
fn tls_tokens(tls: &TomlTlsConfig) -> Result<TokenStream> {
    match tls {
        TomlTlsConfig::None => Ok(quote! { TlsConfig::None }),
        TomlTlsConfig::Psk { identity, psk } => {
            if identity.is_empty() {
                bail!("`[server.tls]` with `type = \"psk\"` requires a non-empty `identity`");
            }
            if psk.is_empty() {
                bail!("`[server.tls]` with `type = \"psk\"` requires a non-empty `psk`");
            }
            let identity_arr = byte_array_tokens(identity.as_bytes());
            let psk_decoded = hex::decode(psk)
                .with_context(|| format!("`psk` must be valid even-length hex, got {psk:?}"))?;
            let psk_arr = byte_array_tokens(&psk_decoded);
            Ok(quote! { TlsConfig::Psk { identity: #identity_arr, psk: #psk_arr } })
        }
    }
}

/// Validate sensor Modbus parameters at build time so misconfigurations fail the build with a clear
/// message instead of surfacing as per-read runtime errors.
fn validate_sensor(s: &SensorDefinitionToml) -> Result<()> {
    if !(1..=247).contains(&s.slave_address) {
        bail!(
            "sensor '{}' has slave_address {} (must be 1..=247)",
            s.external_id,
            s.slave_address
        );
    }
    if s.register_count == 0 || s.register_count > 125 {
        bail!(
            "sensor '{}' has register_count {} (must be 1..=125)",
            s.external_id,
            s.register_count
        );
    }
    if scaling_is_float32be(&s.scaling) && s.register_count < 2 {
        bail!(
            "sensor '{}' uses Float32Be scaling but register_count is {} (needs >= 2)",
            s.external_id,
            s.register_count
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);

    // Allow path override via FIRMWARE_CONFIG env var; fall back to config.toml.
    let config_path = if let Ok(custom_path) = env::var("FIRMWARE_CONFIG") {
        PathBuf::from(custom_path)
    } else {
        manifest_dir.join("config.toml")
    };

    println!("cargo:rerun-if-changed={}", config_path.display());
    println!("cargo:rerun-if-env-changed=FIRMWARE_CONFIG");

    let config_content = fs::read_to_string(&config_path).context("failed to read config file")?;
    let config: ConfigFile =
        toml::from_str(&config_content).context("failed to parse config file")?;

    // Validate every sensor up front.
    for s in &config.sensors {
        validate_sensor(s)?;
    }

    // ---- Codegen: emit `DEFAULT_SENSORS` and `DEFAULT_CONFIG` via `quote`. ----

    let sensor_items: Vec<_> = config
        .sensors
        .iter()
        .map(sensor_tokens)
        .collect::<Result<_>>()?;
    let default_sensors = quote! {
        /// Default sensor definitions generated from TOML configuration.
        pub static DEFAULT_SENSORS: &[SensorDefinition] = &[ #(#sensor_items),* ];
    };

    let provider = &config.provider;
    let ssid = &config.wifi.ssid;
    let password = &config.wifi.password;
    let host = &config.server.host;
    let port = config.server.port;
    let auth_token = &config.server.auth_token;
    let tls = tls_tokens(&config.server.tls)?;
    let baud_rate = config.modbus.baud_rate;
    let timeout_ms = config.modbus.timeout_ms;
    let turnaround_delay_ms = config.modbus.turnaround_delay_ms;
    let poll_interval_secs = config.poll_interval_secs;

    let default_config = quote! {
        /// Default application configuration generated from TOML.
        pub static DEFAULT_CONFIG: AppConfig = AppConfig {
            provider: #provider,
            wifi: WifiConfig { ssid: #ssid, password: #password },
            server: ServerConfig {
                host: #host,
                port: #port,
                auth_token: #auth_token,
                tls: #tls,
            },
            modbus: ModbusConfig {
                baud_rate: #baud_rate,
                timeout_ms: #timeout_ms,
                turnaround_delay_ms: #turnaround_delay_ms,
            },
            poll_interval_secs: #poll_interval_secs,
            sensors: DEFAULT_SENSORS,
        };
    };

    let generated = quote! {
        #default_sensors
        #default_config
    };

    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let dest_path = out_dir.join("config.rs");
    fs::write(&dest_path, generated.to_string()).with_context(|| {
        format!(
            "failed to write generated config to {}",
            dest_path.display()
        )
    })?;
    Ok(())
}
