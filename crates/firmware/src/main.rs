#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use embassy_executor::Spawner;
use embassy_net::{Config, StackResources};
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::timer::timg::TimerGroup;
use esp_hal::uart::{Config as UartConfig, Uart};
use esp_wifi::wifi::{WifiStaDevice, new_with_mode};
use static_cell::StaticCell;

use api_types::InsertMeasurementsRequest;
use firmware::config::{DEFAULT_CONFIG, DEFAULT_SENSORS};
use firmware::http_client::TelemetryHttpClient;
use firmware::modbus::ModbusMaster;
use firmware::sensors::SensorManager;
use firmware::sntp::SyncedClock;
use firmware::wifi::{net_task, wait_for_dhcp_ip, wifi_task};

// 72 KB heap allocation for Wi-Fi buffers, JSON serialization, and dynamic sensor lists.
esp_alloc::heap_allocator!(size: 72 * 1024);

static STACK_RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
static STACK: StaticCell<embassy_net::Stack<'static>> = StaticCell::new();

#[esp_hal::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    log::info!("Starting ESP32-S3 RS485 Modbus Telemetry Firmware");

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Initialize Embassy time driver
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_hal_embassy::init(timg0.timer0);

    // Initialize Wi-Fi peripheral & network stack
    let timg1 = TimerGroup::new(peripherals.TIMG1);
    let init = esp_wifi::init(
        timg1.timer0,
        esp_hal::rng::Rng::new(peripherals.RNG),
        peripherals.RADIO_CLK,
    )
    .unwrap();

    let (wifi_interface, controller) =
        new_with_mode(&init, peripherals.WIFI, WifiStaDevice).unwrap();

    let net_config = Config::dhcpv4(Default::default());
    let seed = 0x1234_5678_9abc_def0;
    let stack_resources = STACK_RESOURCES.init(StackResources::new());
    let (stack, runner) = embassy_net::new(wifi_interface, net_config, stack_resources, seed);
    let stack = STACK.init(stack);

    // Spawn background networking tasks
    spawner.spawn(net_task(runner)).unwrap();
    spawner
        .spawn(wifi_task(
            controller,
            DEFAULT_CONFIG.wifi.ssid,
            DEFAULT_CONFIG.wifi.password,
        ))
        .unwrap();

    // Wait for Wi-Fi connection and DHCP IP address
    wait_for_dhcp_ip(*stack).await;

    // Synchronize system clock via SNTP
    let mut clock = SyncedClock::uninitialized();
    if let Err(e) = clock.sync(*stack, "pool.ntp.org").await {
        log::warn!("Initial SNTP synchronization failed: {e:?}");
    }

    // Configure RS485 UART (UART1 on GPIO 17 TX, GPIO 18 RX) and GPIO 19 for DE/RE
    let uart_config = UartConfig::default().with_baudrate(DEFAULT_CONFIG.modbus.baud_rate);
    let uart = Uart::new(
        peripherals.UART1,
        uart_config,
    )
    .unwrap()
    .with_tx(peripherals.GPIO17)
    .with_rx(peripherals.GPIO18)
    .into_async();

    let de_pin = Output::new(peripherals.GPIO19, Level::Low, OutputConfig::default());
    let mut modbus = ModbusMaster::new(
        uart,
        de_pin,
        DEFAULT_CONFIG.modbus.timeout_ms,
        DEFAULT_CONFIG.modbus.turnaround_delay_ms,
    );

    let http_client = TelemetryHttpClient::new(*stack, DEFAULT_CONFIG.server.clone());
    let mut sensor_manager = SensorManager::new(DEFAULT_SENSORS);

    // Initial Registration: Upsert all sensors with the remote server
    log::info!("Registering sensors with server at http://{}:{}", DEFAULT_CONFIG.server.host, DEFAULT_CONFIG.server.port);
    for sensor in &mut sensor_manager.sensors {
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
                log::error!(
                    "Failed to register sensor '{}': {e:?}",
                    sensor.definition.external_id
                );
            }
        }
    }

    log::info!(
        "Entering periodic measurement loop (interval: {}s)",
        DEFAULT_CONFIG.poll_interval_secs
    );

    let poll_interval = Duration::from_secs(DEFAULT_CONFIG.poll_interval_secs);
    loop {
        let measured_at = clock.now();

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

                    let req = InsertMeasurementsRequest {
                        measurements: vec![measurement],
                    };

                    match http_client.insert_measurements(sensor_id, &req).await {
                        Ok(res) => {
                            log::info!(
                                "Uploaded measurement for '{}': {} row(s) inserted",
                                sensor.definition.external_id,
                                res.inserted
                            );
                        }
                        Err(e) => {
                            log::error!(
                                "Failed to upload measurement for '{}': {e:?}",
                                sensor.definition.external_id
                            );
                        }
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

        Timer::after(poll_interval).await;
    }
}
