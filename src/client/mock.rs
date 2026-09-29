//! A [`PerpClient`] over a mocked JSON-RPC transport, so the reads can be
//! characterised without a node.
//!
//! The mock answers requests in arrival order from a queue the test fills
//! beforehand, and never inspects a request. A test therefore pins three
//! things about a read: how many RPCs it makes (an unanswered request
//! fails, and [`Rpc::is_drained`] proves none was left over), what it
//! makes of the bytes it gets back, and what it remembers between calls.

use alloy::consensus::Header as ConsensusHeader;
use alloy::network::Ethereum;
use alloy::primitives::{Address, B256, Bytes, I256, Signed, U64, U256, Uint};
use alloy::providers::RootProvider;
use alloy::providers::bindings::IMulticall3 as Multicall;
use alloy::rpc::client::RpcClient;
use alloy::rpc::json_rpc::ErrorPayload;
use alloy::rpc::types::{Block, BlockTransactions, Header};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use alloy::transports::mock::Asserter;
use serde_json::value::RawValue;

use crate::contracts::{
    Capacity, IMulticall3, Modules, OpenInterest, Perp, PoolKey, Position, PricePair, Rates,
};
use crate::types::Deployments;
use crate::{HftTransport, TransportConfig};

use super::PerpClient;

/// The market the mocked client points at.
pub(super) const PERP: Address = Address::repeat_byte(0x11);
/// Its collateral token.
pub(super) const USDC: Address = Address::repeat_byte(0x22);
/// The V4 `PoolManager` its pool lives in.
pub(super) const POOL_MANAGER: Address = Address::repeat_byte(0x33);
/// The modules `modules()` names, each distinct so a read that asks the
/// wrong one is asking a different address.
pub(super) const BEACON: Address = Address::repeat_byte(0xb1);
pub(super) const FEES: Address = Address::repeat_byte(0xf1);
pub(super) const FUNDING: Address = Address::repeat_byte(0xf2);
pub(super) const MARGIN_RATIOS: Address = Address::repeat_byte(0xa1);
pub(super) const PRICE_IMPACT: Address = Address::repeat_byte(0xd1);
pub(super) const PRICING: Address = Address::repeat_byte(0xd2);
/// Arbitrum One, so chain-bound paths take the mainnet branch.
pub(super) const CHAIN_ID: u64 = 42_161;

/// A client whose every RPC is answered by the returned [`Rpc`].
pub(super) fn client() -> (PerpClient, Rpc) {
    client_sharing(transport())
}

/// A client whose every RPC is answered by the returned [`Rpc`], over a
/// transport the test hands in — the same one to several clients, to
/// test what the transport remembers on their behalf.
pub(super) fn client_sharing(transport: HftTransport) -> (PerpClient, Rpc) {
    let asserter = Asserter::new();
    let provider = RootProvider::<Ethereum>::new(RpcClient::mocked(asserter.clone()));
    (client_with(provider, transport), Rpc(asserter))
}

/// A client over any provider — a mocked one, or a
/// [`FakeNode`](crate::history::test_support::FakeNode)'s — with a fixed
/// signer and deployments.
pub(super) fn client_over(provider: RootProvider<Ethereum>) -> PerpClient {
    client_with(provider, transport())
}

/// A transport no request ever reaches: the mocked provider is not wired
/// to it, and it connects lazily. It carries the endpoint capabilities the
/// reads consult, so a test can share one between clients.
pub(super) fn transport() -> HftTransport {
    HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint("http://127.0.0.1:1")
            .build()
            .expect("one endpoint is a valid config"),
    )
    .expect("a parseable URL builds a transport")
}

fn client_with(provider: RootProvider<Ethereum>, transport: HftTransport) -> PerpClient {
    let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(0x01))
        .expect("a non-zero scalar is a valid key");
    let deployments = Deployments {
        perp: PERP,
        usdc: USDC,
        pool_manager: POOL_MANAGER,
    };
    PerpClient::from_parts(provider, transport, signer, deployments, CHAIN_ID)
}

/// The mock's answer queue, in the vocabulary of the reads.
pub(super) struct Rpc(Asserter);

impl Rpc {
    /// The next `eth_call` returns `ret`, encoded as `C`'s return.
    pub(super) fn call<C: SolCall>(&self, ret: &C::Return) {
        self.0.push_success(&Bytes::from(returns::<C>(ret)));
    }

    /// The next Multicall3 `aggregate` returns these encoded results, in
    /// call order, stamped with `block`.
    pub(super) fn aggregate(&self, block: u64, results: impl IntoIterator<Item = Vec<u8>>) {
        self.call::<Multicall::aggregateCall>(&Multicall::aggregateReturn {
            blockNumber: U256::from(block),
            returnData: results.into_iter().map(Bytes::from).collect(),
        });
    }

    /// The next Multicall3 `aggregate3` returns these rows.
    pub(super) fn aggregate3(&self, rows: Vec<IMulticall3::Result>) {
        self.call::<IMulticall3::aggregate3Call>(&rows);
    }

    /// The next request returns a hex quantity (`eth_blockNumber`).
    pub(super) fn quantity(&self, n: u64) {
        self.0.push_success(&U64::from(n));
    }

    /// The next `eth_getBlockByNumber` finds a header with this number
    /// and timestamp; returns its hash, which pinned reads carry.
    pub(super) fn block(&self, number: u64, timestamp: u64) -> B256 {
        let header = Header::new(ConsensusHeader {
            number,
            timestamp,
            ..ConsensusHeader::default()
        });
        let hash = header.hash;
        let block: Block = Block::new(header, BlockTransactions::Hashes(Vec::new()));
        self.0.push_success(&block);
        hash
    }

    /// The next `eth_getBlockByNumber` finds nothing.
    pub(super) fn no_block(&self) {
        self.0.push_success(&Option::<Block>::None);
    }

    /// The next `eth_getStorageAt` returns `word`.
    pub(super) fn storage(&self, word: U256) {
        self.0.push_success(&word);
    }

    /// The next request fails as the node's own error, with no revert
    /// data: a transport-level failure.
    pub(super) fn fails(&self, message: &'static str) {
        self.0.push_failure_msg(message);
    }

    /// The next request is refused as a method the endpoint does not
    /// serve (JSON-RPC `-32601`), the one answer worth remembering.
    pub(super) fn method_not_found(&self) {
        self.0.push_failure(ErrorPayload {
            code: -32601,
            message: "the method does not exist/is not available".into(),
            data: None,
        });
    }

    /// The next `eth_call` reverts, carrying `data` the way a node reports
    /// `execution reverted` (JSON-RPC code 3).
    pub(super) fn reverts(&self, data: &[u8]) {
        let data = RawValue::from_string(format!("\"0x{}\"", alloy::hex::encode(data)))
            .expect("a quoted hex string is valid JSON");
        self.0.push_failure(ErrorPayload {
            code: 3,
            message: "execution reverted".into(),
            data: Some(data),
        });
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

/// `ret`, ABI-encoded as `C`'s return: one entry of a multicall answer.
pub(super) fn returns<C: SolCall>(ret: &C::Return) -> Vec<u8> {
    C::abi_encode_returns(ret)
}

/// A successful `aggregate3` row.
pub(super) fn ok_row(return_data: Vec<u8>) -> IMulticall3::Result {
    IMulticall3::Result {
        success: true,
        returnData: return_data.into(),
    }
}

/// A reverted `aggregate3` row.
pub(super) fn failed_row() -> IMulticall3::Result {
    IMulticall3::Result {
        success: false,
        returnData: Bytes::new(),
    }
}

/// `capacity()` in perp atoms.
pub(super) fn capacity(long: u128, short: u128) -> Capacity {
    Capacity { long, short }
}

/// `emas()`: the stored pair, both Q96 narrowed to `uint128`.
pub(super) fn emas(amm_x96: u128, index_x96: u128) -> PricePair {
    PricePair {
        ammPrice: amm_x96,
        index: index_x96,
    }
}

/// `modules()` naming the addresses above.
pub(super) fn modules() -> Modules {
    Modules {
        beacon: BEACON,
        fees: FEES,
        funding: FUNDING,
        marginRatios: MARGIN_RATIOS,
        priceImpact: PRICE_IMPACT,
        pricing: PRICING,
    }
}

/// `poolKey()` with this tick spacing.
pub(super) fn pool_key(tick_spacing: i32) -> PoolKey {
    PoolKey {
        currency0: Address::ZERO,
        currency1: Address::ZERO,
        fee: Uint::ZERO,
        tickSpacing: Signed::try_from(tick_spacing).expect("fits int24"),
        hooks: Address::ZERO,
    }
}

/// A `uint24` ratio or fee as the modules store it, in millionths.
pub(super) fn e6(value: u32) -> Uint<24, 1> {
    Uint::from(value)
}

/// `openInterest()` in perp atoms.
pub(super) fn open_interest(long: u128, short: u128) -> OpenInterest {
    OpenInterest { long, short }
}

/// `positions(id)` for a position holding this margin and no exposure.
/// Zero margin and zero delta together are how the contract reports a
/// position that never existed or was burned.
pub(super) fn position(margin: u128) -> Position {
    Position {
        delta: I256::ZERO,
        margin,
        liqMarginRatio: Uint::ZERO,
        backstopMarginRatio: Uint::ZERO,
        lastCumlFundingX96: I256::ZERO,
    }
}

/// A price as the contract stores it: `mantissa / 2^shift`, in Q96.
pub(super) fn x96(mantissa: u64, shift: u32) -> U256 {
    U256::from(mantissa) << (96 - shift)
}
