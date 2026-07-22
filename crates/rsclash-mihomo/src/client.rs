use std::{collections::HashMap, fmt, net::IpAddr, time::Duration};

use async_trait::async_trait;
use reqwest::{Method, StatusCode, header};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::time::sleep;
use tracing::warn;

use crate::{
    ControllerConfig, ControllerEndpoint, Error, MihomoApi, Result,
    models::{
        BaseConfig, Connections, CoreUpdaterChannel, Groups, Proxies, Proxy, ProxyDelay,
        ProxyProvider, ProxyProviders, RuleProviders, Rules, VersionInfo,
    },
};

const LONG_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const RETRY_BASE_DELAY: Duration = Duration::from_millis(25);
const MAX_ERROR_MESSAGE_BYTES: usize = 512;

#[derive(Clone)]
pub struct MihomoClient {
    config: ControllerConfig,
    http: reqwest::Client,
    base_url: String,
    sends_authorization: bool,
}

impl MihomoClient {
    pub fn new(config: ControllerConfig) -> Result<Self> {
        let (builder, base_url, sends_authorization) = match &config.endpoint {
            ControllerEndpoint::Http { host, port } => {
                if host.trim().is_empty() || host.contains('/') {
                    return Err(Error::InvalidConfiguration(
                        "the HTTP controller host must not be empty or contain a slash".to_string(),
                    ));
                }
                let host = format_host(host);
                (
                    reqwest::Client::builder(),
                    format!("http://{host}:{port}"),
                    true,
                )
            }
            ControllerEndpoint::UnixSocket(path) => {
                if path.as_os_str().is_empty() {
                    return Err(Error::InvalidConfiguration(
                        "the Unix socket path must not be empty".to_string(),
                    ));
                }
                #[cfg(unix)]
                {
                    (
                        reqwest::Client::builder().unix_socket(path.as_path()),
                        "http://localhost".to_string(),
                        false,
                    )
                }
                #[cfg(not(unix))]
                {
                    return Err(Error::UnsupportedTransport("Unix domain socket"));
                }
            }
            ControllerEndpoint::NamedPipe(_) => {
                return Err(Error::UnsupportedTransport("Windows named pipe"));
            }
        };

        let http = builder
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(8)
            .build()
            .map_err(|error| Error::Transport(error.to_string()))?;

        Ok(Self {
            config,
            http,
            base_url,
            sends_authorization,
        })
    }

    pub fn endpoint(&self) -> &ControllerEndpoint {
        &self.config.endpoint
    }

    pub async fn health_check(&self) -> Result<VersionInfo> {
        self.version().await
    }

    async fn get_json<T>(&self, context: &'static str, path: String) -> Result<T>
    where
        T: DeserializeOwned,
    {
        self.request_json(context, RequestSpec::get(path)).await
    }

    async fn request_json<T>(&self, context: &'static str, request: RequestSpec) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let response = self.send(request).await?;
        serde_json::from_slice(&response).map_err(|source| Error::Decode { context, source })
    }

    async fn request_empty(&self, request: RequestSpec) -> Result<()> {
        self.send(request).await.map(|_| ())
    }

    async fn send(&self, request: RequestSpec) -> Result<Vec<u8>> {
        let retries = if request.method == Method::GET {
            self.config.max_safe_retries
        } else {
            0
        };

        for attempt in 0..=retries {
            match self.send_once(&request).await {
                Ok(body) => return Ok(body),
                Err(Error::Timeout(_) | Error::Transport(_)) if attempt < retries => {
                    let backoff = 1_u32 << u32::from(attempt.min(6));
                    let delay = RETRY_BASE_DELAY.saturating_mul(backoff);
                    warn!(
                        attempt,
                        ?delay,
                        "retrying a safe Mihomo request after transport failure"
                    );
                    sleep(delay).await;
                }
                Err(error) => return Err(error),
            }
        }

        Err(Error::Transport(
            "safe retry loop ended unexpectedly".to_string(),
        ))
    }

    async fn send_once(&self, request: &RequestSpec) -> Result<Vec<u8>> {
        let url = format!("{}{}", self.base_url, request.path);
        let mut builder = self
            .http
            .request(request.method.clone(), url)
            .timeout(request.timeout.unwrap_or(self.config.request_timeout));

        if self.sends_authorization && !self.config.secret.is_empty() {
            builder = builder.bearer_auth(self.config.secret.expose());
        }
        if let Some(body) = &request.body {
            builder = builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.clone());
        }

        let timeout = request.timeout.unwrap_or(self.config.request_timeout);
        let response = builder.send().await.map_err(|error| {
            if error.is_timeout() {
                Error::Timeout(timeout)
            } else {
                Error::Transport(error.to_string())
            }
        })?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|error| Error::Transport(error.to_string()))?
            .to_vec();

        if status.is_success() {
            Ok(body)
        } else {
            Err(status_error(status, &body))
        }
    }
}

impl fmt::Debug for MihomoClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MihomoClient")
            .field("endpoint", &self.config.endpoint)
            .field("request_timeout", &self.config.request_timeout)
            .field("max_safe_retries", &self.config.max_safe_retries)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl MihomoApi for MihomoClient {
    async fn version(&self) -> Result<VersionInfo> {
        self.get_json("version", "/version".to_string()).await
    }

    async fn flush_fake_ip_cache(&self) -> Result<()> {
        self.request_empty(RequestSpec::new(Method::POST, "/cache/fakeip/flush"))
            .await
    }

    async fn flush_dns_cache(&self) -> Result<()> {
        self.request_empty(RequestSpec::new(Method::POST, "/cache/dns/flush"))
            .await
    }

    async fn connections(&self) -> Result<Connections> {
        self.get_json("connections", "/connections".to_string())
            .await
    }

    async fn close_all_connections(&self) -> Result<()> {
        self.request_empty(RequestSpec::new(Method::DELETE, "/connections"))
            .await
    }

    async fn close_connection(&self, connection_id: &str) -> Result<()> {
        self.request_empty(RequestSpec::new(
            Method::DELETE,
            format!("/connections/{}", encode(connection_id)),
        ))
        .await
    }

    async fn groups(&self) -> Result<Groups> {
        self.get_json("groups", "/group".to_string()).await
    }

    async fn group(&self, name: &str) -> Result<Proxy> {
        self.get_json("group", format!("/group/{}", encode(name)))
            .await
    }

    async fn delay_group(
        &self,
        name: &str,
        test_url: &str,
        timeout_ms: u32,
    ) -> Result<HashMap<String, u32>> {
        let path = format!(
            "/group/{}/delay?url={}&timeout={timeout_ms}",
            encode(name),
            encode(test_url)
        );
        self.request_json(
            "group delay",
            RequestSpec::get(path).with_timeout(delay_timeout(timeout_ms)),
        )
        .await
    }

    async fn proxy_providers(&self) -> Result<ProxyProviders> {
        self.get_json("proxy providers", "/providers/proxies".to_string())
            .await
    }

    async fn proxy_provider(&self, name: &str) -> Result<ProxyProvider> {
        self.get_json(
            "proxy provider",
            format!("/providers/proxies/{}", encode(name)),
        )
        .await
    }

    async fn update_proxy_provider(&self, name: &str) -> Result<()> {
        self.request_empty(RequestSpec::new(
            Method::PUT,
            format!("/providers/proxies/{}", encode(name)),
        ))
        .await
    }

    async fn healthcheck_proxy_provider(&self, name: &str) -> Result<()> {
        self.request_empty(RequestSpec::get(format!(
            "/providers/proxies/{}/healthcheck",
            encode(name)
        )))
        .await
    }

    async fn healthcheck_provider_proxy(
        &self,
        provider: &str,
        proxy: &str,
        test_url: &str,
        timeout_ms: u32,
    ) -> Result<ProxyDelay> {
        let path = format!(
            "/providers/proxies/{}/{}/healthcheck?url={}&timeout={timeout_ms}",
            encode(provider),
            encode(proxy),
            encode(test_url)
        );
        match self
            .request_json(
                "provider proxy delay",
                RequestSpec::get(path).with_timeout(delay_timeout(timeout_ms)),
            )
            .await
        {
            Err(error) if is_delay_timeout_error(&error) => Ok(ProxyDelay { delay: 0 }),
            result => result,
        }
    }

    async fn proxies(&self) -> Result<Proxies> {
        self.get_json("proxies", "/proxies".to_string()).await
    }

    async fn proxy(&self, name: &str) -> Result<Proxy> {
        self.get_json("proxy", format!("/proxies/{}", encode(name)))
            .await
    }

    async fn select_proxy(&self, group: &str, proxy: &str) -> Result<()> {
        self.request_empty(
            RequestSpec::new(Method::PUT, format!("/proxies/{}", encode(group)))
                .with_json(&json!({ "name": proxy }))?,
        )
        .await
    }

    async fn clear_fixed_proxy(&self, group: &str) -> Result<()> {
        self.request_empty(RequestSpec::new(
            Method::DELETE,
            format!("/proxies/{}", encode(group)),
        ))
        .await
    }

    async fn delay_proxy(&self, name: &str, test_url: &str, timeout_ms: u32) -> Result<ProxyDelay> {
        let path = format!(
            "/proxies/{}/delay?url={}&timeout={timeout_ms}",
            encode(name),
            encode(test_url)
        );
        match self
            .request_json(
                "proxy delay",
                RequestSpec::get(path).with_timeout(delay_timeout(timeout_ms)),
            )
            .await
        {
            Err(error) if is_delay_timeout_error(&error) => Ok(ProxyDelay { delay: 0 }),
            result => result,
        }
    }

    async fn rules(&self) -> Result<Rules> {
        self.get_json("rules", "/rules".to_string()).await
    }

    async fn rule_providers(&self) -> Result<RuleProviders> {
        self.get_json("rule providers", "/providers/rules".to_string())
            .await
    }

    async fn update_rule_provider(&self, name: &str) -> Result<()> {
        self.request_empty(RequestSpec::new(
            Method::PUT,
            format!("/providers/rules/{}", encode(name)),
        ))
        .await
    }

    async fn base_config(&self) -> Result<BaseConfig> {
        self.get_json("base config", "/configs".to_string()).await
    }

    async fn reload_config(&self, path: &str, force: bool) -> Result<()> {
        self.request_empty(
            RequestSpec::new(Method::PUT, format!("/configs?force={force}"))
                .with_json(&json!({ "path": path }))?
                .with_timeout(LONG_REQUEST_TIMEOUT),
        )
        .await
    }

    async fn patch_base_config(&self, patch: Value) -> Result<()> {
        self.request_empty(RequestSpec::new(Method::PATCH, "/configs").with_json(&patch)?)
            .await
    }

    async fn update_geo(&self) -> Result<()> {
        self.request_empty(
            RequestSpec::new(Method::POST, "/configs/geo").with_timeout(LONG_REQUEST_TIMEOUT),
        )
        .await
    }

    async fn restart(&self) -> Result<()> {
        self.request_empty(RequestSpec::new(Method::POST, "/restart"))
            .await
    }

    async fn upgrade_core(&self, channel: CoreUpdaterChannel, force: bool) -> Result<()> {
        self.request_empty(
            RequestSpec::new(
                Method::POST,
                format!("/upgrade?channel={}&force={force}", channel.as_str()),
            )
            .with_timeout(LONG_REQUEST_TIMEOUT),
        )
        .await
    }

    async fn upgrade_ui(&self) -> Result<()> {
        self.request_empty(
            RequestSpec::new(Method::POST, "/upgrade/ui").with_timeout(LONG_REQUEST_TIMEOUT),
        )
        .await
    }

    async fn upgrade_geo(&self) -> Result<()> {
        self.request_empty(
            RequestSpec::new(Method::POST, "/upgrade/geo").with_timeout(LONG_REQUEST_TIMEOUT),
        )
        .await
    }
}

#[derive(Clone, Debug)]
struct RequestSpec {
    method: Method,
    path: String,
    body: Option<Vec<u8>>,
    timeout: Option<Duration>,
}

impl RequestSpec {
    fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: normalize_path(path.into()),
            body: None,
            timeout: None,
        }
    }

    fn get(path: impl Into<String>) -> Self {
        Self::new(Method::GET, path)
    }

    fn with_json<T>(mut self, value: &T) -> Result<Self>
    where
        T: serde::Serialize,
    {
        self.body = Some(serde_json::to_vec(value)?);
        Ok(self)
    }

    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

fn normalize_path(path: String) -> String {
    if path.starts_with('/') {
        path
    } else {
        format!("/{path}")
    }
}

fn format_host(host: &str) -> String {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(_)) => format!("[{host}]"),
        Ok(IpAddr::V4(_)) | Err(_) => host.to_string(),
    }
}

fn encode(segment: &str) -> String {
    urlencoding::encode(segment).into_owned()
}

fn delay_timeout(timeout_ms: u32) -> Duration {
    Duration::from_millis(u64::from(timeout_ms)).saturating_add(Duration::from_secs(10))
}

fn status_error(status: StatusCode, body: &[u8]) -> Error {
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| {
            let end = body.len().min(MAX_ERROR_MESSAGE_BYTES);
            String::from_utf8_lossy(&body[..end]).trim().to_string()
        });
    let message = if message.is_empty() {
        status
            .canonical_reason()
            .unwrap_or("request failed")
            .to_string()
    } else {
        message
    };

    Error::HttpStatus {
        status: status.as_u16(),
        message,
    }
}

fn is_delay_timeout_error(error: &Error) -> bool {
    match error {
        Error::HttpStatus { status, message } => {
            matches!(*status, 408 | 504) || message.to_ascii_lowercase().contains("timeout")
        }
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::time::Duration;
    #[cfg(unix)]
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::MihomoClient;
    use crate::{ControllerConfig, ControllerSecret, MihomoApi, models::VersionInfo};

    #[tokio::test]
    async fn http_transport_sends_bearer_token_and_decodes_version() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("request should connect");
            let mut request = vec![0_u8; 4096];
            let size = stream
                .read(&mut request)
                .await
                .expect("request should be readable");
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.starts_with("GET /version HTTP/1.1"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer secret-value")
            );
            let body = r#"{"meta":true,"version":"1.20.0"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("response should be writable");
        });
        let client = MihomoClient::new(
            ControllerConfig::http(
                address.ip().to_string(),
                address.port(),
                ControllerSecret::new("secret-value"),
            )
            .with_max_safe_retries(0),
        )
        .expect("client should build");

        let version: VersionInfo = client
            .version()
            .await
            .expect("version request should succeed");
        assert!(version.meta);
        assert_eq!(version.version, "1.20.0");
        server.await.expect("server task should finish");
    }

    #[tokio::test]
    async fn http_error_prefers_structured_mihomo_message() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("request should connect");
            let mut request = [0_u8; 1024];
            let _ = stream
                .read(&mut request)
                .await
                .expect("request should be readable");
            let body = r#"{"message":"bad controller secret"}"#;
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("response should be writable");
        });
        let client = MihomoClient::new(
            ControllerConfig::http(
                address.ip().to_string(),
                address.port(),
                ControllerSecret::default(),
            )
            .with_request_timeout(Duration::from_secs(1))
            .with_max_safe_retries(0),
        )
        .expect("client should build");

        let error = client.version().await.expect_err("request should fail");
        assert!(error.to_string().contains("bad controller secret"));
        server.await.expect("server task should finish");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_transport_uses_the_socket_without_authorization() {
        use tokio::net::UnixListener;

        let socket_path = unique_socket_path();
        let listener = UnixListener::bind(&socket_path).expect("test socket should bind");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("request should connect");
            let mut request = vec![0_u8; 4096];
            let size = stream
                .read(&mut request)
                .await
                .expect("request should be readable");
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.starts_with("GET /version HTTP/1.1"));
            assert!(!request.to_ascii_lowercase().contains("authorization:"));
            let body = r#"{"meta":true,"version":"socket-version"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("response should be writable");
        });
        let client = MihomoClient::new(
            ControllerConfig::local(crate::ControllerEndpoint::unix_socket(&socket_path))
                .with_secret(ControllerSecret::new("must-not-be-sent"))
                .with_max_safe_retries(0),
        )
        .expect("client should build");

        let version = client
            .version()
            .await
            .expect("version request should succeed");
        assert_eq!(version.version, "socket-version");
        server.await.expect("server task should finish");
        std::fs::remove_file(socket_path).expect("test socket should be removable");
    }

    #[cfg(unix)]
    fn unique_socket_path() -> PathBuf {
        static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "rsclash-mihomo-{}-{}.sock",
            std::process::id(),
            NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
