//! A [`PerpClient`] over a mocked JSON-RPC transport, so the reads can be
//! characterised without a node.
//!
//! The mock answers requests in arrival order from a queue the test fills
//! beforehand, and answers without inspecting a request. A test therefore
//! pins four things about a read: how many RPCs it makes (an unanswered
//! request fails, and [`Rpc::is_drained`] proves none was left over), what
//! it makes of the bytes it gets back, what it remembers between calls,
//! and, through [`Rpc::calls`], which calls it asked for.

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use alloy::consensus::Header as ConsensusHeader;
use alloy::hex;
use alloy::network::Ethereum;
use alloy::primitives::{Address, B256, Bytes, I256, Signed, U64, U256, Uint};
use alloy::providers::RootProvider;
use alloy::providers::bindings::IMulticall3 as Multicall;
use alloy::rpc::client::RpcClient;
use alloy::rpc::json_rpc::{ErrorPayload, RequestPacket, ResponsePacket};
use alloy::rpc::types::{
    Block, BlockTransactions, EIP1186AccountProofResponse, EIP1186StorageProof, Header,
};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use alloy::transports::mock::{Asserter, MockTransport};
use alloy::transports::{TransportError, TransportFut};
use serde_json::value::RawValue;
use tower::Service;

use crate::contracts::{
    Capacity, IMulticall3, Maker, MakerFunding, Modules, OpenInterest, Perp, PoolKey, Position,
    Rates, SolvencyState, v1,
};
use crate::convert::pack_balance_delta;
use crate::{HftTransport, TransportConfig};

use super::chain::ChainReader;
use super::queries::Era;
use super::{ChainDeployments, PerpClient};

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
/// The mocked chain's `v0.2.2` guard hook, the one [`pool_key_hooked`]
/// names; a known build only because the mocked deployments list it.
pub(super) const V022_HOOK: Address = Address::repeat_byte(0x0a);
/// The mocked chain's `v1.0.0` guard hook.
pub(super) const V1_HOOK: Address = Address::repeat_byte(0x0b);
/// The pool id [`Rpc::immutables`] answers.
pub(super) const POOL_ID: B256 = B256::repeat_byte(0x99);

/// A client whose every RPC is answered by the returned [`Rpc`].
pub(super) fn client() -> (PerpClient, Rpc) {
    client_sharing(transport())
}

/// A client whose every RPC is answered by the returned [`Rpc`], over a
/// transport the test hands in — the same one to several clients, to
/// test what the transport remembers on their behalf.
pub(super) fn client_sharing(transport: HftTransport) -> (PerpClient, Rpc) {
    let (provider, rpc) = mocked();
    (client_with(provider, transport), rpc)
}

/// A provider over the mock's queue, and the queue.
fn mocked() -> (RootProvider<Ethereum>, Rpc) {
    let recorder = Recorder {
        mock: MockTransport::new(Asserter::new()),
        calls: Arc::default(),
    };
    let rpc = Rpc {
        asserter: recorder.mock.asserter().clone(),
        calls: Arc::clone(&recorder.calls),
    };
    (RootProvider::new(RpcClient::new(recorder, true)), rpc)
}

/// The mock transport, keeping the calldata of every `eth_call` it is
/// asked, in arrival order.
#[derive(Clone)]
struct Recorder {
    mock: MockTransport,
    calls: Arc<Mutex<Vec<Bytes>>>,
}

impl Service<RequestPacket> for Recorder {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.mock.poll_ready(cx)
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        for req in req.requests() {
            if req.method() != "eth_call" {
                continue;
            }
            let params: serde_json::Value =
                serde_json::from_str(req.params().map_or("null", RawValue::get))
                    .expect("a request's params are JSON");
            let call = &params[0];
            let input = call["input"].as_str().or_else(|| call["data"].as_str());
            if let Some(input) = input {
                let calldata = hex::decode(input).expect("calldata is hex");
                self.calls.lock().unwrap().push(calldata.into());
            }
        }
        self.mock.call(req)
    }
}

/// A client over any provider — a mocked one, or a
/// [`FakeNode`](crate::history::test_support::FakeNode)'s — with a fixed
/// signer and deployments.
pub(super) fn client_over(provider: RootProvider<Ethereum>) -> PerpClient {
    client_with(provider, transport())
}

/// A client on a real chain's deployments and id, every RPC answered by
/// the returned [`Rpc`]: the hooks its markets' pool keys name are that
/// chain's.
pub(super) fn client_on(deployments: ChainDeployments, chain_id: u64) -> (PerpClient, Rpc) {
    let (provider, rpc) = mocked();
    let chain = ChainReader::from_parts(provider, transport(), deployments, chain_id);
    (PerpClient::new(chain.market(PERP), signer()), rpc)
}

/// A chain reader on its own, every RPC answered by the returned [`Rpc`].
pub(super) fn chain() -> (ChainReader, Rpc) {
    let (provider, rpc) = mocked();
    (chain_with(provider, transport()), rpc)
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

fn chain_with(provider: RootProvider<Ethereum>, transport: HftTransport) -> ChainReader {
    let deployments = ChainDeployments {
        usdc: USDC,
        pool_manager: POOL_MANAGER,
        v022_perp_guard_hook: V022_HOOK,
        v1_perp_guard_hook: V1_HOOK,
        v1_perp_factory: Address::repeat_byte(0x0c),
        v1_withdrawal_controller_impl: Address::repeat_byte(0x0d),
    };
    ChainReader::from_parts(provider, transport, deployments, CHAIN_ID)
}

fn client_with(provider: RootProvider<Ethereum>, transport: HftTransport) -> PerpClient {
    PerpClient::new(chain_with(provider, transport).market(PERP), signer())
}

/// The mocked clients' fixed signer.
fn signer() -> PrivateKeySigner {
    PrivateKeySigner::from_bytes(&B256::repeat_byte(0x01))
        .expect("a non-zero scalar is a valid key")
}

/// The mock's answer queue, in the vocabulary of the reads, and the calls
/// it was asked.
pub(super) struct Rpc {
    asserter: Asserter,
    calls: Arc<Mutex<Vec<Bytes>>>,
}

impl Rpc {
    /// The next `eth_call` returns `ret`, encoded as `C`'s return.
    pub(super) fn call<C: SolCall>(&self, ret: &C::Return) {
        self.asserter.push_success(&Bytes::from(returns::<C>(ret)));
    }

    /// The next `eth_call` returns these bytes as they are: an answer in
    /// a shape no binding of the caller's encodes.
    pub(super) fn raw(&self, data: &[u8]) {
        self.asserter.push_success(&Bytes::copy_from_slice(data));
    }

    /// The next two `eth_call`s are the immutables of a market of `era`:
    /// its pool key, naming the mocked chain's hook for the build, then
    /// the build's pool id view.
    pub(super) fn immutables(&self, era: Era) {
        let hook = match era {
            Era::Legacy => Address::ZERO,
            Era::Upgradeable => V022_HOOK,
            Era::V1 => V1_HOOK,
        };
        self.call::<Perp::poolKeyCall>(&pool_key_with_hook(30, hook));
        match era {
            Era::Legacy | Era::Upgradeable => self.call::<Perp::POOL_IDCall>(&POOL_ID),
            Era::V1 => self.call::<v1::Perp::poolIdCall>(&POOL_ID),
        }
    }

    /// The calldata of every `eth_call` asked so far, in order; a
    /// multicall is one entry.
    pub(super) fn calls(&self) -> Vec<Bytes> {
        self.calls.lock().unwrap().clone()
    }

    /// The selector of every `eth_call` asked so far, and of every call
    /// inside an `aggregate3` in its place, in order.
    pub(super) fn selectors(&self) -> Vec<[u8; 4]> {
        self.calls()
            .iter()
            .flat_map(
                |calldata| match IMulticall3::aggregate3Call::abi_decode(calldata) {
                    Ok(batch) => batch
                        .calls
                        .iter()
                        .map(|call| selector(&call.callData))
                        .collect(),
                    Err(_) => vec![selector(calldata)],
                },
            )
            .collect()
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
        self.asserter.push_success(&U64::from(n));
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
        self.asserter.push_success(&block);
        hash
    }

    /// The next `eth_getBlockByNumber` finds nothing.
    pub(super) fn no_block(&self) {
        self.asserter.push_success(&Option::<Block>::None);
    }

    /// The next `eth_getStorageAt` returns `word`.
    pub(super) fn storage(&self, word: U256) {
        self.asserter.push_success(&word);
    }

    /// The next `eth_getProof` proves these `(slot, value)` pairs of
    /// `account`'s storage; the proofs themselves are empty, since the
    /// reads take the values on trust.
    pub(super) fn proof(&self, account: Address, slots: impl IntoIterator<Item = (U256, U256)>) {
        self.asserter.push_success(&EIP1186AccountProofResponse {
            address: account,
            balance: U256::ZERO,
            code_hash: B256::ZERO,
            nonce: 0,
            storage_hash: B256::ZERO,
            account_proof: Vec::new(),
            storage_proof: slots
                .into_iter()
                .map(|(slot, value)| EIP1186StorageProof {
                    key: B256::from(slot).into(),
                    value,
                    proof: Vec::new(),
                })
                .collect(),
        });
    }

    /// The next request fails as the node's own error, with no revert
    /// data: a transport-level failure.
    pub(super) fn fails(&self, message: &'static str) {
        self.asserter.push_failure_msg(message);
    }

    /// The next request is refused as a method the endpoint does not
    /// serve (JSON-RPC `-32601`), the one answer worth remembering.
    pub(super) fn method_not_found(&self) {
        self.asserter.push_failure(ErrorPayload {
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
        self.asserter.push_failure(ErrorPayload {
            code: 3,
            message: "execution reverted".into(),
            data: Some(data),
        });
    }

    /// Whether every queued answer was consumed: the reads made exactly
    /// the RPCs the test expected.
    pub(super) fn is_drained(&self) -> bool {
        self.asserter.read_q().is_empty()
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

/// `poolState()` sitting at this tick; the rest of the slot is zero.
pub(super) fn pool_state_at_tick(tick: i32) -> Perp::poolStateReturn {
    Perp::poolStateReturn {
        tick: Signed::try_from(tick).expect("a test tick fits int24"),
        sqrtPrice: Uint::ZERO,
        ammPrice: U256::ZERO,
        liquidity: 0,
    }
}

/// `solvencyState()` in 6-decimal atoms.
pub(super) fn solvency(bad_debt: u128, total_margin: u128) -> SolvencyState {
    SolvencyState {
        badDebt: bad_debt,
        totalMargin: total_margin,
    }
}

/// `makerDetails(id)` for a range over `[lower, upper]` holding
/// `liquidity`; the accumulators are zero.
pub(super) fn maker(lower: i32, upper: i32, liquidity: u128) -> Maker {
    Maker {
        tickLower: Signed::try_from(lower).expect("a test tick fits int24"),
        tickUpper: Signed::try_from(upper).expect("a test tick fits int24"),
        liquidity,
        lastLongUtilEarningsX96: U256::ZERO,
        lastShortUtilEarningsX96: U256::ZERO,
        capacity: Capacity { long: 0, short: 0 },
        lastCumlFunding: MakerFunding {
            belowX96: I256::ZERO,
            withinX96: I256::ZERO,
            divSqrtPriceWithinX96: I256::ZERO,
        },
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

/// The stored EMA pair as its storage word holds it: `ammPrice` low,
/// `index` high, both Q96 narrowed to `uint128`.
pub(super) fn emas_word(amm_x96: u128, index_x96: u128) -> U256 {
    (U256::from(index_x96) << 128) | U256::from(amm_x96)
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

/// `poolKey()` with this tick spacing, hookless: a build `58b42b7` market.
pub(super) fn pool_key(tick_spacing: i32) -> PoolKey {
    PoolKey {
        currency0: Address::ZERO,
        currency1: Address::ZERO,
        fee: Uint::ZERO,
        tickSpacing: Signed::try_from(tick_spacing).expect("fits int24"),
        hooks: Address::ZERO,
    }
}

/// `poolKey()` with the mocked chain's `v0.2.2` guard hook: a
/// `v0.2.2-upgradeable` market.
pub(super) fn pool_key_hooked(tick_spacing: i32) -> PoolKey {
    pool_key_with_hook(tick_spacing, V022_HOOK)
}

/// `poolKey()` naming `hooks`.
pub(super) fn pool_key_with_hook(tick_spacing: i32, hooks: Address) -> PoolKey {
    PoolKey {
        hooks,
        ..pool_key(tick_spacing)
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

/// One ABI word of a signed value.
fn signed_word(value: i128) -> [u8; 32] {
    I256::try_from(value)
        .expect("i128 fits I256")
        .to_be_bytes::<32>()
}

/// One ABI word of an unsigned value.
fn word(value: u128) -> [u8; 32] {
    U256::from(value).to_be_bytes::<32>()
}

/// A `v1.0.0` taker's row as the chain returns it, three words:
/// `delta` (the packed `BalanceDelta`, `amount0` high), `margin`,
/// `lastCumlFundingX96`.
pub(super) fn v1_position_words(perp: i128, usd: i128, margin: u128, funding: i128) -> Vec<u8> {
    [
        pack_balance_delta(perp, usd).to_be_bytes::<32>(),
        word(margin),
        signed_word(funding),
    ]
    .concat()
}

/// A `v1.0.0` maker's details as the chain returns them, seven words:
/// the band (`tickLower`, `tickUpper`, `liquidity`), the fee-growth
/// checkpoint, the nested `MakerFunding` (`belowX96`,
/// `withinInventoryX192`) and the funding correction.
pub(super) fn v1_maker_words(lower: i32, upper: i32, liquidity: u128) -> Vec<u8> {
    [
        signed_word(lower.into()),
        signed_word(upper.into()),
        word(liquidity),
        word(7),
        signed_word(-8),
        signed_word(9),
        signed_word(-10),
    ]
    .concat()
}

/// A call's selector: its first four bytes.
fn selector(calldata: &[u8]) -> [u8; 4] {
    calldata[..4].try_into().expect("a call has a selector")
}

/// A price as the contract stores it: `mantissa / 2^shift`, in Q96.
pub(super) fn x96(mantissa: u64, shift: u32) -> U256 {
    U256::from(mantissa) << (96 - shift)
}
