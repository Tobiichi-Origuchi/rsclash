use std::{
  env, fs,
  io::{self, Read as _, Write as _},
  os::unix::{
    ffi::{OsStrExt as _, OsStringExt as _},
    fs::{FileTypeExt as _, PermissionsExt as _},
    net::{UnixListener, UnixStream},
  },
  path::{Path, PathBuf},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  thread::{self, JoinHandle},
  time::Duration,
};

use rsclash_app::AppClient;
use tracing::{debug, warn};

const MAGIC: &[u8; 4] = b"RSC1";
const MAX_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LaunchRequest {
  pub show_window: bool,
  pub arguments: Vec<std::ffi::OsString>,
}

impl LaunchRequest {
  pub(crate) fn from_environment() -> Self {
    let mut show_window = true;
    let arguments = env::args_os()
      .skip(1)
      .filter(|argument| {
        if argument == "--silent" {
          show_window = false;
          false
        } else {
          true
        }
      })
      .collect();
    Self {
      show_window,
      arguments,
    }
  }
}

pub(crate) enum Instance {
  Primary(PrimaryInstance),
  Forwarded,
}

pub(crate) struct PrimaryInstance {
  listener: UnixListener,
  socket_path: PathBuf,
}

pub(crate) struct InstanceHandle {
  stop: Arc<AtomicBool>,
  thread: Option<JoinHandle<()>>,
  socket_path: PathBuf,
}

impl PrimaryInstance {
  pub(crate) fn acquire(request: &LaunchRequest) -> Result<Instance, String> {
    let socket_path = socket_path()?;
    let parent = socket_path
      .parent()
      .ok_or_else(|| "single-instance socket has no parent directory".to_string())?;
    fs::create_dir_all(parent)
      .map_err(|error| format!("create single-instance directory: {error}"))?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
      .map_err(|error| format!("restrict single-instance directory: {error}"))?;

    match UnixListener::bind(&socket_path) {
      Ok(listener) => {
        listener
          .set_nonblocking(true)
          .map_err(|error| format!("configure single-instance listener: {error}"))?;
        Ok(Instance::Primary(Self {
          listener,
          socket_path,
        }))
      },
      Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
        match forward(&socket_path, request) {
          Ok(()) => Ok(Instance::Forwarded),
          Err(forward_error) => {
            reject_non_socket(&socket_path)?;
            fs::remove_file(&socket_path).map_err(|remove_error| {
              format!(
                "existing instance is unreachable ({forward_error}); remove stale socket: \
                 {remove_error}"
              )
            })?;
            let listener = UnixListener::bind(&socket_path)
              .map_err(|bind_error| format!("replace stale instance socket: {bind_error}"))?;
            listener
              .set_nonblocking(true)
              .map_err(|bind_error| format!("configure single-instance listener: {bind_error}"))?;
            Ok(Instance::Primary(Self {
              listener,
              socket_path,
            }))
          },
        }
      },
      Err(error) => Err(format!("bind single-instance socket: {error}")),
    }
  }

  pub(crate) fn listen(
    self,
    client: AppClient,
    dispatch: fn(&AppClient, LaunchRequest),
  ) -> Result<InstanceHandle, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let socket_path = self.socket_path;
    let thread = thread::Builder::new()
      .name("rsclash-instance".to_string())
      .spawn(move || listener_loop(&self.listener, &thread_stop, &client, dispatch))
      .map_err(|error| format!("spawn single-instance listener: {error}"))?;
    Ok(InstanceHandle {
      stop,
      thread: Some(thread),
      socket_path,
    })
  }
}

fn listener_loop(
  listener: &UnixListener,
  stop: &AtomicBool,
  client: &AppClient,
  dispatch: fn(&AppClient, LaunchRequest),
) {
  while !stop.load(Ordering::Acquire) {
    match listener.accept() {
      Ok((mut stream, _)) => handle_stream(&mut stream, client, dispatch),
      Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
        thread::sleep(Duration::from_millis(50));
      },
      Err(error) => {
        warn!(%error, "single-instance listener failed");
        break;
      },
    }
  }
}

fn handle_stream(
  stream: &mut UnixStream,
  client: &AppClient,
  dispatch: fn(&AppClient, LaunchRequest),
) {
  match read_request(stream) {
    Ok(request) => {
      debug!("received a second-instance launch request");
      dispatch(client, request);
    },
    Err(error) => warn!(%error, "rejected a second-instance launch request"),
  }
}

impl Drop for InstanceHandle {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Release);
    if let Some(thread) = self.thread.take() {
      let _ = thread.join();
    }
    if self.socket_path.exists() {
      let _ = fs::remove_file(&self.socket_path);
    }
  }
}

fn socket_path() -> Result<PathBuf, String> {
  if let Some(root) = env::var_os("XDG_RUNTIME_DIR")
    .filter(|value| !value.is_empty())
    .map(PathBuf::from)
    .filter(|path| path.is_absolute())
  {
    return Ok(root.join("rsclash/instance.sock"));
  }
  env::var_os("HOME")
    .filter(|value| !value.is_empty())
    .map(PathBuf::from)
    .map(|home| home.join(".cache/rsclash/instance.sock"))
    .ok_or_else(|| "neither XDG_RUNTIME_DIR nor HOME is available".to_string())
}

fn forward(path: &Path, request: &LaunchRequest) -> Result<(), String> {
  let mut stream =
    UnixStream::connect(path).map_err(|error| format!("connect to existing instance: {error}"))?;
  stream
    .write_all(&encode_request(request)?)
    .map_err(|error| format!("forward launch request: {error}"))
}

fn encode_request(request: &LaunchRequest) -> Result<Vec<u8>, String> {
  let count =
    u32::try_from(request.arguments.len()).map_err(|_| "too many launch arguments".to_string())?;
  let mut bytes = Vec::from(MAGIC);
  bytes.push(u8::from(request.show_window));
  bytes.extend_from_slice(&count.to_be_bytes());
  for argument in &request.arguments {
    let argument = argument.as_os_str().as_bytes();
    let length =
      u32::try_from(argument.len()).map_err(|_| "launch argument is too long".to_string())?;
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(argument);
  }
  if bytes.len() > MAX_REQUEST_BYTES {
    return Err("launch request exceeds the 1 MiB limit".to_string());
  }
  Ok(bytes)
}

fn read_request(stream: &mut UnixStream) -> Result<LaunchRequest, String> {
  let mut bytes = Vec::new();
  stream
    .take((MAX_REQUEST_BYTES + 1) as u64)
    .read_to_end(&mut bytes)
    .map_err(|error| format!("read launch request: {error}"))?;
  decode_request(&bytes)
}

fn decode_request(bytes: &[u8]) -> Result<LaunchRequest, String> {
  if bytes.len() < 9 || &bytes[..4] != MAGIC {
    return Err("invalid launch request header".to_string());
  }
  let show_window = match bytes[4] {
    0 => false,
    1 => true,
    _ => return Err("invalid launch request visibility".to_string()),
  };
  let count = u32::from_be_bytes(
    bytes[5..9]
      .try_into()
      .map_err(|_| "invalid launch request count".to_string())?,
  ) as usize;
  let mut cursor = 9;
  let mut arguments = Vec::with_capacity(count.min(64));
  for _ in 0..count {
    let length_bytes = bytes
      .get(cursor..cursor + 4)
      .ok_or_else(|| "truncated launch argument length".to_string())?;
    let length = u32::from_be_bytes(
      length_bytes
        .try_into()
        .map_err(|_| "invalid launch argument length".to_string())?,
    ) as usize;
    cursor += 4;
    let argument = bytes
      .get(cursor..cursor + length)
      .ok_or_else(|| "truncated launch argument".to_string())?;
    cursor += length;
    arguments.push(std::ffi::OsString::from_vec(argument.to_vec()));
  }
  if cursor != bytes.len() {
    return Err("launch request has trailing data".to_string());
  }
  Ok(LaunchRequest {
    show_window,
    arguments,
  })
}

fn reject_non_socket(path: &Path) -> Result<(), String> {
  let metadata =
    fs::symlink_metadata(path).map_err(|error| format!("inspect instance socket: {error}"))?;
  if metadata.file_type().is_socket() {
    Ok(())
  } else {
    Err(format!(
      "refusing to replace a non-socket instance path: {}",
      path.display()
    ))
  }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::ffi::OsString;

  use super::{LaunchRequest, decode_request, encode_request};

  #[test]
  fn launch_protocol_round_trips_non_utf8_arguments() {
    let request = LaunchRequest {
      show_window: false,
      arguments: vec![
        OsString::from("clash://install-config?url=https%3A%2F%2Fexample.test"),
        std::os::unix::ffi::OsStringExt::from_vec(vec![b'/', b't', b'm', b'p', 0xff]),
      ],
    };
    let encoded = encode_request(&request).expect("request should encode");

    assert_eq!(decode_request(&encoded), Ok(request));
  }
}
