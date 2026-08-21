use api_types::ResponseType;

/// Global firmware configuration.
#[derive(Clone, Debug)]
pub struct AppConfig {
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
    /// 32-bit float composed of 2 consecutive registers (Big-Endian word order).
    Float32Be,
}

/// Definition of a single daisy-chained Modbus-RTU RS485 sensor.
#[derive(Clone, Debug)]
pub struct SensorDefinition {
    /// Unique identifier for this sensor on the hardware bus.
    pub external_id: &'static str,
    /// Provider tag for the backend (e.g. "rs485-esp32s3").
    pub provider: &'static str,
    /// Category / measurement type (e.g. "temperature", "soil_moisture", "ec", "ph").
    pub category: &'static str,
    /// Measurement unit string (e.g. "°C", "%", "µS/cm").
    pub measurement_unit: Option<&'static str>,
    /// Sensor installation depth value if applicable.
    pub depth_value: Option<f64>,
    /// Sensor installation depth unit if applicable.
    pub depth_unit: Option<&'static str>,
    /// Value type for storage (Numeric or Text).
    pub value_type: ResponseType,
    /// Modbus slave address (1..=247).
    pub slave_address: u8,
    /// Modbus read function.
    pub function: ModbusFunction,
    /// Register start address (0-indexed).
    pub register_address: u16,
    /// Number of registers to read.
    pub register_count: u16,
    /// Scaling formula to convert register data to engineering units.
    pub scaling: RegisterScaling,
}

/// Default sensor definitions for a typical daisy-chained RS485 agricultural sensor chain.
pub static DEFAULT_SENSORS: &[SensorDefinition] = &[
    // Slave 1: Soil Moisture & Temperature Sensor at 10cm depth
    SensorDefinition {
        external_id: "modbus_soil_slave1_temp_10cm",
        provider: "rs485-esp32s3",
        category: "soil_temperature",
        measurement_unit: Some("°C"),
        depth_value: Some(10.0),
        depth_unit: Some("cm"),
        value_type: ResponseType::Numeric,
        slave_address: 1,
        function: ModbusFunction::ReadHoldingRegisters,
        register_address: 0x0000,
        register_count: 1,
        scaling: RegisterScaling::SignedScaled(0.1),
    },
    SensorDefinition {
        external_id: "modbus_soil_slave1_moisture_10cm",
        provider: "rs485-esp32s3",
        category: "soil_moisture",
        measurement_unit: Some("%"),
        depth_value: Some(10.0),
        depth_unit: Some("cm"),
        value_type: ResponseType::Numeric,
        slave_address: 1,
        function: ModbusFunction::ReadHoldingRegisters,
        register_address: 0x0001,
        register_count: 1,
        scaling: RegisterScaling::UnsignedScaled(0.1),
    },
    // Slave 2: Soil EC & pH Sensor at 10cm depth
    SensorDefinition {
        external_id: "modbus_soil_slave2_ec_10cm",
        provider: "rs485-esp32s3",
        category: "electrical_conductivity",
        measurement_unit: Some("µS/cm"),
        depth_value: Some(10.0),
        depth_unit: Some("cm"),
        value_type: ResponseType::Numeric,
        slave_address: 2,
        function: ModbusFunction::ReadHoldingRegisters,
        register_address: 0x0002,
        register_count: 1,
        scaling: RegisterScaling::UnsignedScaled(1.0),
    },
    SensorDefinition {
        external_id: "modbus_soil_slave2_ph_10cm",
        provider: "rs485-esp32s3",
        category: "soil_ph",
        measurement_unit: Some("pH"),
        depth_value: Some(10.0),
        depth_unit: Some("cm"),
        value_type: ResponseType::Numeric,
        slave_address: 2,
        function: ModbusFunction::ReadHoldingRegisters,
        register_address: 0x0003,
        register_count: 1,
        scaling: RegisterScaling::UnsignedScaled(0.01),
    },
    // Slave 3: Ambient Weather / Environment Sensor (Input Registers)
    SensorDefinition {
        external_id: "modbus_env_slave3_ambient_temp",
        provider: "rs485-esp32s3",
        category: "ambient_temperature",
        measurement_unit: Some("°C"),
        depth_value: None,
        depth_unit: None,
        value_type: ResponseType::Numeric,
        slave_address: 3,
        function: ModbusFunction::ReadInputRegisters,
        register_address: 0x0000,
        register_count: 1,
        scaling: RegisterScaling::SignedScaled(0.1),
    },
    SensorDefinition {
        external_id: "modbus_env_slave3_ambient_humidity",
        provider: "rs485-esp32s3",
        category: "ambient_humidity",
        measurement_unit: Some("%RH"),
        depth_value: None,
        depth_unit: None,
        value_type: ResponseType::Numeric,
        slave_address: 3,
        function: ModbusFunction::ReadInputRegisters,
        register_address: 0x0001,
        register_count: 1,
        scaling: RegisterScaling::UnsignedScaled(0.1),
    },
];

/// Default application configuration.
pub static DEFAULT_CONFIG: AppConfig = AppConfig {
    wifi: WifiConfig {
        ssid: "YOUR_WIFI_SSID",
        password: "YOUR_WIFI_PASSWORD",
    },
    server: ServerConfig {
        host: "192.168.1.100",
        port: 8080,
        auth_token: "CHANGE_ME_SECRET_AUTH_TOKEN",
    },
    modbus: ModbusConfig {
        baud_rate: 9600,
        timeout_ms: 1000,
        turnaround_delay_ms: 5,
    },
    poll_interval_secs: 30,
    sensors: DEFAULT_SENSORS,
};
