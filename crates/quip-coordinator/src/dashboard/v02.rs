//! `GET /api/v1/mining/attempts`: the v0.2 record mapping from the attempt
//! JSONL to the indexer envelope.

use super::{error_envelope, success_envelope, DashboardState};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::path::Path;

#[derive(Debug, Deserialize)]
pub(super) struct AttemptsQuery {
    /// Global solution number; maps to `data_dir/<solution_number>/`.
    solution_number: Option<String>,
    /// Return only the newest N attempts by `ts_ns`, and add the `totals`
    /// object for the whole trail. Absent means the whole trail and no
    /// `totals`, which is what clients written before QUI-1391 expect.
    limit: Option<String>,
}

pub(super) async fn api_mining_attempts(
    State(state): State<DashboardState>,
    Query(query): Query<AttemptsQuery>,
) -> Response {
    let Some(raw) = query.solution_number.as_deref() else {
        return error_envelope(
            StatusCode::BAD_REQUEST,
            "supply ?solution_number=N",
            "BAD_PARAM",
        );
    };
    let Ok(solution_number) = raw.parse::<u64>() else {
        return error_envelope(
            StatusCode::BAD_REQUEST,
            "solution_number must be an integer",
            "BAD_PARAM",
        );
    };
    let limit = match query.limit.as_deref() {
        None => None,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) => Some(n),
            Err(_) => {
                return error_envelope(
                    StatusCode::BAD_REQUEST,
                    "limit must be a non-negative integer",
                    "BAD_PARAM",
                );
            }
        },
    };
    match load_attempts_envelope(&state.data_dir, solution_number, limit) {
        Ok(data) => success_envelope(&data),
        Err(AttemptsLoadError::NotFound) => error_envelope(
            StatusCode::NOT_FOUND,
            &format!("solution_number {solution_number} not found"),
            "NOT_FOUND",
        ),
        Err(AttemptsLoadError::Io(e)) => {
            tracing::warn!(
                solution_number,
                error = %e,
                "dashboard: failed to read attempts for solution_number"
            );
            error_envelope(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to read attempts",
                "INTERNAL_ERROR",
            )
        }
    }
}

pub(super) enum AttemptsLoadError {
    NotFound,
    Io(std::io::Error),
}

/// Read `data_dir/<solution_number>/attempts.jsonl` and build the
/// `{ submission, attempts }` envelope the indexer parser expects.
///
/// `limit` returns only the newest N attempts by `ts_ns` and adds a `totals`
/// object covering the whole trail, so a client can show a bounded tail without
/// downloading every row. A two-GPU node records about 9,000 attempts on a long
/// qblock, which is roughly 2.3 MB in one response.
pub(super) fn load_attempts_envelope(
    data_dir: &Path,
    solution_number: u64,
    limit: Option<usize>,
) -> Result<Value, AttemptsLoadError> {
    let dir = data_dir.join(solution_number.to_string());
    if !dir.is_dir() {
        return Err(AttemptsLoadError::NotFound);
    }
    let path = dir.join("attempts.jsonl");
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(AttemptsLoadError::NotFound);
        }
        Err(e) => return Err(AttemptsLoadError::Io(e)),
    };

    let mut records: Vec<Map<String, Value>> = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        records.push(obj);
    }
    if records.is_empty() {
        return Err(AttemptsLoadError::NotFound);
    }

    // `iter` numbers a row by its position in the whole trail, so it is
    // assigned before any truncation and stays stable across limits.
    let mut kept: Vec<(usize, &Map<String, Value>)> = records.iter().enumerate().collect();
    if let Some(n) = limit {
        // Newest by `ts_ns`. The trail interleaves miners, so file order is not
        // guaranteed to be time order. A stable sort keeps file order among
        // rows sharing a timestamp, which is the common case within one miner.
        kept.sort_by_key(|(_, rec)| ts_ns_of(rec));
        let drop = kept.len().saturating_sub(n);
        let _ = kept.drain(..drop);
    }

    let attempts = kept
        .into_iter()
        .map(|(i, rec)| attempt_from_record(rec, i + 1, solution_number))
        .collect::<Vec<_>>();

    let submission = submission_from_records(&records, solution_number);
    let mut envelope = Map::new();
    let _ = envelope.insert("submission".into(), submission);
    let _ = envelope.insert("attempts".into(), Value::Array(attempts));
    if limit.is_some() {
        let _ = envelope.insert(
            "totals".into(),
            totals_from_records(&records, solution_number),
        );
    }
    Ok(Value::Object(envelope))
}

/// Summarize the whole trail for a client that only fetched a tail.
///
/// `num_valid` comes from the newest submitted row, matching the row
/// [`submission_from_records`] prefers, so the two objects never disagree.
/// `best_energy_milli` is the lowest resolved energy across the trail; a trail
/// where no row ever cleared the gate and no row carries a raw best reports 0,
/// the same value [`wire_energy_milli`] puts on such a row.
pub(super) fn totals_from_records(records: &[Map<String, Value>], solution_number: u64) -> Value {
    let best = records
        .iter()
        .filter_map(resolved_energy_milli)
        .min()
        .unwrap_or(0);
    let qpu_access_time_us = records.iter().fold(0_u64, |acc, rec| {
        acc.saturating_add(u64_field(rec, "device_access_time_us").unwrap_or(0))
    });
    let num_valid = records
        .iter()
        .rev()
        .find(|r| bool_field(r, "submitted") == Some(true))
        .and_then(|r| u64_field(r, "n_valid"))
        .map_or(Value::Null, |n| safe_u64("num_valid", solution_number, n));

    json!({
        "attempt_count": safe_u64(
            "attempt_count",
            solution_number,
            u64::try_from(records.len()).unwrap_or(u64::MAX),
        ),
        "best_energy_milli": safe_i64("best_energy_milli", solution_number, best),
        "qpu_access_time_us": safe_u64("qpu_access_time_us", solution_number, qpu_access_time_us),
        "num_valid": num_valid,
    })
}

/// Map one v0.3 [`crate::observe::attempt::AttemptRecord`] JSON object onto the v0.2
/// attempt wire shape the indexer parser reads.
pub(super) fn attempt_from_record(
    rec: &Map<String, Value>,
    iter: usize,
    solution_number: u64,
) -> Value {
    let best_energy_milli = wire_energy_milli(rec, solution_number);
    let result_kind = result_kind_of(rec);
    let miner_id = string_field(rec, "miner_id").unwrap_or_default();
    let ts_ns = ts_ns_of(rec);
    // Map device access time onto the QPU field the indexer sums. CPU/GPU
    // miners also record this; the indexer treats the sum as QPU compute.
    let qpu_access_time_us = match u64_field(rec, "device_access_time_us") {
        Some(us) if us > 0 => safe_u64("qpu_access_time_us", solution_number, us),
        _ => Value::Null,
    };
    json!({
        "type": "attempt",
        "ts_ns": wire_u128(ts_ns),
        "miner_id": miner_id,
        "miner_type": string_field(rec, "miner_type").unwrap_or_default(),
        "solution_number": safe_u64("solution_number", solution_number, solution_number),
        "iter": safe_u64(
            "iter",
            solution_number,
            u64::try_from(iter).unwrap_or(u64::MAX),
        ),
        "best_energy_milli": best_energy_milli,
        "result_kind": result_kind,
        "num_valid": rec.get("n_valid").and_then(Value::as_u64)
            .map_or(Value::Null, |n| safe_u64("num_valid", solution_number, n)),
        "diversity_milli": rec.get("diversity_milli").and_then(Value::as_u64)
            .map_or(Value::Null, |n| safe_u64("diversity_milli", solution_number, n)),
        "qpu_access_time_us": qpu_access_time_us,
        "job_id": string_field(rec, "job_id"),
        "accepted": bool_field(rec, "accepted"),
        "submitted": bool_field(rec, "submitted"),
    })
}

/// Build a submission object from the attempt trail. v0.3 does not write
/// `submission.json`; the indexer still requires the submission object.
pub(super) fn submission_from_records(
    records: &[Map<String, Value>],
    solution_number: u64,
) -> Value {
    // Prefer the last submitted attempt; else the last accepted; else the last.
    // Caller guarantees `records` is non-empty; fall back to an empty map only
    // so this helper never panics if that invariant is broken.
    let empty = Map::new();
    let chosen = records
        .iter()
        .rev()
        .find(|r| bool_field(r, "submitted") == Some(true))
        .or_else(|| {
            records
                .iter()
                .rev()
                .find(|r| bool_field(r, "accepted") == Some(true))
        })
        .or_else(|| records.last())
        .unwrap_or(&empty);

    let miner_id = string_field(chosen, "miner_id").unwrap_or_else(|| "unknown".into());
    let energy_milli = wire_energy_milli(chosen, solution_number);
    let diversity_milli = safe_u64(
        "diversity_milli",
        solution_number,
        u64_field(chosen, "diversity_milli").unwrap_or(0),
    );
    let num_valid = u64_field(chosen, "n_valid")
        .map_or(Value::Null, |n| safe_u64("num_valid", solution_number, n));
    let ts_ns = wire_u128(ts_ns_of(chosen));

    let any_submitted = records
        .iter()
        .any(|r| bool_field(r, "submitted") == Some(true));
    let any_accepted = records
        .iter()
        .any(|r| bool_field(r, "accepted") == Some(true));
    let outcome = if any_submitted {
        "submitted"
    } else if any_accepted {
        "stored"
    } else {
        "rejected"
    };

    json!({
        "type": "submission",
        "ts_ns": ts_ns,
        "solution_number": safe_u64("solution_number", solution_number, solution_number),
        "miner_id": miner_id,
        "miner_type": string_field(chosen, "miner_type").unwrap_or_default(),
        "energy_milli": energy_milli,
        "diversity_milli": diversity_milli,
        "threshold_milli": safe_i64(
            "threshold_milli",
            solution_number,
            i64_field(chosen, "threshold_milli").unwrap_or(0),
        ),
        "num_valid": num_valid,
        "last_proof_block_hash": string_field(chosen, "last_proof_block_hash")
            .unwrap_or_else(|| "0x0".to_string()),
        "extrinsic_hash": chosen.get("extrinsic_hash").cloned().unwrap_or(Value::Null),
        "chain_block_hash": chosen.get("chain_block_hash").cloned().unwrap_or(Value::Null),
        "chain_block_number": u64_field(chosen, "chain_block_number")
            .map_or(Value::Null, |n| safe_u64("chain_block_number", solution_number, n)),
        "pow_sequence": u64_field(chosen, "pow_sequence")
            .map_or(Value::Null, |n| safe_u64("pow_sequence", solution_number, n)),
        "outcome": outcome,
    })
}

pub(super) fn result_kind_of(rec: &Map<String, Value>) -> &'static str {
    if bool_field(rec, "submitted") == Some(true) {
        "submitted"
    } else if bool_field(rec, "accepted") == Some(true) {
        "stored"
    } else {
        "rejected"
    }
}

pub(super) fn ts_ns_of(rec: &Map<String, Value>) -> u128 {
    // AttemptRecord records wall time in milliseconds; the wire shape uses ns.
    match u64_field(rec, "ts_ms") {
        Some(ms) => u128::from(ms).saturating_mul(1_000_000),
        None => 0,
    }
}

pub(super) fn string_field(rec: &Map<String, Value>, key: &str) -> Option<String> {
    rec.get(key).and_then(|v| match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    })
}

pub(super) fn bool_field(rec: &Map<String, Value>, key: &str) -> Option<bool> {
    rec.get(key).and_then(Value::as_bool)
}

pub(super) fn i64_field(rec: &Map<String, Value>, key: &str) -> Option<i64> {
    rec.get(key).and_then(|v| match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    })
}

pub(super) fn u64_field(rec: &Map<String, Value>, key: &str) -> Option<u64> {
    rec.get(key).and_then(|v| match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    })
}

/// Resolve the energy a record puts on the wire.
///
/// Order: the gate-passing best when it is not the sentinel, then the raw best
/// when it is not the sentinel, then `0`. Legacy rows written before
/// `raw_best_energy_milli` existed fall to `0` here, which is why the guard in
/// [`safe_i64`] is a backstop and not the fix.
pub(super) fn wire_energy_milli(rec: &Map<String, Value>, solution_number: u64) -> Value {
    let resolved = resolved_energy_milli(rec).unwrap_or(0);
    safe_i64("best_energy_milli", solution_number, resolved)
}

/// The real energy a record holds, or `None` when it holds no real energy:
/// both fields are the sentinel, or neither is present. Separated from
/// [`wire_energy_milli`] so the `totals` fold can skip such rows instead of
/// folding their `0` in as a minimum.
pub(super) fn resolved_energy_milli(rec: &Map<String, Value>) -> Option<i64> {
    match i64_field(rec, "best_energy_milli") {
        Some(best) if best != i64::MAX => Some(best),
        _ => match i64_field(rec, "raw_best_energy_milli") {
            Some(raw) if raw != i64::MAX => Some(raw),
            _ => None,
        },
    }
}

// ---------------------------------------------------------------------------
// Safe-integer guard (rule N1)
// ---------------------------------------------------------------------------

/// Largest integer an IEEE-754 double holds exactly (`Number.MAX_SAFE_INTEGER`).
pub(super) const JS_MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// Smallest integer an IEEE-754 double holds exactly.
pub(super) const JS_MIN_SAFE_INTEGER: i64 = -9_007_199_254_740_991;

/// Serialize `v` as a JSON number, or `0` when it falls outside the range a
/// JavaScript `Number()` holds exactly.
///
/// The dashboard parses every numeric field through `Number()` and stores the
/// result in a `PostgreSQL` `BIGINT`. A value past the safe range round-trips to a
/// different integer and the insert fails, which stalls the indexer checkpoint.
/// Clamping to `0` keeps the walk moving; the warning names the field so the
/// real source is still findable.
pub(super) fn safe_i64(field: &str, solution_number: u64, v: i64) -> Value {
    if (JS_MIN_SAFE_INTEGER..=JS_MAX_SAFE_INTEGER).contains(&v) {
        return Value::from(v);
    }
    tracing::warn!(
        field,
        solution_number,
        value = v,
        "dashboard: integer outside the IEEE-754 safe range; serving 0"
    );
    Value::from(0)
}

/// [`safe_i64`] for unsigned fields. Only the upper bound can be exceeded.
#[expect(clippy::single_match_else, reason = "Brief specifies match structure")]
pub(super) fn safe_u64(field: &str, solution_number: u64, v: u64) -> Value {
    match i64::try_from(v) {
        Ok(n) => safe_i64(field, solution_number, n),
        Err(_) => {
            tracing::warn!(
                field,
                solution_number,
                value = v,
                "dashboard: integer outside the IEEE-754 safe range; serving 0"
            );
            Value::from(0)
        }
    }
}

/// Serialize a field that is large by design as a decimal string.
///
/// Balances, nanosecond timestamps, and chain block numbers routinely exceed the
/// safe range in normal operation, so clamping them to `0` would throw away real
/// data. Rule N1 puts them on the wire as strings instead. The dashboard parser
/// already coerces these fields with `String(...)`.
pub(super) fn wire_u128(v: u128) -> Value {
    Value::String(v.to_string())
}
