//! Integration test: the `v1.0.0` reverts that time or the beacon cause,
//! from deployed bytecode. A market is built on an Anvil fork of Arbitrum
//! Sepolia by the W11 smoke steps (`v1_fork`); then a taker open past its
//! deadline, after a warp past the market's `latency()`, after a beacon
//! update outside the price domain (also a 1-atom open and a maker
//! reduce), and with the market's beacon code removed each reverts, and
//! the node's answer decodes through the SDK's revert decode to its name
//! and its [`RevertClass`].
//!
//! Requires `anvil` (from Foundry) and an Arbitrum Sepolia endpoint in
//! `SEPOLIA_FORK_URL` (default: the public RPC).
//!
//! ```bash
//! cargo test --test v1_revert_fork -- --ignored --nocapture
//! ```

mod v1_fork;

use alloy::primitives::{Address, Bytes, I256, U256};
use alloy::providers::Provider;
use alloy::rpc::json_rpc::RpcSend;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::{SolCall, SolError};
use serde_json::Value;

use perpcity_sdk::contracts::v1::{AdjustMakerParams, IBeacon, OpenTakerParams, Perp};
use perpcity_sdk::errors::decode::{revert_class, try_extract_revert};
use perpcity_sdk::{PerpCityError, RevertClass, TransactionError};

use v1_fork::{SepoliaFork, V1Fork, V1Market};

#[tokio::test]
#[ignore] // Requires `anvil` and an Arbitrum Sepolia endpoint.
async fn v1_time_and_beacon_reverts_decode_with_their_class() {
    let fork = SepoliaFork::start().await;
    let mut w = V1Fork::new(&fork);
    let m = w.build_w11_market().await;

    // A deadline one second before the latest block.
    let deadline = w.now().await - 1;
    let err = open_taker_revert(&w, &m, deadline).await;
    check(
        &err,
        Perp::DeadlineExpired::SELECTOR,
        "DeadlineExpired",
        RevertClass::Transient,
    );

    // The latest block is `latency()` + 1 s after the beacon's last update.
    let snapshot = rpc(&w, "evm_snapshot", ()).await;
    let latency = Perp::new(m.perp, &w.provider)
        .latency()
        .call()
        .await
        .unwrap();
    let updated_at = IBeacon::new(m.beacon, &w.provider)
        .index()
        .call()
        .await
        .unwrap()
        .updatedAt;
    let next = updated_at + U256::from(latency) + U256::from(1);
    rpc(&w, "evm_setNextBlockTimestamp", (next,)).await;
    rpc(&w, "evm_mine", ()).await;
    let deadline = w.deadline().await;
    let err = open_taker_revert(&w, &m, deadline).await;
    check(
        &err,
        Perp::StaleBeaconIndex::SELECTOR,
        "StaleBeaconIndex",
        RevertClass::Transient,
    );
    revert_to(&w, snapshot).await;

    // A signed beacon update to twice the top of the price domain
    // (`MAX_INDEX_X96` = 2^96 * 1e6).
    let snapshot = rpc(&w, "evm_snapshot", ()).await;
    let extreme = U256::from(2_000_000u64) << 96;
    w.push_index("update-extreme", m.verifier, m.beacon, extreme)
        .await;
    let deadline = w.deadline().await;
    let err = open_taker_revert(&w, &m, deadline).await;
    check(
        &err,
        Perp::PriceOutsideRange::SELECTOR,
        "PriceOutsideRange",
        RevertClass::BeaconFault,
    );
    // Not a size refusal: a 1-atom open and a maker reduce fail the same way.
    let tiny = Perp::openTakerCall {
        params: OpenTakerParams {
            holder: w.me,
            margin: v1_fork::TAKER_MARGIN,
            perpDelta: I256::ONE,
            amt1Limit: U256::MAX,
            maxTotalFee: U256::from(v1_fork::MAX_TOTAL_FEE),
            deadline,
        },
    };
    let err = call_revert(&w, m.perp, tiny.abi_encode()).await;
    check(
        &err,
        Perp::PriceOutsideRange::SELECTOR,
        "PriceOutsideRange",
        RevertClass::BeaconFault,
    );
    let reduce = Perp::adjustMakerCall {
        params: AdjustMakerParams {
            posId: m.maker_id,
            marginDelta: 0,
            liquidityDelta: -1_000_000,
            amt0Limit: U256::ZERO,
            amt1Limit: U256::ZERO,
            deadline,
        },
    };
    let err = call_revert(&w, m.perp, reduce.abi_encode()).await;
    check(
        &err,
        Perp::PriceOutsideRange::SELECTOR,
        "PriceOutsideRange",
        RevertClass::BeaconFault,
    );
    revert_to(&w, snapshot).await;

    // The market's own beacon gives no data: its code is gone.
    let snapshot = rpc(&w, "evm_snapshot", ()).await;
    rpc(&w, "anvil_setCode", (m.beacon, Bytes::new())).await;
    let deadline = w.deadline().await;
    let err = open_taker_revert(&w, &m, deadline).await;
    check(
        &err,
        Perp::BeaconIndexUnreadable::SELECTOR,
        "BeaconIndexUnreadable",
        RevertClass::BeaconFault,
    );
    revert_to(&w, snapshot).await;
}

/// The node's error for an `eth_call` of the W11 taker open with
/// `deadline`, from the dev wallet. The call must revert.
async fn open_taker_revert(w: &V1Fork, m: &V1Market, deadline: u64) -> String {
    let call = Perp::openTakerCall {
        params: OpenTakerParams {
            holder: w.me,
            margin: v1_fork::TAKER_MARGIN,
            perpDelta: I256::try_from(v1_fork::TAKER_SIZE).unwrap(),
            amt1Limit: U256::MAX,
            maxTotalFee: U256::from(v1_fork::MAX_TOTAL_FEE),
            deadline,
        },
    };
    call_revert(w, m.perp, call.abi_encode()).await
}

async fn call_revert(w: &V1Fork, to: Address, data: Vec<u8>) -> String {
    let tx = TransactionRequest::default()
        .from(w.me)
        .to(to)
        .input(Bytes::from(data).into());
    let err = w.provider.call(tx).await.expect_err("the call must revert");
    err.to_string()
}

/// The node's answer decodes, by the SDK's revert decode, to `name` with
/// `selector`, and the simulation error it makes has `class`.
fn check(err: &str, selector: [u8; 4], name: &str, class: RevertClass) {
    let (decoded, raw, data) =
        try_extract_revert(err).unwrap_or_else(|| panic!("no revert data in: {err}"));
    assert_eq!(decoded, name, "{err}");
    assert_eq!(raw, selector, "{err}");
    assert_eq!(data, None, "{name} has no arguments");
    assert_eq!(revert_class(raw), class, "{name}");
    let sim: PerpCityError = TransactionError::SimulationReverted {
        error_name: decoded,
        selector: raw.into(),
        revert_data: data,
    }
    .into();
    assert_eq!(sim.revert_class(), Some(class), "{name}");
    assert_eq!(sim.is_transient(), class.is_transient(), "{name}");
    println!("{name}: 0x{} {class:?}", alloy::hex::encode(raw));
}

async fn rpc<P: RpcSend>(w: &V1Fork, method: &'static str, params: P) -> Value {
    w.provider
        .raw_request(method.into(), params)
        .await
        .unwrap_or_else(|e| panic!("{method}: {e}"))
}

async fn revert_to(w: &V1Fork, snapshot: Value) {
    let ok = rpc(w, "evm_revert", (snapshot,)).await;
    assert_eq!(ok, Value::Bool(true), "evm_revert");
}
