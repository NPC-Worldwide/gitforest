use gt_compute::evm::{build_signer, CommandSigner, EvmL2Backend};
use gt_compute::payment::{L2Network, PaymentBackend};

fn regtest_backend() -> Option<EvmL2Backend> {
    let rpc_url = std::env::var("GF_EVM_REGTEST_URL").ok()?;
    let from = std::env::var("GF_EVM_REGTEST_FROM").ok()?;
    let signer_path = format!("{}/../../scripts/evm_sign.py", env!("CARGO_MANIFEST_DIR"));
    let signer = CommandSigner::new(format!("python3 {signer_path}"), from);
    EvmL2Backend::new(L2Network::Base, rpc_url, Box::new(signer)).ok()
}

fn embedded_backend() -> Option<EvmL2Backend> {
    let rpc_url = std::env::var("GF_EVM_REGTEST_URL").ok()?;
    let from = std::env::var("GF_EVM_REGTEST_FROM").ok()?;
    let keystore = std::env::var("GF_EVM_KEYSTORE_PATH").ok()?;
    let signer = build_signer(&from, "", Some(std::path::Path::new(&keystore)), None).ok()?;
    EvmL2Backend::new(L2Network::Base, rpc_url, signer).ok()
}

#[test]
fn evm_regtest_release_moves_real_wei() {
    let Some(backend) = regtest_backend() else {
        eprintln!("skipping: GF_EVM_REGTEST_URL/GF_EVM_REGTEST_FROM not set");
        return;
    };
    let payee = format!("0x{:040x}", rand::random::<u128>());
    assert_eq!(backend.check_balance(&payee).unwrap(), 0);
    let info = backend
        .create_escrow(1_000_000, "regtest-payer", &payee)
        .unwrap();
    assert!(info.payment_details.contains("8453"));
    backend.release_escrow(&info.escrow_id).unwrap();
    assert_eq!(backend.check_balance(&payee).unwrap(), 1_000_000);
}

#[test]
fn evm_regtest_refund_moves_nothing() {
    let Some(backend) = regtest_backend() else {
        eprintln!("skipping: GF_EVM_REGTEST_URL/GF_EVM_REGTEST_FROM not set");
        return;
    };
    let payee = format!("0x{:040x}", rand::random::<u128>());
    let info = backend
        .create_escrow(2_000_000, "regtest-payer", &payee)
        .unwrap();
    backend.refund_escrow(&info.escrow_id).unwrap();
    assert_eq!(backend.check_balance(&payee).unwrap(), 0);
    assert!(backend.refund_escrow(&info.escrow_id).is_err());
}

#[test]
fn evm_regtest_embedded_release_moves_real_wei() {
    let Some(backend) = embedded_backend() else {
        eprintln!("skipping: GF_EVM_KEYSTORE_PATH not set");
        return;
    };
    let payee = format!("0x{:040x}", rand::random::<u128>());
    assert_eq!(backend.check_balance(&payee).unwrap(), 0);
    let info = backend
        .create_escrow(3_000_000, "regtest-payer", &payee)
        .unwrap();
    backend.release_escrow(&info.escrow_id).unwrap();
    assert_eq!(backend.check_balance(&payee).unwrap(), 3_000_000);
}

#[test]
fn evm_regtest_embedded_refund_moves_nothing() {
    let Some(backend) = embedded_backend() else {
        eprintln!("skipping: GF_EVM_KEYSTORE_PATH not set");
        return;
    };
    let payee = format!("0x{:040x}", rand::random::<u128>());
    let info = backend
        .create_escrow(4_000_000, "regtest-payer", &payee)
        .unwrap();
    backend.refund_escrow(&info.escrow_id).unwrap();
    assert_eq!(backend.check_balance(&payee).unwrap(), 0);
}
