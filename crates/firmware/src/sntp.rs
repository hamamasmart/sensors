use chrono::{DateTime, Utc};
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, Stack};
use embassy_time::{Duration, Instant, with_timeout};

const NTP_PORT: u16 = 123;
// Difference in seconds between NTP epoch (1900-01-01) and Unix epoch (1970-01-01)
const NTP_TO_UNIX_OFFSET_SECS: u64 = 2_208_988_800;

/// Clock synchronized via SNTP with monotonic elapsed tracking.
pub struct SyncedClock {
    base_unix_secs: u64,
    sync_instant: Instant,
}

impl SyncedClock {
    pub const fn uninitialized() -> Self {
        Self {
            base_unix_secs: 0,
            sync_instant: Instant::from_ticks(0),
        }
    }

    /// Return the current UTC time.
    pub fn now(&self) -> DateTime<Utc> {
        let elapsed_secs = self.sync_instant.elapsed().as_secs();
        let current_secs = self.base_unix_secs + elapsed_secs;
        DateTime::<Utc>::from_timestamp(current_secs as i64, 0).unwrap_or_default()
    }

    /// Synchronize time against an NTP server.
    pub async fn sync(&mut self, stack: Stack<'_>, ntp_host: &str) -> Result<(), SntpError> {
        let server_ip = resolve_ntp_host(stack, ntp_host).await?;

        let mut rx_meta = [PacketMetadata::EMPTY; 2];
        let mut rx_buffer = [0u8; 128];
        let mut tx_meta = [PacketMetadata::EMPTY; 2];
        let mut tx_buffer = [0u8; 128];

        let mut socket = UdpSocket::new(
            stack,
            &mut rx_meta,
            &mut rx_buffer,
            &mut tx_meta,
            &mut tx_buffer,
        );

        socket.bind(0).map_err(|_| SntpError::BindFailed)?;

        // Standard SNTP v4 Request: LI = 0, VN = 4, Mode = 3 (Client)
        let mut request = [0u8; 48];
        request[0] = 0b00_100_011;

        with_timeout(
            Duration::from_secs(5),
            socket.send_to(&request, (server_ip, NTP_PORT)),
        )
        .await
        .map_err(|_| SntpError::Timeout)?
        .map_err(|_| SntpError::SendFailed)?;

        let mut response = [0u8; 48];
        let (len, _) = with_timeout(Duration::from_secs(5), socket.recv_from(&mut response))
            .await
            .map_err(|_| SntpError::Timeout)?
            .map_err(|_| SntpError::RecvFailed)?;

        if len < 48 {
            return Err(SntpError::InvalidResponse);
        }

        // Transmit Timestamp is at bytes 40..44 (seconds since 1900)
        let ntp_secs = u32::from_be_bytes([
            response[40],
            response[41],
            response[42],
            response[43],
        ]) as u64;

        if ntp_secs < NTP_TO_UNIX_OFFSET_SECS {
            return Err(SntpError::InvalidTimestamp);
        }

        let unix_secs = ntp_secs - NTP_TO_UNIX_OFFSET_SECS;
        self.base_unix_secs = unix_secs;
        self.sync_instant = Instant::now();

        log::info!("SNTP time synchronized: unix timestamp = {unix_secs}");
        Ok(())
    }
}

#[derive(Debug)]
pub enum SntpError {
    DnsLookupFailed,
    BindFailed,
    SendFailed,
    RecvFailed,
    Timeout,
    InvalidResponse,
    InvalidTimestamp,
}

async fn resolve_ntp_host(stack: Stack<'_>, host: &str) -> Result<IpAddress, SntpError> {
    if let Ok(ip) = host.parse::<core::net::Ipv4Addr>() {
        return Ok(IpAddress::Ipv4(ip));
    }

    let dns_client = embassy_net::dns::DnsSocket::new(stack);
    match dns_client.query(host, embassy_net::dns::DnsQueryType::A).await {
        Ok(addrs) => addrs.first().cloned().ok_or(SntpError::DnsLookupFailed),
        Err(_) => Err(SntpError::DnsLookupFailed),
    }
}
