//! `set-family --dry-run` through the real binary, against a scripted chain.

use quip_coordinator::testing::scripted_rpc::ScriptedRpc;
use serde_json::{json, Value};

const HEAD: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";
/// `Foundation.Members` holding one account: SCALE compact length 1, then 32
/// bytes.
const ONE_MEMBER: &str = "0x040101010101010101010101010101010101010101010101010101010101010101";

/// A chain at `spec_version` with one Foundation member, no default topology
/// and nothing mineable.
fn chain_at(spec_version: u32) -> impl Fn(&str, &Value) -> Value + Send + Sync + 'static {
    let members_key = quip_coordinator::types::hex::hex_encode(
        &quip_coordinator::chain::codec::extrinsic::foundation_members_storage_key(),
    );
    move |method, params| match method {
        "state_getRuntimeVersion" => json!({
            "specName": "quip-runtime",
            "specVersion": spec_version,
            "transactionVersion": 7,
            "apis": [],
        }),
        "chain_getBlockHash" => json!(HEAD),
        "state_getStorage" if params.get(0).and_then(Value::as_str) == Some(&members_key) => {
            json!(ONE_MEMBER)
        }
        "state_getKeysPaged" => json!([]),
        _ => Value::Null,
    }
}

async fn set_family_dry_run(validator: &str) -> std::io::Result<std::process::Output> {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_quip-coordinator"))
        .args([
            "set-family",
            "--dry-run",
            "--validator",
            validator,
            "--topology-preset",
            "zephyr-z12t4-live",
            "--weak-milli",
            "45",
            "--pairs",
            "1350",
            "--max-energy-milli",
            "-4886000",
            "--band-hard-milli",
            "-5136000",
            "--band-easy-milli",
            "-4836000",
        ])
        .env_remove("QUIP_FOUNDATION_KEY")
        .env_remove("RUST_LOG")
        .output()
        .await
}

/// The dry run prints the plan from chain reads alone: it needs no signer and
/// sends nothing.
#[tokio::test(flavor = "multi_thread")]
async fn set_family_dry_run_prints_the_plan_and_only_reads_the_chain() {
    let rpc = ScriptedRpc::start(chain_at(120)).await;
    let out = set_family_dry_run(rpc.url()).await.expect("run set-family");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");

    for expected in [
        "family           quip/twisted-ice-dimer r1",
        "params           0x010100020000000c00020001000000040300020000002d0004000400000046050000",
        "graph            4575 nodes, 41474 edges, legacy hash 0x",
        "current default  unset",
        "foundation       1 members, threshold 1 (executes at once)",
        "call 1",
        "proposal hash    0x",
        "motion           0x",
        "dry run: nothing was signed or sent",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }

    let read_only = [
        "state_getRuntimeVersion",
        "chain_getBlockHash",
        "state_getStorage",
        "state_getKeysPaged",
    ];
    let methods = rpc.methods();
    assert!(!methods.is_empty(), "the dry run read nothing");
    for method in &methods {
        assert!(
            read_only.contains(&method.as_str()),
            "a dry run called {method}"
        );
    }
}

/// A runtime without family topologies stops the dry run with the reason,
/// before any other read.
#[tokio::test(flavor = "multi_thread")]
async fn set_family_dry_run_refuses_a_runtime_without_family_topologies() {
    let rpc = ScriptedRpc::start(chain_at(119)).await;
    let out = set_family_dry_run(rpc.url()).await.expect("run set-family");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("runtime 119 has no family topologies"),
        "stderr: {stderr}"
    );
    assert_eq!(rpc.methods(), ["state_getRuntimeVersion"]);
}
