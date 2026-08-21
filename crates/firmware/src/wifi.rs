use embassy_net::{Runner, Stack};
use embassy_time::{Duration, Timer};
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config, Interface, WifiController, sta::StationConfig,
};

/// Run the background network stack task.
#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, Interface>) -> ! {
    runner.run().await
}

/// Run the background Wi-Fi connection manager task.
#[embassy_executor::task]
pub async fn wifi_task(
    mut controller: WifiController<'static>,
    ssid: &'static str,
    password: &'static str,
) -> ! {
    log::info!("Starting Wi-Fi connection task");

    // Apply the station configuration (SSID + WPA2 credentials) once. The controller
    // is created in `main` with a default (empty) config, so without this call the
    // driver has no SSID and `connect_async` fails with `WifiError::InvalidSsid`.
    let station_config = Config::Station(
        StationConfig::default()
            .with_ssid(ssid.try_into().unwrap_or_default())
            .with_authentication(AuthenticationMethodConfig::Wpa2Personal(
                password.try_into().unwrap_or_default(),
            )),
    );
    if let Err(e) = controller.set_config(&station_config) {
        log::error!("Failed to apply Wi-Fi configuration (ssid: {ssid:?}): {e:?}");
    }

    loop {
        if controller.is_connected() {
            Timer::after(Duration::from_secs(5)).await;
            continue;
        }

        log::info!("Connecting to Wi-Fi \"{ssid}\"...");
        match controller.connect_async().await {
            Ok(_) => log::info!("Wi-Fi connected!"),
            Err(e) => {
                log::error!("Wi-Fi connection failed: {e:?}. Retrying in 5 seconds...");
                Timer::after(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Wait until DHCP assigns an IP address to the network stack.
pub async fn wait_for_dhcp_ip(stack: Stack<'_>) {
    log::info!("Waiting for DHCP IP assignment...");
    loop {
        if let Some(config) = stack.config_v4() {
            log::info!("Network ready! Assigned IP: {}", config.address.address());
            break;
        }
        Timer::after(Duration::from_millis(250)).await;
    }
}
