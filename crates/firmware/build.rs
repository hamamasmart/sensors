use serde::Deserialize;
use std::{env, fs, path::PathBuf};

#[derive(Debug, Deserialize)]
struct ConfigFile {
    #[serde(default = "default_provider")]
    provider: String,
    #[serde(default = "default_poll_interval")]
    poll_interval_secs: u64,
    #[serde(default)]
    wifi: WifiConfigToml,
    #[serde(default)]
    server: ServerConfigToml,
    #[serde(default)]
    modbus: ModbusConfigToml,
    #[serde(default)]
    sensors: Vec<SensorDefinitionToml>,
}

fn default_provider() -> String {
    "rs485-esp32s3".to_string()
}

fn default_poll_interval() -> u64 {
    30
}

#[derive(Debug, Default, Deserialize)]
struct WifiConfigToml {
    #[serde(default)]
    ssid: String,
    #[serde(default)]
    password: String,
}

#[derive(Debug, Default, Deserialize)]
struct ServerConfigToml {
    #[serde(default)]
    host: String,
    #[serde(default = "default_server_port")]
    port: u16,
    #[serde(default)]
    auth_token: String,
}

fn default_server_port() -> u16 {
    8080
}

#[derive(Debug, Deserialize)]
struct ModbusConfigToml {
    #[serde(default = "default_baud_rate")]
    baud_rate: u32,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default = "default_turnaround_delay_ms")]
    turnaround_delay_ms: u64,
}

impl Default for ModbusConfigToml {
    fn default() -> Self {
        Self {
            baud_rate: default_baud_rate(),
            timeout_ms: default_timeout_ms(),
            turnaround_delay_ms: default_turnaround_delay_ms(),
        }
    }
}

fn default_baud_rate() -> u32 {
    9600
}

fn default_timeout_ms() -> u64 {
    1000
}

fn default_turnaround_delay_ms() -> u64 {
    5
}

#[derive(Debug, Deserialize)]
struct SensorDefinitionToml {
    external_id: String,
    category: String,
    #[serde(default)]
    measurement_unit: Option<String>,
    #[serde(default)]
    depth_value: Option<f64>,
    #[serde(default)]
    depth_unit: Option<String>,
    slave_address: u8,
    function: TomlModbusFunction,
    register_address: u16,
    #[serde(default = "default_register_count")]
    register_count: u16,
    scaling: TomlScaling,
}

fn default_register_count() -> u16 {
    1
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
        #[serde(default)]
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

fn scaling_to_code(scaling: &TomlScaling) -> Result<String, String> {
    match scaling {
        TomlScaling::Typed {
            scaling_type,
            factor,
        } => {
            let s = scaling_type.to_lowercase();
            match s.as_str() {
                "unsignedscaled" | "unsigned_scaled" => {
                    let f = factor.ok_or_else(|| "UnsignedScaled requires 'factor'".to_string())?;
                    Ok(format!("RegisterScaling::UnsignedScaled({f:?})"))
                }
                "signedscaled" | "signed_scaled" => {
                    let f = factor.ok_or_else(|| "SignedScaled requires 'factor'".to_string())?;
                    Ok(format!("RegisterScaling::SignedScaled({f:?})"))
                }
                "raw" => Ok("RegisterScaling::Raw".to_string()),
                "float32be" | "float32_be" => Ok("RegisterScaling::Float32Be".to_string()),
                other => Err(format!("Unknown scaling type: '{other}'")),
            }
        }
        TomlScaling::EnumStyle(e) => match e {
            TomlScalingEnum::UnsignedScaled(f) => {
                Ok(format!("RegisterScaling::UnsignedScaled({f:?})"))
            }
            TomlScalingEnum::SignedScaled(f) => Ok(format!("RegisterScaling::SignedScaled({f:?})")),
            TomlScalingEnum::Raw => Ok("RegisterScaling::Raw".to_string()),
            TomlScalingEnum::Float32Be => Ok("RegisterScaling::Float32Be".to_string()),
        },
        TomlScaling::Named(name) => {
            let s = name.to_lowercase();
            match s.as_str() {
                "raw" => Ok("RegisterScaling::Raw".to_string()),
                "float32be" | "float32_be" => Ok("RegisterScaling::Float32Be".to_string()),
                other => Err(format!("Unknown scaling name: '{other}'")),
            }
        }
    }
}

/// Validate sensor Modbus parameters at build time so misconfigurations fail
/// the build with a clear message instead of surfacing as per-read runtime errors.
fn validate_sensor(sensor: &SensorDefinitionToml, scaling_code: &str) -> Result<(), String> {
    if !(1..=247).contains(&sensor.slave_address) {
        return Err(format!(
            "sensor '{}' has slave_address {} (must be 1..=247)",
            sensor.external_id, sensor.slave_address
        ));
    }
    if sensor.register_count == 0 || sensor.register_count > 125 {
        return Err(format!(
            "sensor '{}' has register_count {} (must be 1..=125)",
            sensor.external_id, sensor.register_count
        ));
    }
    if scaling_code.contains("Float32Be") && sensor.register_count < 2 {
        return Err(format!(
            "sensor '{}' uses Float32Be scaling but register_count is {} (needs >= 2)",
            sensor.external_id, sensor.register_count
        ));
    }
    Ok(())
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Allow path override via FIRMWARE_CONFIG env var
    let config_path = if let Ok(custom_path) = env::var("FIRMWARE_CONFIG") {
        PathBuf::from(custom_path)
    } else {
        let default_path = manifest_dir.join("config.toml");
        if default_path.exists() {
            default_path
        } else {
            manifest_dir.join("config.toml.example")
        }
    };

    println!("cargo:rerun-if-env-changed=FIRMWARE_CONFIG");
    println!("cargo:rerun-if-env-changed=ESP_WIFI_SSID");
    println!("cargo:rerun-if-env-changed=ESP_WIFI_PASSWORD");
    println!("cargo:rerun-if-env-changed=ESP_SERVER_HOST");
    println!("cargo:rerun-if-env-changed=ESP_SERVER_PORT");
    println!("cargo:rerun-if-env-changed=ESP_SERVER_AUTH_TOKEN");
    println!("cargo:rerun-if-env-changed=ESP_PROVIDER");

    if config_path.exists() {
        println!("cargo:rerun-if-changed={}", config_path.display());
    }

    let config_content = if config_path.exists() {
        fs::read_to_string(&config_path)
            .unwrap_or_else(|e| panic!("Failed to read config file {}: {e}", config_path.display()))
    } else {
        String::new()
    };

    let mut config: ConfigFile = if !config_content.trim().is_empty() {
        toml::from_str(&config_content).unwrap_or_else(|e| {
            panic!("Failed to parse config file {}: {e}", config_path.display())
        })
    } else {
        ConfigFile {
            provider: default_provider(),
            poll_interval_secs: default_poll_interval(),
            wifi: WifiConfigToml::default(),
            server: ServerConfigToml::default(),
            modbus: ModbusConfigToml::default(),
            sensors: Vec::new(),
        }
    };

    // Allow environment variable overrides
    if let Ok(provider) = env::var("ESP_PROVIDER") {
        config.provider = provider;
    }
    if let Ok(ssid) = env::var("ESP_WIFI_SSID") {
        config.wifi.ssid = ssid;
    }
    if let Ok(password) = env::var("ESP_WIFI_PASSWORD") {
        config.wifi.password = password;
    }
    if let Ok(host) = env::var("ESP_SERVER_HOST") {
        config.server.host = host;
    }
    if let Ok(port_str) = env::var("ESP_SERVER_PORT")
        && let Ok(port) = port_str.parse::<u16>()
    {
        config.server.port = port;
    }
    if let Ok(token) = env::var("ESP_SERVER_AUTH_TOKEN") {
        config.server.auth_token = token;
    }

    let mut code = String::new();

    // Generate DEFAULT_SENSORS
    code.push_str("/// Default sensor definitions generated from TOML configuration.\n");
    code.push_str("pub static DEFAULT_SENSORS: &[SensorDefinition] = &[\n");
    for s in &config.sensors {
        let function_str = match s.function {
            TomlModbusFunction::ReadHoldingRegisters => "ModbusFunction::ReadHoldingRegisters",
            TomlModbusFunction::ReadInputRegisters => "ModbusFunction::ReadInputRegisters",
        };

        let scaling_str = scaling_to_code(&s.scaling)
            .unwrap_or_else(|e| panic!("Invalid scaling for sensor '{}': {e}", s.external_id));

        validate_sensor(s, &scaling_str).unwrap_or_else(|e| {
            panic!("Invalid sensor configuration for '{}': {e}", s.external_id)
        });

        let unit_str = match &s.measurement_unit {
            Some(u) => format!("Some({u:?})"),
            None => "None".to_string(),
        };

        let depth_val_str = match s.depth_value {
            Some(v) => format!("Some({v:?})"),
            None => "None".to_string(),
        };

        let depth_unit_str = match &s.depth_unit {
            Some(u) => format!("Some({u:?})"),
            None => "None".to_string(),
        };

        code.push_str("    SensorDefinition {\n");
        code.push_str(&format!("        external_id: {:?},\n", s.external_id));
        code.push_str(&format!("        category: {:?},\n", s.category));
        code.push_str(&format!("        measurement_unit: {unit_str},\n"));
        code.push_str(&format!("        depth_value: {depth_val_str},\n"));
        code.push_str(&format!("        depth_unit: {depth_unit_str},\n"));
        code.push_str(&format!("        slave_address: {},\n", s.slave_address));
        code.push_str(&format!("        function: {function_str},\n"));
        code.push_str(&format!(
            "        register_address: 0x{:04X},\n",
            s.register_address
        ));
        code.push_str(&format!("        register_count: {},\n", s.register_count));
        code.push_str(&format!("        scaling: {scaling_str},\n"));
        code.push_str("    },\n");
    }
    code.push_str("];\n\n");

    // Generate DEFAULT_CONFIG
    code.push_str("/// Default application configuration generated from TOML.\n");
    code.push_str("pub static DEFAULT_CONFIG: AppConfig = AppConfig {\n");
    code.push_str(&format!("    provider: {:?},\n", config.provider));
    code.push_str("    wifi: WifiConfig {\n");
    code.push_str(&format!("        ssid: {:?},\n", config.wifi.ssid));
    code.push_str(&format!("        password: {:?},\n", config.wifi.password));
    code.push_str("    },\n");
    code.push_str("    server: ServerConfig {\n");
    code.push_str(&format!("        host: {:?},\n", config.server.host));
    code.push_str(&format!("        port: {},\n", config.server.port));
    code.push_str(&format!(
        "        auth_token: {:?},\n",
        config.server.auth_token
    ));
    code.push_str("    },\n");
    code.push_str("    modbus: ModbusConfig {\n");
    code.push_str(&format!(
        "        baud_rate: {},\n",
        config.modbus.baud_rate
    ));
    code.push_str(&format!(
        "        timeout_ms: {},\n",
        config.modbus.timeout_ms
    ));
    code.push_str(&format!(
        "        turnaround_delay_ms: {},\n",
        config.modbus.turnaround_delay_ms
    ));
    code.push_str("    },\n");
    code.push_str(&format!(
        "    poll_interval_secs: {},\n",
        config.poll_interval_secs
    ));
    code.push_str("    sensors: DEFAULT_SENSORS,\n");
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
