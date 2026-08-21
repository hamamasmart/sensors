//! Supported configurable RS485 Modbus-RTU sensor models and their register maps.
//!
//! Both supported sensor families are programmed the same way — a Modbus **write single
//! register** (function code `0x06`) to a model-specific holding register holding the slave
//! address, and another holding the baud-rate code. They differ only in *which* registers those
//! are and the legal slave-address range, which is captured by [`SensorProfile`].

/// A configurable RS485 sensor model.
///
/// Each variant is paired (via [`SensorType::profile`]) with the vendor-specific holding-register
/// addresses the sensor uses to persist its slave address and baud rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SensorType {
    /// SN-300AL-GH-N01 — and the broader "N01" RS485 family that shares the
    /// `0x07D0`/`0x07D1` address/baud-rate register map (slave addresses `1..=254`).
    Sn300AlGhN01,
    /// Generic RS485 light-intensity sensor (address register `0x0100`, baud-rate register
    /// `0x0101`, slave addresses `1..=251`). Requires a power cycle after programming.
    LightIntensityRs485,
}

impl SensorType {
    /// The wire profile (register addresses + limits) for this sensor model.
    pub const fn profile(self) -> SensorProfile {
        match self {
            SensorType::Sn300AlGhN01 => SensorProfile {
                name: "SN-300AL-GH-N01",
                address_register: 0x07D0,
                baud_register: 0x07D1,
                max_slave_address: 254,
                requires_power_cycle: false,
            },
            SensorType::LightIntensityRs485 => SensorProfile {
                name: "LightIntensity-RS485",
                address_register: 0x0100,
                baud_register: 0x0101,
                max_slave_address: 251,
                requires_power_cycle: true,
            },
        }
    }
}

/// Wire profile describing where a sensor stores its address and baud rate.
#[derive(Clone, Copy, Debug)]
pub struct SensorProfile {
    /// Human-readable model name for logs.
    pub name: &'static str,
    /// Holding register holding the Modbus slave address.
    pub address_register: u16,
    /// Holding register holding the baud-rate code (see [`encode_baud_rate`]).
    pub baud_register: u16,
    /// Maximum legal slave address for this model (minimum is always 1).
    pub max_slave_address: u8,
    /// Whether the sensor must be power-cycled for the new settings to take effect.
    pub requires_power_cycle: bool,
}

/// Baud rates supported by both sensor families.
pub const SUPPORTED_BAUD_RATES: &[u32] = &[2400, 4800, 9600];

/// Encode a baud rate (bits/s) into the register value both sensors expect.
///
/// `2400 → 0`, `4800 → 1`, `9600 → 2`. Any other rate is unsupported and returns `None`.
pub const fn encode_baud_rate(baud_rate: u32) -> Option<u16> {
    match baud_rate {
        2400 => Some(0),
        4800 => Some(1),
        9600 => Some(2),
        _ => None,
    }
}

/// Decode a register baud-rate value back to bits/s.
pub const fn decode_baud_rate(code: u16) -> Option<u32> {
    match code {
        0 => Some(2400),
        1 => Some(4800),
        2 => Some(9600),
        _ => None,
    }
}
