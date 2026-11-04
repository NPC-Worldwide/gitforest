use gt_core::identity::Identity;
use gt_dht::node::{DhtNode, MAX_PENDING_INCOMING};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

async fn hold_raw_connections(port: u16, n: usize) -> Vec<TcpStream> {
    let mut held = Vec::new();
    for _ in 0..n {
        let stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .expect("connect");
        held.push(stream);
    }
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    held
}

async fn count_live(held: &mut [TcpStream]) -> usize {
    let mut live = 0;
    for stream in held.iter_mut() {
        let mut byte = [0u8; 1];
        match tokio::time::timeout(
            std::time::Duration::from_millis(100),
            stream.read(&mut byte),
        )
        .await
        {
            Err(_) => live += 1,
            Ok(Ok(0)) => {}
            Ok(Ok(_)) => live += 1,
            Ok(Err(_)) => {}
        }
    }
    live
}

#[tokio::test]
async fn pending_connections_under_cap_all_survive() {
    let id = Identity::generate();
    let node = DhtNode::start_on(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        19740,
        17990,
        &id,
        false,
    )
    .await
    .unwrap();
    let mut held = hold_raw_connections(19740, 20).await;
    let live = count_live(&mut held).await;
    assert_eq!(live, 20, "modest connection counts must not be refused");
    node.shutdown().await.ok();
}

#[tokio::test]
async fn pending_flood_is_capped() {
    let id = Identity::generate();
    let node = DhtNode::start_on(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        19742,
        17991,
        &id,
        false,
    )
    .await
    .unwrap();
    let flood = MAX_PENDING_INCOMING as usize + 8;
    let mut held = hold_raw_connections(19742, flood).await;
    let live = count_live(&mut held).await;
    assert!(
        live <= MAX_PENDING_INCOMING as usize,
        "flood must be capped at {MAX_PENDING_INCOMING}, {live} survived"
    );
    assert!(
        live >= MAX_PENDING_INCOMING as usize - 12,
        "cap must not be over-eager, only {live} of {flood} survived"
    );
    node.shutdown().await.ok();
}
