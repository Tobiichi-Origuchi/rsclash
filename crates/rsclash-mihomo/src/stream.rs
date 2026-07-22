use std::{fmt, time::Duration};

use futures_util::{StreamExt, stream};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{
    WebSocketStream, client_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tracing::warn;

use crate::{ControllerEndpoint, Error, MihomoClient, MihomoStream, Result, models::LogLevel};

const RECONNECT_BASE_DELAY: Duration = Duration::from_millis(100);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(5);

trait SocketIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> SocketIo for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

type BoxedSocket = Box<dyn SocketIo>;
type SocketWebSocket = WebSocketStream<BoxedSocket>;

#[derive(Clone, Copy, Debug)]
pub(crate) enum StreamKind {
    Traffic,
    Memory,
    Connections,
    Logs(LogLevel),
}

impl StreamKind {
    fn path(self) -> String {
        match self {
            Self::Traffic => "/traffic".to_string(),
            Self::Memory => "/memory".to_string(),
            Self::Connections => "/connections".to_string(),
            Self::Logs(level) => format!("/logs?level={}", level.as_str()),
        }
    }

    const fn context(self) -> &'static str {
        match self {
            Self::Traffic => "traffic stream",
            Self::Memory => "memory stream",
            Self::Connections => "connections stream",
            Self::Logs(_) => "logs stream",
        }
    }
}

struct ReconnectState {
    client: MihomoClient,
    kind: StreamKind,
    socket: Option<SocketWebSocket>,
    reconnect_delay: Duration,
}

impl MihomoClient {
    pub(crate) async fn typed_stream<T>(&self, kind: StreamKind) -> Result<MihomoStream<T>>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let socket = self.connect_websocket(kind).await?;
        let state = ReconnectState {
            client: self.clone(),
            kind,
            socket: Some(socket),
            reconnect_delay: RECONNECT_BASE_DELAY,
        };

        Ok(Box::pin(stream::unfold(state, |mut state| async move {
            loop {
                if state.socket.is_none() {
                    let delay = state.reconnect_delay;
                    tokio::time::sleep(delay).await;
                    match state.client.connect_websocket(state.kind).await {
                        Ok(socket) => {
                            state.socket = Some(socket);
                            state.reconnect_delay = RECONNECT_BASE_DELAY;
                        }
                        Err(error) => {
                            state.reconnect_delay = next_reconnect_delay(delay);
                            warn!(?delay, "Mihomo stream reconnect failed");
                            return Some((Err(error), state));
                        }
                    }
                }

                let message = match state.socket.as_mut() {
                    Some(socket) => socket.next().await,
                    None => continue,
                };
                match message {
                    Some(Ok(Message::Text(text))) => {
                        let item = decode_stream_item(state.kind.context(), text.as_bytes());
                        return Some((item, state));
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        let item = decode_stream_item(state.kind.context(), &bytes);
                        return Some((item, state));
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        state.socket = None;
                        return Some((
                            Err(Error::WebSocket("controller closed the stream".to_string())),
                            state,
                        ));
                    }
                    Some(Err(error)) => {
                        state.socket = None;
                        return Some((Err(Error::WebSocket(error.to_string())), state));
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                }
            }
        })))
    }

    async fn connect_websocket(&self, kind: StreamKind) -> Result<SocketWebSocket> {
        let config = self.controller_config();
        let request_path = kind.path();
        let (url, socket): (String, BoxedSocket) = match &config.endpoint {
            ControllerEndpoint::Http { host, port } => {
                let stream = tokio::time::timeout(
                    config.request_timeout,
                    tokio::net::TcpStream::connect((host.as_str(), *port)),
                )
                .await
                .map_err(|_| Error::Timeout(config.request_timeout))?
                .map_err(|error| Error::WebSocket(error.to_string()))?;
                let host = websocket_host(host);
                let separator = if request_path.contains('?') { '&' } else { '?' };
                let url = if config.secret.is_empty() {
                    format!("ws://{host}:{port}{request_path}")
                } else {
                    let token = urlencoding::encode(config.secret.expose());
                    format!("ws://{host}:{port}{request_path}{separator}token={token}")
                };
                (url, Box::new(stream))
            }
            ControllerEndpoint::UnixSocket(socket_path) => {
                #[cfg(unix)]
                {
                    let stream = tokio::time::timeout(
                        config.request_timeout,
                        tokio::net::UnixStream::connect(socket_path),
                    )
                    .await
                    .map_err(|_| Error::Timeout(config.request_timeout))?
                    .map_err(|error| Error::WebSocket(error.to_string()))?;
                    (format!("ws://localhost{request_path}"), Box::new(stream))
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

        let request = url.into_client_request().map_err(|_| {
            Error::InvalidConfiguration("failed to build a WebSocket request".to_string())
        })?;
        let (socket, _) =
            tokio::time::timeout(config.request_timeout, client_async(request, socket))
                .await
                .map_err(|_| Error::Timeout(config.request_timeout))?
                .map_err(|error| Error::WebSocket(error.to_string()))?;
        Ok(socket)
    }
}

impl fmt::Debug for ReconnectState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReconnectState")
            .field("kind", &self.kind)
            .field("connected", &self.socket.is_some())
            .field("reconnect_delay", &self.reconnect_delay)
            .finish_non_exhaustive()
    }
}

fn decode_stream_item<T>(context: &'static str, payload: &[u8]) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_slice(payload).map_err(|source| Error::Decode { context, source })
}

fn next_reconnect_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(RECONNECT_MAX_DELAY)
}

fn websocket_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::result_large_err)]
mod tests {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::{
        accept_async, accept_hdr_async,
        tungstenite::{
            Message,
            handshake::server::{Request, Response},
        },
    };

    use crate::{ControllerConfig, ControllerEndpoint, ControllerSecret, MihomoApi, MihomoClient};

    #[tokio::test]
    async fn tcp_stream_authenticates_and_reconnects_after_close() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let server = tokio::spawn(async move {
            for up in [1_u64, 2_u64] {
                let (socket, _) = listener.accept().await.expect("stream should connect");
                let mut websocket =
                    accept_hdr_async(socket, |request: &Request, response: Response| {
                        let uri = request.uri().to_string();
                        assert!(uri.starts_with("/traffic?token="));
                        assert!(uri.contains("a%20b%26c"));
                        Ok(response)
                    })
                    .await
                    .expect("handshake should succeed");
                websocket
                    .send(Message::Text(format!(r#"{{"up":{up},"down":3}}"#).into()))
                    .await
                    .expect("stream item should send");
                websocket.close(None).await.expect("stream should close");
            }
        });
        let client = MihomoClient::new(
            ControllerConfig::http(
                address.ip().to_string(),
                address.port(),
                ControllerSecret::new("a b&c"),
            )
            .with_request_timeout(Duration::from_secs(1)),
        )
        .expect("client should build");
        let mut stream = client
            .traffic_stream()
            .await
            .expect("stream should connect");

        let first = next_with_timeout(&mut stream)
            .await
            .expect("first item should decode");
        assert_eq!(first.up, 1);
        let closed = next_with_timeout(&mut stream).await;
        assert!(closed.is_err());
        let second = next_with_timeout(&mut stream)
            .await
            .expect("reconnected item should decode");
        assert_eq!(second.up, 2);
        server.await.expect("server should finish");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_memory_stream_uses_local_socket() {
        use tokio::net::UnixListener;

        let socket_path = unique_socket_path();
        let listener = UnixListener::bind(&socket_path).expect("test socket should bind");
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("stream should connect");
            let mut websocket = accept_async(socket)
                .await
                .expect("handshake should succeed");
            websocket
                .send(Message::Text(r#"{"inuse":4096,"oslimit":8192}"#.into()))
                .await
                .expect("stream item should send");
        });
        let client = MihomoClient::new(ControllerConfig::local(ControllerEndpoint::unix_socket(
            &socket_path,
        )))
        .expect("client should build");
        let mut stream = client.memory_stream().await.expect("stream should connect");

        let memory = next_with_timeout(&mut stream)
            .await
            .expect("memory should decode");
        assert_eq!(memory.inuse, 4096);
        assert_eq!(memory.oslimit, 8192);
        server.await.expect("server should finish");
        std::fs::remove_file(socket_path).expect("test socket should be removable");
    }

    async fn next_with_timeout<T>(stream: &mut crate::MihomoStream<T>) -> crate::Result<T> {
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("stream item should arrive")
            .expect("stream should remain active")
    }

    #[cfg(unix)]
    fn unique_socket_path() -> PathBuf {
        static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "rsclash-ws-{}-{}.sock",
            std::process::id(),
            NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
