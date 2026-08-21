//! Runtime configuration generated at build time from `config.toml`.
//!
//! [`build.rs`](../../build.rs) parses the TOML, validates the sensor parameters against the chosen
//! model's profile, and emits `DEFAULT_CONFIG` as a static constant for zero-overhead embedded use.

use crate::profiles::SensorType;

/// Modbus RS485 UART timing configuration.
#[derive(Clone, Copy, Debug)]
pub struct ModbusConfig {
    /// Per-transaction response timeout.
    pub timeout_ms: u64,
    /// Delay applied before each transmission (bus turnaround).
    pub turnaround_delay_ms: u64,
}

/// One communication endpoint: a slave id at a baud rate.
#[derive(Clone, Copy, Debug)]
pub struct Endpoint {
    /// Modbus slave address (1..=max for the chosen sensor model).
    pub slave_id: u8,
    /// Baud rate in bits/s (one of 2400, 4800, 9600).
    pub baud_rate: u32,
}

/// A full configurator job: reprogram a sensor from the `input` parameters to the `output` ones.
#[derive(Clone, Copy, Debug)]
pub struct ConfiguratorConfig {
    /// Sensor model to configure (selects the register map).
    pub sensor_type: SensorType,
    /// Current communication parameters the sensor is using right now.
    pub input: Endpoint,
    /// Desired communication parameters to program into the sensor.
    pub output: Endpoint,
    /// Modbus timing.
    pub modbus: ModbusConfig,
}

include!(concat!(env!("OUT_DIR"), "/config.rs"));
