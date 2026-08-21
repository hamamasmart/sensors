# ESP32-S3 Modbus-RTU RS485 Telemetry Firmware

Async Rust firmware built with the `embassy` framework and `esp-hal` for ESP32-S3 microcontrollers.

## Features

- **Modbus-RTU RS485 Master**: Queries daisy-chained sensors over half-duplex RS485 with hardware or software DE/RE direction control.
- **Wi-Fi & DHCP**: Managed connection and auto-reconnect using `esp-wifi` and `embassy-net`.
- **SNTP UTC Synchronization**: Ensures measurements have accurate UTC timestamps.
- **Backend Telemetry Integration**:
  - Automatically registers/upserts sensors on startup (`POST /sensors`).
  - Periodically reads sensors and pushes telemetry batches (`POST /sensors/:sensor_id/measurements`) with Bearer token authentication.

## Hardware Wiring

| ESP32-S3 Pin | RS485 Transceiver (e.g. MAX485 / SP3485) | Description |
| :--- | :--- | :--- |
| `GPIO17` | `DI` (Driver Input) | UART TX |
| `GPIO18` | `RO` (Receiver Output) | UART RX |
| `GPIO19` | `DE` & `RE` (tied together) | Driver / Receiver Enable |
| `3V3` / `5V` | `VCC` | Power |
| `GND` | `GND` | Ground |

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
