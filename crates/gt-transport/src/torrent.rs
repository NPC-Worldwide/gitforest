use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::Path;

/// Piece size for splitting bundles into torrent-like chunks (256KB).
const PIECE_SIZE: usize = 256 * 1024;

/// Metadata for a bundle being shared on the network.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BundleMeta {
    /// SHA-256 of the entire bundle file.
    pub content_hash: String,
    /// Total size in bytes.
    pub total_size: u64,
    /// Size of each piece (last piece may be smaller).
    pub piece_size: usize,
    /// SHA-256 hash of each piece.
    pub piece_hashes: Vec<String>,
    /// Repo this bundle belongs to.
    pub repo_name: String,
    /// Owner's peer ID.
    pub owner: String,
}

impl BundleMeta {
    /// Create metadata for a bundle file on disk.
    pub fn from_file(path: &Path, repo_name: &str, owner: &str) -> Result<Self> {
        let data = std::fs::read(path)?;
        let total_size = data.len() as u64;

        let mut content_hasher = Sha256::new();
        content_hasher.update(&data);
        let content_hash = hex::encode(content_hasher.finalize());

        let mut piece_hashes = Vec::new();
        for chunk in data.chunks(PIECE_SIZE) {
            let mut h = Sha256::new();
            h.update(chunk);
            piece_hashes.push(hex::encode(h.finalize()));
        }

        Ok(BundleMeta {
            content_hash,
            total_size,
            piece_size: PIECE_SIZE,
            piece_hashes,
            repo_name: repo_name.to_string(),
            owner: owner.to_string(),
        })
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

    /// Verify an entire downloaded bundle.
    pub fn verify_bundle(&self, data: &[u8]) -> bool {
        if data.len() as u64 != self.total_size {
            return false;
        }
        let mut hasher = Sha256::new();
        hasher.update(data);
        let hash = hex::encode(hasher.finalize());
        hash == self.content_hash
    }

    /// Number of pieces.
    pub fn piece_count(&self) -> usize {
        self.piece_hashes.len()
    }
}

/// Tracks download progress for a bundle.
pub struct DownloadState {
    pub meta: BundleMeta,
    pub pieces: Vec<Option<Vec<u8>>>,
}

impl DownloadState {
    pub fn new(meta: BundleMeta) -> Self {
        let count = meta.piece_count();
        DownloadState {
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

    /// Assemble the complete bundle from pieces.
    pub fn assemble(&self) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }
        let mut data = Vec::with_capacity(self.meta.total_size as usize);
        for piece in &self.pieces {
            data.extend_from_slice(piece.as_ref().unwrap());
        }
        if self.meta.verify_bundle(&data) {
            Some(data)
        } else {
            None
        }
    }

    /// Download progress as a fraction.
    pub fn progress(&self) -> f64 {
        let done = self.pieces.iter().filter(|p| p.is_some()).count();
        done as f64 / self.pieces.len() as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_piece_size_constant() {
        assert_eq!(PIECE_SIZE, 256 * 1024);
    }

    #[test]
    fn test_bundle_meta_from_file() {
        let data: Vec<u8> = (0..512 * 1024).map(|i| (i % 256) as u8).collect();
        let tmp = std::env::temp_dir().join("gt_test_torrent_meta");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("test.bundle");
        std::fs::write(&path, &data).unwrap();

        let meta = BundleMeta::from_file(&path, "repo", "owner").unwrap();
        assert_eq!(meta.total_size, data.len() as u64);
        assert_eq!(meta.piece_size, PIECE_SIZE);
        assert_eq!(meta.piece_count(), 2);
        assert_eq!(meta.repo_name, "repo");
        assert_eq!(meta.owner, "owner");
        assert!(!meta.content_hash.is_empty());
        assert_eq!(meta.piece_hashes.len(), 2);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_verify_piece_valid() {
        let data: Vec<u8> = (0..300_000).map(|i| (i % 256) as u8).collect();
        let tmp = std::env::temp_dir().join("gt_test_torrent_verify");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("data.bin");
        std::fs::write(&path, &data).unwrap();

        let meta = BundleMeta::from_file(&path, "r", "o").unwrap();
        for (i, chunk) in data.chunks(PIECE_SIZE).enumerate() {
            assert!(meta.verify_piece(i, chunk), "piece {i} should be valid");
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_verify_piece_tampered() {
        let data: Vec<u8> = (0..PIECE_SIZE + 100).map(|i| (i % 256) as u8).collect();
        let tmp = std::env::temp_dir().join("gt_test_torrent_tampered");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("data.bin");
        std::fs::write(&path, &data).unwrap();

        let meta = BundleMeta::from_file(&path, "r", "o").unwrap();

        let mut bad = data[..PIECE_SIZE].to_vec();
        bad[0] = bad[0].wrapping_add(1);
        assert!(!meta.verify_piece(0, &bad));
    }

    #[test]
    fn test_verify_piece_out_of_range() {
        let data = vec![0u8; 100];
        let tmp = std::env::temp_dir().join("gt_test_torrent_oor");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("data.bin");
        std::fs::write(&path, &data).unwrap();

        let meta = BundleMeta::from_file(&path, "r", "o").unwrap();
        assert!(!meta.verify_piece(999, &data));
    }

    #[test]
    fn test_verify_bundle_valid() {
        let data: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        let tmp = std::env::temp_dir().join("gt_test_torrent_bundle_ok");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("data.bin");
        std::fs::write(&path, &data).unwrap();

        let meta = BundleMeta::from_file(&path, "r", "o").unwrap();
        assert!(meta.verify_bundle(&data));
    }

    #[test]
    fn test_verify_bundle_wrong_length() {
        let data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        let tmp = std::env::temp_dir().join("gt_test_torrent_bundle_len");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("data.bin");
        std::fs::write(&path, &data).unwrap();

        let meta = BundleMeta::from_file(&path, "r", "o").unwrap();
        assert!(!meta.verify_bundle(&data[..500]));
    }

    #[test]
    fn test_verify_bundle_tampered() {
        let data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        let tmp = std::env::temp_dir().join("gt_test_torrent_bundle_bad");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("data.bin");
        std::fs::write(&path, &data).unwrap();

        let meta = BundleMeta::from_file(&path, "r", "o").unwrap();
        let mut bad = data.clone();
        bad[0] = bad[0].wrapping_add(1);
        assert!(!meta.verify_bundle(&bad));
    }

    #[test]
    fn test_download_state_lifecycle() {
        let data: Vec<u8> = (0..PIECE_SIZE * 3 + 1000)
            .map(|i| (i % 256) as u8)
            .collect();
        let tmp = std::env::temp_dir().join("gt_test_torrent_dl_life");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("data.bin");
        std::fs::write(&path, &data).unwrap();

        let meta = BundleMeta::from_file(&path, "r", "o").unwrap();
        assert_eq!(meta.piece_count(), 4); // 3 full + 1 partial

        let mut dl = DownloadState::new(meta);
        assert!(!dl.is_complete());
        assert_eq!(dl.progress(), 0.0);
        assert_eq!(dl.needed_pieces().len(), 4);

        for (i, chunk) in data.chunks(PIECE_SIZE).enumerate() {
            assert!(dl.receive_piece(i, chunk.to_vec()));
        }

        assert!(dl.is_complete());
        assert_eq!(dl.progress(), 1.0);
        assert!(dl.needed_pieces().is_empty());

        let assembled = dl.assemble().unwrap();
        assert_eq!(assembled, data);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_download_state_partial() {
        let data: Vec<u8> = (0..PIECE_SIZE * 2).map(|i| (i % 256) as u8).collect();
        let tmp = std::env::temp_dir().join("gt_test_torrent_dl_part");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("data.bin");
        std::fs::write(&path, &data).unwrap();

        let meta = BundleMeta::from_file(&path, "r", "o").unwrap();
        let mut dl = DownloadState::new(meta);

        // Only receive first piece
        dl.receive_piece(0, data[..PIECE_SIZE].to_vec());
        assert!(!dl.is_complete());
        assert_eq!(dl.progress(), 0.5);
        assert_eq!(dl.needed_pieces(), vec![1]);
    }

    #[test]
    fn test_download_state_reject_bad_piece() {
        let data: Vec<u8> = (0..PIECE_SIZE * 2).map(|i| (i % 256) as u8).collect();
        let tmp = std::env::temp_dir().join("gt_test_torrent_dl_bad");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("data.bin");
        std::fs::write(&path, &data).unwrap();

        let meta = BundleMeta::from_file(&path, "r", "o").unwrap();
        let mut dl = DownloadState::new(meta);

        let mut bad = data[..PIECE_SIZE].to_vec();
        bad[0] = bad[0].wrapping_add(1);
        assert!(!dl.receive_piece(0, bad));
        assert!(dl.pieces[0].is_none());
    }

    #[test]
    fn test_assemble_incomplete_returns_none() {
        let data: Vec<u8> = (0..PIECE_SIZE * 2).map(|i| (i % 256) as u8).collect();
        let tmp = std::env::temp_dir().join("gt_test_torrent_dl_inc");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("data.bin");
        std::fs::write(&path, &data).unwrap();

        let meta = BundleMeta::from_file(&path, "r", "o").unwrap();
        let dl = DownloadState::new(meta);
        assert!(dl.assemble().is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_bundle_meta_serialization() {
        let data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        let tmp = std::env::temp_dir().join("gt_test_torrent_serde");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("data.bin");
        std::fs::write(&path, &data).unwrap();

        let meta = BundleMeta::from_file(&path, "myrepo", "owner123").unwrap();
        let json = serde_json::to_string(&meta).unwrap();
        let deserialized: BundleMeta = serde_json::from_str(&json).unwrap();

        assert_eq!(meta.content_hash, deserialized.content_hash);
        assert_eq!(meta.total_size, deserialized.total_size);
        assert_eq!(meta.piece_size, deserialized.piece_size);
        assert_eq!(meta.piece_hashes, deserialized.piece_hashes);
        assert_eq!(meta.repo_name, deserialized.repo_name);
        assert_eq!(meta.owner, deserialized.owner);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
