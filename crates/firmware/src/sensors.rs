extern crate alloc;

use alloc::vec::Vec;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{
    config::{RegisterScaling, SensorDefinition},
    modbus::{ModbusDirectionControl, ModbusError, ModbusMaster},
};
use api_types::{Measurement, MeasurementValue};

/// Runtime representation of a sensor on the daisy-chained bus.
pub struct ManagedSensor {
    pub definition: &'static SensorDefinition,
    pub server_sensor_id: Option<Uuid>,
}

impl ManagedSensor {
    pub fn new(definition: &'static SensorDefinition) -> Self {
        Self {
            definition,
            server_sensor_id: None,
        }
    }

    /// Read this sensor over Modbus RS485 and convert the registers to a Measurement.
    pub async fn read_measurement<U, DE>(
        &self,
        modbus: &mut ModbusMaster<U, DE>,
        measured_at: DateTime<Utc>,
    ) -> Result<Measurement, ModbusError>
    where
        U: embedded_io_async::Read + embedded_io_async::Write,
        DE: ModbusDirectionControl,
    {
        let registers = modbus
            .read_registers(
                self.definition.slave_address,
                self.definition.function,
                self.definition.register_address,
                self.definition.register_count,
            )
            .await?;

        let value = self.convert_registers(&registers)?;
        Ok(Measurement {
            value: MeasurementValue::Number(value),
            measured_at,
        })
    }

    fn convert_registers(&self, registers: &[u16]) -> Result<f64, ModbusError> {
        if registers.is_empty() {
            return Err(ModbusError::InvalidResponseLength);
        }

        match self.definition.scaling {
            RegisterScaling::UnsignedScaled(factor) => {
                let raw = registers[0] as f64;
                Ok(raw * factor)
            }
            RegisterScaling::SignedScaled(factor) => {
                let raw = (registers[0] as i16) as f64;
                Ok(raw * factor)
            }
            RegisterScaling::Raw => {
                let raw = registers[0] as f64;
                Ok(raw)
            }
            RegisterScaling::Float32Be => {
                if registers.len() < 2 {
                    return Err(ModbusError::InvalidResponseLength);
                }
                let bits = ((registers[0] as u32) << 16) | (registers[1] as u32);
                Ok(f32::from_bits(bits) as f64)
            }
        }
    }
}

/// Manager holding the collection of runtime managed sensors.
pub struct SensorManager {
    pub sensors: Vec<ManagedSensor>,
}

impl SensorManager {
    pub fn new(definitions: &'static [SensorDefinition]) -> Self {
        let sensors = definitions.iter().map(ManagedSensor::new).collect();
        Self { sensors }
    }
}
