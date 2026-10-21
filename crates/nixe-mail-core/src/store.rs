//! Raw messages as files, named by their SHA-256, readable by this user only.
//! The same message in two mailboxes (Gmail's labels) is stored once.

use sha2::{Digest, Sha256};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Saves `raw` and returns its name (the hex SHA-256).
    pub fn put(&self, raw: &[u8]) -> std::io::Result<String> {
        let name = hash(raw);
        let path = self.path(&name);
        if path.exists() {
            return Ok(name);
        }
        let dir = path.parent().expect("store paths have a parent");
        std::fs::create_dir_all(dir)?;
        for d in [&self.root, &dir.to_path_buf()] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
        }
        let tmp = dir.join(format!(".{name}.tmp"));
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        file.write_all(raw)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        Ok(name)
    }

    pub fn get(&self, name: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(self.path(name))
    }

    pub fn has(&self, name: &str) -> bool {
        self.path(name).exists()
    }

    pub fn remove(&self, name: &str) -> std::io::Result<()> {
        match std::fs::remove_file(self.path(name)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        let safe: String = name.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        let prefix = safe.get(..2).unwrap_or("00");
        self.root.join(prefix).join(format!("{safe}.eml"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

pub fn hash(raw: &[u8]) -> String {
    Sha256::digest(raw).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_once_privately() {
        let root = std::env::temp_dir().join(format!("nixe-mail-store-{}", std::process::id()));
        let store = Store::new(&root);
        let a = store.put(b"Subject: hi\r\n\r\nbody").unwrap();
        let b = store.put(b"Subject: hi\r\n\r\nbody").unwrap();
        assert_eq!(a, b);
        assert_eq!(store.get(&a).unwrap(), b"Subject: hi\r\n\r\nbody");
        let mode = std::fs::metadata(store.path(&a)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        // Names can't climb out of the store.
        assert!(store.path("../../etc/passwd").starts_with(&root));
        store.remove(&a).unwrap();
        assert!(!store.has(&a));
        store.remove(&a).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
