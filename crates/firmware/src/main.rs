#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use embassy_executor::Spawner;
use embassy_net::{Config, StackResources, dns::DnsSocket, tcp::client::TcpClient};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    timer::timg::TimerGroup,
    uart::{Config as UartConfig, Uart},
};
use esp_radio::wifi::{Interface, WifiController};
use static_cell::StaticCell;
use uuid::Uuid;

use api_types::{InsertMeasurementsRequest, Measurement};
use firmware::{
    config::{DEFAULT_CONFIG, DEFAULT_SENSORS},
    http_client::{TelemetryHttpClient, make_tcp_client_state},
    modbus::ModbusMaster,
    sensors::SensorManager,
    sntp::SyncedClock,
    wifi::{net_task, wait_for_dhcp_ip, wifi_task},
};

// Embed the ESP-IDF app descriptor so espflash accepts the image and the bootloader can verify it.
esp_bootloader_esp_idf::esp_app_desc!();

static STACK_RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
static STACK: StaticCell<embassy_net::Stack<'static>> = StaticCell::new();

/// A measurement awaiting upload (or retrying after a failed upload).
struct PendingMeasurement {
    sensor_id: Uuid,
    measurement: Measurement,
}

/// Bounded ring buffer (channel) of measurements pending a successful upload.
/// On a failed immediate upload the measurement is enqueued here and retried in later cycles.
type RetryQueue = Channel<CriticalSectionRawMutex, PendingMeasurement, 16>;

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // 72 KB heap allocation for Wi-Fi buffers, JSON serialization, and dynamic sensor lists.
    esp_alloc::heap_allocator!(size: 72 * 1024);

    esp_println::logger::init_logger_from_env();
    log::info!("Starting Waveshare ESP32-S3-Relay-6CH RS485 Modbus Telemetry Firmware");

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Initialize RTOS task scheduler & Embassy time driver on TIMG0
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // Initialize Wi-Fi controller & station network interface using esp-radio
    let controller = WifiController::new(peripherals.WIFI, Default::default()).unwrap();
    let wifi_interface = Interface::station();

    let net_config = Config::dhcpv4(Default::default());
    let seed = 0x1234_5678_9abc_def0;
    let stack_resources = STACK_RESOURCES.init(StackResources::new());
    let (stack, runner) = embassy_net::new(wifi_interface, net_config, stack_resources, seed);
    let stack = STACK.init(stack);

    // Spawn background networking tasks
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(
        wifi_task(
            controller,
            DEFAULT_CONFIG.wifi.ssid,
            DEFAULT_CONFIG.wifi.password,
        )
        .unwrap(),
    );

    // Wait for Wi-Fi connection and DHCP IP address
    wait_for_dhcp_ip(*stack).await;

    // HTTP client state backed by a single-connection embassy TCP pool + the stack's DNS.
    let tcp_state = make_tcp_client_state();
    let tcp_client = TcpClient::new(*stack, tcp_state);
    let dns = DnsSocket::new(*stack);
    let mut http_client = TelemetryHttpClient::new(
        &tcp_client,
        &dns,
        DEFAULT_CONFIG.server.clone(),
        DEFAULT_CONFIG.provider,
    );

    let mut sensor_manager = SensorManager::new(DEFAULT_SENSORS);
    let retry_queue = RetryQueue::new();

    // Configure RS485 UART on Waveshare ESP32-S3-Relay-6CH:
    // TX = GPIO17, RX = GPIO18.
    // The Waveshare board features automatic hardware transceiver direction control.
    let uart_config = UartConfig::default().with_baudrate(DEFAULT_CONFIG.modbus.baud_rate);
    let uart = Uart::new(peripherals.UART1, uart_config)
        .unwrap()
        .with_tx(peripherals.GPIO17)
        .with_rx(peripherals.GPIO18)
        .into_async();

    let mut modbus = ModbusMaster::new_auto_direction(
        uart,
        DEFAULT_CONFIG.modbus.timeout_ms,
        DEFAULT_CONFIG.modbus.turnaround_delay_ms,
    );

    // Synchronize system clock via SNTP. Failures are non-fatal here — the loop below keeps
    // retrying and refuses to publish until a real wall-clock base is established.
    let mut clock = SyncedClock::uninitialized();
    if let Err(e) = clock.sync(*stack, "pool.ntp.org").await {
        log::warn!("Initial SNTP synchronization failed: {e:?}");
    }

    log::info!(
        "Entering periodic measurement loop (interval: {}s)",
        DEFAULT_CONFIG.poll_interval_secs
    );

    let poll_interval = Duration::from_secs(DEFAULT_CONFIG.poll_interval_secs);
    loop {
        // Ensure the clock is synchronized before stamping any measurements. An unsynced clock
        // would produce ~1970 timestamps; keep retrying and skip reading until it succeeds.
        if !clock.is_synchronized() {
            log::warn!("Clock not synchronized; retrying SNTP...");
            if let Err(e) = clock.sync(*stack, "pool.ntp.org").await {
                log::error!("SNTP retry failed: {e:?}");
                Timer::after(poll_interval).await;
                continue;
            }
        } else if clock.needs_resync()
            && let Err(e) = clock.sync(*stack, "pool.ntp.org").await
        {
            log::warn!("Periodic SNTP re-sync failed: {e:?}");
        }

        // Registration with retry (#5): any sensor still missing its server id is re-registered
        // each cycle so a transient boot-time failure doesn't brick it for the whole uptime.
        for sensor in &mut sensor_manager.sensors {
            if sensor.server_sensor_id.is_some() {
                continue;
            }
            match http_client.upsert_sensor(sensor.definition).await {
                Ok(res) => {
                    sensor.server_sensor_id = Some(res.sensor_id);
                    log::info!(
                        "Sensor '{}' registered. ID: {}, Last measured: {:?}",
                        sensor.definition.external_id,
                        res.sensor_id,
                        res.last_measured_at
                    );
                }
                Err(e) => {
                    log::warn!(
                        "Failed to register sensor '{}': {e:?}; will retry next cycle",
                        sensor.definition.external_id
                    );
                }
            }
        }

        let measured_at = clock.now();

        // Read each registered sensor and enqueue its measurement (with an immediate upload
        // attempt). Failures are retained in the retry queue rather than dropped.
        for sensor in &sensor_manager.sensors {
            let Some(sensor_id) = sensor.server_sensor_id else {
                log::warn!(
                    "Skipping sensor '{}': not registered on server",
                    sensor.definition.external_id
                );
                continue;
            };

            log::info!(
                "Reading sensor '{}' (Slave: {}, Reg: 0x{:04X})...",
                sensor.definition.external_id,
                sensor.definition.slave_address,
                sensor.definition.register_address
            );

            match sensor.read_measurement(&mut modbus, measured_at).await {
                Ok(measurement) => {
                    log::info!(
                        "Sensor '{}' reading: {:?} at {}",
                        sensor.definition.external_id,
                        measurement.value,
                        measurement.measured_at
                    );

                    // Immediate upload; on failure, retain for retry.
                    let req = InsertMeasurementsRequest {
                        measurements: vec![measurement.clone()],
                    };
                    if let Err(e) = http_client.insert_measurements(sensor_id, &req).await {
                        log::warn!(
                            "Failed to upload measurement for '{}': {e:?}; enqueuing for retry",
                            sensor.definition.external_id
                        );
                        let _ = retry_queue.try_send(PendingMeasurement {
                            sensor_id,
                            measurement,
                        });
                    }
                }
                Err(e) => {
                    log::error!(
                        "Modbus error reading sensor '{}': {e:?}",
                        sensor.definition.external_id
                    );
                }
            }

            // Inter-sensor bus delay
            Timer::after(Duration::from_millis(50)).await;
        }

        // Drain the retry queue (#6): attempt each pending measurement. Stop as soon as one
        // fails (a downed server would otherwise be hammered every cycle); remaining items
        // stay queued for the next cycle. Re-enqueue the failing item at the back.
        drain_retry_queue(&retry_queue, &mut http_client).await;

        Timer::after(poll_interval).await;
    }
}

/// Attempt to upload every pending measurement in `queue`. The first failure aborts the drain
/// for this cycle (with the failed item re-queued) so a persistent server outage doesn't spin.
async fn drain_retry_queue(queue: &RetryQueue, http_client: &mut TelemetryHttpClient<'_>) {
    while let Ok(pending) = queue.try_receive() {
        let req = InsertMeasurementsRequest {
            measurements: vec![pending.measurement.clone()],
        };
        match http_client
            .insert_measurements(pending.sensor_id, &req)
            .await
        {
            Ok(_) => {
                log::info!("Retried upload succeeded for sensor {}", pending.sensor_id);
            }
            Err(e) => {
                log::warn!(
                    "Retried upload failed for sensor {}: {e:?}; will retry next cycle",
                    pending.sensor_id
                );
                let _ = queue.try_send(pending);
                break;
            }
        }
    }
}
