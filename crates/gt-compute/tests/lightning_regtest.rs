use gt_compute::lightning::{InvoiceState, LndBackend, LndConfig};
use gt_compute::payment::PaymentBackend;
use std::time::Duration;

fn backend(url_var: &str, mac_var: &str, cert_var: &str) -> Option<LndBackend> {
    let url = std::env::var(url_var).ok()?;
    let mac = std::env::var(mac_var).ok()?;
    let cert_path = std::env::var(cert_var).ok()?;
    let mut config = LndConfig::new(url, mac);
    config.tls_cert_pem = std::fs::read(cert_path).ok()?;
    config.invoice_expiry_secs = 3600;
    LndBackend::new(config).ok()
}

fn backends() -> Option<(LndBackend, LndBackend)> {
    let alice = backend("GF_LN_ALICE_URL", "GF_LN_ALICE_MAC", "GF_LN_ALICE_CERT")?;
    let bob = backend("GF_LN_BOB_URL", "GF_LN_BOB_MAC", "GF_LN_BOB_CERT")?;
    Some((alice, bob))
}

fn wait_accepted(worker: &LndBackend, escrow_id: &str) {
    for _ in 0..120 {
        let state = worker.invoice_state(escrow_id).unwrap();
        if state == InvoiceState::Accepted {
            return;
        }
        assert_eq!(state, InvoiceState::Open, "unexpected invoice state");
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("hold invoice was never accepted");
}

#[test]
fn ln_regtest_hold_invoice_full_lifecycle() {
    let Some((alice, bob)) = backends() else {
        eprintln!("skipping: GF_LN_*_URL/MAC/CERT not set");
        return;
    };
    let start_balance = alice.check_balance("").unwrap();
    assert!(start_balance > 100_000);

    let amount = 50_000;
    let info = bob.create_escrow(amount, "alice", "bob").unwrap();
    assert_eq!(
        bob.invoice_state(&info.escrow_id).unwrap(),
        InvoiceState::Open
    );
    assert_eq!(bob.escrow_funded(&info.escrow_id).unwrap(), Some(false));

    let payer = alice.clone();
    let request = info.payment_details.clone();
    let pay = std::thread::spawn(move || payer.pay_invoice(&request, amount));

    wait_accepted(&bob, &info.escrow_id);
    assert_eq!(bob.escrow_funded(&info.escrow_id).unwrap(), Some(true));
    pay.join().unwrap().unwrap();
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(
        bob.invoice_state(&info.escrow_id).unwrap(),
        InvoiceState::Accepted,
        "payment must stay locked after payer stops tracking"
    );
    let held_balance = alice.check_balance("").unwrap();
    eprintln!("BAL start={start_balance} held={held_balance}");
    bob.release_escrow(&info.escrow_id).unwrap();
    let settled_balance = alice.check_balance("").unwrap();
    eprintln!("BAL settled={settled_balance}");
    let moved = start_balance - settled_balance;
    assert!(
        (amount..=amount + 5_000).contains(&moved),
        "settle moved {moved} sats for a {amount} sat escrow"
    );

    assert!(bob.refund_escrow("no-such-escrow").is_err());
    let cancel_amount = 20_000;
    let held = bob.create_escrow(cancel_amount, "alice", "bob").unwrap();
    let payer = alice.clone();
    let request = held.payment_details.clone();
    let pay = std::thread::spawn(move || payer.pay_invoice(&request, cancel_amount));
    wait_accepted(&bob, &held.escrow_id);
    pay.join().unwrap().unwrap();
    bob.refund_escrow(&held.escrow_id).unwrap();
    assert!(bob.refund_escrow(&held.escrow_id).is_err());
    let mut canceled_balance = alice.check_balance("").unwrap();
    for _ in 0..60 {
        if settled_balance.abs_diff(canceled_balance) <= 5_000 {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
        canceled_balance = alice.check_balance("").unwrap();
    }
    eprintln!("BAL canceled={canceled_balance}");
    let drift = settled_balance.abs_diff(canceled_balance);
    assert!(
        drift <= 5_000,
        "cancel drifted payer balance by {drift} sats"
    );
}
