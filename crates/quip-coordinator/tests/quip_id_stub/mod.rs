//! A stand-in for id.quip.network's provider API, served over real HTTP.
//!
//! Answers are built from the fixtures in `tests/fixtures/quip-id/`, which are
//! byte-for-byte what id.quip.network serves, so the coordinator's `quip-id`
//! client parses exactly the shapes it will meet in production. Only the
//! verdict, the reasons and the expiry are changed per answer.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::Value;

const DECISION_ALLOWED: &str = include_str!("../fixtures/quip-id/decision-allowed.json");
const DECISION_DENIED: &str = include_str!("../fixtures/quip-id/decision-denied.json");
const WHOAMI: &str = include_str!("../fixtures/quip-id/whoami.json");
const UNAUTHENTICATED: &str = include_str!("../fixtures/quip-id/error-unauthenticated.json");

/// The key the stub accepts.
pub(crate) const API_KEY: &str = "qid_test_stub";

/// One request the stub received.
#[derive(Debug, Clone)]
pub(crate) struct Asked {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) authorization: Option<String>,
    pub(crate) body: Value,
}

/// How `/v1/whoami` answers.
#[derive(Debug, Clone)]
pub(crate) enum WhoamiReply {
    Ready,
    NotReady(Vec<String>),
    Status(u16),
}

#[derive(Debug)]
struct Behaviour {
    allowed: HashSet<String>,
    denied_reasons: HashMap<String, Vec<String>>,
    decisions_status: Option<u16>,
    whoami: WhoamiReply,
    decision_ttl_secs: i64,
    asked: Vec<Asked>,
}

/// A running stub. Dropping it stops answering.
pub(crate) struct IdStub {
    /// Base URL including `/v1`, as an operator would configure it.
    pub(crate) url: String,
    behaviour: Arc<Mutex<Behaviour>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for IdStub {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl IdStub {
    /// Start a stub that denies every consumer with `no_credential` until told
    /// otherwise, and reports the provider ready.
    pub(crate) async fn start() -> Self {
        let behaviour = Arc::new(Mutex::new(Behaviour {
            allowed: HashSet::new(),
            denied_reasons: HashMap::new(),
            decisions_status: None,
            whoami: WhoamiReply::Ready,
            decision_ttl_secs: 3600,
            asked: Vec::new(),
        }));
        let app = Router::new()
            .route("/v1/decisions", post(decide))
            .route("/v1/whoami", get(whoami))
            .with_state(Arc::clone(&behaviour));
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind the stub");
        let port = listener.local_addr().expect("stub address").port();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve the stub");
        });
        Self {
            url: format!("http://127.0.0.1:{port}/v1"),
            behaviour,
            server,
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut Behaviour) -> T) -> T {
        f(&mut self.behaviour.lock().expect("stub state"))
    }

    /// Answer `allowed: true` for this consumer.
    pub(crate) fn allow(&self, consumer: [u8; 32]) {
        self.with(|b| {
            let _ = b.allowed.insert(hex(&consumer));
        });
    }

    /// Deny this consumer with these reason codes.
    pub(crate) fn deny(&self, consumer: [u8; 32], reasons: &[&str]) {
        self.with(|b| {
            let _ = b.allowed.remove(&hex(&consumer));
            let _ = b.denied_reasons.insert(
                hex(&consumer),
                reasons.iter().map(ToString::to_string).collect(),
            );
        });
    }

    /// Answer every decision with this HTTP status and an error body.
    pub(crate) fn fail_decisions_with(&self, status: u16) {
        self.with(|b| b.decisions_status = Some(status));
    }

    /// How `/v1/whoami` answers.
    pub(crate) fn whoami_replies(&self, reply: WhoamiReply) {
        self.with(|b| b.whoami = reply);
    }

    /// Seconds from now until a decision's `expiresUnix`. Zero or less makes
    /// every answer stale on arrival, so the client asks again each time.
    pub(crate) fn decisions_expire_in(&self, secs: i64) {
        self.with(|b| b.decision_ttl_secs = secs);
    }

    /// Every request received so far.
    pub(crate) fn asked(&self) -> Vec<Asked> {
        self.with(|b| b.asked.clone())
    }

    /// The decision requests received so far.
    pub(crate) fn decisions_asked(&self) -> Vec<Asked> {
        self.asked()
            .into_iter()
            .filter(|a| a.path == "/v1/decisions")
            .collect()
    }
}

/// `0x` + lowercase hex, the form id.quip.network names consumers by.
pub(crate) fn hex(account: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    account.iter().fold(String::from("0x"), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn record(behaviour: &Arc<Mutex<Behaviour>>, path: &str, headers: &HeaderMap, body: &Bytes) {
    let asked = Asked {
        method: if body.is_empty() { "GET" } else { "POST" }.to_string(),
        path: path.to_string(),
        authorization: headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(ToString::to_string),
        body: serde_json::from_slice(body).unwrap_or(Value::Null),
    };
    behaviour.lock().expect("stub state").asked.push(asked);
}

fn authorised(headers: &HeaderMap) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {API_KEY}"))
}

fn json(status: u16, body: String) -> Response {
    (
        StatusCode::from_u16(status).expect("status"),
        [("content-type", "application/json")],
        body,
    )
        .into_response()
}

fn error_body(error: &str) -> String {
    serde_json::json!({ "error": error }).to_string()
}

fn now_unix() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after 1970")
            .as_secs(),
    )
    .expect("seconds fit")
}

async fn decide(
    State(behaviour): State<Arc<Mutex<Behaviour>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    record(&behaviour, "/v1/decisions", &headers, &body);
    if !authorised(&headers) {
        return json(401, UNAUTHENTICATED.to_string());
    }
    let b = behaviour.lock().expect("stub state");
    if let Some(status) = b.decisions_status {
        return json(status, error_body("stubbed failure"));
    }
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let Some(consumer) = request.get("consumer").and_then(Value::as_str) else {
        return json(400, error_body("consumer is not acceptable"));
    };
    let consumer = consumer.to_ascii_lowercase();
    let allowed = b.allowed.contains(&consumer);
    let mut decision: serde_json::Map<String, Value> = serde_json::from_str(if allowed {
        DECISION_ALLOWED
    } else {
        DECISION_DENIED
    })
    .expect("fixture is a JSON object");
    if !allowed {
        let reasons = b
            .denied_reasons
            .get(&consumer)
            .cloned()
            .unwrap_or_else(|| vec!["no_credential".to_string()]);
        let _ = decision.insert("reasons".into(), serde_json::json!(reasons));
    }
    let expires = now_unix() + b.decision_ttl_secs;
    let _ = decision.insert("expiresUnix".into(), serde_json::json!(expires.max(0)));
    json(200, Value::Object(decision).to_string())
}

async fn whoami(State(behaviour): State<Arc<Mutex<Behaviour>>>, headers: HeaderMap) -> Response {
    record(&behaviour, "/v1/whoami", &headers, &Bytes::new());
    if !authorised(&headers) {
        return json(401, UNAUTHENTICATED.to_string());
    }
    let reply = behaviour.lock().expect("stub state").whoami.clone();
    match reply {
        WhoamiReply::Ready => json(200, WHOAMI.to_string()),
        WhoamiReply::NotReady(problems) => {
            let mut who: serde_json::Map<String, Value> =
                serde_json::from_str(WHOAMI).expect("fixture is a JSON object");
            let _ = who.insert("ready".into(), Value::Bool(false));
            let _ = who.insert("problems".into(), serde_json::json!(problems));
            json(200, Value::Object(who).to_string())
        }
        WhoamiReply::Status(status) => json(status, error_body("stubbed failure")),
    }
}
