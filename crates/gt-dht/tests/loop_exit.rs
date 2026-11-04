use gt_core::identity::Identity;
use gt_dht::node::DhtNode;

#[tokio::test]
async fn shutdown_drops_liveness() {
    let id = Identity::generate();
    let node = DhtNode::start_on(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        19744,
        17994,
        &id,
        false,
    )
    .await
    .unwrap();
    let sender = node.clone_sender();
    assert!(sender.is_alive());
    node.shutdown().await.unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while sender.is_alive() {
        assert!(
            std::time::Instant::now() < deadline,
            "loop did not exit after shutdown"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
