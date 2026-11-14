//! Talking to the node: one JSON-RPC call, storage reads, runtime API calls,
//! and the reachability log.

use crate::chain::client::RealChainClient;
use crate::chain::ChainError;
use crate::types::hex::{hex_decode, hex_encode};
use parity_scale_codec::Decode;
use serde_json::Value;

impl RealChainClient {
    /// Record the outcome of one RPC round-trip and log reachability changes.
    ///
    /// A validator that goes away (or comes back) is the single most common
    /// reason a coordinator stops mining, and v0.2.1 narrated it on every
    /// reconnect. Reporting only the edges keeps that signal without the
    /// per-call volume.
    pub(in crate::chain) fn note_reachability(&self, url: &str, outcome: Result<(), &ChainError>) {
        let now = outcome.is_ok();
        // Poisoned lock: reachability is advisory, never fail an RPC over it.
        let Ok(mut guard) = self.reachable.lock() else {
            return;
        };
        if *guard == Some(now) {
            return;
        }
        match outcome {
            Ok(()) => tracing::info!(url = %url, "validator RPC reachable"),
            Err(e) => {
                tracing::warn!(url = %url, error = %e, "validator RPC unreachable");
            }
        }
        *guard = Some(now);
    }

    pub(in crate::chain) fn primary_url(&self) -> Result<&str, ChainError> {
        self.validators
            .first()
            .map(String::as_str)
            .ok_or_else(|| ChainError::Unavailable("no validators configured".into()))
    }

    /// Issue `method` against the configured validators in order, returning the
    /// first success.
    ///
    /// `validators` is documented as an ordered failover list, and the default
    /// pair leads with a container-network name that does not resolve on a host
    /// install. Trying only the first entry makes that default unusable off
    /// Docker, so every entry gets a turn before the call is called failed.
    pub(in crate::chain) async fn rpc_call(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, ChainError> {
        if self.validators.is_empty() {
            return Err(ChainError::Unavailable("no validators configured".into()));
        }
        let mut last: Option<ChainError> = None;
        for url in &self.validators {
            let out = self.transport.request(url, method, params.clone()).await;
            tracing::trace!(url = %url, method = %method, ok = out.is_ok(), "rpc call");
            match out {
                Ok(v) => {
                    // Lock only after the await: `reachable` is a std Mutex and
                    // must never be held across a suspension point.
                    self.note_reachability(url, Ok(()));
                    return Ok(v);
                }
                Err(e) => {
                    tracing::debug!(url = %url, method = %method, error = %e, "validator failed; trying next");
                    last = Some(e);
                }
            }
        }
        // Every endpoint failed: report against the primary, which is the one an
        // operator will look at first.
        let err =
            last.unwrap_or_else(|| ChainError::Unavailable("no validators configured".into()));
        if let Some(primary) = self.validators.first() {
            self.note_reachability(primary, Err(&err));
        }
        Err(err)
    }

    /// Raw `state_getRuntimeVersion` response, for the startup compatibility
    /// check in [`super::preflight`].
    ///
    /// # Errors
    /// Returns a transport error when the validator cannot be reached.
    pub(crate) async fn runtime_version_raw(&self) -> Result<Value, ChainError> {
        self.rpc_call("state_getRuntimeVersion", Value::Array(vec![]))
            .await
    }

    /// Raw `system_health` response, for the startup sync gate in
    /// [`super::sync`].
    ///
    /// # Errors
    /// Returns a transport error when the validator cannot be reached.
    pub(crate) async fn system_health_raw(&self) -> Result<Value, ChainError> {
        self.rpc_call("system_health", Value::Array(vec![])).await
    }

    /// Raw `system_syncState` response: the block heights the sync gate reports.
    ///
    /// # Errors
    /// Returns a transport error when the validator cannot be reached, or when
    /// it does not serve this method.
    pub(crate) async fn sync_state_raw(&self) -> Result<Value, ChainError> {
        self.rpc_call("system_syncState", Value::Array(vec![]))
            .await
    }

    /// Read + SCALE-decode a storage value at `at_hex`. `Ok(None)` when the key
    /// is unset (null result).
    pub(in crate::chain) async fn read_storage<T: Decode>(
        &self,
        key: &[u8],
        at_hex: &str,
    ) -> Result<Option<T>, ChainError> {
        let raw = self
            .rpc_call(
                "state_getStorage",
                Value::Array(vec![
                    Value::String(hex_encode(key)),
                    Value::String(at_hex.to_string()),
                ]),
            )
            .await?;
        let Some(hex) = raw.as_str() else {
            return Ok(None);
        };
        let bytes = hex_decode(hex).map_err(ChainError::Decode)?;
        T::decode(&mut &bytes[..])
            .map(Some)
            .map_err(|e| ChainError::Decode(e.to_string()))
    }

    /// `state_call` a `QuantumPowApi` method that takes one `H256` and returns
    /// an `Option`. `Ok(None)` when the runtime answers `None` or null.
    pub(in crate::chain) async fn quantum_pow_call_by_hash<T: Decode>(
        &self,
        method: &str,
        hash: &[u8; 32],
        at_hex: &str,
    ) -> Result<Option<T>, ChainError> {
        let Some(bytes) = self.quantum_pow_bytes_by_hash(method, hash, at_hex).await? else {
            return Ok(None);
        };
        Option::<T>::decode(&mut &bytes[..]).map_err(|e| ChainError::Decode(e.to_string()))
    }

    /// The raw SCALE answer of a `QuantumPowApi` method that takes one `H256`.
    /// `Ok(None)` when the node answers null.
    pub(in crate::chain) async fn quantum_pow_bytes_by_hash(
        &self,
        method: &str,
        hash: &[u8; 32],
        at_hex: &str,
    ) -> Result<Option<Vec<u8>>, ChainError> {
        let result = self
            .rpc_call(
                "state_call",
                Value::Array(vec![
                    Value::String(method.to_string()),
                    Value::String(hex_encode(hash)),
                    Value::String(at_hex.to_string()),
                ]),
            )
            .await?;
        let Some(hex) = result.as_str() else {
            return Ok(None);
        };
        hex_decode(hex).map(Some).map_err(ChainError::Decode)
    }

    /// The current head block hash, `0x`-prefixed.
    pub(crate) async fn head_hex(&self) -> Result<String, ChainError> {
        let head = self
            .rpc_call("chain_getBlockHash", Value::Array(vec![]))
            .await?;
        head.as_str()
            .map(String::from)
            .ok_or_else(|| ChainError::Decode("chain_getBlockHash not a string".into()))
    }

    /// Is any value stored at `key` in the block named by `at_hex`?
    pub(in crate::chain) async fn storage_value_present(
        &self,
        key: &[u8],
        at_hex: &str,
    ) -> Result<bool, ChainError> {
        let raw = self
            .rpc_call(
                "state_getStorage",
                Value::Array(vec![
                    Value::String(hex_encode(key)),
                    Value::String(at_hex.to_string()),
                ]),
            )
            .await?;
        Ok(raw.as_str().is_some())
    }
}

pub(in crate::chain) fn parse_block_number(header: &Value) -> Result<u64, ChainError> {
    let number = header
        .get("number")
        .ok_or_else(|| ChainError::Decode("header missing number".into()))?;
    if let Some(n) = number.as_u64() {
        return Ok(n);
    }
    if let Some(s) = number.as_str() {
        let s = s.strip_prefix("0x").unwrap_or(s);
        return u64::from_str_radix(s, 16).map_err(|e| ChainError::Decode(e.to_string()));
    }
    Err(ChainError::Decode("header.number unparseable".into()))
}

/// The `QuantumPowApi` version from a `state_getRuntimeVersion` response.
pub(in crate::chain) fn quantum_pow_api_version(rv: &Value) -> Result<u32, ChainError> {
    crate::chain::startup::preflight::quantum_pow_api_version(rv)
        .ok_or_else(|| ChainError::Decode("runtime reports no QuantumPowApi".into()))
}

/// `specVersion` from a `state_getRuntimeVersion` response.
pub(crate) fn parse_spec_version(rv: &Value) -> Result<u32, ChainError> {
    u32::try_from(
        rv.get("specVersion")
            .and_then(Value::as_u64)
            .ok_or_else(|| ChainError::Decode("runtime specVersion missing/not a u64".into()))?,
    )
    .map_err(|_| ChainError::Decode("specVersion exceeds u32".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::client::jsonrpsee::{rpc_request, RPC_CONNECT_TIMEOUT, RPC_REQUEST_TIMEOUT};

    #[test]
    fn parse_spec_version_reads_the_number_and_rejects_absence() {
        let rv = serde_json::json!({ "specName": "quip-runtime", "specVersion": 118 });
        assert_eq!(parse_spec_version(&rv).map_err(|e| e.to_string()), Ok(118));
        let missing = serde_json::json!({ "specName": "quip-runtime" });
        assert!(parse_spec_version(&missing).is_err());
    }

    use crate::chain::ChainError;
    use crate::types::miner::MinerKind;

    use serde_json::Value as JsonValue;

    use std::time::{Duration, Instant};
    use tokio::net::TcpListener;

    /// Accept connections and never write a response byte. Models a wedged peer
    /// that passes TCP but blocks the client forever without timeouts.
    async fn spawn_blackhole_listener() -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind blackhole listener");
        let addr = listener.local_addr().expect("blackhole local addr");
        drop(tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                // Hold the accepted socket open and never write. The peer must
                // time out on its own; that is the behaviour under test.
                drop(tokio::spawn(async move {
                    let _sock = sock;
                    std::future::pending::<()>().await;
                }));
            }
        }));
        format!("{addr}")
    }

    /// Upper bound for a single-endpoint blackhole: connect budget plus slack
    /// for scheduler jitter on a loaded CI host. Must stay well under the
    /// test's outer timeout so a hang fails the test, not the harness.
    fn single_endpoint_budget() -> Duration {
        RPC_CONNECT_TIMEOUT
            .saturating_mul(2)
            .saturating_add(Duration::from_secs(5))
    }

    /// Upper bound when the first endpoint blackholes and the second refuses:
    /// one connect timeout plus a refused connect, with CI slack.
    fn failover_budget() -> Duration {
        RPC_CONNECT_TIMEOUT
            .saturating_mul(3)
            .saturating_add(RPC_REQUEST_TIMEOUT)
            .saturating_add(Duration::from_secs(5))
    }

    #[tokio::test]
    async fn http_blackhole_endpoint_errors_within_request_budget() {
        let addr = spawn_blackhole_listener().await;
        let url = format!("http://{addr}");
        // HTTP has no separate connect timeout; request_timeout covers the hang.
        let budget = RPC_REQUEST_TIMEOUT.saturating_add(Duration::from_secs(10));
        let started = Instant::now();
        let outcome = tokio::time::timeout(
            budget,
            rpc_request(&url, "system_health", JsonValue::Array(vec![])),
        )
        .await;
        assert!(outcome.is_ok(), "http blackhole call hung past {budget:?}");
        let err = outcome
            .expect("outer timeout")
            .expect_err("blackhole must not succeed");
        assert!(
            matches!(err, ChainError::Unavailable(_)),
            "timeout must classify as Unavailable for failover, got {err}"
        );
        assert!(
            started.elapsed() < budget,
            "elapsed {:?} exceeds budget",
            started.elapsed()
        );
    }

    /// A single ws blackhole must error within the connect budget, and an
    /// `rpc_call` whose primary blackholes must fail over to the next
    /// validator. `rpc_call` fails over on any error, so the failover path
    /// alone cannot show that the blackhole error classifies as
    /// `Unavailable`. Both calls wait out one real `RPC_CONNECT_TIMEOUT`, so
    /// they run concurrently to pay for that wait once.
    #[tokio::test]
    async fn ws_blackhole_errors_within_connect_budget_and_rpc_call_fails_over_past_it() {
        let addr = spawn_blackhole_listener().await;
        let url = format!("ws://{addr}");
        // Primary accepts and stays silent. Secondary refuses immediately so
        // the call ends after one connect timeout plus a fast connection error.
        let client = RealChainClient::new(
            vec![url.clone(), "ws://127.0.0.1:1".to_string()],
            "//Alice".to_string(),
            MinerKind::Cpu,
        );
        let single = async {
            let started = Instant::now();
            let outcome = tokio::time::timeout(
                single_endpoint_budget(),
                rpc_request(&url, "system_health", JsonValue::Array(vec![])),
            )
            .await;
            (outcome, started.elapsed())
        };
        let failover = async {
            let started = Instant::now();
            let outcome =
                tokio::time::timeout(failover_budget(), client.runtime_version_raw()).await;
            (outcome, started.elapsed())
        };
        let ((single, single_elapsed), (failover, failover_elapsed)) =
            tokio::join!(single, failover);

        assert!(
            single.is_ok(),
            "ws blackhole call hung past {:?}",
            single_endpoint_budget()
        );
        let err = single
            .expect("outer timeout")
            .expect_err("blackhole must not succeed");
        assert!(
            matches!(err, ChainError::Unavailable(_)),
            "timeout must classify as Unavailable for failover, got {err}"
        );
        assert!(
            single_elapsed < single_endpoint_budget(),
            "elapsed {single_elapsed:?} exceeds budget"
        );

        assert!(
            failover.is_ok(),
            "failover hung past {:?}",
            failover_budget()
        );
        let err = failover
            .expect("outer timeout")
            .expect_err("no working validator in the list");
        assert!(
            matches!(err, ChainError::Unavailable(_)),
            "all-endpoint failure must be Unavailable, got {err}"
        );
        // Must have left the blackhole: elapsed is under one connect budget
        // times a small factor, not an unbounded hang.
        assert!(
            failover_elapsed < failover_budget(),
            "elapsed {failover_elapsed:?} exceeds failover budget"
        );
        // And it must have waited long enough that the connect timeout fired
        // (not instant success / skip). A refused-only pair would be near 0.
        assert!(
            failover_elapsed >= RPC_CONNECT_TIMEOUT,
            "expected at least one connect timeout ({RPC_CONNECT_TIMEOUT:?}), got {failover_elapsed:?}"
        );
    }
}
