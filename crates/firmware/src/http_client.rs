extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, Stack};
use embassy_time::{Duration, with_timeout};
use embedded_io_async::{Read, Write};
use uuid::Uuid;

use api_types::{
    InsertMeasurementsRequest, InsertMeasurementsResponse, UpsertSensorRequest,
    UpsertSensorResponse,
};
use crate::config::{SensorDefinition, ServerConfig};

#[derive(Debug)]
pub enum HttpError {
    DnsLookupFailed,
    ConnectionFailed,
    WriteFailed,
    ReadFailed,
    Timeout,
    InvalidHttpResponse,
    HttpStatusError(u16),
    SerializationFailed,
    DeserializationFailed,
}

/// HTTP client for communicating with the telemetry server.
pub struct TelemetryHttpClient<'a> {
    stack: Stack<'a>,
    config: ServerConfig,
}

impl<'a> TelemetryHttpClient<'a> {
    pub fn new(stack: Stack<'a>, config: ServerConfig) -> Self {
        Self { stack, config }
    }

    /// Upsert a sensor registration on the server, returning its unique `sensor_id`.
    pub async fn upsert_sensor(
        &self,
        sensor: &SensorDefinition,
    ) -> Result<UpsertSensorResponse, HttpError> {
        let request_body = UpsertSensorRequest {
            external_id: sensor.external_id.to_string(),
            provider: sensor.provider.to_string(),
            category: sensor.category.to_string(),
            measurement_unit: sensor.measurement_unit.map(|u| u.to_string()),
            depth_value: sensor.depth_value,
            depth_unit: sensor.depth_unit.map(|u| u.to_string()),
            value_type: sensor.value_type,
        };

        let json_bytes = serde_json::to_vec(&request_body).map_err(|_| HttpError::SerializationFailed)?;
        let response_body = self.send_post_request("/sensors", &json_bytes).await?;
        let response: UpsertSensorResponse =
            serde_json::from_slice(&response_body).map_err(|_| HttpError::DeserializationFailed)?;

        Ok(response)
    }

    /// Batch-insert measurements for a sensor.
    pub async fn insert_measurements(
        &self,
        sensor_id: Uuid,
        request: &InsertMeasurementsRequest,
    ) -> Result<InsertMeasurementsResponse, HttpError> {
        let path = format!("/sensors/{sensor_id}/measurements");
        let json_bytes = serde_json::to_vec(request).map_err(|_| HttpError::SerializationFailed)?;
        let response_body = self.send_post_request(&path, &json_bytes).await?;
        let response: InsertMeasurementsResponse =
            serde_json::from_slice(&response_body).map_err(|_| HttpError::DeserializationFailed)?;

        Ok(response)
    }

    async fn resolve_server_ip(&self) -> Result<IpAddress, HttpError> {
        if let Ok(ip) = self.config.host.parse::<core::net::Ipv4Addr>() {
            return Ok(IpAddress::Ipv4(ip));
        }

        let dns_client = embassy_net::dns::DnsSocket::new(self.stack);
        match dns_client.query(self.config.host, embassy_net::dns::DnsQueryType::A).await {
            Ok(addrs) => addrs.first().cloned().ok_or(HttpError::DnsLookupFailed),
            Err(_) => Err(HttpError::DnsLookupFailed),
        }
    }

    async fn send_post_request(&self, path: &str, body: &[u8]) -> Result<Vec<u8>, HttpError> {
        let server_ip = self.resolve_server_ip().await?;

        let mut rx_buffer = [0u8; 4096];
        let mut tx_buffer = [0u8; 4096];
        let mut socket = TcpSocket::new(self.stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(15)));

        let remote_endpoint = (server_ip, self.config.port);
        with_timeout(Duration::from_secs(10), socket.connect(remote_endpoint))
            .await
            .map_err(|_| HttpError::Timeout)?
            .map_err(|_| HttpError::ConnectionFailed)?;

        let request_header = format!(
            "POST {} HTTP/1.1\r\n\
             Host: {}:{}\r\n\
             Authorization: Bearer {}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n",
            path,
            self.config.host,
            self.config.port,
            self.config.auth_token,
            body.len()
        );

        socket
            .write_all(request_header.as_bytes())
            .await
            .map_err(|_| HttpError::WriteFailed)?;
        socket
            .write_all(body)
            .await
            .map_err(|_| HttpError::WriteFailed)?;
        socket.flush().await.map_err(|_| HttpError::WriteFailed)?;

        // Read HTTP response
        let mut response_bytes = Vec::new();
        let mut temp_buf = [0u8; 1024];
        loop {
            match socket.read(&mut temp_buf).await {
                Ok(0) => break,
                Ok(n) => response_bytes.extend_from_slice(&temp_buf[..n]),
                Err(_) => break,
            }
        }

        Self::parse_http_response(&response_bytes)
    }

    fn parse_http_response(raw_response: &[u8]) -> Result<Vec<u8>, HttpError> {
        let response_str = core::str::from_utf8(raw_response).map_err(|_| HttpError::InvalidHttpResponse)?;

        let header_end = response_str
            .find("\r\n\r\n")
            .ok_or(HttpError::InvalidHttpResponse)?;
        let headers_part = &response_str[..header_end];
        let body_part = &raw_response[header_end + 4..];

        let mut lines = headers_part.lines();
        let status_line = lines.next().ok_or(HttpError::InvalidHttpResponse)?;

        let mut parts = status_line.split_whitespace();
        let _http_version = parts.next().ok_or(HttpError::InvalidHttpResponse)?;
        let status_code_str = parts.next().ok_or(HttpError::InvalidHttpResponse)?;
        let status_code: u16 = status_code_str
            .parse()
            .map_err(|_| HttpError::InvalidHttpResponse)?;

        if status_code < 200 || status_code >= 300 {
            return Err(HttpError::HttpStatusError(status_code));
        }

        Ok(body_part.to_vec())
    }
}
