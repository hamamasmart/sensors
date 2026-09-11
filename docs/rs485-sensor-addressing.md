# RS485 Sensor Addressing (Modbus-RTU)

How to read and update (reprogram) the **slave address** (and baud rate) of each RS485 sensor used
by the fleet, over Modbus-RTU. This was previously implemented by the removed
`firmware-configurator` ESP32-S3 firmware crate; the protocol knowledge lives on here.

All sensors are Modbus-RTU slaves on RS485, configured through **holding registers**:

- **Read** registers with function code `0x03` (read holding registers).
- **Write** a single register with function code `0x06` (write single register). The sensor echoes
  the request verbatim (`slave + 0x06 + register + value + CRC16`, 8 bytes); validate the echo
  before considering the write accepted.

## Register maps per sensor model

| Model                       | Address register | Baud-rate register | Slave address range | Power cycle needed? |
| :---                        | :---:            | :---:              | :---:               | :---:               |
| `SN-300AL-GH-N01` (N01 RS485 family) | `0x07D0` | `0x07D1`           | 1–254               | No                  |
| `LightIntensityRs485`       | `0x0100`         | `0x0101`           | 1–251               | **Yes**             |

## Baud-rate encoding

Both sensor families store the baud rate as a small integer code:

| Register value | Baud rate |
| :---:          | :---:     |
| `0`            | 2400      |
| `1`            | 4800      |
| `2`            | 9600      |

Anything else is unsupported.

## Reading the current address and baud rate

Read **2 holding registers** starting at the model's address register — the pair returns
`[slave_address, baud_code]`:

```
request:  slave | 0x03 | addr_hi | addr_lo | 0x00 | 0x02 | CRC16
response: slave | 0x03 | 0x04 | addr_u16 | baud_code_u16 | CRC16
```

Must be done at the sensor's *current* slave address and baud rate.

## Updating the address (and baud rate)

Given **input** parameters (what the sensor is using now) and **output** parameters (the desired
settings), the sequence is:

1. **Write the address register** (FC `0x06`) with the output slave id, addressing the sensor at
   the *input* slave id and *input* baud rate.
2. **Write the baud-rate register** (FC `0x06`) with the output baud code, still at the input baud.
   If this write times out, the address change from step 1 took effect immediately (the SN-300
   family does this) — retry the write addressing the *output* slave id, still at the input baud.
3. **Verify** by re-reading the two registers (as above) at the output slave id / output baud.
   - For models that require a power cycle (the light-intensity sensor), the sensor won't answer
     after reprogramming until it is restarted: power-cycle it first, then read back with
     input = the just-programmed output values to confirm.
4. On failure to write at all: check wiring/power and that the sensor is really at the assumed
   input slave id and baud rate.

Timing parameters that worked in practice: Modbus response timeout 1000 ms, turnaround delay 5 ms,
UART 8N1.

## Bus hardware (ESP32-S3, Waveshare ESP32-S3-Relay-6CH)

The RS485 UART used by the ESP32-S3 board:

| Function        | GPIO    |
| :---            | :---    |
| RS485 TXD       | `GPIO17` |
| RS485 RXD       | `GPIO18` |
| RS485 direction | automatic (hardware transceiver, no GPIO) |

The ESP32 side acted as the Modbus **master**: a plain UART master with CRC16 framing, no
`DE`/`RE` direction GPIO — the Waveshare board handles transmit direction automatically.
