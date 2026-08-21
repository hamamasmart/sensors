use serde::Deserialize;
use std::{env, fs, path::PathBuf};

#[derive(Debug, Deserialize)]
struct ConfigFile {
    sensor_type: TomlSensorType,
    #[serde(default)]
    input: EndpointToml,
    #[serde(default)]
    output: EndpointToml,
    #[serde(default)]
    modbus: ModbusConfigToml,
}

/// TOML form of [`SensorType`]; mapped to the codegen enum variant in `main`.
#[derive(Debug, Deserialize, Clone, Copy)]
enum TomlSensorType {
    #[serde(
        rename = "SN-300AL-GH-N01",
        alias = "sn300alghn01",
        alias = "sn-300al-gh-n01",
        alias = "sn300"
    )]
    Sn300AlGhN01,
    #[serde(
        rename = "LightIntensityRs485",
        alias = "light_intensity_rs485",
        alias = "light-intensity-rs485",
        alias = "light"
    )]
    LightIntensityRs485,
}

impl TomlSensorType {
    /// Codegen token for the matching `SensorType` variant.
    fn code(&self) -> &'static str {
        match self {
            TomlSensorType::Sn300AlGhN01 => "SensorType::Sn300AlGhN01",
            TomlSensorType::LightIntensityRs485 => "SensorType::LightIntensityRs485",
        }
    }

    /// Max slave address for this model — used for build-time validation (mirrors `SensorProfile`).
    fn max_slave_address(&self) -> u8 {
        match self {
            TomlSensorType::Sn300AlGhN01 => 254,
            TomlSensorType::LightIntensityRs485 => 251,
        }
    }
}

#[derive(Debug, Deserialize)]
struct EndpointToml {
    #[serde(default = "default_slave_id")]
    slave_id: u8,
    #[serde(default = "default_baud_rate")]
    baud_rate: u32,
}

impl Default for EndpointToml {
    fn default() -> Self {
        Self {
            slave_id: default_slave_id(),
            baud_rate: default_baud_rate(),
        }
    }
}

fn default_slave_id() -> u8 {
    1
}

fn default_baud_rate() -> u32 {
    9600
}

#[derive(Debug, Deserialize)]
struct ModbusConfigToml {
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default = "default_turnaround_delay_ms")]
    turnaround_delay_ms: u64,
}

impl Default for ModbusConfigToml {
    fn default() -> Self {
        Self {
            timeout_ms: default_timeout_ms(),
            turnaround_delay_ms: default_turnaround_delay_ms(),
        }
    }
}

fn default_timeout_ms() -> u64 {
    1000
}

fn default_turnaround_delay_ms() -> u64 {
    5
}

/// Validate endpoint parameters against the chosen sensor model so a misconfigured `config.toml`
/// fails the build with a clear message instead of producing a firmware that silently misprograms
/// a sensor.
fn validate_endpoint(
    endpoint: &EndpointToml,
    sensor: &TomlSensorType,
    label: &str,
) -> Result<(), String> {
    let max = sensor.max_slave_address();
    if !(1..=max).contains(&endpoint.slave_id) {
        return Err(format!(
            "{label} slave_id {} is out of range (must be 1..={max} for this sensor model)",
            endpoint.slave_id
        ));
    }
    if !matches!(endpoint.baud_rate, 2400 | 4800 | 9600) {
        return Err(format!(
            "{label} baud_rate {} is unsupported (must be one of 2400, 4800, 9600)",
            endpoint.baud_rate
        ));
    }
    Ok(())
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Allow path override via FIRMWARE_CONFIGURATOR_CONFIG env var, else fall back to
    // config.toml, then config.toml.example.
    let config_path = if let Ok(custom_path) = env::var("FIRMWARE_CONFIGURATOR_CONFIG") {
        PathBuf::from(custom_path)
    } else {
        let default_path = manifest_dir.join("config.toml");
        if default_path.exists() {
            default_path
        } else {
            manifest_dir.join("config.toml.example")
        }
    };

    println!("cargo:rerun-if-env-changed=FIRMWARE_CONFIGURATOR_CONFIG");

    if config_path.exists() {
        println!("cargo:rerun-if-changed={}", config_path.display());
    }

    let config_content = if config_path.exists() {
        fs::read_to_string(&config_path)
            .unwrap_or_else(|e| panic!("Failed to read config file {}: {e}", config_path.display()))
    } else {
        String::new()
    };

    let config: ConfigFile = if !config_content.trim().is_empty() {
        toml::from_str(&config_content).unwrap_or_else(|e| {
            panic!("Failed to parse config file {}: {e}", config_path.display())
        })
    } else {
        ConfigFile {
            sensor_type: TomlSensorType::Sn300AlGhN01,
            input: EndpointToml::default(),
            output: EndpointToml::default(),
            modbus: ModbusConfigToml::default(),
        }
    };

    validate_endpoint(&config.input, &config.sensor_type, "input")
        .unwrap_or_else(|e| panic!("Invalid configurator config: {e}"));
    validate_endpoint(&config.output, &config.sensor_type, "output")
        .unwrap_or_else(|e| panic!("Invalid configurator config: {e}"));

    let mut code = String::new();

    code.push_str("/// Default configurator configuration generated from TOML.\n");
    code.push_str("pub static DEFAULT_CONFIG: ConfiguratorConfig = ConfiguratorConfig {\n");
    code.push_str(&format!(
        "    sensor_type: {},\n",
        config.sensor_type.code()
    ));
    code.push_str("    input: Endpoint {\n");
    code.push_str(&format!("        slave_id: {},\n", config.input.slave_id));
    code.push_str(&format!("        baud_rate: {},\n", config.input.baud_rate));
    code.push_str("    },\n");
    code.push_str("    output: Endpoint {\n");
    code.push_str(&format!("        slave_id: {},\n", config.output.slave_id));
    code.push_str(&format!(
        "        baud_rate: {},\n",
        config.output.baud_rate
    ));
    code.push_str("    },\n");
    code.push_str("    modbus: ModbusConfig {\n");
    code.push_str(&format!(
        "        timeout_ms: {},\n",
        config.modbus.timeout_ms
    ));
    code.push_str(&format!(
        "        turnaround_delay_ms: {},\n",
        config.modbus.turnaround_delay_ms
    ));
    code.push_str("    },\n");
    code.push_str("};\n");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let dest_path = out_dir.join("config.rs");
    fs::write(&dest_path, code).unwrap_or_else(|e| {
        panic!(
            "Failed to write generated config to {}: {e}",
            dest_path.display()
        )
    });
}
