use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{Error, ProfileCatalog, Result, from_yaml, to_yaml, transaction::ProfileTransaction};

const CATALOG_HEADER: &str = "# Profiles Config for rsclash\n";
const JOURNAL_PREFIX: &str = ".rsclash-rollback-";
const JOURNAL_SUFFIX: &str = ".yaml";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigPaths {
    pub root: PathBuf,
    pub profiles_dir: PathBuf,
    pub backups_dir: PathBuf,
    pub profiles_catalog: PathBuf,
    pub verge_config: PathBuf,
    pub clash_config: PathBuf,
    pub dns_config: PathBuf,
    pub runtime_config: PathBuf,
    pub cvr_import_marker: PathBuf,
}

impl ConfigPaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            profiles_dir: root.join("profiles"),
            backups_dir: root.join("backups"),
            profiles_catalog: root.join("profiles.yaml"),
            verge_config: root.join("verge.yaml"),
            clash_config: root.join("clash.yaml"),
            dns_config: root.join("dns_config.yaml"),
            runtime_config: root.join("runtime.yaml"),
            cvr_import_marker: root.join(".cvr-imported-v1.yaml"),
            root,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProfileStore {
    paths: ConfigPaths,
}

impl ProfileStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let paths = ConfigPaths::new(root);
        create_private_directory(&paths.root)?;
        create_private_directory(&paths.profiles_dir)?;
        recover_pending_transactions(&paths.root)?;
        Ok(Self { paths })
    }

    pub fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    pub fn load_catalog(&self) -> Result<ProfileCatalog> {
        if !self.paths.profiles_catalog.exists() {
            return Ok(ProfileCatalog::default());
        }
        let source = read_file(&self.paths.profiles_catalog)?;
        from_yaml(&source)
    }

    pub fn save_catalog(&self, catalog: &ProfileCatalog) -> Result<()> {
        let yaml = to_yaml(catalog)?;
        atomic_write(
            &self.paths.profiles_catalog,
            format!("{CATALOG_HEADER}{yaml}").as_bytes(),
        )
    }

    pub fn read_profile(&self, file_name: &str) -> Result<String> {
        let path = self.resolve_profile_path(file_name)?;
        reject_symlink(&path)?;
        read_file(&path)
    }

    pub fn write_profile(&self, file_name: &str, content: &str) -> Result<()> {
        let path = self.resolve_profile_path(file_name)?;
        reject_symlink(&path)?;
        atomic_write(&path, content.as_bytes())
    }

    pub fn begin(&self) -> Result<ProfileTransaction> {
        ProfileTransaction::begin(self.clone(), self.load_catalog()?)
    }

    pub(crate) fn resolve_profile_path(&self, file_name: &str) -> Result<PathBuf> {
        validate_profile_file_name(file_name)?;
        Ok(self.paths.profiles_dir.join(file_name))
    }
}

pub(crate) fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        Error::InvalidConfiguration(format!("{} has no parent directory", path.display()))
    })?;
    create_private_directory(parent)?;
    reject_symlink(path)?;
    let temp_path = unique_temporary_path(path);
    let mut guard = TemporaryFileGuard::new(temp_path.clone());
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)
        .map_err(|source| Error::io("create temporary file", &temp_path, source))?;
    set_private_file_permissions(&file, &temp_path)?;
    file.write_all(content)
        .map_err(|source| Error::io("write temporary file", &temp_path, source))?;
    file.flush()
        .map_err(|source| Error::io("flush temporary file", &temp_path, source))?;
    file.sync_all()
        .map_err(|source| Error::io("sync temporary file", &temp_path, source))?;
    drop(file);
    fs::rename(&temp_path, path)
        .map_err(|source| Error::io("replace destination file", path, source))?;
    guard.persist();
    sync_directory(parent)?;
    Ok(())
}

pub(crate) fn create_staging_file(destination: &Path, content: &[u8]) -> Result<PathBuf> {
    let parent = destination.parent().ok_or_else(|| {
        Error::InvalidConfiguration(format!("{} has no parent directory", destination.display()))
    })?;
    create_private_directory(parent)?;
    reject_symlink(destination)?;
    let path = unique_temporary_path(destination);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .map_err(|source| Error::io("create staging file", &path, source))?;
    set_private_file_permissions(&file, &path)?;
    file.write_all(content)
        .map_err(|source| Error::io("write staging file", &path, source))?;
    file.flush()
        .map_err(|source| Error::io("flush staging file", &path, source))?;
    file.sync_all()
        .map_err(|source| Error::io("sync staging file", &path, source))?;
    drop(file);
    sync_directory(parent)?;
    Ok(path)
}

pub(crate) fn remove_file(path: &Path) -> Result<()> {
    reject_symlink(path)?;
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
            Ok(())
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::io("remove file", path, source)),
    }
}

pub(crate) fn read_bytes_if_exists(path: &Path) -> Result<Option<Vec<u8>>> {
    reject_symlink(path)?;
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::io("open file", path, source)),
    };
    let mut content = Vec::new();
    file.read_to_end(&mut content)
        .map_err(|source| Error::io("read file", path, source))?;
    Ok(Some(content))
}

#[derive(Debug, Serialize, Deserialize)]
struct JournalManifest {
    version: u8,
    entries: Vec<JournalEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct JournalEntry {
    relative_path: PathBuf,
    previous: Option<Vec<u8>>,
}

#[derive(Debug)]
pub(crate) struct RollbackJournal {
    path: PathBuf,
}

impl RollbackJournal {
    pub(crate) fn create(
        root: &Path,
        snapshots: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    ) -> Result<Self> {
        create_private_directory(root)?;
        let entries = snapshots
            .iter()
            .map(|(path, previous)| {
                let relative_path = path.strip_prefix(root).map_err(|_| {
                    Error::InvalidConfiguration(format!(
                        "transaction path {} is outside {}",
                        path.display(),
                        root.display()
                    ))
                })?;
                validate_relative_journal_path(relative_path)?;
                Ok(JournalEntry {
                    relative_path: relative_path.to_path_buf(),
                    previous: previous.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let manifest = JournalManifest {
            version: 1,
            entries,
        };
        let content = to_yaml(&manifest)?;
        let path = unique_journal_path(root);
        atomic_write(&path, content.as_bytes())?;
        Ok(Self { path })
    }

    pub(crate) fn complete(&self) -> Result<()> {
        remove_file(&self.path)
    }
}

pub(crate) fn recover_pending_transactions(root: &Path) -> Result<()> {
    create_private_directory(root)?;
    let mut journals = Vec::new();
    for entry in fs::read_dir(root)
        .map_err(|source| Error::io("read transaction directory", root, source))?
    {
        let entry = entry.map_err(|source| Error::io("read transaction entry", root, source))?;
        let path = entry.path();
        if is_journal_path(&path) {
            journals.push(path);
        }
    }
    journals.sort();
    for journal in journals {
        recover_journal(root, &journal)?;
    }
    Ok(())
}

fn recover_journal(root: &Path, journal: &Path) -> Result<()> {
    reject_symlink(journal)?;
    let source = read_file(journal)?;
    let manifest: JournalManifest = from_yaml(&source)?;
    if manifest.version != 1 {
        return Err(Error::InvalidConfiguration(format!(
            "unsupported rollback journal version {}",
            manifest.version
        )));
    }
    for entry in manifest.entries.iter().rev() {
        validate_relative_journal_path(&entry.relative_path)?;
        let path = root.join(&entry.relative_path);
        match &entry.previous {
            Some(previous) => atomic_write(&path, previous)?,
            None => remove_file(&path)?,
        }
    }
    remove_file(journal)
}

fn validate_relative_journal_path(path: &Path) -> Result<()> {
    let valid = !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidConfiguration(format!(
            "invalid rollback journal path {}",
            path.display()
        )))
    }
}

fn is_journal_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(JOURNAL_PREFIX) && name.ends_with(JOURNAL_SUFFIX))
}

fn unique_journal_path(root: &Path) -> PathBuf {
    static NEXT_JOURNAL_ID: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_JOURNAL_ID.fetch_add(1, Ordering::Relaxed);
    root.join(format!(
        "{JOURNAL_PREFIX}{}-{id}{JOURNAL_SUFFIX}",
        std::process::id()
    ))
}

fn read_file(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|source| Error::io("read file", path, source))
}

fn validate_profile_file_name(file_name: &str) -> Result<()> {
    let path = Path::new(file_name);
    let mut components = path.components();
    let valid_component = matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && !file_name.is_empty()
        && !file_name.starts_with('.');
    let valid_extension = matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("yaml" | "yml" | "js")
    );
    if !valid_component || !valid_extension {
        return Err(Error::InvalidProfilePath(file_name.to_string()));
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(Error::InvalidProfilePath(path.display().to_string()))
        }
        Ok(_) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::io("inspect file", path, source)),
    }
}

fn unique_temporary_path(destination: &Path) -> PathBuf {
    static NEXT_TEMPORARY_ID: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_TEMPORARY_ID.fetch_add(1, Ordering::Relaxed);
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("config");
    destination.with_file_name(format!(".{name}.rsclash-{}-{id}.tmp", std::process::id()))
}

pub(crate) fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|source| Error::io("create directory", path, source))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|source| Error::io("restrict directory permissions", path, source))?;
    }
    Ok(())
}

fn set_private_file_permissions(file: &File, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| Error::io("restrict file permissions", path, source))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (file, path);
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let directory =
            File::open(path).map_err(|source| Error::io("open directory", path, source))?;
        directory
            .sync_all()
            .map_err(|source| Error::io("sync directory", path, source))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

struct TemporaryFileGuard {
    path: PathBuf,
    persisted: bool,
}

impl TemporaryFileGuard {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            persisted: false,
        }
    }

    fn persist(&mut self) {
        self.persisted = true;
    }
}

impl Drop for TemporaryFileGuard {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{ProfileStore, RollbackJournal, atomic_write};

    #[test]
    fn opening_store_recovers_an_unfinished_file_transaction() {
        let directory = TestDirectory::new();
        let store = ProfileStore::open(&directory.path).expect("store should open");
        let target = store.paths().verge_config.clone();
        atomic_write(&target, b"theme_mode: light\n").expect("old config should write");
        let snapshots = BTreeMap::from([(target.clone(), Some(b"theme_mode: light\n".to_vec()))]);
        let _unfinished = RollbackJournal::create(&directory.path, &snapshots)
            .expect("journal should be durable");
        atomic_write(&target, b"theme_mode: dark\n").expect("new config should write");

        ProfileStore::open(&directory.path).expect("reopened store should recover");

        assert_eq!(
            fs::read_to_string(&target).expect("config should be readable"),
            "theme_mode: light\n"
        );
        assert!(
            fs::read_dir(&directory.path)
                .expect("directory should be readable")
                .all(|entry| !entry
                    .expect("entry should be readable")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".rsclash-rollback-"))
        );
    }

    #[test]
    fn recovery_rejects_paths_outside_the_config_root() {
        let directory = TestDirectory::new();
        ProfileStore::open(&directory.path).expect("store should open");
        let outside = directory.path.with_extension("outside.yaml");
        fs::write(&outside, "safe: true\n").expect("outside file should write");
        fs::write(
            directory.path.join(".rsclash-rollback-tampered.yaml"),
            "version: 1\nentries:\n- relative_path: ../rsclash.outside.yaml\n  previous: null\n",
        )
        .expect("tampered journal should write");

        assert!(ProfileStore::open(&directory.path).is_err());
        assert_eq!(
            fs::read_to_string(&outside).expect("outside file should remain readable"),
            "safe: true\n"
        );
        fs::remove_file(outside).expect("outside file should be removed");
    }

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("rsclash-journal-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.path);
        }
    }
}
