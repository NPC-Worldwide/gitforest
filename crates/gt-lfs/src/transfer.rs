use anyhow::Result;
use gt_core::identity::PeerId;
use gt_core::types::BlobId;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::blob::{BlobDownloadState, BlobMeta};
use crate::store::BlobStore;
use crate::tracker::BlobTracker;

/// Status of an active transfer.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TransferStatus {
    pub blob_id: BlobId,
    pub total_size: u64,
    pub piece_count: usize,
    pub pieces_done: usize,
    pub progress: f64,
    pub state: TransferState,
    pub active_peers: Vec<PeerId>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TransferState {
    /// Waiting for peer discovery.
    Discovering,
    /// Actively downloading pieces.
    Downloading,
    /// All pieces received, verifying integrity.
    Verifying,
    /// Transfer complete and stored.
    Complete,
    /// Transfer failed.
    Failed(String),
}

/// Manages active blob transfers (downloads and uploads).
pub struct TransferManager {
    downloads: HashMap<BlobId, ActiveDownload>,
    store: Arc<BlobStore>,
    tracker: Arc<RwLock<BlobTracker>>,
}

struct ActiveDownload {
    state: BlobDownloadState,
    peers: Vec<PeerId>,
    transfer_state: TransferState,
    announced_have: u32,
}

impl TransferManager {
    pub fn new(store: Arc<BlobStore>, tracker: Arc<RwLock<BlobTracker>>) -> Self {
        TransferManager {
            downloads: HashMap::new(),
            store,
            tracker,
        }
    }

    /// Start downloading a blob. Returns immediately; pieces are received via `receive_piece`.
    pub async fn start_download(
        &mut self,
        meta: BlobMeta,
        seeders: Vec<PeerId>,
        our_peer_id: PeerId,
    ) -> Result<()> {
        let blob_id = meta.blob_id.clone();

        if self.store.has(&blob_id) {
            info!("Blob {} already in store, skipping download", blob_id);
            return Ok(());
        }

        if self.downloads.contains_key(&blob_id) {
            info!("Blob {} already downloading", blob_id);
            return Ok(());
        }

        info!(
            "Starting download of blob {} ({} pieces, {} bytes)",
            blob_id,
            meta.piece_count(),
            meta.total_size
        );

        // Register ourselves as a leecher
        {
            let mut tracker = self.tracker.write().await;
            tracker.add_leecher(blob_id.clone(), our_peer_id);
        }

        let download = ActiveDownload {
            state: BlobDownloadState::new(meta),
            peers: seeders,
            transfer_state: TransferState::Downloading,
            announced_have: 0,
        };

        self.downloads.insert(blob_id, download);
        Ok(())
    }

    /// Receive a piece from a peer. Returns true if the piece was valid.
    pub fn receive_piece(&mut self, blob_id: &BlobId, index: usize, data: Vec<u8>) -> bool {
        let download = match self.downloads.get_mut(blob_id) {
            Some(d) => d,
            None => return false,
        };

        let valid = download.state.receive_piece(index, data);
        if !valid {
            warn!("Invalid piece {} for blob {}", index, blob_id);
        }
        valid
    }

    /// Check if a download is complete. If so, store it and promote to seeder.
    pub async fn check_complete(&mut self, blob_id: &BlobId, our_peer_id: PeerId) -> Result<bool> {
        let download = match self.downloads.get_mut(blob_id) {
            Some(d) => d,
            None => return Ok(false),
        };

        if !download.state.is_complete() {
            return Ok(false);
        }

        download.transfer_state = TransferState::Verifying;

        // Store the assembled blob
        self.store.store_from_download(&download.state)?;

        download.transfer_state = TransferState::Complete;

        // Promote to seeder
        {
            let mut tracker = self.tracker.write().await;
            tracker.promote_to_seeder(blob_id, our_peer_id);
        }

        info!("Download complete for blob {}", blob_id);
        Ok(true)
    }

    /// Get the list of pieces we still need for a blob.
    pub fn needed_pieces(&self, blob_id: &BlobId) -> Vec<usize> {
        self.downloads
            .get(blob_id)
            .map(|d| d.state.needed_pieces())
            .unwrap_or_default()
    }

    /// Get the status of a transfer.
    pub fn status(&self, blob_id: &BlobId) -> Option<TransferStatus> {
        self.downloads.get(blob_id).map(|d| {
            let pieces_done = d.state.pieces.iter().filter(|p| p.is_some()).count();
            TransferStatus {
                blob_id: blob_id.clone(),
                total_size: d.state.meta.total_size,
                piece_count: d.state.meta.piece_count(),
                pieces_done,
                progress: d.state.progress(),
                state: d.transfer_state.clone(),
                active_peers: d.peers.clone(),
            }
        })
    }

    /// List all active transfers.
    pub fn active_transfers(&self) -> Vec<TransferStatus> {
        self.downloads
            .keys()
            .filter_map(|id| self.status(id))
            .collect()
    }

    /// Remove a completed or failed download from the active list.
    pub fn remove_download(&mut self, blob_id: &BlobId) {
        self.downloads.remove(blob_id);
    }

    /// Read a piece from a stored blob (for serving to other peers).
    pub fn serve_piece(&self, blob_id: &BlobId, index: usize) -> Result<Option<Vec<u8>>> {
        if self.store.has(blob_id) {
            let meta = match self.store.get_meta(blob_id)? {
                Some(m) => m,
                None => return Ok(None),
            };
            let data = self.store.read_piece(blob_id, &meta, index)?;
            return Ok(Some(data));
        }
        if let Some(download) = self.downloads.get(blob_id) {
            return Ok(download
                .state
                .pieces
                .get(index)
                .and_then(|piece| piece.clone()));
        }
        Ok(None)
    }

    pub fn take_unannounced_progress(&mut self, blob_id: &BlobId) -> Option<u32> {
        let download = self.downloads.get_mut(blob_id)?;
        let have = download.state.pieces.iter().filter(|p| p.is_some()).count() as u32;
        if have == 0 || have == download.announced_have {
            return None;
        }
        download.announced_have = have;
        Some(have)
    }
}

/// Strategy for selecting which pieces to request from which peers.
/// Implements a simple rarest-first approach.
pub struct PieceStrategy;

impl PieceStrategy {
    /// Given a list of needed pieces, return a prioritized order.
    /// For now, uses sequential order. In a full implementation,
    /// this would do rarest-first based on piece availability across peers.
    pub fn prioritize(needed: &[usize]) -> Vec<usize> {
        // Simple strategy: sequential order with random start
        // This naturally distributes requests across pieces
        let mut pieces = needed.to_vec();
        if pieces.len() > 1 {
            // Rotate by a random amount for basic load distribution
            let rotate = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos() as usize
                % pieces.len();
            pieces.rotate_left(rotate);
        }
        pieces
    }

    /// Split needed pieces across available peers for parallel download.
    pub fn assign_to_peers(needed: &[usize], peer_count: usize) -> Vec<Vec<usize>> {
        if peer_count == 0 {
            return vec![];
        }
        let mut assignments: Vec<Vec<usize>> = (0..peer_count).map(|_| Vec::new()).collect();
        for (i, &piece) in needed.iter().enumerate() {
            assignments[i % peer_count].push(piece);
        }
        assignments
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob::BlobMeta;
    use gt_core::identity::Identity;

    #[tokio::test]
    async fn test_transfer_manager_lifecycle() {
        let tmp = std::env::temp_dir().join(format!("gt_test_transfer_{}", uuid::Uuid::new_v4()));
        let store = Arc::new(BlobStore::new(&tmp));
        let tracker = Arc::new(RwLock::new(BlobTracker::new()));
        let mut mgr = TransferManager::new(store.clone(), tracker.clone());

        let data: Vec<u8> = (0..300_000).map(|i| (i % 256) as u8).collect();
        let meta = BlobMeta::from_data(&data);
        let blob_id = meta.blob_id.clone();
        let our_id = Identity::generate().peer_id();
        let seeder = Identity::generate().peer_id();

        // Start download
        mgr.start_download(meta.clone(), vec![seeder.clone()], our_id.clone())
            .await
            .unwrap();

        // Check status
        let status = mgr.status(&blob_id).unwrap();
        assert_eq!(status.state, TransferState::Downloading);
        assert_eq!(status.progress, 0.0);

        // Feed pieces
        for (i, chunk) in data.chunks(meta.piece_size).enumerate() {
            assert!(mgr.receive_piece(&blob_id, i, chunk.to_vec()));
        }

        // Check completion
        assert!(mgr.check_complete(&blob_id, our_id.clone()).await.unwrap());

        let status = mgr.status(&blob_id).unwrap();
        assert_eq!(status.state, TransferState::Complete);
        assert_eq!(status.progress, 1.0);

        // Blob should be in store
        assert!(store.has(&blob_id));

        // We should be a seeder now
        let tracker = tracker.read().await;
        assert_eq!(tracker.seeder_count(&blob_id), 1);
        assert_eq!(tracker.leecher_count(&blob_id), 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_serve_piece() {
        let tmp = std::env::temp_dir().join(format!("gt_test_serve_{}", uuid::Uuid::new_v4()));
        let store = Arc::new(BlobStore::new(&tmp));
        let tracker = Arc::new(RwLock::new(BlobTracker::new()));

        // Store a blob directly
        let data: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        let input_dir = tmp.join("input");
        std::fs::create_dir_all(&input_dir).unwrap();
        let path = input_dir.join("test.bin");
        std::fs::write(&path, &data).unwrap();
        let meta = store.store_file(&path).unwrap();

        let mgr = TransferManager::new(store, tracker);

        // Serve piece 0
        let piece = mgr.serve_piece(&meta.blob_id, 0).unwrap().unwrap();
        assert!(meta.verify_piece(0, &piece));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_serve_piece_falls_back_to_partial_download() {
        let tmp = std::env::temp_dir().join(format!("gt_test_partial_{}", uuid::Uuid::new_v4()));
        let store = Arc::new(BlobStore::new(&tmp));
        let tracker = Arc::new(RwLock::new(BlobTracker::new()));
        let mut mgr = TransferManager::new(store.clone(), tracker);

        let data: Vec<u8> = (0..600_000).map(|i| (i % 251) as u8).collect();
        let meta = BlobMeta::from_data(&data);
        assert_eq!(meta.piece_count(), 3);
        let blob_id = meta.blob_id.clone();
        let our_id = Identity::generate().peer_id();
        mgr.start_download(meta.clone(), vec![], our_id)
            .await
            .unwrap();

        assert!(mgr.serve_piece(&blob_id, 0).unwrap().is_none());
        let first: Vec<u8> = data.chunks(meta.piece_size).next().unwrap().to_vec();
        assert!(mgr.receive_piece(&blob_id, 0, first.clone()));
        assert!(!mgr.receive_piece(&blob_id, 1, vec![0u8; 8]));
        assert_eq!(mgr.serve_piece(&blob_id, 0).unwrap().unwrap(), first);
        assert!(mgr.serve_piece(&blob_id, 1).unwrap().is_none());
        assert!(mgr.serve_piece(&blob_id, 9).unwrap().is_none());

        assert_eq!(mgr.take_unannounced_progress(&blob_id), Some(1));
        assert_eq!(mgr.take_unannounced_progress(&blob_id), None);
        assert_eq!(mgr.take_unannounced_progress(&BlobId([0xee; 32])), None);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_piece_strategy_assign() {
        let needed: Vec<usize> = (0..10).collect();

        let assignments = PieceStrategy::assign_to_peers(&needed, 3);
        assert_eq!(assignments.len(), 3);

        // All pieces accounted for
        let total: usize = assignments.iter().map(|a| a.len()).sum();
        assert_eq!(total, 10);

        // Roughly balanced
        assert!(assignments[0].len() >= 3);
        assert!(assignments[1].len() >= 3);
        assert!(assignments[2].len() >= 3);
    }

    #[test]
    fn test_piece_strategy_single_peer() {
        let needed: Vec<usize> = (0..5).collect();
        let assignments = PieceStrategy::assign_to_peers(&needed, 1);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].len(), 5);
    }

    #[test]
    fn test_piece_strategy_no_peers() {
        let needed: Vec<usize> = (0..5).collect();
        let assignments = PieceStrategy::assign_to_peers(&needed, 0);
        assert!(assignments.is_empty());
    }
}
