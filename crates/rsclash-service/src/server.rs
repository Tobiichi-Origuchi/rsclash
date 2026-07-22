use std::{
  future::Future,
  io,
  os::unix::fs::{FileTypeExt as _, PermissionsExt as _},
  path::{Path, PathBuf},
  sync::Arc,
};

use async_trait::async_trait;
use tokio::{
  fs,
  net::{UnixListener, UnixStream},
  sync::Semaphore,
  task::JoinSet,
};

use crate::{
  Error, PROTOCOL_VERSION, RemoteError, RemoteErrorCode, Request, Response, ResponseData, Result,
  ServiceCommand,
  transport::{read_frame, write_frame},
};

const MAX_CONNECTIONS: usize = 16;

#[async_trait]
pub trait ServiceRequestHandler: Send + Sync + 'static {
  async fn handle(&self, command: ServiceCommand)
  -> std::result::Result<ResponseData, RemoteError>;
}

pub struct ServiceServer<H> {
  socket_path: PathBuf,
  allowed_uid: u32,
  handler: Arc<H>,
}

impl<H> ServiceServer<H>
where
  H: ServiceRequestHandler,
{
  pub fn new(socket_path: impl Into<PathBuf>, allowed_uid: u32, handler: Arc<H>) -> Self {
    Self {
      socket_path: socket_path.into(),
      allowed_uid,
      handler,
    }
  }

  pub async fn run_until<F>(self, shutdown: F) -> Result<()>
  where
    F: Future<Output = ()>,
  {
    let listener = bind_private_socket(&self.socket_path).await?;
    let semaphore = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);

    loop {
      tokio::select! {
        () = &mut shutdown => break,
        accepted = listener.accept() => {
          let (stream, _) = accepted?;
          let permit = Arc::clone(&semaphore)
            .acquire_owned()
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;
          let handler = Arc::clone(&self.handler);
          let allowed_uid = self.allowed_uid;
          connections.spawn(async move {
            let _permit = permit;
            let _ = serve_connection(stream, allowed_uid, handler).await;
          });
        },
        result = connections.join_next(), if !connections.is_empty() => {
          if let Some(Err(error)) = result {
            return Err(io::Error::other(error.to_string()).into());
          }
        },
      }
    }

    connections.abort_all();
    while connections.join_next().await.is_some() {}
    drop(listener);
    remove_owned_socket(&self.socket_path).await
  }
}

async fn serve_connection<H>(
  mut stream: UnixStream,
  allowed_uid: u32,
  handler: Arc<H>,
) -> Result<()>
where
  H: ServiceRequestHandler,
{
  let actual_uid = stream.peer_cred()?.uid();
  if actual_uid != allowed_uid {
    return Err(Error::UnauthorizedPeer {
      expected: allowed_uid,
      actual: actual_uid,
    });
  }
  let request: Request = read_frame(&mut stream).await?;
  let result = if request.protocol_version == PROTOCOL_VERSION {
    handler.handle(request.command).await
  } else {
    Err(RemoteError::new(
      RemoteErrorCode::UnsupportedVersion,
      format!(
        "unsupported protocol version {}; expected {PROTOCOL_VERSION}",
        request.protocol_version
      ),
    ))
  };
  write_frame(
    &mut stream,
    &Response {
      protocol_version: PROTOCOL_VERSION,
      request_id: request.request_id,
      result,
    },
  )
  .await
}

async fn bind_private_socket(path: &Path) -> Result<UnixListener> {
  let parent = path
    .parent()
    .ok_or_else(|| Error::UnsafePath(path.to_path_buf()))?;
  fs::create_dir_all(parent).await?;
  let metadata = fs::symlink_metadata(parent).await?;
  if metadata.file_type().is_symlink() || !metadata.is_dir() {
    return Err(Error::UnsafePath(parent.to_path_buf()));
  }
  fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
  remove_stale_socket(path).await?;
  let listener = UnixListener::bind(path)?;
  fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
  Ok(listener)
}

async fn remove_stale_socket(path: &Path) -> Result<()> {
  match fs::symlink_metadata(path).await {
    Ok(metadata) if metadata.file_type().is_socket() => {
      fs::remove_file(path).await.map_err(Into::into)
    },
    Ok(_) => Err(Error::UnsafePath(path.to_path_buf())),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(error.into()),
  }
}

async fn remove_owned_socket(path: &Path) -> Result<()> {
  match fs::symlink_metadata(path).await {
    Ok(metadata) if metadata.file_type().is_socket() => {
      fs::remove_file(path).await.map_err(Into::into)
    },
    Ok(_) => Err(Error::UnsafePath(path.to_path_buf())),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(error.into()),
  }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::{
    fs,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    sync::{
      Arc,
      atomic::{AtomicU64, Ordering},
    },
    time::Duration,
  };

  use async_trait::async_trait;
  use rsclash_domain::CoreState;
  use tokio::sync::oneshot;

  use super::{ServiceRequestHandler, ServiceServer};
  use crate::{Error, RemoteError, ResponseData, ServiceClient, ServiceCommand, ServiceStatus};

  struct FakeHandler;

  #[async_trait]
  impl ServiceRequestHandler for FakeHandler {
    async fn handle(&self, command: ServiceCommand) -> Result<ResponseData, RemoteError> {
      match command {
        ServiceCommand::Ping => Ok(ResponseData::Pong {
          service_version: "test".to_string(),
        }),
        ServiceCommand::Status => Ok(ResponseData::Status(ServiceStatus {
          service_version: "test".to_string(),
          core: CoreState::Stopped,
        })),
        _ => Ok(ResponseData::CoreState(CoreState::Stopped)),
      }
    }
  }

  #[tokio::test]
  async fn authenticates_the_peer_and_cleans_up_the_socket() {
    let directory = TestDirectory::new();
    let socket = directory.path().join("service.sock");
    let uid = fs::metadata("/proc/self")
      .expect("process metadata should exist")
      .uid();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = ServiceServer::new(&socket, uid, Arc::new(FakeHandler));
    let server_task = tokio::spawn(server.run_until(async move {
      let _ = shutdown_rx.await;
    }));
    wait_for_socket(&socket).await;

    let client = ServiceClient::new(&socket);
    assert_eq!(client.ping().await.ok().as_deref(), Some("test"));
    assert_eq!(
      client.status().await.expect("status should succeed").core,
      CoreState::Stopped
    );
    assert_eq!(
      fs::metadata(&socket)
        .expect("socket metadata should exist")
        .permissions()
        .mode()
        & 0o777,
      0o600
    );

    let _ = shutdown_tx.send(());
    assert!(
      server_task
        .await
        .expect("server task should finish")
        .is_ok()
    );
    assert!(!socket.exists());
  }

  #[tokio::test]
  async fn rejects_a_different_peer_uid() {
    let directory = TestDirectory::new();
    let socket = directory.path().join("service.sock");
    let uid = fs::metadata("/proc/self")
      .expect("process metadata should exist")
      .uid();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = ServiceServer::new(&socket, uid.saturating_add(1), Arc::new(FakeHandler));
    let server_task = tokio::spawn(server.run_until(async move {
      let _ = shutdown_rx.await;
    }));
    wait_for_socket(&socket).await;

    let error = ServiceClient::new(&socket)
      .ping()
      .await
      .expect_err("a different UID should be rejected");
    assert!(matches!(error, Error::Io(_)));

    let _ = shutdown_tx.send(());
    assert!(
      server_task
        .await
        .expect("server task should finish")
        .is_ok()
    );
  }

  async fn wait_for_socket(path: &Path) {
    tokio::time::timeout(Duration::from_secs(1), async {
      while !path.exists() {
        tokio::time::sleep(Duration::from_millis(5)).await;
      }
    })
    .await
    .expect("socket should appear");
  }

  struct TestDirectory(PathBuf);

  impl TestDirectory {
    fn new() -> Self {
      static NEXT_ID: AtomicU64 = AtomicU64::new(0);
      let path = std::env::temp_dir().join(format!(
        "rsclash-service-test-{}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
      ));
      fs::create_dir_all(&path).expect("test directory should be created");
      Self(path)
    }

    fn path(&self) -> &Path {
      &self.0
    }
  }

  impl Drop for TestDirectory {
    fn drop(&mut self) {
      let _ = fs::remove_dir_all(&self.0);
    }
  }
}
