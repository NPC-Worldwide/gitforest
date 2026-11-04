use gt_core::identity::{Identity, PeerId};
use gt_core::sanitize::sanitize_log;
use gt_core::types::{BlobId, CollectionManifest};
use gt_lfs::blob::BlobMeta;
use std::sync::Arc;
use tokio::sync::RwLock;

fn signed_get(
    client: &reqwest::Client,
    url: &str,
    path: &str,
    identity: &Identity,
) -> reqwest::RequestBuilder {
    let mut request = client.get(url);
    for (name, value) in gt_core::identity::peer_call_headers(identity, "GET", path, b"") {
        request = request.header(name, value);
    }
    request
}

pub fn dial_blocked(host: &str) -> bool {
    if host == "169.254.169.254" {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if let std::net::IpAddr::V4(v4) = ip {
            return v4.octets() == [169, 254, 169, 254];
        }
        return false;
    }
    if let Ok(n) = host.parse::<u32>() {
        return n == 2852039166;
    }
    false
}

pub fn peer_http_base(known_peers: &[String], peer: &PeerId) -> Option<String> {
    let suffix = format!("/gf/{peer}");
    let mut fallback: Option<String> = None;
    for entry in known_peers {
        if !entry.contains(&suffix) {
            continue;
        }
        let Some((host, port)) = crate::api::parse_peer_multiaddr(entry) else {
            continue;
        };
        if dial_blocked(host) {
            continue;
        }
        if entry.contains("/api/") {
            return Some(format!("http://{host}:{port}"));
        }
        if fallback.is_none() {
            fallback = Some(format!("http://{host}:{port}"));
        }
    }
    fallback
}

pub(crate) const FETCH_PIECES_PER_TICK: usize = 64;

pub(crate) fn peer_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .ok()
}

pub async fn fetch_meta(
    client: &reqwest::Client,
    base: &str,
    blob_id: &BlobId,
    identity: &Identity,
) -> anyhow::Result<BlobMeta> {
    let url = format!("{base}/api/blobs/{blob_id}/meta");
    let path = format!("/api/blobs/{blob_id}/meta");
    let resp = signed_get(client, &url, &path, identity).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("meta fetch failed: {}", resp.status());
    }
    let meta: BlobMeta = resp.json().await?;
    if meta.blob_id != *blob_id {
        anyhow::bail!("meta blob id mismatch");
    }
    Ok(meta)
}

pub async fn fetch_piece(
    client: &reqwest::Client,
    base: &str,
    blob_id: &BlobId,
    index: usize,
    identity: &Identity,
) -> anyhow::Result<Vec<u8>> {
    let url = format!("{base}/api/blobs/{blob_id}/pieces/{index}");
    let path = format!("/api/blobs/{blob_id}/pieces/{index}");
    let resp = signed_get(client, &url, &path, identity).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("piece fetch failed: {}", resp.status());
    }
    if let Some(len) = resp.content_length() {
        if len > 16 * 1024 * 1024 {
            anyhow::bail!("piece too large: {len}");
        }
    }
    Ok(resp.bytes().await?.to_vec())
}

pub async fn fetch_manifest(
    client: &reqwest::Client,
    base: &str,
    id: &str,
    identity: &Identity,
) -> anyhow::Result<CollectionManifest> {
    let url = format!("{base}/api/collections/{id}");
    let path = format!("/api/collections/{id}");
    let resp = signed_get(client, &url, &path, identity).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("manifest fetch failed: {}", resp.status());
    }
    let value: serde_json::Value = resp.json().await?;
    let manifest: CollectionManifest = serde_json::from_value(value)?;
    if manifest.id != id {
        anyhow::bail!("manifest id mismatch");
    }
    if !manifest.verify() {
        anyhow::bail!("manifest signature invalid");
    }
    Ok(manifest)
}

pub async fn run_fetch_tick(state: Arc<RwLock<crate::api::AppState>>) {
    let wants: Vec<BlobId> = state.read().await.wanted_blobs.iter().cloned().collect();
    let wanted_colls: Vec<String> = state
        .read()
        .await
        .wanted_collections
        .iter()
        .cloned()
        .collect();
    if wants.is_empty() && wanted_colls.is_empty() {
        return;
    }
    let Some(client) = peer_client() else {
        return;
    };
    for id in wanted_colls {
        fetch_collection_once(&state, &client, &id).await;
    }
    for blob_id in wants {
        fetch_blob_once(&state, &client, &blob_id).await;
    }
}

async fn fetch_collection_once(
    state: &Arc<RwLock<crate::api::AppState>>,
    client: &reqwest::Client,
    id: &str,
) {
    if state.read().await.collection_store.get(id).is_some() {
        state.write().await.wanted_collections.remove(id);
        return;
    }
    let (seeders, known_peers, our_id) = {
        let s = state.read().await;
        (
            s.collection_seeders.get(id).cloned().unwrap_or_default(),
            s.known_peers.clone(),
            s.identity.peer_id(),
        )
    };
    let mut fetched = false;
    for seeder in &seeders {
        if seeder == &our_id {
            continue;
        }
        let Some(base) = peer_http_base(&known_peers, seeder) else {
            continue;
        };
        let identity = state.read().await.identity.clone();
        match fetch_manifest(client, &base, id, &identity).await {
            Ok(manifest) => {
                let mut s = state.write().await;
                for file in &manifest.files {
                    if !s.blob_store.has(&file.blob_id) {
                        s.wanted_blobs.insert(file.blob_id.clone());
                    }
                }
                s.collection_store.add(manifest);
                s.wanted_collections.remove(id);
                fetched = true;
                break;
            }
            Err(e) => {
                tracing::warn!(
                    "manifest {} fetch from {} failed: {}",
                    sanitize_log(id),
                    sanitize_log(&base),
                    sanitize_log(&e.to_string())
                );
            }
        }
    }
    if !fetched {
        let addr = gt_lfs::blob::collection_dht_address(id);
        let _ = state.read().await.dht.lookup_collection(addr).await;
    }
}

async fn announce_completed_models(s: &mut crate::api::AppState, blob_id: &BlobId) {
    let completed: Vec<CollectionManifest> = s
        .collection_store
        .list()
        .into_iter()
        .filter(|m| m.files.iter().any(|f| f.blob_id == *blob_id))
        .filter(|m| m.files.iter().all(|f| s.blob_store.has(&f.blob_id)))
        .cloned()
        .collect();
    for manifest in completed {
        announce_model_seeder(s, &manifest).await;
    }
}

pub async fn announce_model_seeder(s: &mut crate::api::AppState, manifest: &CollectionManifest) {
    let Some(ann) = crate::models::build_seeder_announcement(&s.identity, manifest) else {
        return;
    };
    let Ok(meta) = gt_lfs::model::ModelMetadata::from_manifest(manifest) else {
        return;
    };
    let data = serde_json::to_vec(&ann).unwrap_or_default();
    let name_addr = gt_lfs::model::model_name_dht_address(&manifest.name);
    let arch_addr = gt_lfs::model::model_arch_dht_address(&meta.architecture);
    let _ = s.dht.announce_collection(name_addr, data.clone()).await;
    let _ = s.dht.announce_collection(arch_addr, data).await;
    let own = s.identity.peer_id();
    let entry = s.collection_seeders.entry(manifest.id.clone()).or_default();
    if !entry.contains(&own) {
        entry.push(own.clone());
    }
    match s.known_models.get_mut(&manifest.id) {
        Some(record) => {
            record.local = true;
            record.merge_seeder(own);
        }
        None => {
            if let Some(mut record) = crate::models::ModelRecord::from_manifest(manifest, true) {
                record.merge_seeder(own);
                s.known_models.insert(record.manifest_id.clone(), record);
            }
        }
    }
}

async fn fetch_blob_once(
    state: &Arc<RwLock<crate::api::AppState>>,
    client: &reqwest::Client,
    blob_id: &BlobId,
) {
    if state.read().await.blob_store.has(blob_id) {
        state.write().await.wanted_blobs.remove(blob_id);
        return;
    }
    let (seeders, known_peers, our_id) = {
        let s = state.read().await;
        let mut peers = s.blob_tracker.seeders_for(blob_id);
        for leecher in s.blob_tracker.leechers_for(blob_id) {
            if !peers.contains(&leecher) {
                peers.push(leecher);
            }
        }
        (peers, s.known_peers.clone(), s.identity.peer_id())
    };
    let mut bases = Vec::new();
    for seeder in &seeders {
        if seeder == &our_id {
            continue;
        }
        if let Some(base) = peer_http_base(&known_peers, seeder) {
            bases.push(base);
        }
    }
    if bases.is_empty() {
        let addr = gt_lfs::blob::blob_dht_address(blob_id);
        let _ = state.read().await.dht.lookup_blob(addr).await;
        return;
    }
    let has_download = state
        .read()
        .await
        .transfer_manager
        .status(blob_id)
        .is_some();
    if !has_download {
        let mut started = false;
        for base in &bases {
            let identity = state.read().await.identity.clone();
            match fetch_meta(client, base, blob_id, &identity).await {
                Ok(meta) => {
                    let mut s = state.write().await;
                    if s.transfer_manager
                        .start_download(meta, seeders.clone(), our_id.clone())
                        .await
                        .is_ok()
                    {
                        started = true;
                    }
                    break;
                }
                Err(e) => {
                    tracing::warn!(
                        "meta fetch from {} failed: {}",
                        sanitize_log(base),
                        sanitize_log(&e.to_string())
                    );
                }
            }
        }
        if !started {
            return;
        }
    }
    let needed: Vec<usize> = state
        .read()
        .await
        .transfer_manager
        .needed_pieces(blob_id)
        .into_iter()
        .take(FETCH_PIECES_PER_TICK)
        .collect();
    let prioritized = gt_lfs::transfer::PieceStrategy::prioritize(&needed);
    let assignments = gt_lfs::transfer::PieceStrategy::assign_to_peers(&prioritized, bases.len());
    let mut set = tokio::task::JoinSet::new();
    let identity = state.read().await.identity.clone();
    for (peer_idx, pieces) in assignments.iter().enumerate() {
        for &index in pieces {
            let client = client.clone();
            let bases = bases.clone();
            let blob_id = blob_id.clone();
            let identity = identity.clone();
            set.spawn(async move {
                for offset in 0..bases.len() {
                    let base = &bases[(peer_idx + offset) % bases.len()];
                    match fetch_piece(&client, base, &blob_id, index, &identity).await {
                        Ok(data) => return (index, Some(data)),
                        Err(e) => {
                            tracing::warn!(
                                "piece {index} fetch from {} failed: {}",
                                sanitize_log(base),
                                sanitize_log(&e.to_string())
                            );
                        }
                    }
                }
                (index, None)
            });
        }
    }
    let mut received = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((index, Some(data))) = joined {
            received.push((index, data));
        }
    }
    received.sort_by_key(|(index, _)| *index);
    if !received.is_empty() {
        let mut s = state.write().await;
        for (index, data) in received {
            s.transfer_manager.receive_piece(blob_id, index, data);
        }
    }
    {
        let mut s = state.write().await;
        let status = s.transfer_manager.status(blob_id);
        let incomplete = status
            .as_ref()
            .is_some_and(|st| st.pieces_done < st.piece_count);
        if incomplete {
            if let Some(have) = s.transfer_manager.take_unannounced_progress(blob_id) {
                let status = status.expect("checked above");
                let mut announcement = gt_core::protocol::BlobAnnouncement {
                    peer_id: s.identity.peer_id(),
                    blob_id: blob_id.clone(),
                    size: status.total_size,
                    piece_count: status.piece_count as u32,
                    have_pieces: Some(have),
                    signature: Vec::new(),
                };
                announcement.signature = s.identity.sign(&announcement.signable_payload());
                let addr = gt_lfs::blob::blob_dht_address(blob_id);
                let data = serde_json::to_vec(&announcement).unwrap_or_default();
                let _ = s.dht.announce_blob(addr, data).await;
            }
        }
    }
    let mut s = state.write().await;
    match s
        .transfer_manager
        .check_complete(blob_id, our_id.clone())
        .await
    {
        Ok(true) => {
            s.wanted_blobs.remove(blob_id);
            let blob_id = blob_id.clone();
            if let Ok(Some(meta)) = s.blob_store.get_meta(&blob_id) {
                let mut announcement = gt_core::protocol::BlobAnnouncement {
                    peer_id: s.identity.peer_id(),
                    blob_id: blob_id.clone(),
                    size: meta.total_size,
                    piece_count: meta.piece_count() as u32,
                    have_pieces: None,
                    signature: Vec::new(),
                };
                announcement.signature = s.identity.sign(&announcement.signable_payload());
                let addr = gt_lfs::blob::blob_dht_address(&blob_id);
                let data = serde_json::to_vec(&announcement).unwrap_or_default();
                let _ = s.dht.announce_blob(addr, data).await;
            }
            announce_completed_models(&mut s, &blob_id).await;
        }
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(
                "download of {blob_id} failed verification: {}",
                sanitize_log(&e.to_string())
            );
            s.transfer_manager.remove_download(blob_id);
            s.wanted_blobs.remove(blob_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_core::identity::Identity;

    #[test]
    fn test_peer_http_base() {
        let peer = Identity::generate().peer_id();
        let other = Identity::generate().peer_id();
        let peers = vec![
            "/ip4/10.0.0.2/tcp/9418/p2p/QmX/gf/00".to_string(),
            format!("/ip4/10.0.0.9/tcp/9418/api/7878/p2p/QmY/gf/{peer}"),
        ];
        assert_eq!(
            peer_http_base(&peers, &peer),
            Some("http://10.0.0.9:7878".to_string())
        );
        assert_eq!(peer_http_base(&peers, &other), None);
        assert_eq!(peer_http_base(&[], &peer), None);
    }

    #[test]
    fn test_peer_http_base_defaults_to_7878() {
        let peer = Identity::generate().peer_id();
        let peers = vec![format!("/ip4/192.168.1.5/tcp/9418/p2p/QmZ/gf/{peer}")];
        assert_eq!(
            peer_http_base(&peers, &peer),
            Some("http://192.168.1.5:7878".to_string())
        );
    }

    #[test]
    fn test_peer_http_base_prefers_explicit_api_port() {
        let peer = Identity::generate().peer_id();
        let peers = vec![
            format!("/ip4/127.0.0.1/tcp/19422/p2p/QmDHT/gf/{peer}"),
            format!("/ip4/127.0.0.1/tcp/19422/api/17882/p2p/QmX/gf/{peer}"),
        ];
        assert_eq!(
            peer_http_base(&peers, &peer),
            Some("http://127.0.0.1:17882".to_string())
        );
        assert_eq!(
            peer_http_base(&peers[..1], &peer),
            Some("http://127.0.0.1:7878".to_string())
        );
    }

    #[test]
    fn test_dial_blocked() {
        assert!(dial_blocked("169.254.169.254"));
        assert!(dial_blocked("2852039166"));
        assert!(!dial_blocked("127.0.0.1"));
        assert!(!dial_blocked("10.0.0.9"));
        assert!(!dial_blocked("example.com"));
        assert!(!dial_blocked("::1"));
    }

    #[test]
    fn test_peer_http_base_skips_blocked() {
        let peer = Identity::generate().peer_id();
        let peers = vec![
            format!("/ip4/169.254.169.254/tcp/80/api/80/p2p/QmZ/gf/{peer}"),
            format!("/ip4/10.1.2.3/tcp/9418/api/7878/p2p/QmZ/gf/{peer}"),
        ];
        assert_eq!(
            peer_http_base(&peers, &peer),
            Some("http://10.1.2.3:7878".to_string())
        );
        assert_eq!(peer_http_base(&peers[..1], &peer), None);
    }
}
