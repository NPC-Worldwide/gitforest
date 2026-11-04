use gt_core::identity::Identity;
use gt_core::protocol::ComputeOfferMessage;
use gt_dht::node::{DhtEvent, DhtNode};
use std::time::Duration;

fn test_offer(worker: &Identity) -> ComputeOfferMessage {
    let mut msg = ComputeOfferMessage {
        worker_id: worker.peer_id(),
        cpu_cores: 4,
        ram_mb: 8192,
        disk_mb: 50000,
        has_gpu: false,
        hourly_rate_sats: 1000,
        accepted_chains: vec!["lightning".into()],
        max_concurrent_jobs: 2,
        pow_nonce: 0,
        signature: vec![],
    };
    let payload = msg.signable_payload();
    msg.signature = worker.sign(&payload);
    msg
}

fn compute_offer_address(worker: &gt_core::identity::PeerId) -> [u8; 32] {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"gitforest:compute:");
    hasher.update(worker.0);
    hasher.finalize().into()
}

async fn wait_for_peer(node: &mut DhtNode, want: &str) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match node.recv().await {
                Some(DhtEvent::PeerDiscovered {
                    gitforest_peer_id: Some(gf),
                    ..
                }) if gf == want => break,
                Some(_) => continue,
                None => panic!("dht event channel closed"),
            }
        }
    })
    .await
    .expect("timed out waiting for peer identify");
}

#[tokio::test]
async fn announced_compute_offer_is_found_by_lookup() {
    let worker = Identity::generate();
    let seeker_id = Identity::generate();
    let worker_node = DhtNode::start_on(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        19631,
        17881,
        &worker,
        false,
    )
    .await
    .unwrap();
    let mut seeker = DhtNode::start_on(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        19632,
        17882,
        &seeker_id,
        false,
    )
    .await
    .unwrap();

    worker_node
        .announce_compute(test_offer(&worker))
        .await
        .unwrap();
    seeker
        .connect_peer_str("/ip4/127.0.0.1/tcp/19631")
        .await
        .unwrap();
    wait_for_peer(&mut seeker, &worker.peer_id().to_string()).await;

    seeker
        .lookup_compute(compute_offer_address(&worker.peer_id()))
        .await
        .unwrap();
    let found = tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            match seeker.recv().await {
                Some(DhtEvent::ComputeOfferFound(offer)) => break offer,
                Some(_) => continue,
                None => panic!("dht event channel closed"),
            }
        }
    })
    .await
    .expect("timed out waiting for ComputeOfferFound");

    assert_eq!(found.worker_id, worker.peer_id());
    assert_eq!(found.cpu_cores, 4);
    assert_eq!(found.ram_mb, 8192);

    worker_node.shutdown().await.ok();
    seeker.shutdown().await.ok();
}
