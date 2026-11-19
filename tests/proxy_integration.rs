#![cfg(feature = "sqlite")]

//! Live integration tests for the proxy. Each test:
//!   1. Spawns an in-process upstream HTTP server on 127.0.0.1:0
//!   2. Spawns the proxy on 127.0.0.1:0 with a fresh sqlite audit DB and JSON state file
//!   3. Sends requests via reqwest configured to use the proxy
//!   4. Asserts on response, audit log (sqlite), and persisted wallet (state.json)

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use agent_payment_sandbox::proxy::{self, ProxyConfig};
use agent_payment_sandbox::sandbox::SandboxConfig;
use agent_payment_sandbox::wallet::WalletState;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use sqlx::Row;
use tempfile::TempDir;
use tokio::net::TcpListener;

/// Behavior the mock upstream should exhibit on a given request.
#[derive(Clone)]
struct MockHandler(Arc<dyn Fn(&MockRequest) -> MockResponse + Send + Sync>);

#[derive(Debug, Clone)]
struct MockRequest {
    #[allow(dead_code)]
    method: Method,
    path: String,
    x_payment: Option<String>,
}

struct MockResponse {
    status: u16,
    body: Bytes,
    headers: Vec<(String, String)>,
}

#[derive(Default)]
struct MockState {
    requests: StdMutex<Vec<MockRequest>>,
}

async fn spawn_upstream(handler: MockHandler) -> (SocketAddr, Arc<MockState>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(MockState::default());
    let st = state.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => break,
            };
            let handler = handler.clone();
            let state = st.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let handler = handler.clone();
                    let state = state.clone();
                    async move {
                        let mock_req = MockRequest {
                            method: req.method().clone(),
                            path: req.uri().path().to_string(),
                            x_payment: req
                                .headers()
                                .get("x-payment")
                                .and_then(|v| v.to_str().ok())
                                .map(String::from),
                        };
                        state.requests.lock().unwrap().push(mock_req.clone());
                        let resp = (handler.0)(&mock_req);
                        let mut builder = Response::builder().status(resp.status);
                        for (k, v) in resp.headers {
                            builder = builder.header(k, v);
                        }
                        Ok::<_, Infallible>(builder.body(Full::new(resp.body)).unwrap())
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    (addr, state)
}

struct Harness {
    proxy_url: String,
    upstream_addr: SocketAddr,
    audit_db_path: PathBuf,
    state_path: PathBuf,
    _tmp: TempDir,
    _upstream_state: Arc<MockState>,
}

async fn start_harness(policy_yaml: &str, handler: MockHandler) -> Harness {
    let (upstream_addr, upstream_state) = spawn_upstream(handler).await;

    let tmp = tempfile::tempdir().unwrap();
    let policy_path = tmp.path().join("policy.yaml");
    std::fs::write(&policy_path, policy_yaml).unwrap();
    let state_path = tmp.path().join("state.json");
    let audit_db_path = tmp.path().join("audit.db");
    let ca_dir = tmp.path().join("ca");

    let cfg = ProxyConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        sandbox: SandboxConfig {
            policy_path: Some(policy_path),
            state_path: Some(state_path.clone()),
            audit_url: format!("sqlite://{}?mode=rwc", audit_db_path.display()),
        },
        ca_dir,
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let rt = proxy::build_runtime(&cfg).await.unwrap();
    tokio::spawn(async move {
        let _ = proxy::accept_loop(listener, rt).await;
    });

    // Give the spawned task a moment to start accepting.
    tokio::time::sleep(Duration::from_millis(50)).await;

    Harness {
        proxy_url: format!("http://{proxy_addr}"),
        upstream_addr,
        audit_db_path,
        state_path,
        _tmp: tmp,
        _upstream_state: upstream_state,
    }
}

impl Harness {
    fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::http(&self.proxy_url).unwrap())
            .build()
            .unwrap()
    }

    fn upstream_url(&self, path: &str) -> String {
        format!("http://{}{}", self.upstream_addr, path)
    }

    async fn audit_kinds(&self) -> Vec<String> {
        // Wait briefly for async writes to land.
        for _ in 0..20 {
            let kinds = self.audit_kinds_now().await;
            if !kinds.is_empty() {
                return kinds;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.audit_kinds_now().await
    }

    async fn audit_kinds_now(&self) -> Vec<String> {
        let url = format!("sqlite://{}", self.audit_db_path.display());
        let pool = match sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
        {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };
        let rows = sqlx::query("SELECT kind FROM payment_events ORDER BY timestamp")
            .fetch_all(&pool)
            .await
            .unwrap_or_default();
        rows.into_iter()
            .map(|r| r.try_get::<String, _>("kind").unwrap_or_default())
            .collect()
    }

    fn wallet(&self) -> WalletState {
        if !self.state_path.exists() {
            return WalletState::default();
        }
        let raw = std::fs::read_to_string(&self.state_path).unwrap_or_default();
        if raw.trim().is_empty() {
            return WalletState::default();
        }
        serde_json::from_str(&raw).unwrap_or_default()
    }
}

fn challenge_body(addr: SocketAddr, path: &str, atomic_amount: &str) -> String {
    serde_json::json!({
        "x402Version": 1,
        "accepts": [{
            "scheme": "exact",
            "network": "base",
            "maxAmountRequired": atomic_amount,
            "resource": format!("http://{addr}{path}"),
            "description": "test",
            "mimeType": "application/json",
            "payTo": "0x000000000000000000000000000000000000dEaD",
            "asset": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
            "maxTimeoutSeconds": 60,
            "extra": { "name": "USDC", "version": "2" }
        }]
    })
    .to_string()
}

const ALLOW_ANY_POLICY: &str = r#"
agent: test
mode: sandbox
version: itest-v1
limits:
  max_per_payment:
    amount: "1.00"
    currency: USD
allow:
  currencies: ["USDC"]
  networks: ["base"]
"#;

fn x_payment_for(value_atomic: &str) -> String {
    use base64::Engine;
    let payload = serde_json::json!({
        "x402Version": 1,
        "scheme": "exact",
        "network": "base",
        "payload": {
            "signature": "0xabcd",
            "authorization": {
                "from": "0xdead",
                "to": "0x000000000000000000000000000000000000dEaD",
                "value": value_atomic,
                "validAfter": "0",
                "validBefore": "9999999999",
                "nonce": "0x01",
            }
        }
    });
    base64::engine::general_purpose::STANDARD.encode(payload.to_string())
}

// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn http_402_caches_then_xpayment_replay_reaches_upstream() {
    let upstream_for_handler: Arc<StdMutex<Option<SocketAddr>>> = Arc::new(StdMutex::new(None));
    let cap = upstream_for_handler.clone();

    let handler = MockHandler(Arc::new(move |req| {
        let upstream_addr = cap.lock().unwrap().clone().unwrap();
        if let Some(_x) = &req.x_payment {
            MockResponse {
                status: 200,
                body: Bytes::from(r#"{"ok":true}"#),
                headers: vec![("content-type".into(), "application/json".into())],
            }
        } else {
            MockResponse {
                status: 402,
                body: Bytes::from(challenge_body(upstream_addr, &req.path, "100000")),
                headers: vec![("content-type".into(), "application/json".into())],
            }
        }
    }));

    let h = start_harness(ALLOW_ANY_POLICY, handler).await;
    *upstream_for_handler.lock().unwrap() = Some(h.upstream_addr);
    let url = h.upstream_url("/v1/data");
    let client = h.client();

    // Step 1: GET without X-PAYMENT → 402 from upstream is forwarded; proxy caches it.
    let r1 = client.get(&url).send().await.unwrap();
    assert_eq!(r1.status(), 402);

    // Step 2: GET with X-PAYMENT for 0.10 USDC → policy allows, upstream returns 200.
    let pay = x_payment_for("100000");
    let r2 = client
        .get(&url)
        .header("X-PAYMENT", pay)
        .send()
        .await
        .unwrap();
    assert_eq!(r2.status(), 200);
    let json: Value = r2.json().await.unwrap();
    assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(true));

    let kinds = h.audit_kinds().await;
    assert!(
        kinds.contains(&"payment_attempt_created".to_string())
            && kinds.contains(&"policy_evaluated".to_string())
            && kinds.contains(&"payment_allowed".to_string())
            && kinds.contains(&"settlement_succeeded".to_string()),
        "missing expected events; got {kinds:?}"
    );
    assert!(
        !kinds.contains(&"settlement_failed".to_string()),
        "should not have settlement_failed on a 200 upstream"
    );

    let wallet = h.wallet();
    assert_eq!(wallet.history.len(), 1, "exactly one settled spend");
    assert_eq!(wallet.history[0].merchant.contains("127.0.0.1"), true);
}

#[tokio::test]
async fn malformed_x_payment_audits_block() {
    let handler = MockHandler(Arc::new(|_req| MockResponse {
        status: 500,
        body: Bytes::from("upstream should never be reached"),
        headers: vec![],
    }));

    let h = start_harness(ALLOW_ANY_POLICY, handler).await;
    let url = h.upstream_url("/v1/data");
    let client = h.client();

    let r = client
        .get(&url)
        .header("X-PAYMENT", "***not-base64***")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 402, "synthetic 402 with reason");
    let body = r.text().await.unwrap();
    assert!(body.contains("malformed X-PAYMENT"), "body: {body}");

    let kinds = h.audit_kinds().await;
    assert!(
        kinds.contains(&"payment_blocked".to_string()),
        "expected payment_blocked, got {kinds:?}"
    );
    assert!(
        !kinds.contains(&"settlement_succeeded".to_string())
            && !kinds.contains(&"settlement_failed".to_string()),
        "no settlement on a malformed header"
    );

    // Wallet must remain empty — request never reached the upstream.
    let wallet = h.wallet();
    assert!(wallet.history.is_empty(), "wallet should be untouched");
}

#[tokio::test]
async fn allow_then_upstream_500_does_not_credit_spend() {
    let upstream_for_handler: Arc<StdMutex<Option<SocketAddr>>> = Arc::new(StdMutex::new(None));
    let cap = upstream_for_handler.clone();

    let handler = MockHandler(Arc::new(move |req| {
        let upstream_addr = cap.lock().unwrap().clone().unwrap();
        if req.x_payment.is_some() {
            // Pretend the facilitator returned an error.
            MockResponse {
                status: 500,
                body: Bytes::from(r#"{"error":"settlement failed"}"#),
                headers: vec![],
            }
        } else {
            MockResponse {
                status: 402,
                body: Bytes::from(challenge_body(upstream_addr, &req.path, "100000")),
                headers: vec![],
            }
        }
    }));

    let h = start_harness(ALLOW_ANY_POLICY, handler).await;
    *upstream_for_handler.lock().unwrap() = Some(h.upstream_addr);
    let url = h.upstream_url("/v1/data");
    let client = h.client();

    // Prime the cache.
    let _ = client.get(&url).send().await.unwrap();

    // Pay — policy allows but upstream 500s.
    let pay = x_payment_for("100000");
    let r = client
        .get(&url)
        .header("X-PAYMENT", pay)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 500, "upstream status passed through");

    let kinds = h.audit_kinds().await;
    assert!(
        kinds.contains(&"payment_allowed".to_string())
            && kinds.contains(&"settlement_failed".to_string()),
        "expected allowed + settlement_failed, got {kinds:?}"
    );
    assert!(
        !kinds.contains(&"settlement_succeeded".to_string()),
        "must not be marked succeeded"
    );

    let wallet = h.wallet();
    assert!(
        wallet.history.is_empty(),
        "spend must not be credited on settlement failure: {:?}",
        wallet.history
    );
}

#[tokio::test]
async fn cache_key_separates_custom_ports() {
    // Two upstreams. Each returns its OWN port number in the response body so we can
    // confirm the proxy didn't cross-wire them.
    let port_a_handler = MockHandler(Arc::new(|req| {
        if req.x_payment.is_some() {
            MockResponse {
                status: 200,
                body: Bytes::from(r#"{"server":"A"}"#),
                headers: vec![],
            }
        } else {
            // Real challenge body so the proxy caches it.
            MockResponse {
                status: 402,
                body: Bytes::from(
                    r#"{"x402Version":1,"accepts":[{"scheme":"exact","network":"base","maxAmountRequired":"100000","resource":"http://A/v1","description":"A","mimeType":"application/json","payTo":"0x000000000000000000000000000000000000dEaD","asset":"0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913","extra":{"name":"USDC","version":"2"}}]}"#,
                ),
                headers: vec![],
            }
        }
    }));
    let port_b_handler = MockHandler(Arc::new(|req| {
        if req.x_payment.is_some() {
            MockResponse {
                status: 200,
                body: Bytes::from(r#"{"server":"B"}"#),
                headers: vec![],
            }
        } else {
            MockResponse {
                status: 402,
                body: Bytes::from(
                    r#"{"x402Version":1,"accepts":[{"scheme":"exact","network":"base","maxAmountRequired":"100000","resource":"http://B/v1","description":"B","mimeType":"application/json","payTo":"0x000000000000000000000000000000000000dEaD","asset":"0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913","extra":{"name":"USDC","version":"2"}}]}"#,
                ),
                headers: vec![],
            }
        }
    }));

    let (addr_a, _) = spawn_upstream(port_a_handler).await;
    let (addr_b, _) = spawn_upstream(port_b_handler).await;

    let tmp = tempfile::tempdir().unwrap();
    let policy_path = tmp.path().join("p.yaml");
    std::fs::write(&policy_path, ALLOW_ANY_POLICY).unwrap();
    let cfg = ProxyConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        sandbox: SandboxConfig {
            policy_path: Some(policy_path),
            state_path: Some(tmp.path().join("state.json")),
            audit_url: format!(
                "sqlite://{}?mode=rwc",
                tmp.path().join("audit.db").display()
            ),
        },
        ca_dir: tmp.path().join("ca"),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let rt = proxy::build_runtime(&cfg).await.unwrap();
    tokio::spawn(async move {
        let _ = proxy::accept_loop(listener, rt).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let proxy_url = format!("http://{proxy_addr}");
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(&proxy_url).unwrap())
        .build()
        .unwrap();

    // Hit A first to seed its cache.
    let _ = client
        .get(format!("http://{addr_a}/v1"))
        .send()
        .await
        .unwrap();
    // Then B (different port) — must not reuse A's challenge.
    let _ = client
        .get(format!("http://{addr_b}/v1"))
        .send()
        .await
        .unwrap();

    let pay = x_payment_for("100000");
    let resp_a = client
        .get(format!("http://{addr_a}/v1"))
        .header("X-PAYMENT", &pay)
        .send()
        .await
        .unwrap();
    let resp_b = client
        .get(format!("http://{addr_b}/v1"))
        .header("X-PAYMENT", &pay)
        .send()
        .await
        .unwrap();

    let body_a: Value = resp_a.json().await.unwrap();
    let body_b: Value = resp_b.json().await.unwrap();
    assert_eq!(body_a.get("server").and_then(|v| v.as_str()), Some("A"));
    assert_eq!(body_b.get("server").and_then(|v| v.as_str()), Some("B"));
}
