use anyhow::Result;
use gt_core::types::BlobId;

/// A pointer file that replaces a large file in a git repository.
/// Similar concept to Git LFS, but fully decentralized.
///
/// Format:
/// ```text
/// version gitforest-lfs/1
/// oid sha256:<64 hex chars>
/// size <bytes>
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct PointerFile {
    pub blob_id: BlobId,
    pub size: u64,
}

const POINTER_VERSION: &str = "version gitforest-lfs/1";

impl PointerFile {
    pub fn new(blob_id: BlobId, size: u64) -> Self {
        PointerFile { blob_id, size }
    }

    /// Encode as a pointer file string.
    pub fn encode(&self) -> String {
        format!(
            "{}\noid sha256:{}\nsize {}\n",
            POINTER_VERSION, self.blob_id, self.size
        )
    }

    /// Decode from a pointer file string.
    pub fn decode(content: &str) -> Result<Self> {
        let mut blob_id = None;
        let mut size = None;
        let mut has_version = false;

        for line in content.lines() {
            let line = line.trim();
            if line == POINTER_VERSION {
                has_version = true;
            } else if let Some(oid) = line.strip_prefix("oid sha256:") {
                blob_id = Some(oid.parse::<BlobId>().map_err(|e| anyhow::anyhow!(e))?);
            } else if let Some(s) = line.strip_prefix("size ") {
                size = Some(s.parse::<u64>()?);
            }
        }

        if !has_version {
            anyhow::bail!("missing version line");
        }

        match (blob_id, size) {
            (Some(id), Some(sz)) => Ok(PointerFile::new(id, sz)),
            _ => anyhow::bail!("invalid pointer file: missing oid or size"),
        }
    }
}

/// Check if some file content looks like a gitforest LFS pointer file.
pub fn is_pointer_file(content: &[u8]) -> bool {
    if let Ok(s) = std::str::from_utf8(content) {
        s.starts_with(POINTER_VERSION) && s.contains("oid sha256:") && s.contains("size ")
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let blob_id = BlobId([0xab; 32]);
        let pointer = PointerFile::new(blob_id, 1_500_000_000);

        let encoded = pointer.encode();
        let decoded = PointerFile::decode(&encoded).unwrap();

        assert_eq!(pointer, decoded);
    }

    #[test]
    fn test_is_pointer_file() {
        let blob_id = BlobId([0x42; 32]);
        let pointer = PointerFile::new(blob_id, 1024);
        let encoded = pointer.encode();

        assert!(is_pointer_file(encoded.as_bytes()));
        assert!(!is_pointer_file(b"not a pointer file"));
        assert!(!is_pointer_file(
            b"version gitforest-lfs/1\nbut missing fields"
        ));
        assert!(!is_pointer_file(&[0xff, 0xfe, 0x00])); // invalid UTF-8
    }

    #[test]
    fn test_decode_invalid() {
        assert!(PointerFile::decode("garbage").is_err());
        assert!(PointerFile::decode("version gitforest-lfs/1\n").is_err());
    }

    #[test]
    fn test_pointer_format() {
        let blob_id = BlobId([0x00; 32]);
        let pointer = PointerFile::new(blob_id, 42);
        let encoded = pointer.encode();

        assert!(encoded.starts_with("version gitforest-lfs/1\n"));
        assert!(encoded.contains(
            "oid sha256:0000000000000000000000000000000000000000000000000000000000000000"
        ));
        assert!(encoded.contains("size 42"));
    }
}
