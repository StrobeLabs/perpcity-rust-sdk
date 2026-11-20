//! Integration test: the position reads on a `v1.0.0` market. On an Anvil
//! fork of Arbitrum Sepolia, the W11 market (`v1_fork`: one maker, moved
//! and reduced, and one closed taker) is read through the SDK, then a new
//! taker is opened on it, and every read agrees with a raw `eth_call` of
//! the `v1` binding at the same block. `get_positions_by_owner` asks
//! `positionOwner`, so it finds the maker and the new taker and not the
//! closed taker, whose owner the contract zeroes; the earlier builds'
//! `ownerOf` walk on the same market finds nothing (map row 13, silent).
//!
//! Requires `anvil` (from Foundry) and an Arbitrum Sepolia endpoint in
//! `SEPOLIA_FORK_URL` (default: the public RPC).
//!
//! ```bash
//! cargo test --test v1_positions_fork -- --ignored --nocapture
//! ```

mod v1_fork;

use alloy::eips::BlockId;
use alloy::primitives::I256;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::DynProvider;
use alloy::sol_types::{SolError, SolValue};
use perpcity_sdk::contracts::v1::OpenTakerParams;
use perpcity_sdk::errors::decode::try_extract_revert;

use perpcity_sdk::contracts::{Perp as EarlierPerp, v1::Perp};
use perpcity_sdk::{
    ChainReader, ContractError, Era, HftTransport, LUnits, MakerBand, PerpCityError, PositionRow,
    StateAt, TickRange, TransportConfig,
};

use v1_fork::{SepoliaFork, V1Fork};

#[tokio::test]
#[ignore] // Requires `anvil` and an Arbitrum Sepolia endpoint.
async fn position_reads_agree_with_the_v1_market() {
    let fork = SepoliaFork::start().await;
    let mut w = V1Fork::new(&fork);
    let m = w.build_w11_market().await;
    println!(
        "v1 market {}: maker {}, taker {}",
        m.perp, m.maker_id, m.taker_id
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
    let market = chain.market(m.perp);
    assert_eq!(market.era().await.unwrap(), Era::V1);

    // What the contract says: the closed taker's owner is zero, the
    // maker's is the dev wallet, and two ids were minted.
    let provider = w.provider.clone();
    let raw = Perp::new(m.perp, &provider);
    assert_eq!(raw.positionOwner(m.maker_id).call().await.unwrap(), w.me);
    assert_eq!(
        raw.positionOwner(m.taker_id).call().await.unwrap(),
        Address::ZERO
    );
    let minted: u64 = raw.nextPosId().call().await.unwrap().try_into().unwrap();
    assert_eq!(minted, 3, "ids 1 and 2");

    // The earlier builds' calls on this market fail loudly: v1 has no
    // `ownerOf`, and its three-word row does not decode as five.
    let earlier = EarlierPerp::new(m.perp, &provider);
    let owner_of = earlier.ownerOf(m.maker_id).call().await.unwrap_err();
    let revert_data = match &owner_of {
        alloy::contract::Error::TransportError(e) => {
            e.as_error_resp().and_then(|resp| resp.as_revert_data())
        }
        _ => None,
    };
    println!("ownerOf on v1: {owner_of}; revert data {revert_data:?}");
    // Anvil behaviour: the empty revert carries `0x` as its data, so the
    // old walk takes it for a burned token. Nitro may attach no data,
    // which the old walk fails on loudly instead.
    assert_eq!(revert_data, Some(Bytes::new()), "anvil: an empty revert");
    assert!(earlier.positions(m.maker_id).call().await.is_err());

    // The walk: the two minted ids less the closed taker.
    assert_eq!(
        market.get_positions_by_owner(w.me).await.unwrap(),
        vec![m.maker_id]
    );

    // The now-read against the raw row.
    let row = market.get_position(m.maker_id).await.unwrap();
    assert_eq!(
        row,
        PositionRow::from(raw.positions(m.maker_id).call().await.unwrap())
    );
    assert!(row.stored_margin_ratios.is_none());
    let err = market.get_position(m.taker_id).await.unwrap_err();
    assert!(
        matches!(
            err,
            PerpCityError::Contract(ContractError::PositionNotFound { pos_id })
                if pos_id == m.taker_id
        ),
        "{err}"
    );

    // The pinned reads against raw calls at the same block.
    let state = market.state_at_head().await.unwrap();
    let ids: Vec<U256> = (1..minted).map(U256::from).collect();
    reads_agree_with_raw_calls(&state, &raw, &ids).await;

    // The closed taker is gone from every read; the maker is in each.
    let index = |id: U256| ids.iter().position(|&i| i == id).unwrap();
    let (rows, owners, bands) = tokio::join!(
        state.positions(&ids),
        state.owners(&ids),
        state.maker_bands(&ids)
    );
    let (maker, taker) = (index(m.maker_id), index(m.taker_id));
    assert!(rows[taker].row.as_ref().unwrap().is_none());
    assert!(owners[taker].row.as_ref().unwrap().is_none());
    assert!(bands[taker].row.as_ref().unwrap().is_none());
    assert!(rows[maker].row.as_ref().unwrap().is_some());
    assert_eq!(*owners[maker].row.as_ref().unwrap(), Some(w.me));
    assert!(bands[maker].row.as_ref().unwrap().is_some());

    // A new taker on the same market: two live ids, one owner.
    let initial = state.tvl_cap().await.unwrap();
    assert_eq!(initial.cap, U256::MAX);
    let margin = v1_fork::TAKER_MARGIN;
    let limit = initial.accounted + U256::from(margin);
    w.send("lower-tvl-cap", raw.lowerTvlCap(limit)).await;
    let capped = market
        .state_at_head()
        .await
        .unwrap()
        .tvl_cap()
        .await
        .unwrap();
    assert_eq!(capped.cap, limit);
    assert!(capped.allows_deposit(U256::from(margin)));
    assert!(!capped.allows_deposit(U256::from(margin + 1)));
    let oversized = OpenTakerParams {
        holder: w.me,
        margin: margin + 1,
        perpDelta: I256::try_from(NEW_TAKER_SIZE).unwrap(),
        amt1Limit: U256::MAX,
        maxTotalFee: U256::from(v1_fork::MAX_TOTAL_FEE),
        deadline: w.deadline().await,
    };
    let err = raw
        .openTaker(oversized)
        .from(w.me)
        .call()
        .await
        .unwrap_err();
    assert_eq!(
        try_extract_revert(&err.to_string()).unwrap().1,
        Perp::TvlCapExceeded::SELECTOR
    );
    let new_taker = w.open_taker_sized(m.perp, NEW_TAKER_SIZE).await;
    println!("new taker {new_taker}");
    assert_eq!(
        market.get_positions_by_owner(w.me).await.unwrap(),
        vec![m.maker_id, new_taker]
    );
    let row = market.get_position(new_taker).await.unwrap();
    assert_eq!(
        row,
        PositionRow::from(raw.positions(new_taker).call().await.unwrap())
    );
    assert_eq!(row.perp.atoms(), i128::from(NEW_TAKER_SIZE));
    assert!(row.stored_margin_ratios.is_none());
    assert!(matches!(
        market.get_position(m.taker_id).await.unwrap_err(),
        PerpCityError::Contract(ContractError::PositionNotFound { .. })
    ));

    let state = market.state_at_head().await.unwrap();
    let minted: u64 = raw.nextPosId().call().await.unwrap().try_into().unwrap();
    let previews = state
        .preview_positions(&[m.maker_id, m.taker_id, new_taker])
        .await
        .unwrap();
    for preview in previews {
        let row = preview.row.unwrap();
        if preview.pos_id == m.taker_id {
            assert!(row.is_none());
        } else {
            let value = row.unwrap();
            assert_eq!(value.isMaker, preview.pos_id == m.maker_id);
            let raw_value = raw
                .previewPosition(preview.pos_id)
                .block(state.block().hash.into())
                .call()
                .await
                .unwrap();
            assert_eq!(value.abi_encode(), raw_value.abi_encode());
        }
    }
    assert_eq!(minted, 4, "ids 1 to 3");
    let ids: Vec<U256> = (1..minted).map(U256::from).collect();
    reads_agree_with_raw_calls(&state, &raw, &ids).await;
    let index = |id: U256| ids.iter().position(|&i| i == id).unwrap();
    let (rows, owners, bands) = tokio::join!(
        state.positions(&ids),
        state.owners(&ids),
        state.maker_bands(&ids)
    );
    let (maker, taker, live) = (index(m.maker_id), index(m.taker_id), index(new_taker));
    assert!(rows[taker].row.as_ref().unwrap().is_none());
    assert!(owners[taker].row.as_ref().unwrap().is_none());
    assert!(bands[taker].row.as_ref().unwrap().is_none());
    assert!(state.position(m.taker_id).await.unwrap().is_none());
    assert!(state.maker_band(m.taker_id).await.unwrap().is_none());
    assert_eq!(*rows[live].row.as_ref().unwrap(), Some(row));
    assert_eq!(*owners[live].row.as_ref().unwrap(), Some(w.me));
    assert!(
        bands[live].row.as_ref().unwrap().is_none(),
        "a taker has no band"
    );
    assert!(state.maker_band(new_taker).await.unwrap().is_none());
    assert_eq!(*owners[maker].row.as_ref().unwrap(), Some(w.me));

    // Map row 13, the silent case, on real v1.0.0 bytecode: the earlier
    // builds' walk (`ownerOf` over `1..nextPosId`, an error with revert
    // data taken for a burned token, an error without it propagated)
    // finds neither live position.
    assert!(
        earlier_owner_of_walk(&earlier, minted, w.me)
            .await
            .unwrap()
            .is_empty(),
        "anvil: the old walk drops every v1 position silently"
    );
}

/// The perp atoms of the new taker: after the fourteen W11 steps a
/// `TAKER_SIZE` open reverts `PriceImpactTooHigh` on the thin band.
const NEW_TAKER_SIZE: i64 = 100_000;

/// Every pinned read of `ids` on `state` against the raw `v1` call at the
/// same block.
async fn reads_agree_with_raw_calls(
    state: &StateAt,
    raw: &Perp::PerpInstance<&DynProvider>,
    ids: &[U256],
) {
    let at = BlockId::hash(state.block().hash);
    let (rows, owners, bands) = tokio::join!(
        state.positions(ids),
        state.owners(ids),
        state.maker_bands(ids)
    );
    for (((&pos_id, row), owner), band) in ids.iter().zip(rows).zip(owners).zip(bands) {
        let position = raw.positions(pos_id).block(at).call().await.unwrap();
        let holder = raw.positionOwner(pos_id).block(at).call().await.unwrap();
        let expected_owner = (holder != Address::ZERO).then_some(holder);
        let expected_row = expected_owner.map(|_| PositionRow::from(position));
        assert_eq!(row.row.unwrap(), expected_row, "{pos_id}: positions");
        assert_eq!(
            state.position(pos_id).await.unwrap(),
            expected_row,
            "{pos_id}: position"
        );

        assert_eq!(owner.row.unwrap(), expected_owner, "{pos_id}: owners");

        let maker = raw.makerDetails(pos_id).block(at).call().await.unwrap();
        let expected_band = (maker.liquidity != 0).then(|| {
            MakerBand::new(
                TickRange::new(maker.tickLower.as_i32(), maker.tickUpper.as_i32()).unwrap(),
                LUnits::new(maker.liquidity),
            )
        });
        assert_eq!(band.row.unwrap(), expected_band, "{pos_id}: maker_bands");
        assert_eq!(
            state.maker_band(pos_id).await.unwrap(),
            expected_band,
            "{pos_id}: maker_band"
        );
        println!(
            "{pos_id}: row {expected_row:?}, owner {expected_owner:?}, band {expected_band:?}"
        );
    }
}

/// The earlier builds' `get_positions_by_owner` walk, as it was before
/// `v1.0.0`: `ownerOf` for each id in `1..next_pos_id`; a revert with
/// revert data is a burned token and skipped, an error with none is
/// propagated. Keep it in step with `nft_owner` in `src/client/queries.rs`.
async fn earlier_owner_of_walk(
    earlier: &EarlierPerp::PerpInstance<&DynProvider>,
    next_pos_id: u64,
    owner: Address,
) -> Result<Vec<U256>, alloy::contract::Error> {
    let mut owned = Vec::new();
    for id in 1..next_pos_id {
        let pos_id = U256::from(id);
        match earlier.ownerOf(pos_id).call().await {
            Ok(holder) if holder == owner => owned.push(pos_id),
            Ok(_) => {}
            Err(alloy::contract::Error::TransportError(e))
                if e.as_error_resp()
                    .and_then(|resp| resp.as_revert_data())
                    .is_none() =>
            {
                return Err(alloy::contract::Error::TransportError(e));
            }
            Err(_) => {}
        }
    }
    Ok(owned)
}
