extern crate alloc;

use alloc::{format, string::ToString, vec::Vec};
use embassy_net::{
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
};
use reqwless::{
    client::HttpClient,
    headers::ContentType,
    request::{Method, RequestBuilder},
};
use uuid::Uuid;

use crate::config::{SensorDefinition, ServerConfig};
use api_types::{
    InsertMeasurementsRequest, InsertMeasurementsResponse, ResponseType, UpsertSensorRequest,
    UpsertSensorResponse,
};

/// Capacity of the embassy TCP connection pool backing the HTTP client (single concurrent
/// request — the firmware issues one request at a time).
const POOL_SIZE: usize = 1;
const TCP_TX_SZ: usize = 1024;
const TCP_RX_SZ: usize = 1024;

/// HTTP client for communicating with the telemetry server, backed by [`reqwless`].
pub struct TelemetryHttpClient<'a> {
    client: HttpClient<'a, TcpClient<'a, POOL_SIZE, TCP_TX_SZ, TCP_RX_SZ>, DnsSocket<'a>>,
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
}

impl<'a> TelemetryHttpClient<'a> {
    pub fn new(
        tcp_client: &'a TcpClient<'a, POOL_SIZE, TCP_TX_SZ, TCP_RX_SZ>,
        dns: &'a DnsSocket<'a>,
        config: ServerConfig,
        provider: &'static str,
    ) -> Self {
        Self {
            client: HttpClient::new(tcp_client, dns),
            host: config.host,
            port: config.port,
            auth_token: config.auth_token,
            provider,
        }
    }

    /// Upsert a sensor registration on the server, returning its unique `sensor_id`.
    pub async fn upsert_sensor(
        &mut self,
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
        let response_body = self.post_json("/sensors", &json_bytes).await?;
        let response: UpsertSensorResponse =
            serde_json::from_slice(&response_body).map_err(|_| HttpError::DeserializationFailed)?;

        Ok(response)
    }

    /// Batch-insert measurements for a sensor.
    pub async fn insert_measurements(
        &mut self,
        sensor_id: Uuid,
        request: &InsertMeasurementsRequest,
    ) -> Result<InsertMeasurementsResponse, HttpError> {
        let path = format!("/sensors/{sensor_id}/measurements");
        let json_bytes = serde_json::to_vec(request).map_err(|_| HttpError::SerializationFailed)?;
        let response_body = self.post_json(&path, &json_bytes).await?;
        let response: InsertMeasurementsResponse =
            serde_json::from_slice(&response_body).map_err(|_| HttpError::DeserializationFailed)?;

        Ok(response)
    }

    /// POST a JSON body to `path` with the configured Bearer token, returning the response body
    /// on a 2xx status. reqwless handles DNS resolution (including IP-literal hosts), chunked
    /// transfer-encoding, and bounded response parsing.
    async fn post_json(&mut self, path: &str, body: &[u8]) -> Result<Vec<u8>, HttpError> {
        let url = format!("http://{}:{}{}", self.host, self.port, path);
        let bearer = format!("Bearer {}", self.auth_token);
        let headers = [("Authorization", bearer.as_str())];
        let mut rx_buf = [0u8; 2048];
        let mut out_buf = [0u8; 1024];

        // Bind the request handle so the returned response (which borrows its connection) has a
        // stable owner for the duration of the send + body read. `.body()` returns a new handle
        // type, so it must be a named `mut` local for `.send(&mut self)` to borrow stably.
        let handle = self.client.request(Method::POST, &url).await.map_err(|e| {
            log::warn!("HTTP request to {url} failed: {e:?}");
            HttpError::RequestFailed
        })?;
        let mut body_handle = handle
            .headers(&headers)
            .content_type(ContentType::ApplicationJson)
            .body(body);

        let response = body_handle.send(&mut rx_buf).await.map_err(|e| {
            log::warn!("HTTP send to {url} failed: {e:?}");
            HttpError::RequestFailed
        })?;

        if !response.status.is_successful() {
            log::warn!("HTTP {url} returned status {}", response.status.0);
            return Err(HttpError::HttpStatusError(response.status.0));
        }

        let n = response
            .body()
            .reader()
            .read_to_end(&mut out_buf)
            .await
            .map_err(|e| {
                log::warn!("HTTP body read from {url} failed: {e:?}");
                HttpError::RequestFailed
            })?;

        Ok(out_buf[..n].to_vec())
    }
}

/// Allocate the static TCP client state required by the reqwless HTTP client.
///
/// Returns a reference with `'static` lifetime; the underlying memory lives for program
/// duration. Call once at startup and pass the result to [`TelemetryHttpClient::new`].
pub fn make_tcp_client_state() -> &'static TcpClientState<POOL_SIZE, TCP_TX_SZ, TCP_RX_SZ> {
    static STATE: static_cell::StaticCell<TcpClientState<POOL_SIZE, TCP_TX_SZ, TCP_RX_SZ>> =
        static_cell::StaticCell::new();
    STATE.init(TcpClientState::new())
}
