use anyhow::{Context, Result};
use base64::Engine;
use gt_core::identity::{verify_signature, Identity, PeerId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;

pub const JOB_TOKEN_PREFIX: &str = "gfj.";
pub const JOB_TOKEN_VERSION: u32 = 1;
pub const JOB_TOKEN_EXPIRY_MARGIN_SECS: u64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionScope {
    Contents,
    Workflows,
    Models,
}

impl FromStr for PermissionScope {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "contents" => Ok(PermissionScope::Contents),
            "workflows" => Ok(PermissionScope::Workflows),
            "models" => Ok(PermissionScope::Models),
            other => anyhow::bail!(
                "unknown permission scope '{other}': expected contents, workflows, or models"
            ),
        }
    }
}

impl PermissionScope {
    pub fn name(&self) -> &'static str {
        match self {
            PermissionScope::Contents => "contents",
            PermissionScope::Workflows => "workflows",
            PermissionScope::Models => "models",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionLevel {
    None,
    Read,
    Write,
}

impl FromStr for PermissionLevel {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(PermissionLevel::None),
            "read" => Ok(PermissionLevel::Read),
            "write" => Ok(PermissionLevel::Write),
            other => {
                anyhow::bail!("unknown permission level '{other}': expected none, read, or write")
            }
        }
    }
}

impl PermissionLevel {
    pub fn name(&self) -> &'static str {
        match self {
            PermissionLevel::None => "none",
            PermissionLevel::Read => "read",
            PermissionLevel::Write => "write",
        }
    }

    pub fn satisfies(&self, required: PermissionLevel) -> bool {
        *self >= required
    }

    pub fn min(self, other: PermissionLevel) -> PermissionLevel {
        std::cmp::min(self, other)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Permissions {
    pub grants: HashMap<PermissionScope, PermissionLevel>,
}

impl Permissions {
    pub fn none() -> Self {
        Permissions {
            grants: HashMap::new(),
        }
    }

    pub fn all(level: PermissionLevel) -> Self {
        Permissions {
            grants: HashMap::from([
                (PermissionScope::Contents, level),
                (PermissionScope::Workflows, level),
                (PermissionScope::Models, level),
            ]),
        }
    }

    pub fn level(&self, scope: PermissionScope) -> PermissionLevel {
        self.grants
            .get(&scope)
            .copied()
            .unwrap_or(PermissionLevel::None)
    }

    pub fn allows(&self, scope: PermissionScope, required: PermissionLevel) -> bool {
        self.level(scope).satisfies(required)
    }

    pub fn intersect(&self, other: &Permissions) -> Permissions {
        let mut scopes: Vec<PermissionScope> = self
            .grants
            .keys()
            .chain(other.grants.keys())
            .copied()
            .collect();
        scopes.sort_by_key(|s| s.name());
        scopes.dedup();
        Permissions {
            grants: scopes
                .into_iter()
                .map(|s| (s, self.level(s).min(other.level(s))))
                .filter(|(_, l)| *l != PermissionLevel::None)
                .collect(),
        }
    }

    pub fn overlay(&self, decl: &PermissionsDecl) -> Result<Permissions> {
        let over = decl.resolve()?;
        let mut scopes: Vec<PermissionScope> = self
            .grants
            .keys()
            .chain(over.grants.keys())
            .copied()
            .collect();
        scopes.sort_by_key(|s| s.name());
        scopes.dedup();
        Ok(Permissions {
            grants: scopes
                .into_iter()
                .map(|s| {
                    let level = over
                        .grants
                        .get(&s)
                        .copied()
                        .unwrap_or_else(|| self.level(s));
                    (s, level)
                })
                .filter(|(_, l)| *l != PermissionLevel::None)
                .collect(),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PermissionsDecl {
    #[default]
    Empty,
    Shorthand(String),
    Map(HashMap<String, String>),
}

impl PermissionsDecl {
    pub fn is_empty(&self) -> bool {
        match self {
            PermissionsDecl::Empty => true,
            PermissionsDecl::Shorthand(s) => s.trim().is_empty(),
            PermissionsDecl::Map(m) => m.is_empty(),
        }
    }

    pub fn resolve(&self) -> Result<Permissions> {
        match self {
            PermissionsDecl::Empty => Ok(Permissions::none()),
            PermissionsDecl::Map(map) => {
                let mut grants = HashMap::new();
                for (scope_name, level_name) in map {
                    let scope = scope_name.parse::<PermissionScope>()?;
                    let level = level_name.parse::<PermissionLevel>()?;
                    grants.insert(scope, level);
                }
                Ok(Permissions { grants })
            }
            PermissionsDecl::Shorthand(s) => match s.trim().to_ascii_lowercase().as_str() {
                "" => Ok(Permissions::none()),
                "read-all" => Ok(Permissions::all(PermissionLevel::Read)),
                "write-all" => Ok(Permissions::all(PermissionLevel::Write)),
                other => anyhow::bail!(
                    "unknown permissions shorthand '{other}': expected read-all or write-all"
                ),
            },
        }
    }

    pub fn is_inherit(&self) -> bool {
        matches!(self, PermissionsDecl::Shorthand(s) if s.trim().eq_ignore_ascii_case("inherit"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobTokenPayload {
    v: u32,
    iss: String,
    job: String,
    scopes: HashMap<String, String>,
    exp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobTokenClaims {
    pub minter: PeerId,
    pub job_id: String,
    pub permissions: Permissions,
    pub expires_at: u64,
}

pub fn mint_job_token(
    identity: &Identity,
    job_id: &str,
    permissions: &Permissions,
    expires_at: u64,
) -> String {
    let mut scopes = HashMap::new();
    for scope in [
        PermissionScope::Contents,
        PermissionScope::Workflows,
        PermissionScope::Models,
    ] {
        let level = permissions.level(scope);
        if level != PermissionLevel::None {
            scopes.insert(scope.name().to_string(), level.name().to_string());
        }
    }
    let payload = JobTokenPayload {
        v: JOB_TOKEN_VERSION,
        iss: identity.peer_id().to_string(),
        job: job_id.to_string(),
        scopes,
        exp: expires_at,
    };
    let payload_json = serde_json::to_vec(&payload).unwrap_or_default();
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let payload_b64 = engine.encode(&payload_json);
    let signature = identity.sign(payload_b64.as_bytes());
    let sig_b64 = engine.encode(&signature);
    format!("{JOB_TOKEN_PREFIX}{payload_b64}.{sig_b64}")
}

pub fn verify_job_token(
    token: &str,
    expected_minter: &PeerId,
    now_secs: u64,
) -> Result<JobTokenClaims> {
    let rest = token
        .strip_prefix(JOB_TOKEN_PREFIX)
        .context("not a job token")?;
    let (payload_b64, sig_b64) = rest.split_once('.').context("malformed job token")?;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let payload_json = engine.decode(payload_b64).context("malformed job token")?;
    let signature = engine.decode(sig_b64).context("malformed job token")?;
    let payload: JobTokenPayload =
        serde_json::from_slice(&payload_json).context("malformed job token")?;
    if payload.v != JOB_TOKEN_VERSION {
        anyhow::bail!("unsupported job token version {}", payload.v);
    }
    let minter: PeerId = payload.iss.parse().context("malformed job token minter")?;
    if &minter != expected_minter {
        anyhow::bail!("job token was not minted by this node");
    }
    verify_signature(&minter, payload_b64.as_bytes(), &signature)
        .map_err(|e| anyhow::anyhow!("job token signature invalid: {e}"))?;
    if now_secs >= payload.exp {
        anyhow::bail!("job token expired");
    }
    let mut grants = HashMap::new();
    for (scope_name, level_name) in &payload.scopes {
        let scope = scope_name.parse::<PermissionScope>()?;
        let level = level_name.parse::<PermissionLevel>()?;
        grants.insert(scope, level);
    }
    Ok(JobTokenClaims {
        minter,
        job_id: payload.job,
        permissions: Permissions { grants },
        expires_at: payload.exp,
    })
}

pub fn job_token_expiry(now_secs: u64, timeout_secs: u64) -> u64 {
    now_secs
        .saturating_add(timeout_secs)
        .saturating_add(JOB_TOKEN_EXPIRY_MARGIN_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl_map(pairs: &[(&str, &str)]) -> PermissionsDecl {
        PermissionsDecl::Map(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn test_decl_map_resolves_and_rejects_unknown() {
        let perms = decl_map(&[("contents", "read"), ("workflows", "write")])
            .resolve()
            .unwrap();
        assert!(perms.allows(PermissionScope::Contents, PermissionLevel::Read));
        assert!(!perms.allows(PermissionScope::Contents, PermissionLevel::Write));
        assert!(perms.allows(PermissionScope::Workflows, PermissionLevel::Write));
        assert!(!perms.allows(PermissionScope::Models, PermissionLevel::Read));
        assert!(decl_map(&[("bogus", "read")]).resolve().is_err());
        assert!(decl_map(&[("contents", "admin")]).resolve().is_err());
    }

    #[test]
    fn test_decl_shorthands() {
        let read_all = PermissionsDecl::Shorthand("read-all".to_string())
            .resolve()
            .unwrap();
        assert!(read_all.allows(PermissionScope::Models, PermissionLevel::Read));
        assert!(!read_all.allows(PermissionScope::Models, PermissionLevel::Write));
        let write_all = PermissionsDecl::Shorthand("write-all".to_string())
            .resolve()
            .unwrap();
        assert!(write_all.allows(PermissionScope::Contents, PermissionLevel::Write));
        assert!(PermissionsDecl::Shorthand("everything".to_string())
            .resolve()
            .is_err());
        assert!(PermissionsDecl::default()
            .resolve()
            .unwrap()
            .grants
            .is_empty());
    }

    #[test]
    fn test_overlay_job_wins_per_scope() {
        let base = decl_map(&[("contents", "write"), ("models", "read")])
            .resolve()
            .unwrap();
        let over = base.overlay(&decl_map(&[("contents", "read")])).unwrap();
        assert!(over.allows(PermissionScope::Contents, PermissionLevel::Read));
        assert!(!over.allows(PermissionScope::Contents, PermissionLevel::Write));
        assert!(over.allows(PermissionScope::Models, PermissionLevel::Read));
    }

    #[test]
    fn test_intersect_can_only_narrow() {
        let caller = decl_map(&[("contents", "read"), ("workflows", "write")])
            .resolve()
            .unwrap();
        let callee = decl_map(&[("contents", "write"), ("workflows", "read")])
            .resolve()
            .unwrap();
        let effective = caller.intersect(&callee);
        assert!(effective.allows(PermissionScope::Contents, PermissionLevel::Read));
        assert!(!effective.allows(PermissionScope::Contents, PermissionLevel::Write));
        assert!(effective.allows(PermissionScope::Workflows, PermissionLevel::Read));
        assert!(!effective.allows(PermissionScope::Workflows, PermissionLevel::Write));
    }

    #[test]
    fn test_token_roundtrip() {
        let minter = Identity::generate();
        let perms = decl_map(&[("contents", "read")]).resolve().unwrap();
        let token = mint_job_token(&minter, "job-1", &perms, 2000);
        assert!(token.starts_with(JOB_TOKEN_PREFIX));
        let claims = verify_job_token(&token, &minter.peer_id(), 1000).unwrap();
        assert_eq!(claims.job_id, "job-1");
        assert_eq!(claims.expires_at, 2000);
        assert!(claims
            .permissions
            .allows(PermissionScope::Contents, PermissionLevel::Read));
        assert!(!claims
            .permissions
            .allows(PermissionScope::Workflows, PermissionLevel::Read));
    }

    #[test]
    fn test_token_rejects_tamper_expiry_and_foreign_minter() {
        let minter = Identity::generate();
        let other = Identity::generate();
        let perms = decl_map(&[("contents", "write")]).resolve().unwrap();
        let token = mint_job_token(&minter, "job-1", &perms, 2000);
        assert!(verify_job_token(&token, &minter.peer_id(), 2000).is_err());
        assert!(verify_job_token(&token, &other.peer_id(), 1000).is_err());
        let mut tampered = token.clone();
        tampered.push('A');
        assert!(verify_job_token(&tampered, &minter.peer_id(), 1000).is_err());
        assert!(verify_job_token("Bearer abc", &minter.peer_id(), 1000).is_err());
        let foreign = mint_job_token(&other, "job-1", &perms, 9999);
        assert!(verify_job_token(&foreign, &minter.peer_id(), 1000).is_err());
    }
}
