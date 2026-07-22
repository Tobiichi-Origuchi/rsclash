use std::{
  collections::{BTreeMap, BTreeSet},
  fs,
  path::{Component, Path, PathBuf},
  sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};
use serde_yaml_ng::Mapping;

use crate::{
  Error, MihomoConfig, ProfileCatalog, ProfileStore, Result, VergeConfig,
  store::{
    RollbackJournal, atomic_write, create_private_directory, read_bytes_if_exists, remove_file,
  },
  to_yaml,
};

const CVR_CLASH_CONFIG: &str = "config.yaml";
const CVR_VERGE_CONFIG: &str = "verge.yaml";
const CVR_PROFILES_CATALOG: &str = "profiles.yaml";
const CVR_PROFILES_DIRECTORY: &str = "profiles";
const CVR_DNS_CONFIG: &str = "dns_config.yaml";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CvrImportOutcome {
  Imported(CvrImportReport),
  AlreadyImported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CvrImportReport {
  pub copied_files: usize,
  pub backup_directory: PathBuf,
}

#[derive(Clone, Debug)]
pub struct CvrImporter {
  source_root: PathBuf,
  target_root: PathBuf,
}

impl CvrImporter {
  #[must_use]
  pub fn new(source_root: impl Into<PathBuf>, target_root: impl Into<PathBuf>) -> Self {
    Self {
      source_root: source_root.into(),
      target_root: target_root.into(),
    }
  }

  pub fn import(&self) -> Result<CvrImportOutcome> {
    ensure_separate_roots(&self.source_root, &self.target_root)?;
    let marker = self.target_root.join(".cvr-imported-v1.yaml");
    if read_bytes_if_exists(&marker)?.is_some() {
      return Ok(CvrImportOutcome::AlreadyImported);
    }

    let bundle = ImportBundle::load(&self.source_root)?;
    let store = ProfileStore::open(&self.target_root)?;
    ensure_separate_roots(&self.source_root, &store.paths().root)?;
    if read_bytes_if_exists(&store.paths().cvr_import_marker)?.is_some() {
      return Ok(CvrImportOutcome::AlreadyImported);
    }

    let writes = bundle.destination_writes(&store)?;
    let snapshots = capture_snapshots(writes.keys())?;
    let backup_directory = create_backup(&store, &snapshots)?;
    make_backup_read_only(&backup_directory)?;
    let receipt = ImportReceipt {
      version: 1,
      source_root: fs::canonicalize(&self.source_root)
        .map_err(|source| Error::io("canonicalize CVR source", &self.source_root, source))?,
      backup_directory: backup_directory.clone(),
    };
    let mut writes = writes;
    writes.insert(
      store.paths().cvr_import_marker.clone(),
      to_yaml(&receipt)?.into_bytes(),
    );
    let mut snapshots = snapshots;
    snapshots.insert(
      store.paths().cvr_import_marker.clone(),
      read_bytes_if_exists(&store.paths().cvr_import_marker)?,
    );
    let journal = RollbackJournal::create(&store.paths().root, &snapshots)?;

    if let Err(commit_error) = apply_import_writes(&writes, &store.paths().cvr_import_marker) {
      return rollback_import(&snapshots, &journal, commit_error);
    }
    if let Err(commit_error) = journal.complete() {
      return rollback_import(&snapshots, &journal, commit_error);
    }
    Ok(CvrImportOutcome::Imported(CvrImportReport {
      copied_files: writes.len().saturating_sub(1),
      backup_directory,
    }))
  }
}

#[derive(Serialize, Deserialize)]
struct ImportReceipt {
  version: u8,
  source_root: PathBuf,
  backup_directory: PathBuf,
}

struct ImportBundle {
  clash: Vec<u8>,
  verge: Vec<u8>,
  catalog: Vec<u8>,
  dns: Option<Vec<u8>>,
  profiles: BTreeMap<String, Vec<u8>>,
}

impl ImportBundle {
  fn load(source_root: &Path) -> Result<Self> {
    let metadata = fs::symlink_metadata(source_root)
      .map_err(|source| Error::io("inspect CVR source", source_root, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
      return Err(Error::InvalidConfiguration(format!(
        "CVR source is not a regular directory: {}",
        source_root.display()
      )));
    }

    let clash = read_regular_source_file(&source_root.join(CVR_CLASH_CONFIG))?;
    let verge = read_regular_source_file(&source_root.join(CVR_VERGE_CONFIG))?;
    let catalog = read_regular_source_file(&source_root.join(CVR_PROFILES_CATALOG))?;
    validate_yaml::<MihomoConfig>(&clash, CVR_CLASH_CONFIG)?;
    validate_yaml::<VergeConfig>(&verge, CVR_VERGE_CONFIG)?;
    let parsed_catalog = validate_yaml::<ProfileCatalog>(&catalog, CVR_PROFILES_CATALOG)?;

    let profiles_root = source_root.join(CVR_PROFILES_DIRECTORY);
    let metadata = fs::symlink_metadata(&profiles_root)
      .map_err(|source| Error::io("inspect CVR profiles directory", &profiles_root, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
      return Err(Error::InvalidConfiguration(format!(
        "CVR profiles path is not a regular directory: {}",
        profiles_root.display()
      )));
    }
    let mut referenced = BTreeSet::new();
    let mut profiles = BTreeMap::new();
    for item in parsed_catalog.items() {
      let Some(file_name) = item.file.as_deref() else {
        continue;
      };
      validate_profile_name(file_name)?;
      if !referenced.insert(file_name.to_string()) {
        continue;
      }
      let path = profiles_root.join(file_name);
      let content = read_regular_source_file(&path)?;
      validate_import_profile(&path, &content, item.is_source())?;
      profiles.insert(file_name.to_string(), content);
    }

    let dns_path = source_root.join(CVR_DNS_CONFIG);
    let dns = match fs::symlink_metadata(&dns_path) {
      Ok(_) => {
        let content = read_regular_source_file(&dns_path)?;
        validate_yaml::<Mapping>(&content, CVR_DNS_CONFIG)?;
        Some(content)
      },
      Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
      Err(source) => return Err(Error::io("inspect CVR DNS config", dns_path, source)),
    };

    Ok(Self {
      clash,
      verge,
      catalog,
      dns,
      profiles,
    })
  }

  fn destination_writes(&self, store: &ProfileStore) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut writes = BTreeMap::from([
      (store.paths().clash_config.clone(), self.clash.clone()),
      (store.paths().verge_config.clone(), self.verge.clone()),
      (store.paths().profiles_catalog.clone(), self.catalog.clone()),
    ]);
    if let Some(dns) = &self.dns {
      writes.insert(store.paths().dns_config.clone(), dns.clone());
    }
    for (file_name, content) in &self.profiles {
      writes.insert(store.resolve_profile_path(file_name)?, content.clone());
    }
    Ok(writes)
  }
}

fn validate_yaml<T>(content: &[u8], name: &str) -> Result<T>
where
  T: serde::de::DeserializeOwned,
{
  serde_yaml_ng::from_slice(content)
    .map_err(|error| Error::InvalidConfiguration(format!("invalid CVR {name}: {error}")))
}

fn validate_profile_name(file_name: &str) -> Result<()> {
  let path = Path::new(file_name);
  let mut components = path.components();
  let valid = matches!(components.next(), Some(Component::Normal(_)))
    && components.next().is_none()
    && !file_name.starts_with('.')
    && matches!(
      path.extension().and_then(|extension| extension.to_str()),
      Some("yaml" | "yml" | "js")
    );
  if valid {
    Ok(())
  } else {
    Err(Error::InvalidProfilePath(file_name.to_string()))
  }
}

fn validate_import_profile(path: &Path, content: &[u8], source_profile: bool) -> Result<()> {
  if path.extension().and_then(|extension| extension.to_str()) == Some("js") {
    if content.is_empty() {
      return Err(Error::InvalidConfiguration(format!(
        "CVR script profile {} is empty",
        path.display()
      )));
    }
    return Ok(());
  }
  let mapping: Mapping = serde_yaml_ng::from_slice(content).map_err(Error::DecodeYaml)?;
  if source_profile && mapping.is_empty() {
    return Err(Error::InvalidConfiguration(format!(
      "CVR source profile {} is empty",
      path.display()
    )));
  }
  Ok(())
}

fn read_regular_source_file(path: &Path) -> Result<Vec<u8>> {
  let metadata = fs::symlink_metadata(path)
    .map_err(|source| Error::io("inspect CVR source file", path, source))?;
  if metadata.file_type().is_symlink() || !metadata.is_file() {
    return Err(Error::InvalidConfiguration(format!(
      "CVR source is not a regular file: {}",
      path.display()
    )));
  }
  fs::read(path).map_err(|source| Error::io("read CVR source file", path, source))
}

fn ensure_separate_roots(source: &Path, target: &Path) -> Result<()> {
  let source = absolute_path(source)?;
  let target = absolute_path(target)?;
  if source == target || source.starts_with(&target) || target.starts_with(&source) {
    return Err(Error::InvalidConfiguration(format!(
      "CVR source and rsclash target must be separate directories: {} and {}",
      source.display(),
      target.display()
    )));
  }
  Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
  if path.exists() {
    return fs::canonicalize(path)
      .map_err(|source| Error::io("canonicalize directory", path, source));
  }
  if path.is_absolute() {
    Ok(path.to_path_buf())
  } else {
    std::env::current_dir()
      .map(|directory| directory.join(path))
      .map_err(|source| Error::io("read current directory", path, source))
  }
}

fn capture_snapshots<'a>(
  paths: impl Iterator<Item = &'a PathBuf>,
) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>> {
  paths
    .map(|path| Ok((path.clone(), read_bytes_if_exists(path)?)))
    .collect()
}

#[derive(Serialize)]
struct BackupManifest {
  version: u8,
  entries: Vec<BackupEntry>,
}

#[derive(Serialize)]
struct BackupEntry {
  relative_path: PathBuf,
  existed: bool,
}

fn create_backup(
  store: &ProfileStore,
  snapshots: &BTreeMap<PathBuf, Option<Vec<u8>>>,
) -> Result<PathBuf> {
  create_private_directory(&store.paths().backups_dir)?;
  let backup = unique_backup_path(&store.paths().backups_dir);
  create_private_directory(&backup)?;
  let mut entries = Vec::new();
  for (path, content) in snapshots {
    let relative = path.strip_prefix(&store.paths().root).map_err(|_| {
      Error::InvalidConfiguration(format!(
        "backup path {} is outside target root",
        path.display()
      ))
    })?;
    entries.push(BackupEntry {
      relative_path: relative.to_path_buf(),
      existed: content.is_some(),
    });
    if let Some(content) = content {
      let destination = backup.join(relative);
      if let Some(parent) = destination.parent() {
        create_private_directory(parent)?;
      }
      atomic_write(&destination, content)?;
    }
  }
  let manifest = BackupManifest {
    version: 1,
    entries,
  };
  atomic_write(
    &backup.join("manifest.yaml"),
    to_yaml(&manifest)?.as_bytes(),
  )?;
  Ok(backup)
}

fn unique_backup_path(root: &Path) -> PathBuf {
  static NEXT_BACKUP_ID: AtomicU64 = AtomicU64::new(0);
  let id = NEXT_BACKUP_ID.fetch_add(1, Ordering::Relaxed);
  root.join(format!("cvr-import-{}-{id}", std::process::id()))
}

fn apply_import_writes(writes: &BTreeMap<PathBuf, Vec<u8>>, marker: &Path) -> Result<()> {
  for (path, content) in writes.iter().filter(|(path, _)| path.as_path() != marker) {
    atomic_write(path, content)?;
  }
  #[cfg(test)]
  if should_inject_marker_failure(marker) {
    return Err(Error::InvalidConfiguration(
      "injected CVR import marker failure".to_string(),
    ));
  }
  let marker_content = writes
    .get(marker)
    .ok_or_else(|| Error::InvalidConfiguration("CVR import marker was not prepared".to_string()))?;
  atomic_write(marker, marker_content)
}

#[cfg(test)]
fn should_inject_marker_failure(marker: &Path) -> bool {
  let Ok(mut injected) = INJECTED_MARKER_FAILURE.lock() else {
    return false;
  };
  if injected.as_deref() == Some(marker) {
    injected.take();
    true
  } else {
    false
  }
}

#[cfg(test)]
static INJECTED_MARKER_FAILURE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn rollback_import(
  snapshots: &BTreeMap<PathBuf, Option<Vec<u8>>>,
  journal: &RollbackJournal,
  commit_error: Error,
) -> Result<CvrImportOutcome> {
  for (path, content) in snapshots.iter().rev() {
    let result = match content {
      Some(content) => atomic_write(path, content),
      None => remove_file(path),
    };
    if let Err(rollback_error) = result {
      return Err(Error::CommitRollback {
        commit_error: commit_error.to_string(),
        rollback_error: rollback_error.to_string(),
      });
    }
  }
  if let Err(rollback_error) = journal.complete() {
    return Err(Error::CommitRollback {
      commit_error: commit_error.to_string(),
      rollback_error: rollback_error.to_string(),
    });
  }
  Err(commit_error)
}

fn make_backup_read_only(path: &Path) -> Result<()> {
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;

    for entry in
      fs::read_dir(path).map_err(|source| Error::io("read import backup", path, source))?
    {
      let entry = entry.map_err(|source| Error::io("read import backup entry", path, source))?;
      let entry_path = entry.path();
      if entry
        .file_type()
        .map_err(|source| Error::io("inspect import backup entry", &entry_path, source))?
        .is_dir()
      {
        make_backup_read_only(&entry_path)?;
      } else {
        fs::set_permissions(&entry_path, fs::Permissions::from_mode(0o400))
          .map_err(|source| Error::io("make import backup read-only", &entry_path, source))?;
      }
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o500))
      .map_err(|source| Error::io("make import backup directory read-only", path, source))?;
  }
  #[cfg(not(unix))]
  {
    let _ = path;
  }
  Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use std::{fs, path::PathBuf};

  use super::{CvrImportOutcome, CvrImporter};

  #[test]
  fn imports_valid_cvr_data_once_without_linking_source() {
    let source = TestDirectory::new("cvr-source");
    let target = TestDirectory::new("cvr-target");
    write_valid_source(&source.path);
    fs::write(target.path.join("verge.yaml"), "theme_mode: light\n")
      .expect("old target should write");

    let outcome = CvrImporter::new(&source.path, &target.path)
      .import()
      .expect("import should succeed");
    let CvrImportOutcome::Imported(report) = outcome else {
      panic!("first import should copy files");
    };
    assert_eq!(report.copied_files, 5);
    assert_eq!(
      fs::read_to_string(target.path.join("verge.yaml")).expect("imported verge should read"),
      "theme_mode: dark\n"
    );
    assert_eq!(
      fs::read_to_string(report.backup_directory.join("verge.yaml")).expect("backup should read"),
      "theme_mode: light\n"
    );
    fs::write(source.path.join("profiles/local.yaml"), "mode: global\n")
      .expect("source should remain independently writable");
    assert_eq!(
      fs::read_to_string(target.path.join("profiles/local.yaml"))
        .expect("imported profile should read"),
      "mode: rule\n"
    );
    assert_eq!(
      CvrImporter::new(&source.path, &target.path)
        .import()
        .expect("second import should be handled"),
      CvrImportOutcome::AlreadyImported
    );
  }

  #[cfg(unix)]
  #[test]
  fn rejects_symlinked_source_profile_without_modifying_target() {
    use std::os::unix::fs::symlink;

    let source = TestDirectory::new("cvr-symlink-source");
    let target = TestDirectory::new("cvr-symlink-target");
    write_valid_source(&source.path);
    fs::remove_file(source.path.join("profiles/local.yaml")).expect("profile should be removed");
    symlink(
      source.path.join("config.yaml"),
      source.path.join("profiles/local.yaml"),
    )
    .expect("profile symlink should be created");

    assert!(
      CvrImporter::new(&source.path, &target.path)
        .import()
        .is_err()
    );
    assert!(!target.path.join("profiles.yaml").exists());
  }

  #[test]
  fn rejects_overlapping_source_and_target_roots() {
    let source = TestDirectory::new("cvr-overlap");
    write_valid_source(&source.path);
    assert!(
      CvrImporter::new(&source.path, source.path.join("rsclash"))
        .import()
        .is_err()
    );
  }

  #[test]
  fn failed_import_restores_every_target_file() {
    let source = TestDirectory::new("cvr-rollback-source");
    let target = TestDirectory::new("cvr-rollback-target");
    write_valid_source(&source.path);
    fs::write(target.path.join("verge.yaml"), "theme_mode: light\n")
      .expect("old target should write");
    *super::INJECTED_MARKER_FAILURE
      .lock()
      .expect("failure injection should lock") = Some(target.path.join(".cvr-imported-v1.yaml"));

    assert!(
      CvrImporter::new(&source.path, &target.path)
        .import()
        .is_err()
    );
    assert_eq!(
      fs::read_to_string(target.path.join("verge.yaml"))
        .expect("old target should remain readable"),
      "theme_mode: light\n"
    );
    assert!(!target.path.join("clash.yaml").exists());
    assert!(!target.path.join("profiles.yaml").exists());
    assert!(!target.path.join(".cvr-imported-v1.yaml").exists());
  }

  fn write_valid_source(root: &std::path::Path) {
    fs::create_dir_all(root.join("profiles")).expect("profiles directory should create");
    fs::write(root.join("config.yaml"), "mixed-port: 7890\n").expect("clash config should write");
    fs::write(root.join("verge.yaml"), "theme_mode: dark\n").expect("verge config should write");
    fs::write(
      root.join("profiles.yaml"),
      "current: local\nitems:\n- uid: local\n  type: local\n  file: local.yaml\n",
    )
    .expect("catalog should write");
    fs::write(root.join("profiles/local.yaml"), "mode: rule\n").expect("profile should write");
    fs::write(root.join("dns_config.yaml"), "dns: {enable: true}\n")
      .expect("DNS config should write");
  }

  struct TestDirectory {
    path: PathBuf,
  }

  impl TestDirectory {
    fn new(label: &str) -> Self {
      static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
      let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
      let path = std::env::temp_dir().join(format!("rsclash-{label}-{}-{id}", std::process::id()));
      fs::create_dir_all(&path).expect("test directory should create");
      Self { path }
    }
  }

  impl Drop for TestDirectory {
    fn drop(&mut self) {
      #[cfg(unix)]
      {
        use std::os::unix::fs::PermissionsExt;
        let _ignored = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o700));
        let backup_root = self.path.join("backups");
        if let Ok(entries) = fs::read_dir(&backup_root) {
          for entry in entries.flatten() {
            let _ignored = make_tree_writable(&entry.path());
          }
        }
      }
      let _ignored = fs::remove_dir_all(&self.path);
    }
  }

  #[cfg(unix)]
  fn make_tree_writable(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    if path.is_dir() {
      for entry in fs::read_dir(path)? {
        make_tree_writable(&entry?.path())?;
      }
    }
    Ok(())
  }
}
