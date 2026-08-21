# ESP32-S3 RS485 Sensor Configurator Firmware

Async Rust firmware for the ESP32-S3 (Waveshare ESP32-S3-Relay-6CH) that **reprograms a single
Modbus-RTU RS485 sensor's slave address and baud rate** in the field. You flash it once per
configuration job with the desired parameters, then flash the normal telemetry firmware back.

It reuses the Modbus master driver from the [`firmware`](../firmware) crate (extended here with a
write-single-register capability) and programs the two registers every supported sensor exposes for
its address and baud rate.

## Supported Sensors

| Model (`sensor_type`)        | Address register | Baud-rate register | Slave range | Power cycle? |
| :---                         | :---:            | :---:              | :---:       | :---:        |
| `SN-300AL-GH-N01`            | `0x07D0`         | `0x07D1`           | 1–254       | No           |
| `LightIntensityRs485`        | `0x0100`         | `0x0101`           | 1–251       | Yes          |

Both use function code `0x06` (write single holding register) and the same baud-rate encoding:
`0 = 2400`, `1 = 4800`, `2 = 9600`.

## How It Works

Given an **input** endpoint (the sensor's current `slave_id` + `baud_rate`) and an **output**
endpoint (the desired values), the firmware:

1. Talks to the sensor at the **input** parameters and writes the slave-address register to the
   output slave id (function `0x06`).
2. Writes the baud-rate register to the output baud code — still at the input baud. If the address
   change took effect immediately (some SN-300-family sensors do this) and the input slave stops
   replying, the write is retried against the **output** slave id.
3. Switches the UART to the **output** baud rate and reads the two registers back to verify. If the
   sensor requires a power cycle (e.g. the light-intensity sensor), this read won't answer yet — the
   logs instruct you to power-cycle and re-run with the output values as the new input.

## Configuration

Edit [`config.toml`](file:///home/shahar/IdeaProjects/sensors/crates/firmware-configurator/config.toml)
(or set `FIRMWARE_CONFIGURATOR_CONFIG` to an alternate path). Invalid slave ids or unsupported baud
rates fail the build with a clear message.

```toml
sensor_type = "SN-300AL-GH-N01"

[input]
slave_id = 1      # current address
baud_rate = 4800  # current baud rate

[output]
slave_id = 2      # desired address
baud_rate = 9600  # desired baud rate

[modbus]
timeout_ms = 1000
turnaround_delay_ms = 5
```

## Waveshare ESP32-S3-Relay-6CH Pinout

Same RS485 UART as the telemetry firmware:

| Function       | GPIO    |
| :---           | :---    |
| RS485 TXD      | `GPIO17` |
| RS485 RXD      | `GPIO18` |
| RS485 direction | automatic (no GPIO) |

## Build & Flash

The ESP32-S3 build requires the Espressif Rust toolchain and the same linker setup as the
`firmware` crate:

```bash
# Select the xtensa-capable esp toolchain (export-esp.sh alone is not enough).
export RUSTUP_TOOLCHAIN=esp

cd crates/firmware-configurator
espflash flash --monitor
```

The `.cargo/config.toml` pins the `xtensa-esp32s3-none-elf` target, `build-std`, and the
`-Tlinkall.x` linker script (do **not** add `-Trom-functions.x`).
