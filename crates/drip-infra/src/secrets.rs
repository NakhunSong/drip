//! A file-backed secret store: a TOML map at `~/.drip/secrets.toml` with `0600`
//! permissions. Cross-platform and testable, with no native dependency. The same
//! [`drip_domain::SecretStore`] port can be backed by the OS keychain later without any
//! change to callers. Keys use underscores (no dots) so they stay flat TOML keys.

use drip_domain::{DomainError, Result, SecretStore};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Stores secrets as a flat TOML key/value map on disk.
#[derive(Debug, Clone)]
pub struct FileSecretStore {
    path: PathBuf,
}

impl FileSecretStore {
    pub fn new(path: PathBuf) -> FileSecretStore {
        FileSecretStore { path }
    }

    fn load(&self) -> Result<BTreeMap<String, String>> {
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }
        let text = std::fs::read_to_string(&self.path).map_err(io_err)?;
        toml::from_str(&text).map_err(|e| DomainError::Secret(format!("parse secrets: {e}")))
    }

    fn store(&self, map: &BTreeMap<String, String>) -> Result<()> {
        let text = toml::to_string(map)
            .map_err(|e| DomainError::Secret(format!("serialize secrets: {e}")))?;
        std::fs::write(&self.path, text).map_err(io_err)?;
        restrict(&self.path)
    }
}

impl SecretStore for FileSecretStore {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.load()?.get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        let mut map = self.load()?;
        map.insert(key.to_string(), value.to_string());
        self.store(&map)
    }
    fn delete(&self, key: &str) -> Result<()> {
        let mut map = self.load()?;
        map.remove(key);
        self.store(&map)
    }
}

fn io_err(e: std::io::Error) -> DomainError {
    DomainError::Secret(e.to_string())
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io_err)
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSecretStore::new(dir.path().join("secrets.toml"));
        assert_eq!(store.get("kis_app_key").unwrap(), None);
        store.set("kis_app_key", "abc").unwrap();
        assert_eq!(store.get("kis_app_key").unwrap().as_deref(), Some("abc"));
        store.delete("kis_app_key").unwrap();
        assert_eq!(store.get("kis_app_key").unwrap(), None);
    }
}
