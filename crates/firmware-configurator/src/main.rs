#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    timer::timg::TimerGroup,
    uart::{Config as UartConfig, Uart},
};

use firmware::{
    config::ModbusFunction,
    modbus::{ModbusError, ModbusMaster},
};

use firmware_configurator::{
    config::DEFAULT_CONFIG,
    profiles::{decode_baud_rate, encode_baud_rate},
};

// Embed the ESP-IDF app descriptor so espflash accepts the image and the bootloader can verify it.
esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    // 32 KB heap for Modbus response buffers (read_registers returns a Vec).
    esp_alloc::heap_allocator!(size: 32 * 1024);

    esp_println::logger::init_logger_from_env();

    let cfg = &DEFAULT_CONFIG;
    let profile = cfg.sensor_type.profile();

    log::info!("ESP32-S3 RS485 Sensor Configurator");
    log::info!("Sensor model: {}", profile.name);
    log::info!(
        "Input  -> slave {} @ {} baud",
        cfg.input.slave_id,
        cfg.input.baud_rate
    );
    log::info!(
        "Output -> slave {} @ {} baud",
        cfg.output.slave_id,
        cfg.output.baud_rate
    );

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    // Initialize RTOS task scheduler & Embassy time driver on TIMG0.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // Configure RS485 UART on the Waveshare ESP32-S3-Relay-6CH:
    // TX = GPIO17, RX = GPIO18, with automatic hardware transceiver direction control.
    // Start at the *input* baud rate — that is what the sensor is currently using.
    let uart_config = UartConfig::default().with_baudrate(cfg.input.baud_rate);
    let uart = Uart::new(peripherals.UART1, uart_config)
        .unwrap()
        .with_tx(peripherals.GPIO17)
        .with_rx(peripherals.GPIO18)
        .into_async();

    let mut modbus = ModbusMaster::new_auto_direction(
        uart,
        cfg.modbus.timeout_ms,
        cfg.modbus.turnaround_delay_ms,
    );

    if cfg.input.slave_id == cfg.output.slave_id && cfg.input.baud_rate == cfg.output.baud_rate {
        log::info!("Input already equals output; nothing to configure.");
        halt().await;
    }

    let output_baud_code = match encode_baud_rate(cfg.output.baud_rate) {
        Some(code) => code,
        None => {
            log::error!(
                "Output baud rate {} is not supported (use 2400, 4800, or 9600)",
                cfg.output.baud_rate
            );
            halt().await;
        }
    };

    // 1) Write the slave-address register at the current (input) parameters.
    log::info!(
        "Writing address register 0x{:04X} -> slave {} ...",
        profile.address_register,
        cfg.output.slave_id
    );
    if let Err(e) = modbus
        .write_single_register(
            cfg.input.slave_id,
            profile.address_register,
            cfg.output.slave_id as u16,
        )
        .await
    {
        log::error!(
            "Failed to write address register. Is the sensor connected and powered, and \
             really at slave {} @ {} baud? Error: {:?}",
            cfg.input.slave_id,
            cfg.input.baud_rate,
            e
        );
        halt().await;
    }
    log::info!("Address register programmed.");

    // 2) Write the baud-rate register. The address change may have taken effect immediately on
    //    some models (SN-300 family), so if addressing the *input* slave times out, retry the
    //    write against the *output* slave (still at the input baud rate — the baud register has
    //    not been changed yet).
    log::info!(
        "Writing baud-rate register 0x{:04X} -> {} baud (code {}) ...",
        profile.baud_register,
        cfg.output.baud_rate,
        output_baud_code
    );
    match modbus
        .write_single_register(cfg.input.slave_id, profile.baud_register, output_baud_code)
        .await
    {
        Ok(()) => {
            log::info!("Baud-rate register programmed (addressed as input slave).");
        }
        Err(ModbusError::Timeout) => {
            log::warn!(
                "No reply at input slave {} — address change likely took effect immediately. \
                 Retrying baud-rate write at output slave {} ...",
                cfg.input.slave_id,
                cfg.output.slave_id
            );
            if let Err(e) = modbus
                .write_single_register(cfg.output.slave_id, profile.baud_register, output_baud_code)
                .await
            {
                log::error!("Failed to write baud-rate register: {:?}", e);
                halt().await;
            }
            log::info!("Baud-rate register programmed (addressed as output slave).");
        }
        Err(e) => {
            log::error!("Failed to write baud-rate register: {:?}", e);
            halt().await;
        }
    }

    // 3) Verify by reading the two registers back at the output parameters.
    //
    // Sensors that require a power cycle (e.g. the light-intensity sensor) won't answer at the new
    // baud until restarted, so for those we skip the read-back and just instruct a power cycle —
    // re-running with the output values as the new input verifies the result.
    if profile.requires_power_cycle {
        log::info!(
            "Registers programmed. This sensor model requires a power cycle for the new settings \
             to take effect. After power-cycling, re-run with input = (slave {}, {} baud) to verify.",
            cfg.output.slave_id,
            cfg.output.baud_rate
        );
        log::info!("Configuration sequence complete.");
        halt().await;
    }

    log::info!(
        "Switching UART to {} baud to verify the new settings ...",
        cfg.output.baud_rate
    );
    let verify_config = UartConfig::default().with_baudrate(cfg.output.baud_rate);
    if let Err(e) = modbus.uart_mut().apply_config(&verify_config) {
        log::error!("Failed to reconfigure UART baud rate: {:?}", e);
        halt().await;
    }

    match modbus
        .read_registers(
            cfg.output.slave_id,
            ModbusFunction::ReadHoldingRegisters,
            profile.address_register,
            2,
        )
        .await
    {
        Ok(regs) if regs.len() == 2 => {
            let addr = regs[0];
            let baud_code = regs[1];
            match decode_baud_rate(baud_code) {
                Some(baud)
                    if addr == cfg.output.slave_id as u16 && baud == cfg.output.baud_rate =>
                {
                    log::info!(
                        "Verified: slave={}, baud={} — configuration successful.",
                        addr,
                        baud
                    );
                }
                Some(baud) => {
                    log::warn!(
                        "Read back slave={}, baud={} (expected slave {}, {} baud). The sensor may \
                         require a power cycle for the new settings to take effect.",
                        addr,
                        baud,
                        cfg.output.slave_id,
                        cfg.output.baud_rate
                    );
                }
                None => {
                    log::warn!(
                        "Read back slave={}, unknown baud code {} — configuration may be incomplete.",
                        addr,
                        baud_code
                    );
                }
            }
        }
        Ok(_) => {
            log::warn!(
                "Unexpected read-back length. Registers were written; power-cycle the sensor to \
                 apply the new settings."
            );
        }
        Err(e) => {
            // Expected when the sensor needs a power cycle before the new baud rate takes effect.
            log::warn!(
                "Could not read back at slave {} @ {} baud ({:?}). This is expected if the sensor \
                 requires a power cycle for the new baud rate to take effect. Power-cycle the \
                 sensor, then re-run with input = (slave {}, {} baud) to verify.",
                cfg.output.slave_id,
                cfg.output.baud_rate,
                e,
                cfg.output.slave_id,
                cfg.output.baud_rate
            );
        }
    }

    log::info!("Configuration sequence complete.");
    halt().await;
}

/// Park forever once the configuration sequence is done (or has failed).
///
/// A configurator runs a single job per boot; there is no periodic loop to return to.
async fn halt() -> ! {
    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}
