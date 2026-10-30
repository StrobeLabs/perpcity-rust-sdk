//! Operations for contract source revision `5141c2f`, pending deployment review.
//!
//! Operators supply reviewed deployment addresses and code hashes. No production
//! defaults are included. Each read checks the code and proxy implementation at its
//! block. Existing legacy APIs remain unchanged.

use std::result::Result as StdResult;

use alloy::primitives::{Address, B256, Bytes, U256, Uint, b256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionReceipt;
use alloy::sol_types::{SolCall, SolValue};
use serde::de::Error as DeserializeError;
use serde::{Deserialize, Deserializer, Serialize};

use crate::contracts::IERC20;
use crate::contracts::operations::{
    OperationsController, OperationsFees, OperationsMultiSend, OperationsPerp, OperationsSafe,
};
use crate::errors::{ContractError, PerpCityError, Result, ValidationError};
use crate::math::BlockContext;
use crate::units::UsdcAtoms;

use super::state::pinned_read_error;
use super::{ChainReader, PerpClient, StateAt};

const IMPLEMENTATION_SLOT: B256 =
    b256!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");

/// Expected proxy and implementation for the new contract interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentDeployment {
    /// Runtime hash of the market proxy.
    pub proxy_code_hash: B256,
    /// Expected ERC-1967 implementation address.
    pub implementation: Address,
    /// Runtime hash at the implementation address.
    pub implementation_code_hash: B256,
}

/// A reviewed non-proxy contract and its runtime hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractCode {
    /// Contract address.
    pub address: Address,
    /// Keccak-256 of its complete runtime code.
    pub code_hash: B256,
}

/// Encoded call. The caller must verify that the target deployment supports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractCall {
    /// Called contract.
    pub to: Address,
    /// ABI calldata.
    pub data: Bytes,
    /// Native currency, in wei.
    pub value: u128,
}

impl ContractCall {
    /// Simulate as the executor without signing.
    pub async fn simulate(&self, chain: &ChainReader, from: Address) -> Result<()> {
        Ok(chain
            .preflight_call(from, self.to, &self.data, self.value, None)
            .await?)
    }

    /// Submit once through the existing transaction pipeline.
    /// The caller must recheck any conditions that can change before sending.
    pub async fn send(&self, client: &PerpClient) -> Result<TransactionReceipt> {
        client
            .tx(self.to, self.data.clone())
            .with_value(self.value)
            .send()
            .await
    }
}

/// Market state at one block. TVL figures use USDC atoms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationalState {
    /// Observation block.
    pub block: BlockContext,
    /// Market address.
    pub market: Address,
    /// Current market owner and controller guardian.
    pub owner: Address,
    /// Current governor.
    pub governor: Address,
    /// Latest rate checkpoint timestamp.
    pub last_touch: u64,
    /// Admission cap in USDC atoms. U256::MAX permits any representable TVL.
    pub tvl_cap: U256,
    /// Contract-accounted TVL in USDC atoms, excluding controller escrow.
    pub accounted_tvl: U256,
    /// Active fees module.
    pub fees: Address,
    /// The active controller. Zero disables controller routing.
    pub controller: Address,
}

/// The three utilization curve parameters, each scaled by 1e18.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UtilizationCurve {
    /// Utilization kink, strictly between zero and 1e18.
    target: U256,
    /// Slope below the kink.
    before: U256,
    /// Slope above the kink.
    after: U256,
}

impl<'de> Deserialize<'de> for UtilizationCurve {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> StdResult<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Parameters {
            target: U256,
            before: U256,
            after: U256,
        }
        let parameters = Parameters::deserialize(deserializer)?;
        Self::new(parameters.target, parameters.before, parameters.after).map_err(D::Error::custom)
    }
}

impl UtilizationCurve {
    /// Construct a curve within the contract's kink, slope and daily-rate bounds.
    pub fn new(target: U256, before: U256, after: U256) -> Result<Self> {
        let curve = Self {
            target,
            before,
            after,
        };
        curve.validate()?;
        Ok(curve)
    }

    /// Utilization kink in WAD.
    pub fn target(self) -> U256 {
        self.target
    }

    /// Slope below the kink in WAD.
    pub fn before(self) -> U256 {
        self.before
    }

    /// Slope above the kink in WAD.
    pub fn after(self) -> U256 {
        self.after
    }

    /// Enforce the source contract's parameter bounds with checked arithmetic.
    fn validate(&self) -> Result<()> {
        let wad = U256::from(1_000_000_000_000_000_000u64);
        if self.target == U256::ZERO || self.target >= wad || self.after < self.before {
            return Err(invalid("invalid utilization kink or slope ordering"));
        }
        let a = self
            .target
            .checked_mul(self.before)
            .ok_or_else(|| invalid("curve overflow"))?
            / wad;
        let b = (wad - self.target)
            .checked_mul(self.after)
            .ok_or_else(|| invalid("curve overflow"))?
            / wad;
        if a.checked_add(b).is_none_or(|rate| rate > wad) {
            return Err(invalid("utilization curve exceeds 100% per day"));
        }
        Ok(())
    }
}

/// The fee module parameters and owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeState {
    /// Observation block.
    pub block: BlockContext,
    /// Module address.
    pub address: Address,
    /// The fee module owner, which can differ from the market owner.
    pub owner: Address,
    /// Current utilization curve.
    pub curve: UtilizationCurve,
}

/// Controller configuration and reservation state at one block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerState {
    /// Observation block.
    pub block: BlockContext,
    /// Controller address.
    pub address: Address,
    /// Permanently bound market, verified against the state handle.
    pub market: Address,
    /// Current delay in seconds, including for already pending claims.
    pub challenge_window: u64,
    /// Blocks both immediate payouts and claim release.
    pub releases_paused: bool,
    /// Maximum immediate payout burst.
    pub capacity: UsdcAtoms,
    /// Stored bucket balance, before time-dependent refill.
    pub available: UsdcAtoms,
    /// Bucket refill per second.
    pub refill_per_second: UsdcAtoms,
    /// Stored bucket checkpoint timestamp.
    pub last_updated: u64,
    /// Total pending reservation in USDC atoms.
    pub total_reserved: U256,
    /// Next monotonically increasing claim identifier.
    pub next_claim_id: U256,
}

/// Source contract's claim lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    /// No claim under this identifier.
    None,
    /// The claim remains in escrow until release or cancellation.
    Pending,
    /// Paid to the recipient.
    Completed,
    /// Reduced to zero.
    Cancelled,
}

/// One claim read from a verified controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithdrawalClaim {
    /// Controller-local claim id.
    pub id: U256,
    /// Original amount.
    pub original_amount: UsdcAtoms,
    /// Remaining escrow amount.
    pub current_amount: UsdcAtoms,
    /// Immutable payout recipient.
    pub recipient: Address,
    /// Creation timestamp.
    pub created_at: u64,
    /// Current lifecycle status.
    pub status: ClaimStatus,
    /// Individual pause flag.
    pub paused: bool,
}

/// Which backstop entry point acquires the position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackstopKind {
    /// Maker liquidity position.
    Maker,
    /// Taker directional position.
    Taker,
}

/// Exact funded acquisition with a contract-enforced deadline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackstopOrder {
    /// Expected kind.
    pub kind: BackstopKind,
    /// Position id.
    pub position: U256,
    /// Exact collateral to deposit.
    pub margin: UsdcAtoms,
    /// New position owner.
    pub recipient: Address,
    /// Last allowed block timestamp.
    pub deadline: u64,
}

/// Position state used to detect changes before a backstop submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackstopPosition {
    /// Current owner.
    pub owner: Address,
    /// Keccak of owner, position, maker and taker storage returned by views.
    pub fingerprint: B256,
    /// Current classification from maker liquidity.
    pub kind: BackstopKind,
}

/// Funding and approval at the state handle's block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundingState {
    /// Holder's collateral balance.
    pub balance: UsdcAtoms,
    /// Collateral allowance to this market, in USDC atoms.
    pub allowance: U256,
}

/// Controller calls. The caller and contract must check authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ControllerAction {
    /// Guardian blocks releases and immediate payouts.
    PauseReleases,
    /// Guardian pauses one pending claim.
    PauseClaim {
        /// Claim id.
        claim: U256,
    },
    /// Guardian only decreases rate limits.
    LowerRateLimit {
        /// Burst limit.
        capacity: UsdcAtoms,
        /// Refill per second.
        refill_per_second: UsdcAtoms,
    },
    /// Guardian only increases the delay.
    IncreaseChallengeWindow {
        /// New delay.
        seconds: u64,
    },
    /// The guardian reduces a paused pending claim and pays the difference to the owner.
    ReduceClaim {
        /// Claim id.
        claim: U256,
        /// Remaining amount.
        new_amount: UsdcAtoms,
        /// Incident evidence digest.
        evidence: B256,
    },
    /// Governor unpauses releases.
    UnpauseReleases,
    /// Governor unpauses one claim.
    UnpauseClaim {
        /// Claim id.
        claim: U256,
    },
}

fn invalid(reason: &str) -> PerpCityError {
    ValidationError::InvalidConfig {
        reason: reason.into(),
    }
    .into()
}
fn unsupported(address: Address, reason: &str) -> PerpCityError {
    ContractError::UnsupportedOperations {
        address,
        reason: reason.into(),
    }
    .into()
}
fn call(to: Address, data: Vec<u8>) -> ContractCall {
    ContractCall {
        to,
        data: data.into(),
        value: 0,
    }
}

impl StateAt {
    /// Verify fresh proxy code, ERC-1967 implementation and implementation code.
    /// This check does not use the cached legacy era.
    pub async fn verify_current(&self, expected: &CurrentDeployment) -> Result<()> {
        let market = self.market().perp();
        if expected.implementation == Address::ZERO {
            return Err(unsupported(market, "zero implementation pin"));
        }
        self.verify_contract(ContractCode {
            address: market,
            code_hash: expected.proxy_code_hash,
        })
        .await?;
        let slot = self
            .market()
            .chain()
            .provider()
            .get_storage_at(market, U256::from_be_bytes(IMPLEMENTATION_SLOT.0))
            .block_id(self.id())
            .await
            .map_err(|e| pinned_read_error(e.into(), self.block().number))?;
        let bytes = slot.to_be_bytes::<32>();
        if bytes[..12].iter().any(|v| *v != 0)
            || Address::from_slice(&bytes[12..]) != expected.implementation
        {
            return Err(unsupported(market, "ERC-1967 implementation changed"));
        }
        self.verify_contract(ContractCode {
            address: expected.implementation,
            code_hash: expected.implementation_code_hash,
        })
        .await
    }

    /// Verify a reviewed non-proxy contract's runtime code at this block.
    pub async fn verify_contract(&self, expected: ContractCode) -> Result<()> {
        if expected.address == Address::ZERO || expected.code_hash == B256::ZERO {
            return Err(unsupported(expected.address, "missing code pin"));
        }
        let code = self
            .market()
            .chain()
            .provider()
            .get_code_at(expected.address)
            .block_id(self.id())
            .await
            .map_err(|e| pinned_read_error(e.into(), self.block().number))?;
        if code.is_empty() || keccak256(&code) != expected.code_hash {
            return Err(unsupported(
                expected.address,
                "runtime code does not match reviewed pin",
            ));
        }
        Ok(())
    }

    /// Read the last checkpoint without calling TVL or controller getters.
    pub async fn operational_last_touch(&self, expected: &CurrentDeployment) -> Result<u64> {
        self.verify_current(expected).await?;
        let c = OperationsPerp::new(self.market().perp(), self.market().chain().provider());
        Ok(c.rates()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?
            .lastTouch
            .to::<u64>())
    }

    /// Read the new contract's market state at this handle's block hash.
    pub async fn operations(&self, expected: &CurrentDeployment) -> Result<OperationalState> {
        self.verify_current(expected).await?;
        let c = OperationsPerp::new(self.market().perp(), self.market().chain().provider());
        let owner = c
            .owner()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let governor = c
            .governor()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let rates = c
            .rates()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let modules = c
            .modules()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let tvl_cap = c
            .tvlCap()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let accounted_tvl = c
            .accountedTvl()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let controller = c
            .withdrawalController()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        Ok(OperationalState {
            block: self.block(),
            market: self.market().perp(),
            owner,
            governor,
            last_touch: rates.lastTouch.to::<u64>(),
            tvl_cap,
            accounted_tvl,
            fees: modules.fees,
            controller,
        })
    }

    /// Read the fee module's owner and curve after checking its runtime hash.
    pub async fn fee_state(&self, code: ContractCode) -> Result<FeeState> {
        self.verify_contract(code).await?;
        let c = OperationsFees::new(code.address, self.market().chain().provider());
        let owner = c
            .owner()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let target = c
            .targetUtilization()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let before = c
            .slopeBeforeTarget()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let after = c
            .slopeAfterTarget()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let curve = UtilizationCurve::new(target, before, after)?;
        Ok(FeeState {
            block: self.block(),
            address: code.address,
            owner,
            curve,
        })
    }

    async fn verify_controller(&self, code: ContractCode) -> Result<()> {
        self.verify_contract(code).await?;
        let c = OperationsController::new(code.address, self.market().chain().provider());
        let market = c
            .perp()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        if market != self.market().perp() {
            return Err(unsupported(
                code.address,
                "controller is bound to another market",
            ));
        }
        Ok(())
    }

    /// Read a bound controller, including a historical controller after rotation.
    pub async fn controller_state(&self, code: ContractCode) -> Result<ControllerState> {
        self.verify_controller(code).await?;
        let c = OperationsController::new(code.address, self.market().chain().provider());
        let challenge_window = c
            .challengeWindow()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?
            .to::<u64>();
        let releases_paused = c
            .releasesPaused()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let rate = c
            .rateLimit()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let total_reserved = c
            .totalReserved()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let next_claim_id = c
            .nextClaimId()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        Ok(ControllerState {
            block: self.block(),
            address: code.address,
            market: self.market().perp(),
            challenge_window,
            releases_paused,
            capacity: UsdcAtoms::new(rate.capacity),
            available: UsdcAtoms::new(rate.available),
            refill_per_second: UsdcAtoms::new(rate.refillPerSecond),
            last_updated: rate.lastUpdated.to::<u64>(),
            total_reserved,
            next_claim_id,
        })
    }

    /// Read one claim only after verifying controller code and permanent binding.
    pub async fn withdrawal_claim(&self, code: ContractCode, id: U256) -> Result<WithdrawalClaim> {
        self.verify_controller(code).await?;
        let c = OperationsController::new(code.address, self.market().chain().provider());
        let r = c
            .claims(id)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let status = match r.status {
            0 => ClaimStatus::None,
            1 => ClaimStatus::Pending,
            2 => ClaimStatus::Completed,
            3 => ClaimStatus::Cancelled,
            _ => return Err(unsupported(code.address, "unknown claim status")),
        };
        Ok(WithdrawalClaim {
            id,
            original_amount: UsdcAtoms::new(r.originalAmount),
            current_amount: UsdcAtoms::new(r.currentAmount),
            recipient: r.recipient,
            created_at: r.createdAt.to::<u64>(),
            status,
            paused: r.paused,
        })
    }

    /// Read collateral balance and allowance to this market at the handle's block.
    pub async fn funding_state(&self, holder: Address) -> Result<FundingState> {
        let token = IERC20::new(
            self.market().chain().deployments().usdc,
            self.market().chain().provider(),
        );
        let balance = token
            .balanceOf(holder)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let allowance = token
            .allowance(holder, self.market().perp())
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        Ok(FundingState {
            balance: UsdcAtoms::try_from(balance)?,
            allowance,
        })
    }

    /// Read the position fingerprint. It cannot prevent changes before execution.
    pub async fn backstop_position(
        &self,
        expected: &CurrentDeployment,
        position: U256,
    ) -> Result<BackstopPosition> {
        self.verify_current(expected).await?;
        let c = OperationsPerp::new(self.market().perp(), self.market().chain().provider());
        let owner = c
            .positionOwner(position)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        if owner == Address::ZERO {
            return Err(ContractError::PositionNotFound { pos_id: position }.into());
        }
        let row = c
            .positions(position)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let maker = c
            .makerDetails(position)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let taker = c
            .takerDetails(position)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let mut bytes = owner.abi_encode();
        bytes.extend(row.abi_encode());
        bytes.extend(maker.abi_encode());
        bytes.extend(taker.abi_encode());
        Ok(BackstopPosition {
            owner,
            fingerprint: keccak256(bytes),
            kind: if maker.liquidity == 0 {
                BackstopKind::Taker
            } else {
                BackstopKind::Maker
            },
        })
    }

    /// Encode a current-build backstop with fresh deployment and deadline checks.
    pub async fn backstop_call(
        &self,
        expected: &CurrentDeployment,
        order: &BackstopOrder,
    ) -> Result<ContractCall> {
        self.verify_current(expected).await?;
        if order.margin.atoms() == 0
            || order.recipient == Address::ZERO
            || order.deadline < self.block().timestamp
        {
            return Err(invalid(
                "backstop needs positive margin, nonzero recipient and an unexpired deadline",
            ));
        }
        let data = match order.kind {
            BackstopKind::Maker => OperationsPerp::backstopMakerCall {
                id: order.position,
                margin: order.margin.atoms(),
                recipient: order.recipient,
                deadline: order.deadline,
            }
            .abi_encode(),
            BackstopKind::Taker => OperationsPerp::backstopTakerCall {
                id: order.position,
                margin: order.margin.atoms(),
                recipient: order.recipient,
                deadline: order.deadline,
            }
            .abi_encode(),
        };
        Ok(call(self.market().perp(), data))
    }

    /// Read the Safe nonce after checking its runtime code.
    pub async fn safe_nonce(&self, code: ContractCode) -> Result<U256> {
        self.verify_contract(code).await?;
        OperationsSafe::new(code.address, self.market().chain().provider())
            .nonce()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))
    }

    /// The singleton currently used by a verified Safe proxy.
    /// The caller separately matches and verifies the singleton's reviewed code.
    pub async fn safe_master_copy(&self, code: ContractCode) -> Result<Address> {
        self.verify_contract(code).await?;
        OperationsSafe::new(code.address, self.market().chain().provider())
            .masterCopy()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))
    }

    /// Read the Safe version after checking its runtime code.
    pub async fn safe_version(&self, code: ContractCode) -> Result<String> {
        self.verify_contract(code).await?;
        OperationsSafe::new(code.address, self.market().chain().provider())
            .VERSION()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))
    }
}

/// Permissionless touch calldata, shared by legacy and staged current builds.
pub fn touch_call(market: Address) -> ContractCall {
    call(market, OperationsPerp::touchCall {}.abi_encode())
}
/// Encode a cap change after the caller verifies the live governor and contract build.
pub fn set_tvl_cap_call(market: Address, cap: U256) -> ContractCall {
    call(market, OperationsPerp::setTvlCapCall { cap }.abi_encode())
}
/// Encode the fee-module setter with a validated curve.
pub fn set_utilization_curve_call(
    module: Address,
    curve: UtilizationCurve,
) -> Result<ContractCall> {
    Ok(call(
        module,
        OperationsFees::setUtilFeeCurveCall {
            target: curve.target,
            before: curve.before,
            after: curve.after,
        }
        .abi_encode(),
    ))
}
/// Encode a controller call. The caller checks its authority and permitted changes.
pub fn controller_call(controller: Address, action: ControllerAction) -> Result<ContractCall> {
    let data = match action {
        ControllerAction::PauseReleases => OperationsController::pauseReleasesCall {}.abi_encode(),
        ControllerAction::PauseClaim { claim } => {
            OperationsController::pauseClaimCall { claim }.abi_encode()
        }
        ControllerAction::LowerRateLimit {
            capacity,
            refill_per_second,
        } => OperationsController::lowerRateLimitCall {
            capacity: capacity.atoms(),
            refill: refill_per_second.atoms(),
        }
        .abi_encode(),
        ControllerAction::IncreaseChallengeWindow { seconds } => {
            if seconds == 0 || seconds >= (1u64 << 48) {
                return Err(invalid("challenge window must fit positive uint48"));
            }
            OperationsController::increaseChallengeWindowCall {
                seconds_: Uint::<48, 1>::from(seconds),
            }
            .abi_encode()
        }
        ControllerAction::ReduceClaim {
            claim,
            new_amount,
            evidence,
        } => {
            if evidence == B256::ZERO {
                return Err(invalid("claim reduction requires evidence hash"));
            }
            OperationsController::reduceClaimCall {
                claim,
                newAmount: new_amount.atoms(),
                evidence,
            }
            .abi_encode()
        }
        ControllerAction::UnpauseReleases => {
            OperationsController::unpauseReleasesCall {}.abi_encode()
        }
        ControllerAction::UnpauseClaim { claim } => {
            OperationsController::unpauseClaimCall { claim }.abi_encode()
        }
    };
    Ok(call(controller, data))
}
/// Encode an exact USDC return transfer. It restores cash but does not repair accounting.
pub fn usdc_transfer_call(
    token: Address,
    recipient: Address,
    amount: UsdcAtoms,
) -> Result<ContractCall> {
    if recipient == Address::ZERO || amount.atoms() == 0 {
        return Err(invalid("transfer needs nonzero recipient and amount"));
    }
    Ok(call(
        token,
        IERC20::transferCall {
            to: recipient,
            amount: U256::from(amount.atoms()),
        }
        .abi_encode(),
    ))
}
/// Encode ordered calls for Safe MultiSend. The Safe must use delegatecall to run
/// the verified MultiSend contract so each call retains the Safe as its sender.
pub fn multisend_call(multisend: Address, calls: &[ContractCall]) -> Result<ContractCall> {
    if multisend == Address::ZERO || calls.is_empty() {
        return Err(invalid("multisend needs a contract and at least one call"));
    }
    let mut packed = Vec::new();
    for c in calls {
        packed.push(0);
        packed.extend_from_slice(c.to.as_slice());
        packed.extend(U256::from(c.value).to_be_bytes::<32>());
        packed.extend(U256::from(c.data.len()).to_be_bytes::<32>());
        packed.extend_from_slice(&c.data);
    }
    Ok(call(
        multisend,
        OperationsMultiSend::multiSendCall {
            transactions: packed.into(),
        }
        .abi_encode(),
    ))
}

#[cfg(test)]
mod tests {
    use std::task::{Context, Poll};

    use alloy::consensus::Header as ConsensusHeader;
    use alloy::providers::RootProvider;
    use alloy::rpc::client::RpcClient;
    use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
    use alloy::rpc::types::{Block, BlockTransactions, Header};
    use alloy::transports::mock::{Asserter, MockTransport};
    use alloy::transports::{TransportError, TransportFut};
    use serde_json::Value;
    use tower::Service;

    use super::*;
    use crate::client::mock::{self, PERP};
    use crate::contracts::{Perp, operations::OperationsRates};

    #[derive(Clone)]
    struct PinnedTransport {
        inner: MockTransport,
        hash: B256,
    }

    impl Service<RequestPacket> for PinnedTransport {
        type Response = ResponsePacket;
        type Error = TransportError;
        type Future = TransportFut<'static>;
        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<StdResult<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }
        fn call(&mut self, request: RequestPacket) -> Self::Future {
            let RequestPacket::Single(req) = &request else {
                panic!("unexpected batch")
            };
            if req.method() != "eth_getBlockByNumber" {
                let params: Value = serde_json::from_str(req.params().unwrap().get()).unwrap();
                assert_eq!(
                    params.as_array().unwrap().last().unwrap()["blockHash"],
                    serde_json::json!(self.hash),
                    "{} must use the handle's hash",
                    req.method()
                );
            }
            self.inner.call(request)
        }
    }

    #[tokio::test]
    async fn code_storage_and_view_rpc_requests_use_the_same_block_hash() {
        let asserter = Asserter::new();
        let header = Header::new(ConsensusHeader {
            number: 42,
            timestamp: 100,
            ..Default::default()
        });
        let hash = header.hash;
        asserter.push_success(&Block::new(
            header,
            BlockTransactions::<B256>::Hashes(Vec::new()),
        ));
        asserter.push_success(&Bytes::from_static(&[0x11]));
        asserter.push_success(&U256::from_be_slice(Address::repeat_byte(0x22).as_slice()));
        asserter.push_success(&Bytes::from_static(&[0x33]));
        // The five-word source ABI: funding, long, short, timestamp, mark.
        let mut rates = vec![0u8; 160];
        rates[127] = 75;
        rates[159] = 9;
        asserter.push_success(&Bytes::from(rates));
        let provider = RootProvider::new(RpcClient::new(
            PinnedTransport {
                inner: MockTransport::new(asserter.clone()),
                hash,
            },
            true,
        ));
        let client = mock::client_over(provider);
        let state = client.market().state_at(42).await.unwrap();
        assert_eq!(
            state.operational_last_touch(&deployment()).await.unwrap(),
            75
        );
        assert!(asserter.read_q().is_empty());
    }

    fn deployment() -> CurrentDeployment {
        CurrentDeployment {
            proxy_code_hash: keccak256([0x11]),
            implementation: Address::repeat_byte(0x22),
            implementation_code_hash: keccak256([0x33]),
        }
    }
    fn code(rpc: &mock::Rpc) {
        rpc.bytes(&[0x11]);
        rpc.storage(U256::from_be_slice(Address::repeat_byte(0x22).as_slice()));
        rpc.bytes(&[0x33]);
    }

    #[tokio::test]
    async fn current_touch_reads_only_rates_and_rechecks_upgrade_identity() {
        let (chain, rpc) = mock::chain();
        rpc.block(42, 100);
        let state = chain.market(PERP).state_at(42).await.unwrap();
        code(&rpc);
        rpc.call::<OperationsPerp::ratesCall>(&OperationsRates {
            fundingPerDay: Default::default(),
            longUtilFeePerDay: 0,
            shortUtilFeePerDay: 0,
            lastTouch: Uint::<40, 1>::from(75),
            lastMark: U256::from(999),
        });
        assert_eq!(
            state.operational_last_touch(&deployment()).await.unwrap(),
            75
        );
        assert!(rpc.is_drained());
        // A proxy upgrade behind an unchanged proxy hash must invalidate the next read.
        rpc.bytes(&[0x11]);
        rpc.storage(U256::from_be_slice(Address::repeat_byte(0x44).as_slice()));
        let error = state
            .operational_last_touch(&deployment())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PerpCityError::Contract(ContractError::UnsupportedOperations { .. })
        ));
        assert!(!error.is_transient());
        assert!(rpc.is_drained());
    }

    #[tokio::test]
    async fn unknown_code_and_unavailable_rpc_remain_different() {
        let (chain, rpc) = mock::chain();
        rpc.block(5, 100);
        let state = chain.market(PERP).state_at(5).await.unwrap();
        rpc.bytes(&[0x55]);
        assert!(matches!(
            state.verify_current(&deployment()).await.unwrap_err(),
            PerpCityError::Contract(ContractError::UnsupportedOperations { .. })
        ));
        rpc.fails("endpoint unavailable");
        assert!(matches!(
            state.verify_current(&deployment()).await.unwrap_err(),
            PerpCityError::Rpc(_)
        ));
    }

    #[tokio::test]
    async fn controller_from_another_market_cannot_supply_claims() {
        let (chain, rpc) = mock::chain();
        rpc.block(5, 100);
        let state = chain.market(PERP).state_at(5).await.unwrap();
        rpc.bytes(&[0x55]);
        rpc.call::<OperationsController::perpCall>(&Address::repeat_byte(0xee));
        let error = state
            .withdrawal_claim(
                ContractCode {
                    address: Address::repeat_byte(0x66),
                    code_hash: keccak256([0x55]),
                },
                U256::from(1),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PerpCityError::Contract(ContractError::UnsupportedOperations { .. })
        ));
        assert!(rpc.is_drained());
    }

    #[tokio::test]
    async fn current_backstop_keeps_deadline_and_legacy_selector_distinct() {
        // Selectors independently computed with Foundry cast sig from contracts 5141c2f.
        assert_eq!(
            OperationsPerp::backstopMakerCall::SELECTOR,
            [0x29, 0x65, 0x08, 0x1d]
        );
        assert_eq!(
            OperationsPerp::backstopTakerCall::SELECTOR,
            [0x19, 0xed, 0xa8, 0x72]
        );
        assert_ne!(
            OperationsPerp::backstopMakerCall::SELECTOR,
            Perp::backstopMakerCall::SELECTOR
        );
        let (chain, rpc) = mock::chain();
        rpc.block(42, 100);
        let state = chain.market(PERP).state_at(42).await.unwrap();
        let mut order = BackstopOrder {
            kind: BackstopKind::Maker,
            position: U256::from(9),
            margin: UsdcAtoms::new(5_000_000),
            recipient: Address::repeat_byte(0x99),
            deadline: 150,
        };
        code(&rpc);
        let encoded = state.backstop_call(&deployment(), &order).await.unwrap();
        assert_eq!(encoded.data.len(), 132);
        assert_eq!(
            U256::from_be_slice(&encoded.data[100..132]),
            U256::from(150)
        );
        order.deadline = 99;
        code(&rpc);
        assert!(state.backstop_call(&deployment(), &order).await.is_err());
        assert!(rpc.is_drained());
    }

    #[test]
    fn curve_bounds_and_multisend_preserve_contract_rules_and_call_order() {
        let wad = U256::from(1_000_000_000_000_000_000u64);
        let valid = UtilizationCurve::new(
            wad / U256::from(2),
            wad / U256::from(10),
            wad / U256::from(5),
        )
        .unwrap();
        for invalid_curve in [
            UtilizationCurve {
                target: U256::ZERO,
                ..valid
            },
            UtilizationCurve {
                after: wad / U256::from(20),
                ..valid
            },
            UtilizationCurve {
                before: wad,
                after: wad * U256::from(2),
                ..valid
            },
            UtilizationCurve {
                before: U256::MAX,
                after: U256::MAX,
                ..valid
            },
        ] {
            assert!(
                UtilizationCurve::new(
                    invalid_curve.target,
                    invalid_curve.before,
                    invalid_curve.after
                )
                .is_err()
            );
            assert!(
                serde_json::from_value::<UtilizationCurve>(
                    serde_json::to_value(invalid_curve).unwrap()
                )
                .is_err(),
                "deserialization must enforce curve bounds"
            );
        }
        let c = touch_call(PERP);
        let setter = set_utilization_curve_call(Address::repeat_byte(0x44), valid).unwrap();
        assert_eq!(&setter.data[..4], &[0xeb, 0x05, 0xf9, 0xaf]);
        let encoded = multisend_call(
            Address::repeat_byte(0x77),
            &[c.clone(), setter.clone(), c.clone()],
        )
        .unwrap();
        assert_eq!(&encoded.data[..4], &[0x8d, 0x80, 0xff, 0x0a]);
        let packed = OperationsMultiSend::multiSendCall::abi_decode(&encoded.data)
            .unwrap()
            .transactions;
        let mut cursor = 0;
        for expected in [&c, &setter, &c] {
            assert_eq!(
                packed[cursor], 0,
                "inner calls retain Safe authority through CALL"
            );
            assert_eq!(&packed[cursor + 1..cursor + 21], expected.to.as_slice());
            let len = U256::from_be_slice(&packed[cursor + 53..cursor + 85]).to::<usize>();
            assert_eq!(
                &packed[cursor + 85..cursor + 85 + len],
                expected.data.as_ref()
            );
            cursor += 85 + len;
        }
        assert_eq!(cursor, packed.len());
    }
}
