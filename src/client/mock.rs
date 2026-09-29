//! A [`PerpClient`] over a mocked JSON-RPC transport, so the reads can be
//! characterised without a node.
//!
//! The mock answers requests in arrival order from a queue the test fills
//! beforehand, and never inspects a request. A test therefore pins three
//! things about a read: how many RPCs it makes (an unanswered request
//! fails, and [`Rpc::is_drained`] proves none was left over), what it
//! makes of the bytes it gets back, and what it remembers between calls.

use alloy::network::Ethereum;
use alloy::primitives::{Address, B256, Bytes, Signed, U256, Uint};
use alloy::providers::RootProvider;
use alloy::rpc::client::RpcClient;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use alloy::transports::mock::Asserter;

use crate::contracts::{Perp, Rates};
use crate::types::Deployments;
use crate::{HftTransport, TransportConfig};

use super::PerpClient;

/// The market the mocked client points at.
pub(super) const PERP: Address = Address::repeat_byte(0x11);
/// Its collateral token.
pub(super) const USDC: Address = Address::repeat_byte(0x22);
/// The V4 `PoolManager` its pool lives in.
pub(super) const POOL_MANAGER: Address = Address::repeat_byte(0x33);
/// Arbitrum One, so chain-bound paths take the mainnet branch.
pub(super) const CHAIN_ID: u64 = 42_161;

/// A client whose every RPC is answered by the returned [`Rpc`].
pub(super) fn client() -> (PerpClient, Rpc) {
    let asserter = Asserter::new();
    let provider = RootProvider::<Ethereum>::new(RpcClient::mocked(asserter.clone()));
    // Health diagnostics only: the provider above is not wired to it, and
    // the transport connects lazily, so nothing ever reaches this address.
    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint("http://127.0.0.1:1")
            .build()
            .expect("one endpoint is a valid config"),
    )
    .expect("a parseable URL builds a transport");
    let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(0x01))
        .expect("a non-zero scalar is a valid key");
    let deployments = Deployments {
        perp: PERP,
        usdc: USDC,
        pool_manager: POOL_MANAGER,
    };
    let client = PerpClient::from_parts(provider, transport, signer, deployments, CHAIN_ID);
    (client, Rpc(asserter))
}

/// The mock's answer queue, in the vocabulary of the reads.
pub(super) struct Rpc(Asserter);

impl Rpc {
    /// The next `eth_call` returns `ret`, encoded as `C`'s return.
    pub(super) fn call<C: SolCall>(&self, ret: &C::Return) {
        self.0
            .push_success(&Bytes::from(C::abi_encode_returns(ret)));
    }

    /// The next request fails as the node's own error, with no revert
    /// data: a transport-level failure.
    pub(super) fn fails(&self, message: &'static str) {
        self.0.push_failure_msg(message);
    }

    /// Whether every queued answer was consumed: the reads made exactly
    /// the RPCs the test expected.
    pub(super) fn is_drained(&self) -> bool {
        self.0.read_q().is_empty()
    }
}

/// `poolState()` with this AMM price; the rest of the slot is zero.
pub(super) fn pool_state(amm_price_x96: U256) -> Perp::poolStateReturn {
    Perp::poolStateReturn {
        tick: Signed::ZERO,
        sqrtPrice: Uint::ZERO,
        ammPrice: amm_price_x96,
        liquidity: 0,
    }
}

/// `rates()` with this per-day funding, scaled by 1e18 as on chain.
pub(super) fn rates(funding_per_day_wad: i128) -> Rates {
    Rates {
        fundingPerDay: Signed::try_from(funding_per_day_wad).expect("fits int88"),
        longUtilFeePerDay: 0,
        shortUtilFeePerDay: 0,
        lastTouch: Uint::ZERO,
    }
}

/// A price as the contract stores it: `mantissa / 2^shift`, in Q96.
pub(super) fn x96(mantissa: u64, shift: u32) -> U256 {
    U256::from(mantissa) << (96 - shift)
}
