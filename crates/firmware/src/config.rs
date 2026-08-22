//! Firmware config types (runtime, borrowed `&'static` form).
//!
//! `build.rs` defines mirror `*Toml` structs (owned `String`/`Vec`) for deserializing `config.toml`
//! and emits `DEFAULT_CONFIG` / `DEFAULT_SENSORS` `static`s into `OUT_DIR/config.rs` (included at the
//! bottom). These runtime structs use `&'static str` / `&'static [u8]` so the whole config lives in
//! `.rodata` with no heap allocation. The `&'static` string/byte fields cannot be deserialized from
//! TOML, hence the separate owned mirror types in `build.rs`.

/// Global firmware configuration.
#[derive(Clone, Debug)]
pub struct AppConfig {
    /// Provider tag applied to every sensor on this hardware bus (e.g. "rs485-esp32s3").
    pub provider: &'static str,
    pub wifi: WifiConfig,
    pub server: ServerConfig,
    pub modbus: ModbusConfig,
    pub poll_interval_secs: u64,
    pub sensors: &'static [SensorDefinition],
}

/// Wi-Fi connection settings.
#[derive(Clone, Debug)]
pub struct WifiConfig {
    pub ssid: &'static str,
    pub password: &'static str,
}

/// Transport security for the telemetry link to the backend server.
///
/// The `Psk` variant carries the identity and pre-shared key inline, so `ServerConfig` holds a
/// single `tls` field rather than scattered optional PSK fields.
#[derive(Clone, Copy, Debug)]
pub enum TlsConfig {
    /// Plain HTTP.
    None,
    /// HTTPS with TLS-PSK (pre-shared key).
    Psk {
        /// PSK identity offered to the server during the TLS handshake (UTF-8 bytes).
        identity: &'static [u8],
        /// Pre-shared key (raw bytes, hex-decoded at build time from `config.toml`).
        psk: &'static [u8],
    },
}

/// Remote backend server settings.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub host: &'static str,
    pub port: u16,
    pub auth_token: &'static str,
    /// Transport security (plain HTTP vs. TLS-PSK).
    pub tls: TlsConfig,
}

/// Modbus RS485 UART bus configuration.
#[derive(Clone, Debug)]
pub struct ModbusConfig {
    pub baud_rate: u32,
    pub timeout_ms: u64,
    pub turnaround_delay_ms: u64,
}

/// Supported Modbus function codes for sensor reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModbusFunction {
    ReadHoldingRegisters = 0x03,
    ReadInputRegisters = 0x04,
}

/// How to interpret and scale the raw 16-bit register values.
#[derive(Clone, Copy, Debug)]
pub enum RegisterScaling {
    /// Raw unsigned 16-bit value multiplied by a scale factor (e.g. 0.1 for 0.1 deg C).
    UnsignedScaled(f64),
    /// Signed 16-bit value (two's complement) multiplied by a scale factor.
    SignedScaled(f64),
    /// Raw unscaled value.
    Raw,
    /// 32-bit float composed of 2 consecutive registers (Big-Endian word order, "ABCD").
    /// Note: only the standard Modbus big-endian word order is supported; devices using
    /// swapped word orders (CDAB/BADC) are not handled and would misread silently.
    Float32Be,
}

/// Definition of a single daisy-chained Modbus-RTU RS485 sensor.
#[derive(Clone, Debug)]
pub struct SensorDefinition {
    /// Unique identifier for this sensor on the hardware bus.
    pub external_id: &'static str,
    /// Category / measurement type (e.g. "temperature", "soil_moisture", "ec", "ph").
    pub category: &'static str,
    /// Measurement unit string (e.g. "°C", "%", "µS/cm").
    pub measurement_unit: Option<&'static str>,
    /// Sensor installation depth value if applicable.
    pub depth_value: Option<f64>,
    /// Sensor installation depth unit if applicable.
    pub depth_unit: Option<&'static str>,
    /// Modbus slave address (1..=247).
    pub slave_address: u8,
    /// Modbus read function.
    pub function: ModbusFunction,
    /// Register start address (0-indexed).
    pub register_address: u16,
    /// Number of registers to read (1..=125).
    pub register_count: u16,
    /// Scaling formula to convert register data to engineering units.
    pub scaling: RegisterScaling,
}

include!(concat!(env!("OUT_DIR"), "/config.rs"));
