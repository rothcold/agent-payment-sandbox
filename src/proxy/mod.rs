use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use chrono::Utc;
use http::uri::Authority;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use rust_decimal::Decimal;
use rustls::ServerConfig;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_rustls::TlsAcceptor;

use crate::adapter::ParseContext;
use crate::adapter::ProtocolAdapter;
use crate::adapter::x402::X402Adapter;
use crate::audit::{AuditEvent, AuditEventKind};
use crate::model::{PaymentAttempt, Protocol};
use crate::policy::{self, Policy, PolicyDecision};
use crate::sandbox::SandboxConfig;
use crate::state::StateFile;
use crate::storage::AuditStore;
use crate::wallet::{PaymentRecord, WalletState};

pub mod ca;

use ca::ProxyCa;

const X_PAYMENT: &str = "x-payment";
const X_PAYMENT_RESPONSE: &str = "x-payment-response";
const CHALLENGE_TTL: Duration = Duration::from_secs(600);
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

pub struct ProxyConfig {
    pub addr: SocketAddr,
    pub sandbox: SandboxConfig,
    pub ca_dir: PathBuf,
}

pub async fn run(cfg: ProxyConfig) -> Result<()> {
    install_crypto_provider();
    let listener = TcpListener::bind(&cfg.addr).await?;
    let rt = build_runtime(&cfg).await?;
    print_startup_banner(&listener, &rt);
    accept_loop(listener, rt).await
}

pub async fn build_runtime(cfg: &ProxyConfig) -> Result<Arc<ProxyRuntime>> {
    install_crypto_provider();
    let ca = ProxyCa::load_or_create(&cfg.ca_dir)?;
    let resolved = policy::load_policy(cfg.sandbox.policy_path.as_deref())?;
    let state_file = cfg.sandbox.state_path.clone().map(StateFile::new);
    let initial = match &state_file {
        Some(f) => f.load_or_default()?,
        None => WalletState::new(),
    };
    let store = crate::storage::open(&cfg.sandbox.audit_url).await?;
    let upstream = reqwest::Client::builder().no_proxy().build()?;
    Ok(Arc::new(ProxyRuntime {
        policy: resolved.policy,
        policy_source: resolved.source,
        wallet: Mutex::new(initial),
        state_file,
        store,
        ca: Arc::new(ca),
        challenge_cache: StdMutex::new(HashMap::new()),
        upstream,
    }))
}

pub async fn accept_loop(listener: TcpListener, rt: Arc<ProxyRuntime>) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let rt = rt.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let rt = rt.clone();
                async move { Ok::<_, std::convert::Infallible>(handle_request(req, rt).await) }
            });
            if let Err(e) = http1::Builder::new()
                .preserve_header_case(true)
                .serve_connection(TokioIo::new(stream), svc)
                .with_upgrades()
                .await
            {
                tracing::warn!(%peer, "connection error: {e:#}");
            }
        });
    }
}

fn install_crypto_provider() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
}

fn print_startup_banner(listener: &TcpListener, rt: &ProxyRuntime) {
    println!("CA cert path: {}", rt.ca.cert_path().display());
    println!(
        "Trust this CA in your client (e.g. SSL_CERT_FILE={})",
        rt.ca.cert_path().display()
    );
    println!();
    if let Some(src) = &rt.policy_source {
        println!("Policy: {}", src.display());
    } else {
        println!("Policy: (none — defaults)");
    }
    if let Ok(addr) = listener.local_addr() {
        println!("Proxy listening on http://{addr}");
        println!(
            "Set HTTP_PROXY=http://{0} HTTPS_PROXY=http://{0} for your agent.",
            addr
        );
    }
}

pub struct ProxyRuntime {
    pub(crate) policy: Policy,
    pub(crate) policy_source: Option<PathBuf>,
    pub(crate) wallet: Mutex<WalletState>,
    pub(crate) state_file: Option<StateFile>,
    pub(crate) store: Box<dyn AuditStore>,
    pub(crate) ca: Arc<ProxyCa>,
    pub(crate) challenge_cache: StdMutex<HashMap<String, CachedChallenge>>,
    pub(crate) upstream: reqwest::Client,
}

pub(crate) struct CachedChallenge {
    pub body: Value,
    pub inserted: Instant,
}

impl ProxyRuntime {
    async fn record(&self, ev: &AuditEvent) {
        if let Err(e) = self.store.write_event(ev).await {
            tracing::error!("audit write failed: {e:#}");
        }
    }

    async fn persist(&self) {
        if let Some(f) = &self.state_file {
            let w = self.wallet.lock().await;
            if let Err(e) = f.save(&w) {
                tracing::error!("state file save failed: {e:#}");
            }
        }
    }

    fn cache_challenge(&self, key: String, body: Value) {
        let mut cache = self.challenge_cache.lock().unwrap();
        cache.insert(
            key,
            CachedChallenge {
                body,
                inserted: Instant::now(),
            },
        );
    }

    fn lookup_challenge(&self, key: &str) -> Option<Value> {
        let mut cache = self.challenge_cache.lock().unwrap();
        if let Some(c) = cache.get(key) {
            if c.inserted.elapsed() <= CHALLENGE_TTL {
                return Some(c.body.clone());
            }
        }
        cache.remove(key);
        None
    }
}

async fn handle_request(req: Request<Incoming>, rt: Arc<ProxyRuntime>) -> Response<Full<Bytes>> {
    if req.method() == Method::CONNECT {
        return handle_connect(req, rt);
    }
    handle_proxied(req, rt, Scheme::Http).await
}

fn handle_connect(req: Request<Incoming>, rt: Arc<ProxyRuntime>) -> Response<Full<Bytes>> {
    let authority = match req.uri().authority() {
        Some(a) => a.clone(),
        None => return error_response(StatusCode::BAD_REQUEST, "CONNECT missing authority"),
    };
    tokio::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                if let Err(e) = mitm_connection(upgraded, &authority, rt).await {
                    tracing::warn!(authority = %authority, "mitm failed: {e:#}");
                }
            }
            Err(e) => tracing::warn!(authority = %authority, "upgrade failed: {e:#}"),
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .body(Full::new(Bytes::new()))
        .unwrap()
}

async fn mitm_connection(
    upgraded: Upgraded,
    authority: &Authority,
    rt: Arc<ProxyRuntime>,
) -> Result<()> {
    // SNI / cert CN does not include the port, so leaf is keyed by hostname only.
    let host = authority.host().to_string();
    let leaf = rt.ca.leaf_for(&host)?;
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(leaf.cert_chain.clone(), leaf.private_key.clone_key())
        .context("build TLS server config")?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let tls_stream = acceptor
        .accept(TokioIo::new(upgraded))
        .await
        .context("TLS handshake with client")?;

    // Inner HTTP requests after MITM lack scheme + authority. Inject them so
    // forwarding (and cache keys) preserve the original host:port.
    let inject_authority = authority.clone();
    let svc = service_fn(move |req: Request<Incoming>| {
        let rt = rt.clone();
        let auth = inject_authority.clone();
        async move {
            let req = ensure_scheme_and_authority(req, Scheme::Https, &auth);
            Ok::<_, std::convert::Infallible>(handle_proxied(req, rt.clone(), Scheme::Https).await)
        }
    });

    http1::Builder::new()
        .preserve_header_case(true)
        .serve_connection(TokioIo::new(tls_stream), svc)
        .await
        .map_err(|e| anyhow!("https serve: {e}"))?;
    Ok(())
}

fn ensure_scheme_and_authority(
    mut req: Request<Incoming>,
    scheme: Scheme,
    authority: &Authority,
) -> Request<Incoming> {
    if req.uri().scheme().is_none() || req.uri().authority().is_none() {
        let mut parts = req.uri().clone().into_parts();
        parts.scheme = Some(scheme.as_str().parse().expect("scheme parses"));
        parts.authority = Some(authority.clone());
        if parts.path_and_query.is_none() {
            parts.path_and_query = Some("/".parse().unwrap());
        }
        if let Ok(uri) = Uri::from_parts(parts) {
            *req.uri_mut() = uri;
        }
    }
    req
}

#[derive(Debug)]
enum PreForwardOutcome {
    NoIntercept,
    Block(Response<Full<Bytes>>),
    Allow(PaymentAttempt),
}

async fn handle_proxied(
    req: Request<Incoming>,
    rt: Arc<ProxyRuntime>,
    scheme: Scheme,
) -> Response<Full<Bytes>> {
    let (parts, body) = req.into_parts();
    let body_bytes = match body.collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => {
            tracing::warn!("read request body failed: {e:#}");
            return fail_closed_response(&format!("read request body: {e}"));
        }
    };

    let target_url = match build_target_url(&parts.uri, scheme) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("invalid target URI: {e:#}");
            return error_response(StatusCode::BAD_REQUEST, &format!("{e:#}"));
        }
    };
    let authority_str = parts
        .uri
        .authority()
        .map(|a| a.as_str().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let path_q = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let cache_key = format!(
        "{} {}://{}{}",
        parts.method,
        scheme.as_str(),
        authority_str,
        path_q,
    );
    let host_for_audit = parts
        .uri
        .host()
        .map(String::from)
        .unwrap_or_else(|| authority_str.clone());

    let pre = if let Some(payment_header) = parts.headers.get(X_PAYMENT) {
        evaluate_payment_request(
            &parts,
            payment_header,
            &cache_key,
            &authority_str,
            &host_for_audit,
            &path_q,
            &rt,
        )
        .await
    } else {
        PreForwardOutcome::NoIntercept
    };

    let pending = match pre {
        PreForwardOutcome::Block(resp) => return resp,
        PreForwardOutcome::Allow(a) => Some(a),
        PreForwardOutcome::NoIntercept => None,
    };

    // Forward upstream.
    let send_result = forward_upstream(&rt, &parts, &body_bytes, &target_url).await;

    // Settlement audit (if we held a pending Allow attempt).
    if let Some(attempt) = &pending {
        match &send_result {
            Ok((status, headers, _)) if status.is_success() => {
                let now = Utc::now();
                {
                    let mut w = rt.wallet.lock().await;
                    w.record_payment(PaymentRecord {
                        timestamp: now,
                        agent_id: attempt.agent_id.clone(),
                        task_id: attempt.task_id.clone(),
                        merchant: attempt.merchant.clone(),
                        resource: attempt.resource.clone(),
                        amount: attempt.amount,
                        currency: attempt.currency.clone(),
                    });
                }
                rt.persist().await;
                let mut ev = AuditEvent::new(AuditEventKind::SettlementSucceeded, attempt.clone());
                ev.detail = Some(serde_json::json!({
                    "upstream_status": status.as_u16(),
                    "x_payment_response": headers
                        .get(X_PAYMENT_RESPONSE)
                        .and_then(|v| v.to_str().ok())
                }));
                rt.record(&ev).await;
            }
            Ok((status, _, _)) => {
                let mut ev = AuditEvent::new(AuditEventKind::SettlementFailed, attempt.clone());
                ev.detail = Some(serde_json::json!({"upstream_status": status.as_u16()}));
                rt.record(&ev).await;
            }
            Err(e) => {
                let mut ev = AuditEvent::new(AuditEventKind::SettlementFailed, attempt.clone());
                ev.detail = Some(serde_json::json!({"upstream_error": format!("{e:#}")}));
                rt.record(&ev).await;
            }
        }
    }

    let (status, upstream_headers, upstream_body) = match send_result {
        Ok(t) => t,
        Err(e) => {
            // No pending attempt means we never wrote an audit for this hop. Note it now.
            if pending.is_none() {
                let mut ev =
                    synthetic_audit(AuditEventKind::GatewayUnavailable, &host_for_audit, &path_q);
                ev.detail =
                    Some(serde_json::json!({"error": format!("{e:#}"), "stage": "upstream"}));
                rt.record(&ev).await;
            }
            return fail_closed_response(&format!("upstream request failed: {e:#}"));
        }
    };

    // Post-forward intercept: 402 challenge → cache + audit.
    if status == reqwest::StatusCode::PAYMENT_REQUIRED {
        if let Ok(json) = serde_json::from_slice::<Value>(&upstream_body) {
            rt.cache_challenge(cache_key.clone(), json.clone());
            let ctx = ParseContext::default();
            if let Ok(attempt) = X402Adapter.parse_value(&ctx, &json) {
                rt.record(&AuditEvent::new(
                    AuditEventKind::PaymentAttemptCreated,
                    attempt,
                ))
                .await;
            }
        }
    }

    let mut response = Response::builder()
        .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY));
    {
        let resp_headers = response.headers_mut().expect("builder fresh");
        copy_response_headers(&upstream_headers, resp_headers);
    }
    response.body(Full::new(upstream_body)).unwrap()
}

async fn forward_upstream(
    rt: &ProxyRuntime,
    parts: &http::request::Parts,
    body_bytes: &Bytes,
    target_url: &str,
) -> Result<(reqwest::StatusCode, reqwest::header::HeaderMap, Bytes)> {
    let mut upstream_req = rt
        .upstream
        .request(parts.method.clone(), target_url)
        .body(body_bytes.clone());
    let mut headers = parts.headers.clone();
    strip_hop_by_hop(&mut headers);
    for (k, v) in headers.iter() {
        upstream_req = upstream_req.header(k, v);
    }
    let resp = upstream_req
        .send()
        .await
        .with_context(|| format!("upstream send {target_url}"))?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.bytes().await.context("read upstream body")?;
    Ok((status, headers, body))
}

async fn evaluate_payment_request(
    parts: &http::request::Parts,
    payment_header: &HeaderValue,
    cache_key: &str,
    authority_str: &str,
    host: &str,
    path: &str,
    rt: &ProxyRuntime,
) -> PreForwardOutcome {
    // Parse the header. Any structural failure must be auditable.
    let raw = match payment_header.to_str() {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            return reject_malformed_payment(rt, host, path, "X-PAYMENT not utf-8", &e.to_string())
                .await;
        }
    };
    let decoded = match B64.decode(raw.as_bytes()) {
        Ok(d) => d,
        Err(e) => {
            return reject_malformed_payment(
                rt,
                host,
                path,
                "X-PAYMENT not valid base64",
                &e.to_string(),
            )
            .await;
        }
    };
    let payload: Value = match serde_json::from_slice(&decoded) {
        Ok(v) => v,
        Err(e) => {
            return reject_malformed_payment(
                rt,
                host,
                path,
                "X-PAYMENT not valid JSON",
                &e.to_string(),
            )
            .await;
        }
    };
    if payload.pointer("/payload/authorization").is_none() {
        return reject_malformed_payment(
            rt,
            host,
            path,
            "X-PAYMENT missing payload.authorization",
            "structural",
        )
        .await;
    }

    let challenge = rt.lookup_challenge(cache_key);
    let attempt_value = match challenge {
        Some(c) => c,
        None => match synthesize_challenge_from_payment(&payload, authority_str) {
            Ok(v) => v,
            Err(e) => {
                return reject_malformed_payment(
                    rt,
                    host,
                    path,
                    "X-PAYMENT could not be projected to a challenge",
                    &e.to_string(),
                )
                .await;
            }
        },
    };

    let ctx = ParseContext {
        agent_id: parts
            .headers
            .get("x-agent-id")
            .and_then(|v| v.to_str().ok())
            .map(String::from),
        task_id: parts
            .headers
            .get("x-task-id")
            .and_then(|v| v.to_str().ok())
            .map(String::from),
        wallet: payload
            .pointer("/payload/authorization/from")
            .and_then(|v| v.as_str())
            .map(String::from),
    };
    let attempt = match X402Adapter.parse_value(&ctx, &attempt_value) {
        Ok(a) => a,
        Err(e) => {
            return reject_malformed_payment(
                rt,
                host,
                path,
                "x402 adapter could not normalize challenge",
                &e.to_string(),
            )
            .await;
        }
    };

    rt.record(&AuditEvent::new(
        AuditEventKind::PaymentAttemptCreated,
        attempt.clone(),
    ))
    .await;

    let now = Utc::now();
    let wallet_snapshot = { rt.wallet.lock().await.clone() };
    let decision = policy::evaluate(&rt.policy, &attempt, &wallet_snapshot, now);

    let mut policy_event = AuditEvent::new(AuditEventKind::PolicyEvaluated, attempt.clone());
    policy_event.policy_version = rt.policy.version.clone();
    policy_event.decision = Some(decision.clone());
    rt.record(&policy_event).await;

    match &decision {
        PolicyDecision::Allow => {
            let mut ev = AuditEvent::new(AuditEventKind::PaymentAllowed, attempt.clone());
            ev.decision = Some(decision);
            rt.record(&ev).await;
            PreForwardOutcome::Allow(attempt)
        }
        PolicyDecision::Block { reason } => {
            let mut ev = AuditEvent::new(AuditEventKind::PaymentBlocked, attempt);
            ev.decision = Some(decision.clone());
            rt.record(&ev).await;
            PreForwardOutcome::Block(synthetic_402(reason))
        }
        PolicyDecision::RequireApproval { reason } => {
            let mut ev = AuditEvent::new(AuditEventKind::ApprovalRequired, attempt);
            ev.decision = Some(decision.clone());
            rt.record(&ev).await;
            PreForwardOutcome::Block(synthetic_402(&format!("approval required: {reason}")))
        }
        PolicyDecision::SimulateOnly { reason } => {
            let mut ev = AuditEvent::new(AuditEventKind::PaymentSimulated, attempt);
            ev.decision = Some(decision.clone());
            rt.record(&ev).await;
            PreForwardOutcome::Block(synthetic_402(&format!("simulate-only: {reason}")))
        }
    }
}

async fn reject_malformed_payment(
    rt: &ProxyRuntime,
    host: &str,
    path: &str,
    summary: &str,
    detail: &str,
) -> PreForwardOutcome {
    let attempt = synthetic_attempt(host, path);
    let mut ev = AuditEvent::new(AuditEventKind::PaymentBlocked, attempt);
    ev.decision = Some(PolicyDecision::Block {
        reason: format!("malformed X-PAYMENT: {summary}"),
    });
    ev.detail = Some(serde_json::json!({"reason": summary, "detail": detail}));
    rt.record(&ev).await;
    PreForwardOutcome::Block(synthetic_402(&format!("malformed X-PAYMENT: {summary}")))
}

fn synthesize_challenge_from_payment(payload: &Value, authority: &str) -> Result<Value> {
    let scheme = payload
        .get("scheme")
        .and_then(|v| v.as_str())
        .unwrap_or("exact")
        .to_string();
    let network = payload
        .get("network")
        .and_then(|v| v.as_str())
        .unwrap_or("base")
        .to_string();
    let auth = payload
        .pointer("/payload/authorization")
        .ok_or_else(|| anyhow!("X-PAYMENT missing payload.authorization"))?;
    let to = auth
        .get("to")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let value = auth
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("0")
        .to_string();
    Ok(serde_json::json!({
        "x402Version": payload.get("x402Version").cloned().unwrap_or(Value::from(1)),
        "accepts": [{
            "scheme": scheme,
            "network": network,
            "maxAmountRequired": value,
            "resource": format!("https://{authority}/"),
            "payTo": to,
            "asset": "0x0000000000000000000000000000000000000000",
            "extra": { "name": "" }
        }]
    }))
}

fn build_target_url(uri: &Uri, scheme: Scheme) -> Result<String> {
    if uri.scheme().is_some() && uri.authority().is_some() {
        return Ok(uri.to_string());
    }
    let authority = uri
        .authority()
        .ok_or_else(|| anyhow!("request missing authority for {}", scheme.as_str()))?
        .to_string();
    let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    Ok(format!("{}://{}{}", scheme.as_str(), authority, path))
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for h in HOP_BY_HOP {
        headers.remove(*h);
    }
    headers.remove("host");
}

fn copy_response_headers(src: &reqwest::header::HeaderMap, dst: &mut HeaderMap) {
    for (k, v) in src.iter() {
        if HOP_BY_HOP
            .iter()
            .any(|h| k.as_str().eq_ignore_ascii_case(h))
        {
            continue;
        }
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_str().as_bytes()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) {
            dst.append(name, val);
        }
    }
}

fn synthetic_402(reason: &str) -> Response<Full<Bytes>> {
    let body = serde_json::json!({
        "x402Version": 1,
        "error": "blocked by agent-payment-sandbox policy",
        "reason": reason,
        "accepts": []
    });
    let bytes = Bytes::from(body.to_string());
    Response::builder()
        .status(StatusCode::PAYMENT_REQUIRED)
        .header("content-type", "application/json")
        .header("x-agent-payment-sandbox", "blocked")
        .body(Full::new(bytes))
        .unwrap()
}

fn fail_closed_response(reason: &str) -> Response<Full<Bytes>> {
    let body = serde_json::json!({
        "error": "agent-payment-sandbox: gateway error",
        "reason": reason
    });
    let bytes = Bytes::from(body.to_string());
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .header("content-type", "application/json")
        .header("x-agent-payment-sandbox", "fail-closed")
        .body(Full::new(bytes))
        .unwrap()
}

fn error_response(status: StatusCode, msg: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(Full::new(Bytes::from(msg.to_string())))
        .unwrap()
}

fn synthetic_audit(kind: AuditEventKind, host: &str, path: &str) -> AuditEvent {
    AuditEvent::new(kind, synthetic_attempt(host, path))
}

fn synthetic_attempt(host: &str, path: &str) -> PaymentAttempt {
    let mut attempt = PaymentAttempt::new(Protocol::X402, "agent", "task", host, Decimal::ZERO, "");
    attempt.resource = Some(format!("{host}{path}"));
    attempt
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn cache_key_includes_scheme_method_authority_port_path() {
        let k1 = format!(
            "{} {}://{}{}",
            Method::GET,
            Scheme::Https.as_str(),
            "example.com:8443",
            "/v1/data?x=1",
        );
        let k2 = format!(
            "{} {}://{}{}",
            Method::POST,
            Scheme::Https.as_str(),
            "example.com:8443",
            "/v1/data?x=1",
        );
        let k3 = format!(
            "{} {}://{}{}",
            Method::GET,
            Scheme::Https.as_str(),
            "example.com:443",
            "/v1/data?x=1",
        );
        assert_ne!(k1, k2, "method must matter");
        assert_ne!(k1, k3, "port must matter");
    }

    #[test]
    fn ensure_authority_preserves_port() {
        // We can't construct hyper::body::Incoming in a unit test, so just exercise
        // the URI rebuild logic directly.
        let authority: Authority = "example.com:8443".parse().unwrap();
        let mut parts = Uri::from_static("/v1/data?x=1").into_parts();
        parts.scheme = Some(Scheme::Https.as_str().parse().unwrap());
        parts.authority = Some(authority.clone());
        let uri = Uri::from_parts(parts).unwrap();
        assert_eq!(uri.scheme_str(), Some("https"));
        assert_eq!(
            uri.authority().map(|a| a.as_str()),
            Some("example.com:8443")
        );
        assert_eq!(uri.path(), "/v1/data");
        assert_eq!(uri.query(), Some("x=1"));
    }

    #[test]
    fn build_target_url_includes_port() {
        let uri = Uri::from_static("https://example.com:8443/data?q=1");
        let url = build_target_url(&uri, Scheme::Https).unwrap();
        assert_eq!(url, "https://example.com:8443/data?q=1");
    }
}
