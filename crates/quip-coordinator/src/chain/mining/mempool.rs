//! Enumerating the `QuantumComputeMempool.JobOrders` map.
//!
//! Order discovery used to read `JobProposed` events, which needs runtime
//! metadata and only sees the head block. Walking the storage map needs
//! neither. It also finds orders opened in earlier blocks, which the event
//! scan missed.

use crate::chain::codec::extrinsic::twox128;

/// Length of the two `twox128` hashes that name a storage map.
const MAP_PREFIX_LEN: usize = 32;
/// Length of the `Blake2_128` hash that `Blake2_128Concat` puts before the key.
const BLAKE_HASH_LEN: usize = 16;

/// Storage prefix for every entry in `QuantumComputeMempool.JobOrders`.
#[must_use]
pub fn job_orders_prefix() -> Vec<u8> {
    let mut key = Vec::with_capacity(MAP_PREFIX_LEN);
    key.extend_from_slice(&twox128(b"QuantumComputeMempool"));
    key.extend_from_slice(&twox128(b"JobOrders"));
    key
}

/// Recover the order id from a full `JobOrders` storage key.
///
/// `Blake2_128Concat` stores the raw SCALE-encoded key after its hash, so the
/// trailing eight bytes are the little-endian `u64` order id.
///
/// Returns `None` when `key` is not a `JobOrders` key of the expected length.
#[must_use]
pub fn order_id_from_key(key: &[u8]) -> Option<u64> {
    let want = MAP_PREFIX_LEN + BLAKE_HASH_LEN + 8;
    if key.len() != want || !key.starts_with(&job_orders_prefix()) {
        return None;
    }
    let tail = key.get(want - 8..)?;
    let mut buf = [0u8; 8];
    buf.copy_from_slice(tail);
    Some(u64::from_le_bytes(buf))
}

use crate::chain::client::storage::parse_block_number;
use crate::chain::client::RealChainClient;
use crate::chain::codec::extrinsic::job_orders_storage_key;
use crate::chain::codec::scale_types::{JobOrderScale, OrderStatus, OrderTiming};
use crate::chain::ChainError;
use crate::types::hex::{hex_decode, hex_encode};
use crate::types::order::{JobMode, JobOrder};
use parity_scale_codec::Decode;
use serde_json::Value;

/// Keys requested per `state_getKeysPaged` call when walking the order map.
///
/// The mempool holds tens of orders in practice. A page of 200 fetches them in
/// one round trip while staying far below any node response limit.
pub(in crate::chain) const ORDER_PAGE_SIZE: u32 = 200;

impl RealChainClient {
    /// Read the open mempool orders; see `ChainClient::fetch_mempool_orders`.
    #[expect(
        clippy::too_many_lines,
        reason = "one order walk: registration, paged keys, status, expiry, and eligibility"
    )]
    pub(in crate::chain) async fn read_mempool_orders(
        &self,
        miner_account: [u8; 32],
    ) -> Result<Vec<JobOrder>, ChainError> {
        // Discover order ids by walking the JobOrders map at head, then
        // storage-read each order. Without a live node this returns transport
        // errors; with a node, empty open-order sets yield Ok(vec![]).
        let head = self
            .rpc_call("chain_getBlockHash", Value::Array(vec![]))
            .await?;
        let head_hex = head
            .as_str()
            .ok_or_else(|| ChainError::Decode("chain_getBlockHash not a string".into()))?;
        let head_bytes = hex_decode(head_hex).map_err(ChainError::Decode)?;
        if head_bytes.len() != 32 {
            return Err(ChainError::Decode(format!(
                "head hash is {} bytes, expected 32",
                head_bytes.len()
            )));
        }
        // Walk the JobOrders map rather than decoding JobProposed events. The
        // map holds every open order, not only those proposed in the head
        // block, and it needs no runtime metadata.
        let prefix = hex_encode(&job_orders_prefix());
        let mut order_ids: Vec<u64> = Vec::new();
        let mut start_key: Option<String> = None;
        loop {
            let mut params = vec![
                Value::String(prefix.clone()),
                Value::Number(ORDER_PAGE_SIZE.into()),
            ];
            // state_getKeysPaged takes the previous page's last key as the
            // resume point. The block hash is always the final argument, so a
            // missing start key must still be sent as null.
            params.push(match &start_key {
                Some(k) => Value::String(k.clone()),
                None => Value::Null,
            });
            params.push(Value::String(head_hex.to_string()));
            let page = self
                .rpc_call("state_getKeysPaged", Value::Array(params))
                .await?;
            let Some(keys) = page.as_array() else {
                break;
            };
            if keys.is_empty() {
                break;
            }
            for k in keys {
                let Some(hex) = k.as_str() else { continue };
                let bytes = hex_decode(hex).map_err(ChainError::Decode)?;
                if let Some(oid) = order_id_from_key(&bytes) {
                    order_ids.push(oid);
                }
            }
            let full_page = keys.len() == ORDER_PAGE_SIZE as usize;
            start_key = keys.last().and_then(|k| k.as_str()).map(String::from);
            if !full_page {
                break;
            }
        }

        let header = self
            .rpc_call(
                "chain_getHeader",
                Value::Array(vec![Value::String(head_hex.to_string())]),
            )
            .await?;
        let head_number = parse_block_number(&header)?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let mut orders = Vec::new();
        for oid in order_ids {
            let key = job_orders_storage_key(oid);
            let raw = self
                .rpc_call(
                    "state_getStorage",
                    Value::Array(vec![
                        Value::String(hex_encode(&key)),
                        Value::String(head_hex.to_string()),
                    ]),
                )
                .await?;
            let Some(hex) = raw.as_str() else {
                continue;
            };
            let bytes = hex_decode(hex).map_err(ChainError::Decode)?;
            let scale = JobOrderScale::decode(&mut &bytes[..])
                .map_err(|e| ChainError::Decode(format!("JobOrder {oid}: {e}")))?;
            if scale.status != OrderStatus::Opened {
                continue;
            }
            // The pallet expires orders lazily, so an `Opened` status can
            // outlive the order. Skip one already past its expiry.
            let Some(deadline_ms) = order_deadline_ms(
                scale.created_at,
                scale.first_solution_at,
                &scale.timing,
                head_number,
                now_ms,
            ) else {
                continue;
            };
            let order = JobOrder {
                order_id: oid.to_le_bytes().to_vec(),
                mode: scale.mode,
                nodes: scale.ising_params.nodes,
                edges: scale.ising_params.edges,
                h_milli: scale.ising_params.h_values,
                j_milli: scale.ising_params.j_values,
                min_energy_milli: scale.ising_params.min_energy_milli,
                min_diversity_milli: scale.ising_params.min_diversity_milli,
                min_solutions: scale.ising_params.min_solutions,
                deadline_ms,
            };
            let eligible = match &order.mode {
                JobMode::Open => true,
                JobMode::Bid { .. } => self
                    .registered_solver_type(miner_account, head_hex)
                    .await?
                    .is_some_and(|solver_type| order.is_eligible_for(&miner_account, solver_type)),
            };
            if !eligible {
                tracing::debug!(
                    order_id = oid,
                    "mempool order excludes this solver; skipping it"
                );
                continue;
            }
            orders.push(order);
        }
        Ok(orders)
    }
}

/// Block time assumed when turning a block count into wall-clock time.
pub(in crate::chain) const BLOCK_TIME_ESTIMATE_MS: u64 = 6_000;

/// Absolute deadline, in Unix milliseconds, for a mempool order seen at `head`.
///
/// Miners compare `deadline_ms` with their own clock, so it must be absolute.
/// Mirrors the pallet's `effective_expiry`: the hard deadline, cut short by
/// `block_wait` once a first solution lands. `None` once `head` has reached
/// that expiry, when `submit_solution` would find the order closed.
pub(in crate::chain) fn order_deadline_ms(
    created_at: u32,
    first_solution_at: Option<u32>,
    timing: &OrderTiming,
    head: u64,
    now_ms: u64,
) -> Option<u64> {
    let hard = created_at.saturating_add(timing.deadline_blocks);
    let expiry = first_solution_at.map_or(hard, |first| {
        hard.min(first.saturating_add(timing.block_wait))
    });
    let remaining = u64::from(expiry).checked_sub(head).filter(|&b| b > 0)?;
    Some(now_ms.saturating_add(remaining.saturating_mul(BLOCK_TIME_ESTIMATE_MS)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::codec::extrinsic::job_orders_storage_key;

    #[test]
    fn the_prefix_is_the_leading_half_of_a_full_order_key() {
        let prefix = job_orders_prefix();
        assert_eq!(prefix.len(), 32);
        let full = job_orders_storage_key(42);
        assert!(full.starts_with(&prefix), "prefix must lead the full key");
    }

    /// `Blake2_128Concat` appends the SCALE-encoded key, so the order id is
    /// recoverable from the key alone. That is what makes enumeration work.
    #[test]
    fn an_order_id_round_trips_through_its_storage_key() {
        for id in [0u64, 1, 42, u64::MAX] {
            let key = job_orders_storage_key(id);
            assert_eq!(order_id_from_key(&key), Some(id), "id {id}");
        }
    }

    #[test]
    fn a_key_that_is_not_an_order_key_yields_none() {
        // A key whose prefix does not match must not be misread as an order.
        let mut foreign = vec![0xAAu8; 32];
        foreign.extend_from_slice(&[0u8; 16]);
        foreign.extend_from_slice(&7u64.to_le_bytes());
        for (label, key) in [("too short", vec![0u8; 8]), ("different map", foreign)] {
            assert_eq!(order_id_from_key(&key), None, "{label}");
        }
    }
}

#[cfg(test)]
mod fetch_tests {

    #[test]
    fn an_order_deadline_is_absolute_and_honours_block_wait() {
        let timing = super::OrderTiming {
            deadline_blocks: 50,
            block_wait: 5,
        };
        assert_eq!(
            super::order_deadline_ms(100, None, &timing, 120, 1_000),
            Some(1_000 + 30 * 6_000)
        );
        assert_eq!(
            super::order_deadline_ms(100, Some(110), &timing, 112, 0),
            Some(3 * 6_000)
        );
        assert_eq!(
            super::order_deadline_ms(100, Some(110), &timing, 115, 0),
            None,
            "block_wait has elapsed since the first solution"
        );
        assert_eq!(
            super::order_deadline_ms(100, None, &timing, 150, 0),
            None,
            "the hard deadline has been reached"
        );
    }

    mod bid_orders {
        use crate::chain::codec::extrinsic::{
            job_orders_storage_key, signer_account_bytes, solvers_storage_key,
        };
        use crate::chain::codec::scale_types::{
            IsingParams, JobOrderScale, OrderStatus, OrderTiming, ResultDelivery, RewardResolution,
            SolverInfoScale,
        };
        use crate::chain::sign::submit::Confirmation;
        use crate::types::miner::SolverType;
        use crate::types::order::JobMode;

        use crate::chain::{
            BoxStream, ChainClient, ChainError, RealChainClient, RegistrationOutcome, RpcTransport,
        };
        use crate::types::hex::{hex_decode, hex_encode};
        use crate::types::miner::MinerKind;
        use parity_scale_codec::Encode as _;
        use quip_transaction_crypto::HybridPair;
        use serde_json::Value;
        use sp_core::{Pair as _, H256};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        const HEAD: [u8; 32] = [0xab; 32];

        struct BidValidator {
            account: [u8; 32],
            registration: Mutex<Value>,
            solver_reads: AtomicUsize,
            orders: Vec<(u64, JobOrderScale)>,
        }

        fn alice_account() -> [u8; 32] {
            signer_account_bytes(
                &HybridPair::from_string("//Alice", None).expect("public dev account"),
            )
        }

        fn registration(account: [u8; 32], solver_type: SolverType) -> Value {
            Value::String(hex_encode(
                &SolverInfoScale {
                    account,
                    solver_type,
                    registered_at: 1,
                    solutions_submitted: 0,
                    rewards_earned: 0,
                }
                .encode(),
            ))
        }

        fn scale_order(mode: JobMode) -> JobOrderScale {
            JobOrderScale {
                spec_id: H256::repeat_byte(7),
                proposer: [3; 32],
                ising_params: IsingParams {
                    nodes: vec![0, 1],
                    edges: vec![(0, 1)],
                    h_values: vec![0, 0],
                    j_values: vec![-1000],
                    min_energy_milli: None,
                    min_diversity_milli: None,
                    min_solutions: Some(1),
                },
                reward: 100,
                mode,
                resolution: RewardResolution::SingleBest,
                timing: OrderTiming {
                    deadline_blocks: 50,
                    block_wait: 5,
                },
                delivery: ResultDelivery::OnChainOnly,
                status: OrderStatus::Opened,
                created_at: 1,
                first_solution_at: None,
                solution_count: 0,
            }
        }

        fn type_bid(tag: u8) -> JobOrderScale {
            scale_order(JobMode::Bid {
                miners: None,
                miner_types: Some(vec![tag]),
            })
        }

        fn client_for(
            account: [u8; 32],
            solver_type: Option<SolverType>,
            orders: Vec<JobOrderScale>,
            configured_kind: MinerKind,
        ) -> (Arc<BidValidator>, RealChainClient) {
            let validator = Arc::new(BidValidator {
                account,
                registration: Mutex::new(
                    solver_type.map_or(Value::Null, |kind| registration(account, kind)),
                ),
                solver_reads: AtomicUsize::new(0),
                orders: orders
                    .into_iter()
                    .enumerate()
                    .map(|(index, order)| (u64::try_from(index + 1).expect("small fixture"), order))
                    .collect(),
            });
            let client = RealChainClient::with_transport(
                vec!["ws://scripted".to_string()],
                "//Alice".to_string(),
                configured_kind,
                validator.clone(),
            );
            (validator, client)
        }

        async fn assert_order_count(client: &RealChainClient, account: [u8; 32], expected: usize) {
            let orders = client.fetch_mempool_orders(account).await.expect("orders");
            assert_eq!(orders.len(), expected);
        }

        fn assert_decode_error(error: &ChainError) {
            assert!(match error {
                ChainError::Decode(_) => true,
                ChainError::Unavailable(_) | ChainError::Submit(_) => false,
            });
        }

        #[async_trait::async_trait]
        impl RpcTransport for BidValidator {
            async fn request(
                &self,
                _url: &str,
                method: &str,
                params: Value,
            ) -> Result<Value, ChainError> {
                let arg = |index: usize| {
                    params
                        .get(index)
                        .and_then(Value::as_str)
                        .expect("string RPC argument")
                };
                match method {
                    "chain_getBlockHash" => Ok(Value::String(hex_encode(&HEAD))),
                    "chain_getHeader" => {
                        assert_eq!(arg(0), hex_encode(&HEAD));
                        Ok(serde_json::json!({ "number": "0x02" }))
                    }
                    "state_getKeysPaged" => {
                        assert_eq!(arg(0), hex_encode(&crate::chain::job_orders_prefix()));
                        assert_eq!(params.get(1).and_then(Value::as_u64), Some(200));
                        assert_eq!(params.get(2), Some(&Value::Null));
                        assert_eq!(arg(3), hex_encode(&HEAD));
                        Ok(Value::Array(
                            self.orders
                                .iter()
                                .map(|(id, _)| {
                                    Value::String(hex_encode(&job_orders_storage_key(*id)))
                                })
                                .collect(),
                        ))
                    }
                    "state_getStorage" => {
                        assert_eq!(arg(1), hex_encode(&HEAD));
                        let key = hex_decode(arg(0)).map_err(ChainError::Decode)?;
                        if key == solvers_storage_key(&self.account) {
                            let _ = self.solver_reads.fetch_add(1, Ordering::Relaxed);
                            let stored = self.registration.lock().expect("registration").clone();
                            if let Some(error) = stored.get("rpc_error").and_then(Value::as_str) {
                                return Err(ChainError::Unavailable(error.to_string()));
                            }
                            return Ok(stored);
                        }
                        for (id, order) in &self.orders {
                            if key == job_orders_storage_key(*id) {
                                return Ok(Value::String(hex_encode(&order.encode())));
                            }
                        }
                        Ok(Value::Null)
                    }
                    other => Err(ChainError::Unavailable(format!(
                        "unexpected RPC method {other}"
                    ))),
                }
            }

            async fn subscribe(
                &self,
                _url: &str,
                _sub: &str,
                _params: Value,
                _unsub: &str,
            ) -> Result<BoxStream<'static, Result<Value, ChainError>>, ChainError> {
                Err(ChainError::Unavailable(
                    "no subscriptions in this test".into(),
                ))
            }
        }

        #[tokio::test]
        async fn fetch_excludes_bid_and_preserves_allowed_modes() {
            let account = alice_account();
            let account_bid = JobMode::Bid {
                miners: Some(vec![account]),
                miner_types: Some(vec![1]),
            };
            let type_bid = JobMode::Bid {
                miners: Some(vec![[2; 32]]),
                miner_types: Some(vec![0]),
            };
            let (validator, client) = client_for(
                account,
                Some(SolverType::Cpu),
                vec![
                    scale_order(JobMode::Bid {
                        miners: Some(vec![[2; 32]]),
                        miner_types: Some(vec![1]),
                    }),
                    scale_order(account_bid.clone()),
                    scale_order(type_bid.clone()),
                    scale_order(JobMode::Open),
                ],
                MinerKind::QpuDwave,
            );
            let orders = client.fetch_mempool_orders(account).await.expect("orders");
            let ids: Vec<Vec<u8>> = orders.iter().map(|order| order.order_id.clone()).collect();
            assert_eq!(
                ids,
                vec![
                    2u64.to_le_bytes().to_vec(),
                    3u64.to_le_bytes().to_vec(),
                    4u64.to_le_bytes().to_vec(),
                ],
            );
            let modes: Vec<JobMode> = orders.iter().map(|order| order.mode.clone()).collect();
            assert_eq!(modes, vec![account_bid, type_bid, JobMode::Open]);
            let again = client
                .fetch_mempool_orders(account)
                .await
                .expect("second read");
            assert_eq!(again.len(), 3);
            assert_eq!(validator.solver_reads.load(Ordering::Relaxed), 1);
        }

        #[tokio::test]
        async fn startup_registration_seeds_the_gpu_type_for_metal() {
            let account = alice_account();
            let (validator, client) = client_for(
                account,
                Some(SolverType::Gpu),
                vec![type_bid(1)],
                MinerKind::Metal,
            );
            assert_eq!(
                client.ensure_solver_registered().await.expect("registered"),
                RegistrationOutcome::AlreadyRegistered,
            );
            for _ in 0..2 {
                assert_order_count(&client, account, 1).await;
            }
            assert_eq!(validator.solver_reads.load(Ordering::Relaxed), 1);
        }

        /// A confirmation that reads the wrong type is rejected and caches
        /// nothing. A later confirmation that reads the right type replaces
        /// the type cached by the order read in between.
        #[tokio::test]
        async fn registration_confirmation_checks_and_refreshes_the_cached_type() {
            let account = alice_account();
            let (validator, client) = client_for(
                account,
                Some(SolverType::Cpu),
                vec![type_bid(1)],
                MinerKind::Metal,
            );
            let head = hex_encode(&HEAD);
            let confirmation = Confirmation::SolverRegistered { account };
            assert!(!client
                .confirmation_present(&head, &confirmation)
                .await
                .expect("CPU confirmation"));
            assert_order_count(&client, account, 0).await;
            assert_eq!(validator.solver_reads.load(Ordering::Relaxed), 2);

            *validator.registration.lock().expect("registration") =
                registration(account, SolverType::Gpu);
            assert!(client
                .confirmation_present(&head, &confirmation)
                .await
                .expect("GPU confirmation"));
            assert_order_count(&client, account, 1).await;
            assert_eq!(validator.solver_reads.load(Ordering::Relaxed), 3);
        }

        /// A missing, malformed, or unreachable registration is never cached,
        /// so the next order read retries it.
        #[tokio::test]
        async fn a_failed_registration_read_is_retried() {
            let account = alice_account();
            for (label, stored, expected_error) in [
                ("missing", Value::Null, None),
                (
                    "malformed",
                    Value::String("0xff".into()),
                    Some(ChainError::Decode(String::new())),
                ),
                (
                    "outage",
                    serde_json::json!({ "rpc_error": "scripted outage" }),
                    Some(ChainError::Unavailable(String::new())),
                ),
            ] {
                let (validator, client) =
                    client_for(account, None, vec![type_bid(0)], MinerKind::Cpu);
                *validator.registration.lock().expect("registration") = stored;
                let first = client.fetch_mempool_orders(account).await;
                match expected_error {
                    None => assert_eq!(first.expect(label).len(), 0, "{label}"),
                    Some(expected) => {
                        let error = first.expect_err(label);
                        assert_eq!(
                            std::mem::discriminant(&error),
                            std::mem::discriminant(&expected),
                            "{label}: {error}"
                        );
                    }
                }
                *validator.registration.lock().expect("registration") =
                    registration(account, SolverType::Cpu);
                assert_order_count(&client, account, 1).await;
                assert_eq!(validator.solver_reads.load(Ordering::Relaxed), 2, "{label}");
            }
        }

        #[tokio::test]
        async fn failed_registration_refresh_clears_the_previous_type() {
            let account = alice_account();
            let (validator, client) = client_for(
                account,
                Some(SolverType::Gpu),
                vec![type_bid(1)],
                MinerKind::Metal,
            );
            let _ = client
                .ensure_solver_registered()
                .await
                .expect("GPU registration");
            *validator.registration.lock().expect("registration") = Value::String("0xff".into());
            let error = client
                .ensure_solver_registered()
                .await
                .expect_err("bad refresh");
            assert_decode_error(&error);
            *validator.registration.lock().expect("registration") =
                registration(account, SolverType::Cpu);
            assert_order_count(&client, account, 0).await;
            assert_eq!(validator.solver_reads.load(Ordering::Relaxed), 3);
        }

        #[tokio::test]
        async fn a_cached_type_is_never_used_for_another_account() {
            let account = alice_account();
            let (validator, client) = client_for(
                account,
                Some(SolverType::Cpu),
                vec![type_bid(0)],
                MinerKind::Cpu,
            );
            assert_order_count(&client, account, 1).await;
            assert_order_count(&client, [2; 32], 0).await;
            assert_order_count(&client, account, 1).await;
            assert_eq!(validator.solver_reads.load(Ordering::Relaxed), 2);
        }

        #[tokio::test]
        async fn open_discovery_does_not_require_a_solver_type_read() {
            let account = alice_account();
            let (validator, client) = client_for(
                account,
                None,
                vec![scale_order(JobMode::Open)],
                MinerKind::Cpu,
            );
            assert_order_count(&client, account, 1).await;
            assert_eq!(validator.solver_reads.load(Ordering::Relaxed), 0);
        }
    }
}
