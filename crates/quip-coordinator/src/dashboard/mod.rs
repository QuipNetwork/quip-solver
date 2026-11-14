//! REST dashboard and indexer `/api/v1` surface (agm.4).
//!
//! Serves the per-qblock attempt logs the [`crate::observe::attempt`] writer appends,
//! plus the three endpoints the dashboard indexer polls:
//! `GET /api/v1/status`, `GET /api/v1/stats`, and
//! `GET /api/v1/mining/attempts?solution_number=N`.
//!
//! Static files under `data_dir` remain available via the fallback service
//! (`GET /<qblock_id>/attempts.jsonl`). The attempts JSONL is also the data
//! source for the `/api/v1/mining/attempts` envelope: `solution_number` maps
//! to the directory name under `data_dir`.

use crate::observe::metrics::CoordinatorMetrics;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::services::ServeDir;

mod time;
mod v02;

use time::{iso8601_utc, unix_ts};
use v02::{api_mining_attempts, safe_u64, wire_u128};

/// What the `/api/v1` handlers read: the attempt-log root plus the live
/// coordinator counters and identity.
#[derive(Clone)]
pub struct DashboardState {
    /// Root directory holding `<solution_number>/attempts.jsonl`.
    pub data_dir: PathBuf,
    /// Live counters, identity, and chain view.
    pub metrics: Arc<CoordinatorMetrics>,
}

/// Build the dashboard router:
/// - `GET /qblocks` → JSON array of available qblock ids (directory names)
/// - `GET /healthz` → `ok`
/// - `GET /api/v1/status` → indexer status envelope
/// - `GET /api/v1/stats` → indexer stats envelope
/// - `GET /api/v1/mining/attempts?solution_number=N` → submission + attempts
/// - everything else → static files under `data_dir`, e.g.
///   `GET /<qblock_id>/attempts.jsonl`.
pub fn router(state: DashboardState) -> Router {
    let data_dir = state.data_dir.clone();
    Router::new()
        .route("/healthz", get(healthz))
        .route("/qblocks", get(list_qblocks))
        .route("/api/v1/status", get(api_status))
        .route("/api/v1/stats", get(api_stats))
        .route("/api/v1/mining/attempts", get(api_mining_attempts))
        .fallback_service(ServeDir::new(&data_dir))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

/// List the qblock directories under the data root (sorted). Returns an empty
/// list if the root does not exist yet.
async fn list_qblocks(State(state): State<DashboardState>) -> Json<Vec<String>> {
    let data_dir = state.data_dir;
    let mut ids = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(&data_dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let is_dir = entry.file_type().await.is_ok_and(|t| t.is_dir());
            if is_dir {
                if let Some(name) = entry.file_name().to_str() {
                    ids.push(name.to_string());
                }
            }
        }
    }
    ids.sort();
    Json(ids)
}

/// Serve the dashboard on `listen` (e.g. `0.0.0.0:20100`) until the task is
/// aborted. A bind failure is logged and the task exits without taking down the
/// coordinator.
pub async fn serve(listen: String, state: DashboardState) {
    let listener = match tokio::net::TcpListener::bind(&listen).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("dashboard: bind {listen} failed: {e}");
            return;
        }
    };
    tracing::info!(
        "dashboard: serving {} at http://{listen}",
        state.data_dir.display()
    );
    if let Err(e) = axum::serve(listener, router(state)).await {
        tracing::warn!("dashboard: server error: {e}");
    }
}

// ---------------------------------------------------------------------------
// Response envelope (matches v0.2 telemetry process + indexer client)
// ---------------------------------------------------------------------------

fn success_envelope(data: &Value) -> Response {
    Json(json!({
        "success": true,
        "data": data,
        "timestamp": unix_ts(),
    }))
    .into_response()
}

fn error_envelope(status: StatusCode, message: &str, code: &str) -> Response {
    (
        status,
        Json(json!({
            "success": false,
            "error": message,
            "code": code,
            "timestamp": unix_ts(),
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/status
// ---------------------------------------------------------------------------

/// Indexer status probe: chain identity, the coordinator's own view of the chain
/// head, on-chain miner registration, the advertised miner roster, and the
/// per-backend `modes` split.
///
/// Every field is live. An unkeyed coordinator serves empty strings, an empty
/// roster, and `miner_info: null`, and still answers 200 — that is a valid state,
/// not an error.
async fn api_status(State(state): State<DashboardState>) -> Response {
    let identity = state.metrics.identity();
    let chain = state.metrics.chain();
    let modes = state
        .metrics
        .mode_views()
        .into_iter()
        .map(|(backend, view)| {
            (
                backend,
                json!({
                    "controller": counters_json(&view.counters),
                    "miners": miners_json(&view.miners),
                }),
            )
        })
        .collect::<Map<String, Value>>();

    success_envelope(&json!({
        "ss58_address": identity.ss58_address,
        "account_id_hex": identity.account_id_hex,
        "node_id": identity.node_id,
        "is_mining": chain.is_mining,
        "last_successful_submission": state
            .metrics
            .last_successful_submission()
            .map_or(Value::Null, |t| Value::String(iso8601_utc(t))),
        "uptime_seconds": safe_u64("uptime_seconds", 0, state.metrics.uptime_seconds()),
        "chain": {
            "head_hash": chain.head_hash,
            "head_number": safe_u64("head_number", 0, chain.head_number),
        },
        "miner_registered": chain.miner_registered,
        "miner_info": chain.miner_info.map_or(Value::Null, |i| miner_info_json(&i)),
        "miners": miners_json(&state.metrics.miners()),
        "modes": Value::Object(modes),
    }))
}

/// Serialize a miner roster for the `miners` array.
fn miners_json(miners: &[crate::observe::metrics::MinerEntry]) -> Value {
    Value::Array(
        miners
            .iter()
            .map(|m| json!({ "id": m.id, "type": m.miner_type }))
            .collect(),
    )
}

/// Serialize `QuantumPow.Miners[account]`.
///
/// This object is intentionally mixed-type: `deposit` and `rewards_earned` are
/// real `u128` balances, so rule N1 puts them on the wire as decimal strings.
/// `registered_at` (a substrate block height) and `proofs_submitted` /
/// `proofs_won` (`u64` lifetime counts) can never approach 2^53, so rule N1's
/// "large by design" clause does not apply to them; they stay guarded numbers
/// and agree with `controller.proofs_submitted` in [`counters_json`], which
/// serializes the same field name as a number.
fn miner_info_json(info: &crate::types::miner::MinerInfo) -> Value {
    json!({
        "registered_at": safe_u64("registered_at", 0, info.registered_at),
        "deposit": wire_u128(info.deposit),
        "proofs_submitted": safe_u64("proofs_submitted", 0, info.proofs_submitted),
        "proofs_won": safe_u64("proofs_won", 0, info.proofs_won),
        "rewards_earned": wire_u128(info.rewards_earned),
    })
}

// ---------------------------------------------------------------------------
// GET /api/v1/stats
// ---------------------------------------------------------------------------

/// Indexer stats probe. Reports the process-global controller counters, which
/// are authoritative; the per-mode split `/api/v1/status` carries may lag them
/// momentarily under concurrent updates (see [`crate::observe::metrics::CoordinatorMetrics::global`]).
async fn api_stats(State(state): State<DashboardState>) -> Response {
    success_envelope(&json!({
        "controller": counters_json(&state.metrics.global()),
    }))
}

/// Serialize one counter set. Every counter passes the safe-integer guard, so a
/// runaway counter cannot stall the indexer the way the energy sentinel did.
fn counters_json(c: &crate::observe::metrics::CounterSnapshot) -> Value {
    json!({
        "heads_observed": safe_u64("heads_observed", 0, c.heads_observed),
        "contexts_dispatched": safe_u64("contexts_dispatched", 0, c.contexts_dispatched),
        "results_received": safe_u64("results_received", 0, c.results_received),
        "proofs_submitted": safe_u64("proofs_submitted", 0, c.proofs_submitted),
        "stale_drops": safe_u64("stale_drops", 0, c.stale_drops),
        "submission_errors": safe_u64("submission_errors", 0, c.submission_errors),
        "duplicate_result_drops": safe_u64("duplicate_result_drops", 0, c.duplicate_result_drops),
    })
}

// ---------------------------------------------------------------------------
// GET /api/v1/mining/attempts?solution_number=N
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::v02::{safe_i64, JS_MAX_SAFE_INTEGER, JS_MIN_SAFE_INTEGER};
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::path::Path;
    use std::time::UNIX_EPOCH;
    use tower::ServiceExt; // oneshot

    fn attempt_line(
        miner_id: &str,
        best_energy_milli: i64,
        diversity_milli: u32,
        n_valid: u32,
        accepted: bool,
        submitted: bool,
        device_access_time_us: u64,
    ) -> String {
        serde_json::json!({
            "ts_ms": 1_700_000_000_000_u64,
            "qblock_id": 42,
            "generation": 1,
            "miner_id": miner_id,
            "job_id": "ab",
            "is_pow": true,
            "order_id": "",
            "best_energy_milli": best_energy_milli,
            "diversity_milli": diversity_milli,
            "n_valid": n_valid,
            "accepted": accepted,
            "submitted": submitted,
            "device_access_time_us": device_access_time_us,
        })
        .to_string()
    }

    async fn body_json(resp: Response) -> Value {
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn at<'a>(v: &'a Value, path: &str) -> &'a Value {
        v.pointer(path).unwrap_or(&Value::Null)
    }

    /// One GET against `app`, whatever the status.
    async fn fetch(app: Router, uri: &str) -> Response {
        app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn sentinel_line(raw_best_energy_milli: i64) -> String {
        serde_json::json!({
            "ts_ms": 1_700_000_000_000_u64,
            "qblock_id": 42,
            "generation": 1,
            "miner_id": "cpu-0",
            "job_id": "ab",
            "is_pow": true,
            "order_id": "",
            "best_energy_milli": i64::MAX,
            "raw_best_energy_milli": raw_best_energy_milli,
            "diversity_milli": 0,
            "n_valid": 0,
            "accepted": false,
            "submitted": false,
            "device_access_time_us": 0,
        })
        .to_string()
    }

    fn legacy_sentinel_line() -> String {
        serde_json::json!({
            "ts_ms": 1_700_000_000_000_u64,
            "qblock_id": 42,
            "generation": 1,
            "miner_id": "cpu-0",
            "job_id": "ab",
            "is_pow": true,
            "order_id": "",
            "best_energy_milli": i64::MAX,
            "diversity_milli": 0,
            "n_valid": 0,
            "accepted": false,
            "submitted": false,
            "device_access_time_us": 0,
        })
        .to_string()
    }

    /// Walk every number in a response body and assert rule N1 holds.
    ///
    /// `path` accumulates a JSON-pointer-like trail so a failure names the exact
    /// field, not just the value.
    fn assert_safe_integers(v: &Value, path: &str) {
        match v {
            Value::Number(n) => {
                let as_i128 = n
                    .as_i64()
                    .map(i128::from)
                    .or_else(|| n.as_u64().map(i128::from));
                let Some(x) = as_i128 else {
                    panic!("{path}: {n} is not an integer; the wire shape has no floats");
                };
                assert!(
                    x >= i128::from(JS_MIN_SAFE_INTEGER) && x <= i128::from(JS_MAX_SAFE_INTEGER),
                    "{path}: {x} is outside the IEEE-754 safe integer range"
                );
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    assert_safe_integers(item, &format!("{path}/{i}"));
                }
            }
            Value::Object(map) => {
                for (k, item) in map {
                    assert_safe_integers(item, &format!("{path}/{k}"));
                }
            }
            Value::String(_) | Value::Bool(_) | Value::Null => {}
        }
    }

    /// The attempt-file shapes the sweep and golden tests run against.
    enum Fixture {
        /// Rows that cleared the gate. The ordinary case.
        Normal,
        /// Every row carries the `i64::MAX` no-solution sentinel.
        AllSentinel,
        /// A row reporting `u64::MAX` device access time. The next sentinel.
        HugeDeviceTime,
        /// A submitted, winning row with every D4 chain field populated — the
        /// shape [`submitted_line`] builds for `c8`.
        Submitted,
    }

    /// Write `rows` as the attempt trail of `solution_number`.
    fn write_rows(tmp: &Path, solution_number: u64, rows: &[String]) {
        let dir = tmp.join(solution_number.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("attempts.jsonl"), rows.join("\n") + "\n").unwrap();
    }

    fn write_fixture(tmp: &Path, solution_number: u64, fixture: &Fixture) {
        let rows = match fixture {
            Fixture::Normal => vec![
                attempt_line("cpu-0", -14_000, 200, 3, true, false, 0),
                attempt_line("cpu-0", -14_200, 250, 6, true, true, 12_000),
            ],
            Fixture::AllSentinel => vec![sentinel_line(-500), legacy_sentinel_line()],
            Fixture::HugeDeviceTime => {
                vec![attempt_line("cpu-0", -14_200, 250, 6, true, true, u64::MAX)]
            }
            Fixture::Submitted => vec![submitted_line()],
        };
        write_rows(tmp, solution_number, &rows);
    }

    async fn get_body(tmp: &Path, uri: &str) -> Value {
        let resp = fetch(router(test_state(tmp)), uri).await;
        assert_eq!(resp.status(), StatusCode::OK, "{uri} did not return 200");
        body_json(resp).await
    }

    /// QUI-1391: a response body too large for [`body_json`]'s 1 MB cap.
    async fn get_body_unbounded(tmp: &Path, uri: &str) -> Value {
        let resp = fetch(router(test_state(tmp)), uri).await;
        assert_eq!(resp.status(), StatusCode::OK, "{uri} did not return 200");
        let bytes = axum::body::to_bytes(resp.into_body(), 64 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Write a trail of `n` rows whose `ts_ms` increases with position, so the
    /// newest rows are unambiguous. Every 1,000th row is submitted.
    fn write_long_trail(tmp: &Path, solution_number: u64, n: u64) {
        let dir = tmp.join(solution_number.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let mut body = String::new();
        for i in 0..n {
            let mut rec: Value =
                serde_json::from_str(&attempt_line("cpu-0", -14_000, 200, 3, true, false, 7))
                    .unwrap();
            let map = rec.as_object_mut().unwrap();
            let _ = map.insert("ts_ms".into(), json!(1_700_000_000_000_u64 + i));
            // One deeper energy in the middle, to prove the totals fold reads
            // the whole trail and not just the returned tail.
            if i == n / 2 {
                let _ = map.insert("best_energy_milli".into(), json!(-99_000));
            }
            if i % 1_000 == 999 {
                let _ = map.insert("submitted".into(), json!(true));
                let _ = map.insert("n_valid".into(), json!(41));
            }
            body.push_str(&rec.to_string());
            body.push('\n');
        }
        std::fs::write(dir.join("attempts.jsonl"), body).unwrap();
    }

    /// QUI-1391: `limit` returns the newest N rows and totals for the whole
    /// trail. 10,001 rows is past the ~9,000 a two-GPU node records. A request
    /// without `limit` keeps the shape it had before, so clients that predate
    /// the parameter are untouched.
    #[tokio::test]
    async fn attempts_limit_returns_the_newest_rows_with_whole_trail_totals() {
        let tmp = tempfile::tempdir().unwrap();
        write_long_trail(tmp.path(), 1, 10_001);

        let v = get_body_unbounded(
            tmp.path(),
            "/api/v1/mining/attempts?solution_number=1&limit=500",
        )
        .await;
        let attempts = at(&v, "/data/attempts").as_array().unwrap();
        assert_eq!(attempts.len(), 500);

        // The newest 500 are positions 9502..=10001, so `iter` is unchanged by
        // truncation and still numbers rows within the whole trail.
        assert_eq!(at(attempts.first().unwrap(), "/iter"), &json!(9_502));
        assert_eq!(at(attempts.last().unwrap(), "/iter"), &json!(10_001));

        assert_eq!(at(&v, "/data/totals/attempt_count"), &json!(10_001));
        assert_eq!(at(&v, "/data/totals/best_energy_milli"), &json!(-99_000));
        assert_eq!(at(&v, "/data/totals/qpu_access_time_us"), &json!(70_007));
        assert_eq!(at(&v, "/data/totals/num_valid"), &json!(41));
        assert_safe_integers(&v, "attempts?limit=500");

        let v = get_body_unbounded(tmp.path(), "/api/v1/mining/attempts?solution_number=1").await;
        assert_eq!(at(&v, "/data/attempts").as_array().unwrap().len(), 10_001);
        assert!(
            at(&v, "/data").get("totals").is_none(),
            "totals must not appear without limit"
        );
    }

    /// C3: no response from any endpoint carries an unsafe integer, whatever
    /// the attempt file holds. The ordinary, sentinel and huge-device-time
    /// fixtures are swept by the golden test; this one sits above
    /// `JS_MAX_SAFE_INTEGER` and exercises the `solution_number` guard itself,
    /// not just the fields copied from the attempt record.
    #[tokio::test]
    async fn c3_every_response_number_is_a_safe_integer() {
        let n = 9_007_199_254_740_992_u64;
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(tmp.path(), n, &Fixture::Normal);
        for uri in [
            "/api/v1/status".to_string(),
            "/api/v1/stats".to_string(),
            format!("/api/v1/mining/attempts?solution_number={n}"),
        ] {
            let body = get_body(tmp.path(), &uri).await;
            assert_safe_integers(&body, &uri);
        }
    }

    async fn attempts_body(tmp: &Path, solution_number: u64) -> Value {
        let uri = format!("/api/v1/mining/attempts?solution_number={solution_number}");
        get_body(tmp, &uri).await
    }

    /// Sentinel handling across the attempt trail, one directory per shape.
    #[tokio::test]
    async fn attempt_energy_sentinels_never_reach_the_response() {
        let tmp = tempfile::tempdir().unwrap();

        // QUI-1391: a trail where no row ever cleared the gate reports 0 rather
        // than the `i64::MAX` sentinel.
        write_fixture(tmp.path(), 1, &Fixture::AllSentinel);
        let v = get_body(
            tmp.path(),
            "/api/v1/mining/attempts?solution_number=1&limit=10",
        )
        .await;
        assert_eq!(at(&v, "/data/totals/best_energy_milli"), &json!(-500));
        assert_eq!(at(&v, "/data/totals/num_valid"), &Value::Null);

        // C1: every row holds the no-solution sentinel; the raw best is served.
        write_rows(tmp.path(), 42, &[sentinel_line(-500), sentinel_line(-500)]);
        let v = attempts_body(tmp.path(), 42).await;
        assert_eq!(at(&v, "/data/submission/energy_milli"), &json!(-500));
        assert_eq!(at(&v, "/data/attempts/0/best_energy_milli"), &json!(-500));

        // C2: the legacy on-disk shape has no raw field. It must read as 0, not
        // as the sentinel, and the request must still return 200.
        write_rows(
            tmp.path(),
            44,
            &[legacy_sentinel_line(), legacy_sentinel_line()],
        );
        let v = attempts_body(tmp.path(), 44).await;
        assert_eq!(at(&v, "/data/submission/energy_milli"), &json!(0));
        assert_eq!(at(&v, "/data/attempts/0/best_energy_milli"), &json!(0));

        // The raw field is a fallback, not an override: a row that cleared the
        // gate still serves its gate-passing best.
        write_rows(
            tmp.path(),
            9,
            &[attempt_line("cpu-0", -14_200, 250, 6, true, true, 0)],
        );
        let v = attempts_body(tmp.path(), 9).await;
        assert_eq!(at(&v, "/data/submission/energy_milli"), &json!(-14_200));
        assert_eq!(
            at(&v, "/data/attempts/0/best_energy_milli"),
            &json!(-14_200)
        );

        // A rejected attempt has no chain outcome. The four nullable fields
        // stay null and the two non-nullable ones keep their types, or the
        // dashboard parser throws.
        write_rows(tmp.path(), 43, &[sentinel_line(-500)]);
        let v = attempts_body(tmp.path(), 43).await;
        let s = at(&v, "/data/submission");
        assert!(at(s, "/extrinsic_hash").is_null());
        assert!(at(s, "/chain_block_hash").is_null());
        assert!(at(s, "/chain_block_number").is_null());
        assert!(at(s, "/pow_sequence").is_null());
        assert!(at(s, "/threshold_milli").is_number());
        assert!(at(s, "/miner_type").is_string());
    }

    fn submitted_line() -> String {
        serde_json::json!({
            "ts_ms": 1_700_000_000_000_u64,
            "qblock_id": 42,
            "generation": 1,
            "miner_id": "cpu-0",
            "miner_type": "CPU",
            "job_id": "ab",
            "is_pow": true,
            "order_id": "",
            "best_energy_milli": -14_200,
            "raw_best_energy_milli": -14_200,
            "diversity_milli": 250,
            "n_valid": 6,
            "accepted": true,
            "submitted": true,
            "device_access_time_us": 12_000,
            "threshold_milli": -2_500_000,
            "last_proof_block_hash": LAST_PROOF_BLOCK_HASH,
            "extrinsic_hash": EXTRINSIC_HASH,
            "chain_block_hash": CHAIN_BLOCK_HASH,
            "chain_block_number": 10_249_u64,
            "pow_sequence": 412_u64,
        })
        .to_string()
    }

    // Three distinct 32-byte hashes: a same-shaped string can't hide a field
    // swap (e.g. extrinsic_hash and chain_block_hash exchanged) or a doubled
    // `0x` prefix landing on the wrong field.
    const LAST_PROOF_BLOCK_HASH: &str =
        "0x1111111111111111111111111111111111111111111111111111111111111111";
    const EXTRINSIC_HASH: &str =
        "0x2222222222222222222222222222222222222222222222222222222222222222";
    const CHAIN_BLOCK_HASH: &str =
        "0x3333333333333333333333333333333333333333333333333333333333333333";

    /// Whether `s` matches `^0x[0-9a-f]{64}$`: one `0x` prefix, exactly 64
    /// lowercase hex digits. A doubled prefix (`0x0x...`) or a truncated hash
    /// fails this even when the total length coincidentally matches.
    fn is_hex_hash(s: &str) -> bool {
        s.strip_prefix("0x").is_some_and(|h| {
            h.len() == 64
                && h.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    }

    /// C8: a submitted, winning result reports every field the dashboard shows.
    #[tokio::test]
    async fn c8_submission_reports_the_real_chain_fields() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(tmp.path(), 42, &Fixture::Submitted);

        let v = attempts_body(tmp.path(), 42).await;
        let s = at(&v, "/data/submission");

        assert_eq!(at(s, "/threshold_milli"), &json!(-2_500_000));
        // Three distinct fields, checked by shape and by exact value: a
        // same-shaped 66-char string can't hide a field swap between
        // extrinsic_hash and chain_block_hash, and the shape check catches a
        // doubled `0x` prefix even where a swap would not.
        for (field, expected) in [
            ("/last_proof_block_hash", LAST_PROOF_BLOCK_HASH),
            ("/extrinsic_hash", EXTRINSIC_HASH),
            ("/chain_block_hash", CHAIN_BLOCK_HASH),
        ] {
            let hash = at(s, field).as_str().unwrap();
            assert!(
                is_hex_hash(hash),
                "{field}: {hash:?} does not match ^0x[0-9a-f]{{64}}$"
            );
            assert_eq!(hash, expected, "{field}: value does not round-trip exactly");
        }
        assert!(at(s, "/miner_type").as_str().unwrap().starts_with("CPU"));
        // Block heights are `u64` and never approach 2^53, so rule N1's
        // "large by design" clause does not apply; they stay guarded numbers.
        assert_eq!(at(s, "/chain_block_number"), &json!(10_249));
        assert_eq!(at(s, "/pow_sequence"), &json!(412));
        assert_eq!(at(&v, "/data/attempts/0/miner_type"), &json!("CPU"));
    }

    #[tokio::test]
    async fn serves_attempts_file_and_lists_qblocks() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("42")).unwrap();
        std::fs::write(
            tmp.path().join("42").join("attempts.jsonl"),
            "{\"job_id\":\"ab\"}\n",
        )
        .unwrap();

        let app = router(test_state(tmp.path()));

        // File download.
        let resp = fetch(app.clone(), "/42/attempts.jsonl").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("\"job_id\":\"ab\""));

        // Index lists the qblock dir.
        let resp = fetch(app, "/qblocks").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let ids: Vec<String> = serde_json::from_slice(&body).unwrap();
        assert_eq!(ids, vec!["42".to_string()]);
    }

    fn keyed_state(tmp: &Path) -> DashboardState {
        let metrics = Arc::new(CoordinatorMetrics::new(&[(
            "cpu-0".to_string(),
            "cpu".to_string(),
        )]));
        metrics.set_identity(crate::observe::metrics::Identity {
            ss58_address: "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY".into(),
            account_id_hex: "0xd43593c715fdd31c61141abd04a99fd6822c8558854ccde39a5684e7a56da27d"
                .into(),
            node_id: "quip-miner-pow-CPU-1".into(),
        });
        metrics.set_chain(crate::observe::metrics::ChainView {
            head_hash: "0x00ff".into(),
            head_number: 10_249,
            is_mining: true,
            miner_registered: true,
            miner_info: Some(crate::types::miner::MinerInfo {
                registered_at: 8_100,
                deposit: 1_000_000_000_000,
                proofs_submitted: 412,
                proofs_won: 7,
                rewards_earned: 70_000_000_000_000,
            }),
        });
        DashboardState {
            data_dir: tmp.to_path_buf(),
            metrics,
        }
    }

    async fn status_body(state: DashboardState) -> Value {
        let resp = fetch(router(state), "/api/v1/status").await;
        assert_eq!(resp.status(), StatusCode::OK);
        body_json(resp).await
    }

    #[test]
    fn iso8601_utc_formats_known_instants() {
        use std::time::Duration;
        assert_eq!(iso8601_utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        // 2000-02-29 exercises the leap-century rule.
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400 + 3_661);
        assert_eq!(iso8601_utc(leap), "2000-02-29T01:01:01Z");
        let t = UNIX_EPOCH + Duration::from_secs(1_786_742_808);
        assert_eq!(iso8601_utc(t), "2026-08-14T21:26:48Z");
    }

    /// QUI-830: the field is null until the chain accepts a proof, and an
    /// error or stale drop does not set it.
    #[tokio::test]
    async fn status_last_successful_submission_tracks_accepted_proofs_only() {
        let tmp = tempfile::tempdir().unwrap();
        let state = keyed_state(tmp.path());
        let v = status_body(state.clone()).await;
        assert_eq!(at(&v, "/data/last_successful_submission"), &Value::Null);

        state.metrics.record_submission_error("cpu-0");
        state.metrics.record_stale_drop("cpu-0");
        let v = status_body(state.clone()).await;
        assert_eq!(at(&v, "/data/last_successful_submission"), &Value::Null);

        state.metrics.record_proof_submitted("cpu-0");
        let v = status_body(state).await;
        let ts = at(&v, "/data/last_successful_submission").as_str().unwrap();
        assert_eq!(ts.len(), 20, "{ts}");
        assert!(ts.ends_with('Z'), "{ts}");
    }

    /// C5: a keyed coordinator with one registered CPU miner.
    #[tokio::test]
    async fn c5_status_reports_the_configured_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let v = status_body(keyed_state(tmp.path())).await;

        assert_eq!(at(&v, "/success"), &json!(true));
        assert!(at(&v, "/data").is_object());
        assert!(at(&v, "/data/modes").is_object());
        assert!(at(&v, "/timestamp").is_number());
        assert_eq!(
            at(&v, "/data/ss58_address"),
            &json!("5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY")
        );
        assert_eq!(at(&v, "/data/node_id"), &json!("quip-miner-pow-CPU-1"));
        assert_eq!(at(&v, "/data/is_mining"), &json!(true));
        assert_eq!(at(&v, "/data/miner_registered"), &json!(true));

        let miners = at(&v, "/data/miners").as_array().unwrap();
        assert_eq!(miners.len(), 1);
        #[expect(
            clippy::indexing_slicing,
            reason = "asserted len() == 1 above; index 0 exists"
        )]
        {
            assert_eq!(at(&miners[0], "/id"), &json!("cpu-0"));
            assert!(at(&miners[0], "/type").as_str().unwrap().starts_with("CPU"));
        }

        assert!(at(&v, "/data/chain").is_object());
        assert!(at(&v, "/data/chain/head_number").as_u64().unwrap() > 0);
        assert_eq!(at(&v, "/data/chain/head_hash"), &json!("0x00ff"));

        assert!(at(&v, "/data/miner_info").is_object());
        // u128 balances cross the wire as decimal strings, per rule N1.
        for key in ["deposit", "rewards_earned"] {
            assert!(
                at(&v, &format!("/data/miner_info/{key}")).is_string(),
                "miner_info.{key} must be a decimal string"
            );
        }
        // u64 lifetime counts and the block height stay guarded numbers; they
        // agree with `controller.proofs_submitted` (a number) in stats_body.
        for key in ["registered_at", "proofs_submitted", "proofs_won"] {
            assert!(
                at(&v, &format!("/data/miner_info/{key}")).is_number(),
                "miner_info.{key} must be a number"
            );
        }
        assert_eq!(at(&v, "/data/miner_info/deposit"), &json!("1000000000000"));
        assert_eq!(at(&v, "/data/miner_info/proofs_submitted"), &json!(412));
        assert_eq!(at(&v, "/data/miner_info/registered_at"), &json!(8_100));
    }

    /// C6: an unkeyed coordinator is a valid state, not an error.
    #[tokio::test]
    async fn c6_status_with_no_identity_returns_200_and_empty_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let state = DashboardState {
            data_dir: tmp.path().to_path_buf(),
            metrics: Arc::new(CoordinatorMetrics::new(&[])),
        };
        let v = status_body(state).await;

        assert_eq!(at(&v, "/success"), &json!(true));
        assert_eq!(at(&v, "/data/ss58_address"), &json!(""));
        assert_eq!(at(&v, "/data/miners"), &json!([]));
        assert_eq!(at(&v, "/data/miner_info"), &Value::Null);
        assert_eq!(at(&v, "/data/miner_registered"), &json!(false));
        assert_eq!(at(&v, "/data/modes"), &json!({}));
    }

    /// The per-backend panel needs one `modes` entry per active backend group,
    /// each with its own counters and roster.
    #[tokio::test]
    async fn status_modes_carry_one_entry_per_backend_group() {
        let tmp = tempfile::tempdir().unwrap();
        let metrics = Arc::new(CoordinatorMetrics::new(&[
            ("cpu-0".to_string(), "cpu".to_string()),
            ("cuda-0".to_string(), "cuda".to_string()),
        ]));
        metrics.record_contexts_dispatched("cuda-0", 3);
        let state = DashboardState {
            data_dir: tmp.path().to_path_buf(),
            metrics,
        };
        let v = status_body(state).await;

        assert_eq!(
            at(&v, "/data/modes/cpu/controller/contexts_dispatched"),
            &json!(0)
        );
        assert_eq!(
            at(&v, "/data/modes/cuda/controller/contexts_dispatched"),
            &json!(3)
        );
        assert_eq!(at(&v, "/data/modes/cuda/miners/0/id"), &json!("cuda-0"));
        assert_eq!(at(&v, "/data/modes/cuda/miners/0/type"), &json!("GPU-CUDA"));
    }

    fn test_state(tmp: &Path) -> DashboardState {
        DashboardState {
            data_dir: tmp.to_path_buf(),
            metrics: Arc::new(CoordinatorMetrics::new(&[(
                "cpu-0".to_string(),
                "cpu".to_string(),
            )])),
        }
    }

    async fn stats_body(state: DashboardState) -> Value {
        let resp = fetch(router(state), "/api/v1/stats").await;
        assert_eq!(resp.status(), StatusCode::OK);
        body_json(resp).await
    }

    /// C7: dispatch one context and receive one result. Both counters advance
    /// by exactly one and no counter goes backwards. All seven keys must be
    /// present as numbers, so the dashboard parser never falls back to its
    /// `?? 0` default and hides a missing counter.
    #[tokio::test]
    async fn c7_stats_counters_advance() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let metrics = Arc::clone(&state.metrics);

        let before = stats_body(state.clone()).await;
        assert_eq!(at(&before, "/success"), &json!(true));
        metrics.record_contexts_dispatched("cpu-0", 1);
        metrics.record_result_received("cpu-0");
        let after = stats_body(state).await;

        assert_eq!(
            at(&before, "/data/controller/contexts_dispatched"),
            &json!(0)
        );
        assert_eq!(at(&before, "/data/controller/results_received"), &json!(0));
        assert_eq!(
            at(&after, "/data/controller/contexts_dispatched"),
            &json!(1)
        );
        assert_eq!(at(&after, "/data/controller/results_received"), &json!(1));

        for key in [
            "heads_observed",
            "contexts_dispatched",
            "results_received",
            "proofs_submitted",
            "stale_drops",
            "submission_errors",
            "duplicate_result_drops",
        ] {
            let b = at(&before, &format!("/data/controller/{key}"))
                .as_u64()
                .unwrap_or_else(|| panic!("missing controller.{key}"));
            let a = at(&after, &format!("/data/controller/{key}"))
                .as_u64()
                .unwrap_or_else(|| panic!("missing controller.{key}"));
            assert!(a >= b, "{key} decreased from {b} to {a}");
        }
    }

    /// The attempts endpoint: row projection on the ordinary fixture, then the
    /// `limit` range edges, then the status-code table. C9: the status code
    /// and the `code` field agree, for every row of the table. The dashboard
    /// maps 404 to a skip that still advances the checkpoint, so turning a 404
    /// into a 200 would stall the walk.
    #[tokio::test]
    async fn mining_attempts_serves_rows_and_documented_error_codes() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(tmp.path(), 7, &Fixture::Normal);
        let dir = tmp.path().join("8");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("attempts.jsonl"), "\n\n").unwrap();

        let v = attempts_body(tmp.path(), 7).await;
        assert_eq!(at(&v, "/success"), &json!(true));
        assert_eq!(at(&v, "/data/submission/solution_number"), &json!(7));
        assert_eq!(at(&v, "/data/submission/miner_id"), &json!("cpu-0"));
        assert_eq!(at(&v, "/data/submission/energy_milli"), &json!(-14_200));
        assert_eq!(at(&v, "/data/submission/diversity_milli"), &json!(250));
        assert_eq!(at(&v, "/data/submission/outcome"), &json!("submitted"));
        assert_eq!(at(&v, "/data/submission/num_valid"), &json!(6));
        let attempts = at(&v, "/data/attempts").as_array().unwrap();
        assert_eq!(attempts.len(), 2);
        let a0 = attempts.first().unwrap();
        let a1 = attempts.get(1).unwrap();
        assert_eq!(at(a0, "/iter"), &json!(1));
        assert_eq!(at(a0, "/result_kind"), &json!("stored"));
        assert_eq!(at(a1, "/iter"), &json!(2));
        assert_eq!(at(a1, "/result_kind"), &json!("submitted"));
        assert_eq!(at(a1, "/qpu_access_time_us"), &json!(12_000));

        // QUI-1391: a limit past the trail length returns the whole trail, and
        // `limit=0` is a totals-only probe rather than an error.
        let v = get_body(
            tmp.path(),
            "/api/v1/mining/attempts?solution_number=7&limit=999",
        )
        .await;
        assert_eq!(at(&v, "/data/attempts").as_array().unwrap().len(), 2);
        assert_eq!(at(&v, "/data/totals/attempt_count"), &json!(2));
        let v = get_body(
            tmp.path(),
            "/api/v1/mining/attempts?solution_number=7&limit=0",
        )
        .await;
        assert_eq!(at(&v, "/data/attempts").as_array().unwrap().len(), 0);
        assert_eq!(at(&v, "/data/totals/attempt_count"), &json!(2));

        for (label, query, expected_status, expected_code) in [
            ("found", "?solution_number=7", StatusCode::OK, ""),
            (
                "missing parameter",
                "",
                StatusCode::BAD_REQUEST,
                "BAD_PARAM",
            ),
            (
                "unparsable parameter",
                "?solution_number=not-a-number",
                StatusCode::BAD_REQUEST,
                "BAD_PARAM",
            ),
            // QUI-1391: a non-numeric limit is a 400, not a silent full trail.
            (
                "non-numeric limit",
                "?solution_number=7&limit=abc",
                StatusCode::BAD_REQUEST,
                "BAD_PARAM",
            ),
            (
                "no directory for that number",
                "?solution_number=999",
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
            ),
            // An empty attempt file is a 404, not a 200 with an empty body.
            // The indexer treats a 404 as a skip; a 200 with no submission
            // makes the parser throw, which is a hard error that stalls the
            // checkpoint.
            (
                "empty attempt file",
                "?solution_number=8",
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
            ),
        ] {
            let resp = fetch(
                router(test_state(tmp.path())),
                &format!("/api/v1/mining/attempts{query}"),
            )
            .await;
            assert_eq!(resp.status(), expected_status, "{label}");
            let v = body_json(resp).await;
            assert_eq!(
                at(&v, "/code").as_str().unwrap_or_default(),
                expected_code,
                "{label}"
            );
            assert_eq!(
                at(&v, "/success"),
                &json!(expected_status == StatusCode::OK),
                "{label}"
            );
        }
    }

    #[test]
    fn safe_i64_passes_in_range_values_and_clamps_the_rest_to_zero() {
        for (value, expected) in [
            (-14_200, -14_200),
            (0, 0),
            (JS_MAX_SAFE_INTEGER, JS_MAX_SAFE_INTEGER),
            (JS_MIN_SAFE_INTEGER, JS_MIN_SAFE_INTEGER),
            (i64::MAX, 0),
            (i64::MIN, 0),
            (JS_MAX_SAFE_INTEGER + 1, 0),
            (JS_MIN_SAFE_INTEGER - 1, 0),
        ] {
            assert_eq!(
                safe_i64("best_energy_milli", 42, value),
                json!(expected),
                "{value}"
            );
        }
    }

    #[test]
    fn safe_u64_clamps_above_the_safe_range_only() {
        assert_eq!(safe_u64("qpu_access_time_us", 42, 12_000), json!(12_000));
        assert_eq!(safe_u64("qpu_access_time_us", 42, 0), json!(0));
        assert_eq!(safe_u64("qpu_access_time_us", 42, u64::MAX), json!(0));
    }

    /// Fields that are large by design serialize as decimal strings, so no
    /// clamp applies and no precision is lost.
    #[test]
    fn wire_u128_is_a_decimal_string() {
        assert_eq!(wire_u128(0), json!("0"));
        assert_eq!(wire_u128(70_000_000_000_000), json!("70000000000000"));
        assert_eq!(
            wire_u128(u128::MAX),
            json!("340282366920938463463374607431768211455")
        );
    }

    /// Replace the two fields that change on every run, so the golden file is
    /// byte-stable. Everything else in the body is a pure projection of the
    /// fixture.
    fn normalize_for_golden(v: &mut Value) {
        if let Some(map) = v.as_object_mut() {
            if map.contains_key("timestamp") {
                let _ = map.insert("timestamp".into(), json!(1_786_742_808_u64));
            }
            if let Some(data) = map.get_mut("data").and_then(Value::as_object_mut) {
                if data.contains_key("uptime_seconds") {
                    let _ = data.insert("uptime_seconds".into(), json!(3_612_u64));
                }
            }
        }
    }

    /// C4: the committed response bodies the dashboard repo tests against.
    ///
    /// Set `UPDATE_DASHBOARD_GOLDEN=1` to rewrite the file after an intentional
    /// wire change, then commit it and tell the dashboard team.
    #[tokio::test]
    async fn c4_dashboard_rest_golden_matches_the_committed_file() {
        let mut golden = Map::new();
        for (n, name, fixture) in [
            (1_u64, "normal", Fixture::Normal),
            (2, "all_sentinel", Fixture::AllSentinel),
            (3, "huge_device_time", Fixture::HugeDeviceTime),
            (4, "submitted", Fixture::Submitted),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            write_fixture(tmp.path(), n, &fixture);
            let state = keyed_state(tmp.path());
            for (endpoint, uri) in [
                ("status", "/api/v1/status".to_string()),
                ("stats", "/api/v1/stats".to_string()),
                (
                    "mining_attempts",
                    format!("/api/v1/mining/attempts?solution_number={n}"),
                ),
            ] {
                let resp = fetch(router(state.clone()), &uri).await;
                assert_eq!(resp.status(), StatusCode::OK, "{uri}");
                let mut body = body_json(resp).await;
                normalize_for_golden(&mut body);
                assert_safe_integers(&body, &format!("{name}/{endpoint}"));
                let _ = golden.insert(format!("{name}/{endpoint}"), body);
            }
        }

        let rendered = format!("{}\n", serde_json::to_string_pretty(&golden).unwrap());
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/dashboard_rest_golden.json");

        if std::env::var("UPDATE_DASHBOARD_GOLDEN").is_ok() {
            std::fs::write(&path, &rendered).unwrap();
            return;
        }

        let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "cannot read {}: {e}. Run with UPDATE_DASHBOARD_GOLDEN=1 to create it.",
                path.display()
            )
        });
        assert_eq!(
            committed, rendered,
            "the dashboard REST wire shape changed. Re-run with \
             UPDATE_DASHBOARD_GOLDEN=1, commit \
             crates/quip-coordinator/fixtures/dashboard_rest_golden.json, \
             and tell the dashboard team before merging."
        );
    }
}
