use chrono::{DateTime, Utc};
use embassy_net::{
    IpAddress, Stack,
    dns::DnsSocket,
    udp::{PacketMetadata, UdpSocket},
};
use embassy_time::{Duration, Instant, with_timeout};
use sntpc::{NtpContext, get_time};
use sntpc_net_embassy::UdpSocketWrapper;
use sntpc_time_embassy::EmbassyTimestampGenerator;

const NTP_PORT: u16 = 123;
/// Re-synchronize against the NTP server this often so accumulated drift stays bounded.
const RESYNC_INTERVAL: Duration = Duration::from_secs(3600);

/// Clock synchronized via SNTP with monotonic elapsed tracking.
///
/// Uses the [`sntpc`] crate for the actual NTP exchange. Between synchronizations the current
/// UTC time is extrapolated from the last known NTP second plus embassy's monotonic elapsed
/// time; this is only valid while synchronized, so callers must gate on [`SyncedClock::is_synchronized`].
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

    /// Whether a successful NTP exchange has established a real wall-clock base.
    pub fn is_synchronized(&self) -> bool {
        self.base_unix_secs != 0
    }

    /// Whether enough time has passed since the last sync that we should re-sync.
    pub fn needs_resync(&self) -> bool {
        self.is_synchronized() && self.sync_instant.elapsed() >= RESYNC_INTERVAL
    }

    /// Return the current UTC time. Meaningless until synchronized.
    pub fn now(&self) -> DateTime<Utc> {
        let elapsed_secs = self.sync_instant.elapsed().as_secs();
        let current_secs = self.base_unix_secs.saturating_add(elapsed_secs);
        DateTime::<Utc>::from_timestamp(current_secs as i64, 0).unwrap_or_default()
    }

    /// Synchronize time against an NTP server using the [`sntpc`] client.
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
        let socket = UdpSocketWrapper::new(socket);

        let context = NtpContext::new(EmbassyTimestampGenerator::default());
        let server_addr = core::net::SocketAddr::new(server_ip, NTP_PORT);

        let result = with_timeout(
            Duration::from_secs(5),
            get_time(server_addr, &socket, context),
        )
        .await
        .map_err(|_| SntpError::Timeout)?
        .map_err(|_| SntpError::NoResponse)?;

        self.base_unix_secs = result.sec();
        self.sync_instant = Instant::now();
        log::info!(
            "SNTP synchronized: unix timestamp = {}",
            self.base_unix_secs
        );
        Ok(())
    }
}

#[derive(Debug)]
pub enum SntpError {
    DnsLookupFailed,
    BindFailed,
    Timeout,
    NoResponse,
}

async fn resolve_ntp_host(stack: Stack<'_>, host: &str) -> Result<core::net::IpAddr, SntpError> {
    if let Ok(ip) = host.parse::<core::net::Ipv4Addr>() {
        return Ok(core::net::IpAddr::V4(ip));
    }
    if let Ok(ip) = host.parse::<core::net::Ipv6Addr>() {
        return Ok(core::net::IpAddr::V6(ip));
    }

    let dns_client = DnsSocket::new(stack);
    match dns_client
        .query(host, embassy_net::dns::DnsQueryType::A)
        .await
    {
        Ok(addrs) => addrs
            .iter()
            .map(|ip| match ip {
                IpAddress::Ipv4(v) => core::net::IpAddr::V4(*v),
            })
            .next()
            .ok_or(SntpError::DnsLookupFailed),
        Err(_) => Err(SntpError::DnsLookupFailed),
    }
}
