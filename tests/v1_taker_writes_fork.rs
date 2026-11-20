//! Integration test: the `v1.0.0` taker writes on an Anvil fork of Arbitrum
//! Sepolia (`v1_fork`). A new market gets one wide maker and three makers
//! on adjacent ranges above the pool price; then the SDK opens a taker
//! whose swap crosses at least three initialized ticks (its fill equals the
//! local quote and the receipt has `TicksCrossed`), reduces it with
//! `reduceOnly`, refuses a reduce-only flip before the send (the chain
//! reverts the same call `ReduceOnlyViolated`), and closes it. With
//! automine off, a stuck open is replaced: same nonce, a new deadline of
//! now plus 600 s, a new signature, and the replacement mines. Last, an
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
use alloy::primitives::{Address, I256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionReceipt;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::{SolCall, SolError, SolEvent};
use perpcity_sdk::client::PositionRole;
use perpcity_sdk::contracts::v1::{AdjustTakerParams, OpenMakerParams, Perp};
use perpcity_sdk::errors::decode::try_extract_revert;
use perpcity_sdk::{
    ChainReader, Era, ExactAdjustTakerParams, ExactOpenTakerParams, HftTransport,
    MAX_TOTAL_FEE_SLACK, MaxTotalFee, PerpCityError, PerpClient, PerpDelta, TransactionError,
    TransportConfig, Urgency, UsdcAtoms, UsdcDelta, V1TakerAdjustment, V1TakerReplacement,
    ValidationError,
};
use serde_json::Value;
use tokio::time::sleep;

use v1_fork::{SepoliaFork, V1Fork};

/// The three adjacent ranges above the pool price (tick 39122 at the
/// index of 50), each with this liquidity, inside the wide maker's band.
const ADJACENT: [(i32, i32); 3] = [(39_150, 39_180), (39_180, 39_210), (39_210, 39_240)];
const ADJACENT_LIQUIDITY: u128 = 200_000_000;
const OPEN_SIZE: i128 = 450_000;
const REDUCE_SIZE: i128 = 150_000;

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

async fn input_of<P: Provider>(provider: &P, hash: alloy::primitives::B256) -> Vec<u8> {
    provider
        .get_transaction_by_hash(hash)
        .await
        .unwrap()
        .expect("the node has the transaction")
        .input()
        .to_vec()
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
    let probe = alloy::providers::ProviderBuilder::new().connect_http(fork.url.parse().unwrap());
    probe
        .raw_request::<_, Value>(method.to_string().into(), params)
        .await
        .unwrap()
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
    w.open_maker(perp).await;
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
    println!("v1 market {perp}: four makers");

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
    println!("fee rates: {rates:?}");

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
    assert_eq!(
        sent.maxTotalFee,
        U256::from(
            rates
                .max_total_fee(amt1_limit, MAX_TOTAL_FEE_SLACK)
                .unwrap()
                .atoms()
        )
    );
    assert!((before + 600..=after + 600).contains(&sent.deadline));
    assert_eq!(sent.amt1Limit, U256::from(amt1_limit.atoms()));
    assert!(opened.event.sr.totalFeeAmt <= I256::try_from(sent.maxTotalFee).unwrap());

    // ── Adjust: a reduce-only partial sell ──────────────────────────
    let state = client.market().state_at_head().await.unwrap();
    let sell = state
        .pool()
        .await
        .unwrap()
        .quote_perp(PerpDelta::new(-REDUCE_SIZE))
        .unwrap();
    let volume = UsdcAtoms::new(sell.usd_delta.magnitude().atoms() * 101 / 100);
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
    let sent = Perp::adjustTakerCall::abi_decode(&input_of(&w.provider, reduced.tx_hash).await)
        .unwrap()
        .params;
    assert!(sent.reduceOnly);
    assert!(sent.deadline >= after + 600);

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

    // ── Close: reduce-only, the whole perp amount ───────────────────
    let buyback = client
        .market()
        .state_at_head()
        .await
        .unwrap()
        .pool()
        .await
        .unwrap()
        .quote_perp(PerpDelta::new(-left))
        .unwrap();
    let volume = UsdcAtoms::new(buyback.usd_delta.magnitude().atoms() * 101 / 100);
    client.chain().refresh_gas().await.unwrap();
    let closed = client
        .close_taker_v1(taker, None, MaxTotalFee::ForVolume(volume), Urgency::Normal)
        .await
        .unwrap();
    assert_eq!(closed.pos_id, taker);
    assert_eq!(closed.margin, Some(UsdcAtoms::ZERO));
    assert_eq!(
        raw_perp.positionOwner(taker).call().await.unwrap(),
        Address::ZERO
    );
    let sent = Perp::adjustTakerCall::abi_decode(&input_of(&w.provider, closed.tx_hash).await)
        .unwrap()
        .params;
    assert!(sent.reduceOnly);
    assert_eq!(sent.perpDelta, I256::try_from(-left).unwrap());
    println!("taker {taker} closed in {}", closed.tx_hash);

    // ── The deadline bump: a stuck open, replaced ───────────────────
    raw(&fork, "evm_setAutomine", serde_json::json!([false])).await;
    client.chain().refresh_gas().await.unwrap();
    let small = UsdcAtoms::new(6_000_000);
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
    let replaced_at = wall_secs();
    let (replaced, _) = tokio::join!(client.replace_v1_taker(stuck, Urgency::High), async {
        sleep(Duration::from_secs(3)).await;
        raw(&fork, "evm_mine", serde_json::json!([])).await;
    });
    let V1TakerReplacement::Opened(replaced) = replaced.unwrap() else {
        panic!("an open replaced as an open");
    };
    assert_ne!(replaced.tx_hash, stuck, "the replacement mined");
    let replacement_tx = w
        .provider
        .get_transaction_by_hash(replaced.tx_hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replacement_tx.nonce(), stuck_tx.nonce(), "same nonce");
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
    assert!(
        w.provider
            .get_transaction_receipt(stuck)
            .await
            .unwrap()
            .is_none(),
        "the stuck transaction did not mine"
    );
    raw(&fork, "evm_setAutomine", serde_json::json!([true])).await;
    println!(
        "stuck {stuck} replaced by {} (nonce {})",
        replaced.tx_hash,
        replacement_tx.nonce()
    );

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
    println!("\n=== v1 taker writes fork test passed ===");
}

/// `(amount0, amount1)` of a packed `BalanceDelta`.
fn unpack(delta: I256) -> (i128, i128) {
    let raw: U256 = delta.into_raw();
    let amount0 = (raw >> 128usize).to::<u128>() as i128;
    let amount1 = (raw & U256::from(u128::MAX)).to::<u128>() as i128;
    (amount0, amount1)
}
