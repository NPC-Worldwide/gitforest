use anyhow::{bail, Result};
use gt_core::types::{CollectionFile, CollectionManifest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelFormat {
    Safetensors,
    Gguf,
    Pytorch,
    Onnx,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub architecture: String,
    pub parameter_count: u64,
    pub quantization: Option<String>,
    pub context_length: Option<u64>,
    pub format: ModelFormat,
    pub license: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ModelQuery {
    pub text: Option<String>,
    pub architecture: Option<String>,
    pub quantization: Option<String>,
    pub format: Option<ModelFormat>,
    pub min_params: Option<u64>,
    pub max_params: Option<u64>,
}

impl ModelFormat {
    pub fn as_str(&self) -> String {
        match self {
            ModelFormat::Safetensors => "safetensors".to_string(),
            ModelFormat::Gguf => "gguf".to_string(),
            ModelFormat::Pytorch => "pytorch".to_string(),
            ModelFormat::Onnx => "onnx".to_string(),
            ModelFormat::Other(s) => s.clone(),
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.to_lowercase().as_str() {
            "safetensors" => ModelFormat::Safetensors,
            "gguf" => ModelFormat::Gguf,
            "pytorch" | "bin" | "pt" | "pth" => ModelFormat::Pytorch,
            "onnx" => ModelFormat::Onnx,
            other => ModelFormat::Other(other.to_string()),
        }
    }
}

pub fn parse_param_count(value: &str) -> Result<u64> {
    let trimmed = value.trim().to_lowercase().replace('_', "");
    if trimmed.is_empty() {
        bail!("parameter count is empty");
    }
    let (digits, scale) = if let Some(stripped) = trimmed.strip_suffix('b') {
        (stripped, 1_000_000_000u64)
    } else if let Some(stripped) = trimmed.strip_suffix('m') {
        (stripped, 1_000_000u64)
    } else if let Some(stripped) = trimmed.strip_suffix('k') {
        (stripped, 1_000u64)
    } else {
        (trimmed.as_str(), 1u64)
    };
    let base: f64 = digits
        .parse()
        .map_err(|_| anyhow::anyhow!("bad parameter count {value}"))?;
    if !base.is_finite() || base <= 0.0 {
        bail!("parameter count must be positive");
    }
    Ok((base * scale as f64) as u64)
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

impl ModelMetadata {
    pub fn validate(&self) -> Result<()> {
        if !valid_token(&self.architecture) {
            bail!("architecture must be 1-64 chars of letters, digits, - _ .");
        }
        if self.parameter_count == 0 {
            bail!("parameter count must be positive");
        }
        if let Some(q) = &self.quantization {
            if !valid_token(q) {
                bail!("quantization must be 1-64 chars of letters, digits, - _ .");
            }
        }
        if let Some(ctx) = self.context_length {
            if ctx == 0 {
                bail!("context length must be positive");
            }
        }
        Ok(())
    }

    pub fn to_map(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        out.insert("architecture".to_string(), self.architecture.clone());
        out.insert(
            "parameter_count".to_string(),
            self.parameter_count.to_string(),
        );
        out.insert("format".to_string(), self.format.as_str());
        if let Some(q) = &self.quantization {
            out.insert("quantization".to_string(), q.clone());
        }
        if let Some(ctx) = self.context_length {
            out.insert("context_length".to_string(), ctx.to_string());
        }
        if let Some(license) = &self.license {
            out.insert("license".to_string(), license.clone());
        }
        out.insert("kind".to_string(), "llm".to_string());
        out
    }

    pub fn from_map(map: &HashMap<String, String>) -> Result<Self> {
        let architecture = map.get("architecture").cloned().unwrap_or_default();
        let params_raw = map.get("parameter_count").cloned().unwrap_or_default();
        if architecture.is_empty() {
            bail!("collection has no architecture");
        }
        if params_raw.is_empty() {
            bail!("collection has no parameter_count");
        }
        let meta = ModelMetadata {
            architecture,
            parameter_count: parse_param_count(&params_raw)?,
            quantization: map.get("quantization").cloned(),
            context_length: map
                .get("context_length")
                .and_then(|v| v.parse::<u64>().ok()),
            format: map
                .get("format")
                .map(|v| ModelFormat::parse(v))
                .unwrap_or(ModelFormat::Other("unknown".to_string())),
            license: map.get("license").cloned(),
        };
        meta.validate()?;
        Ok(meta)
    }

    pub fn from_manifest(manifest: &CollectionManifest) -> Result<Self> {
        Self::from_map(&manifest.metadata)
    }
}

pub fn model_name_dht_address(name: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"gitforest:model-name:");
    hasher.update(name.to_lowercase().as_bytes());
    hasher.finalize().into()
}

pub fn model_arch_dht_address(architecture: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"gitforest:model-arch:");
    hasher.update(architecture.to_lowercase().as_bytes());
    hasher.finalize().into()
}

pub fn manifest_matches(manifest: &CollectionManifest, query: &ModelQuery) -> bool {
    if let Some(text) = &query.text {
        let needle = text.to_lowercase();
        let hay = format!("{} {}", manifest.name, manifest.description).to_lowercase();
        if !hay.contains(&needle) {
            return false;
        }
    }
    if let Some(arch) = &query.architecture {
        let got = manifest
            .metadata
            .get("architecture")
            .cloned()
            .unwrap_or_default();
        if !got.eq_ignore_ascii_case(arch) {
            return false;
        }
    }
    if let Some(quant) = &query.quantization {
        let got = manifest
            .metadata
            .get("quantization")
            .cloned()
            .unwrap_or_default();
        if !got.eq_ignore_ascii_case(quant) {
            return false;
        }
    }
    if let Some(format) = &query.format {
        let got = manifest
            .metadata
            .get("format")
            .map(|v| ModelFormat::parse(v));
        if got.as_ref() != Some(format) {
            return false;
        }
    }
    if query.min_params.is_some() || query.max_params.is_some() {
        let params = manifest
            .metadata
            .get("parameter_count")
            .and_then(|v| parse_param_count(v).ok());
        let Some(count) = params else {
            return false;
        };
        if let Some(min) = query.min_params {
            if count < min {
                return false;
            }
        }
        if let Some(max) = query.max_params {
            if count > max {
                return false;
            }
        }
    }
    true
}

pub fn filter_manifests<'a>(
    manifests: &[&'a CollectionManifest],
    query: &ModelQuery,
) -> Vec<&'a CollectionManifest> {
    manifests
        .iter()
        .copied()
        .filter(|m| manifest_matches(m, query))
        .collect()
}

pub fn is_gguf_magic(prefix: &[u8]) -> bool {
    prefix.len() >= 4 && &prefix[..4] == b"GGUF"
}

pub fn is_safetensors_header(prefix: &[u8]) -> bool {
    if prefix.len() < 8 {
        return false;
    }
    let mut len_bytes = [0u8; 8];
    len_bytes.copy_from_slice(&prefix[..8]);
    let header_len = u64::from_le_bytes(len_bytes) as usize;
    if header_len == 0 || header_len > 64 * 1024 * 1024 {
        return false;
    }
    if prefix.len() < 8 + header_len.min(prefix.len().saturating_sub(8)) {
        return prefix.len() >= 8;
    }
    let end = 8 + header_len;
    if prefix.len() < end {
        return true;
    }
    serde_json::from_slice::<serde_json::Value>(&prefix[8..end]).is_ok()
}

pub fn detect_model_format(file_name: &str, prefix: &[u8]) -> ModelFormat {
    if is_gguf_magic(prefix) {
        return ModelFormat::Gguf;
    }
    let ext = file_name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "gguf" => ModelFormat::Gguf,
        "safetensors" => ModelFormat::Safetensors,
        "bin" | "pt" | "pth" => ModelFormat::Pytorch,
        "onnx" => ModelFormat::Onnx,
        other => ModelFormat::Other(other.to_string()),
    }
}

pub fn shard_base(path: &str) -> String {
    let (dir, file) = match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    };
    let stem = match file.rfind('.') {
        Some(i) => &file[..i],
        None => file,
    };
    if let Some(of_pos) = stem.rfind("-of-") {
        let total = &stem[of_pos + 4..];
        let left = &stem[..of_pos];
        if total.len() == 5
            && total.chars().all(|c| c.is_ascii_digit())
            && left.len() > 6
            && left[left.len() - 6..].starts_with('-')
            && left[left.len() - 5..].chars().all(|c| c.is_ascii_digit())
        {
            let base = &left[..left.len() - 6];
            if dir.is_empty() {
                return base.to_string();
            }
            return format!("{dir}/{base}");
        }
    }
    if dir.is_empty() {
        return stem.to_string();
    }
    format!("{dir}/{stem}")
}

pub fn group_shards(files: &[CollectionFile]) -> HashMap<String, Vec<&CollectionFile>> {
    let mut out: HashMap<String, Vec<&CollectionFile>> = HashMap::new();
    for file in files {
        out.entry(shard_base(&file.path)).or_default().push(file);
    }
    for members in out.values_mut() {
        members.sort_by(|a, b| a.path.cmp(&b.path));
    }
    out
}

fn safe_member_path(path: &str) -> Result<std::path::PathBuf> {
    if path.is_empty() || path.len() > 512 {
        bail!("bad collection member path");
    }
    let mut out = std::path::PathBuf::new();
    for part in path.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.len() > 255
            || part.chars().any(|c| c.is_control())
        {
            bail!("bad collection member path");
        }
        out.push(part);
    }
    Ok(out)
}

pub const MATERIALIZED_MARKER: &str = ".gitforest-materialized";

pub fn materialize_manifest(
    store: &crate::store::BlobStore,
    manifest: &CollectionManifest,
    out_dir: &std::path::Path,
) -> Result<Vec<std::path::PathBuf>> {
    if manifest.files.is_empty() {
        bail!("manifest has no files");
    }
    let marker = out_dir.join(MATERIALIZED_MARKER);
    if marker.exists() {
        let mut out = Vec::new();
        for file in &manifest.files {
            out.push(out_dir.join(safe_member_path(&file.path)?));
        }
        return Ok(out);
    }
    std::fs::create_dir_all(out_dir)?;
    let mut out = Vec::new();
    for file in &manifest.files {
        let rel = safe_member_path(&file.path)?;
        let dest = out_dir.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !store.has(&file.blob_id) {
            bail!(
                "blob {} for {} is not in the local store",
                file.blob_id,
                file.path
            );
        }
        std::fs::copy(store.blob_path(&file.blob_id), &dest)?;
        let actual = std::fs::metadata(&dest)?.len();
        if actual != file.size {
            let _ = std::fs::remove_file(&dest);
            bail!("size mismatch materializing {}", file.path);
        }
        out.push(dest);
    }
    std::fs::write(&marker, manifest.id.as_bytes())?;
    Ok(out)
}

pub fn pick_weights_file(files: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    let rank = |p: &std::path::PathBuf| -> u32 {
        match p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase()
            .as_str()
        {
            "gguf" => 0,
            "safetensors" | "st" => 1,
            "bin" | "pt" | "pth" => 2,
            "onnx" => 3,
            _ => 9,
        }
    };
    files.iter().min_by_key(|p| rank(p)).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_core::identity::Identity;

    fn sample_manifest() -> CollectionManifest {
        let owner = Identity::generate();
        let meta = ModelMetadata {
            architecture: "llama".to_string(),
            parameter_count: 7_000_000_000,
            quantization: Some("q4_0".to_string()),
            context_length: Some(4096),
            format: ModelFormat::Gguf,
            license: Some("MIT".to_string()),
        }
        .to_map();
        CollectionManifest::new(
            &owner,
            "llama-7b-q4".to_string(),
            "Quantized LLaMA 7B".to_string(),
            vec![],
            meta,
        )
    }

    #[test]
    fn metadata_roundtrips_through_map() {
        let meta = ModelMetadata {
            architecture: "mistral".to_string(),
            parameter_count: 7_000_000_000,
            quantization: Some("q5_K_M".to_string()),
            context_length: Some(8192),
            format: ModelFormat::Safetensors,
            license: None,
        };
        assert!(meta.validate().is_ok());
        let map = meta.to_map();
        let back = ModelMetadata::from_map(&map).unwrap();
        assert_eq!(back.architecture, "mistral");
        assert_eq!(back.parameter_count, 7_000_000_000);
        assert_eq!(back.quantization.unwrap(), "q5_K_M");
        assert_eq!(back.context_length.unwrap(), 8192);
        assert_eq!(back.format, ModelFormat::Safetensors);
    }

    #[test]
    fn bad_metadata_fails_validation() {
        let empty_arch = ModelMetadata {
            architecture: "".to_string(),
            parameter_count: 100,
            quantization: None,
            context_length: None,
            format: ModelFormat::Gguf,
            license: None,
        };
        assert!(empty_arch.validate().is_err());
        let zero_params = ModelMetadata {
            architecture: "llama".to_string(),
            parameter_count: 0,
            quantization: None,
            context_length: None,
            format: ModelFormat::Gguf,
            license: None,
        };
        assert!(zero_params.validate().is_err());
        let bad_quant = ModelMetadata {
            architecture: "llama".to_string(),
            parameter_count: 100,
            quantization: Some("has space".to_string()),
            context_length: None,
            format: ModelFormat::Gguf,
            license: None,
        };
        assert!(bad_quant.validate().is_err());
    }

    #[test]
    fn param_suffixes_parse() {
        assert_eq!(parse_param_count("7000000000").unwrap(), 7_000_000_000);
        assert_eq!(parse_param_count("7B").unwrap(), 7_000_000_000);
        assert_eq!(parse_param_count("7b").unwrap(), 7_000_000_000);
        assert_eq!(parse_param_count("1.5B").unwrap(), 1_500_000_000);
        assert_eq!(parse_param_count("350M").unwrap(), 350_000_000);
        assert_eq!(parse_param_count("90k").unwrap(), 90_000);
        assert!(parse_param_count("").is_err());
        assert!(parse_param_count("zero").is_err());
    }

    #[test]
    fn model_addresses_are_stable_and_scoped() {
        let a = model_name_dht_address("Llama-7B");
        let b = model_name_dht_address("llama-7b");
        assert_eq!(a, b);
        assert_ne!(a, model_name_dht_address("mistral-7b"));
        assert_ne!(a, model_arch_dht_address("llama-7b"));
    }

    #[test]
    fn query_filters_match_expected_manifests() {
        let one = sample_manifest();
        let owner = Identity::generate();
        let mut other_meta = HashMap::new();
        other_meta.insert("architecture".to_string(), "bert".to_string());
        other_meta.insert("parameter_count".to_string(), "110M".to_string());
        other_meta.insert("format".to_string(), "safetensors".to_string());
        let two = CollectionManifest::new(
            &owner,
            "bert-base".to_string(),
            "encoder".to_string(),
            vec![],
            other_meta,
        );
        let all = vec![&one, &two];
        let arch = ModelQuery {
            architecture: Some("llama".to_string()),
            ..Default::default()
        };
        assert_eq!(filter_manifests(&all, &arch).len(), 1);
        let text = ModelQuery {
            text: Some("encoder".to_string()),
            ..Default::default()
        };
        assert_eq!(filter_manifests(&all, &text).len(), 1);
        let big = ModelQuery {
            min_params: Some(1_000_000_000),
            ..Default::default()
        };
        assert_eq!(filter_manifests(&all, &big).len(), 1);
        let none = ModelQuery {
            architecture: Some("gpt".to_string()),
            ..Default::default()
        };
        assert!(filter_manifests(&all, &none).is_empty());
    }

    #[test]
    fn format_detection_uses_magic_then_extension() {
        assert_eq!(detect_model_format("x.bin", b"GGUFrest"), ModelFormat::Gguf);
        assert_eq!(
            detect_model_format("model.safetensors", b"12345678"),
            ModelFormat::Safetensors
        );
        assert_eq!(detect_model_format("w.bin", b"xxxx"), ModelFormat::Pytorch);
        assert_eq!(detect_model_format("m.onnx", b"xxxx"), ModelFormat::Onnx);
    }

    #[test]
    fn shard_grouping_collects_split_weights() {
        let files = vec![
            CollectionFile {
                blob_id: gt_core::types::BlobId([1u8; 32]),
                path: "weights/model-00001-of-00004.safetensors".to_string(),
                size: 1,
                media_type: None,
            },
            CollectionFile {
                blob_id: gt_core::types::BlobId([2u8; 32]),
                path: "weights/model-00002-of-00004.safetensors".to_string(),
                size: 1,
                media_type: None,
            },
            CollectionFile {
                blob_id: gt_core::types::BlobId([3u8; 32]),
                path: "tokenizer.json".to_string(),
                size: 1,
                media_type: None,
            },
        ];
        let groups = group_shards(&files);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups["weights/model"].len(), 2);
        assert_eq!(groups["tokenizer"].len(), 1);
    }

    #[test]
    fn manifest_without_model_fields_is_rejected() {
        let owner = Identity::generate();
        let plain = CollectionManifest::new(
            &owner,
            "plain".to_string(),
            "no fields".to_string(),
            vec![],
            HashMap::new(),
        );
        assert!(ModelMetadata::from_manifest(&plain).is_err());
        let good = sample_manifest();
        assert!(ModelMetadata::from_manifest(&good).is_ok());
    }

    fn stored_file(
        store: &crate::store::BlobStore,
        dir: &std::path::Path,
        name: &str,
        bytes: &[u8],
    ) -> CollectionFile {
        let src = dir.join(name);
        if let Some(parent) = src.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&src, bytes).unwrap();
        let meta = store.store_file(&src).unwrap();
        CollectionFile {
            blob_id: meta.blob_id,
            path: format!("weights/{name}"),
            size: bytes.len() as u64,
            media_type: None,
        }
    }

    #[test]
    fn materialize_copies_blobs_and_caches() {
        let dir = std::env::temp_dir().join(format!("gf-mat-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = crate::store::BlobStore::new(&dir);
        let weights = stored_file(&store, &dir, "tiny.gguf", b"GGUFfake");
        let config = stored_file(&store, &dir, "config.json", b"{}");
        let mut manifest = sample_manifest();
        manifest.files = vec![weights, config];
        let out_dir = dir.join("materialized");
        let first = materialize_manifest(&store, &manifest, &out_dir).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(
            std::fs::read(out_dir.join("weights/tiny.gguf")).unwrap(),
            b"GGUFfake"
        );
        assert!(out_dir.join(MATERIALIZED_MARKER).exists());
        let picked = pick_weights_file(&first).unwrap();
        assert!(picked.ends_with("weights/tiny.gguf"), "{picked:?}");
        std::fs::remove_file(out_dir.join("weights/tiny.gguf")).unwrap();
        let second = materialize_manifest(&store, &manifest, &out_dir).unwrap();
        assert_eq!(second, first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn materialize_rejects_escape_paths_and_missing_blobs() {
        let dir = std::env::temp_dir().join(format!("gf-mat-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = crate::store::BlobStore::new(&dir);
        let mut evil = sample_manifest();
        evil.files = vec![CollectionFile {
            blob_id: gt_core::types::BlobId([9u8; 32]),
            path: "../evil".to_string(),
            size: 1,
            media_type: None,
        }];
        assert!(materialize_manifest(&store, &evil, &dir.join("out")).is_err());
        let mut missing = sample_manifest();
        missing.files = vec![CollectionFile {
            blob_id: gt_core::types::BlobId([9u8; 32]),
            path: "weights/ghost.gguf".to_string(),
            size: 1,
            media_type: None,
        }];
        assert!(materialize_manifest(&store, &missing, &dir.join("out2")).is_err());
        assert!(pick_weights_file(&[]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
