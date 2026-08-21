extern crate alloc;

use alloc::vec::Vec;
use embassy_time::{Duration, Timer, with_timeout};
use embedded_hal::digital::OutputPin;
use embedded_io_async::{Read, Write};

use crate::config::ModbusFunction;
use async_modbus::{
    Frame, FrameBuilder, FrameView,
    pdu::request::{ReadHoldings, ReadInputs, WriteHolding},
    zerocopy::IntoBytes,
};

// Re-export async_modbus for downstream use.
pub use async_modbus;

/// Trait for RS485 direction pin control (or no-op for automatic direction hardware).
pub trait ModbusDirectionControl {
    fn enable_tx(&mut self);
    fn disable_tx(&mut self);
}

/// No-op direction control for hardware with automatic transceiver direction control (e.g. Waveshare).
#[derive(Clone, Copy, Debug, Default)]
pub struct AutoDirection;

impl ModbusDirectionControl for AutoDirection {
    fn enable_tx(&mut self) {}
    fn disable_tx(&mut self) {}
}

/// Direction control using a dedicated GPIO output pin (DE / RE).
pub struct PinDirection<P>(pub P);

impl<P: OutputPin> ModbusDirectionControl for PinDirection<P> {
    fn enable_tx(&mut self) {
        let _ = self.0.set_high();
    }
    fn disable_tx(&mut self) {
        let _ = self.0.set_low();
    }
}

/// Modbus communication errors.
#[derive(Debug)]
pub enum ModbusError {
    Timeout,
    IoError,
    CrcMismatch,
    InvalidResponseLength,
    InvalidSlaveAddress {
        expected: u8,
        actual: u8,
    },
    ExceptionResponse {
        function: u8,
        code: u8,
    },
    UnexpectedFunction {
        expected: u8,
        actual: u8,
    },
    InvalidByteCount {
        expected: u8,
        actual: u8,
    },
    /// Write-single-register (0x06) response did not echo the register/value that was sent.
    WriteEchoMismatch {
        register: u16,
        value: u16,
        echoed_register: u16,
        echoed_value: u16,
    },
}

/// Async Modbus Master driver with half-duplex RS485 direction management and turnaround timing.
///
/// Built on the [`async_modbus`] crate for frame construction and CRC validation. The response
/// is read in two phases (header first, then the variable-length remainder sized from the
/// response itself) so that Modbus **exception** frames — which are always 5 bytes regardless of
/// the requested quantity — are detected explicitly instead of stalling until the read timeout.
pub struct ModbusMaster<U, DE> {
    uart: U,
    direction: DE,
    timeout_ms: u64,
    turnaround_delay_ms: u64,
}

impl<U> ModbusMaster<U, AutoDirection> {
    pub fn new_auto_direction(uart: U, timeout_ms: u64, turnaround_delay_ms: u64) -> Self {
        Self {
            uart,
            direction: AutoDirection,
            timeout_ms,
            turnaround_delay_ms,
        }
    }
}

impl<U, P: OutputPin> ModbusMaster<U, PinDirection<P>> {
    pub fn new_pin_direction(
        uart: U,
        de_pin: P,
        timeout_ms: u64,
        turnaround_delay_ms: u64,
    ) -> Self {
        Self {
            uart,
            direction: PinDirection(de_pin),
            timeout_ms,
            turnaround_delay_ms,
        }
    }
}

impl<U, DE> ModbusMaster<U, DE>
where
    U: Read + Write,
    DE: ModbusDirectionControl,
{
    /// Borrow the underlying UART.
    ///
    /// Provided so callers (e.g. a sensor configurator) can reconfigure the UART at runtime —
    /// typically to switch the baud rate after programming a sensor's baud register.
    pub fn uart_mut(&mut self) -> &mut U {
        &mut self.uart
    }

    /// Read `quantity` registers from `slave_address` starting at `start_register`.
    pub async fn read_registers(
        &mut self,
        slave_address: u8,
        function: ModbusFunction,
        start_register: u16,
        quantity: u16,
    ) -> Result<Vec<u16>, ModbusError> {
        let function_code = match function {
            ModbusFunction::ReadHoldingRegisters => 0x03,
            ModbusFunction::ReadInputRegisters => 0x04,
        };

        // Always start from a clean RX buffer so leftover bytes from a previous
        // timed-out or malformed frame cannot desynchronize this read.
        self.drain_rx().await;

        // Build the request frame with async_modbus (unit id + PDU + CRC). The request PDUs
        // carry the register count as a runtime field, so a runtime `quantity` is fine. Both
        // read requests are 8 bytes on the wire (slave + func + start + qty + crc); copy the
        // bytes into an owned array so the builder (and its borrow) can be dropped safely.
        let mut request = [0u8; 8];
        match function {
            ModbusFunction::ReadHoldingRegisters => {
                let mut builder = FrameBuilder::<ReadHoldings>::new(slave_address);
                builder.pdu_mut().set_starting_register(start_register);
                builder.pdu_mut().set_n_registers(quantity);
                request.copy_from_slice(builder.build_ref().as_bytes());
            }
            ModbusFunction::ReadInputRegisters => {
                let mut builder = FrameBuilder::<ReadInputs>::new(slave_address);
                builder.pdu_mut().set_starting_register(start_register);
                builder.pdu_mut().set_n_registers(quantity);
                request.copy_from_slice(builder.build_ref().as_bytes());
            }
        }

        // Pre-transmission turnaround delay
        if self.turnaround_delay_ms > 0 {
            Timer::after(Duration::from_millis(self.turnaround_delay_ms)).await;
        }

        // Enable TX mode, send the frame, then switch back to RX.
        self.direction.enable_tx();
        self.uart
            .write_all(&request)
            .await
            .map_err(|_| ModbusError::IoError)?;
        self.uart.flush().await.map_err(|_| ModbusError::IoError)?;
        self.direction.disable_tx();

        // Read and parse the response with a single overall timeout.
        let timeout = Duration::from_millis(self.timeout_ms);
        let mut buffer = [0u8; 256];
        let len = with_timeout(
            timeout,
            self.read_frame(&mut buffer, slave_address, function_code, quantity),
        )
        .await
        .map_err(|_| ModbusError::Timeout)??;

        // Validate the frame via async_modbus: FrameView checks the length and CRC.
        let frame_view =
            FrameView::try_from_bytes(&buffer[..len]).ok_or(ModbusError::InvalidResponseLength)?;
        let pdu = frame_view.pdu().map_err(|_| ModbusError::CrcMismatch)?;

        // Exception response: the function code has its high bit set (0x80 | func).
        if pdu.function_code & 0x80 != 0 {
            let code = pdu.data.first().copied().unwrap_or(0);
            return Err(ModbusError::ExceptionResponse {
                function: pdu.function_code & 0x7F,
                code,
            });
        }

        if pdu.function_code != function_code {
            return Err(ModbusError::UnexpectedFunction {
                expected: function_code,
                actual: pdu.function_code,
            });
        }

        // Normal response PDU data: [byte_count, reg0_hi, reg0_lo, reg1_hi, ...].
        let byte_count = *pdu.data.first().ok_or(ModbusError::InvalidResponseLength)?;
        let expected_byte_count = (2 * quantity) as u8;
        if byte_count != expected_byte_count {
            return Err(ModbusError::InvalidByteCount {
                expected: expected_byte_count,
                actual: byte_count,
            });
        }

        let reg_bytes = pdu
            .data
            .get(1..1 + byte_count as usize)
            .ok_or(ModbusError::InvalidResponseLength)?;

        let mut registers = Vec::with_capacity(quantity as usize);
        for chunk in reg_bytes.chunks_exact(2) {
            registers.push(u16::from_be_bytes([chunk[0], chunk[1]]));
        }
        Ok(registers)
    }

    /// Write a single holding register (function code 0x06) on `slave_address`.
    ///
    /// The sensor echoes the request verbatim (`slave + 0x06 + register + value + CRC`, 8 bytes).
    /// The response is validated for CRC, function code (including exception frames), and that the
    /// echoed register/value match what was sent — so a corrupted or mis-routed reply is rejected
    /// rather than silently accepted.
    pub async fn write_single_register(
        &mut self,
        slave_address: u8,
        register: u16,
        value: u16,
    ) -> Result<(), ModbusError> {
        const FUNCTION_CODE: u8 = 0x06;

        // Always start from a clean RX buffer so leftover bytes from a previous frame cannot
        // desynchronize this transaction.
        self.drain_rx().await;

        // Build the 8-byte write-single-register frame (unit id + PDU + CRC) with async_modbus.
        // Copy into an owned array so the frame (and its borrow) is dropped before transmission.
        let frame = Frame::new(
            slave_address,
            WriteHolding::new()
                .with_register(register)
                .with_value(value),
        );
        let mut request = [0u8; 8];
        request.copy_from_slice(frame.as_bytes());

        // Pre-transmission turnaround delay
        if self.turnaround_delay_ms > 0 {
            Timer::after(Duration::from_millis(self.turnaround_delay_ms)).await;
        }

        // Enable TX mode, send the frame, then switch back to RX.
        self.direction.enable_tx();
        self.uart
            .write_all(&request)
            .await
            .map_err(|_| ModbusError::IoError)?;
        self.uart.flush().await.map_err(|_| ModbusError::IoError)?;
        self.direction.disable_tx();

        // Read and parse the echoed response with a single overall timeout.
        let timeout = Duration::from_millis(self.timeout_ms);
        let mut buffer = [0u8; 256];
        let len = with_timeout(timeout, self.read_write_frame(&mut buffer, slave_address))
            .await
            .map_err(|_| ModbusError::Timeout)??;

        // Validate the frame via async_modbus: FrameView checks the length and CRC.
        let frame_view =
            FrameView::try_from_bytes(&buffer[..len]).ok_or(ModbusError::InvalidResponseLength)?;
        let pdu = frame_view.pdu().map_err(|_| ModbusError::CrcMismatch)?;

        // Exception response: the function code has its high bit set (0x80 | func).
        if pdu.function_code & 0x80 != 0 {
            let code = pdu.data.first().copied().unwrap_or(0);
            return Err(ModbusError::ExceptionResponse {
                function: pdu.function_code & 0x7F,
                code,
            });
        }

        if pdu.function_code != FUNCTION_CODE {
            return Err(ModbusError::UnexpectedFunction {
                expected: FUNCTION_CODE,
                actual: pdu.function_code,
            });
        }

        // Normal write response PDU data: [reg_hi, reg_lo, val_hi, val_lo].
        let data = &pdu.data;
        if data.len() != 4 {
            return Err(ModbusError::InvalidResponseLength);
        }
        let echoed_register = u16::from_be_bytes([data[0], data[1]]);
        let echoed_value = u16::from_be_bytes([data[2], data[3]]);
        if echoed_register != register || echoed_value != value {
            return Err(ModbusError::WriteEchoMismatch {
                register,
                value,
                echoed_register,
                echoed_value,
            });
        }

        Ok(())
    }

    /// Read a full Modbus-RTU response frame into `buffer`, sizing it from the response itself.
    ///
    /// Two phases:
    /// 1. Read the 3-byte header: `[slave, function, byte_count_or_excode]`.
    /// 2. If the function byte has its high bit set, this is a 5-byte exception frame — read the
    ///    remaining 2 CRC bytes. Otherwise read `byte_count` data bytes + 2 CRC bytes.
    ///    The total length is returned (validated to fit the 256-byte buffer).
    async fn read_frame(
        &mut self,
        buffer: &mut [u8; 256],
        slave_address: u8,
        function_code: u8,
        quantity: u16,
    ) -> Result<usize, ModbusError> {
        // Phase 1: slave + function + control byte (byte count or exception code).
        Self::read_exact(&mut self.uart, &mut buffer[..3]).await?;

        if buffer[0] != slave_address {
            return Err(ModbusError::InvalidSlaveAddress {
                expected: slave_address,
                actual: buffer[0],
            });
        }

        let is_exception = buffer[1] & 0x80 != 0;
        if is_exception {
            // Exception frame: [slave, 0x80|func, excode, crc_lo, crc_hi] — read 2 more.
            Self::read_exact(&mut self.uart, &mut buffer[3..5]).await?;
            return Ok(5);
        }

        if buffer[1] != function_code {
            return Err(ModbusError::UnexpectedFunction {
                expected: function_code,
                actual: buffer[1],
            });
        }

        // Phase 2 (normal): read `byte_count` data bytes + 2 CRC bytes.
        let byte_count = buffer[2] as usize;
        let expected = 2 * quantity as usize;
        // Bounds-check before slicing: protects against a malformed/garbage byte count.
        if byte_count != expected {
            return Err(ModbusError::InvalidByteCount {
                expected: expected as u8,
                actual: buffer[2],
            });
        }
        let end = 3 + byte_count + 2; // header + data + crc
        if end > buffer.len() {
            return Err(ModbusError::InvalidResponseLength);
        }
        Self::read_exact(&mut self.uart, &mut buffer[3..end]).await?;
        Ok(end)
    }

    /// Read a write-single-register (0x06) response into `buffer`.
    ///
    /// Two phases, mirroring [`read_frame`], so that a 5-byte Modbus **exception** frame is
    /// detected explicitly instead of stalling until the read timeout waiting for the 8 bytes
    /// a normal echo would contain:
    /// 1. Read the 3-byte header: `[slave, function, reg_hi_or_excode]`.
    /// 2. If the function byte has its high bit set, this is a 5-byte exception frame — read the
    ///    remaining 2 CRC bytes. Otherwise read `reg_lo + value + crc` (5 bytes) for 8 total.
    async fn read_write_frame(
        &mut self,
        buffer: &mut [u8; 256],
        slave_address: u8,
    ) -> Result<usize, ModbusError> {
        const FUNCTION_CODE: u8 = 0x06;

        // Phase 1: slave + function + control byte (register high byte or exception code).
        Self::read_exact(&mut self.uart, &mut buffer[..3]).await?;

        if buffer[0] != slave_address {
            return Err(ModbusError::InvalidSlaveAddress {
                expected: slave_address,
                actual: buffer[0],
            });
        }

        let is_exception = buffer[1] & 0x80 != 0;
        if is_exception {
            // Exception frame: [slave, 0x86, excode, crc_lo, crc_hi] — read 2 more.
            Self::read_exact(&mut self.uart, &mut buffer[3..5]).await?;
            return Ok(5);
        }

        if buffer[1] != FUNCTION_CODE {
            return Err(ModbusError::UnexpectedFunction {
                expected: FUNCTION_CODE,
                actual: buffer[1],
            });
        }

        // Phase 2 (normal): reg_lo + val_hi + val_lo + crc_lo + crc_hi (5 bytes).
        Self::read_exact(&mut self.uart, &mut buffer[3..8]).await?;
        Ok(8)
    }

    /// Read exactly `buf.len()` bytes, blocking until full or error.
    ///
    /// `Ok(0)` from the UART (peer closed / no device) is treated as an error: a Modbus
    /// response that ends early is malformed.
    async fn read_exact(uart: &mut U, buf: &mut [u8]) -> Result<(), ModbusError> {
        let mut filled = 0;
        while filled < buf.len() {
            match uart.read(&mut buf[filled..]).await {
                Ok(0) => return Err(ModbusError::IoError),
                Ok(n) => filled += n,
                Err(_) => return Err(ModbusError::IoError),
            }
        }
        Ok(())
    }

    /// Drain any leftover bytes from the RX FIFO so they cannot corrupt the next read.
    ///
    /// Reads with a short per-attempt timeout until no more bytes arrive. This is bounded: each
    /// iteration either discards pending bytes or times out (ending the drain).
    async fn drain_rx(&mut self) {
        let mut discard = [0u8; 32];
        while let Ok(Ok(n)) =
            with_timeout(Duration::from_millis(2), self.uart.read(&mut discard)).await
        {
            if n == 0 {
                break;
            }
        }
    }
}
