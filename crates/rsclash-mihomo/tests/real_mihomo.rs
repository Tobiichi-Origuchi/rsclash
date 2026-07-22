#![cfg(unix)]

use std::{
  ffi::OsString,
  fs,
  os::unix::fs::PermissionsExt,
  path::PathBuf,
  process::{Child, Command, Stdio},
  sync::atomic::{AtomicU64, Ordering},
  time::Duration,
};

use futures_util::StreamExt;
use rsclash_mihomo::{
  ControllerConfig, ControllerEndpoint, ControllerSecret, MihomoApi, MihomoClient, models::LogLevel,
};

const CONFIG: &str = include_str!("fixtures/minimal-config.yaml");

#[tokio::test]
#[ignore = "requires a pinned Mihomo binary through RSCLASH_MIHOMO_BIN"]
#[allow(clippy::expect_used)]
async fn pinned_mihomo_supports_rest_and_all_streams() {
  let binary = std::env::var_os("RSCLASH_MIHOMO_BIN")
    .expect("RSCLASH_MIHOMO_BIN must point to the pinned Mihomo binary");
  let mut core = TestCore::start(binary);
  let client = MihomoClient::new(
    ControllerConfig::local(ControllerEndpoint::unix_socket(&core.socket_path))
      .with_secret(ControllerSecret::new("local-secret-must-not-be-required"))
      .with_request_timeout(Duration::from_secs(3)),
  )
  .expect("client should build");
  let version = wait_until_ready(&client, &mut core).await;

  assert!(version.meta);
  assert!(!version.version.is_empty());
  let config = client.base_config().await.expect("config should load");
  assert_eq!(config.mode, "rule");
  let proxies = client.proxies().await.expect("proxies should load");
  assert!(proxies.proxies.contains_key("GLOBAL"));
  let rules = client.rules().await.expect("rules should load");
  assert_eq!(rules.rules.len(), 1);

  client
    .select_proxy("GLOBAL", "REJECT")
    .await
    .expect("proxy should switch");
  let group = client.group("GLOBAL").await.expect("group should load");
  assert_eq!(group.now.as_deref(), Some("REJECT"));

  let mut traffic = client
    .traffic_stream()
    .await
    .expect("traffic stream should connect");
  let traffic_item = next_item(&mut traffic).await;
  assert!(traffic_item.up_total >= traffic_item.up);

  let mut memory = client
    .memory_stream()
    .await
    .expect("memory stream should connect");
  let memory_item = next_item(&mut memory).await;
  assert!(memory_item.oslimit >= memory_item.inuse);

  let mut connections = client
    .connections_stream()
    .await
    .expect("connections stream should connect");
  let _ = next_item(&mut connections).await;

  let mut logs = client
    .logs_stream(LogLevel::Debug)
    .await
    .expect("logs stream should connect");
  generate_proxy_log(core.mixed_port).await;
  let log_item = next_item(&mut logs).await;
  assert!(!log_item.level.is_empty());

  drop((traffic, memory, connections, logs));
  client
    .reload_config(core.config_path.to_string_lossy().as_ref(), false)
    .await
    .expect("config should reload");
  let reloaded = wait_until_ready(&client, &mut core).await;
  assert_eq!(reloaded.version, version.version);
}

#[allow(clippy::expect_used)]
async fn wait_until_ready(
  client: &MihomoClient,
  core: &mut TestCore,
) -> rsclash_mihomo::models::VersionInfo {
  for _ in 0..100 {
    if let Some(status) = core
      .child
      .try_wait()
      .expect("core status should be readable")
    {
      panic!("Mihomo exited before it became ready: {status}");
    }
    if let Ok(version) = client.health_check().await {
      return version;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  panic!("Mihomo did not become ready before the integration timeout");
}

#[allow(clippy::expect_used)]
async fn next_item<T>(stream: &mut rsclash_mihomo::MihomoStream<T>) -> T {
  tokio::time::timeout(Duration::from_secs(5), stream.next())
    .await
    .expect("stream item should arrive before the timeout")
    .expect("stream should remain active")
    .expect("stream item should decode")
}

struct TestCore {
  child: Child,
  directory: PathBuf,
  config_path: PathBuf,
  socket_path: PathBuf,
  mixed_port: u16,
}

impl TestCore {
  #[allow(clippy::expect_used)]
  fn start(binary: OsString) -> Self {
    let directory = unique_test_directory();
    fs::create_dir_all(&directory).expect("test directory should be created");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
      .expect("test directory permissions should be restricted");
    let config_path = directory.join("config.yaml");
    let socket_path = directory.join("controller.sock");
    let mixed_port = reserve_tcp_port();
    let config = CONFIG.replace("mixed-port: 0", &format!("mixed-port: {mixed_port}"));
    fs::write(&config_path, config).expect("test config should be written");
    let child = Command::new(binary)
      .arg("-d")
      .arg(&directory)
      .arg("-f")
      .arg(&config_path)
      .arg("-ext-ctl-unix")
      .arg(&socket_path)
      .arg("-secret")
      .arg("integration-secret")
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .expect("Mihomo should start");

    Self {
      child,
      directory,
      config_path,
      socket_path,
      mixed_port,
    }
  }
}

impl Drop for TestCore {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
    let _ = fs::remove_dir_all(&self.directory);
  }
}

fn unique_test_directory() -> PathBuf {
  static NEXT_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);
  std::env::temp_dir().join(format!(
    "rsclash-real-mihomo-{}-{}",
    std::process::id(),
    NEXT_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed)
  ))
}

#[allow(clippy::expect_used)]
fn reserve_tcp_port() -> u16 {
  let listener =
    std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback test port should be available");
  listener
    .local_addr()
    .expect("test port should have an address")
    .port()
}

#[allow(clippy::expect_used)]
async fn generate_proxy_log(port: u16) {
  use tokio::io::AsyncWriteExt;

  let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
    .await
    .expect("test proxy should accept a connection");
  stream
    .write_all(b"GET http://example.invalid/ HTTP/1.1\r\nHost: example.invalid\r\n\r\n")
    .await
    .expect("test proxy request should be writable");
}
