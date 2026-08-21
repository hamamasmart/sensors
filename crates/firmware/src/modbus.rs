extern crate alloc;

use alloc::vec::Vec;
use embedded_hal::digital::OutputPin;
use embedded_io_async::{Read, Write};
use embassy_time::{Duration, Timer, with_timeout};

use crate::config::ModbusFunction;

/// Modbus communication errors.
#[derive(Debug)]
pub enum ModbusError {
    Timeout,
    IoError,
    CrcMismatch { expected: u16, actual: u16 },
    InvalidResponseLength,
    InvalidSlaveAddress { expected: u8, actual: u8 },
    ExceptionResponse { function: u8, code: u8 },
    UnexpectedFunction { expected: u8, actual: u8 },
}

/// Calculate standard Modbus-RTU CRC-16 (Polynomial 0xA001, initial value 0xFFFF).
pub fn calculate_crc(data: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &byte in data {
        crc ^= byte as u16;
        for _ in 0..8 {
            if (crc & 0x0001) != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

/// Asynchronous Modbus-RTU Master managing RS485 half-duplex communication.
pub struct ModbusMaster<U, DE> {
    uart: U,
    de_pin: DE,
    timeout: Duration,
    turnaround_delay: Duration,
}

impl<U, DE> ModbusMaster<U, DE>
where
    U: Read + Write,
    DE: OutputPin,
{
    pub fn new(uart: U, mut de_pin: DE, timeout_ms: u64, turnaround_delay_ms: u64) -> Self {
        let _ = de_pin.set_low();
        Self {
            uart,
            de_pin,
            timeout: Duration::from_millis(timeout_ms),
            turnaround_delay: Duration::from_millis(turnaround_delay_ms),
        }
    }

    /// Read holding (0x03) or input (0x04) registers from a target slave device.
    pub async fn read_registers(
        &mut self,
        slave_address: u8,
        function: ModbusFunction,
        start_register: u16,
        quantity: u16,
    ) -> Result<Vec<u16>, ModbusError> {
        let function_code = function as u8;

        // Build 8-byte request frame
        let mut request = [0u8; 8];
        request[0] = slave_address;
        request[1] = function_code;
        request[2] = (start_register >> 8) as u8;
        request[3] = (start_register & 0xFF) as u8;
        request[4] = (quantity >> 8) as u8;
        request[5] = (quantity & 0xFF) as u8;

        let crc = calculate_crc(&request[0..6]);
        request[6] = (crc & 0xFF) as u8;
        request[7] = (crc >> 8) as u8;

        // RS485 Transmit: Enable Driver (DE = HIGH)
        let _ = self.de_pin.set_high();
        Timer::after(Duration::from_micros(50)).await;

        self.uart
            .write_all(&request)
            .await
            .map_err(|_| ModbusError::IoError)?;
        self.uart.flush().await.map_err(|_| ModbusError::IoError)?;

        // RS485 Turnaround: Switch back to Receive (DE = LOW)
        Timer::after(self.turnaround_delay).await;
        let _ = self.de_pin.set_low();

        // Expected normal response length: 1 (slave) + 1 (fn) + 1 (byte count) + 2 * quantity + 2 (crc)
        let expected_bytes = 5 + (quantity as usize * 2);
        let mut rx_buf = [0u8; 256];

        let bytes_read = match with_timeout(self.timeout, self.read_response(&mut rx_buf, expected_bytes)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(ModbusError::Timeout),
        };

        if bytes_read < 5 {
            return Err(ModbusError::InvalidResponseLength);
        }

        // Validate Slave Address
        if rx_buf[0] != slave_address {
            return Err(ModbusError::InvalidSlaveAddress {
                expected: slave_address,
                actual: rx_buf[0],
            });
        }

        // Check for Exception Response (bit 7 set)
        if rx_buf[1] == (function_code | 0x80) {
            let received_crc = (rx_buf[bytes_read - 1] as u16) << 8 | (rx_buf[bytes_read - 2] as u16);
            let calculated = calculate_crc(&rx_buf[0..bytes_read - 2]);
            if received_crc != calculated {
                return Err(ModbusError::CrcMismatch {
                    expected: calculated,
                    actual: received_crc,
                });
            }
            return Err(ModbusError::ExceptionResponse {
                function: function_code,
                code: rx_buf[2],
            });
        }

        // Validate Function Code
        if rx_buf[1] != function_code {
            return Err(ModbusError::UnexpectedFunction {
                expected: function_code,
                actual: rx_buf[1],
            });
        }

        let byte_count = rx_buf[2] as usize;
        if byte_count != quantity as usize * 2 || bytes_read < 3 + byte_count + 2 {
            return Err(ModbusError::InvalidResponseLength);
        }

        // Validate CRC-16
        let total_frame_len = 3 + byte_count + 2;
        let received_crc = (rx_buf[total_frame_len - 1] as u16) << 8 | (rx_buf[total_frame_len - 2] as u16);
        let calculated_crc = calculate_crc(&rx_buf[0..total_frame_len - 2]);
        if received_crc != calculated_crc {
            return Err(ModbusError::CrcMismatch {
                expected: calculated_crc,
                actual: received_crc,
            });
        }

        // Parse 16-bit register values (Big Endian)
        let mut registers = Vec::with_capacity(quantity as usize);
        for i in 0..quantity as usize {
            let offset = 3 + i * 2;
            let val = ((rx_buf[offset] as u16) << 8) | (rx_buf[offset + 1] as u16);
            registers.push(val);
        }

        Ok(registers)
    }

    async fn read_response(&mut self, buf: &mut [u8], expected_len: usize) -> Result<usize, ModbusError> {
        let mut total_read = 0;
        while total_read < expected_len {
            let n = self
                .uart
                .read(&mut buf[total_read..expected_len])
                .await
                .map_err(|_| ModbusError::IoError)?;
            if n == 0 {
                break;
            }
            total_read += n;
        }
        Ok(total_read)
    }
}
