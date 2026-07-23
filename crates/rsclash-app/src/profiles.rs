use std::{
  collections::BTreeSet,
  fs,
  path::{Path, PathBuf},
  sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
  },
  time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::{Client, Proxy, Url, header::HeaderMap, redirect::Policy};
use rsclash_config::{
  ApplicationLayer, EnhancementInput, EnhancementPipeline, ListenerPolicy, ManualLayer,
  MihomoConfig, NativeTransform, ProfileCatalog, ProfileItem, ProfileKind, ProfileOptions,
  ProfileStore, RuntimeActivator, RuntimeDeployer, RuntimeStore, RuntimeValidator, SequenceEdit,
  SequenceLayers, SubscriptionInfo, TargetPlatform, extract_control_plane,
};
use rsclash_domain::{
  ProfileDownloadProxy, ProfileSourceKind, ProfileSummary, ProfilesSnapshot, RemoteProfileOptions,
  SubscriptionUsage,
};
use rsclash_platform::SystemProxyBackend;
use serde_yaml_ng::Value;
use tokio::{
  sync::mpsc,
  task::spawn_blocking,
  time::{MissedTickBehavior, interval},
};

const MAX_PROFILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROFILE_NAME_CHARS: usize = 128;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
const AUTO_UPDATE_POLL_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct ProfileAccess {
  store: ProfileStore,
  validator: Arc<dyn RuntimeValidator>,
  direct_http: Client,
  system_proxy: Option<Arc<dyn SystemProxyBackend>>,
}

impl ProfileAccess {
  pub fn new(store: ProfileStore, validator: Arc<dyn RuntimeValidator>) -> Result<Self, String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = Client::builder()
      .connect_timeout(Duration::from_secs(10))
      .timeout(DOWNLOAD_TIMEOUT)
      .redirect(Policy::limited(5))
      .user_agent(concat!("rsclash/", env!("CARGO_PKG_VERSION")))
      .no_proxy()
      .build()
      .map_err(|error| format!("build the subscription HTTP client: {error}"))?;
    Ok(Self {
      store,
      validator,
      direct_http: http,
      system_proxy: None,
    })
  }

  pub fn with_system_proxy_backend(mut self, backend: Arc<dyn SystemProxyBackend>) -> Self {
    self.system_proxy = Some(backend);
    self
  }
}

#[derive(Clone, Debug)]
pub(crate) enum ProfileBridgeCommand {
  Refresh,
  Import(ProfileImportCommand),
  Activate { uid: String },
  Mutate(ProfileMutationCommand),
  Update(ProfileUpdateCommand),
}

#[derive(Clone, Debug)]
pub(crate) enum ProfileImportCommand {
  Local {
    name: String,
    path: String,
  },
  Remote {
    name: String,
    url: String,
    options: RemoteProfileOptions,
  },
}

#[derive(Clone, Debug)]
pub(crate) enum ProfileMutationCommand {
  Rename {
    uid: String,
    name: String,
  },
  Duplicate {
    uid: String,
  },
  Delete {
    uids: Vec<String>,
  },
  Reorder {
    uid: String,
    new_index: usize,
  },
  SetRemoteOptions {
    uid: String,
    options: RemoteProfileOptions,
  },
}

#[derive(Clone, Debug)]
pub(crate) enum ProfileUpdateCommand {
  One { uid: String },
  All,
}

pub(crate) enum ProfileBridgeEvent {
  Snapshot(ProfilesSnapshot),
  RuntimeChanged,
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
    self.update_automatic_profiles().await;
    let mut auto_update = interval(AUTO_UPDATE_POLL_INTERVAL);
    auto_update.set_missed_tick_behavior(MissedTickBehavior::Skip);
    auto_update.tick().await;
    loop {
      let command = tokio::select! {
        command = command_rx.recv() => command,
        _ = auto_update.tick() => {
          self.update_automatic_profiles().await;
          continue;
        },
      };
      let Some(command) = command else {
        break;
      };
      self.handle_command(command).await;
    }
  }

  async fn handle_command(&mut self, command: ProfileBridgeCommand) {
    match command {
      ProfileBridgeCommand::Refresh => self.refresh().await,
      ProfileBridgeCommand::Import(command) => {
        self.set_busy(true).await;
        let result = self.import(command).await;
        self.finish_operation(result).await;
      },
      ProfileBridgeCommand::Activate { uid } => {
        self.set_busy(true).await;
        let result = self.activate(uid).await;
        self.finish_operation(result).await;
      },
      ProfileBridgeCommand::Mutate(command) => {
        self.set_busy(true).await;
        let result = self.mutate(command).await;
        self.finish_operation(result).await;
      },
      ProfileBridgeCommand::Update(command) => {
        self.set_busy(true).await;
        let result = match command {
          ProfileUpdateCommand::One { uid } => self.update_remote(uid).await,
          ProfileUpdateCommand::All => self.update_all_remote().await,
        };
        self.finish_operation(result).await;
      },
    }
  }

  async fn import(&self, command: ProfileImportCommand) -> Result<(), String> {
    match command {
      ProfileImportCommand::Local { name, path } => {
        let store = self.access.store.clone();
        spawn_blocking(move || import_local(&store, &name, Path::new(&path)))
          .await
          .map_err(|error| format!("local profile import task failed: {error}"))?
      },
      ProfileImportCommand::Remote { name, url, options } => {
        self.import_remote(name, url, options).await
      },
    }
  }

  async fn mutate(&self, command: ProfileMutationCommand) -> Result<(), String> {
    let store = self.access.store.clone();
    spawn_blocking(move || match command {
      ProfileMutationCommand::Rename { uid, name } => rename_profile(&store, &uid, &name),
      ProfileMutationCommand::Duplicate { uid } => duplicate_profile(&store, &uid),
      ProfileMutationCommand::Delete { uids } => delete_profiles(&store, &uids),
      ProfileMutationCommand::Reorder { uid, new_index } => {
        reorder_profile(&store, &uid, new_index)
      },
      ProfileMutationCommand::SetRemoteOptions { uid, options } => {
        set_remote_options(&store, &uid, &options)
      },
    })
    .await
    .map_err(|error| format!("profile mutation task failed: {error}"))?
  }

  async fn import_remote(
    &self,
    name: String,
    url: String,
    mut options: RemoteProfileOptions,
  ) -> Result<(), String> {
    validate_profile_name(&name)?;
    normalize_remote_options(&mut options);
    validate_remote_options(&options)?;
    let download = self.download_remote(&url, &options).await?;
    if options.update_interval_minutes.is_none() {
      options.update_interval_minutes = download.suggested_update_interval_minutes;
    }
    let store = self.access.store.clone();
    spawn_blocking(move || {
      import_content(
        &store,
        &name,
        ProfileKind::Remote,
        Some(url),
        Some(options),
        download,
      )
    })
    .await
    .map_err(|error| format!("remote profile import task failed: {error}"))?
  }

  async fn download_remote(
    &self,
    url: &str,
    options: &RemoteProfileOptions,
  ) -> Result<DownloadedProfile, String> {
    let parsed = Url::parse(url).map_err(|error| format!("invalid subscription URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
      return Err("subscription URL must use HTTP or HTTPS".to_string());
    }
    let client = self.http_client(options, parsed.scheme()).await?;
    let headers;
    let mut response = client
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
    headers = response.headers().clone();
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
    if content.starts_with(&[0xef, 0xbb, 0xbf]) {
      content.drain(..3);
    }
    let metadata = parse_subscription_headers(&headers);
    Ok(DownloadedProfile {
      content,
      usage: metadata.usage,
      home_page: metadata.home_page,
      suggested_update_interval_minutes: metadata.suggested_update_interval_minutes,
    })
  }

  async fn http_client(
    &self,
    options: &RemoteProfileOptions,
    target_scheme: &str,
  ) -> Result<Client, String> {
    if options.user_agent.is_none()
      && options.timeout_seconds == DOWNLOAD_TIMEOUT.as_secs()
      && options.download_proxy == ProfileDownloadProxy::Direct
      && !options.accept_invalid_certs
    {
      return Ok(self.access.direct_http.clone());
    }

    let proxy_url = match options.download_proxy {
      ProfileDownloadProxy::Direct => None,
      ProfileDownloadProxy::System => {
        let backend =
          self.access.system_proxy.as_ref().ok_or_else(|| {
            "system proxy downloads are not available on this platform".to_string()
          })?;
        let snapshot = backend
          .current()
          .await
          .map_err(|error| format!("read system proxy settings: {error}"))?;
        if !snapshot.enabled || snapshot.mode.as_deref() != Some("manual") {
          return Err("the system proxy is not configured in manual mode".to_string());
        }
        let endpoint = if target_scheme == "https" {
          snapshot.https_proxy.or(snapshot.http_proxy)
        } else {
          snapshot.http_proxy
        }
        .ok_or_else(|| format!("the system proxy has no endpoint for {target_scheme}"))?;
        Some(format!("http://{endpoint}"))
      },
      ProfileDownloadProxy::Mihomo => {
        let store = self.access.store.clone();
        Some(
          spawn_blocking(move || mihomo_proxy_url(&store))
            .await
            .map_err(|error| format!("Mihomo proxy lookup task failed: {error}"))??,
        )
      },
    };

    let timeout_seconds = options.timeout_seconds;
    let mut builder = Client::builder()
      .connect_timeout(Duration::from_secs(timeout_seconds.min(10)))
      .timeout(Duration::from_secs(timeout_seconds))
      .redirect(Policy::limited(5))
      .danger_accept_invalid_certs(options.accept_invalid_certs)
      .no_proxy();
    if let Some(user_agent) = options.user_agent.as_deref() {
      builder = builder.user_agent(user_agent);
    } else {
      builder = builder.user_agent(concat!("rsclash/", env!("CARGO_PKG_VERSION")));
    }
    if let Some(proxy_url) = proxy_url {
      let proxy =
        Proxy::all(&proxy_url).map_err(|error| format!("configure download proxy: {error}"))?;
      builder = builder.proxy(proxy);
    }
    builder
      .build()
      .map_err(|error| format!("build subscription HTTP client: {error}"))
  }

  async fn update_remote(&self, uid: String) -> Result<(), String> {
    let store = self.access.store.clone();
    let lookup_uid = uid.clone();
    let profile = spawn_blocking(move || remote_profile(&store, &lookup_uid))
      .await
      .map_err(|error| format!("profile lookup task failed: {error}"))??;
    let download = self.download_remote(&profile.url, &profile.options).await?;
    let store = self.access.store.clone();
    let replace_uid = uid.clone();
    let rollback = spawn_blocking(move || replace_profile(&store, &replace_uid, download))
      .await
      .map_err(|error| format!("profile update task failed: {error}"))??;

    if !profile.active {
      return Ok(());
    }
    if let Err(update_error) = self.activate(uid.clone()).await {
      let store = self.access.store.clone();
      let restore_uid = uid;
      let restore = spawn_blocking(move || restore_profile(&store, &restore_uid, rollback))
        .await
        .map_err(|error| format!("profile restore task failed: {error}"))
        .and_then(|result| result);
      return match restore {
        Ok(()) => Err(format!(
          "activate updated profile: {update_error}; the previous subscription was restored"
        )),
        Err(restore_error) => Err(format!(
          "activate updated profile: {update_error}; restore previous subscription: {restore_error}"
        )),
      };
    }
    let _ = self.event_tx.send(ProfileBridgeEvent::RuntimeChanged).await;
    Ok(())
  }

  async fn update_all_remote(&self) -> Result<(), String> {
    let store = self.access.store.clone();
    let uids = spawn_blocking(move || remote_profile_uids(&store))
      .await
      .map_err(|error| format!("profile lookup task failed: {error}"))??;
    self.update_remote_profiles(uids).await
  }

  async fn update_remote_profiles(&self, uids: Vec<String>) -> Result<(), String> {
    let mut failures = Vec::new();
    for uid in uids {
      if let Err(error) = self.update_remote(uid.clone()).await {
        failures.push(format!("{uid}: {error}"));
      }
    }
    if failures.is_empty() {
      Ok(())
    } else {
      Err(format!(
        "{} subscription update(s) failed: {}",
        failures.len(),
        failures.join("; ")
      ))
    }
  }

  async fn update_automatic_profiles(&mut self) {
    let store = self.access.store.clone();
    let result = spawn_blocking(move || due_remote_profile_uids(&store, unix_seconds()))
      .await
      .map_err(|error| format!("automatic profile lookup task failed: {error}"))
      .and_then(|result| result);
    let uids = match result {
      Ok(uids) if uids.is_empty() => return,
      Ok(uids) => uids,
      Err(error) => {
        self.fail(error).await;
        return;
      },
    };
    self.set_busy(true).await;
    let result = self.update_remote_profiles(uids).await;
    self.finish_operation(result).await;
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
    let error = result.err();
    self.refresh().await;
    if let Some(error) = error {
      self.fail(error).await;
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
  import_content(
    store,
    name,
    ProfileKind::Local,
    None,
    None,
    DownloadedProfile::from_content(content),
  )
}

fn import_content(
  store: &ProfileStore,
  name: &str,
  kind: ProfileKind,
  url: Option<String>,
  options: Option<RemoteProfileOptions>,
  download: DownloadedProfile,
) -> Result<(), String> {
  validate_profile_name(name)?;
  validate_source_content(&download.content)?;
  let uid = unique_profile_uid();
  let item = ProfileItem {
    uid: Some(uid.clone()),
    kind: Some(kind),
    name: Some(name.trim().to_string()),
    file: Some(format!("{uid}.yaml")),
    url,
    extra: download.usage,
    option: options.as_ref().map(remote_options_to_config),
    home: download.home_page,
    updated: Some(unix_seconds()),
    ..ProfileItem::default()
  };
  let mut transaction = store.begin().map_err(|error| error.to_string())?;
  transaction
    .add_profile(item, download.content)
    .map_err(|error| error.to_string())?;
  transaction.validate().map_err(|error| error.to_string())?;
  transaction.commit().map_err(|error| error.to_string())?;
  Ok(())
}

struct RemoteProfile {
  url: String,
  active: bool,
  options: RemoteProfileOptions,
}

struct DownloadedProfile {
  content: Vec<u8>,
  usage: Option<SubscriptionInfo>,
  home_page: Option<String>,
  suggested_update_interval_minutes: Option<u64>,
}

impl DownloadedProfile {
  const fn from_content(content: Vec<u8>) -> Self {
    Self {
      content,
      usage: None,
      home_page: None,
      suggested_update_interval_minutes: None,
    }
  }
}

struct ProfileRollback {
  content: Vec<u8>,
  updated_at: Option<u64>,
  usage: Option<SubscriptionInfo>,
  home_page: Option<String>,
  options: Option<ProfileOptions>,
}

fn remote_profile(store: &ProfileStore, uid: &str) -> Result<RemoteProfile, String> {
  let catalog = store.load_catalog().map_err(|error| error.to_string())?;
  let item = catalog
    .get(uid)
    .ok_or_else(|| format!("profile {uid} does not exist"))?;
  if item.kind != Some(ProfileKind::Remote) {
    return Err(format!("profile {uid} is not a remote subscription"));
  }
  let url = item
    .url
    .clone()
    .filter(|url| !url.is_empty())
    .ok_or_else(|| format!("remote profile {uid} has no subscription URL"))?;
  Ok(RemoteProfile {
    url,
    active: catalog.current.as_deref() == Some(uid),
    options: remote_options_from_config(item.option.as_ref()),
  })
}

fn remote_profile_uids(store: &ProfileStore) -> Result<Vec<String>, String> {
  let catalog = store.load_catalog().map_err(|error| error.to_string())?;
  Ok(
    catalog
      .items()
      .iter()
      .filter(|item| item.kind == Some(ProfileKind::Remote))
      .filter_map(|item| item.uid.clone())
      .collect(),
  )
}

fn due_remote_profile_uids(store: &ProfileStore, now: u64) -> Result<Vec<String>, String> {
  let catalog = store.load_catalog().map_err(|error| error.to_string())?;
  Ok(
    catalog
      .items()
      .iter()
      .filter(|item| item.kind == Some(ProfileKind::Remote))
      .filter(|item| {
        let options = item.option.as_ref();
        if !options
          .and_then(|options| options.allow_auto_update)
          .unwrap_or(true)
        {
          return false;
        }
        let Some(interval_minutes) = options.and_then(|options| options.update_interval) else {
          return false;
        };
        let due_at = item
          .updated
          .unwrap_or(0)
          .saturating_add(interval_minutes.saturating_mul(60));
        now >= due_at
      })
      .filter_map(|item| item.uid.clone())
      .collect(),
  )
}

fn rename_profile(store: &ProfileStore, uid: &str, name: &str) -> Result<(), String> {
  validate_profile_name(name)?;
  let mut transaction = store.begin().map_err(|error| error.to_string())?;
  if transaction.catalog().get(uid).is_none() {
    return Err(format!("profile {uid} does not exist"));
  }
  let name = name.trim().to_string();
  transaction
    .edit_catalog(|catalog| {
      if let Some(item) = catalog
        .items_mut()
        .iter_mut()
        .find(|item| item.uid.as_deref() == Some(uid))
      {
        item.name = Some(name);
      }
    })
    .map_err(|error| error.to_string())?;
  transaction.validate().map_err(|error| error.to_string())?;
  transaction.commit().map_err(|error| error.to_string())?;
  Ok(())
}

fn duplicate_profile(store: &ProfileStore, uid: &str) -> Result<(), String> {
  let catalog = store.load_catalog().map_err(|error| error.to_string())?;
  let source = catalog
    .get(uid)
    .cloned()
    .ok_or_else(|| format!("profile {uid} does not exist"))?;
  if matches!(
    source.kind,
    Some(ProfileKind::Script | ProfileKind::Unknown(_)) | None
  ) {
    return Err(format!("profile {uid} cannot be duplicated"));
  }
  let source_file = source
    .require_file()
    .map_err(|error| error.to_string())?
    .to_string();
  let content = store
    .read_profile(&source_file)
    .map_err(|error| error.to_string())?
    .into_bytes();
  let new_uid = unique_profile_uid();
  let extension = Path::new(&source_file)
    .extension()
    .and_then(|extension| extension.to_str())
    .filter(|extension| !extension.is_empty())
    .unwrap_or("yaml");
  let mut copy = source;
  copy.uid = Some(new_uid.clone());
  copy.file = Some(format!("{new_uid}.{extension}"));
  copy.name = Some(format!(
    "{} (copy)",
    copy.name.as_deref().unwrap_or("Unnamed profile")
  ));
  copy.selected = None;
  copy.updated = Some(unix_seconds());
  copy.file_data = None;

  let mut transaction = store.begin().map_err(|error| error.to_string())?;
  transaction
    .add_profile(copy, content)
    .map_err(|error| error.to_string())?;
  transaction.validate().map_err(|error| error.to_string())?;
  transaction.commit().map_err(|error| error.to_string())?;
  Ok(())
}

fn delete_profiles(store: &ProfileStore, uids: &[String]) -> Result<(), String> {
  let unique = uids
    .iter()
    .map(String::as_str)
    .filter(|uid| !uid.is_empty())
    .collect::<BTreeSet<_>>();
  if unique.is_empty() {
    return Err("select at least one profile to delete".to_string());
  }
  let mut transaction = store.begin().map_err(|error| error.to_string())?;
  for uid in &unique {
    if transaction.catalog().get(uid).is_none() {
      return Err(format!("profile {uid} does not exist"));
    }
  }
  for uid in unique {
    transaction
      .remove_profile(uid)
      .map_err(|error| error.to_string())?;
  }
  transaction.validate().map_err(|error| error.to_string())?;
  transaction.commit().map_err(|error| error.to_string())?;
  Ok(())
}

fn reorder_profile(store: &ProfileStore, uid: &str, new_index: usize) -> Result<(), String> {
  let mut transaction = store.begin().map_err(|error| error.to_string())?;
  let item_count = transaction.catalog().items().len();
  if new_index >= item_count {
    return Err(format!(
      "profile index {new_index} is outside the {item_count}-item catalog"
    ));
  }
  let old_index = transaction
    .catalog()
    .items()
    .iter()
    .position(|item| item.uid.as_deref() == Some(uid))
    .ok_or_else(|| format!("profile {uid} does not exist"))?;
  transaction
    .edit_catalog(|catalog| {
      let item = catalog.items_mut().remove(old_index);
      catalog.items_mut().insert(new_index, item);
    })
    .map_err(|error| error.to_string())?;
  transaction.validate().map_err(|error| error.to_string())?;
  transaction.commit().map_err(|error| error.to_string())?;
  Ok(())
}

fn set_remote_options(
  store: &ProfileStore,
  uid: &str,
  options: &RemoteProfileOptions,
) -> Result<(), String> {
  validate_remote_options(options)?;
  let mut transaction = store.begin().map_err(|error| error.to_string())?;
  let item = transaction
    .catalog()
    .get(uid)
    .ok_or_else(|| format!("profile {uid} does not exist"))?;
  if item.kind != Some(ProfileKind::Remote) {
    return Err(format!("profile {uid} is not a remote subscription"));
  }
  let options = options.clone();
  transaction
    .edit_catalog(|catalog| {
      if let Some(item) = catalog
        .items_mut()
        .iter_mut()
        .find(|item| item.uid.as_deref() == Some(uid))
      {
        let mut configured = item.option.clone().unwrap_or_default();
        apply_remote_options(&mut configured, &options);
        item.option = Some(configured);
      }
    })
    .map_err(|error| error.to_string())?;
  transaction.validate().map_err(|error| error.to_string())?;
  transaction.commit().map_err(|error| error.to_string())?;
  Ok(())
}

fn replace_profile(
  store: &ProfileStore,
  uid: &str,
  download: DownloadedProfile,
) -> Result<ProfileRollback, String> {
  validate_source_content(&download.content)?;
  let catalog = store.load_catalog().map_err(|error| error.to_string())?;
  let item = catalog
    .get(uid)
    .ok_or_else(|| format!("profile {uid} does not exist"))?;
  if item.kind != Some(ProfileKind::Remote) {
    return Err(format!("profile {uid} is not a remote subscription"));
  }
  let previous = ProfileRollback {
    content: store
      .read_profile(item.require_file().map_err(|error| error.to_string())?)
      .map_err(|error| error.to_string())?
      .into_bytes(),
    updated_at: item.updated,
    usage: item.extra,
    home_page: item.home.clone(),
    options: item.option.clone(),
  };
  let suggested_interval = download.suggested_update_interval_minutes;
  let usage = download.usage;
  let home_page = download.home_page;
  let mut transaction = store.begin().map_err(|error| error.to_string())?;
  transaction
    .edit_catalog(|catalog| {
      if let Some(item) = catalog
        .items_mut()
        .iter_mut()
        .find(|item| item.uid.as_deref() == Some(uid))
      {
        item.updated = Some(unix_seconds());
        item.extra = usage;
        item.home = home_page;
        if item
          .option
          .as_ref()
          .is_none_or(|options| options.update_interval.is_none())
          && let Some(interval) = suggested_interval
        {
          item
            .option
            .get_or_insert_with(ProfileOptions::default)
            .update_interval = Some(interval);
        }
      }
    })
    .map_err(|error| error.to_string())?;
  transaction
    .stage_profile(uid, download.content)
    .map_err(|error| error.to_string())?;
  transaction.validate().map_err(|error| error.to_string())?;
  transaction.commit().map_err(|error| error.to_string())?;
  Ok(previous)
}

fn restore_profile(
  store: &ProfileStore,
  uid: &str,
  rollback: ProfileRollback,
) -> Result<(), String> {
  let mut transaction = store.begin().map_err(|error| error.to_string())?;
  if transaction.catalog().get(uid).is_none() {
    return Err(format!("profile {uid} does not exist"));
  }
  transaction
    .edit_catalog(|catalog| {
      if let Some(item) = catalog
        .items_mut()
        .iter_mut()
        .find(|item| item.uid.as_deref() == Some(uid))
      {
        item.updated = rollback.updated_at;
        item.extra = rollback.usage;
        item.home = rollback.home_page;
        item.option = rollback.options;
      }
    })
    .map_err(|error| error.to_string())?;
  transaction
    .stage_profile(uid, rollback.content)
    .map_err(|error| error.to_string())?;
  transaction.validate().map_err(|error| error.to_string())?;
  transaction.commit().map_err(|error| error.to_string())?;
  Ok(())
}

fn remote_options_from_config(options: Option<&ProfileOptions>) -> RemoteProfileOptions {
  let options = options.cloned().unwrap_or_default();
  let download_proxy = if options.self_proxy.unwrap_or(false) {
    ProfileDownloadProxy::Mihomo
  } else if options.with_proxy.unwrap_or(false) {
    ProfileDownloadProxy::System
  } else {
    ProfileDownloadProxy::Direct
  };
  RemoteProfileOptions {
    user_agent: options.user_agent,
    update_interval_minutes: options.update_interval,
    timeout_seconds: options
      .timeout_seconds
      .unwrap_or(DOWNLOAD_TIMEOUT.as_secs()),
    download_proxy,
    accept_invalid_certs: options.danger_accept_invalid_certs.unwrap_or(false),
    allow_auto_update: options.allow_auto_update.unwrap_or(true),
  }
}

fn remote_options_to_config(options: &RemoteProfileOptions) -> ProfileOptions {
  let mut configured = ProfileOptions::default();
  apply_remote_options(&mut configured, options);
  configured
}

fn apply_remote_options(configured: &mut ProfileOptions, options: &RemoteProfileOptions) {
  configured.user_agent = options
    .user_agent
    .as_deref()
    .map(str::trim)
    .filter(|user_agent| !user_agent.is_empty())
    .map(str::to_string);
  configured.update_interval = options.update_interval_minutes;
  configured.timeout_seconds = Some(options.timeout_seconds);
  configured.danger_accept_invalid_certs = Some(options.accept_invalid_certs);
  configured.allow_auto_update = Some(options.allow_auto_update);
  configured.with_proxy = Some(options.download_proxy == ProfileDownloadProxy::System);
  configured.self_proxy = Some(options.download_proxy == ProfileDownloadProxy::Mihomo);
}

fn normalize_remote_options(options: &mut RemoteProfileOptions) {
  options.user_agent = options
    .user_agent
    .as_deref()
    .map(str::trim)
    .filter(|user_agent| !user_agent.is_empty())
    .map(str::to_string);
}

fn validate_remote_options(options: &RemoteProfileOptions) -> Result<(), String> {
  if !(1..=300).contains(&options.timeout_seconds) {
    return Err("subscription timeout must be between 1 and 300 seconds".to_string());
  }
  if options
    .update_interval_minutes
    .is_some_and(|minutes| !(1..=525_600).contains(&minutes))
  {
    return Err("subscription update interval must be between 1 minute and 1 year".to_string());
  }
  if options
    .user_agent
    .as_ref()
    .is_some_and(|user_agent| user_agent.chars().count() > 512)
  {
    return Err("subscription User-Agent exceeds 512 characters".to_string());
  }
  Ok(())
}

fn mihomo_proxy_url(store: &ProfileStore) -> Result<String, String> {
  let source = fs::read_to_string(&store.paths().runtime_config).map_err(|error| {
    format!(
      "read runtime config {}: {error}",
      store.paths().runtime_config.display()
    )
  })?;
  let runtime = MihomoConfig::parse(&source).map_err(|error| error.to_string())?;
  let port = runtime
    .get("mixed-port")
    .and_then(Value::as_u64)
    .and_then(|port| u16::try_from(port).ok())
    .filter(|port| *port != 0)
    .ok_or_else(|| "the runtime config has no valid mixed-port".to_string())?;
  Ok(format!("http://127.0.0.1:{port}"))
}

struct SubscriptionHeaders {
  usage: Option<SubscriptionInfo>,
  home_page: Option<String>,
  suggested_update_interval_minutes: Option<u64>,
}

fn parse_subscription_headers(headers: &HeaderMap) -> SubscriptionHeaders {
  let usage = headers.iter().find_map(|(name, value)| {
    let name = name.as_str();
    let prefix = name.strip_suffix("subscription-userinfo")?;
    if !prefix.is_empty() && !prefix.ends_with('-') {
      return None;
    }
    let value = value.to_str().ok()?;
    Some(SubscriptionInfo {
      upload: parse_header_u64(value, "upload").unwrap_or(0),
      download: parse_header_u64(value, "download").unwrap_or(0),
      total: parse_header_u64(value, "total").unwrap_or(0),
      expire: parse_header_u64(value, "expire").unwrap_or(0),
    })
  });
  let home_page = headers
    .get("profile-web-page-url")
    .and_then(|value| value.to_str().ok())
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .map(str::to_string);
  let suggested_update_interval_minutes = headers
    .get("profile-update-interval")
    .and_then(|value| value.to_str().ok())
    .and_then(|value| value.trim().parse::<u64>().ok())
    .and_then(|hours| hours.checked_mul(60))
    .filter(|minutes| *minutes != 0);
  SubscriptionHeaders {
    usage,
    home_page,
    suggested_update_interval_minutes,
  }
}

fn parse_header_u64(value: &str, key: &str) -> Option<u64> {
  value.split(';').find_map(|field| {
    let (name, value) = field.trim().split_once('=')?;
    (name.trim().eq_ignore_ascii_case(key))
      .then(|| value.trim().parse::<u64>().ok())
      .flatten()
  })
}

fn validate_source_content(content: &[u8]) -> Result<(), String> {
  let source =
    std::str::from_utf8(content).map_err(|_| "profile must be valid UTF-8 YAML".to_string())?;
  let config =
    MihomoConfig::parse(source).map_err(|error| format!("parse profile YAML: {error}"))?;
  if config.mapping().is_empty() {
    return Err("profile YAML must not be empty".to_string());
  }
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
  let enhancements = load_profile_enhancements(store, &catalog, item)?;
  let runtime = EnhancementPipeline::enhance(EnhancementInput {
    current,
    sequence: enhancements.sequence,
    application: ApplicationLayer {
      defaults,
      listeners,
      platform: TargetPlatform::current(),
      enable_tun,
      native_transforms: NativeTransform::compatibility_defaults().to_vec(),
      ..ApplicationLayer::default()
    },
    global: enhancements.global,
    profile: enhancements.profile,
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

struct ProfileEnhancements {
  sequence: SequenceLayers,
  global: ManualLayer,
  profile: ManualLayer,
}

fn load_profile_enhancements(
  store: &ProfileStore,
  catalog: &ProfileCatalog,
  source: &ProfileItem,
) -> Result<ProfileEnhancements, String> {
  let options = source.option.as_ref();
  let sequence = SequenceLayers {
    rules: read_sequence_edit(
      store,
      catalog,
      options.and_then(|options| options.rules.as_deref()),
      ProfileKind::Rules,
    )?,
    proxies: read_sequence_edit(
      store,
      catalog,
      options.and_then(|options| options.proxies.as_deref()),
      ProfileKind::Proxies,
    )?,
    groups: read_sequence_edit(
      store,
      catalog,
      options.and_then(|options| options.groups.as_deref()),
      ProfileKind::Groups,
    )?,
  };
  let profile = ManualLayer {
    merge: read_merge(
      store,
      catalog,
      options.and_then(|options| options.merge.as_deref()),
    )?,
  };
  let global = ManualLayer {
    merge: catalog
      .get("Merge")
      .filter(|item| item.kind == Some(ProfileKind::Merge))
      .map(|_| read_merge(store, catalog, Some("Merge")))
      .transpose()?
      .flatten(),
  };
  Ok(ProfileEnhancements {
    sequence,
    global,
    profile,
  })
}

fn read_sequence_edit(
  store: &ProfileStore,
  catalog: &ProfileCatalog,
  uid: Option<&str>,
  expected_kind: ProfileKind,
) -> Result<SequenceEdit, String> {
  let Some(source) = read_enhancement_profile(store, catalog, uid, expected_kind)? else {
    return Ok(SequenceEdit::default());
  };
  serde_yaml_ng::from_str(&source).map_err(|error| format!("parse sequence profile YAML: {error}"))
}

fn read_merge(
  store: &ProfileStore,
  catalog: &ProfileCatalog,
  uid: Option<&str>,
) -> Result<Option<serde_yaml_ng::Mapping>, String> {
  let Some(source) = read_enhancement_profile(store, catalog, uid, ProfileKind::Merge)? else {
    return Ok(None);
  };
  serde_yaml_ng::from_str(&source)
    .map(Some)
    .map_err(|error| format!("parse merge profile YAML: {error}"))
}

fn read_enhancement_profile(
  store: &ProfileStore,
  catalog: &ProfileCatalog,
  uid: Option<&str>,
  expected_kind: ProfileKind,
) -> Result<Option<String>, String> {
  let Some(uid) = uid else {
    return Ok(None);
  };
  let item = catalog
    .get(uid)
    .ok_or_else(|| format!("referenced {expected_kind} profile {uid} does not exist"))?;
  if item.kind.as_ref() != Some(&expected_kind) {
    return Err(format!(
      "referenced profile {uid} has type {}, expected {expected_kind}",
      item
        .kind
        .as_ref()
        .map_or_else(|| "unknown".to_string(), ToString::to_string)
    ));
  }
  store
    .read_profile(item.require_file().map_err(|error| error.to_string())?)
    .map(Some)
    .map_err(|error| error.to_string())
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
        Some(ProfileKind::Merge) => ProfileSourceKind::Merge,
        Some(ProfileKind::Rules) => ProfileSourceKind::Rules,
        Some(ProfileKind::Proxies) => ProfileSourceKind::Proxies,
        Some(ProfileKind::Groups) => ProfileSourceKind::Groups,
        _ => ProfileSourceKind::Other,
      };
      Some(ProfileSummary {
        active: catalog.current.as_deref() == Some(uid.as_str()),
        name: item.name.clone().unwrap_or_else(|| uid.clone()),
        uid,
        source,
        location: None,
        updated_at: item.updated,
        home_page: item.home.clone(),
        usage: item.extra.map(|usage| SubscriptionUsage {
          upload: usage.upload,
          download: usage.download,
          total: usage.total,
          expire: usage.expire,
        }),
        remote_options: (item.kind == Some(ProfileKind::Remote))
          .then(|| remote_options_from_config(item.option.as_ref())),
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
    DownloadedProfile, ProfileAccess, ProfileWorker, delete_profiles, due_remote_profile_uids,
    duplicate_profile, import_content, import_local, load_snapshot, prepare_activation,
    rename_profile, reorder_profile, set_current_profile, set_remote_options,
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
    let mut transaction = store
      .begin()
      .expect("the enhancement transaction should begin");
    for (uid, kind, content) in [
      (
        "merge-test",
        rsclash_config::ProfileKind::Merge,
        "custom-field: applied\n",
      ),
      (
        "rules-test",
        rsclash_config::ProfileKind::Rules,
        "prepend: ['DOMAIN,example.com,DIRECT']\nappend: []\ndelete: []\n",
      ),
      (
        "proxies-test",
        rsclash_config::ProfileKind::Proxies,
        "prepend: []\nappend:\n- name: Node B\n  type: direct\ndelete: []\n",
      ),
      (
        "groups-test",
        rsclash_config::ProfileKind::Groups,
        "prepend: []\nappend:\n- name: Group B\n  type: select\n  proxies: [Node B]\ndelete: []\n",
      ),
    ] {
      transaction
        .add_profile(
          rsclash_config::ProfileItem {
            uid: Some(uid.to_string()),
            kind: Some(kind),
            file: Some(format!("{uid}.yaml")),
            ..rsclash_config::ProfileItem::default()
          },
          content,
        )
        .expect("the enhancement profile should stage");
    }
    transaction
      .edit_catalog(|catalog| {
        catalog
          .items_mut()
          .iter_mut()
          .find(|item| item.uid.as_deref() == Some(uid.as_str()))
          .expect("the source profile should exist")
          .option = Some(rsclash_config::ProfileOptions {
          merge: Some("merge-test".to_string()),
          rules: Some("rules-test".to_string()),
          proxies: Some("proxies-test".to_string()),
          groups: Some("groups-test".to_string()),
          ..rsclash_config::ProfileOptions::default()
        });
      })
      .expect("the source profile should reference enhancements");
    transaction
      .validate()
      .expect("the enhancement transaction should validate");
    transaction
      .commit()
      .expect("the enhancement transaction should commit");
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
        .get("custom-field")
        .and_then(Value::as_str),
      Some("applied")
    );
    assert_eq!(
      prepared
        .next_runtime
        .get("proxies")
        .and_then(Value::as_sequence)
        .map(Vec::len),
      Some(2)
    );
    assert_eq!(
      prepared
        .next_runtime
        .get("proxy-groups")
        .and_then(Value::as_sequence)
        .map(Vec::len),
      Some(2)
    );
    assert_eq!(
      prepared
        .next_runtime
        .get("rules")
        .and_then(Value::as_sequence)
        .map(Vec::len),
      Some(2)
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

  #[test]
  fn profile_lifecycle_mutations_are_transactional() {
    let directory = TestDirectory::new();
    let store =
      initialize_default_runtime(&directory.root).expect("the default runtime should initialize");
    let content = b"mode: rule\nproxies: []\nproxy-groups: []\nrules: []\n".to_vec();
    import_content(
      &store,
      "Original",
      rsclash_config::ProfileKind::Local,
      None,
      None,
      DownloadedProfile::from_content(content.clone()),
    )
    .expect("the profile should import");
    let original_uid = store
      .load_catalog()
      .expect("the catalog should load")
      .items()[0]
      .uid
      .clone()
      .expect("the profile should have a UID");

    rename_profile(&store, &original_uid, "Renamed").expect("the profile should rename");
    duplicate_profile(&store, &original_uid).expect("the profile should duplicate");
    let catalog = store.load_catalog().expect("the catalog should load");
    assert_eq!(catalog.items().len(), 2);
    assert_eq!(
      catalog
        .get(&original_uid)
        .and_then(|item| item.name.as_deref()),
      Some("Renamed")
    );
    let copy = catalog
      .items()
      .iter()
      .find(|item| item.uid.as_deref() != Some(original_uid.as_str()))
      .expect("the copy should exist");
    let copy_uid = copy.uid.clone().expect("the copy should have a UID");
    assert_eq!(copy.name.as_deref(), Some("Renamed (copy)"));
    assert_eq!(
      store
        .read_profile(copy.require_file().expect("the copy should have a file"))
        .expect("the copy should be readable")
        .as_bytes(),
      content
    );

    reorder_profile(&store, &copy_uid, 0).expect("the copy should move to the beginning");
    assert_eq!(
      store
        .load_catalog()
        .expect("the catalog should load")
        .items()[0]
        .uid
        .as_deref(),
      Some(copy_uid.as_str())
    );
    set_current_profile(&store, &original_uid).expect("the original should become current");
    delete_profiles(&store, &[original_uid, copy_uid])
      .expect("the profiles should be deleted together");
    let catalog = store.load_catalog().expect("the catalog should load");
    assert!(catalog.items().is_empty());
    assert!(catalog.current.is_none());
    assert_eq!(
      fs::read_dir(&store.paths().profiles_dir)
        .expect("the profiles directory should be readable")
        .count(),
      0
    );
  }

  struct AcceptValidator;

  #[async_trait::async_trait]
  impl RuntimeValidator for AcceptValidator {
    async fn validate(&self, _staging_path: &std::path::Path) -> ConfigResult<()> {
      Ok(())
    }
  }

  struct RejectValidator;

  #[async_trait::async_trait]
  impl RuntimeValidator for RejectValidator {
    async fn validate(&self, _staging_path: &std::path::Path) -> ConfigResult<()> {
      Err(rsclash_config::Error::RuntimeValidation(
        "rejected by test validator".to_string(),
      ))
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
      let read = socket
        .read(&mut request)
        .await
        .expect("the request should be readable");
      assert!(String::from_utf8_lossy(&request[..read]).contains("user-agent: rsclash-test-agent"));
      let body = b"mode: rule\nproxies: []\nproxy-groups: []\nrules: []\n";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nSubscription-Userinfo: upload=1024; download=2048; total=8192; expire=2000000000\r\nProfile-Update-Interval: 6\r\nProfile-Web-Page-Url: https://portal.example.com/\r\nConnection: close\r\n\r\n",
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
    let options = rsclash_domain::RemoteProfileOptions {
      user_agent: Some("rsclash-test-agent".to_string()),
      ..rsclash_domain::RemoteProfileOptions::default()
    };

    worker
      .import_remote(
        "Remote test".to_string(),
        format!("http://{address}/subscription?token=secret"),
        options,
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
    assert_eq!(
      catalog.items()[0].extra,
      Some(rsclash_config::SubscriptionInfo {
        upload: 1_024,
        download: 2_048,
        total: 8_192,
        expire: 2_000_000_000,
      })
    );
    assert_eq!(
      catalog.items()[0].home.as_deref(),
      Some("https://portal.example.com/")
    );
    let options = catalog.items()[0]
      .option
      .as_ref()
      .expect("remote options should be stored");
    assert_eq!(options.user_agent.as_deref(), Some("rsclash-test-agent"));
    assert_eq!(options.update_interval, Some(360));
    let uid = catalog.items()[0]
      .uid
      .as_deref()
      .expect("the remote profile should have a UID");
    let updated = catalog.items()[0]
      .updated
      .expect("the remote profile should have an update time");
    assert!(
      due_remote_profile_uids(&store, updated + 360 * 60 - 1)
        .expect("the automatic update schedule should load")
        .is_empty()
    );
    assert_eq!(
      due_remote_profile_uids(&store, updated + 360 * 60)
        .expect("the automatic update schedule should load"),
      vec![uid.to_string()]
    );

    let disabled = rsclash_domain::RemoteProfileOptions {
      user_agent: Some("replacement-agent".to_string()),
      update_interval_minutes: Some(5),
      allow_auto_update: false,
      ..rsclash_domain::RemoteProfileOptions::default()
    };
    set_remote_options(&store, uid, &disabled).expect("the remote options should update");
    assert!(
      due_remote_profile_uids(&store, u64::MAX)
        .expect("the disabled automatic update schedule should load")
        .is_empty()
    );
  }

  #[tokio::test]
  async fn active_remote_update_restores_source_when_runtime_validation_fails() {
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
      let body =
        b"mode: rule\nproxies:\n- name: Updated\n  type: direct\nproxy-groups: []\nrules: []\n";
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
    let old_content =
      b"mode: rule\nproxies:\n- name: Original\n  type: direct\nproxy-groups: []\nrules: []\n";
    import_content(
      &store,
      "Remote",
      rsclash_config::ProfileKind::Remote,
      Some(format!("http://{address}/subscription")),
      Some(rsclash_domain::RemoteProfileOptions::default()),
      DownloadedProfile::from_content(old_content.to_vec()),
    )
    .expect("the remote profile should import");
    let catalog = store.load_catalog().expect("the catalog should load");
    let item = catalog.items()[0].clone();
    let uid = item.uid.clone().expect("the profile should have a UID");
    let previous_updated = item.updated;
    let file = item.file.clone().expect("the profile should have a file");
    set_current_profile(&store, &uid).expect("the profile should become current");

    let access = ProfileAccess::new(store.clone(), Arc::new(RejectValidator))
      .expect("profile access should build");
    let (event_tx, _event_rx) = mpsc::channel(4);
    let worker = ProfileWorker::new(access, Arc::new(NoopActivator), event_tx);
    let error = worker
      .update_remote(uid.clone())
      .await
      .expect_err("runtime validation should reject the update");
    server.await.expect("the HTTP server should finish");

    assert!(error.contains("previous subscription was restored"));
    assert_eq!(
      store
        .read_profile(&file)
        .expect("the restored profile should be readable")
        .as_bytes(),
      old_content
    );
    let catalog = store.load_catalog().expect("the catalog should load");
    assert_eq!(catalog.current.as_deref(), Some(uid.as_str()));
    assert_eq!(
      catalog.get(&uid).and_then(|item| item.updated),
      previous_updated
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
