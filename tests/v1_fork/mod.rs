//! A `v1.0.0` market on an Anvil fork of Arbitrum Sepolia, built by the
//! W11 smoke steps with the deployed `v1.0.0` factory, modules and beacon
//! factories, and Anvil's public dev key. Nothing is sent to a real chain.
//!
//! `SEPOLIA_FORK_URL` overrides the endpoint (default: the public Arbitrum
//! Sepolia RPC; the fork is at the head, so no archive is needed).
//!
//! Each step is a method, so a test can stop part way or add its own steps;
//! [`V1Fork::build_w11_market`] runs all fourteen in order.

#![allow(dead_code)]

use std::env;
use std::io::Read;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use alloy::consensus::Transaction;
use alloy::contract::{CallBuilder, CallDecoder};
use alloy::eips::BlockNumberOrTag;
use alloy::hex;
use alloy::network::EthereumWallet;
use alloy::primitives::aliases::{U24, U48};
use alloy::primitives::{Address, B256, I256, U256, address, keccak256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionReceipt;
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolValue;
use tokio::time::sleep;

use perpcity_sdk::contracts::v1::{
    AdjustMakerParams, AdjustTakerParams, Modules, MoveMakerParams, OpenMakerParams,
    OpenTakerParams, Perp, PerpFactory, WithdrawalConfig,
};

sol! {
    /// The test USDC on Arbitrum Sepolia: a public `mint`.
    #[sol(rpc)]
    interface ITestUsdc {
        function mint(address to, uint256 amount) external;
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
    }

    /// Beacons `f762da1`, `ECDSAVerifierFactory`.
    #[sol(rpc)]
    interface IEcdsaVerifierFactory {
        function createVerifier(address signer) external returns (address);
    }

    /// Beacons `f762da1`, `IdentityFactory` (the identity beacon factory).
    #[sol(rpc)]
    interface IIdentityBeaconFactory {
        function createBeacon(address verifier, uint256 initialIndex) external returns (address);
    }

    /// Beacons `f762da1`, `ECDSAVerifier`.
    #[sol(rpc)]
    interface IEcdsaVerifier {
        function SIGNER() external view returns (address);
        function binder() external view returns (address);
        function authorizedCaller() external view returns (address);
        function bind(address caller) external;
        function digest(uint256[] values, uint256 nonce) external view returns (bytes32);
        function lastNonce() external view returns (uint256);
    }

    /// Beacons `f762da1`, `IdentityBeacon`.
    #[sol(rpc)]
    interface IIdentityBeacon {
        function verifier() external view returns (address);
        function update(bytes proof, bytes inputs) external;
    }
}

// ── Arbitrum Sepolia ONLY: the same address is another contract on One ──

pub const SEPOLIA_CHAIN_ID: u64 = 421_614;
pub const USDC: Address = address!("BEF280BefeE2Cb28c20D1E4Cc1da999B4DA0f1fD");
pub const FACTORY: Address = address!("FD7491670e03c522078448d87411091c23b757Fb");
pub const PERP_IMPLEMENTATION: Address = address!("A3b26624250922C3E274816779d26cFE308b7190");
pub const HOOK: Address = address!("9FCBEc9019d37780A0c21573cC4F556691252aA0");
pub const FEES: Address = address!("8BB0812C621A427FfD0B0686a64006Ab9dba9c2a");
pub const FUNDING: Address = address!("a92aB4E82B0d571e1A0DF18c583232688B196477");
pub const MARGIN_RATIOS: Address = address!("48dfbffA972A000e13b900AD131B6E5Bc40CE5Bd");
pub const PRICE_IMPACT: Address = address!("Fc66d25a5AFe713F42703C4feF2029E3De2616AB");
pub const PRICING: Address = address!("BB679a4c15Dd4e8eC8016FA7FE42097017cbA11E");
pub const IDENTITY_BEACON_FACTORY: Address = address!("25b1306CADe5861eF38B216DD3CA2E80C986Bd84");
pub const ECDSA_VERIFIER_FACTORY: Address = address!("1e033361E33fa25D276DecCB01EF9C7b256C2085");
pub const POOL_MANAGER: Address = address!("FB3e0C6F74eB1a21CC1Da29aeC80D2Dfe6C9a317");

/// A fixed gas limit for every send. Anvil's estimate on this fork can be
/// too tight for the market's try/catch beacon read: the transaction then
/// mines and reverts below its limit. Each [`Step`] keeps the estimate.
const SEND_GAS: u64 = 10_000_000;

/// The seconds between two blocks. Anvil's default clock gives two blocks
/// in the same wall-clock second the same timestamp, and a market accrues
/// (`CumulativesAccrued`) only when the time has moved since its last touch.
pub const BLOCK_INTERVAL: u64 = 2;

/// Anvil's default private key #0 (well-known, test-only).
const ANVIL_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

// ── The W11 parameters ──────────────────────────────────────────────

pub const EMA_WINDOW: u32 = 600;
pub const LATENCY: u32 = 86_400;
pub const MAKER_MARGIN: u128 = 100_000_000;
pub const MAKER_TICKS: (i32, i32) = (38_700, 39_540);
pub const MAKER_LIQUIDITY: u128 = 335_000_000;
pub const MOVE_TICKS: (i32, i32) = (38_400, 39_840);
pub const MOVE_LIQUIDITY: u128 = 150_000_000;
pub const REDUCE_LIQUIDITY: u128 = 50_000_000;
pub const TAKER_MARGIN: u128 = 10_000_000;
pub const TAKER_SIZE: i64 = 400_000;
pub const MAX_TOTAL_FEE: u64 = 2_000_000;

/// `50 * 2^96`, the first index.
pub fn index_1() -> U256 {
    U256::from(50u64) << 96
}

/// `50.5 * 2^96`, the second index (+1%).
pub fn index_2() -> U256 {
    U256::from(101u64) << 95
}

pub fn fork_url() -> String {
    env::var("SEPOLIA_FORK_URL")
        .unwrap_or_else(|_| "https://sepolia-rollup.arbitrum.io/rpc".to_string())
}

/// An Anvil process forking Arbitrum Sepolia at the head, on a free port,
/// with a block every [`BLOCK_INTERVAL`] seconds of chain time.
pub struct SepoliaFork {
    child: Child,
    pub url: String,
}

impl SepoliaFork {
    pub async fn start() -> Self {
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("no free local port")
            .port();
        let url = format!("http://127.0.0.1:{port}");
        let fork_url = fork_url();
        let child = Command::new("anvil")
            .args([
                "--fork-url",
                &fork_url,
                "--port",
                &port.to_string(),
                "--timeout",
                "60000",
                "--retries",
                "5",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to start anvil — is it installed? (`foundryup`)");
        let mut fork = Self { child, url };
        let probe = ProviderBuilder::new().connect_http(fork.url.parse().unwrap());
        for _ in 0..120 {
            sleep(Duration::from_millis(500)).await;
            fork.check_running();
            if let Ok(id) = probe.get_chain_id().await {
                // The answer is this process's only if it still runs: an
                // Anvil that lost the port exits, and another one answers.
                fork.check_running();
                assert_eq!(id, SEPOLIA_CHAIN_ID, "the fork is not Arbitrum Sepolia");
                let _: serde_json::Value = probe
                    .raw_request("anvil_setBlockTimestampInterval".into(), (BLOCK_INTERVAL,))
                    .await
                    .unwrap();
                return fork;
            }
        }
        panic!("Anvil did not become ready within 60 seconds");
    }

    fn check_running(&mut self) {
        if let Ok(Some(status)) = self.child.try_wait() {
            let mut err = String::new();
            if let Some(mut stderr) = self.child.stderr.take() {
                let _ = stderr.read_to_string(&mut err);
            }
            panic!("anvil exited ({status}): {err}");
        }
    }
}

impl Drop for SepoliaFork {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One mined step: its W11 label, its receipt, its block's timestamp, and
/// Anvil's gas estimate for it (`None` when the estimate failed).
pub struct Step {
    pub label: &'static str,
    pub receipt: TransactionReceipt,
    pub timestamp: u64,
    pub estimate: Option<u64>,
}

/// The market the fourteen steps leave: one open maker (moved, then
/// reduced), one closed taker, and the beacon at the second index.
pub struct V1Market {
    pub verifier: Address,
    pub beacon: Address,
    pub perp: Address,
    pub controller: Address,
    pub maker_id: U256,
    pub taker_id: U256,
}

/// The dev wallet on the fork and the steps it has mined.
pub struct V1Fork {
    pub provider: DynProvider,
    pub signer: PrivateKeySigner,
    pub me: Address,
    pub steps: Vec<Step>,
}

impl V1Fork {
    pub fn new(fork: &SepoliaFork) -> Self {
        let signer: PrivateKeySigner = ANVIL_KEY.parse().unwrap();
        let me = signer.address();
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer.clone()))
            .connect_http(fork.url.parse().unwrap())
            .erased();
        Self {
            provider,
            signer,
            me,
            steps: Vec::new(),
        }
    }

    /// The fork must hold the Sepolia `v1.0.0` deploy, not another one.
    pub async fn check_deploy(&self) {
        let factory = PerpFactory::new(FACTORY, &self.provider);
        assert_eq!(
            factory.perpImplementation().call().await.unwrap(),
            PERP_IMPLEMENTATION,
            "factory implementation differs: not the Sepolia v1.0.0 deploy"
        );
        assert_eq!(factory.PERP_GUARD_HOOK().call().await.unwrap(), HOOK);
    }

    async fn block_time(&self, block: BlockNumberOrTag) -> u64 {
        self.provider
            .get_block_by_number(block)
            .await
            .unwrap()
            .unwrap()
            .header
            .timestamp
    }

    /// The latest block's timestamp.
    pub async fn now(&self) -> u64 {
        self.block_time(BlockNumberOrTag::Latest).await
    }

    /// One hour from the fork's clock.
    pub async fn deadline(&self) -> u64 {
        self.now().await + 3_600
    }

    /// Estimate `call` from the dev wallet, send it with [`SEND_GAS`], and
    /// keep the mined step. A revert fails the test.
    async fn send<P: Provider, D: CallDecoder>(
        &mut self,
        label: &'static str,
        call: CallBuilder<P, D>,
    ) -> &TransactionReceipt {
        let call = call.from(self.me);
        let estimate = call.estimate_gas().await.ok();
        let receipt = call
            .gas(SEND_GAS)
            .send()
            .await
            .unwrap_or_else(|e| panic!("{label}: send failed: {e}"))
            .get_receipt()
            .await
            .unwrap_or_else(|e| panic!("{label}: no receipt: {e}"));
        if !receipt.status() {
            let tx = self
                .provider
                .get_transaction_by_hash(receipt.transaction_hash)
                .await
                .unwrap()
                .unwrap();
            panic!(
                "{label}: reverted, gas used {} of limit {}",
                receipt.gas_used,
                tx.gas_limit()
            );
        }
        let block = receipt.block_number.expect("a mined receipt has a block");
        let timestamp = self.block_time(block.into()).await;
        self.steps.push(Step {
            label,
            receipt,
            timestamp,
            estimate,
        });
        &self.steps.last().unwrap().receipt
    }

    // ── 1. test USDC ────────────────────────────────────────────────

    pub async fn mint_usdc(&mut self) {
        let usdc = ITestUsdc::new(USDC, self.provider.clone());
        self.send(
            "mint-usdc",
            usdc.mint(self.me, U256::from(1_000_000_000u64)),
        )
        .await;
    }

    // ── 2-4. verifier, beacon, bind ─────────────────────────────────

    pub async fn create_verifier(&mut self) -> Address {
        let factory = IEcdsaVerifierFactory::new(ECDSA_VERIFIER_FACTORY, self.provider.clone());
        let call = factory.createVerifier(self.me);
        let verifier = call.call().await.unwrap();
        self.send("create-verifier", call).await;
        let v = IEcdsaVerifier::new(verifier, &self.provider);
        assert_eq!(v.SIGNER().call().await.unwrap(), self.me);
        assert_eq!(v.binder().call().await.unwrap(), self.me);
        verifier
    }

    pub async fn create_beacon(&mut self, verifier: Address) -> Address {
        let factory = IIdentityBeaconFactory::new(IDENTITY_BEACON_FACTORY, self.provider.clone());
        let call = factory.createBeacon(verifier, index_1());
        let beacon = call.call().await.unwrap();
        self.send("create-beacon", call).await;
        let b = IIdentityBeacon::new(beacon, &self.provider);
        assert_eq!(b.verifier().call().await.unwrap(), verifier);
        beacon
    }

    pub async fn bind(&mut self, verifier: Address, beacon: Address) {
        let v = IEcdsaVerifier::new(verifier, self.provider.clone());
        self.send("bind", v.bind(beacon)).await;
        assert_eq!(v.authorizedCaller().call().await.unwrap(), beacon);
    }

    // ── 5, 10. signed index updates ─────────────────────────────────

    /// Sign `Measurement([index], lastNonce + 1)` with the dev key and push
    /// it to the beacon.
    pub async fn push_index(
        &mut self,
        label: &'static str,
        verifier: Address,
        beacon: Address,
        index: U256,
    ) {
        let v = IEcdsaVerifier::new(verifier, self.provider.clone());
        let nonce = v.lastNonce().call().await.unwrap() + U256::from(1);
        let digest: B256 = v.digest(vec![index], nonce).call().await.unwrap();
        let sig = self.signer.sign_hash_sync(&digest).unwrap();
        let inputs = (vec![index], nonce).abi_encode_params();
        let b = IIdentityBeacon::new(beacon, self.provider.clone());
        let update = b.update(sig.as_bytes().to_vec().into(), inputs.into());
        if let Err(e) = update.call().await {
            panic!(
                "{label}: the update reverts: {e}; sig {}",
                hex::encode(sig.as_bytes())
            );
        }
        self.send(label, update).await;
        assert_eq!(v.lastNonce().call().await.unwrap(), nonce);
    }

    // ── 6-7. the market ─────────────────────────────────────────────

    pub async fn create_perp(&mut self, beacon: Address) -> Address {
        let factory = PerpFactory::new(FACTORY, self.provider.clone());
        let salt = keccak256(format!("v1-fork-{}-{}", self.me, self.now().await));
        let modules = Modules {
            beacon,
            fees: FEES,
            funding: FUNDING,
            marginRatios: MARGIN_RATIOS,
            priceImpact: PRICE_IMPACT,
            pricing: PRICING,
        };
        let config = WithdrawalConfig {
            capacity: u128::MAX,
            refillPerSecond: u128::MAX,
            challengeWindow: U48::from(86_400u64),
        };
        let call = factory.createPerp(
            self.me,
            self.me,
            modules,
            U24::from(EMA_WINDOW),
            LATENCY,
            salt,
            config,
        );
        let perp = call.call().await.unwrap();
        self.send("create-perp", call).await;
        assert_eq!(
            Perp::new(perp, &self.provider)
                .owner()
                .call()
                .await
                .unwrap(),
            self.me
        );
        perp
    }

    pub async fn approve_usdc(&mut self, perp: Address) {
        let usdc = ITestUsdc::new(USDC, self.provider.clone());
        self.send("approve-usdc", usdc.approve(perp, U256::MAX))
            .await;
    }

    // ── 8-9. maker and taker opens ──────────────────────────────────

    pub async fn open_maker(&mut self, perp: Address) -> U256 {
        let p = Perp::new(perp, self.provider.clone());
        let id = p.nextPosId().call().await.unwrap();
        let params = OpenMakerParams {
            holder: self.me,
            margin: MAKER_MARGIN,
            tickLower: MAKER_TICKS.0.try_into().unwrap(),
            tickUpper: MAKER_TICKS.1.try_into().unwrap(),
            liquidity: MAKER_LIQUIDITY,
            maxAmt0In: U256::MAX,
            maxAmt1In: U256::MAX,
            deadline: self.deadline().await,
        };
        self.send("open-maker", p.openMaker(params)).await;
        assert_eq!(p.positionOwner(id).call().await.unwrap(), self.me);
        id
    }

    pub async fn open_taker(&mut self, perp: Address) -> U256 {
        let p = Perp::new(perp, self.provider.clone());
        let id = p.nextPosId().call().await.unwrap();
        let params = OpenTakerParams {
            holder: self.me,
            margin: TAKER_MARGIN,
            perpDelta: I256::try_from(TAKER_SIZE).unwrap(),
            amt1Limit: U256::MAX,
            maxTotalFee: U256::from(MAX_TOTAL_FEE),
            deadline: self.deadline().await,
        };
        self.send("open-taker", p.openTaker(params)).await;
        assert_eq!(p.positionOwner(id).call().await.unwrap(), self.me);
        id
    }

    // ── 11-14. touch, taker close, maker move and reduce ────────────

    /// Mine the next block `seconds` after the latest one, in place of the
    /// [`BLOCK_INTERVAL`]; the blocks after it follow at the interval again.
    /// (`evm_increaseTime` does not move a clock that runs at an interval.)
    pub async fn warp(&self, seconds: u64) {
        let next = self.now().await + seconds;
        let _: serde_json::Value = self
            .provider
            .raw_request("evm_setNextBlockTimestamp".into(), (next,))
            .await
            .unwrap();
    }

    /// A minute after the second index, so the touch accrues over more
    /// than one block interval (W11 step 11 ran a minute after the opens).
    pub async fn touch(&mut self, perp: Address) {
        self.warp(60).await;
        let p = Perp::new(perp, self.provider.clone());
        self.send("touch", p.touch()).await;
    }

    pub async fn close_taker(&mut self, perp: Address, taker_id: U256) {
        let p = Perp::new(perp, self.provider.clone());
        let params = AdjustTakerParams {
            posId: taker_id,
            marginDelta: 0,
            perpDelta: I256::try_from(-TAKER_SIZE).unwrap(),
            amt1Limit: U256::ZERO,
            maxTotalFee: U256::from(MAX_TOTAL_FEE),
            reduceOnly: true,
            deadline: self.deadline().await,
        };
        self.send("close-taker", p.adjustTaker(params)).await;
        assert_eq!(
            p.positionOwner(taker_id).call().await.unwrap(),
            Address::ZERO
        );
    }

    pub async fn move_maker(&mut self, perp: Address, maker_id: U256) {
        let p = Perp::new(perp, self.provider.clone());
        let params = MoveMakerParams {
            posId: maker_id,
            tickLower: MOVE_TICKS.0.try_into().unwrap(),
            tickUpper: MOVE_TICKS.1.try_into().unwrap(),
            liquidity: MOVE_LIQUIDITY,
            removeAmt0Min: U256::ZERO,
            removeAmt1Min: U256::ZERO,
            addAmt0Max: U256::MAX,
            addAmt1Max: U256::MAX,
            marginDelta: 0,
            deadline: self.deadline().await,
        };
        self.send("move-maker", p.moveMaker(params)).await;
    }

    /// A partial reduce. The only maker cannot close in full while it has a
    /// residual short: no other maker gives the close depth (W11 fact 1).
    pub async fn reduce_maker(&mut self, perp: Address, maker_id: U256) {
        let p = Perp::new(perp, self.provider.clone());
        let params = AdjustMakerParams {
            posId: maker_id,
            marginDelta: 0,
            liquidityDelta: -(REDUCE_LIQUIDITY as i128),
            amt0Limit: U256::ZERO,
            amt1Limit: U256::ZERO,
            deadline: self.deadline().await,
        };
        self.send("reduce-maker", p.adjustMaker(params)).await;
    }

    /// The fourteen W11 steps, in order.
    pub async fn build_w11_market(&mut self) -> V1Market {
        self.check_deploy().await;
        self.mint_usdc().await;
        let verifier = self.create_verifier().await;
        let beacon = self.create_beacon(verifier).await;
        self.bind(verifier, beacon).await;
        self.push_index("update-1", verifier, beacon, index_1())
            .await;
        let perp = self.create_perp(beacon).await;
        let controller = Perp::new(perp, &self.provider)
            .withdrawalController()
            .call()
            .await
            .unwrap();
        self.approve_usdc(perp).await;
        let maker_id = self.open_maker(perp).await;
        let taker_id = self.open_taker(perp).await;
        self.push_index("update-2", verifier, beacon, index_2())
            .await;
        self.touch(perp).await;
        self.close_taker(perp, taker_id).await;
        self.move_maker(perp, maker_id).await;
        self.reduce_maker(perp, maker_id).await;
        assert_eq!(self.steps.len(), 14);
        V1Market {
            verifier,
            beacon,
            perp,
            controller,
            maker_id,
            taker_id,
        }
    }
}
