use std::{collections::BTreeMap, path::PathBuf};

use serde_yaml_ng::Mapping;

use crate::{
  Error, ProfileCatalog, ProfileItem, ProfileKind, ProfileStore, Result,
  store::{RollbackJournal, atomic_write, read_bytes_if_exists, remove_file},
  to_yaml,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DraftState {
  Editing,
  Validated,
  Committed,
  RolledBack,
}

impl DraftState {
  const fn label(self) -> &'static str {
    match self {
      Self::Editing => "editing",
      Self::Validated => "validated",
      Self::Committed => "committed",
      Self::RolledBack => "rolled back",
    }
  }
}

#[derive(Clone, Debug)]
pub struct Draft<T> {
  original: T,
  working: T,
  state: DraftState,
}

impl<T> Draft<T>
where
  T: Clone,
{
  pub fn begin(value: T) -> Self {
    Self {
      original: value.clone(),
      working: value,
      state: DraftState::Editing,
    }
  }

  pub const fn state(&self) -> DraftState {
    self.state
  }

  pub const fn get(&self) -> &T {
    &self.working
  }

  pub fn edit<R>(&mut self, edit: impl FnOnce(&mut T) -> R) -> Result<R> {
    self.ensure_active()?;
    self.state = DraftState::Editing;
    Ok(edit(&mut self.working))
  }

  pub fn validate(&mut self, validator: impl FnOnce(&T) -> Result<()>) -> Result<()> {
    self.ensure_active()?;
    validator(&self.working)?;
    self.state = DraftState::Validated;
    Ok(())
  }

  pub fn commit(&mut self) -> Result<T> {
    if self.state != DraftState::Validated {
      return Err(Error::InvalidDraftState {
        expected: "validated",
        actual: self.state.label(),
      });
    }
    self.state = DraftState::Committed;
    Ok(self.working.clone())
  }

  pub fn rollback(&mut self) -> Result<T> {
    self.ensure_active()?;
    self.working = self.original.clone();
    self.state = DraftState::RolledBack;
    Ok(self.working.clone())
  }

  const fn ensure_active(&self) -> Result<()> {
    if matches!(self.state, DraftState::Editing | DraftState::Validated) {
      Ok(())
    } else {
      Err(Error::InvalidDraftState {
        expected: "editing or validated",
        actual: self.state.label(),
      })
    }
  }
}

#[derive(Debug)]
pub struct ProfileTransaction {
  store: ProfileStore,
  catalog: Draft<ProfileCatalog>,
  staged_files: BTreeMap<PathBuf, Option<Vec<u8>>>,
}

impl ProfileTransaction {
  pub(crate) fn begin(store: ProfileStore, catalog: ProfileCatalog) -> Result<Self> {
    validate_catalog(&store, &catalog)?;
    Ok(Self {
      store,
      catalog: Draft::begin(catalog),
      staged_files: BTreeMap::new(),
    })
  }

  pub const fn state(&self) -> DraftState {
    self.catalog.state()
  }

  pub const fn catalog(&self) -> &ProfileCatalog {
    self.catalog.get()
  }

  pub fn edit_catalog<R>(&mut self, edit: impl FnOnce(&mut ProfileCatalog) -> R) -> Result<R> {
    self.catalog.edit(edit)
  }

  pub fn stage_profile(&mut self, uid: &str, content: impl Into<Vec<u8>>) -> Result<()> {
    let item = self
      .catalog
      .get()
      .get(uid)
      .ok_or_else(|| Error::InvalidConfiguration(format!("profile {uid} does not exist")))?;
    let path = self.store.resolve_profile_path(item.require_file()?)?;
    self.staged_files.insert(path, Some(content.into()));
    self.catalog.edit(|_| ())?;
    Ok(())
  }

  pub fn add_profile(&mut self, item: ProfileItem, content: impl Into<Vec<u8>>) -> Result<()> {
    let uid = item.require_uid()?.to_string();
    let path = self.store.resolve_profile_path(item.require_file()?)?;
    if self.catalog.get().get(&uid).is_some() {
      return Err(Error::InvalidConfiguration(format!(
        "profile UID {uid} already exists"
      )));
    }
    self
      .catalog
      .edit(|catalog| catalog.items_mut().push(item))?;
    self.staged_files.insert(path, Some(content.into()));
    Ok(())
  }

  pub fn remove_profile(&mut self, uid: &str) -> Result<()> {
    let item = self
      .catalog
      .get()
      .get(uid)
      .cloned()
      .ok_or_else(|| Error::InvalidConfiguration(format!("profile {uid} does not exist")))?;
    let path = self.store.resolve_profile_path(item.require_file()?)?;
    self.catalog.edit(|catalog| {
      catalog
        .items_mut()
        .retain(|item| item.uid.as_deref() != Some(uid));
      if catalog.current.as_deref() == Some(uid) {
        catalog.current = None;
      }
    })?;
    self.staged_files.insert(path, None);
    Ok(())
  }

  pub fn validate(&mut self) -> Result<()> {
    let store = self.store.clone();
    let staged = self.staged_files.clone();
    self.catalog.validate(|catalog| {
      validate_catalog(&store, catalog)?;
      for (path, content) in &staged {
        if let Some(content) = content {
          validate_profile_content(path, content)?;
        }
      }
      Ok(())
    })
  }

  pub fn commit(mut self) -> Result<ProfileCatalog> {
    if self.catalog.state() != DraftState::Validated {
      return Err(Error::InvalidDraftState {
        expected: "validated",
        actual: self.catalog.state().label(),
      });
    }
    let catalog = self.catalog.get().clone();
    let mut writes = self.staged_files.clone();
    let catalog_yaml = to_yaml(&catalog)?;
    writes.insert(
      self.store.paths().profiles_catalog.clone(),
      Some(format!("# Profiles Config for rsclash\n{catalog_yaml}").into_bytes()),
    );
    let snapshots = capture_snapshots(writes.keys())?;
    let journal = RollbackJournal::create(&self.store.paths().root, &snapshots)?;

    if let Err(commit_error) = apply_writes(&writes, &self.store.paths().profiles_catalog) {
      if let Err(rollback_error) = restore_snapshots(&snapshots) {
        return Err(Error::CommitRollback {
          commit_error: commit_error.to_string(),
          rollback_error: rollback_error.to_string(),
        });
      }
      if let Err(rollback_error) = journal.complete() {
        return Err(Error::CommitRollback {
          commit_error: commit_error.to_string(),
          rollback_error: rollback_error.to_string(),
        });
      }
      return Err(commit_error);
    }

    if let Err(commit_error) = journal.complete() {
      if let Err(rollback_error) = restore_snapshots(&snapshots) {
        return Err(Error::CommitRollback {
          commit_error: commit_error.to_string(),
          rollback_error: rollback_error.to_string(),
        });
      }
      return Err(commit_error);
    }

    self.catalog.commit()
  }

  pub fn rollback(mut self) -> Result<ProfileCatalog> {
    self.staged_files.clear();
    self.catalog.rollback()
  }
}

fn validate_catalog(store: &ProfileStore, catalog: &ProfileCatalog) -> Result<()> {
  let mut seen = std::collections::BTreeSet::new();
  for item in catalog.items() {
    let uid = item.require_uid()?;
    if !seen.insert(uid) {
      return Err(Error::InvalidConfiguration(format!(
        "duplicate profile UID {uid}"
      )));
    }
    if let Some(file) = item.file.as_deref() {
      let _ = store.resolve_profile_path(file)?;
    } else if !matches!(item.kind, Some(ProfileKind::Unknown(_)) | None) {
      return Err(Error::InvalidConfiguration(format!(
        "profile {uid} has no file"
      )));
    }
  }
  if let Some(current) = catalog.current.as_deref()
    && catalog.get(current).is_none()
  {
    return Err(Error::InvalidConfiguration(format!(
      "current profile {current} does not exist"
    )));
  }
  Ok(())
}

fn validate_profile_content(path: &std::path::Path, content: &[u8]) -> Result<()> {
  if path.extension().and_then(|value| value.to_str()) == Some("js") {
    if content.is_empty() {
      return Err(Error::InvalidConfiguration(format!(
        "script profile {} is empty",
        path.display()
      )));
    }
    return Ok(());
  }
  let mapping: Mapping = serde_yaml_ng::from_slice(content).map_err(Error::DecodeYaml)?;
  if mapping.is_empty() {
    return Err(Error::InvalidConfiguration(format!(
      "YAML profile {} is empty",
      path.display()
    )));
  }
  Ok(())
}

fn capture_snapshots<'a>(
  paths: impl Iterator<Item = &'a PathBuf>,
) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>> {
  paths
    .map(|path| Ok((path.clone(), read_bytes_if_exists(path)?)))
    .collect()
}

fn apply_writes(
  writes: &BTreeMap<PathBuf, Option<Vec<u8>>>,
  catalog_path: &std::path::Path,
) -> Result<()> {
  for (path, content) in writes
    .iter()
    .filter(|(path, _)| path.as_path() != catalog_path)
  {
    match content {
      Some(content) => atomic_write(path, content)?,
      None => remove_file(path)?,
    }
  }
  #[cfg(test)]
  if should_inject_catalog_failure(catalog_path) {
    return Err(Error::InvalidConfiguration(
      "injected catalog commit failure".to_string(),
    ));
  }
  if let Some(content) = writes.get(catalog_path) {
    match content {
      Some(content) => atomic_write(catalog_path, content)?,
      None => remove_file(catalog_path)?,
    }
  }
  Ok(())
}

#[cfg(test)]
fn should_inject_catalog_failure(catalog_path: &std::path::Path) -> bool {
  let Ok(mut injected) = INJECTED_CATALOG_FAILURE.lock() else {
    return false;
  };
  if injected.as_deref() == Some(catalog_path) {
    injected.take();
    true
  } else {
    false
  }
}

#[cfg(test)]
static INJECTED_CATALOG_FAILURE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn restore_snapshots(snapshots: &BTreeMap<PathBuf, Option<Vec<u8>>>) -> Result<()> {
  for (path, content) in snapshots.iter().rev() {
    match content {
      Some(content) => atomic_write(path, content)?,
      None => remove_file(path)?,
    }
  }
  Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clear failures")]
mod tests {
  use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
  };

  use crate::{
    Draft, DraftState, ProfileCatalog, ProfileItem, ProfileKind, ProfileStore, from_yaml,
  };

  #[test]
  fn generic_draft_requires_validation_before_commit() {
    let mut draft = Draft::begin(1_u32);
    draft.edit(|value| *value = 2).expect("draft should edit");
    assert!(draft.commit().is_err());
    draft.validate(|_| Ok(())).expect("draft should validate");
    assert_eq!(draft.commit().expect("draft should commit"), 2);
    assert_eq!(draft.state(), DraftState::Committed);
  }

  #[test]
  fn profile_transaction_commits_catalog_and_content() {
    let directory = TestDirectory::new();
    let store = ProfileStore::open(&directory.path).expect("store should open");
    let mut transaction = store.begin().expect("transaction should begin");
    transaction
      .add_profile(local_profile("local"), "proxies: [DIRECT]\n")
      .expect("profile should stage");
    transaction
      .edit_catalog(|catalog| catalog.current = Some("local".to_string()))
      .expect("catalog should edit");
    transaction.validate().expect("transaction should validate");
    transaction.commit().expect("transaction should commit");

    let catalog = store.load_catalog().expect("catalog should load");
    assert_eq!(catalog.current.as_deref(), Some("local"));
    assert_eq!(
      store
        .read_profile("local.yaml")
        .expect("profile should load"),
      "proxies: [DIRECT]\n"
    );
    assert!(
      fs::read_dir(store.paths().profiles_dir.clone())
        .expect("profiles directory should be readable")
        .all(|entry| !entry
          .expect("entry should be readable")
          .file_name()
          .to_string_lossy()
          .ends_with(".tmp"))
    );
  }

  #[test]
  fn rollback_discards_staged_changes() {
    let directory = TestDirectory::new();
    let store = ProfileStore::open(&directory.path).expect("store should open");
    let mut transaction = store.begin().expect("transaction should begin");
    transaction
      .add_profile(local_profile("local"), "proxies: [DIRECT]\n")
      .expect("profile should stage");
    let catalog = transaction
      .rollback()
      .expect("transaction should roll back");

    assert!(catalog.items().is_empty());
    assert!(!store.paths().profiles_dir.join("local.yaml").exists());
  }

  #[test]
  fn profile_paths_cannot_escape_the_private_directory() {
    let directory = TestDirectory::new();
    let store = ProfileStore::open(&directory.path).expect("store should open");

    assert!(
      store
        .write_profile("../outside.yaml", "mode: rule")
        .is_err()
    );
    assert!(
      store
        .write_profile("nested/profile.yaml", "mode: rule")
        .is_err()
    );
    assert!(store.write_profile(".hidden.yaml", "mode: rule").is_err());
  }

  #[test]
  fn store_round_trip_keeps_catalog_extensions() {
    let directory = TestDirectory::new();
    let store = ProfileStore::open(&directory.path).expect("store should open");
    let catalog: ProfileCatalog = from_yaml(
      r"
future_catalog: keep
items: []
",
    )
    .expect("catalog should parse");
    store.save_catalog(&catalog).expect("catalog should save");

    let loaded = store.load_catalog().expect("catalog should load");
    assert!(loaded.unknown.contains_key("future_catalog"));
  }

  #[test]
  fn failed_catalog_commit_restores_previous_profile_content() {
    let directory = TestDirectory::new();
    let store = ProfileStore::open(&directory.path).expect("store should open");
    let mut initial = store.begin().expect("transaction should begin");
    initial
      .add_profile(local_profile("local"), "proxies: [DIRECT]\n")
      .expect("profile should stage");
    initial.validate().expect("transaction should validate");
    initial.commit().expect("initial transaction should commit");

    let mut update = store.begin().expect("transaction should begin");
    update
      .stage_profile("local", "proxies: [REJECT]\n")
      .expect("profile should stage");
    update.validate().expect("transaction should validate");
    *super::INJECTED_CATALOG_FAILURE
      .lock()
      .expect("failure injection should lock") = Some(store.paths().profiles_catalog.clone());
    assert!(update.commit().is_err());

    assert_eq!(
      store
        .read_profile("local.yaml")
        .expect("profile should load"),
      "proxies: [DIRECT]\n"
    );
    assert_eq!(
      store
        .load_catalog()
        .expect("catalog should load")
        .items()
        .len(),
      1
    );
  }

  fn local_profile(uid: &str) -> ProfileItem {
    ProfileItem {
      uid: Some(uid.to_string()),
      kind: Some(ProfileKind::Local),
      file: Some(format!("{uid}.yaml")),
      ..ProfileItem::default()
    }
  }

  struct TestDirectory {
    path: PathBuf,
  }

  impl TestDirectory {
    fn new() -> Self {
      static NEXT_ID: AtomicU64 = AtomicU64::new(0);
      let path = std::env::temp_dir().join(format!(
        "rsclash-config-{}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
      ));
      fs::create_dir_all(&path).expect("test directory should be created");
      Self { path }
    }
  }

  impl Drop for TestDirectory {
    fn drop(&mut self) {
      let _ = fs::remove_dir_all(&self.path);
    }
  }
}
