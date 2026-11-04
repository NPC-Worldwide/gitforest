use anyhow::Result;
use gt_core::types::BlobId;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// Compute adaptive piece size based on total file size.
/// - < 100 MB:  256 KB pieces
/// - < 1 GB:    1 MB pieces
/// - >= 1 GB:   4 MB pieces
pub fn adaptive_piece_size(total_size: u64) -> usize {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;

    if total_size < 100 * MB {
        256 * 1024
    } else if total_size < GB {
        1024 * 1024
    } else {
        4 * 1024 * 1024
    }
}

/// Metadata for a blob being shared on the network.
/// Unlike BundleMeta, this is content-addressed (no repo/owner)
/// and uses adaptive piece sizing for multi-GB files.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlobMeta {
    /// SHA-256 of the entire file content (this IS the blob's identity).
    pub blob_id: BlobId,
    /// Total size in bytes.
    pub total_size: u64,
    /// Size of each piece (last piece may be smaller).
    pub piece_size: usize,
    /// SHA-256 hash of each piece.
    pub piece_hashes: Vec<String>,
}

impl BlobMeta {
    /// Create metadata by streaming a file from disk.
    /// Computes the overall SHA-256 and per-piece hashes in a single pass
    /// without loading the entire file into memory.
    pub fn from_file(path: &Path) -> Result<Self> {
        let mut file = std::fs::File::open(path)?;
        let total_size = file.metadata()?.len();
        let piece_size = adaptive_piece_size(total_size);

        let mut content_hasher = Sha256::new();
        let mut piece_hashes = Vec::new();
        let mut buf = vec![0u8; piece_size];

        loop {
            let mut piece_len = 0;
            // Read exactly one piece (or less at EOF)
            while piece_len < piece_size {
                let n = file.read(&mut buf[piece_len..])?;
                if n == 0 {
                    break;
                }
                piece_len += n;
            }
            if piece_len == 0 {
                break;
            }

            let piece_data = &buf[..piece_len];
            content_hasher.update(piece_data);

            let mut piece_hasher = Sha256::new();
            piece_hasher.update(piece_data);
            piece_hashes.push(hex::encode(piece_hasher.finalize()));
        }

        let content_hash: [u8; 32] = content_hasher.finalize().into();

        Ok(BlobMeta {
            blob_id: BlobId(content_hash),
            total_size,
            piece_size,
            piece_hashes,
        })
    }

    /// Create metadata from in-memory data (for small blobs or testing).
    pub fn from_data(data: &[u8]) -> Self {
        let total_size = data.len() as u64;
        let piece_size = adaptive_piece_size(total_size);

        let mut content_hasher = Sha256::new();
        content_hasher.update(data);
        let content_hash: [u8; 32] = content_hasher.finalize().into();

        let mut piece_hashes = Vec::new();
        for chunk in data.chunks(piece_size) {
            let mut h = Sha256::new();
            h.update(chunk);
            piece_hashes.push(hex::encode(h.finalize()));
        }

        BlobMeta {
            blob_id: BlobId(content_hash),
            total_size,
            piece_size,
            piece_hashes,
        }
    }

    /// Verify a downloaded piece against its expected hash.
    pub fn verify_piece(&self, index: usize, data: &[u8]) -> bool {
        if index >= self.piece_hashes.len() {
            return false;
        }
        let mut hasher = Sha256::new();
        hasher.update(data);
        let hash = hex::encode(hasher.finalize());
        hash == self.piece_hashes[index]
    }

    /// Number of pieces.
    pub fn piece_count(&self) -> usize {
        self.piece_hashes.len()
    }
}

/// Tracks download progress for a blob.
/// Pieces are held in memory during download, then assembled to disk.
pub struct BlobDownloadState {
    pub meta: BlobMeta,
    pub pieces: Vec<Option<Vec<u8>>>,
}

impl BlobDownloadState {
    pub fn new(meta: BlobMeta) -> Self {
        let count = meta.piece_count();
        BlobDownloadState {
            meta,
            pieces: vec![None; count],
        }
    }

    /// Record a received piece. Returns false if verification fails.
    pub fn receive_piece(&mut self, index: usize, data: Vec<u8>) -> bool {
        if !self.meta.verify_piece(index, &data) {
            return false;
        }
        if index < self.pieces.len() {
            self.pieces[index] = Some(data);
        }
        true
    }

    /// Check if download is complete.
    pub fn is_complete(&self) -> bool {
        self.pieces.iter().all(|p| p.is_some())
    }

    /// Pieces we still need (indexes).
    pub fn needed_pieces(&self) -> Vec<usize> {
        self.pieces
            .iter()
            .enumerate()
            .filter_map(|(i, p)| if p.is_none() { Some(i) } else { None })
            .collect()
    }

    /// Download progress as a fraction.
    pub fn progress(&self) -> f64 {
        if self.pieces.is_empty() {
            return 1.0;
        }
        let done = self.pieces.iter().filter(|p| p.is_some()).count();
        done as f64 / self.pieces.len() as f64
    }

    /// Assemble pieces and write directly to a file.
    /// This avoids holding the entire blob in memory at once for large files.
    pub fn assemble_to_file(&self, path: &Path) -> Result<()> {
        if !self.is_complete() {
            anyhow::bail!("download not complete");
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut file = std::fs::File::create(path)?;
        for piece in &self.pieces {
            std::io::Write::write_all(&mut file, piece.as_ref().unwrap())?;
        }
        Ok(())
    }

    /// Assemble into a Vec (only suitable for small blobs).
    pub fn assemble(&self) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }
        let mut data = Vec::with_capacity(self.meta.total_size as usize);
        for piece in &self.pieces {
            data.extend_from_slice(piece.as_ref().unwrap());
        }
        Some(data)
    }
}

/// Compute the DHT address for a blob.
pub fn blob_dht_address(blob_id: &BlobId) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"gitforest:blob:");
    hasher.update(blob_id.0);
    hasher.finalize().into()
}

/// Compute the DHT address for a collection.
pub fn collection_dht_address(manifest_id: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"gitforest:collection:");
    hasher.update(manifest_id.as_bytes());
    hasher.finalize().into()
}

/// Compute the DHT address for a workflow-cache key.
pub fn cache_dht_address(key: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"gitforest:cache:");
    hasher.update(key.as_bytes());
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adaptive_piece_size_small() {
        // < 100 MB => 256 KB
        assert_eq!(adaptive_piece_size(0), 256 * 1024);
        assert_eq!(adaptive_piece_size(1024), 256 * 1024);
        assert_eq!(adaptive_piece_size(50 * 1024 * 1024), 256 * 1024);
        assert_eq!(adaptive_piece_size(99 * 1024 * 1024), 256 * 1024);
    }

    #[test]
    fn test_adaptive_piece_size_medium() {
        // 100 MB..1 GB => 1 MB
        assert_eq!(adaptive_piece_size(100 * 1024 * 1024), 1024 * 1024);
        assert_eq!(adaptive_piece_size(500 * 1024 * 1024), 1024 * 1024);
        assert_eq!(adaptive_piece_size(1024 * 1024 * 1024 - 1), 1024 * 1024);
    }

    #[test]
    fn test_adaptive_piece_size_large() {
        // >= 1 GB => 4 MB
        assert_eq!(adaptive_piece_size(1024 * 1024 * 1024), 4 * 1024 * 1024);
        assert_eq!(
            adaptive_piece_size(10 * 1024 * 1024 * 1024),
            4 * 1024 * 1024
        );
    }

    #[test]
    fn test_blob_meta_from_data() {
        let data: Vec<u8> = (0..512 * 1024).map(|i| (i % 256) as u8).collect();
        let meta = BlobMeta::from_data(&data);

        assert_eq!(meta.total_size, data.len() as u64);
        assert_eq!(meta.piece_size, 256 * 1024);
        assert_eq!(meta.piece_count(), 2); // 512KB / 256KB = 2

        // Verify each piece
        for (i, chunk) in data.chunks(meta.piece_size).enumerate() {
            assert!(meta.verify_piece(i, chunk));
        }

        // Tampered piece fails
        let mut bad = data[..256 * 1024].to_vec();
        bad[0] = bad[0].wrapping_add(1);
        assert!(!meta.verify_piece(0, &bad));
    }

    #[test]
    fn test_blob_meta_from_file() {
        let data: Vec<u8> = (0..300_000).map(|i| (i % 256) as u8).collect();
        let tmp = std::env::temp_dir().join("gt_test_blob_meta");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("test.bin");
        std::fs::write(&path, &data).unwrap();

        let meta = BlobMeta::from_file(&path).unwrap();
        let mem_meta = BlobMeta::from_data(&data);

        // File-based and memory-based should produce identical results
        assert_eq!(meta.blob_id, mem_meta.blob_id);
        assert_eq!(meta.total_size, mem_meta.total_size);
        assert_eq!(meta.piece_size, mem_meta.piece_size);
        assert_eq!(meta.piece_hashes, mem_meta.piece_hashes);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_blob_download_state() {
        let data: Vec<u8> = (0..512 * 1024).map(|i| (i % 256) as u8).collect();
        let meta = BlobMeta::from_data(&data);

        let mut dl = BlobDownloadState::new(meta.clone());
        assert!(!dl.is_complete());
        assert_eq!(dl.progress(), 0.0);
        assert_eq!(dl.needed_pieces(), vec![0, 1]);

        // Receive first piece
        let piece0 = data[..256 * 1024].to_vec();
        assert!(dl.receive_piece(0, piece0));
        assert!(!dl.is_complete());
        assert_eq!(dl.progress(), 0.5);
        assert_eq!(dl.needed_pieces(), vec![1]);

        // Tampered piece rejected
        let mut bad_piece = data[256 * 1024..].to_vec();
        bad_piece[0] = bad_piece[0].wrapping_add(1);
        assert!(!dl.receive_piece(1, bad_piece));

        // Correct second piece
        let piece1 = data[256 * 1024..].to_vec();
        assert!(dl.receive_piece(1, piece1));
        assert!(dl.is_complete());
        assert_eq!(dl.progress(), 1.0);

        let assembled = dl.assemble().unwrap();
        assert_eq!(assembled, data);
    }

    #[test]
    fn test_assemble_to_file() {
        let data: Vec<u8> = (0..128 * 1024).map(|i| (i % 256) as u8).collect();
        let meta = BlobMeta::from_data(&data);

        let mut dl = BlobDownloadState::new(meta);
        dl.receive_piece(0, data.clone());

        let tmp = std::env::temp_dir().join("gt_test_blob_assemble");
        let path = tmp.join("assembled.bin");
        dl.assemble_to_file(&path).unwrap();

        let read_back = std::fs::read(&path).unwrap();
        assert_eq!(read_back, data);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_blob_dht_address_deterministic() {
        let id = BlobId([42u8; 32]);
        let addr1 = blob_dht_address(&id);
        let addr2 = blob_dht_address(&id);
        assert_eq!(addr1, addr2);

        let other = BlobId([99u8; 32]);
        assert_ne!(addr1, blob_dht_address(&other));
    }

    #[test]
    fn test_collection_dht_address_deterministic() {
        let addr1 = collection_dht_address("model-123");
        let addr2 = collection_dht_address("model-123");
        assert_eq!(addr1, addr2);
        assert_ne!(addr1, collection_dht_address("model-456"));
    }

    #[test]
    fn test_cache_dht_address_deterministic() {
        let addr1 = cache_dht_address("linux-cargo-abc");
        let addr2 = cache_dht_address("linux-cargo-abc");
        assert_eq!(addr1, addr2);
        assert_ne!(addr1, cache_dht_address("linux-cargo-abd"));
        assert_ne!(addr1, collection_dht_address("linux-cargo-abc"));
    }
}
