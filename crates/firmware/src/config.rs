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

/// Remote backend server settings.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub host: &'static str,
    pub port: u16,
    pub auth_token: &'static str,
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
