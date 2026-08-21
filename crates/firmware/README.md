# ESP32-S3 Modbus-RTU RS485 Telemetry Firmware

Async Rust firmware built with the `embassy` framework and `esp-hal` for ESP32-S3 microcontrollers (including the **Waveshare ESP32-S3-Relay-6CH** board).

## Features

- **Modbus-RTU RS485 Master**: Queries daisy-chained sensors over RS485 with hardware auto-direction or manual DE/RE pin control.
- **Wi-Fi & DHCP**: Managed connection and auto-reconnect using `esp-wifi` and `embassy-net`.
- **SNTP UTC Synchronization**: Ensures measurements have accurate UTC timestamps.
- **Backend Telemetry Integration**:
  - Automatically registers/upserts sensors on startup (`POST /sensors`).
  - Periodically reads sensors and pushes telemetry batches (`POST /sensors/:sensor_id/measurements`) with Bearer token authentication.
- **Build-time Configuration Generation**: Configures Wi-Fi, backend, Modbus, and daisy-chained sensor definitions via `config.toml` (generated at compile-time via `build.rs`).

## Configuration

Firmware configuration is defined in [`config.toml`](file:///home/shahar/IdeaProjects/sensors/crates/firmware/config.toml) (or customized via the `FIRMWARE_CONFIG` environment variable). During the build process, [`build.rs`](file:///home/shahar/IdeaProjects/sensors/crates/firmware/build.rs) parses the TOML file and generates static Rust constants in `OUT_DIR/config.rs` for zero-overhead embedded execution.

Environment variable overrides are also supported at build time:
- `ESP_WIFI_SSID`
- `ESP_WIFI_PASSWORD`
- `ESP_SERVER_HOST`
- `ESP_SERVER_PORT`
- `ESP_SERVER_AUTH_TOKEN`

## Waveshare ESP32-S3-Relay-6CH Pinout

The Waveshare ESP32-S3-Relay-6CH has the isolated RS485 transceiver and relays wired to the following GPIOs:

| Function / Peripheral | ESP32-S3 GPIO | Description |
| :--- | :--- | :--- |
| **RS485 TXD** | `GPIO17` | Hardware connected to onboard RS485 transceiver |
| **RS485 RXD** | `GPIO18` | Hardware connected to onboard RS485 transceiver |
| **RS485 Direction (DE/RE)** | *Automatic* | Hardware automatic direction control (no GPIO needed) |
| **Relay 1 (CH1)** | `GPIO1` | Onboard Relay Channel 1 |
| **Relay 2 (CH2)** | `GPIO2` | Onboard Relay Channel 2 |
| **Relay 3 (CH3)** | `GPIO41` | Onboard Relay Channel 3 |
| **Relay 4 (CH4)** | `GPIO42` | Onboard Relay Channel 4 |
| **Relay 5 (CH5)** | `GPIO45` | Onboard Relay Channel 5 |
| **Relay 6 (CH6)** | `GPIO46` | Onboard Relay Channel 6 |
| **Buzzer** | `GPIO21` | Active buzzer |
| **RGB LED** | `GPIO38` | WS2812 RGB LED |

## Prerequisites

Install the Espressif Rust toolchain and flashing utility:

```bash
cargo install espup
espup install
cargo install espflash
```

## Flashing to ESP32-S3

To build, flash, and open the serial monitor:

```bash
cd crates/firmware
espflash flash --monitor
```
