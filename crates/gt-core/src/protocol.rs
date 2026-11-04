use crate::identity::{verify_signature, PeerId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// All wire protocol messages exchanged between gitforest peers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    // === Discovery ===
    /// Announce that we're hosting a repo.
    AnnounceRepo(RepoAnnouncement),

    /// Request info about a repo.
    QueryRepo(RepoQuery),

    /// Response with repo info and peer list.
    RepoInfo(RepoInfoResponse),

    // === Git Operations ===
    /// Request to clone/fetch a repo (initiates git pack protocol).
    GitFetch(GitFetchRequest),

    /// Request to push to a repo.
    GitPush(GitPushRequest),

    /// Response to a push request (accepted/rejected).
    GitPushResponse(GitPushResponse),

    // === Collaboration ===
    /// Submit a signed patch set.
    SubmitPatch(PatchSubmission),

    /// Submit a merge request (fork-based).
    MergeRequest(MergeRequestMessage),

    /// Update ACL (grant/revoke access).
    UpdateAcl(AclUpdate),

    // === Compute Marketplace ===
    /// Announce available compute resources.
    ComputeOffer(ComputeOfferMessage),

    /// Query for compute providers.
    QueryCompute(ComputeQueryMessage),

    /// Submit a compute job.
    SubmitJob(JobSubmissionMessage),

    /// Accept a compute job.
    JobAccepted(JobAcceptedMessage),

    /// Report job result.
    JobResult(JobResultMessage),

    /// Cancel a submitted job (submitter to worker).
    CancelJob(JobCancelMessage),

    /// Open a payment channel.
    PaymentOpen(PaymentOpenMessage),

    /// Release payment (job completed).
    PaymentRelease(PaymentReleaseMessage),

    /// Dispute a payment.
    PaymentDispute(PaymentDisputeMessage),

    // === Large File Storage ===
    /// Announce that we're seeding a blob.
    AnnounceBlob(BlobAnnouncement),

    /// Query for a blob by its content hash.
    QueryBlob(BlobQuery),

    /// Response with blob info and seeders.
    BlobInfo(BlobInfoResponse),

    /// Request specific pieces of a blob.
    RequestPieces(PieceRequest),

    /// Respond with piece data.
    PieceData(PieceResponse),

    /// Announce a collection (model/dataset) manifest.
    AnnounceCollection(CollectionAnnouncement),

    /// Query for a collection by ID.
    QueryCollection(CollectionQuery),

    // === Peer Management ===
    /// Ping to check liveness.
    Ping { nonce: u64 },

    /// Pong response.
    Pong { nonce: u64 },
}

/// Announcement that a peer is hosting/seeding a repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoAnnouncement {
    /// Owner of the repo.
    pub owner: PeerId,
    /// Repo name.
    pub name: String,
    /// Short description.
    pub description: String,
    /// Default branch.
    pub default_branch: String,
    /// HEAD commit hash.
    pub head: String,
    /// Signature over the announcement by the owner.
    pub signature: Vec<u8>,
    /// Optional display name of the announcing peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// Query for a specific repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoQuery {
    /// The repo address hash (sha256 of owner pubkey + repo name).
    pub address: [u8; 32],
}

/// Response to a repo query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoInfoResponse {
    /// The repo announcement (if found).
    pub announcement: Option<RepoAnnouncement>,
    /// Peers currently seeding this repo.
    pub seeders: Vec<PeerAddr>,
    /// Number of peers currently downloading.
    pub leecher_count: u32,
}

/// A peer's network address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerAddr {
    pub peer_id: PeerId,
    pub addresses: Vec<String>,
}

/// Request to fetch (clone/pull) from a repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitFetchRequest {
    pub owner: PeerId,
    pub repo_name: String,
    /// If set, only fetch these refs. Empty = full clone.
    pub want_refs: Vec<String>,
}

/// Request to push to a remote repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitPushRequest {
    /// The pusher's identity.
    pub pusher: PeerId,
    /// Target repo owner.
    pub owner: PeerId,
    /// Target repo name.
    pub repo_name: String,
    /// Refs being pushed (refname -> new SHA).
    pub ref_updates: Vec<RefUpdate>,
    /// Signature over the push request.
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefUpdate {
    pub ref_name: String,
    pub old_sha: String,
    pub new_sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitPushResponse {
    pub accepted: bool,
    pub message: String,
}

/// A signed patch submission (like git-email but P2P).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchSubmission {
    /// Who submitted the patch.
    pub author: PeerId,
    /// Target repo owner.
    pub target_owner: PeerId,
    /// Target repo name.
    pub target_repo: String,
    /// Target branch.
    pub target_branch: String,
    /// Human-readable title.
    pub title: String,
    /// Description/cover letter.
    pub description: String,
    /// The actual patch data (git format-patch output).
    pub patches: Vec<Vec<u8>>,
    /// Signature over the submission.
    pub signature: Vec<u8>,
}

/// A merge request: "I forked your repo, please merge my branch."
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeRequestMessage {
    /// Who is requesting the merge.
    pub author: PeerId,
    /// The forker's repo name (on their node).
    pub source_repo: String,
    /// Branch to merge from.
    pub source_branch: String,
    /// Target repo owner.
    pub target_owner: PeerId,
    /// Target repo name.
    pub target_repo: String,
    /// Target branch to merge into.
    pub target_branch: String,
    /// Title.
    pub title: String,
    /// Description.
    pub description: String,
    /// Signature.
    pub signature: Vec<u8>,
}

/// ACL update: grant or revoke push access.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AclUpdate {
    /// Repo owner (must be the signer).
    pub owner: PeerId,
    /// Repo name.
    pub repo_name: String,
    /// Changes to the ACL.
    pub changes: Vec<AclChange>,
    /// Signature by owner.
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AclChange {
    /// Grant push access to a peer.
    Grant {
        peer_id: PeerId,
        /// Which refs they can push to (glob pattern, e.g. "refs/heads/*").
        ref_pattern: String,
    },
    /// Revoke access from a peer.
    Revoke { peer_id: PeerId },
}

// ============ Compute Marketplace Messages ============

/// A peer advertising available compute resources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeOfferMessage {
    pub worker_id: PeerId,
    pub cpu_cores: u32,
    pub ram_mb: u64,
    pub disk_mb: u64,
    pub has_gpu: bool,
    pub hourly_rate_sats: u64,
    pub accepted_chains: Vec<String>,
    pub max_concurrent_jobs: u32,
    #[serde(default)]
    pub pow_nonce: u64,
    pub signature: Vec<u8>,
}

impl ComputeOfferMessage {
    pub fn signable_payload(&self) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(b"gitforest:compute-offer:");
        hasher.update(self.worker_id.0);
        hasher.update(self.cpu_cores.to_le_bytes());
        hasher.update(self.ram_mb.to_le_bytes());
        hasher.update(self.disk_mb.to_le_bytes());
        hasher.update([u8::from(self.has_gpu)]);
        hasher.update(self.hourly_rate_sats.to_le_bytes());
        for chain in &self.accepted_chains {
            hasher.update(chain.as_bytes());
            hasher.update([0]);
        }
        hasher.update(self.max_concurrent_jobs.to_le_bytes());
        hasher.update(self.pow_nonce.to_le_bytes());
        hasher.finalize().to_vec()
    }

    pub fn verify(&self) -> bool {
        self.verify_with_bits(crate::identity::MIN_POW_BITS)
    }

    /// Verify the offer's PoW against `min_bits`, floored at the genesis
    /// difficulty: no configuration can make a verifier accept weaker
    /// proofs than the network minimum. Higher-proof offers always pass a
    /// lower minimum, which is what makes difficulty raises deployable
    /// without a flag day.
    pub fn verify_with_bits(&self, min_bits: u8) -> bool {
        let min_bits = min_bits.max(crate::identity::MIN_POW_BITS);
        if !crate::identity::verify_pow(&self.worker_id, self.pow_nonce, min_bits) {
            return false;
        }
        verify_signature(&self.worker_id, &self.signable_payload(), &self.signature).is_ok()
    }
}

/// Query for compute providers matching requirements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeQueryMessage {
    pub min_cpu_cores: u32,
    pub min_ram_mb: u64,
    pub gpu_required: bool,
    pub preferred_chain: String,
}

/// Submit a compute job to a worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSubmissionMessage {
    pub job_id: String,
    pub submitter: PeerId,
    pub repo_owner: PeerId,
    pub repo_name: String,
    pub commit_sha: String,
    pub job_config_json: String,
    pub min_cpu_cores: u32,
    pub min_ram_mb: u64,
    pub min_disk_mb: u64,
    pub gpu_required: bool,
    pub network_access: bool,
    pub max_payment_sats: u64,
    pub preferred_chain: String,
    pub signature: Vec<u8>,
}

impl JobSubmissionMessage {
    pub fn signable_payload(&self) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(b"gitforest:job-submission:");
        hasher.update(&self.job_id);
        hasher.update(self.submitter.0);
        hasher.update(self.repo_owner.0);
        hasher.update(&self.repo_name);
        hasher.update(&self.commit_sha);
        hasher.update(&self.job_config_json);
        hasher.update(self.min_cpu_cores.to_le_bytes());
        hasher.update(self.min_ram_mb.to_le_bytes());
        hasher.update(self.min_disk_mb.to_le_bytes());
        hasher.update([u8::from(self.gpu_required)]);
        hasher.update([u8::from(self.network_access)]);
        hasher.update(self.max_payment_sats.to_le_bytes());
        hasher.update(&self.preferred_chain);
        hasher.finalize().to_vec()
    }

    pub fn verify(&self) -> bool {
        verify_signature(&self.submitter, &self.signable_payload(), &self.signature).is_ok()
    }
}

/// Worker accepts a compute job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobAcceptedMessage {
    pub job_id: String,
    pub worker_id: PeerId,
    pub estimated_time_secs: u64,
    pub agreed_price_sats: u64,
    pub payment_address: String,
    pub signature: Vec<u8>,
}

/// Worker reports job result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResultMessage {
    pub job_id: String,
    pub worker_id: PeerId,
    pub result_id: String,
    pub exit_code: i32,
    pub stdout_hash: String,
    pub stderr_hash: String,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub artifact_hashes: Vec<(String, String)>,
    #[serde(default)]
    pub artifact_blobs: Vec<crate::types::ArtifactBlob>,
    pub execution_time_ms: u64,
    #[serde(default)]
    pub outputs: HashMap<String, String>,
    #[serde(default)]
    pub summary_md: String,
    #[serde(default)]
    pub peak_cpu_percent: f32,
    #[serde(default)]
    pub peak_ram_mb: u64,
    #[serde(default)]
    pub disk_used_mb: u64,
    pub result_signature: Vec<u8>,
    pub signature: Vec<u8>,
}

impl JobResultMessage {
    pub fn signable_payload(&self) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(b"gitforest:job-result-msg:");
        hasher.update(&self.job_id);
        hasher.update(self.worker_id.0);
        hasher.update(&self.result_id);
        hasher.update(self.exit_code.to_le_bytes());
        hasher.update(&self.stdout_hash);
        hasher.update(&self.stderr_hash);
        hasher.update(&self.stdout_tail);
        hasher.update(&self.stderr_tail);
        for (name, hash) in &self.artifact_hashes {
            hasher.update(name);
            hasher.update(hash);
        }
        let mut blobs: Vec<&crate::types::ArtifactBlob> = self.artifact_blobs.iter().collect();
        blobs.sort_by(|a, b| a.name.cmp(&b.name));
        for blob in blobs {
            hasher.update(&blob.name);
            hasher.update(blob.blob_id.0);
            hasher.update(blob.size.to_le_bytes());
        }
        let mut outputs: Vec<(&String, &String)> = self.outputs.iter().collect();
        outputs.sort_by(|a, b| a.0.cmp(b.0));
        for (key, value) in outputs {
            hasher.update(key);
            hasher.update([0]);
            hasher.update(value);
            hasher.update([0]);
        }
        hasher.update(self.execution_time_ms.to_le_bytes());
        hasher.update(self.peak_cpu_percent.to_le_bytes());
        hasher.update(self.peak_ram_mb.to_le_bytes());
        hasher.update(self.disk_used_mb.to_le_bytes());
        hasher.update(&self.result_signature);
        hasher.finalize().to_vec()
    }

    pub fn verify(&self) -> bool {
        verify_signature(&self.worker_id, &self.signable_payload(), &self.signature).is_ok()
    }
}

/// Submitter cancels a job previously sent to a worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobCancelMessage {
    pub job_id: String,
    pub submitter: PeerId,
    pub reason: String,
    pub signature: Vec<u8>,
}

impl JobCancelMessage {
    pub fn signable_payload(&self) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(b"gitforest:job-cancel:");
        hasher.update(&self.job_id);
        hasher.update(self.submitter.0);
        hasher.update(&self.reason);
        hasher.finalize().to_vec()
    }

    pub fn verify(&self) -> bool {
        verify_signature(&self.submitter, &self.signable_payload(), &self.signature).is_ok()
    }
}

/// Open a payment channel for a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentOpenMessage {
    pub channel_id: String,
    pub job_id: String,
    pub payer: PeerId,
    pub payee: PeerId,
    pub chain: String,
    pub escrow_amount: u64,
    pub payer_address: String,
    pub payee_address: String,
    pub signature: Vec<u8>,
}

/// Release payment after successful job completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentReleaseMessage {
    pub channel_id: String,
    pub job_id: String,
    pub payer: PeerId,
    pub signature: Vec<u8>,
}

/// Dispute a payment (job failed or result incorrect).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentDisputeMessage {
    pub channel_id: String,
    pub job_id: String,
    pub disputer: PeerId,
    pub reason: String,
    pub signature: Vec<u8>,
}

// ============ Large File Storage Messages ============

use crate::types::BlobId;

/// Announce that a peer is seeding a blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobAnnouncement {
    pub peer_id: PeerId,
    pub blob_id: BlobId,
    pub size: u64,
    pub piece_count: u32,
    /// Verified pieces held. None means a full seeder; Some(n) with n below
    /// piece_count marks a partial holder. Advisory only and unsigned:
    /// every served piece is hash-verified on receipt.
    #[serde(default)]
    pub have_pieces: Option<u32>,
    pub signature: Vec<u8>,
}

impl BlobAnnouncement {
    pub fn signable_payload(&self) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(b"gitforest:blob-announce:");
        hasher.update(self.peer_id.0);
        hasher.update(self.blob_id.0);
        hasher.update(self.size.to_le_bytes());
        hasher.update(self.piece_count.to_le_bytes());
        hasher.finalize().to_vec()
    }

    pub fn announces_complete(&self) -> bool {
        self.have_pieces.is_none_or(|have| have >= self.piece_count)
    }

    pub fn verify(&self) -> bool {
        verify_signature(&self.peer_id, &self.signable_payload(), &self.signature).is_ok()
    }
}

/// Query for a blob by content hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobQuery {
    pub blob_id: BlobId,
}

/// Response with blob info and known seeders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobInfoResponse {
    pub blob_id: BlobId,
    pub size: Option<u64>,
    pub seeders: Vec<PeerAddr>,
    pub leecher_count: u32,
}

/// Request specific pieces of a blob from a peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PieceRequest {
    pub blob_id: BlobId,
    pub piece_indices: Vec<u32>,
}

/// Response carrying piece data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PieceResponse {
    pub blob_id: BlobId,
    pub piece_index: u32,
    pub data: Vec<u8>,
}

/// Announce a collection (model, dataset) on the network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionAnnouncement {
    pub owner: PeerId,
    pub manifest_id: String,
    pub name: String,
    pub file_count: u32,
    pub total_size: u64,
    pub signature: Vec<u8>,
}

impl CollectionAnnouncement {
    pub fn signable_payload(&self) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(b"gitforest:collection-announce:");
        hasher.update(self.owner.0);
        hasher.update(&self.manifest_id);
        hasher.update([0]);
        hasher.update(&self.name);
        hasher.update([0]);
        hasher.update(self.file_count.to_le_bytes());
        hasher.update(self.total_size.to_le_bytes());
        hasher.finalize().to_vec()
    }

    pub fn verify(&self) -> bool {
        verify_signature(&self.owner, &self.signable_payload(), &self.signature).is_ok()
    }
}

/// Query for a collection by manifest ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionQuery {
    pub manifest_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAnnouncement {
    pub owner: PeerId,
    pub manifest_id: String,
    pub name: String,
    pub architecture: String,
    pub parameter_count: u64,
    pub quantization: Option<String>,
    pub format: String,
    #[serde(default)]
    pub seeder: Option<PeerId>,
    pub signature: Vec<u8>,
}

/// Advertisement for one workflow-cache key held by a daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheAnnouncement {
    pub owner: PeerId,
    pub key: String,
    pub blob_id: BlobId,
    pub size: u64,
    pub updated_at: i64,
    pub signature: Vec<u8>,
}

impl CacheAnnouncement {
    pub fn signable_payload(&self) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(b"gitforest:cache-announce:");
        hasher.update(self.owner.0);
        hasher.update(&self.key);
        hasher.update([0]);
        hasher.update(self.blob_id.0);
        hasher.update(self.size.to_le_bytes());
        hasher.update(self.updated_at.to_le_bytes());
        hasher.finalize().to_vec()
    }

    pub fn verify(&self) -> bool {
        verify_signature(&self.owner, &self.signable_payload(), &self.signature).is_ok()
    }
}

impl ModelAnnouncement {
    pub fn signable_payload(&self) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(b"gitforest:model-announce:");
        hasher.update(self.owner.0);
        hasher.update(&self.manifest_id);
        hasher.update([0]);
        hasher.update(&self.name);
        hasher.update([0]);
        hasher.update(&self.architecture);
        hasher.update([0]);
        hasher.update(self.parameter_count.to_le_bytes());
        hasher.update(self.quantization.clone().unwrap_or_default().as_bytes());
        hasher.update([0]);
        hasher.update(&self.format);
        hasher.finalize().to_vec()
    }

    pub fn verify(&self) -> bool {
        let signer = self.seeder.as_ref().unwrap_or(&self.owner);
        verify_signature(signer, &self.signable_payload(), &self.signature).is_ok()
    }
}

/// Build the exact bytes a pusher signs for `POST /api/remote/{peer}/{repo}/push`.
///
/// Binds the repo name, the ref updates (as the base64 header value, so no
/// JSON canonicalization is needed), and the bundle bytes. The pusher id is
/// inside the ref updates; the server verifies the signature against that id,
/// which is what stops cluster members from impersonating each other.
pub fn push_signable_payload(repo: &str, refs_b64: &str, bundle: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"gitforest:push:");
    hasher.update(repo.as_bytes());
    hasher.update([0]);
    hasher.update(refs_b64.as_bytes());
    hasher.update([0]);
    hasher.update(Sha256::digest(bundle));
    hasher.finalize().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    fn sample_submission(submitter: &Identity) -> JobSubmissionMessage {
        JobSubmissionMessage {
            job_id: "job-1".into(),
            submitter: submitter.peer_id(),
            repo_owner: submitter.peer_id(),
            repo_name: "demo".into(),
            commit_sha: "abc".into(),
            job_config_json: r#"{"name":"ci"}"#.into(),
            min_cpu_cores: 2,
            min_ram_mb: 4096,
            min_disk_mb: 10240,
            gpu_required: false,
            network_access: false,
            max_payment_sats: 500,
            preferred_chain: "lightning".into(),
            signature: vec![],
        }
    }

    #[test]
    fn test_submission_sign_verify() {
        let submitter = Identity::generate();
        let mut msg = sample_submission(&submitter);
        assert!(!msg.verify());
        let payload = msg.signable_payload();
        msg.signature = submitter.sign(&payload);
        assert!(msg.verify());
        msg.max_payment_sats = 999;
        assert!(!msg.verify());
    }

    #[test]
    fn test_submission_wrong_signer_fails() {
        let submitter = Identity::generate();
        let other = Identity::generate();
        let mut msg = sample_submission(&submitter);
        let payload = msg.signable_payload();
        msg.signature = other.sign(&payload);
        assert!(!msg.verify());
    }

    fn sample_cancel(submitter: &Identity) -> JobCancelMessage {
        JobCancelMessage {
            job_id: "job-1".into(),
            submitter: submitter.peer_id(),
            reason: "stale match".into(),
            signature: vec![],
        }
    }

    #[test]
    fn test_cancel_sign_verify() {
        let submitter = Identity::generate();
        let mut msg = sample_cancel(&submitter);
        assert!(!msg.verify());
        let payload = msg.signable_payload();
        msg.signature = submitter.sign(&payload);
        assert!(msg.verify());
        msg.job_id = "job-2".into();
        assert!(!msg.verify());
    }

    #[test]
    fn test_cancel_wrong_signer_fails() {
        let submitter = Identity::generate();
        let other = Identity::generate();
        let mut msg = sample_cancel(&submitter);
        let payload = msg.signable_payload();
        msg.signature = other.sign(&payload);
        assert!(!msg.verify());
    }

    #[test]
    fn test_result_sign_verify() {
        let worker = Identity::generate();
        let mut msg = JobResultMessage {
            job_id: "job-1".into(),
            worker_id: worker.peer_id(),
            result_id: "res-1".into(),
            exit_code: 0,
            stdout_hash: "aa".into(),
            stderr_hash: "bb".into(),
            stdout_tail: "ok".into(),
            stderr_tail: "".into(),
            artifact_hashes: vec![("a".into(), "cc".into())],
            artifact_blobs: vec![],
            execution_time_ms: 100,
            outputs: HashMap::from([("sha".into(), "abc".into())]),
            summary_md: String::new(),
            peak_cpu_percent: 12.5,
            peak_ram_mb: 256,
            disk_used_mb: 64,
            result_signature: vec![1, 2, 3],
            signature: vec![],
        };
        assert!(!msg.verify());
        let payload = msg.signable_payload();
        msg.signature = worker.sign(&payload);
        assert!(msg.verify());
        msg.exit_code = 1;
        assert!(!msg.verify());
        msg.exit_code = 0;
        let payload = msg.signable_payload();
        msg.signature = worker.sign(&payload);
        assert!(msg.verify());
        msg.outputs.insert("sha".into(), "tampered".into());
        assert!(!msg.verify());
        msg.exit_code = 0;
        let payload = msg.signable_payload();
        msg.signature = worker.sign(&payload);
        assert!(msg.verify());
        msg.peak_ram_mb = 257;
        assert!(!msg.verify());
    }

    #[test]
    fn test_push_payload_binds_all_fields() {
        let base = push_signable_payload("repo", "refs64", b"bundle");
        assert_ne!(base, push_signable_payload("other", "refs64", b"bundle"));
        assert_ne!(base, push_signable_payload("repo", "other64", b"bundle"));
        assert_ne!(base, push_signable_payload("repo", "refs64", b"other"));
        assert_eq!(base, push_signable_payload("repo", "refs64", b"bundle"));
    }

    fn sample_blob_announcement(peer: &Identity) -> BlobAnnouncement {
        BlobAnnouncement {
            peer_id: peer.peer_id(),
            blob_id: crate::types::BlobId([7u8; 32]),
            size: 1024,
            piece_count: 4,
            have_pieces: None,
            signature: vec![],
        }
    }

    #[test]
    fn test_blob_announcement_completeness() {
        let peer = Identity::generate();
        let full = sample_blob_announcement(&peer);
        assert!(full.announces_complete());
        let mut partial = sample_blob_announcement(&peer);
        partial.have_pieces = Some(2);
        assert!(!partial.announces_complete());
        partial.have_pieces = Some(4);
        assert!(partial.announces_complete());
        let legacy: BlobAnnouncement = serde_json::from_str(
            r#"{"peer_id":"d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4","blob_id":"0707070707070707070707070707070707070707070707070707070707070707","size":1024,"piece_count":4,"signature":[]}"#,
        )
        .unwrap();
        assert!(legacy.announces_complete());
    }

    #[test]
    fn test_blob_announcement_binds_blob_id() {
        let peer = Identity::generate();
        let mut ann = sample_blob_announcement(&peer);
        let payload = ann.signable_payload();
        ann.signature = peer.sign(&payload);
        assert!(ann.verify());
        ann.blob_id = crate::types::BlobId([9u8; 32]);
        assert!(!ann.verify(), "transplanted signature must fail");
    }

    #[test]
    fn test_blob_announcement_wrong_signer_fails() {
        let peer = Identity::generate();
        let other = Identity::generate();
        let mut ann = sample_blob_announcement(&peer);
        let payload = ann.signable_payload();
        ann.signature = other.sign(&payload);
        assert!(!ann.verify());
    }

    #[test]
    fn test_collection_announcement_binds_manifest() {
        let owner = Identity::generate();
        let mut ann = CollectionAnnouncement {
            owner: owner.peer_id(),
            manifest_id: "model-v1".into(),
            name: "llama-weights".into(),
            file_count: 2,
            total_size: 2048,
            signature: vec![],
        };
        let payload = ann.signable_payload();
        ann.signature = owner.sign(&payload);
        assert!(ann.verify());
        ann.manifest_id = "other-model".into();
        assert!(!ann.verify(), "transplanted signature must fail");
    }

    #[test]
    fn test_cache_announcement_binds_fields() {
        let owner = Identity::generate();
        let other = Identity::generate();
        let mut ann = CacheAnnouncement {
            owner: owner.peer_id(),
            key: "linux-cargo-abc".into(),
            blob_id: crate::types::BlobId([3u8; 32]),
            size: 4096,
            updated_at: 1_700_000_000,
            signature: vec![],
        };
        assert!(!ann.verify());
        let payload = ann.signable_payload();
        ann.signature = owner.sign(&payload);
        assert!(ann.verify());
        ann.blob_id = crate::types::BlobId([9u8; 32]);
        assert!(!ann.verify());
        ann.blob_id = crate::types::BlobId([3u8; 32]);
        ann.size = 4097;
        assert!(!ann.verify());
        ann.size = 4096;
        ann.key = "linux-cargo-abd".into();
        assert!(!ann.verify());
        ann.key = "linux-cargo-abc".into();
        let payload = ann.signable_payload();
        ann.signature = owner.sign(&payload);
        ann.owner = other.peer_id();
        assert!(!ann.verify());
    }

    #[test]
    fn test_model_announcement_binds_fields() {
        let owner = Identity::generate();
        let other = Identity::generate();
        let mut ann = ModelAnnouncement {
            owner: owner.peer_id(),
            manifest_id: "manifest-1".into(),
            name: "llama-7b".into(),
            architecture: "llama".into(),
            parameter_count: 7_000_000_000,
            quantization: Some("q4_0".into()),
            format: "gguf".into(),
            seeder: None,
            signature: vec![],
        };
        let payload = ann.signable_payload();
        ann.signature = owner.sign(&payload);
        assert!(ann.verify());
        ann.parameter_count = 13_000_000_000;
        assert!(!ann.verify());
        let payload = ann.signable_payload();
        ann.signature = owner.sign(&payload);
        ann.owner = other.peer_id();
        assert!(!ann.verify());
    }

    #[test]
    fn test_model_announcement_seeder_reannounce() {
        let owner = Identity::generate();
        let seeder = Identity::generate();
        let mut ann = ModelAnnouncement {
            owner: owner.peer_id(),
            manifest_id: "manifest-1".into(),
            name: "llama-7b".into(),
            architecture: "llama".into(),
            parameter_count: 7_000_000_000,
            quantization: None,
            format: "gguf".into(),
            seeder: Some(seeder.peer_id()),
            signature: vec![],
        };
        let payload = ann.signable_payload();
        ann.signature = seeder.sign(&payload);
        assert!(ann.verify());
        ann.signature = owner.sign(&payload);
        assert!(!ann.verify());
        let legacy = serde_json::json!({
            "owner": owner.peer_id().to_string(),
            "manifest_id": "manifest-1",
            "name": "llama-7b",
            "architecture": "llama",
            "parameter_count": 7_000_000_000u64,
            "quantization": null,
            "format": "gguf",
            "signature": [],
        });
        let parsed: ModelAnnouncement = serde_json::from_value(legacy).unwrap();
        assert!(parsed.seeder.is_none());
    }
}
