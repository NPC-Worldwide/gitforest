use anyhow::Result;
use gt_core::types::BlobId;
use std::path::{Path, PathBuf};

use crate::blob::{BlobDownloadState, BlobMeta};

/// Content-addressed blob store on the local filesystem.
///
/// Layout: `<root>/blobs/<first-2-hex>/<next-2-hex>/<full-sha256-hex>`
///
/// The two-level shard avoids directories with millions of entries.
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(data_dir: &Path) -> Self {
        BlobStore {
            root: data_dir.join("blobs"),
        }
    }

    /// Get the filesystem path for a blob by its content hash.
    pub fn blob_path(&self, id: &BlobId) -> PathBuf {
        let hex = hex::encode(id.0);
        self.root.join(&hex[..2]).join(&hex[2..4]).join(&hex)
    }

    /// Get the path where blob metadata is stored alongside the blob.
    fn meta_path(&self, id: &BlobId) -> PathBuf {
        let hex = hex::encode(id.0);
        self.root
            .join(&hex[..2])
            .join(&hex[2..4])
            .join(format!("{hex}.meta.json"))
    }

    /// Check if a blob exists in the store.
    pub fn has(&self, id: &BlobId) -> bool {
        self.blob_path(id).exists()
    }

    /// Import a file into the blob store. Returns its metadata.
    /// The file is hashed and copied to the content-addressed path.
    pub fn store_file(&self, path: &Path) -> Result<BlobMeta> {
        let meta = BlobMeta::from_file(path)?;
        let dest = self.blob_path(&meta.blob_id);

        if dest.exists() {
            // Already stored (content-addressed = idempotent)
            return Ok(meta);
        }

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::copy(path, &dest)?;

        // Store metadata alongside
        let meta_json = serde_json::to_string_pretty(&meta)?;
        std::fs::write(self.meta_path(&meta.blob_id), meta_json)?;

        Ok(meta)
    }

    pub fn store_bytes(&self, data: &[u8]) -> Result<BlobMeta> {
        let meta = BlobMeta::from_data(data);
        let dest = self.blob_path(&meta.blob_id);
        if dest.exists() {
            return Ok(meta);
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, data)?;
        let meta_json = serde_json::to_string_pretty(&meta)?;
        std::fs::write(self.meta_path(&meta.blob_id), meta_json)?;
        Ok(meta)
    }

    /// Store a blob assembled from downloaded pieces.
    pub fn store_from_download(&self, download: &BlobDownloadState) -> Result<()> {
        let dest = self.blob_path(&download.meta.blob_id);

        if dest.exists() {
            return Ok(());
        }

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        download.assemble_to_file(&dest)?;

        let actual = BlobMeta::from_file(&dest)?;
        if actual.blob_id != download.meta.blob_id {
            let _ = std::fs::remove_file(&dest);
            anyhow::bail!("assembled content does not match blob id");
        }

        let meta_json = serde_json::to_string_pretty(&download.meta)?;
        std::fs::write(self.meta_path(&download.meta.blob_id), meta_json)?;

        Ok(())
    }

    /// Load blob metadata from the store.
    pub fn get_meta(&self, id: &BlobId) -> Result<Option<BlobMeta>> {
        let meta_path = self.meta_path(id);
        if !meta_path.exists() {
            return Ok(None);
        }
        let data = std::fs::read_to_string(meta_path)?;
        let meta: BlobMeta = serde_json::from_str(&data)?;
        Ok(Some(meta))
    }

    /// Read a specific piece from a stored blob.
    pub fn read_piece(&self, id: &BlobId, meta: &BlobMeta, index: usize) -> Result<Vec<u8>> {
        if index >= meta.piece_count() {
            anyhow::bail!("piece index {index} out of range ({})", meta.piece_count());
        }

        let path = self.blob_path(id);
        let file = std::fs::File::open(&path)?;
        let offset = index as u64 * meta.piece_size as u64;

        use std::io::{Read, Seek, SeekFrom};
        let mut reader = std::io::BufReader::new(file);
        reader.seek(SeekFrom::Start(offset))?;

        let remaining = meta.total_size - offset;
        let read_size = std::cmp::min(meta.piece_size as u64, remaining) as usize;

        let mut buf = vec![0u8; read_size];
        reader.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Remove a blob from the store.
    pub fn remove(&self, id: &BlobId) -> Result<()> {
        let path = self.blob_path(id);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        let meta = self.meta_path(id);
        if meta.exists() {
            std::fs::remove_file(&meta)?;
        }
        Ok(())
    }

    /// List all blob IDs in the store.
    pub fn list(&self) -> Result<Vec<BlobId>> {
        let mut ids = Vec::new();

        if !self.root.exists() {
            return Ok(ids);
        }

        for shard1 in std::fs::read_dir(&self.root)? {
            let shard1 = shard1?;
            if !shard1.file_type()?.is_dir() {
                continue;
            }
            for shard2 in std::fs::read_dir(shard1.path())? {
                let shard2 = shard2?;
                if !shard2.file_type()?.is_dir() {
                    continue;
                }
                for entry in std::fs::read_dir(shard2.path())? {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().to_string();
                    // Skip .meta.json files; only match the raw hex files
                    if name.len() == 64 && !name.contains('.') {
                        if let Ok(id) = name.parse::<BlobId>() {
                            ids.push(id);
                        }
                    }
                }
            }
        }

        Ok(ids)
    }

    /// Total size of all blobs in the store.
    pub fn total_size(&self) -> Result<u64> {
        let mut total = 0u64;
        for id in self.list()? {
            let path = self.blob_path(&id);
            if let Ok(meta) = std::fs::metadata(&path) {
                total += meta.len();
            }
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (BlobStore, PathBuf) {
        let tmp = std::env::temp_dir().join(format!("gt_test_blobstore_{}", uuid::Uuid::new_v4()));
        let store = BlobStore::new(&tmp);
        (store, tmp)
    }

    #[test]
    fn test_store_bytes_roundtrip_is_idempotent() {
        let (store, tmp) = test_store();
        let data = b"artifact bytes here";
        let first = store.store_bytes(data).unwrap();
        assert_eq!(first.total_size, data.len() as u64);
        assert!(store.has(&first.blob_id));
        assert_eq!(
            std::fs::read(store.blob_path(&first.blob_id)).unwrap(),
            data
        );
        let second = store.store_bytes(data).unwrap();
        assert_eq!(second.blob_id, first.blob_id);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_store_and_retrieve() {
        let (store, tmp) = test_store();

        // Create a test file
        let file_dir = tmp.join("input");
        std::fs::create_dir_all(&file_dir).unwrap();
        let file_path = file_dir.join("model.bin");
        let data: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        std::fs::write(&file_path, &data).unwrap();

        // Store it
        let meta = store.store_file(&file_path).unwrap();
        assert!(store.has(&meta.blob_id));

        // Retrieve metadata
        let loaded_meta = store.get_meta(&meta.blob_id).unwrap().unwrap();
        assert_eq!(loaded_meta.blob_id, meta.blob_id);
        assert_eq!(loaded_meta.total_size, data.len() as u64);

        // Read the stored blob directly
        let stored = std::fs::read(store.blob_path(&meta.blob_id)).unwrap();
        assert_eq!(stored, data);

        // List
        let ids = store.list().unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], meta.blob_id);

        // Total size
        assert_eq!(store.total_size().unwrap(), data.len() as u64);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_store_idempotent() {
        let (store, tmp) = test_store();

        let file_dir = tmp.join("input");
        std::fs::create_dir_all(&file_dir).unwrap();
        let path = file_dir.join("data.bin");
        std::fs::write(&path, b"hello world").unwrap();

        let meta1 = store.store_file(&path).unwrap();
        let meta2 = store.store_file(&path).unwrap();
        assert_eq!(meta1.blob_id, meta2.blob_id);
        assert_eq!(store.list().unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_read_piece() {
        let (store, tmp) = test_store();

        let file_dir = tmp.join("input");
        std::fs::create_dir_all(&file_dir).unwrap();
        let path = file_dir.join("big.bin");
        let data: Vec<u8> = (0..512 * 1024).map(|i| (i % 256) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        let meta = store.store_file(&path).unwrap();
        assert_eq!(meta.piece_count(), 2);

        let piece0 = store.read_piece(&meta.blob_id, &meta, 0).unwrap();
        assert_eq!(piece0, &data[..256 * 1024]);

        let piece1 = store.read_piece(&meta.blob_id, &meta, 1).unwrap();
        assert_eq!(piece1, &data[256 * 1024..]);

        // Pieces should pass verification
        assert!(meta.verify_piece(0, &piece0));
        assert!(meta.verify_piece(1, &piece1));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_remove_blob() {
        let (store, tmp) = test_store();

        let file_dir = tmp.join("input");
        std::fs::create_dir_all(&file_dir).unwrap();
        let path = file_dir.join("temp.bin");
        std::fs::write(&path, b"temporary data").unwrap();

        let meta = store.store_file(&path).unwrap();
        assert!(store.has(&meta.blob_id));

        store.remove(&meta.blob_id).unwrap();
        assert!(!store.has(&meta.blob_id));
        assert_eq!(store.list().unwrap().len(), 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_store_from_download() {
        let (store, tmp) = test_store();

        let data: Vec<u8> = (0..300_000).map(|i| (i % 256) as u8).collect();
        let meta = BlobMeta::from_data(&data);

        let mut dl = crate::blob::BlobDownloadState::new(meta.clone());
        for (i, chunk) in data.chunks(meta.piece_size).enumerate() {
            assert!(dl.receive_piece(i, chunk.to_vec()));
        }

        store.store_from_download(&dl).unwrap();
        assert!(store.has(&meta.blob_id));

        let stored = std::fs::read(store.blob_path(&meta.blob_id)).unwrap();
        assert_eq!(stored, data);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_store_from_download_rejects_lying_meta() {
        let (store, tmp) = test_store();

        let data_a: Vec<u8> = vec![0xAA; 300_000];
        let data_b: Vec<u8> = vec![0xBB; 300_000];
        let meta_a = BlobMeta::from_data(&data_a);
        let meta_b = BlobMeta::from_data(&data_b);
        let lying = BlobMeta {
            blob_id: meta_a.blob_id.clone(),
            ..meta_b.clone()
        };

        let mut dl = crate::blob::BlobDownloadState::new(lying);
        for (i, chunk) in data_b.chunks(meta_b.piece_size).enumerate() {
            assert!(dl.receive_piece(i, chunk.to_vec()));
        }

        assert!(store.store_from_download(&dl).is_err());
        assert!(!store.has(&meta_a.blob_id));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
