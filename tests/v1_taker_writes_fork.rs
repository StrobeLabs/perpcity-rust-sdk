//! Integration test: the `v1.0.0` taker writes on an Anvil fork of Arbitrum
//! Sepolia (`v1_fork`). A new market gets one wide maker and three makers
//! on adjacent ranges above the pool price, and a protocol fee of 100 set
//! by the impersonated `ProtocolFeeManager` owner and synced to the
//! market. Then the SDK opens a taker whose swap crosses at least three
//! initialized ticks (its fill equals the local quote, the receipt has
//! `TicksCrossed`, and the protocol fee is charged), reduces it with
//! `reduceOnly`, refuses a reduce-only flip before the send (the chain
//! reverts the same call `ReduceOnlyViolated`), flips it to a short
//! without `reduceOnly`, and closes the short at the market. A new short
//! opens and closes at the market. With automine off, a stuck open and a
//! stuck adjustment are each replaced: same nonce, a new deadline of now
//! plus 600 s, a new signature, and the replacement mines. Last, an
//! under-margined taker gets the `v1.0.0` liquidation calldata, which
//! liquidates it.
//!
//! Requires `anvil` (from Foundry) and an Arbitrum Sepolia endpoint in
//! `SEPOLIA_FORK_URL` (default: the public RPC).
//!
//! ```bash
//! cargo test --test v1_taker_writes_fork -- --ignored --nocapture
//! ```

mod v1_fork;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::consensus::Transaction as _;
use alloy::eips::BlockId;
use alloy::hex;
use alloy::primitives::{Address, B256, I256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionReceipt;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{SolCall, SolError, SolEvent};
use perpcity_sdk::client::PositionRole;
use perpcity_sdk::contracts::v1::{
    AdjustTakerParams, IFees, MoveMakerParams, OpenMakerParams, Perp, SwapResult,
};
use perpcity_sdk::errors::decode::try_extract_revert;
use perpcity_sdk::{
    ChainReader, Era, ExactAdjustTakerParams, ExactOpenTakerParams, HftTransport, MaxTotalFee,
    NotReplaceableReason, PerpCityError, PerpClient, PerpDelta, TransactionError, TransportConfig,
    Urgency, UsdcAtoms, UsdcDelta, V1TakerAdjustment, V1TakerReplacement, ValidationError,
};
use serde_json::Value;
use tokio::time::sleep;

use v1_fork::{SepoliaFork, V1Fork};

/// The three adjacent ranges above the pool price (tick 39122 at the
/// index of 50), each with this liquidity, inside the wide maker's band.
const ADJACENT: [(i32, i32); 3] = [(39_150, 39_180), (39_180, 39_210), (39_210, 39_240)];
const ADJACENT_LIQUIDITY: u128 = 200_000_000;
/// Six more 30-tick bands above the adjacent ones, from tick 39,240, and
/// eight thin ones below tick 39,120 that a sell crosses one by one: each
/// boundary is an initialized tick that a trade in a new second walks.
const MORE_ABOVE: i32 = 6;
const THIN_BELOW: i32 = 8;
const THIN_LIQUIDITY: u128 = 20_000_000;
/// The quiet seconds before the flip, so it accrues over a long gap.
const QUIET: u64 = 300;
/// The range the wide maker moves to at the end, its liquidity there and
/// the margin it adds.
const WIDE: (i32, i32) = (33_000, 45_000);
const WIDE_LIQUIDITY: u128 = 100_000_000;
const WIDE_MARGIN: i128 = 1_000_000_000;
const OPEN_SIZE: i128 = 450_000;
const REDUCE_SIZE: i128 = 150_000;
const SHORT_SIZE: i128 = 120_000;
/// The protocol fee the test sets, in millionths.
const PROTOCOL_FEE: u64 = 100;

sol! {
    #[sol(rpc)]
    interface IProtocolFeeManager {
        function owner() external view returns (address);
        function protocolFee() external view returns (uint256);
        function setProtocolFee(uint256 newProtocolFee) external;
    }
}

fn wall_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn open_params(margin: u128, size: i128, amt1_limit: UsdcAtoms) -> ExactOpenTakerParams {
    ExactOpenTakerParams {
        margin: UsdcAtoms::new(margin),
        perp_delta: PerpDelta::new(size),
        amt1_limit,
    }
}

async fn input_of<P: Provider>(provider: &P, hash: B256) -> Vec<u8> {
    provider
        .get_transaction_by_hash(hash)
        .await
        .unwrap()
        .expect("the node has the transaction")
        .input()
        .to_vec()
}

/// The cap from the tagged contract views and the fixed 100-atom slack.
async fn expected_fee_cap(provider: &impl Provider, market: Address, volume: UsdcAtoms) -> U256 {
    let block = BlockId::number(provider.get_block_number().await.unwrap());
    let perp = Perp::new(market, provider);
    let modules = perp.modules().block(block).call().await.unwrap();
    let fees = IFees::new(modules.fees, provider);
    let swap_rate = fees.maxSwapFee().block(block).call().await.unwrap();
    let protocol_rate = perp.protocolFee().block(block).call().await.unwrap();
    let scale = U256::from(1_000_000);
    (U256::from(volume.atoms()) * (swap_rate + protocol_rate) + scale - U256::ONE) / scale
        + U256::from(100)
}

fn assert_swap_fees(swap: &SwapResult, cap: U256) {
    let charged = swap.lpFeeAmt + swap.protocolFeeAmt + swap.creatorFeeAmt + swap.insuranceFeeAmt;
    assert_eq!(swap.totalFeeAmt, I256::try_from(charged).unwrap());
    assert!(charged <= cap);
}

/// The initialized ticks a swap from `start` to `end` passes.
fn crossed(ticks: impl Iterator<Item = i32>, start: i32, end: i32) -> usize {
    let (low, high) = (start.min(end), start.max(end));
    ticks.filter(|t| *t > low && *t <= high).count()
}

fn ticks_crossed_event(receipt: &TransactionReceipt, perp: Address) -> Perp::TicksCrossed {
    receipt
        .inner
        .logs()
        .iter()
        .filter(|log| log.address() == perp)
        .find_map(|log| Perp::TicksCrossed::decode_log(&log.inner).ok())
        .expect("the receipt has TicksCrossed")
        .data
}

async fn raw(fork: &SepoliaFork, method: &str, params: Value) -> Value {
    let probe = ProviderBuilder::new().connect_http(fork.url.parse().unwrap());
    probe
        .raw_request::<_, Value>(method.to_string().into(), params)
        .await
        .unwrap()
}

/// Send `data` to `to` from an impersonated `from` and require it to mine
/// successfully (automine is on).
async fn send_as(fork: &SepoliaFork, from: Address, to: Address, data: Vec<u8>) {
    let probe = ProviderBuilder::new().connect_http(fork.url.parse().unwrap());
    raw(fork, "anvil_impersonateAccount", serde_json::json!([from])).await;
    raw(
        fork,
        "anvil_setBalance",
        serde_json::json!([from, "0xDE0B6B3A7640000"]),
    )
    .await;
    let hash: B256 = serde_json::from_value(
        raw(
            fork,
            "eth_sendTransaction",
            serde_json::json!([{
                "from": from,
                "to": to,
                "data": format!("0x{}", hex::encode(data)),
                "gas": "0x1E8480",
            }]),
        )
        .await,
    )
    .unwrap();
    raw(
        fork,
        "anvil_stopImpersonatingAccount",
        serde_json::json!([from]),
    )
    .await;
    let receipt = probe
        .get_transaction_receipt(hash)
        .await
        .unwrap()
        .expect("automine mined it");
    assert!(receipt.status(), "the impersonated send reverted");
}

#[tokio::test]
#[ignore] // Requires `anvil` and an Arbitrum Sepolia endpoint.
async fn v1_taker_writes_on_a_sepolia_fork() {
    let fork = SepoliaFork::start().await;
    let mut w = V1Fork::new(&fork);

    // ── The market: W11 steps 1-7, one wide maker, three adjacent ones ──
    w.check_deploy().await;
    w.mint_usdc().await;
    let verifier = w.create_verifier().await;
    let beacon = w.create_beacon(verifier).await;
    w.bind(verifier, beacon).await;
    w.push_index("update-1", verifier, beacon, v1_fork::index_1())
        .await;
    let perp = w.create_perp(beacon).await;
    w.approve_usdc(perp).await;
    let wide_maker = w.open_maker(perp).await;
    let raw_perp = Perp::new(perp, w.provider.clone());
    for (lower, upper) in ADJACENT {
        let params = OpenMakerParams {
            holder: w.me,
            margin: v1_fork::MAKER_MARGIN,
            tickLower: lower.try_into().unwrap(),
            tickUpper: upper.try_into().unwrap(),
            liquidity: ADJACENT_LIQUIDITY,
            maxAmt0In: U256::MAX,
            maxAmt1In: U256::MAX,
            deadline: w.deadline().await,
        };
        w.send("open-adjacent-maker", raw_perp.openMaker(params))
            .await;
    }
    for _ in 0..4 {
        w.mint_usdc().await;
    }
    let above = (0..MORE_ABOVE).map(|i| (39_240 + 30 * i, 39_270 + 30 * i, ADJACENT_LIQUIDITY));
    let below = (0..THIN_BELOW).map(|i| (39_090 - 30 * i, 39_120 - 30 * i, THIN_LIQUIDITY));
    for (lower, upper, liquidity) in above.chain(below) {
        let params = OpenMakerParams {
            holder: w.me,
            margin: v1_fork::MAKER_MARGIN,
            tickLower: lower.try_into().unwrap(),
            tickUpper: upper.try_into().unwrap(),
            liquidity,
            maxAmt0In: U256::MAX,
            maxAmt1In: U256::MAX,
            deadline: w.deadline().await,
        };
        let hash = w
            .send("open-band-maker", raw_perp.openMaker(params))
            .await
            .transaction_hash;
        v1_fork::measure_gas(&w.provider, hash, &format!("band-maker-open-{lower}")).await;
    }
    println!("v1 market {perp}: {} makers", 4 + MORE_ABOVE + THIN_BELOW);

    // ── A nonzero protocol fee: set by the owner, synced to the market ──
    let fee_manager = raw_perp.protocolFeeManager().call().await.unwrap();
    let manager = IProtocolFeeManager::new(fee_manager, w.provider.clone());
    let owner = manager.owner().call().await.unwrap();
    send_as(
        &fork,
        owner,
        fee_manager,
        IProtocolFeeManager::setProtocolFeeCall {
            newProtocolFee: U256::from(PROTOCOL_FEE),
        }
        .abi_encode(),
    )
    .await;
    assert_eq!(
        manager.protocolFee().call().await.unwrap(),
        U256::from(PROTOCOL_FEE)
    );
    w.send("sync-protocol-fee", raw_perp.syncProtocolFee())
        .await;
    assert_eq!(
        raw_perp.protocolFee().call().await.unwrap(),
        U256::from(PROTOCOL_FEE)
    );

    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&fork.url)
            .build()
            .unwrap(),
    )
    .unwrap();
    let chain = ChainReader::arbitrum_sepolia(transport)
        .checked()
        .await
        .unwrap();
    let signer: PrivateKeySigner = w.signer.clone();
    let client = PerpClient::new(chain.market(perp), signer);
    client.sync_nonce().await.unwrap();
    assert_eq!(client.market().era().await.unwrap(), Era::V1);
    let rates = client.market().get_taker_fee_rates().await.unwrap();
    assert_eq!(u64::from(rates.protocol_fee.e6()), PROTOCOL_FEE);
    assert_eq!(
        u64::try_from(manager.protocolFee().call().await.unwrap()).unwrap(),
        PROTOCOL_FEE,
        "the manager's fee"
    );
    assert_eq!(
        u64::try_from(raw_perp.protocolFee().call().await.unwrap()).unwrap(),
        PROTOCOL_FEE,
        "the perp's synced fee"
    );

    // ── Open: a swap across three initialized ticks, equal to the quote ──
    let pool = client
        .market()
        .state_at_head()
        .await
        .unwrap()
        .pool()
        .await
        .unwrap();
    let quote = pool.quote_perp(PerpDelta::new(OPEN_SIZE)).unwrap();
    println!(
        "quote: {} perp for {} USDC, ticks crossed {:?}",
        quote.perp_delta, quote.usd_delta, quote.ticks_crossed
    );
    assert!(quote.ticks_crossed.len() >= 3, "{:?}", quote.ticks_crossed);
    let amt1_limit = quote.amt1_limit(100);
    let expected_cap = expected_fee_cap(&w.provider, perp, amt1_limit).await;
    client.chain().refresh_gas().await.unwrap();
    let before = wall_secs();
    let opened = client
        .open_taker_v1(
            &open_params(10_000_000, OPEN_SIZE, amt1_limit),
            MaxTotalFee::ForVolume(amt1_limit),
            Urgency::Normal,
        )
        .await
        .unwrap();
    let after = wall_secs();
    let taker = opened.pos_id;
    println!(
        "taker {taker} opened in {}: {:?}",
        opened.tx_hash, opened.event.sr
    );
    let (perp_atoms, usd_atoms) = unpack(opened.event.sr.delta);
    assert_eq!(
        perp_atoms,
        quote.perp_delta.atoms(),
        "fill equals the quote"
    );
    assert_eq!(usd_atoms, quote.usd_delta.atoms(), "cost equals the quote");
    let receipt = client.poll_receipt(opened.tx_hash).await.unwrap();
    v1_fork::measure_gas(&w.provider, opened.tx_hash, "taker-open").await;
    let event = ticks_crossed_event(&receipt, perp);
    let start: i32 = event.startingTick.as_i32();
    let end: i32 = event.endingTick.as_i32();
    assert!(!event.zeroForOne, "a buy");
    assert_eq!(
        crossed(pool.ticks.keys().copied(), start, end),
        quote.ticks_crossed.len()
    );
    assert_eq!(end, quote.tick_after);

    let sent = Perp::openTakerCall::abi_decode(&input_of(&w.provider, opened.tx_hash).await)
        .unwrap()
        .params;
    assert_eq!(sent.maxTotalFee, expected_cap);
    assert!((before + 600..=after + 600).contains(&sent.deadline));
    assert_eq!(sent.amt1Limit, U256::from(amt1_limit.atoms()));
    assert_swap_fees(&opened.event.sr, sent.maxTotalFee);
    assert!(
        opened.event.sr.feeFundFeeAmt > U256::ZERO,
        "the fixture credits the fee fund"
    );
    assert!(
        opened.event.sr.protocolFeeAmt > U256::ZERO,
        "the synced protocol fee is charged"
    );
    let charged = u128::try_from(opened.event.sr.totalFeeAmt).unwrap();
    assert_eq!(opened.margin, Some(UsdcAtoms::new(10_000_000 - charged)));

    // ── Adjust: a reduce-only partial sell ──────────────────────────
    let state = client.market().state_at_head().await.unwrap();
    let sell = state
        .pool()
        .await
        .unwrap()
        .quote_perp(PerpDelta::new(-REDUCE_SIZE))
        .unwrap();
    let volume = UsdcAtoms::new(sell.usd_delta.magnitude().atoms() * 101 / 100);
    let expected_cap = expected_fee_cap(&w.provider, perp, volume).await;
    client.chain().refresh_gas().await.unwrap();
    let reduced = client
        .adjust_taker_v1(
            &ExactAdjustTakerParams {
                pos_id: taker,
                margin_delta: UsdcDelta::ZERO,
                perp_delta: PerpDelta::new(-REDUCE_SIZE),
                amt1_limit: sell.amt1_limit(100),
            },
            MaxTotalFee::ForVolume(volume),
            true,
            Urgency::Normal,
        )
        .await
        .unwrap();
    let V1TakerAdjustment::Adjusted(reduced) = reduced else {
        panic!("a partial reduce leaves the position open: {reduced:?}");
    };
    assert_eq!(unpack(reduced.event.delta).0, OPEN_SIZE - REDUCE_SIZE);
    v1_fork::measure_gas(&w.provider, reduced.tx_hash, "taker-adjust").await;
    let sent = Perp::adjustTakerCall::abi_decode(&input_of(&w.provider, reduced.tx_hash).await)
        .unwrap()
        .params;
    assert!(sent.reduceOnly);
    assert!(sent.deadline >= after + 600);
    assert_eq!(sent.maxTotalFee, expected_cap);
    assert_swap_fees(&reduced.event.sr, sent.maxTotalFee);

    // ── The flip refusal: the SDK before the send, the chain on the call ─
    let left = OPEN_SIZE - REDUCE_SIZE;
    let nonce = w.provider.get_transaction_count(w.me).await.unwrap();
    let flip = ExactAdjustTakerParams {
        pos_id: taker,
        margin_delta: UsdcDelta::ZERO,
        perp_delta: PerpDelta::new(-2 * left),
        amt1_limit: UsdcAtoms::ZERO,
    };
    let err = client
        .adjust_taker_v1(
            &flip,
            MaxTotalFee::Exact(UsdcAtoms::new(v1_fork::MAX_TOTAL_FEE.into())),
            true,
            Urgency::Normal,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            PerpCityError::Validation(ValidationError::ReduceOnlyViolated { position, perp_delta })
                if position == left && perp_delta == -2 * left
        ),
        "{err}"
    );
    assert_eq!(
        w.provider.get_transaction_count(w.me).await.unwrap(),
        nonce,
        "nothing was sent"
    );
    let chain_err = raw_perp
        .adjustTaker(AdjustTakerParams {
            posId: taker,
            marginDelta: 0,
            perpDelta: I256::try_from(-2 * left).unwrap(),
            amt1Limit: U256::ZERO,
            maxTotalFee: U256::from(v1_fork::MAX_TOTAL_FEE),
            reduceOnly: true,
            deadline: w.deadline().await,
        })
        .from(w.me)
        .call()
        .await
        .err()
        .expect("the chain refuses the flip");
    assert_eq!(
        try_extract_revert(&chain_err.to_string()).unwrap().1,
        Perp::ReduceOnlyViolated::SELECTOR
    );

    // ── A permitted flip after a quiet gap: the same change without
    // reduce-only, across the thin bands, in a new second ───────────
    w.warp(QUIET).await;
    raw(&fork, "evm_mine", serde_json::json!([])).await;
    client.chain().refresh_gas().await.unwrap();
    let flipped = client
        .adjust_taker_v1(
            &flip,
            MaxTotalFee::Exact(UsdcAtoms::new(v1_fork::MAX_TOTAL_FEE.into())),
            false,
            Urgency::Normal,
        )
        .await
        .unwrap();
    let V1TakerAdjustment::Adjusted(flipped) = flipped else {
        panic!("a flip leaves the position open: {flipped:?}");
    };
    assert_eq!(unpack(flipped.event.delta).0, -left, "now a short");
    let gas = v1_fork::measure_gas(&w.provider, flipped.tx_hash, "taker-flip").await;
    let flip_ticks =
        ticks_crossed_event(&client.poll_receipt(flipped.tx_hash).await.unwrap(), perp);
    let band = raw_perp.utilizationBand().call().await.unwrap();
    let tick_of =
        |sqrt_x96: u128| (2.0 * (sqrt_x96 as f64 / 2f64.powi(96)).ln() / 1.0001f64.ln()) as i32;
    let (band_lower, band_upper) = (tick_of(band.lower), tick_of(band.upper));
    let in_band = pool
        .ticks
        .keys()
        .filter(|tick| (band_lower..=band_upper).contains(*tick))
        .count();
    println!(
        "flip after {QUIET} s from tick {} to {}, {in_band} initialized ticks in the utilization band {band_lower}..{band_upper}: used {}, sent limit {}",
        flip_ticks.startingTick.as_i32(),
        flip_ticks.endingTick.as_i32(),
        gas.used,
        gas.sent
    );
    assert!(in_band >= 12, "the band holds the extra makers' ticks");
    let sent = Perp::adjustTakerCall::abi_decode(&input_of(&w.provider, flipped.tx_hash).await)
        .unwrap()
        .params;
    assert!(!sent.reduceOnly);
    assert_eq!(
        client.market().get_position(taker).await.unwrap().perp,
        PerpDelta::new(-left)
    );
    println!("taker {taker} flipped to {} in {}", -left, flipped.tx_hash);

    // ── Close the short at the market: a reduce-only buy ────────────
    let buyback = client
        .market()
        .state_at_head()
        .await
        .unwrap()
        .pool()
        .await
        .unwrap()
        .quote_perp(PerpDelta::new(left))
        .unwrap();
    let volume = UsdcAtoms::new(buyback.usd_delta.magnitude().atoms() * 101 / 100);
    let expected_cap = expected_fee_cap(&w.provider, perp, volume).await;
    client.chain().refresh_gas().await.unwrap();
    let closed = client
        .close_taker_v1(taker, None, MaxTotalFee::ForVolume(volume), Urgency::Normal)
        .await
        .unwrap();
    assert_eq!(closed.pos_id, taker);
    v1_fork::measure_gas(&w.provider, closed.tx_hash, "taker-close").await;
    assert_eq!(closed.margin, Some(UsdcAtoms::ZERO));
    assert_eq!(
        raw_perp.positionOwner(taker).call().await.unwrap(),
        Address::ZERO
    );
    let sent = Perp::adjustTakerCall::abi_decode(&input_of(&w.provider, closed.tx_hash).await)
        .unwrap()
        .params;
    assert!(sent.reduceOnly);
    assert_eq!(sent.perpDelta, I256::try_from(left).unwrap());
    assert_eq!(sent.amt1Limit, U256::from(u128::MAX), "any cost for a buy");
    assert_eq!(sent.maxTotalFee, expected_cap);
    assert_swap_fees(&closed.event.sr, sent.maxTotalFee);
    println!("taker {taker} closed in {}", closed.tx_hash);

    // ── A new short, opened and closed at the market ────────────────
    client.chain().refresh_gas().await.unwrap();
    let fee = MaxTotalFee::Exact(UsdcAtoms::new(v1_fork::MAX_TOTAL_FEE.into()));
    let short = client
        .open_taker_v1(
            &open_params(10_000_000, -SHORT_SIZE, UsdcAtoms::ZERO),
            fee,
            Urgency::Normal,
        )
        .await
        .unwrap();
    assert_eq!(unpack(short.event.sr.delta).0, -SHORT_SIZE);
    v1_fork::measure_gas(&w.provider, short.tx_hash, "taker-open-short").await;
    client.chain().refresh_gas().await.unwrap();
    let closed = client
        .close_taker_v1(short.pos_id, None, fee, Urgency::Normal)
        .await
        .unwrap();
    assert_eq!(closed.pos_id, short.pos_id);
    v1_fork::measure_gas(&w.provider, closed.tx_hash, "taker-close-short").await;
    let sent = Perp::adjustTakerCall::abi_decode(&input_of(&w.provider, closed.tx_hash).await)
        .unwrap()
        .params;
    assert!(sent.reduceOnly);
    assert_eq!(sent.perpDelta, I256::try_from(SHORT_SIZE).unwrap());
    assert_eq!(sent.amt1Limit, U256::from(u128::MAX));
    println!("short {} opened and closed at the market", short.pos_id);

    // ── A long sold back across the ticks it bought through, right after
    // a reduce: a multi-tick close in the block after the last touch ──
    let pool = client
        .market()
        .state_at_head()
        .await
        .unwrap()
        .pool()
        .await
        .unwrap();
    let quote = pool.quote_perp(PerpDelta::new(OPEN_SIZE)).unwrap();
    assert!(quote.ticks_crossed.len() >= 3, "{:?}", quote.ticks_crossed);
    client.chain().refresh_gas().await.unwrap();
    let long = client
        .open_taker_v1(
            &open_params(10_000_000, OPEN_SIZE, quote.amt1_limit(100)),
            fee,
            Urgency::Normal,
        )
        .await
        .unwrap();
    v1_fork::measure_gas(&w.provider, long.tx_hash, "taker-open-long").await;
    client.chain().refresh_gas().await.unwrap();
    let reduced = client
        .adjust_taker_v1(
            &ExactAdjustTakerParams {
                pos_id: long.pos_id,
                margin_delta: UsdcDelta::ZERO,
                perp_delta: PerpDelta::new(-REDUCE_SIZE),
                amt1_limit: UsdcAtoms::ZERO,
            },
            fee,
            true,
            Urgency::Normal,
        )
        .await
        .unwrap();
    let V1TakerAdjustment::Adjusted(reduced) = reduced else {
        panic!("a partial reduce leaves the position open: {reduced:?}");
    };
    v1_fork::measure_gas(&w.provider, reduced.tx_hash, "taker-reduce-long").await;
    let sell = client
        .market()
        .state_at_head()
        .await
        .unwrap()
        .pool()
        .await
        .unwrap()
        .quote_perp(PerpDelta::new(REDUCE_SIZE - OPEN_SIZE))
        .unwrap();
    println!(
        "long {} sells {} perp back across ticks {:?}",
        long.pos_id,
        OPEN_SIZE - REDUCE_SIZE,
        sell.ticks_crossed
    );
    assert!(sell.ticks_crossed.len() >= 3, "{:?}", sell.ticks_crossed);
    client.chain().refresh_gas().await.unwrap();
    let closed = client
        .close_taker_v1(long.pos_id, None, fee, Urgency::Normal)
        .await
        .unwrap();
    assert_eq!(closed.pos_id, long.pos_id);
    let sent = Perp::adjustTakerCall::abi_decode(&input_of(&w.provider, closed.tx_hash).await)
        .unwrap()
        .params;
    assert_eq!(
        sent.perpDelta,
        I256::try_from(REDUCE_SIZE - OPEN_SIZE).unwrap()
    );
    v1_fork::measure_gas(&w.provider, closed.tx_hash, "taker-close-long").await;

    // ── The deadline bump: a stuck open, replaced ───────────────────
    raw(&fork, "evm_setAutomine", serde_json::json!([false])).await;
    client.chain().refresh_gas().await.unwrap();
    let small = UsdcAtoms::new(6_000_000);
    let expected_cap = expected_fee_cap(&w.provider, perp, small).await;
    let err = client
        .open_taker_v1(
            &open_params(10_000_000, 100_000, small),
            MaxTotalFee::ForVolume(small),
            Urgency::Normal,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            PerpCityError::Transaction(TransactionError::ReceiptTimeout { .. })
        ),
        "{err}"
    );
    let stuck = err.tx_hash().unwrap();
    let stuck_tx = w
        .provider
        .get_transaction_by_hash(stuck)
        .await
        .unwrap()
        .unwrap();
    let stuck_call = Perp::openTakerCall::abi_decode(stuck_tx.input())
        .unwrap()
        .params;
    sleep(Duration::from_secs(2)).await;
    // The receipt timeout outlived the gas cache; a replacement, like a
    // send, needs fresh gas data.
    client.chain().refresh_gas().await.unwrap();
    let replaced_at = wall_secs();
    let (replaced, _) = tokio::join!(client.replace_v1_taker(stuck, Urgency::High), async {
        sleep(Duration::from_secs(3)).await;
        raw(&fork, "evm_mine", serde_json::json!([])).await;
    });
    let V1TakerReplacement::Opened(replaced) = replaced.unwrap() else {
        panic!("an open replaced as an open");
    };
    assert_ne!(replaced.tx_hash, stuck, "the replacement mined");
    v1_fork::measure_gas(&w.provider, replaced.tx_hash, "taker-open-replacement").await;
    let replacement_tx = w
        .provider
        .get_transaction_by_hash(replaced.tx_hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replacement_tx.nonce(), stuck_tx.nonce(), "same nonce");
    let old_max = stuck_tx.max_fee_per_gas();
    let old_priority = stuck_tx.max_priority_fee_per_gas().unwrap();
    assert!(replacement_tx.max_fee_per_gas() >= old_max + old_max.div_ceil(8));
    assert!(
        replacement_tx.max_priority_fee_per_gas().unwrap()
            >= old_priority + old_priority.div_ceil(8)
    );
    assert_ne!(
        replacement_tx.input(),
        stuck_tx.input(),
        "never the old calldata"
    );
    assert_ne!(
        replacement_tx.inner.signature(),
        stuck_tx.inner.signature(),
        "a new signature"
    );
    let renewed = Perp::openTakerCall::abi_decode(replacement_tx.input())
        .unwrap()
        .params;
    assert!(renewed.deadline > stuck_call.deadline);
    assert!(
        (replaced_at + 600..=wall_secs() + 600).contains(&renewed.deadline),
        "deadline {} is now + 600 s",
        renewed.deadline
    );
    assert_eq!(renewed.perpDelta, stuck_call.perpDelta);
    assert_eq!(renewed.maxTotalFee, stuck_call.maxTotalFee);
    assert_eq!(renewed.maxTotalFee, expected_cap);
    assert_swap_fees(&replaced.event.sr, renewed.maxTotalFee);
    assert!(
        w.provider
            .get_transaction_receipt(stuck)
            .await
            .unwrap()
            .is_none(),
        "the stuck transaction did not mine"
    );
    println!(
        "stuck {stuck} replaced by {} (nonce {})",
        replaced.tx_hash,
        replacement_tx.nonce()
    );

    // ── The deadline bump: a stuck reduce-only adjustment, replaced ──
    client.sync_nonce().await.unwrap();
    client.chain().refresh_gas().await.unwrap();
    let err = client
        .adjust_taker_v1(
            &ExactAdjustTakerParams {
                pos_id: replaced.pos_id,
                margin_delta: UsdcDelta::ZERO,
                perp_delta: PerpDelta::new(-40_000),
                amt1_limit: UsdcAtoms::ZERO,
            },
            MaxTotalFee::Exact(UsdcAtoms::new(v1_fork::MAX_TOTAL_FEE.into())),
            true,
            Urgency::Normal,
        )
        .await
        .unwrap_err();
    assert!(err.is_unconfirmed(), "{err}");
    let stuck = err.tx_hash().unwrap();
    let stuck_tx = w
        .provider
        .get_transaction_by_hash(stuck)
        .await
        .unwrap()
        .unwrap();
    let stuck_call = Perp::adjustTakerCall::abi_decode(stuck_tx.input())
        .unwrap()
        .params;
    sleep(Duration::from_secs(2)).await;
    client.chain().refresh_gas().await.unwrap();
    let (replaced_adjustment, _) =
        tokio::join!(client.replace_v1_taker(stuck, Urgency::High), async {
            sleep(Duration::from_secs(3)).await;
            raw(&fork, "evm_mine", serde_json::json!([])).await;
        });
    let V1TakerReplacement::Adjustment(V1TakerAdjustment::Adjusted(adjusted)) =
        replaced_adjustment.unwrap()
    else {
        panic!("an adjustment replaced as an adjustment");
    };
    assert_ne!(adjusted.tx_hash, stuck, "the replacement mined");
    v1_fork::measure_gas(&w.provider, adjusted.tx_hash, "taker-adjust-replacement").await;
    assert_eq!(adjusted.pos_id, replaced.pos_id);
    let replacement_tx = w
        .provider
        .get_transaction_by_hash(adjusted.tx_hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replacement_tx.nonce(), stuck_tx.nonce(), "same nonce");
    let renewed = Perp::adjustTakerCall::abi_decode(replacement_tx.input())
        .unwrap()
        .params;
    assert!(renewed.deadline > stuck_call.deadline);
    assert!(renewed.reduceOnly);
    assert_eq!(renewed.perpDelta, stuck_call.perpDelta);
    assert_eq!(unpack(adjusted.event.delta).0, 100_000 - 40_000);
    println!("stuck adjustment {stuck} replaced by {}", adjusted.tx_hash);
    // ── S -> R1 -> R2: replace the replacement that hash_to_replace names ──
    client.sync_nonce().await.unwrap();
    client.chain().refresh_gas().await.unwrap();
    let err = client
        .open_taker_v1(
            &open_params(10_000_000, 100_000, small),
            MaxTotalFee::ForVolume(small),
            Urgency::Normal,
        )
        .await
        .unwrap_err();
    let s = err.hash_to_replace().unwrap();
    assert_eq!(err.tx_hash(), Some(s));
    let s_tx = w
        .provider
        .get_transaction_by_hash(s)
        .await
        .unwrap()
        .unwrap();
    client.chain().refresh_gas().await.unwrap();
    let err1 = client.replace_v1_taker(s, Urgency::High).await.unwrap_err();
    let PerpCityError::Transaction(TransactionError::ReplacementUnconfirmed {
        original,
        nonce,
        replacement: Some(r1),
        refused: false,
        ..
    }) = &err1
    else {
        panic!("R1 taken and not mined: {err1:?}");
    };
    assert_eq!(*original, s);
    assert_eq!(*nonce, Some(s_tx.nonce()));
    assert!(err1.is_unconfirmed() && err1.is_transient(), "{err1}");
    assert_eq!(err1.hash_to_replace(), Some(*r1), "replace R1 next");
    let r1 = *r1;
    // The node dropped S for R1, so S is no longer a hash to replace.
    client.chain().refresh_gas().await.unwrap();
    let again = client.replace_v1_taker(s, Urgency::High).await.unwrap_err();
    assert!(
        matches!(
            again,
            PerpCityError::Transaction(TransactionError::NotReplaceable {
                tx_hash,
                reason: NotReplaceableReason::UnknownToNode,
                ..
            }) if tx_hash == s
        ),
        "{again:?}"
    );
    assert!(again.is_unconfirmed());
    let r1_tx = w
        .provider
        .get_transaction_by_hash(r1)
        .await
        .unwrap()
        .unwrap();
    client.chain().refresh_gas().await.unwrap();
    let (r2, _) = tokio::join!(client.replace_v1_taker(r1, Urgency::High), async {
        sleep(Duration::from_secs(3)).await;
        raw(&fork, "evm_mine", serde_json::json!([])).await;
    });
    let V1TakerReplacement::Opened(r2) = r2.unwrap() else {
        panic!("an open replaced as an open");
    };
    let r2_tx = w
        .provider
        .get_transaction_by_hash(r2.tx_hash)
        .await
        .unwrap()
        .unwrap();
    let deadline = |tx: &alloy::rpc::types::Transaction| {
        Perp::openTakerCall::abi_decode(tx.input())
            .unwrap()
            .params
            .deadline
    };
    for (earlier, later) in [(&s_tx, &r1_tx), (&r1_tx, &r2_tx)] {
        assert_eq!(later.nonce(), s_tx.nonce(), "one nonce");
        assert!(later.max_fee_per_gas() > earlier.max_fee_per_gas());
        assert!(later.max_priority_fee_per_gas() > earlier.max_priority_fee_per_gas());
        assert!(deadline(later) > deadline(earlier));
    }
    for (hash, mined) in [(s, false), (r1, false), (r2.tx_hash, true)] {
        let receipt = w.provider.get_transaction_receipt(hash).await.unwrap();
        assert_eq!(receipt.map(|r| r.status()), mined.then_some(true), "{hash}");
    }
    // After R2 mined, neither earlier hash is known: nothing to replace.
    for hash in [s, r1] {
        client.chain().refresh_gas().await.unwrap();
        let late = client
            .replace_v1_taker(hash, Urgency::High)
            .await
            .unwrap_err();
        assert!(
            matches!(
                late,
                PerpCityError::Transaction(TransactionError::NotReplaceable {
                    reason: NotReplaceableReason::UnknownToNode,
                    ..
                })
            ),
            "{late:?}"
        );
        assert_eq!(late.hash_to_replace(), None);
    }
    println!("S {s} replaced by R1 {r1}, then by R2 {}", r2.tx_hash);
    raw(&fork, "evm_setAutomine", serde_json::json!([true])).await;

    // ── Liquidation: an under-margined taker ────────────────────────
    client.chain().refresh_gas().await.unwrap();
    let lev = client
        .open_taker_v1(
            &open_params(5_200_000, 400_000, UsdcAtoms::new(u128::MAX)),
            MaxTotalFee::Exact(UsdcAtoms::new(v1_fork::MAX_TOTAL_FEE.into())),
            Urgency::Normal,
        )
        .await
        .unwrap();
    // A new provider: the first one's nonce filler cached a nonce that the
    // SDK's sends from the same wallet have used since.
    let mut w = V1Fork::new(&fork);
    w.push_index("update-low", verifier, beacon, U256::from(38u64) << 96)
        .await;
    let state = client.market().state_at_head().await.unwrap();
    let preview = state.preview_position(lev.pos_id).await.unwrap().unwrap();
    println!(
        "under-margined taker {}: equity {}, value {}, liquidatable {}",
        lev.pos_id, preview.equity, preview.value, preview.liquidatable
    );
    assert!(preview.liquidatable);
    let calldata = client
        .market()
        .liquidation_calldata(PositionRole::Taker, lev.pos_id, w.me)
        .await
        .unwrap();
    assert_eq!(
        calldata.as_ref(),
        Perp::liquidateTakerCall {
            posId: lev.pos_id,
            liquidationFeeRecipient: w.me,
            perpAmount: 400_000,
        }
        .abi_encode()
        .as_slice()
    );
    // The index push went from the same wallet outside the SDK.
    client.sync_nonce().await.unwrap();
    client.chain().refresh_gas().await.unwrap();
    let receipt = client
        .liquidate_taker(lev.pos_id, w.me, Urgency::Normal)
        .await
        .unwrap();
    let liquidated = receipt
        .inner
        .logs()
        .iter()
        .find_map(|log| Perp::TakerLiquidated::decode_log(&log.inner).ok())
        .expect("the receipt has TakerLiquidated")
        .data;
    assert_eq!(liquidated.posId, lev.pos_id);
    println!(
        "taker {} liquidated in {}",
        lev.pos_id, receipt.transaction_hash
    );

    // ── The wide maker moved wider after a quiet gap, sent by the SDK ──
    // A new provider again: the liquidation used a nonce of the same wallet.
    let mut w = V1Fork::new(&fork);
    w.mint_usdc().await;
    w.mint_usdc().await;
    w.warp(QUIET).await;
    raw(&fork, "evm_mine", serde_json::json!([])).await;
    client.sync_nonce().await.unwrap();
    client.chain().refresh_gas().await.unwrap();
    let move_wide = Perp::moveMakerCall {
        params: MoveMakerParams {
            posId: wide_maker,
            tickLower: WIDE.0.try_into().unwrap(),
            tickUpper: WIDE.1.try_into().unwrap(),
            liquidity: WIDE_LIQUIDITY,
            removeAmt0Min: U256::ZERO,
            removeAmt1Min: U256::ZERO,
            addAmt0Max: U256::MAX,
            addAmt1Max: U256::MAX,
            marginDelta: WIDE_MARGIN,
            deadline: w.deadline().await,
        },
    };
    let moved = client
        .tx(perp, move_wide.abi_encode().into())
        .send()
        .await
        .unwrap();
    let gas =
        v1_fork::measure_gas(&w.provider, moved.transaction_hash, "maker-move-wide-bands").await;
    println!(
        "wide maker {wide_maker} moved to {WIDE:?} after {QUIET} s: used {}, sent limit {}",
        gas.used, gas.sent
    );
    let band = raw_perp.makerDetails(wide_maker).call().await.unwrap();
    assert_eq!((band.tickLower.as_i32(), band.tickUpper.as_i32()), WIDE);
    println!("\n=== v1 taker writes fork test passed ===");
}

/// `(amount0, amount1)` of a packed `BalanceDelta`.
fn unpack(delta: I256) -> (i128, i128) {
    let raw: U256 = delta.into_raw();
    let amount0 = (raw >> 128usize).to::<u128>() as i128;
    let amount1 = (raw & U256::from(u128::MAX)).to::<u128>() as i128;
    (amount0, amount1)
}
