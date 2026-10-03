//! Integration test: the raw EMA storage word equals the deployed `emas()`
//! view on a build-`58b42b7` perp, at a pinned historical block.
//!
//! `v0.2.2-upgradeable` removed the `emas()` view, so the SDK reads the
//! `PerpStorage.emas` word (slot 11) instead. This proves the slot and
//! the decode against the old contract, which still has the view.
//!
//! Requires:
//! - `RPC_URL`, an Arbitrum One ARCHIVE endpoint (e.g.
//!   `https://arbitrum.gateway.tenderly.co`)
//!
//! Run with:
//!
//! ```bash
//! RPC_URL="https://..." cargo test --test emas_slot_live -- --ignored --nocapture
//! ```

use alloy::primitives::{Address, U256, address};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;

use perpcity_sdk::PricePair;

// The HORMUZ perp (build 58b42b7) and a block it was live at.
const HORMUZ_PERP: Address = address!("137e00487dc079dad69ba149994320a8ff4c5b17");
const BLOCK: u64 = 510_200_629;
/// `PerpStorage.emas`: struct base slot 3 + field index 8.
const EMAS_SLOT: u64 = 11;

sol! {
    #[sol(rpc)]
    interface IEmas {
        struct Pair { uint128 ammPrice; uint128 index; }
        function emas() external view returns (Pair memory);
    }
}

#[tokio::test]
#[ignore] // Requires a live archive RPC endpoint — run with: cargo test --test emas_slot_live -- --ignored --nocapture
async fn slot_11_equals_the_emas_view_on_an_old_perp() {
    let url = std::env::var("RPC_URL").expect("RPC_URL environment variable must be set");
    let provider = ProviderBuilder::new().connect_http(url.parse().expect("RPC_URL is a URL"));

    let word = provider
        .get_storage_at(HORMUZ_PERP, U256::from(EMAS_SLOT))
        .block_id(BLOCK.into())
        .await
        .expect("eth_getStorageAt");
    let decoded = PricePair {
        amm: (word & U256::from(u128::MAX)).to::<u128>(),
        index: word.wrapping_shr(128).to::<u128>(),
    };

    let view = IEmas::new(HORMUZ_PERP, &provider)
        .emas()
        .block(BLOCK.into())
        .call()
        .await
        .expect("emas()");
    println!(
        "slot 11 = {word:#x}; emas() = ({}, {})",
        view.ammPrice, view.index
    );
    assert_eq!(decoded.amm, view.ammPrice);
    assert_eq!(decoded.index, view.index);
}
