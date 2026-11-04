//! Content-addressed blob store: `blobs/<aa>/<bb>/<blake3-hex>`.
//!
//! Write order is fixed by: temp file -> fsync -> rename -> (the
//! caller then inserts the `blobs` row in the same transaction as the page
//! update). A crash therefore leaves at most an unreferenced file, never a row
//! without bytes.

use std::path::{Path, PathBuf};

use crate::core::error::{Result, WidowError};
use crate::core::url::content_hash;

#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn path_for(&self, hash: &str) -> Option<PathBuf> {
        if !is_valid_hash(hash) {
            return None;
        }
        Some(self.root.join(&hash[0..2]).join(&hash[2..4]).join(hash))
    }

    /// Store `bytes`, returning their BLAKE3-256 hex. Content-addressed: storing
    /// identical bytes twice writes one file.
    pub async fn put(&self, bytes: &[u8]) -> Result<String> {
        let hash = content_hash(bytes);
        let path = self
            .path_for(&hash)
            .ok_or_else(|| WidowError::internal("computed hash is not 64 hex characters"))?;
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(hash);
        }
        let parent = path
            .parent()
            .ok_or_else(|| WidowError::internal("blob path has no parent"))?;
        tokio::fs::create_dir_all(parent).await.map_err(|error| {
            WidowError::Internal(format!("cannot create {}: {error}", parent.display()))
        })?;

        let temp = parent.join(format!(
            ".{hash}.tmp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let write = async {
            tokio::fs::write(&temp, bytes).await?;
            let file = tokio::fs::File::open(&temp).await?;
            file.sync_all().await?;
            tokio::fs::rename(&temp, &path).await?;
            Ok::<(), std::io::Error>(())
        };
        if let Err(error) = write.await {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(WidowError::Internal(format!(
                "cannot write blob {}: {error}",
                path.display()
            )));
        }
        Ok(hash)
    }

    pub async fn read(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let path = match self.path_for(hash) {
            Some(path) => path,
            None => return Ok(None),
        };
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(WidowError::Internal(format!(
                "cannot read blob {}: {error}",
                path.display()
            ))),
        }
    }

    pub async fn remove(&self, hash: &str) -> Result<()> {
        if let Some(path) = self.path_for(hash) {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(WidowError::Internal(format!(
                        "cannot remove blob {}: {error}",
                        path.display()
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

pub fn is_valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stores_identical_bytes_once() {
        let temp = tempfile::tempdir().unwrap();
        let store = BlobStore::new(temp.path().to_path_buf());
        let first = store.put(b"<html>same</html>").await.unwrap();
        let second = store.put(b"<html>same</html>").await.unwrap();
        assert_eq!(first, second);
        assert_eq!(store.put(b"<html>other</html>").await.unwrap().len(), 64);
        assert_eq!(
            store.read(&first).await.unwrap().unwrap(),
            b"<html>same</html>"
        );
        let path = store.path_for(&first).unwrap();
        assert!(path.starts_with(temp.path()));
        assert!(path.ends_with(&first));
        assert_eq!(store.read("nope").await.unwrap(), None);
    }

    #[test]
    fn rejects_path_traversal_hashes() {
        let store = BlobStore::new(PathBuf::from("/tmp/web-widow-test"));
        assert!(store.path_for("../../etc/passwd").is_none());
        assert!(store.path_for("short").is_none());
        assert!(store.path_for(&"g".repeat(64)).is_none());
        assert!(store.path_for(&"a".repeat(64)).is_some());
    }
}
