use std::{
  path::{Path, PathBuf},
  sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
  },
  time::Duration,
};

use serde::{Serialize, de::DeserializeOwned};
use tokio::{
  io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
  net::UnixStream,
  time::timeout,
};

use crate::{
  Error, PROTOCOL_VERSION, Request, Response, ResponseData, Result, ServiceCommand, ServiceStatus,
};

const MAX_FRAME_BYTES: usize = 64 * 1024;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
pub struct ServiceClient {
  socket_path: PathBuf,
  request_timeout: Duration,
  next_request_id: Arc<AtomicU64>,
}

impl ServiceClient {
  pub fn new(socket_path: impl Into<PathBuf>) -> Self {
    Self {
      socket_path: socket_path.into(),
      request_timeout: DEFAULT_REQUEST_TIMEOUT,
      next_request_id: Arc::new(AtomicU64::new(1)),
    }
  }

  #[must_use]
  pub const fn with_timeout(mut self, request_timeout: Duration) -> Self {
    self.request_timeout = request_timeout;
    self
  }

  pub fn socket_path(&self) -> &Path {
    &self.socket_path
  }

  pub async fn ping(&self) -> Result<String> {
    match self.request(ServiceCommand::Ping).await? {
      ResponseData::Pong { service_version } => Ok(service_version),
      response => Err(unexpected_response("pong", response)),
    }
  }

  pub async fn status(&self) -> Result<ServiceStatus> {
    match self.request(ServiceCommand::Status).await? {
      ResponseData::Status(status) => Ok(status),
      response => Err(unexpected_response("status", response)),
    }
  }

  pub async fn command(&self, command: ServiceCommand) -> Result<rsclash_domain::CoreState> {
    match self.request(command).await? {
      ResponseData::CoreState(state) => Ok(state),
      response => Err(unexpected_response("core state", response)),
    }
  }

  async fn request(&self, command: ServiceCommand) -> Result<ResponseData> {
    timeout(self.request_timeout, self.request_inner(command))
      .await
      .map_err(|_| Error::TimedOut)?
  }

  async fn request_inner(&self, command: ServiceCommand) -> Result<ResponseData> {
    let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
    let request = Request {
      protocol_version: PROTOCOL_VERSION,
      request_id,
      command,
    };
    let mut stream = UnixStream::connect(&self.socket_path).await?;
    write_frame(&mut stream, &request).await?;
    stream.shutdown().await?;
    let response: Response = read_frame(&mut stream).await?;
    if response.protocol_version != PROTOCOL_VERSION {
      return Err(Error::ProtocolMismatch {
        expected: PROTOCOL_VERSION,
        actual: response.protocol_version,
      });
    }
    if response.request_id != request_id {
      return Err(Error::ResponseMismatch {
        expected: request_id,
        actual: response.request_id,
      });
    }
    response.result.map_err(Error::from)
  }
}

fn unexpected_response(expected: &str, response: ResponseData) -> Error {
  Error::Io(std::io::Error::new(
    std::io::ErrorKind::InvalidData,
    format!("expected {expected} response, received {response:?}"),
  ))
}

pub(crate) async fn read_frame<R, T>(reader: &mut R) -> Result<T>
where
  R: AsyncRead + Send + Unpin,
  T: DeserializeOwned + Send,
{
  let frame_bytes = reader.read_u32().await? as usize;
  if frame_bytes > MAX_FRAME_BYTES {
    return Err(Error::FrameTooLarge { bytes: frame_bytes });
  }
  let mut frame = vec![0; frame_bytes];
  reader.read_exact(&mut frame).await?;
  serde_json::from_slice(&frame).map_err(Error::Decode)
}

pub(crate) async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<()>
where
  W: AsyncWrite + Send + Unpin,
  T: Serialize + Sync,
{
  let frame = serde_json::to_vec(value).map_err(Error::Encode)?;
  if frame.len() > MAX_FRAME_BYTES {
    return Err(Error::FrameTooLarge { bytes: frame.len() });
  }
  let frame_bytes =
    u32::try_from(frame.len()).map_err(|_| Error::FrameTooLarge { bytes: frame.len() })?;
  writer.write_u32(frame_bytes).await?;
  writer.write_all(&frame).await?;
  writer.flush().await.map_err(Error::from)
}
