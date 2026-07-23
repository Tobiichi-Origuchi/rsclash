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

use reqwest::{Client, Url, redirect::Policy};
use rsclash_config::{
  ApplicationLayer, EnhancementInput, EnhancementPipeline, ListenerPolicy, MihomoConfig,
  NativeTransform, ProfileItem, ProfileKind, ProfileStore, RuntimeActivator, RuntimeDeployer,
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
  Import(ProfileImportCommand),
  Activate { uid: String },
  Mutate(ProfileMutationCommand),
  Update(ProfileUpdateCommand),
}

#[derive(Clone, Debug)]
pub(crate) enum ProfileImportCommand {
  Local { name: String, path: String },
  Remote { name: String, url: String },
}

#[derive(Clone, Debug)]
pub(crate) enum ProfileMutationCommand {
  Rename { uid: String, name: String },
  Duplicate { uid: String },
  Delete { uids: Vec<String> },
  Reorder { uid: String, new_index: usize },
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
    while let Some(command) = command_rx.recv().await {
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
  }

  async fn import(&self, command: ProfileImportCommand) -> Result<(), String> {
    match command {
      ProfileImportCommand::Local { name, path } => {
        let store = self.access.store.clone();
        spawn_blocking(move || import_local(&store, &name, Path::new(&path)))
          .await
          .map_err(|error| format!("local profile import task failed: {error}"))?
      },
      ProfileImportCommand::Remote { name, url } => self.import_remote(name, url).await,
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
    })
    .await
    .map_err(|error| format!("profile mutation task failed: {error}"))?
  }

  async fn import_remote(&self, name: String, url: String) -> Result<(), String> {
    validate_profile_name(&name)?;
    let content = self.download_remote(&url).await?;
    let store = self.access.store.clone();
    spawn_blocking(move || import_content(&store, &name, ProfileKind::Remote, Some(url), content))
      .await
      .map_err(|error| format!("remote profile import task failed: {error}"))?
  }

  async fn download_remote(&self, url: &str) -> Result<Vec<u8>, String> {
    let parsed = Url::parse(url).map_err(|error| format!("invalid subscription URL: {error}"))?;
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
    Ok(content)
  }

  async fn update_remote(&self, uid: String) -> Result<(), String> {
    let store = self.access.store.clone();
    let lookup_uid = uid.clone();
    let profile = spawn_blocking(move || remote_profile(&store, &lookup_uid))
      .await
      .map_err(|error| format!("profile lookup task failed: {error}"))??;
    let content = self.download_remote(&profile.url).await?;
    let store = self.access.store.clone();
    let replace_uid = uid.clone();
    let rollback = spawn_blocking(move || replace_profile(&store, &replace_uid, content))
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
  validate_source_content(&content)?;
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

struct RemoteProfile {
  url: String,
  active: bool,
}

struct ProfileRollback {
  content: Vec<u8>,
  updated_at: Option<u64>,
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

fn replace_profile(
  store: &ProfileStore,
  uid: &str,
  content: Vec<u8>,
) -> Result<ProfileRollback, String> {
  validate_source_content(&content)?;
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
  };
  let mut transaction = store.begin().map_err(|error| error.to_string())?;
  transaction
    .edit_catalog(|catalog| {
      if let Some(item) = catalog
        .items_mut()
        .iter_mut()
        .find(|item| item.uid.as_deref() == Some(uid))
      {
        item.updated = Some(unix_seconds());
      }
    })
    .map_err(|error| error.to_string())?;
  transaction
    .stage_profile(uid, content)
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
  let runtime = EnhancementPipeline::enhance(EnhancementInput {
    current,
    application: ApplicationLayer {
      defaults,
      listeners,
      platform: TargetPlatform::current(),
      enable_tun,
      native_transforms: NativeTransform::compatibility_defaults().to_vec(),
      ..ApplicationLayer::default()
    },
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
    ProfileAccess, ProfileWorker, delete_profiles, duplicate_profile, import_content, import_local,
    load_snapshot, prepare_activation, rename_profile, reorder_profile, set_current_profile,
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
      content.clone(),
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
      old_content.to_vec(),
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
