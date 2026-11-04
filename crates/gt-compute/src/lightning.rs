use crate::payment::{ChainType, EscrowInfo, PaymentBackend};
use gt_core::sanitize::sanitize_log;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const FIRST_UPDATE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceState {
    Open,
    Accepted,
    Settled,
    Canceled,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct LndConfig {
    pub grpc_url: String,
    pub macaroon_hex: String,
    pub tls_cert_pem: Vec<u8>,
    pub invoice_expiry_secs: u64,
    pub cltv_expiry: u64,
}

impl LndConfig {
    pub fn new(grpc_url: String, macaroon_hex: String) -> Self {
        LndConfig {
            grpc_url,
            macaroon_hex,
            tls_cert_pem: Vec::new(),
            invoice_expiry_secs: 86_400,
            cltv_expiry: 40,
        }
    }
}

mod proto {
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct AddHoldInvoiceRequest {
        #[prost(string, tag = "1")]
        pub memo: String,
        #[prost(bytes, tag = "2")]
        pub hash: Vec<u8>,
        #[prost(int64, tag = "3")]
        pub value: i64,
        #[prost(int64, tag = "5")]
        pub expiry: i64,
        #[prost(uint64, tag = "7")]
        pub cltv_expiry: u64,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct AddHoldInvoiceResponse {
        #[prost(string, tag = "1")]
        pub payment_request: String,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct LookupInvoiceMsg {
        #[prost(oneof = "InvoiceRef", tags = "1")]
        pub invoice_ref: Option<InvoiceRef>,
    }

    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum InvoiceRef {
        #[prost(bytes, tag = "1")]
        PaymentHash(Vec<u8>),
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, ::prost::Enumeration)]
    #[repr(i32)]
    pub enum InvoiceStateEnum {
        Open = 0,
        Settled = 1,
        Canceled = 2,
        Accepted = 3,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Invoice {
        #[prost(enumeration = "InvoiceStateEnum", tag = "21")]
        pub state: i32,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct SettleInvoiceMsg {
        #[prost(bytes, tag = "1")]
        pub preimage: Vec<u8>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct CancelInvoiceMsg {
        #[prost(bytes, tag = "1")]
        pub payment_hash: Vec<u8>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Empty {}

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Amount {
        #[prost(uint64, tag = "1")]
        pub sat: u64,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct ChannelBalanceResponse {
        #[prost(message, tag = "3")]
        pub local_balance: Option<Amount>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct SendPaymentRequest {
        #[prost(string, tag = "5")]
        pub payment_request: String,
        #[prost(int32, tag = "6")]
        pub timeout_seconds: i32,
        #[prost(int64, tag = "7")]
        pub fee_limit_sat: i64,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, ::prost::Enumeration)]
    #[repr(i32)]
    pub enum PaymentStatusEnum {
        Unknown = 0,
        InFlight = 1,
        Succeeded = 2,
        Failed = 3,
        Initiated = 4,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, ::prost::Enumeration)]
    #[repr(i32)]
    pub enum PaymentFailureReason {
        None = 0,
        Timeout = 1,
        NoRoute = 2,
        Error = 3,
        IncorrectPaymentDetails = 4,
        InsufficientBalance = 5,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Payment {
        #[prost(enumeration = "PaymentStatusEnum", tag = "10")]
        pub status: i32,
        #[prost(enumeration = "PaymentFailureReason", tag = "16")]
        pub failure_reason: i32,
    }
}

pub trait LndTransport: Send + Sync {
    fn add_hold_invoice(
        &self,
        memo: &str,
        hash: [u8; 32],
        amount_sats: u64,
        expiry_secs: u64,
        cltv_expiry: u64,
    ) -> anyhow::Result<String>;
    fn lookup_state(&self, hash: [u8; 32]) -> anyhow::Result<InvoiceState>;
    fn settle(&self, preimage: [u8; 32]) -> anyhow::Result<()>;
    fn cancel(&self, hash: [u8; 32]) -> anyhow::Result<()>;
    fn initiate_payment(
        &self,
        payment_request: &str,
        timeout_secs: u64,
        fee_limit_sats: u64,
    ) -> anyhow::Result<()>;
    fn channel_balance(&self) -> anyhow::Result<u64>;
}

#[derive(Debug)]
struct PinningVerifier {
    pinned_der: Vec<u8>,
    inner: Arc<rustls::client::WebPkiServerVerifier>,
}

impl PinningVerifier {
    fn new(pinned_der: Vec<u8>) -> anyhow::Result<Self> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(rustls::pki_types::CertificateDer::from(pinned_der.clone()))?;
        let inner = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots)).build()?;
        Ok(PinningVerifier { pinned_der, inner })
    }
}

impl rustls::client::danger::ServerCertVerifier for PinningVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() != self.pinned_der.as_slice() {
            return Err(rustls::Error::General(
                "lnd server certificate does not match pinned tls.cert".into(),
            ));
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

#[derive(Clone)]
struct PinningConnector {
    tls_config: Arc<rustls::ClientConfig>,
}

impl tower_service::Service<http::Uri> for PinningConnector {
    type Response = hyper_util::rt::TokioIo<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;
    type Error = anyhow::Error;
    type Future = futures_util::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: http::Uri) -> Self::Future {
        let tls_config = self.tls_config.clone();
        Box::pin(async move {
            let host = uri
                .host()
                .ok_or_else(|| anyhow::anyhow!("lnd grpc url has no host"))?
                .to_string();
            let port = uri.port_u16().unwrap_or(10009);
            let tcp = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
            let server_name = rustls::pki_types::ServerName::try_from(host)
                .map_err(|_| anyhow::anyhow!("bad lnd host"))?;
            let connector = tokio_rustls::TlsConnector::from(tls_config);
            let tls = connector.connect(server_name, tcp).await?;
            Ok(hyper_util::rt::TokioIo::new(tls))
        })
    }
}

struct GrpcTransport {
    grpc_url: String,
    macaroon_hex: String,
    tls_config: Arc<rustls::ClientConfig>,
    runtime: tokio::runtime::Runtime,
    channel: std::sync::OnceLock<tonic::transport::Channel>,
}

impl GrpcTransport {
    fn new(config: &LndConfig) -> anyhow::Result<Self> {
        if config.tls_cert_pem.is_empty() {
            anyhow::bail!("lnd tls certificate is required (pin the node's tls.cert)");
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        use rustls::pki_types::pem::PemObject;
        let pinned_der = rustls::pki_types::CertificateDer::from_pem_slice(&config.tls_cert_pem)
            .map_err(|_| anyhow::anyhow!("lnd tls_cert is not a valid PEM certificate"))?;
        let verifier = Arc::new(PinningVerifier::new(pinned_der.as_ref().to_vec())?);
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls_config = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        tls_config.alpn_protocols = vec![b"h2".to_vec()];
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        Ok(GrpcTransport {
            grpc_url: config.grpc_url.clone(),
            macaroon_hex: config.macaroon_hex.clone(),
            tls_config: Arc::new(tls_config),
            runtime,
            channel: std::sync::OnceLock::new(),
        })
    }

    fn channel(&self) -> anyhow::Result<tonic::transport::Channel> {
        if let Some(channel) = self.channel.get() {
            return Ok(channel.clone());
        }
        let mut url = self.grpc_url.clone();
        if let Some(rest) = url.strip_prefix("https://") {
            url = format!("http://{rest}");
        }
        let endpoint = tonic::transport::Channel::from_shared(url)
            .map_err(|e| anyhow::anyhow!("bad lnd grpc url: {e}"))?
            .timeout(RPC_TIMEOUT)
            .connect_timeout(RPC_TIMEOUT);
        let connector = PinningConnector {
            tls_config: self.tls_config.clone(),
        };
        let channel = self
            .runtime
            .block_on(endpoint.connect_with_connector(connector))?;
        let _ = self.channel.set(channel.clone());
        Ok(channel)
    }

    fn authed<Req: prost::Message>(&self, message: Req) -> anyhow::Result<tonic::Request<Req>> {
        let mut request = tonic::Request::new(message);
        let value: tonic::metadata::MetadataValue<_> = self
            .macaroon_hex
            .parse()
            .map_err(|_| anyhow::anyhow!("lnd macaroon is not valid metadata"))?;
        request.metadata_mut().insert("macaroon", value);
        Ok(request)
    }

    fn unary<Req: prost::Message + 'static, Resp: prost::Message + Default + 'static>(
        &self,
        path: &'static str,
        message: Req,
    ) -> anyhow::Result<Resp> {
        let channel = self.channel()?;
        let request = self.authed(message)?;
        let path: http::uri::PathAndQuery = path
            .parse()
            .map_err(|e| anyhow::anyhow!("bad grpc path: {e}"))?;
        self.runtime.block_on(async move {
            let mut grpc = tonic::client::Grpc::new(channel);
            grpc.ready().await?;
            let response = grpc
                .unary(request, path, tonic::codec::ProstCodec::default())
                .await?;
            Ok(response.into_inner())
        })
    }
}

impl LndTransport for GrpcTransport {
    fn add_hold_invoice(
        &self,
        memo: &str,
        hash: [u8; 32],
        amount_sats: u64,
        expiry_secs: u64,
        cltv_expiry: u64,
    ) -> anyhow::Result<String> {
        let request = proto::AddHoldInvoiceRequest {
            memo: memo.to_string(),
            hash: hash.to_vec(),
            value: amount_sats as i64,
            expiry: expiry_secs as i64,
            cltv_expiry,
        };
        let response: proto::AddHoldInvoiceResponse =
            self.unary("/invoicesrpc.Invoices/AddHoldInvoice", request)?;
        if response.payment_request.is_empty() {
            anyhow::bail!("lnd hold invoice missing payment_request");
        }
        Ok(response.payment_request)
    }

    fn lookup_state(&self, hash: [u8; 32]) -> anyhow::Result<InvoiceState> {
        let request = proto::LookupInvoiceMsg {
            invoice_ref: Some(proto::InvoiceRef::PaymentHash(hash.to_vec())),
        };
        let invoice: proto::Invoice =
            self.unary("/invoicesrpc.Invoices/LookupInvoiceV2", request)?;
        Ok(match invoice.state {
            0 => InvoiceState::Open,
            1 => InvoiceState::Settled,
            2 => InvoiceState::Canceled,
            3 => InvoiceState::Accepted,
            _ => InvoiceState::Unknown,
        })
    }

    fn settle(&self, preimage: [u8; 32]) -> anyhow::Result<()> {
        let request = proto::SettleInvoiceMsg {
            preimage: preimage.to_vec(),
        };
        let _empty: proto::Empty = self.unary("/invoicesrpc.Invoices/SettleInvoice", request)?;
        Ok(())
    }

    fn cancel(&self, hash: [u8; 32]) -> anyhow::Result<()> {
        let request = proto::CancelInvoiceMsg {
            payment_hash: hash.to_vec(),
        };
        let _empty: proto::Empty = self.unary("/invoicesrpc.Invoices/CancelInvoice", request)?;
        Ok(())
    }

    fn initiate_payment(
        &self,
        payment_request: &str,
        timeout_secs: u64,
        fee_limit_sats: u64,
    ) -> anyhow::Result<()> {
        let channel = self.channel()?;
        let request = self.authed(proto::SendPaymentRequest {
            payment_request: payment_request.to_string(),
            timeout_seconds: timeout_secs.min(i32::MAX as u64) as i32,
            fee_limit_sat: fee_limit_sats as i64,
        })?;
        let path: http::uri::PathAndQuery = "/routerrpc.Router/SendPaymentV2"
            .parse()
            .map_err(|e| anyhow::anyhow!("bad grpc path: {e}"))?;
        self.runtime.block_on(async move {
            let mut grpc = tonic::client::Grpc::new(channel);
            grpc.ready().await?;
            let mut stream = grpc
                .server_streaming(request, path, tonic::codec::ProstCodec::default())
                .await?
                .into_inner();
            let first: Option<proto::Payment> =
                tokio::time::timeout(FIRST_UPDATE_TIMEOUT, stream.message()).await??;
            let Some(payment) = first else {
                anyhow::bail!("lightning payment ended with no update");
            };
            match payment.status {
                1 | 2 | 4 => Ok(()),
                3 => {
                    let reason = match payment.failure_reason {
                        1 => "TIMEOUT",
                        2 => "NO_ROUTE",
                        3 => "ERROR",
                        4 => "INCORRECT_PAYMENT_DETAILS",
                        5 => "INSUFFICIENT_BALANCE",
                        _ => "UNKNOWN",
                    };
                    anyhow::bail!("lightning payment failed: {reason}");
                }
                other => anyhow::bail!("lightning payment in unexpected state {other}"),
            }
        })
    }

    fn channel_balance(&self) -> anyhow::Result<u64> {
        let response: proto::ChannelBalanceResponse =
            self.unary("/lnrpc.Lightning/ChannelBalance", proto::Empty {})?;
        Ok(response.local_balance.map(|b| b.sat).unwrap_or(0))
    }
}

#[cfg(test)]
pub struct MockTransport {
    invoices: Mutex<HashMap<[u8; 32], (InvoiceState, String)>>,
    settled: Mutex<Vec<[u8; 32]>>,
    canceled: Mutex<Vec<[u8; 32]>>,
    pay_outcome: Mutex<Option<anyhow::Result<()>>>,
    balance: u64,
}

#[cfg(test)]
impl MockTransport {
    fn new(balance: u64) -> Self {
        MockTransport {
            invoices: Mutex::new(HashMap::new()),
            settled: Mutex::new(Vec::new()),
            canceled: Mutex::new(Vec::new()),
            pay_outcome: Mutex::new(None),
            balance,
        }
    }

    fn fail_next_pay(&self, reason: &str) {
        *self.pay_outcome.lock().unwrap() =
            Some(Err(anyhow::anyhow!("lightning payment failed: {reason}")));
    }

    fn set_state(&self, hash: [u8; 32], state: InvoiceState) {
        if let Some(entry) = self.invoices.lock().unwrap().get_mut(&hash) {
            entry.0 = state;
        }
    }

    fn hash_for_request(&self, payment_request: &str) -> Option<[u8; 32]> {
        self.invoices
            .lock()
            .unwrap()
            .iter()
            .find(|(_, (_, req))| req == payment_request)
            .map(|(hash, _)| *hash)
    }
}

#[cfg(test)]
impl LndTransport for MockTransport {
    fn add_hold_invoice(
        &self,
        _memo: &str,
        hash: [u8; 32],
        _amount_sats: u64,
        _expiry_secs: u64,
        _cltv_expiry: u64,
    ) -> anyhow::Result<String> {
        let request = format!("lnbc1mock{}", hex::encode(&hash[..8]));
        self.invoices
            .lock()
            .unwrap()
            .insert(hash, (InvoiceState::Open, request.clone()));
        Ok(request)
    }

    fn lookup_state(&self, hash: [u8; 32]) -> anyhow::Result<InvoiceState> {
        self.invoices
            .lock()
            .unwrap()
            .get(&hash)
            .map(|(state, _)| *state)
            .ok_or_else(|| anyhow::anyhow!("unknown invoice"))
    }

    fn settle(&self, preimage: [u8; 32]) -> anyhow::Result<()> {
        let digest = Sha256::digest(preimage);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&digest);
        let mut invoices = self.invoices.lock().unwrap();
        let entry = invoices
            .get_mut(&hash)
            .ok_or_else(|| anyhow::anyhow!("invoice not found"))?;
        if entry.0 != InvoiceState::Accepted {
            anyhow::bail!("invoice not yet paid");
        }
        entry.0 = InvoiceState::Settled;
        self.settled.lock().unwrap().push(preimage);
        Ok(())
    }

    fn cancel(&self, hash: [u8; 32]) -> anyhow::Result<()> {
        let mut invoices = self.invoices.lock().unwrap();
        let entry = invoices
            .get_mut(&hash)
            .ok_or_else(|| anyhow::anyhow!("invoice not found"))?;
        entry.0 = InvoiceState::Canceled;
        self.canceled.lock().unwrap().push(hash);
        Ok(())
    }

    fn initiate_payment(
        &self,
        payment_request: &str,
        _timeout_secs: u64,
        _fee_limit_sats: u64,
    ) -> anyhow::Result<()> {
        if let Some(outcome) = self.pay_outcome.lock().unwrap().take() {
            return outcome;
        }
        let hash = self
            .hash_for_request(payment_request)
            .ok_or_else(|| anyhow::anyhow!("lightning payment failed: NO_ROUTE"))?;
        self.set_state(hash, InvoiceState::Accepted);
        Ok(())
    }

    fn channel_balance(&self) -> anyhow::Result<u64> {
        Ok(self.balance)
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct HoldInvoice {
    #[serde(with = "hex_32")]
    payment_hash: [u8; 32],
    #[serde(with = "hex_32")]
    preimage: [u8; 32],
    amount_sats: u64,
}

mod hex_32 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'d, D: Deserializer<'d>>(d: D) -> Result<[u8; 32], D::Error> {
        let hex_str = String::deserialize(d)?;
        let bytes = hex::decode(&hex_str).map_err(serde::de::Error::custom)?;
        bytes
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

#[derive(Clone)]
pub struct LndBackend {
    config: LndConfig,
    transport: Arc<dyn LndTransport>,
    pending: Arc<Mutex<HashMap<String, HoldInvoice>>>,
    store_path: Option<std::path::PathBuf>,
}

impl LndBackend {
    pub fn new(config: LndConfig) -> anyhow::Result<Self> {
        let transport = GrpcTransport::new(&config)?;
        Ok(LndBackend {
            config,
            transport: Arc::new(transport),
            pending: Arc::new(Mutex::new(HashMap::new())),
            store_path: None,
        })
    }

    #[cfg(test)]
    fn with_transport(config: LndConfig, transport: Arc<dyn LndTransport>) -> Self {
        LndBackend {
            config,
            transport,
            pending: Arc::new(Mutex::new(HashMap::new())),
            store_path: None,
        }
    }

    pub fn with_store_path(mut self, path: std::path::PathBuf) -> Self {
        if let Ok(contents) = std::fs::read(&path) {
            if let Ok(saved) = serde_json::from_slice::<HashMap<String, HoldInvoice>>(&contents) {
                tracing::info!("restored {} LND escrows from disk", saved.len());
                *self.pending.lock().expect("lnd lock") = saved;
            } else {
                tracing::warn!("ignoring unreadable LND escrow store");
            }
        }
        self.store_path = Some(path);
        self.reconcile_restored();
        self
    }

    fn reconcile_restored(&self) {
        let restored: Vec<(String, [u8; 32])> = self
            .pending
            .lock()
            .expect("lnd lock")
            .iter()
            .map(|(id, invoice)| (id.clone(), invoice.payment_hash))
            .collect();
        if restored.is_empty() {
            return;
        }
        let mut terminal = Vec::new();
        for (id, hash) in &restored {
            match self.transport.lookup_state(*hash) {
                Ok(InvoiceState::Settled) | Ok(InvoiceState::Canceled) => {
                    terminal.push(id.clone());
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        "LND unreachable during escrow reconcile ({e}); keeping {} restored escrows",
                        restored.len()
                    );
                    return;
                }
            }
        }
        if terminal.is_empty() {
            return;
        }
        {
            let mut pending = self.pending.lock().expect("lnd lock");
            for id in &terminal {
                pending.remove(id);
            }
        }
        tracing::info!(
            "reconciled {} terminal LND escrows off disk",
            terminal.len()
        );
        self.persist();
    }

    fn persist(&self) {
        let Some(path) = &self.store_path else {
            return;
        };
        let pending = self.pending.lock().expect("lnd lock");
        let Ok(contents) = serde_json::to_vec(&*pending) else {
            return;
        };
        drop(pending);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if write_secret_file(path, &contents).is_err() {
            tracing::warn!("could not persist LND escrow store");
        }
    }

    pub fn pay_invoice_with_fee_limit(
        &self,
        payment_request: &str,
        max_fee_sats: u64,
    ) -> anyhow::Result<()> {
        self.transport.initiate_payment(
            payment_request,
            self.config.invoice_expiry_secs,
            max_fee_sats,
        )
    }

    pub fn invoice_state(&self, escrow_id: &str) -> anyhow::Result<InvoiceState> {
        let payment_hash: [u8; 32] = {
            let pending = self.pending.lock().expect("lnd lock");
            pending
                .get(escrow_id)
                .map(|invoice| invoice.payment_hash)
                .ok_or_else(|| anyhow::anyhow!("unknown lnd escrow: {escrow_id}"))?
        };
        self.transport.lookup_state(payment_hash)
    }
}

impl PaymentBackend for LndBackend {
    fn create_escrow(&self, amount: u64, payer: &str, payee: &str) -> anyhow::Result<EscrowInfo> {
        let mut preimage = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut preimage);
        let digest = Sha256::digest(preimage);
        let mut payment_hash = [0u8; 32];
        payment_hash.copy_from_slice(&digest);

        let payment_request = self.transport.add_hold_invoice(
            &format!("gitforest escrow {payer} -> {payee}"),
            payment_hash,
            amount,
            self.config.invoice_expiry_secs,
            self.config.cltv_expiry,
        )?;

        let escrow_id = uuid::Uuid::new_v4().to_string();
        tracing::info!(
            "LND hold invoice {escrow_id} for {amount} sats: {}",
            sanitize_log(&payment_request)
        );
        self.pending.lock().expect("lnd lock").insert(
            escrow_id.clone(),
            HoldInvoice {
                payment_hash,
                preimage,
                amount_sats: amount,
            },
        );
        self.persist();
        Ok(EscrowInfo {
            escrow_id,
            payment_details: payment_request,
        })
    }

    fn release_escrow(&self, channel_id: &str) -> anyhow::Result<()> {
        let invoice = {
            let pending = self.pending.lock().expect("lnd lock");
            pending
                .get(channel_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown lnd escrow: {channel_id}"))?
        };
        self.transport.settle(invoice.preimage)?;
        self.pending.lock().expect("lnd lock").remove(channel_id);
        self.persist();
        tracing::info!(
            "LND escrow {channel_id} settled ({} sats)",
            invoice.amount_sats
        );
        Ok(())
    }

    fn refund_escrow(&self, channel_id: &str) -> anyhow::Result<()> {
        let invoice = {
            let pending = self.pending.lock().expect("lnd lock");
            pending
                .get(channel_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown lnd escrow: {channel_id}"))?
        };
        self.transport.cancel(invoice.payment_hash)?;
        self.pending.lock().expect("lnd lock").remove(channel_id);
        self.persist();
        tracing::info!(
            "LND escrow {channel_id} cancelled ({} sats back to payer)",
            invoice.amount_sats
        );
        Ok(())
    }

    fn escrow_funded(&self, escrow_id: &str) -> anyhow::Result<Option<bool>> {
        Ok(match self.invoice_state(escrow_id)? {
            InvoiceState::Accepted | InvoiceState::Settled => Some(true),
            InvoiceState::Open | InvoiceState::Canceled => Some(false),
            InvoiceState::Unknown => None,
        })
    }

    fn pay_invoice(&self, payment_details: &str, amount: u64) -> anyhow::Result<()> {
        let max_fee = (amount / 100).max(1);
        self.pay_invoice_with_fee_limit(payment_details, max_fee)
    }

    fn check_balance(&self, _address: &str) -> anyhow::Result<u64> {
        self.transport.channel_balance()
    }

    fn chain_type(&self) -> ChainType {
        ChainType::BitcoinLightning
    }
}

fn write_secret_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    use std::io::Write;
    let mut file = opts.open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> LndConfig {
        LndConfig::new("https://127.0.0.1:10009".into(), "ab12".into())
    }

    fn backend_with_mock(balance: u64) -> (LndBackend, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport::new(balance));
        let backend = LndBackend::with_transport(test_config(), transport.clone());
        (backend, transport)
    }

    #[test]
    fn hold_invoice_settles_with_matching_preimage() {
        let (backend, transport) = backend_with_mock(0);
        let info = backend.create_escrow(50_000, "alice", "bob").unwrap();
        assert!(info.payment_details.starts_with("lnbc1mock"));

        backend.pay_invoice(&info.payment_details, 50_000).unwrap();
        assert_eq!(
            backend.invoice_state(&info.escrow_id).unwrap(),
            InvoiceState::Accepted
        );
        backend.release_escrow(&info.escrow_id).unwrap();

        let settled = transport.settled.lock().unwrap();
        assert_eq!(settled.len(), 1);
        assert!(settled[0] != [0u8; 32]);
        assert!(backend
            .pending
            .lock()
            .unwrap()
            .get(&info.escrow_id)
            .is_none());
    }

    #[test]
    fn settles_only_after_payment() {
        let (backend, _transport) = backend_with_mock(0);
        let info = backend.create_escrow(50_000, "alice", "bob").unwrap();
        let err = backend.release_escrow(&info.escrow_id).unwrap_err();
        assert!(err.to_string().contains("not yet paid"), "{err}");
        assert_eq!(
            backend.invoice_state(&info.escrow_id).unwrap(),
            InvoiceState::Open
        );
    }

    #[test]
    fn cancel_returns_funds_to_payer() {
        let (backend, transport) = backend_with_mock(0);
        let info = backend.create_escrow(1_000, "alice", "bob").unwrap();
        backend.pay_invoice(&info.payment_details, 1_000).unwrap();
        backend.refund_escrow(&info.escrow_id).unwrap();
        assert_eq!(transport.canceled.lock().unwrap().len(), 1);
        assert!(backend.invoice_state(&info.escrow_id).is_err());
    }

    #[test]
    fn unknown_escrow_is_rejected() {
        let (backend, _transport) = backend_with_mock(0);
        assert!(backend.release_escrow("nope").is_err());
        assert!(backend.refund_escrow("nope").is_err());
        assert!(backend.invoice_state("nope").is_err());
    }

    #[test]
    fn pay_invoice_marks_accepted() {
        let (backend, _transport) = backend_with_mock(0);
        let info = backend.create_escrow(2_000, "alice", "bob").unwrap();
        backend
            .pay_invoice_with_fee_limit(&info.payment_details, 50)
            .unwrap();
        assert_eq!(
            backend.invoice_state(&info.escrow_id).unwrap(),
            InvoiceState::Accepted
        );
        assert_eq!(backend.escrow_funded(&info.escrow_id).unwrap(), Some(true));
    }

    #[test]
    fn pay_invoice_reports_failure() {
        let (backend, transport) = backend_with_mock(0);
        transport.fail_next_pay("NO_ROUTE");
        let err = backend
            .pay_invoice_with_fee_limit("lnbc1test", 50)
            .unwrap_err();
        assert!(err.to_string().contains("NO_ROUTE"), "{err}");
    }

    #[test]
    fn escrows_survive_backend_restart() {
        let dir = std::env::temp_dir().join(format!("gf_lnd_store_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("escrows.json");
        let transport = Arc::new(MockTransport::new(0));
        let backend = LndBackend::with_transport(test_config(), transport.clone())
            .with_store_path(path.clone());
        let info = backend.create_escrow(7_000, "alice", "bob").unwrap();
        assert!(path.exists());

        let revived = LndBackend::with_transport(test_config(), transport.clone())
            .with_store_path(path.clone());
        backend.pay_invoice(&info.payment_details, 7_000).unwrap();
        revived.release_escrow(&info.escrow_id).unwrap();
        assert_eq!(transport.settled.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn escrow_store_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("gf_lnd_perm_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("escrows.json");
        let (backend, _transport) = backend_with_mock(0);
        let backend = backend.with_store_path(path.clone());
        backend.create_escrow(7_000, "alice", "bob").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_settle_keeps_escrow_for_retry() {
        let (backend, transport) = backend_with_mock(0);
        let info = backend.create_escrow(7_000, "alice", "bob").unwrap();
        let err = backend.release_escrow(&info.escrow_id).unwrap_err();
        assert!(err.to_string().contains("not yet paid"), "{err}");
        assert_eq!(
            backend.invoice_state(&info.escrow_id).unwrap(),
            InvoiceState::Open
        );
        backend.pay_invoice(&info.payment_details, 7_000).unwrap();
        backend.release_escrow(&info.escrow_id).unwrap();
        assert_eq!(transport.settled.lock().unwrap().len(), 1);
    }

    #[test]
    fn channel_balance_parses() {
        let (backend, _transport) = backend_with_mock(123_456);
        assert_eq!(backend.check_balance("").unwrap(), 123_456);
    }

    fn escrow_hash(backend: &LndBackend, escrow_id: &str) -> [u8; 32] {
        backend.pending.lock().unwrap()[escrow_id].payment_hash
    }

    #[test]
    fn startup_reconcile_drops_terminal_escrows() {
        let dir = std::env::temp_dir().join(format!("gf_lnd_recon_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("escrows.json");
        let transport = Arc::new(MockTransport::new(0));
        let backend = LndBackend::with_transport(test_config(), transport.clone())
            .with_store_path(path.clone());
        let open = backend.create_escrow(1_000, "a", "b").unwrap();
        let done = backend.create_escrow(2_000, "a", "b").unwrap();
        backend.pay_invoice(&done.payment_details, 2_000).unwrap();
        transport.set_state(
            escrow_hash(&backend, &done.escrow_id),
            InvoiceState::Settled,
        );

        let revived = LndBackend::with_transport(test_config(), transport.clone())
            .with_store_path(path.clone());
        let pending = revived.pending.lock().unwrap();
        assert!(pending.get(&open.escrow_id).is_some());
        assert!(pending.get(&done.escrow_id).is_none());
        drop(pending);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_reconcile_keeps_escrows_when_lnd_down() {
        let dir = std::env::temp_dir().join(format!("gf_lnd_recon_down_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("escrows.json");
        let transport = Arc::new(MockTransport::new(0));
        let backend = LndBackend::with_transport(test_config(), transport.clone())
            .with_store_path(path.clone());
        let info = backend.create_escrow(1_000, "a", "b").unwrap();

        let fresh_transport = Arc::new(MockTransport::new(0));
        let revived = LndBackend::with_transport(test_config(), fresh_transport)
            .with_store_path(path.clone());
        assert!(revived
            .pending
            .lock()
            .unwrap()
            .get(&info.escrow_id)
            .is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grpc_transport_requires_tls_cert() {
        let config = LndConfig::new("https://127.0.0.1:10009".into(), "ab12".into());
        let err = GrpcTransport::new(&config)
            .err()
            .expect("missing cert must fail");
        assert!(err.to_string().contains("tls certificate"), "{err}");
    }

    #[test]
    fn proto_messages_roundtrip() {
        use prost::Message;
        let req = proto::AddHoldInvoiceRequest {
            memo: "test".into(),
            hash: vec![9u8; 32],
            value: 50000,
            expiry: 86400,
            cltv_expiry: 40,
        };
        let bytes = req.encode_to_vec();
        let decoded = proto::AddHoldInvoiceRequest::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded, req);
        let pay = proto::Payment {
            status: 1,
            failure_reason: 0,
        };
        let bytes = pay.encode_to_vec();
        let decoded = proto::Payment::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.status, 1);
    }
}
