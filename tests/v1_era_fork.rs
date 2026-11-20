//! Integration test: the era of a market on each build, read by the SDK
//! from deployed bytecode. On an Anvil fork of Arbitrum Sepolia, a
//! `v1.0.0` market built by the first W11 steps (`v1_fork`) is `V1`, the
//! `v0.2.2` rehearsal market is `Upgradeable`, and a pre-`v1.0.0` market
//! is `Legacy`; each market's pool id is its era's pool id view and the
//! keccak of its pool key.
//!
//! Requires `anvil` (from Foundry) and an Arbitrum Sepolia endpoint in
//! `SEPOLIA_FORK_URL` (default: the public RPC).
//!
//! ```bash
//! cargo test --test v1_era_fork -- --ignored --nocapture
//! ```

mod v1_fork;

use alloy::primitives::{Address, U256, address, keccak256};
use alloy::providers::Provider;
use alloy::sol_types::SolValue;
use alloy::transports::RpcError;

use perpcity_sdk::client::PositionRole;
use perpcity_sdk::constants::SNAPSHOT_BLOCK_LAG;
use perpcity_sdk::contracts::{
    Perp as PerpV022Surface,
    v1::{Perp, PerpFactory},
};
use perpcity_sdk::{
    ARBITRUM_CHAIN_ID, ChainDeployments, ChainReader, ContractError, Era, HftTransport,
    PerpCityError, TransportConfig,
};

use v1_fork::{HOOK, SepoliaFork, V1Fork, index_1};

/// The `v0.2.2-upgradeable` rehearsal market (IMI), from the factory
/// `0x4C9D…87e0`.
const V022_PERP: Address = address!("9A499A8d1798aD1146EB5E37F26d701702D684d4");

/// A market of the pre-`v1.0.0` factory `0xa54F…E7a7`, hookless.
const LEGACY_PERP: Address = address!("6d4051Ffb71f391a5B4D8643a29Ec6F66F67df50");

#[tokio::test]
#[ignore] // Requires `anvil` and an Arbitrum Sepolia endpoint.
async fn each_build_reads_as_its_era_on_a_sepolia_fork() {
    let fork = SepoliaFork::start().await;
    let mut w = V1Fork::new(&fork);
    w.check_deploy().await;
    let verifier = w.create_verifier().await;
    let beacon = w.create_beacon(verifier).await;
    w.bind(verifier, beacon).await;
    w.push_index("update-1", verifier, beacon, index_1()).await;
    let v1_perp = w.create_perp(beacon).await;
    println!("v1 market {v1_perp}");

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
    assert_eq!(chain.deployments().v1_perp_guard_hook, HOOK);
    assert_eq!(
        chain.deployments(),
        &ChainDeployments::ARBITRUM_SEPOLIA,
        "the fork reader carries the Sepolia deployments"
    );

    let sepolia = ChainDeployments::ARBITRUM_SEPOLIA;
    let factory = PerpFactory::new(sepolia.v1_perp_factory, &w.provider);
    let hook = factory.PERP_GUARD_HOOK().call().await.unwrap();
    assert_eq!(hook, sepolia.v1_perp_guard_hook);
    let controller_impl = factory.WITHDRAWAL_CONTROLLER_IMPL().call().await.unwrap();
    assert_eq!(controller_impl, sepolia.v1_withdrawal_controller_impl);

    let v1_market = chain.market(v1_perp);
    assert_eq!(v1_market.era().await.unwrap(), Era::V1);
    assert!(!v1_market.is_upgradeable().await.unwrap());
    let key = Perp::new(v1_perp, &w.provider)
        .poolKey()
        .call()
        .await
        .unwrap();
    assert_eq!(key.hooks, HOOK);
    let pool_id = Perp::new(v1_perp, &w.provider)
        .poolId()
        .call()
        .await
        .unwrap();
    assert_eq!(pool_id, keccak256(key.abi_encode()));
    println!("v1: era V1, pool id {pool_id}");

    // The tape and liquidation paths still refuse v1.
    let unsupported = |err: &PerpCityError, operation: &str| {
        matches!(
            err,
            PerpCityError::Contract(ContractError::UnsupportedEra { era: Era::V1, operation: o })
                if *o == operation
        )
    };
    let err = v1_market.tape_addresses().await.unwrap_err();
    assert!(unsupported(&err, "tape_addresses"), "{err}");
    let recipient = Address::repeat_byte(0x44);
    let err = v1_market
        .liquidation_calldata(PositionRole::Taker, U256::from(1u8), recipient)
        .await
        .unwrap_err();
    assert!(unsupported(&err, "liquidation_calldata"), "{err}");
    let err = v1_market
        .simulate_liquidate_maker(w.me, U256::from(1u8), recipient)
        .await
        .unwrap_err();
    assert!(unsupported(&err, "liquidation_calldata"), "{err}");
    // The two block-pinned reads read the head less SNAPSHOT_BLOCK_LAG: mine
    // past it, so the pinned block holds the market's code.
    let lag = U256::from(SNAPSHOT_BLOCK_LAG + 2);
    let _: serde_json::Value = w
        .provider
        .raw_request("anvil_mine".into(), (lag,))
        .await
        .unwrap();
    let pinned = v1_market.state().await.unwrap().block().number;
    let code = w
        .provider
        .get_code_at(v1_perp)
        .block_id(pinned.into())
        .await
        .unwrap();
    assert!(
        !code.is_empty(),
        "the pinned block {pinned} holds the v1 market"
    );
    // The earlier maker-equity batch still asks for the old EMA window.
    let multicall_reverted = |err: &PerpCityError| {
        matches!(
            err,
            PerpCityError::Abi(alloy::contract::Error::TransportError(RpcError::ErrorResp(payload)))
                if payload.code == 3 && payload.message.contains("Multicall3: call failed")
        ) && !err.is_transient()
    };
    let snapshot = v1_market.get_pool_snapshot().await.unwrap();
    let raw = Perp::new(v1_perp, &w.provider)
        .poolState()
        .block(snapshot.block.number.into())
        .call()
        .await
        .unwrap();
    assert_eq!(snapshot.sqrt_price.x96(), raw.sqrtPrice.to::<U256>());
    let err = v1_market
        .get_maker_equities(&[U256::from(1u8)])
        .await
        .unwrap_err();
    assert!(multicall_reverted(&err), "maker equities: {err:?}");
    println!("v1 get_maker_equities fails loudly: {err}");

    for (perp, era) in [(V022_PERP, Era::Upgradeable), (LEGACY_PERP, Era::Legacy)] {
        let market = chain.market(perp);
        assert_eq!(market.era().await.unwrap(), era, "{perp}");
        assert_eq!(
            market.is_upgradeable().await.unwrap(),
            era == Era::Upgradeable
        );
        let surface = PerpV022Surface::new(perp, &w.provider);
        let key = surface.poolKey().call().await.unwrap();
        let pool_id = surface.POOL_ID().call().await.unwrap();
        assert_eq!(pool_id, keccak256(key.abi_encode()), "{perp}");
        let tape = market.tape_addresses().await.unwrap();
        assert_eq!(
            tape.pool_id, pool_id,
            "{perp}: immutables() reads POOL_ID()"
        );
        println!("{perp}: era {era:?}, hook {}, pool id {pool_id}", key.hooks);
    }

    // A hook is one chain's: under Arbitrum One's deployments the Sepolia
    // hooks name no build, so each hooked market is refused, never guessed.
    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&fork.url)
            .build()
            .unwrap(),
    )
    .unwrap();
    let wrong = ChainReader::new(transport, ChainDeployments::ARBITRUM, ARBITRUM_CHAIN_ID);
    for perp in [v1_perp, V022_PERP] {
        let err = wrong.market(perp).era().await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::UnknownBuild { perp: p, .. }) if p == perp
            ),
            "{perp}: {err}"
        );
        assert!(!err.is_transient());
    }
    assert_eq!(wrong.market(LEGACY_PERP).era().await.unwrap(), Era::Legacy);
    println!("under Arbitrum One's deployments: v1 and v0.2.2 refused, legacy is Legacy");
}
