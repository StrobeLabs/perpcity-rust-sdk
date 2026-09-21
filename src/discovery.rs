//! Market discovery: the perps a set of factories created.
//!
//! Every PerpCity market is a `Perp` contract, and the `PerpFactory` that
//! deploys it announces it with a `PerpCreated` log. Discovery reads those
//! logs from a factory list the caller supplies. A contract redeploy brings
//! a new factory while the old factory's perps stay live, so no single
//! factory address is authoritative, and this module hardcodes none.
//!
//! Two `PerpCreated` shapes exist, and [`decode_perp_created`] reads both
//! into one [`PerpCreation`]:
//!
//! | Factory | Extra fields | Missing fields |
//! |---|---|---|
//! | Deployed ([`PerpFactory::PerpCreated`]) | position-NFT `name`, `symbol`, `tokenUri` → [`PerpCreation::metadata`] | `latency` |
//! | Redeployed ([`PerpFactoryRedeployEvents::PerpCreated`]) | `latency` → [`PerpCreation::latency`] | NFT metadata (those perps are not ERC721s) |
//!
//! The factory list is a trust boundary: any contract can emit a log with
//! the `PerpCreated` signature, so only logs from the listed factories
//! count. Test perps and abandoned deploys also emit `PerpCreated`;
//! whether a market is listed is an off-chain decision this module does
//! not make.

use alloy::primitives::{Address, B256, U256};
use alloy::rpc::types::Log;
use alloy::sol_types::SolEvent;

use crate::contracts::{Modules, PerpFactory, PerpFactoryRedeployEvents};
use crate::feeds::events::decode_raw;

/// Name, symbol and token URI of a market's position NFT, as its factory
/// announced them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerpMetadata {
    /// ERC721 name.
    pub name: String,
    /// ERC721 symbol.
    pub symbol: String,
    /// ERC721 token URI.
    pub token_uri: String,
}

/// A perp market as its factory announced it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PerpCreation {
    /// The market's `Perp` contract.
    pub perp: Address,
    /// The factory that emitted the log.
    pub factory: Address,
    /// Block the market was created in.
    pub block_number: u64,
    /// The market's Uniswap V4 pool id.
    pub pool_id: B256,
    /// The modules the market was created with; governance may swap them
    /// later, so read `Perp::modules()` for the current set.
    pub modules: Modules,
    /// The beacon index at creation, Q96 fixed-point.
    pub initial_index_x96: U256,
    /// EMA window, in seconds.
    pub ema_window: u32,
    /// Maximum beacon index age the market accepts, in seconds. `None` for
    /// perps from a deployed-era factory, which have no such bound.
    pub latency: Option<u32>,
    /// Protocol fee at creation, scaled by 1e6. Kept as emitted, so a log
    /// never fails to decode over its width.
    pub protocol_fee: U256,
    /// The pool's starting sqrt price, Q64.96.
    pub sqrt_price_x96: U256,
    /// The pool's starting tick.
    pub tick: i32,
    /// The market's initial owner.
    pub owner: Address,
    /// Position-NFT metadata. `None` for perps from a redeployed factory,
    /// which are not ERC721s.
    pub metadata: Option<PerpMetadata>,
}

/// Decode a `PerpCreated` log of either factory shape.
///
/// Returns `None` for any other log, for a log that fails to decode, and
/// for a pending log (no block number), which is not yet a creation. The
/// caller decides which emitters to trust; see the module docs.
pub fn decode_perp_created(log: &Log) -> Option<PerpCreation> {
    let block_number = log.block_number?;
    let topic0 = *log.topic0()?;
    let factory = log.address();
    if topic0 == PerpFactory::PerpCreated::SIGNATURE_HASH {
        let e = decode_raw::<PerpFactory::PerpCreated>(log)?;
        Some(PerpCreation {
            perp: e.perp,
            factory,
            block_number,
            pool_id: e.poolId,
            modules: e.modules,
            initial_index_x96: e.initialIndex,
            ema_window: e.emaWindow.to(),
            latency: None,
            protocol_fee: e.protocolFee,
            sqrt_price_x96: U256::from(e.sqrtPriceX96),
            tick: e.tick.as_i32(),
            owner: e.owner,
            metadata: Some(PerpMetadata {
                name: e.name,
                symbol: e.symbol,
                token_uri: e.tokenUri,
            }),
        })
    } else if topic0 == PerpFactoryRedeployEvents::PerpCreated::SIGNATURE_HASH {
        let e = decode_raw::<PerpFactoryRedeployEvents::PerpCreated>(log)?;
        Some(PerpCreation {
            perp: e.perp,
            factory,
            block_number,
            pool_id: e.poolId,
            modules: e.modules,
            initial_index_x96: e.initialIndex,
            ema_window: e.emaWindow.to(),
            latency: Some(e.latency),
            protocol_fee: e.protocolFee,
            sqrt_price_x96: U256::from(e.sqrtPriceX96),
            tick: e.tick.as_i32(),
            owner: e.owner,
            metadata: None,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Signed, U160, Uint, address, b256, uint};

    use super::*;
    use crate::contracts::IBeacon;
    use crate::convert::price_x96_to_f64;

    const MAINNET_LOG: &str = include_str!("../tests/fixtures/perp_created_arbitrum_one.json");

    fn mainnet_log() -> Log {
        serde_json::from_str(MAINNET_LOG).unwrap()
    }

    fn modules() -> Modules {
        Modules {
            beacon: address!("1b37de2b5dc8cf5d290d6dcfded11aaa7d0ef884"),
            fees: address!("bfda8aa80132c51995b37c03d9fe384dcbf0056e"),
            funding: address!("b9572a6cdd39965e2d03f13d5559e0ca9fe599d1"),
            marginRatios: address!("8afca53c52b1f02d76aefb811c6b08f4bd3e4cf9"),
            priceImpact: address!("71889d6b403ddc5007d1ebabb52d1fcd5ec04832"),
            pricing: address!("f4689da0cac3f23a04145236dbfe81c3c58cfe22"),
        }
    }

    /// Golden vector: the `PerpCreated` log of the HORMUZ market
    /// (0x137E0048…, Arbitrum One factory 0xCE0c5f65…, tx 0x92306c56…),
    /// committed exactly as `eth_getLogs` returned it.
    #[test]
    fn decodes_a_mainnet_perp_created_log() {
        let created = decode_perp_created(&mainnet_log()).expect("mainnet PerpCreated");
        assert_eq!(
            created,
            PerpCreation {
                perp: address!("137e00487dc079dad69ba149994320a8ff4c5b17"),
                factory: address!("ce0c5f65a5eda69a1dfb3f3273749b649abc4ec6"),
                block_number: 486_214_447,
                pool_id: b256!("37ad79e6fbdceff901cbcd8ffb5fa871aa1757f2c2823d1b3c0e41d776525768"),
                modules: modules(),
                initial_index_x96: uint!(3723723638170423866896565665792_U256),
                ema_window: 3_600,
                latency: None,
                protocol_fee: U256::ZERO,
                sqrt_price_x96: uint!(543160916822237860744942667458_U256),
                tick: 38_503,
                owner: address!("2d0be18386297d833e63d0b1f6bc93f391af6f93"),
                metadata: Some(PerpMetadata {
                    name: "Hormuz Vessel Count Perp".into(),
                    symbol: "HORMUZ-COUNT-PERP".into(),
                    token_uri: String::new(),
                }),
            }
        );
        assert_eq!(price_x96_to_f64(created.initial_index_x96).unwrap(), 47.0);
    }

    /// No factory emits the redeploy shape yet, so this vector is encoded
    /// from the binding; the topic0 it produces is locked in `abi_lock`.
    #[test]
    fn decodes_the_redeploy_shape_with_latency_and_no_metadata() {
        let event = PerpFactoryRedeployEvents::PerpCreated {
            perp: Address::repeat_byte(0x01),
            poolId: B256::repeat_byte(0x02),
            modules: modules(),
            initialIndex: U256::from(1u64) << 96,
            emaWindow: Uint::<24, 1>::from(900u32),
            latency: 300,
            protocolFee: U256::from(10_000u64),
            sqrtPriceX96: U160::from(1u64) << 96,
            tick: Signed::<24, 1>::try_from(-60).unwrap(),
            owner: Address::repeat_byte(0x03),
        };
        let mut log = mainnet_log();
        log.inner.data = event.encode_log_data();

        let created = decode_perp_created(&log).expect("redeploy PerpCreated");
        assert_eq!(created.perp, Address::repeat_byte(0x01));
        assert_eq!(created.latency, Some(300));
        assert_eq!(created.metadata, None);
        assert_eq!(created.ema_window, 900);
        assert_eq!(created.protocol_fee, U256::from(10_000u64));
        assert_eq!(created.tick, -60);
        assert_eq!(created.modules, modules());
    }

    #[test]
    fn a_pending_or_foreign_log_is_not_a_creation() {
        let mut pending = mainnet_log();
        pending.block_number = None;
        assert_eq!(decode_perp_created(&pending), None);

        let mut foreign = mainnet_log();
        foreign.inner.data = IBeacon::IndexUpdated {
            index: U256::from(1u64),
        }
        .encode_log_data();
        assert_eq!(decode_perp_created(&foreign), None);
    }
}
