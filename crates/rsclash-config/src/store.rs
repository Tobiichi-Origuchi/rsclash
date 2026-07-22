use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{Error, ProfileCatalog, Result, from_yaml, to_yaml, transaction::ProfileTransaction};

const CATALOG_HEADER: &str = "# Profiles Config for rsclash\n";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigPaths {
    pub root: PathBuf,
    pub profiles_dir: PathBuf,
    pub profiles_catalog: PathBuf,
    pub verge_config: PathBuf,
    pub clash_config: PathBuf,
    pub runtime_config: PathBuf,
}

impl ConfigPaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            profiles_dir: root.join("profiles"),
            profiles_catalog: root.join("profiles.yaml"),
            verge_config: root.join("verge.yaml"),
            clash_config: root.join("clash.yaml"),
            runtime_config: root.join("runtime.yaml"),
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

fn create_private_directory(path: &Path) -> Result<()> {
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
