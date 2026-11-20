//! Integration test: the position reads on a `v1.0.0` market. On an Anvil
//! fork of Arbitrum Sepolia, the W11 market (`v1_fork`: one maker, moved
//! and reduced, and one closed taker) is read through the SDK, and every
//! read agrees with a raw `eth_call` of the `v1` binding at the same block.
//! `get_positions_by_owner` asks `positionOwner`, so it finds the maker and
//! not the closed taker, whose owner the contract zeroes.
//!
//! Requires `anvil` (from Foundry) and an Arbitrum Sepolia endpoint in
//! `SEPOLIA_FORK_URL` (default: the public RPC).
//!
//! ```bash
//! cargo test --test v1_positions_fork -- --ignored --nocapture
//! ```

mod v1_fork;

use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};

use perpcity_sdk::contracts::{Perp as EarlierPerp, v1::Perp};
use perpcity_sdk::{
    ChainReader, ContractError, Era, HftTransport, LUnits, MakerBand, PerpCityError, PositionRow,
    TickRange, TransportConfig,
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
    let raw = Perp::new(m.perp, &w.provider);
    assert_eq!(raw.positionOwner(m.maker_id).call().await.unwrap(), w.me);
    assert_eq!(
        raw.positionOwner(m.taker_id).call().await.unwrap(),
        Address::ZERO
    );
    let minted: u64 = raw.nextPosId().call().await.unwrap().try_into().unwrap();
    assert_eq!(minted, 3, "ids 1 and 2");

    // The earlier builds' calls on this market fail loudly: v1 has no
    // `ownerOf`, and its three-word row does not decode as five.
    let earlier = EarlierPerp::new(m.perp, &w.provider);
    let owner_of = earlier.ownerOf(m.maker_id).call().await.unwrap_err();
    let revert_data = match &owner_of {
        alloy::contract::Error::TransportError(e) => {
            e.as_error_resp().and_then(|resp| resp.as_revert_data())
        }
        _ => None,
    };
    println!("ownerOf on v1: {owner_of}; revert data {revert_data:?}");
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
    let at = BlockId::hash(state.block().hash);
    let ids: Vec<U256> = (1..minted).map(U256::from).collect();
    let (rows, owners, bands) = tokio::join!(
        state.positions(&ids),
        state.owners(&ids),
        state.maker_bands(&ids)
    );
    for (((&pos_id, row), owner), band) in ids.iter().zip(rows).zip(owners).zip(bands) {
        let position = raw.positions(pos_id).block(at).call().await.unwrap();
        let expected_row = (position.margin != 0 || !position.delta.is_zero())
            .then(|| PositionRow::from(position));
        assert_eq!(row.row.unwrap(), expected_row, "{pos_id}: positions");
        assert_eq!(
            state.position(pos_id).await.unwrap(),
            expected_row,
            "{pos_id}: position"
        );

        let holder = raw.positionOwner(pos_id).block(at).call().await.unwrap();
        let expected_owner = (holder != Address::ZERO).then_some(holder);
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
}
