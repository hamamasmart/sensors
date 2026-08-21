extern crate alloc;

use alloc::{format, string::ToString, vec::Vec};
use embassy_net::tcp::client::{TcpClientState, TcpConnection};
use embassy_time::{Duration, with_timeout};
use reqwless::{
    client::HttpResource,
    headers::ContentType,
    request::RequestBuilder,
};
use uuid::Uuid;

use crate::config::SensorDefinition;
use api_types::{
    InsertMeasurementsRequest, InsertMeasurementsResponse, ResponseType, UpsertSensorRequest,
    UpsertSensorResponse,
};

/// Capacity of the embassy TCP connection pool backing the HTTP client (single concurrent
/// request — the firmware issues one request at a time).
const POOL_SIZE: usize = 1;
const TCP_TX_SZ: usize = 1024;
const TCP_RX_SZ: usize = 1024;

/// A persistent keep-alive HTTP resource backed by a single embassy TCP connection.
///
/// The `'a` lifetime is the borrow of the [`HttpClient`] that owns the connection: a resource is
/// obtained once via `HttpClient::resource` and then reused for every request, so a single TCP
/// connection serves the whole measurement loop instead of being opened and abruptly torn down
/// per request. The connection is dropped (and a fresh resource established) only when it dies.
pub type HttpResourceConn<'a> =
    HttpResource<'a, TcpConnection<'a, POOL_SIZE, TCP_TX_SZ, TCP_RX_SZ>>;

/// Telemetry server credentials and addressing. This holds only static configuration; the live
/// [`HttpClient`] and persistent [`HttpResourceConn`] live in `main` so the resource can borrow the
/// client without a self-referential struct.
pub struct TelemetryHttpClient {
    host: &'static str,
    port: u16,
    auth_token: &'static str,
    provider: &'static str,
}

#[derive(Debug)]
pub enum HttpError {
    RequestFailed,
    HttpStatusError(u16),
    SerializationFailed,
    DeserializationFailed,
    /// No response within the per-request deadline. The connection may have a partially-read
    /// response, so the caller must drop the resource and re-establish a fresh one.
    Timeout,
    /// The keep-alive TCP connection died (server closed it, RST, or a read/write failed mid-frame).
    /// The caller drops the resource and establishes a new one; the failed request is retried.
    ConnectionDead,
}

impl TelemetryHttpClient {
    pub fn new(host: &'static str, port: u16, auth_token: &'static str, provider: &'static str) -> Self {
        Self {
            host,
            port,
            auth_token,
            provider,
        }
    }

    /// The base URL (`http://host:port`) for establishing a persistent keep-alive resource.
    pub fn base_url(&self) -> alloc::string::String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// Upsert a sensor registration on the server, returning its unique `sensor_id`.
    pub async fn upsert_sensor<'a>(
        &self,
        resource: &mut HttpResourceConn<'a>,
        sensor: &SensorDefinition,
    ) -> Result<UpsertSensorResponse, HttpError> {
        let request_body = UpsertSensorRequest {
            external_id: sensor.external_id.to_string(),
            provider: self.provider.to_string(),
            category: sensor.category.to_string(),
            measurement_unit: sensor.measurement_unit.map(|u| u.to_string()),
            depth_value: sensor.depth_value,
            depth_unit: sensor.depth_unit.map(|u| u.to_string()),
            // Modbus registers are inherently numeric; the server pins a sensor to its
            // value_type on first upsert, so always register as numeric here.
            value_type: ResponseType::Numeric,
        };

        let json_bytes =
            serde_json::to_vec(&request_body).map_err(|_| HttpError::SerializationFailed)?;
        let response_body = self.post_json(resource, "/sensors", &json_bytes).await?;
        let response: UpsertSensorResponse =
            serde_json::from_slice(&response_body).map_err(|_| HttpError::DeserializationFailed)?;

        Ok(response)
    }

    /// Batch-insert measurements for a sensor.
    pub async fn insert_measurements<'a>(
        &self,
        resource: &mut HttpResourceConn<'a>,
        sensor_id: Uuid,
        request: &InsertMeasurementsRequest,
    ) -> Result<InsertMeasurementsResponse, HttpError> {
        let path = format!("/sensors/{sensor_id}/measurements");
        let json_bytes = serde_json::to_vec(request).map_err(|_| HttpError::SerializationFailed)?;
        let response_body = self.post_json(resource, &path, &json_bytes).await?;
        let response: InsertMeasurementsResponse =
            serde_json::from_slice(&response_body).map_err(|_| HttpError::DeserializationFailed)?;

        Ok(response)
    }

    /// POST a JSON body to `path` over the persistent keep-alive `resource`, returning the
    /// response body on a 2xx status.
    ///
    /// The request is bounded by a deadline: reqwless applies no timeout of its own, and the
    /// embassy `TcpClient` socket timeout defaults to `None`, so without this guard a single
    /// unresponsive server response would deadlock the firmware. A timeout means the connection is
    /// now in an indeterminate (possibly mid-response) state, so it is reported as
    /// [`HttpError::Timeout`] and the caller drops the resource and reconnects.
    async fn post_json<'a>(
        &self,
        resource: &mut HttpResourceConn<'a>,
        path: &str,
        body: &[u8],
    ) -> Result<Vec<u8>, HttpError> {
        let bearer = format!("Bearer {}", self.auth_token);
        let headers = [("Authorization", bearer.as_str())];
        let mut rx_buf = [0u8; 2048];
        let mut out_buf = [0u8; 1024];

        // Per-request deadline. Generous for a normal round trip on an established connection,
        // short enough that a dead server is recovered within one poll cycle.
        const REQUEST_DEADLINE: Duration = Duration::from_secs(12);

        let request = async {
            let response = resource
                .post(path)
                .headers(&headers)
                .content_type(ContentType::ApplicationJson)
                .body(body)
                .send(&mut rx_buf)
                .await
                .map_err(map_send_error(path))?;

            if !response.status.is_successful() {
                log::warn!("HTTP {path} returned status {}", response.status.0);
                return Err(HttpError::HttpStatusError(response.status.0));
            }

            // Reading the full response body returns the keep-alive connection to idle for the
            // next request. A failure here means the connection is unusable — report it dead.
            let n = response
                .body()
                .reader()
                .read_to_end(&mut out_buf)
                .await
                .map_err(|_| HttpError::ConnectionDead)?;

            Ok(out_buf[..n].to_vec())
        };

        with_timeout(REQUEST_DEADLINE, request)
            .await
            .map_err(|_| {
                log::warn!("HTTP {path} timed out after {REQUEST_DEADLINE:?}; reconnecting");
                HttpError::Timeout
            })?
    }
}

/// Map a reqwless send error. Connection-level failures (server closed/reset, any network error)
/// mean the keep-alive connection is dead and must be re-established; everything else is a
/// one-off failure the caller retries without reconnecting.
fn map_send_error(path: &str) -> impl FnOnce(reqwless::Error) -> HttpError + '_ {
    move |e| {
        log::warn!("HTTP send to {path} failed: {e:?}");
        match e {
            reqwless::Error::ConnectionAborted
            | reqwless::Error::Network(_) => HttpError::ConnectionDead,
            _ => HttpError::RequestFailed,
        }
    }
}

/// Allocate the static TCP client state required by the reqwless HTTP client.
///
/// Returns a reference with `'static` lifetime; the underlying memory lives for program
/// duration. Call once at startup and pass the result to `TcpClient::new`.
pub fn make_tcp_client_state() -> &'static TcpClientState<POOL_SIZE, TCP_TX_SZ, TCP_RX_SZ> {
    static STATE: static_cell::StaticCell<TcpClientState<POOL_SIZE, TCP_TX_SZ, TCP_RX_SZ>> =
        static_cell::StaticCell::new();
    STATE.init(TcpClientState::new())
}
