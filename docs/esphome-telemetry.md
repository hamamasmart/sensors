# RS485 Sensor Telemetry over ESPHome

How the ESP32-S3 RS485 device reads the Modbus-RTU light sensors and pushes their readings to the
server. The device runs [ESPHome](https://esphome.io); the config lives at `esphome/esp1.yaml`.

How the sensors' slave addresses and baud rates are programmed is covered in
[rs485-sensor-addressing.md](rs485-sensor-addressing.md).

```
Sensors ──RS485──> ESP32-S3 (ESPHome) ──HTTP──> crates/server
                   uart 17/18 @ 9600 8N1        POST /readings (bearer auth)
                   modbus_controller FC03       upsert sensor + insert measurement
```

## Hardware

Board: **Waveshare ESP32-S3-RS485-CAN**. Its RS485 transceiver is isolated and **not**
auto-direction: firmware must drive the DE/RE pin (HIGH = transmit, LOW = receive). ESPHome's
`modbus.flow_control_pin` does exactly that.

| Function            | GPIO     |
| :---                | :---:    |
| RS485 TXD (UART1)   | `GPIO17` |
| RS485 RXD (UART1)   | `GPIO18` |
| RS485 DE/RE         | `GPIO21` |

Bus parameters: 9600 baud, 8N1. ESPHome times Modbus responses out on UART idle/silence
(baud-adaptive) and retries each command up to `max_cmd_retries` (default 4) times, so no fixed
response timeout is configured.

## Device config (`esphome/esp1.yaml`)

The file contains the device's base config (esphome/esp32/logger/api/ota/wifi) plus the telemetry
blocks. Fill in the placeholder secrets (`wifi`, `api` key) and check the substitutions at the top:

| Substitution   | Meaning                                                                 |
| :---           | :---                                                                    |
| `server_url`   | LAN host running `crates/server`, e.g. `http://10.100.102.19:8080`.     |
| `auth_token`   | The server's `AUTH_TOKEN`.                                              |
| `provider`     | Provider tag the sensors are stored under (see below).                  |
| `poll_interval`| Register poll and upload cadence.                                       |

Sensors are keyed server-side by `(external_id, provider)`, with the `provider` tag set by the
`provider` substitution (`rs485-esp32s3` by default).

## Server contract

`POST /readings` (bearer auth, batched) — one request per `poll_interval`, both readings together:

```json
{
  "provider": "rs485-esp32s3",
  "readings": [
    { "external_id": "room-lux", "category": "light_intensity", "measurement_unit": "lx",
      "value": 123, "measured_at": "2026-09-11T12:00:00Z" },
    { "external_id": "room-par", "category": "light_intensity", "measurement_unit": "PPFD",
      "value": 456, "measured_at": "2026-09-11T12:00:00Z" }
  ]
}
```

The server upserts each sensor (`ON CONFLICT (external_id, provider) DO UPDATE`, value type pinned
to `numeric` on first sight) and inserts its measurements with
`ON CONFLICT (sensor_id, measured_at) DO NOTHING` — re-posting the same timestamp is a no-op, so
device-side retries are safe. Readings are only sent once SNTP has synced (`time.has_time`-style
guard in the config), so the pre-sync 1970 clock never produces fake timestamps.

## Bringing it up

1. **Server (local/LAN):** `docker compose up -d postgres && cargo run -p server` — migrations run
   at startup, server binds `0.0.0.0:8080`. Smoke-test the endpoint:

   ```bash
   curl -is -X POST http://localhost:8080/readings \
     -H "Authorization: Bearer testing" -H "Content-Type: application/json" \
     -d '{"provider":"rs485-esp32s3","readings":[{"external_id":"room-lux","category":"light_intensity","measurement_unit":"lx","value":123,"measured_at":"2026-09-11T12:00:00Z"}]}'
   # -> {"inserted":1}; repeating the identical request -> {"inserted":0} (dedup)
   ```

2. **Device:** flash the config (e.g. `esphome run esphome/esp1.yaml`, or Build → Install in the
   ESPHome dashboard). The base config provides the ESPHome API and OTA, so the dashboard and
   over-the-air updates work.

3. **Check:** device logs show the modbus polls and the `http_request.post` calls; server side:

   ```sql
   SELECT external_id, category, measurement_unit, value_type FROM sensors WHERE provider = 'rs485-esp32s3';
   SELECT external_id, value, measured_at FROM measurements JOIN sensors USING (sensor_id)
     WHERE provider = 'rs485-esp32s3' ORDER BY measured_at DESC LIMIT 20;
   ```
