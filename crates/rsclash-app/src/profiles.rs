use std::{
  fs,
  path::{Path, PathBuf},
  sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
  },
  time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::{Client, Url, redirect::Policy};
use rsclash_config::{
  ApplicationLayer, BoaScriptExecutor, EnhancementInput, EnhancementPipeline, ListenerPolicy,
  MihomoConfig, ProfileItem, ProfileKind, ProfileStore, RuntimeActivator, RuntimeDeployer,
  RuntimeStore, RuntimeValidator, TargetPlatform, extract_control_plane,
};
use rsclash_domain::{ProfileSourceKind, ProfileSummary, ProfilesSnapshot};
use serde_yaml_ng::Value;
use tokio::{sync::mpsc, task::spawn_blocking};

const MAX_PROFILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROFILE_NAME_CHARS: usize = 128;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct ProfileAccess {
  store: ProfileStore,
  validator: Arc<dyn RuntimeValidator>,
  http: Client,
}

impl ProfileAccess {
  pub fn new(store: ProfileStore, validator: Arc<dyn RuntimeValidator>) -> Result<Self, String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = Client::builder()
      .connect_timeout(Duration::from_secs(10))
      .timeout(DOWNLOAD_TIMEOUT)
      .redirect(Policy::limited(5))
      .user_agent(concat!("rsclash/", env!("CARGO_PKG_VERSION")))
      .build()
      .map_err(|error| format!("build the subscription HTTP client: {error}"))?;
    Ok(Self {
      store,
      validator,
      http,
    })
  }
}

#[derive(Clone, Debug)]
pub(crate) enum ProfileBridgeCommand {
  Refresh,
  ImportLocal { name: String, path: String },
  ImportRemote { name: String, url: String },
  Activate { uid: String },
}

pub(crate) enum ProfileBridgeEvent {
  Snapshot(ProfilesSnapshot),
  CommandFailed(String),
}

struct ProfileWorker {
  access: ProfileAccess,
  activator: Arc<dyn RuntimeActivator>,
  snapshot: ProfilesSnapshot,
  event_tx: mpsc::Sender<ProfileBridgeEvent>,
}

impl ProfileWorker {
  fn new(
    access: ProfileAccess,
    activator: Arc<dyn RuntimeActivator>,
    event_tx: mpsc::Sender<ProfileBridgeEvent>,
  ) -> Self {
    Self {
      access,
      activator,
      snapshot: ProfilesSnapshot::default(),
      event_tx,
    }
  }

  async fn run(mut self, mut command_rx: mpsc::Receiver<ProfileBridgeCommand>) {
    self.refresh().await;
    while let Some(command) = command_rx.recv().await {
      match command {
        ProfileBridgeCommand::Refresh => self.refresh().await,
        ProfileBridgeCommand::ImportLocal { name, path } => {
          self.set_busy(true).await;
          let store = self.access.store.clone();
          let result = spawn_blocking(move || import_local(&store, &name, Path::new(&path)))
            .await
            .map_err(|error| format!("local profile import task failed: {error}"))
            .and_then(|result| result);
          self.finish_operation(result).await;
        },
        ProfileBridgeCommand::ImportRemote { name, url } => {
          self.set_busy(true).await;
          let result = self.import_remote(name, url).await;
          self.finish_operation(result).await;
        },
        ProfileBridgeCommand::Activate { uid } => {
          self.set_busy(true).await;
          let result = self.activate(uid).await;
          self.finish_operation(result).await;
        },
      }
    }
  }

  async fn import_remote(&self, name: String, url: String) -> Result<(), String> {
    validate_profile_name(&name)?;
    let parsed = Url::parse(&url).map_err(|error| format!("invalid subscription URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
      return Err("subscription URL must use HTTP or HTTPS".to_string());
    }
    let mut response = self
      .access
      .http
      .get(parsed)
      .send()
      .await
      .map_err(|error| format!("download subscription: {}", error.without_url()))?
      .error_for_status()
      .map_err(|error| format!("download subscription: {}", error.without_url()))?;
    if response
      .content_length()
      .is_some_and(|length| length > MAX_PROFILE_BYTES as u64)
    {
      return Err(format!(
        "subscription exceeds the {} MiB limit",
        MAX_PROFILE_BYTES / 1024 / 1024
      ));
    }
    let mut content = Vec::new();
    while let Some(chunk) = response
      .chunk()
      .await
      .map_err(|error| format!("read subscription response: {}", error.without_url()))?
    {
      if content.len().saturating_add(chunk.len()) > MAX_PROFILE_BYTES {
        return Err(format!(
          "subscription exceeds the {} MiB limit",
          MAX_PROFILE_BYTES / 1024 / 1024
        ));
      }
      content.extend_from_slice(&chunk);
    }
    let store = self.access.store.clone();
    spawn_blocking(move || import_content(&store, &name, ProfileKind::Remote, Some(url), content))
      .await
      .map_err(|error| format!("remote profile import task failed: {error}"))?
  }

  async fn activate(&self, uid: String) -> Result<(), String> {
    let store = self.access.store.clone();
    let prepared = spawn_blocking(move || prepare_activation(&store, &uid))
      .await
      .map_err(|error| format!("profile preparation task failed: {error}"))??;
    let runtime_store =
      RuntimeStore::open(&prepared.runtime_path).map_err(|error| error.to_string())?;
    let deployer = RuntimeDeployer::new(
      &runtime_store,
      self.access.validator.as_ref(),
      self.activator.as_ref(),
    );
    deployer
      .deploy(&prepared.next_runtime)
      .await
      .map_err(|error| format!("activate profile runtime: {error}"))?;

    let store = self.access.store.clone();
    let uid = prepared.uid.clone();
    let catalog_result = spawn_blocking(move || set_current_profile(&store, &uid))
      .await
      .map_err(|error| format!("profile catalog task failed: {error}"))?;
    if let Err(catalog_error) = catalog_result {
      let rollback = deployer.deploy(&prepared.previous_runtime).await;
      return match rollback {
        Ok(_) => Err(format!("save active profile: {catalog_error}")),
        Err(rollback_error) => Err(format!(
          "save active profile: {catalog_error}; restore previous runtime: {rollback_error}"
        )),
      };
    }
    Ok(())
  }

  async fn refresh(&mut self) {
    let store = self.access.store.clone();
    let result = spawn_blocking(move || load_snapshot(&store))
      .await
      .map_err(|error| format!("profile refresh task failed: {error}"))
      .and_then(|result| result);
    match result {
      Ok(snapshot) => {
        self.snapshot = snapshot;
        self.publish().await;
      },
      Err(error) => self.fail(error).await,
    }
  }

  async fn finish_operation(&mut self, result: Result<(), String>) {
    match result {
      Ok(()) => self.refresh().await,
      Err(error) => {
        self.set_busy(false).await;
        self.fail(error).await;
      },
    }
  }

  async fn set_busy(&mut self, busy: bool) {
    if self.snapshot.busy != busy {
      self.snapshot.busy = busy;
      self.publish().await;
    }
  }

  async fn fail(&self, error: String) {
    let _ = self
      .event_tx
      .send(ProfileBridgeEvent::CommandFailed(error))
      .await;
  }

  async fn publish(&self) {
    let _ = self
      .event_tx
      .send(ProfileBridgeEvent::Snapshot(self.snapshot.clone()))
      .await;
  }
}

struct PreparedActivation {
  uid: String,
  runtime_path: PathBuf,
  previous_runtime: MihomoConfig,
  next_runtime: MihomoConfig,
}

fn import_local(store: &ProfileStore, name: &str, source: &Path) -> Result<(), String> {
  validate_profile_name(name)?;
  let metadata = fs::symlink_metadata(source)
    .map_err(|error| format!("inspect local profile {}: {error}", source.display()))?;
  if metadata.file_type().is_symlink() || !metadata.is_file() {
    return Err("local profile must be a regular, non-symlink file".to_string());
  }
  if metadata.len() > MAX_PROFILE_BYTES as u64 {
    return Err(format!(
      "local profile exceeds the {} MiB limit",
      MAX_PROFILE_BYTES / 1024 / 1024
    ));
  }
  let content = fs::read(source)
    .map_err(|error| format!("read local profile {}: {error}", source.display()))?;
  import_content(store, name, ProfileKind::Local, None, content)
}

fn import_content(
  store: &ProfileStore,
  name: &str,
  kind: ProfileKind,
  url: Option<String>,
  content: Vec<u8>,
) -> Result<(), String> {
  validate_profile_name(name)?;
  let source =
    std::str::from_utf8(&content).map_err(|_| "profile must be valid UTF-8 YAML".to_string())?;
  let config =
    MihomoConfig::parse(source).map_err(|error| format!("parse profile YAML: {error}"))?;
  if config.mapping().is_empty() {
    return Err("profile YAML must not be empty".to_string());
  }
  let uid = unique_profile_uid();
  let item = ProfileItem {
    uid: Some(uid.clone()),
    kind: Some(kind),
    name: Some(name.trim().to_string()),
    file: Some(format!("{uid}.yaml")),
    url,
    updated: Some(unix_seconds()),
    ..ProfileItem::default()
  };
  let mut transaction = store.begin().map_err(|error| error.to_string())?;
  transaction
    .add_profile(item, content)
    .map_err(|error| error.to_string())?;
  transaction.validate().map_err(|error| error.to_string())?;
  transaction.commit().map_err(|error| error.to_string())?;
  Ok(())
}

fn prepare_activation(store: &ProfileStore, uid: &str) -> Result<PreparedActivation, String> {
  let catalog = store.load_catalog().map_err(|error| error.to_string())?;
  let item = catalog
    .get(uid)
    .ok_or_else(|| format!("profile {uid} does not exist"))?;
  if !item.is_source() {
    return Err(format!("profile {uid} is not a local or remote source"));
  }
  let profile = store
    .read_profile(item.require_file().map_err(|error| error.to_string())?)
    .map_err(|error| error.to_string())?;
  let current = MihomoConfig::parse(&profile).map_err(|error| error.to_string())?;
  let runtime_path = store.paths().runtime_config.clone();
  let previous_source = fs::read_to_string(&runtime_path)
    .map_err(|error| format!("read current runtime {}: {error}", runtime_path.display()))?;
  let previous_runtime =
    MihomoConfig::parse(&previous_source).map_err(|error| error.to_string())?;
  let defaults = extract_control_plane(&previous_runtime);
  let listeners = ListenerPolicy {
    socks: defaults.contains_key("socks-port"),
    http: defaults.contains_key("port"),
    redir: defaults.contains_key("redir-port"),
    tproxy: defaults.contains_key("tproxy-port"),
    external_controller: defaults.contains_key("external-controller"),
  };
  let enable_tun = defaults
    .get("tun")
    .and_then(Value::as_mapping)
    .and_then(|tun| tun.get("enable"))
    .and_then(Value::as_bool)
    .unwrap_or(false);
  let runtime = EnhancementPipeline::new(&BoaScriptExecutor::default()).enhance(EnhancementInput {
    current,
    application: ApplicationLayer {
      defaults,
      listeners,
      platform: TargetPlatform::current(),
      enable_tun,
      ..ApplicationLayer::default()
    },
    profile_name: item.name.clone().unwrap_or_else(|| uid.to_string()),
    ..EnhancementInput::default()
  });
  let next_runtime = runtime
    .config
    .ok_or_else(|| "profile enhancement did not produce a runtime config".to_string())?;
  Ok(PreparedActivation {
    uid: uid.to_string(),
    runtime_path,
    previous_runtime,
    next_runtime,
  })
}

fn set_current_profile(store: &ProfileStore, uid: &str) -> rsclash_config::Result<()> {
  let mut transaction = store.begin()?;
  transaction.edit_catalog(|catalog| catalog.current = Some(uid.to_string()))?;
  transaction.validate()?;
  transaction.commit()?;
  Ok(())
}

fn load_snapshot(store: &ProfileStore) -> Result<ProfilesSnapshot, String> {
  let catalog = store.load_catalog().map_err(|error| error.to_string())?;
  let items = catalog
    .items()
    .iter()
    .filter_map(|item| {
      let uid = item.uid.as_ref()?.clone();
      let source = match item.kind.as_ref() {
        Some(ProfileKind::Local) => ProfileSourceKind::Local,
        Some(ProfileKind::Remote) => ProfileSourceKind::Remote,
        _ => ProfileSourceKind::Other,
      };
      Some(ProfileSummary {
        active: catalog.current.as_deref() == Some(uid.as_str()),
        name: item.name.clone().unwrap_or_else(|| uid.clone()),
        uid,
        source,
        location: None,
        updated_at: item.updated,
      })
    })
    .collect();
  Ok(ProfilesSnapshot { items, busy: false })
}

fn validate_profile_name(name: &str) -> Result<(), String> {
  let trimmed = name.trim();
  if trimmed.is_empty() {
    return Err("profile name must not be empty".to_string());
  }
  if trimmed.chars().count() > MAX_PROFILE_NAME_CHARS {
    return Err(format!(
      "profile name exceeds {MAX_PROFILE_NAME_CHARS} characters"
    ));
  }
  Ok(())
}

fn unique_profile_uid() -> String {
  static NEXT_ID: AtomicU64 = AtomicU64::new(0);
  format!(
    "profile-{}-{}",
    SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap_or_default()
      .as_millis(),
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
  )
}

fn unix_seconds() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs()
}

pub(crate) async fn run_profile_worker(
  access: ProfileAccess,
  activator: Arc<dyn RuntimeActivator>,
  command_rx: mpsc::Receiver<ProfileBridgeCommand>,
  event_tx: mpsc::Sender<ProfileBridgeEvent>,
) {
  ProfileWorker::new(access, activator, event_tx)
    .run(command_rx)
    .await;
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::{
    fs,
    path::PathBuf,
    sync::{
      Arc,
      atomic::{AtomicU64, Ordering},
    },
  };

  use rsclash_config::initialize_default_runtime;
  use rsclash_config::{Result as ConfigResult, RuntimeActivator, RuntimeValidator};
  use serde_yaml_ng::Value;
  use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
    sync::mpsc,
  };

  use super::{
    ProfileAccess, ProfileWorker, import_local, load_snapshot, prepare_activation,
    set_current_profile,
  };

  static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

  #[test]
  fn local_import_and_activation_preserve_application_control_fields() {
    let directory = TestDirectory::new();
    let store =
      initialize_default_runtime(&directory.root).expect("the default runtime should initialize");
    let source = directory.root.with_extension("source.yaml");
    fs::write(
      &source,
      "mixed-port: 7890\nmode: global\nproxies:\n- name: Node A\n  type: direct\nproxy-groups:\n- name: GLOBAL\n  type: select\n  proxies:\n  - Node A\nrules:\n- MATCH,GLOBAL\n",
    )
    .expect("the local profile should be written");

    import_local(&store, "Local test", &source).expect("the local profile should import");
    let catalog = store.load_catalog().expect("the catalog should load");
    let uid = catalog.items()[0]
      .uid
      .clone()
      .expect("the imported profile should have a UID");
    let prepared = prepare_activation(&store, &uid).expect("activation should prepare");

    assert_eq!(
      prepared
        .next_runtime
        .get("mixed-port")
        .and_then(Value::as_u64),
      Some(17_897)
    );
    assert_eq!(
      prepared.next_runtime.get("mode").and_then(Value::as_str),
      Some("rule")
    );
    assert_eq!(
      prepared
        .next_runtime
        .get("tun")
        .and_then(Value::as_mapping)
        .and_then(|tun| tun.get("enable"))
        .and_then(Value::as_bool),
      Some(false)
    );
    assert_eq!(
      prepared
        .next_runtime
        .get("proxies")
        .and_then(Value::as_sequence)
        .map(Vec::len),
      Some(1)
    );

    set_current_profile(&store, &uid).expect("the profile should become current");
    assert_eq!(
      load_snapshot(&store)
        .expect("the profile snapshot should load")
        .current()
        .map(|profile| profile.name.as_str()),
      Some("Local test")
    );
    fs::remove_file(source).expect("the local source should be removed");
  }

  struct AcceptValidator;

  #[async_trait::async_trait]
  impl RuntimeValidator for AcceptValidator {
    async fn validate(&self, _staging_path: &std::path::Path) -> ConfigResult<()> {
      Ok(())
    }
  }

  struct NoopActivator;

  #[async_trait::async_trait]
  impl RuntimeActivator for NoopActivator {
    async fn reload(&self, _runtime_path: &std::path::Path) -> ConfigResult<()> {
      Ok(())
    }

    async fn restart(&self, _runtime_path: &std::path::Path) -> ConfigResult<()> {
      Ok(())
    }
  }

  #[tokio::test]
  async fn remote_import_downloads_a_bounded_http_profile() {
    let directory = TestDirectory::new();
    let store =
      initialize_default_runtime(&directory.root).expect("the default runtime should initialize");
    let listener = TcpListener::bind("127.0.0.1:0")
      .await
      .expect("the HTTP listener should bind");
    let address = listener
      .local_addr()
      .expect("the HTTP listener should have an address");
    let server = tokio::spawn(async move {
      let (mut socket, _) = listener.accept().await.expect("the client should connect");
      let mut request = [0_u8; 1_024];
      let _ = socket
        .read(&mut request)
        .await
        .expect("the request should be readable");
      let body = b"mode: rule\nproxies: []\nproxy-groups: []\nrules: []\n";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      socket
        .write_all(response.as_bytes())
        .await
        .expect("the response head should send");
      socket
        .write_all(body)
        .await
        .expect("the response body should send");
    });
    let access = ProfileAccess::new(store.clone(), Arc::new(AcceptValidator))
      .expect("profile access should build");
    let (event_tx, _event_rx) = mpsc::channel(4);
    let worker = ProfileWorker::new(access, Arc::new(NoopActivator), event_tx);

    worker
      .import_remote(
        "Remote test".to_string(),
        format!("http://{address}/subscription?token=secret"),
      )
      .await
      .expect("the remote profile should import");
    server.await.expect("the HTTP server should finish");
    let catalog = store.load_catalog().expect("the catalog should load");
    assert_eq!(catalog.items()[0].name.as_deref(), Some("Remote test"));
    assert_eq!(
      catalog.items()[0].kind,
      Some(rsclash_config::ProfileKind::Remote)
    );
  }

  struct TestDirectory {
    root: PathBuf,
  }

  impl TestDirectory {
    fn new() -> Self {
      let root = std::env::temp_dir().join(format!(
        "rsclash-profile-worker-test-{}-{}",
        std::process::id(),
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
      ));
      fs::create_dir_all(&root).expect("the test root should be created");
      Self { root }
    }
  }

  impl Drop for TestDirectory {
    fn drop(&mut self) {
      let _ = fs::remove_dir_all(&self.root);
    }
  }
}
