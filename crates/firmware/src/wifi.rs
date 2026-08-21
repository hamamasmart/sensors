use embassy_net::{Runner, Stack};
use embassy_time::{Duration, Timer};
use esp_wifi::wifi::{
    AuthMethod, ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent,
    WifiState,
};

/// Run the background network stack task.
#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, WifiDevice<'static, esp_wifi::wifi::WifiStaDevice>>) -> ! {
    runner.run().await
}

/// Run the background Wi-Fi connection manager task.
#[embassy_executor::task]
pub async fn wifi_task(
    mut controller: WifiController<'static>,
    ssid: &'static str,
    password: &'static str,
) -> ! {
    log::info!("Starting Wi-Fi connection task for SSID: {ssid}");

    loop {
        if esp_wifi::wifi::wifi_state() == WifiState::StaConnected {
            controller.wait_for_event(WifiEvent::StaDisconnected).await;
            log::warn!("Wi-Fi disconnected! Reconnecting in 2 seconds...");
            Timer::after(Duration::from_secs(2)).await;
        }

        if !controller.is_started().unwrap_or(false) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: ssid.try_into().unwrap_or_default(),
                password: password.try_into().unwrap_or_default(),
                auth_method: if password.is_empty() {
                    AuthMethod::None
                } else {
                    AuthMethod::WPA2Personal
                },
                ..Default::default()
            });

            if let Err(e) = controller.set_configuration(&client_config) {
                log::error!("Failed to set Wi-Fi config: {e:?}");
                Timer::after(Duration::from_secs(2)).await;
                continue;
            }

            if let Err(e) = controller.start().await {
                log::error!("Failed to start Wi-Fi controller: {e:?}");
                Timer::after(Duration::from_secs(2)).await;
                continue;
            }
        }

        log::info!("Connecting to Wi-Fi...");
        match controller.connect().await {
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
