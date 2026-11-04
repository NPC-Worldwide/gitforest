use gt_core::identity::PeerId;
use gt_core::types::CollectionManifest;
use std::collections::HashMap;

/// In-memory store for collection manifests.
pub struct CollectionStore {
    manifests: HashMap<String, CollectionManifest>,
}

impl CollectionStore {
    pub fn new() -> Self {
        CollectionStore {
            manifests: HashMap::new(),
        }
    }

    pub fn add(&mut self, manifest: CollectionManifest) {
        self.manifests.insert(manifest.id.clone(), manifest);
    }

    pub fn get(&self, id: &str) -> Option<&CollectionManifest> {
        self.manifests.get(id)
    }

    pub fn list(&self) -> Vec<&CollectionManifest> {
        self.manifests.values().collect()
    }

    pub fn by_owner(&self, owner: &PeerId) -> Vec<&CollectionManifest> {
        self.manifests
            .values()
            .filter(|m| &m.owner == owner)
            .collect()
    }

    pub fn remove(&mut self, id: &str) -> Option<CollectionManifest> {
        self.manifests.remove(id)
    }
}

impl Default for CollectionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_core::identity::Identity;
    use gt_core::types::{BlobId, CollectionFile};

    fn sample_files() -> Vec<CollectionFile> {
        vec![
            CollectionFile {
                blob_id: BlobId([0x11; 32]),
                path: "weights/layer1.safetensors".into(),
                size: 500_000_000,
                media_type: Some("application/octet-stream".into()),
            },
            CollectionFile {
                blob_id: BlobId([0x22; 32]),
                path: "weights/layer2.safetensors".into(),
                size: 500_000_000,
                media_type: Some("application/octet-stream".into()),
            },
            CollectionFile {
                blob_id: BlobId([0x33; 32]),
                path: "tokenizer.json".into(),
                size: 2_000_000,
                media_type: Some("application/json".into()),
            },
        ]
    }

    #[test]
    fn test_create_and_verify_manifest() {
        let owner = Identity::generate();
        let files = sample_files();

        let manifest = CollectionManifest::new(
            &owner,
            "llama-7b-q4".into(),
            "Quantized LLaMA 7B model weights".into(),
            files,
            HashMap::from([
                ("architecture".into(), "llama".into()),
                ("quantization".into(), "q4_0".into()),
                ("license".into(), "MIT".into()),
            ]),
        );

        assert!(manifest.verify());
        assert_eq!(manifest.owner, owner.peer_id());
        assert_eq!(manifest.files.len(), 3);
        assert_eq!(manifest.total_size(), 1_002_000_000);
        assert_eq!(manifest.metadata.get("architecture").unwrap(), "llama");
    }

    #[test]
    fn test_tampered_manifest_fails() {
        let owner = Identity::generate();
        let mut manifest = CollectionManifest::new(
            &owner,
            "my-model".into(),
            "Original description".into(),
            sample_files(),
            HashMap::new(),
        );

        assert!(manifest.verify());

        manifest.name = "tampered-name".into();
        assert!(!manifest.verify());
    }

    #[test]
    fn test_wrong_signer_fails() {
        let owner = Identity::generate();
        let imposter = Identity::generate();

        let mut manifest = CollectionManifest::new(
            &owner,
            "model".into(),
            "desc".into(),
            sample_files(),
            HashMap::new(),
        );

        manifest.owner = imposter.peer_id();
        assert!(!manifest.verify());
    }

    #[test]
    fn test_collection_store() {
        let alice = Identity::generate();
        let bob = Identity::generate();

        let m1 = CollectionManifest::new(
            &alice,
            "model-a".into(),
            "Alice's model".into(),
            vec![],
            HashMap::new(),
        );
        let m2 = CollectionManifest::new(
            &bob,
            "dataset-b".into(),
            "Bob's dataset".into(),
            sample_files(),
            HashMap::new(),
        );

        let m1_id = m1.id.clone();
        let m2_id = m2.id.clone();

        let mut store = CollectionStore::new();
        store.add(m1);
        store.add(m2);

        assert_eq!(store.list().len(), 2);
        assert!(store.get(&m1_id).is_some());
        assert!(store.get(&m2_id).is_some());

        assert_eq!(store.by_owner(&alice.peer_id()).len(), 1);
        assert_eq!(store.by_owner(&bob.peer_id()).len(), 1);

        store.remove(&m1_id);
        assert_eq!(store.list().len(), 1);
        assert!(store.get(&m1_id).is_none());
    }
}
