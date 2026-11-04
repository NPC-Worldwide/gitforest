use gt_core::identity::PeerId;
use gt_core::protocol::ModelAnnouncement;
use gt_core::types::CollectionManifest;
use gt_lfs::model::{ModelFormat, ModelMetadata, ModelQuery};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRecord {
    pub manifest_id: String,
    pub name: String,
    pub architecture: String,
    pub parameter_count: u64,
    pub quantization: Option<String>,
    pub format: String,
    pub owner: PeerId,
    pub seeders: Vec<PeerId>,
    pub local: bool,
}

impl ModelRecord {
    pub fn from_manifest(manifest: &CollectionManifest, local: bool) -> Option<Self> {
        let meta = ModelMetadata::from_manifest(manifest).ok()?;
        Some(ModelRecord {
            manifest_id: manifest.id.clone(),
            name: manifest.name.clone(),
            architecture: meta.architecture,
            parameter_count: meta.parameter_count,
            quantization: meta.quantization,
            format: meta.format.as_str(),
            owner: manifest.owner.clone(),
            seeders: vec![manifest.owner.clone()],
            local,
        })
    }

    pub fn from_announcement(ann: &ModelAnnouncement) -> Option<Self> {
        if !ann.verify() {
            return None;
        }
        Some(ModelRecord {
            manifest_id: ann.manifest_id.clone(),
            name: ann.name.clone(),
            architecture: ann.architecture.clone(),
            parameter_count: ann.parameter_count,
            quantization: ann.quantization.clone(),
            format: ann.format.clone(),
            owner: ann.owner.clone(),
            seeders: vec![ann.seeder.clone().unwrap_or_else(|| ann.owner.clone())],
            local: false,
        })
    }

    pub fn merge_seeder(&mut self, seeder: PeerId) {
        if !self.seeders.contains(&seeder) {
            self.seeders.push(seeder);
        }
    }

    pub fn matches(&self, query: &ModelQuery) -> bool {
        if let Some(text) = &query.text {
            let needle = text.to_lowercase();
            let hay = format!("{} {}", self.name, self.architecture).to_lowercase();
            if !hay.contains(&needle) {
                return false;
            }
        }
        if let Some(arch) = &query.architecture {
            if !self.architecture.eq_ignore_ascii_case(arch) {
                return false;
            }
        }
        if let Some(quant) = &query.quantization {
            if self.quantization.as_deref().unwrap_or("").to_lowercase() != quant.to_lowercase() {
                return false;
            }
        }
        if let Some(format) = &query.format {
            if ModelFormat::parse(&self.format) != *format {
                return false;
            }
        }
        if let Some(min) = query.min_params {
            if self.parameter_count < min {
                return false;
            }
        }
        if let Some(max) = query.max_params {
            if self.parameter_count > max {
                return false;
            }
        }
        true
    }
}

pub fn local_model_records(
    manifests: &[&CollectionManifest],
    query: &ModelQuery,
) -> Vec<ModelRecord> {
    manifests
        .iter()
        .filter_map(|m| ModelRecord::from_manifest(m, true))
        .filter(|r| r.matches(query))
        .collect()
}

pub fn merge_records(
    local: Vec<ModelRecord>,
    remote: &HashMap<String, ModelRecord>,
    query: &ModelQuery,
) -> Vec<ModelRecord> {
    let mut out = local;
    for record in &mut out {
        if let Some(known) = remote.get(&record.manifest_id) {
            for seeder in &known.seeders {
                record.merge_seeder(seeder.clone());
            }
        }
    }
    let mut seen: Vec<String> = out.iter().map(|r| r.manifest_id.clone()).collect();
    let mut remotes: Vec<&ModelRecord> = remote.values().collect();
    remotes.sort_by(|a, b| a.name.cmp(&b.name));
    for record in remotes {
        if seen.contains(&record.manifest_id) {
            continue;
        }
        if !record.matches(query) {
            continue;
        }
        seen.push(record.manifest_id.clone());
        out.push(record.clone());
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn build_announcement(
    identity: &gt_core::identity::Identity,
    manifest: &CollectionManifest,
) -> Option<ModelAnnouncement> {
    let meta = ModelMetadata::from_manifest(manifest).ok()?;
    let mut ann = ModelAnnouncement {
        owner: identity.peer_id(),
        manifest_id: manifest.id.clone(),
        name: manifest.name.clone(),
        architecture: meta.architecture,
        parameter_count: meta.parameter_count,
        quantization: meta.quantization,
        format: meta.format.as_str(),
        seeder: None,
        signature: vec![],
    };
    ann.signature = identity.sign(&ann.signable_payload());
    Some(ann)
}

pub fn build_seeder_announcement(
    identity: &gt_core::identity::Identity,
    manifest: &CollectionManifest,
) -> Option<ModelAnnouncement> {
    let meta = ModelMetadata::from_manifest(manifest).ok()?;
    let mut ann = ModelAnnouncement {
        owner: manifest.owner.clone(),
        manifest_id: manifest.id.clone(),
        name: manifest.name.clone(),
        architecture: meta.architecture,
        parameter_count: meta.parameter_count,
        quantization: meta.quantization,
        format: meta.format.as_str(),
        seeder: Some(identity.peer_id()),
        signature: vec![],
    };
    ann.signature = identity.sign(&ann.signable_payload());
    Some(ann)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_core::identity::Identity;
    use gt_lfs::collection::CollectionStore;

    fn manifest_with_meta(
        owner: &Identity,
        name: &str,
        arch: &str,
        params: &str,
        quant: Option<&str>,
    ) -> CollectionManifest {
        let mut meta = HashMap::new();
        meta.insert("architecture".to_string(), arch.to_string());
        meta.insert("parameter_count".to_string(), params.to_string());
        meta.insert("format".to_string(), "gguf".to_string());
        if let Some(q) = quant {
            meta.insert("quantization".to_string(), q.to_string());
        }
        CollectionManifest::new(
            owner,
            name.to_string(),
            "test model".to_string(),
            vec![],
            meta,
        )
    }

    #[test]
    fn records_roundtrip_manifest_and_announcement() {
        let owner = Identity::generate();
        let manifest = manifest_with_meta(&owner, "llama-7b", "llama", "7B", Some("q4_0"));
        let record = ModelRecord::from_manifest(&manifest, true).unwrap();
        assert_eq!(record.parameter_count, 7_000_000_000);
        assert!(record.local);
        let ann = build_announcement(&owner, &manifest).unwrap();
        assert!(ann.verify());
        let remote = ModelRecord::from_announcement(&ann).unwrap();
        assert!(!remote.local);
        assert_eq!(remote.name, "llama-7b");
        let mut tampered = ann.clone();
        tampered.name = "other".to_string();
        assert!(ModelRecord::from_announcement(&tampered).is_none());
    }

    #[test]
    fn seeder_announcement_keeps_owner_and_adds_seeder() {
        let owner = Identity::generate();
        let seeder = Identity::generate();
        let manifest = manifest_with_meta(&owner, "llama-7b", "llama", "7B", Some("q4_0"));
        let ann = build_seeder_announcement(&seeder, &manifest).unwrap();
        assert_eq!(ann.owner, owner.peer_id());
        assert_eq!(ann.seeder, Some(seeder.peer_id()));
        assert!(ann.verify());
        let record = ModelRecord::from_announcement(&ann).unwrap();
        assert_eq!(record.owner, owner.peer_id());
        assert_eq!(record.seeders, vec![seeder.peer_id()]);
        assert!(!record.local);
    }

    #[test]
    fn merge_unions_learned_seeders_into_local() {
        let owner = Identity::generate();
        let seeder = Identity::generate();
        let manifest = manifest_with_meta(&owner, "llama-7b", "llama", "7B", None);
        let mut store = CollectionStore::new();
        store.add(manifest.clone());
        let query = ModelQuery::default();
        let local = local_model_records(&store.list(), &query);
        let mut remote: HashMap<String, ModelRecord> = HashMap::new();
        let ann = build_seeder_announcement(&seeder, &manifest).unwrap();
        remote.insert(
            manifest.id.clone(),
            ModelRecord::from_announcement(&ann).unwrap(),
        );
        let merged = merge_records(local, &remote, &query);
        assert_eq!(merged.len(), 1);
        assert!(merged[0].local);
        assert!(merged[0].seeders.contains(&owner.peer_id()));
        assert!(merged[0].seeders.contains(&seeder.peer_id()));
    }

    #[test]
    fn plain_collections_are_not_models() {
        let owner = Identity::generate();
        let plain = CollectionManifest::new(
            &owner,
            "plain".to_string(),
            "no meta".to_string(),
            vec![],
            HashMap::new(),
        );
        assert!(ModelRecord::from_manifest(&plain, true).is_none());
        assert!(build_announcement(&owner, &plain).is_none());
    }

    #[test]
    fn merge_prefers_local_and_filters() {
        let owner = Identity::generate();
        let one = manifest_with_meta(&owner, "llama-7b", "llama", "7B", Some("q4_0"));
        let two = manifest_with_meta(&owner, "bert-base", "bert", "110M", None);
        let mut store = CollectionStore::new();
        store.add(one.clone());
        store.add(two.clone());
        let all = store.list();
        let query = ModelQuery::default();
        let local = local_model_records(&all, &query);
        assert_eq!(local.len(), 2);
        let mut remote: HashMap<String, ModelRecord> = HashMap::new();
        let ann = build_announcement(&owner, &one).unwrap();
        remote.insert(
            one.id.clone(),
            ModelRecord::from_announcement(&ann).unwrap(),
        );
        let other_owner = Identity::generate();
        let three = manifest_with_meta(&other_owner, "mistral-7b", "mistral", "7B", None);
        let ann3 = build_announcement(&other_owner, &three).unwrap();
        remote.insert(
            three.id.clone(),
            ModelRecord::from_announcement(&ann3).unwrap(),
        );
        let merged = merge_records(local, &remote, &query);
        assert_eq!(merged.len(), 3);
        let dupes: Vec<&ModelRecord> = merged.iter().filter(|r| r.manifest_id == one.id).collect();
        assert_eq!(dupes.len(), 1);
        assert!(dupes[0].local);
        assert!(dupes[0].seeders.contains(&owner.peer_id()));
        let arch_query = ModelQuery {
            architecture: Some("bert".to_string()),
            ..Default::default()
        };
        let filtered = merge_records(
            local_model_records(&store.list(), &arch_query),
            &remote,
            &arch_query,
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "bert-base");
    }
}
