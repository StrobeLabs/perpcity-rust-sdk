//! Block-pinned v1 liquidation previews, close capacity and collateral totals.

use alloy::primitives::{Bytes, U256};
use alloy::sol_types::{SolCall, SolError};
use alloy::transports::RpcError;

use crate::client::chain::row_revert;
use crate::contracts::v1;
use crate::errors::Result;
use crate::math::capacity::Capacity;
use crate::storage::v1_perp_physical_cap_slots;
use crate::units::{PerSide, PerpAtoms, UsdcAtoms};

use super::{RowOutcome, StateAt, decode_exact, decode_exact_row, pinned_read_error};

/// V1 collateral cap and accounted collateral, in USDC atoms at one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TvlCap {
    /// Maximum accounted collateral.
    pub cap: U256,
    /// Collateral already counted against the cap.
    pub accounted: U256,
}

impl TvlCap {
    /// Whether the resulting accounted TVL fits the cap.
    pub fn allows_deposit(&self, amount: UsdcAtoms) -> bool {
        self.accounted
            .checked_add(U256::from(amount.atoms()))
            .is_some_and(|total| total <= self.cap)
    }
}

/// A reverted preview: `None` for the missing-position revert, else the
/// typed revert with its selector and transience.
fn preview_revert(data: &Bytes) -> Result<Option<v1::PositionPreview>> {
    if data[..] == v1::Perp::PositionDoesNotExist::SELECTOR {
        return Ok(None);
    }
    Err(row_revert(data))
}

impl StateAt {
    /// Read the v1 liquidation preview at this block. `None` for a
    /// position that is not open, as in [`Self::preview_positions`]; any
    /// other revert keeps its selector and transience, as a batch row does.
    pub async fn preview_position(&self, pos_id: U256) -> Result<Option<v1::PositionPreview>> {
        self.require_read_era(true, "preview_position").await?;
        let call = v1::Perp::new(self.market.perp, self.market.chain.provider())
            .previewPosition(pos_id)
            .block(self.id())
            .call_raw()
            .await;
        // A node answers a revert without data as a bare `execution
        // reverted`; that is the empty row a batch reads, not a read error.
        let revert = match &call {
            Err(alloy::contract::Error::TransportError(RpcError::ErrorResp(payload))) => {
                payload.as_revert_data().or_else(|| {
                    payload
                        .message
                        .contains("execution reverted")
                        .then(Bytes::new)
                })
            }
            _ => None,
        };
        if let Some(data) = revert {
            return preview_revert(&data);
        }
        let bytes = call.map_err(|e| self.read_error(e))?;
        decode_exact::<v1::Perp::previewPositionCall>(pos_id, &bytes, "previewPosition").map(Some)
    }

    /// Read v1 previews in input order, each row as
    /// [`Self::preview_position`] reads it.
    pub async fn preview_positions(
        &self,
        pos_ids: &[U256],
    ) -> Result<Vec<RowOutcome<v1::PositionPreview>>> {
        self.require_read_era(true, "preview_positions").await?;
        if pos_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self
            .rows(
                pos_ids,
                |pos_id| [v1::Perp::previewPositionCall { posId: pos_id }.abi_encode()],
                |id, [row]| {
                    if !row.success {
                        return preview_revert(&row.returnData);
                    }
                    decode_exact_row::<v1::Perp::previewPositionCall>(id, row, "previewPosition")
                        .map(Some)
                },
            )
            .await)
    }

    /// Physical long-close and short-close depth at this block, in perp atoms.
    pub async fn physical_close_capacity(&self) -> Result<Capacity> {
        self.require_read_era(true, "physical_close_capacity")
            .await?;
        let depth = v1::Perp::new(self.market.perp, self.market.chain.provider())
            .physicalCloseCapacity()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        Ok(Capacity::new(
            PerpAtoms::new(depth.longClose),
            PerpAtoms::new(depth.shortClose),
        ))
    }

    /// Physical close depth at this block in X64 perp atoms, from the
    /// `physicalCap` storage words (slots 21 and 22). The input
    /// [`MarketCapacity::fits_maker_removal`](crate::MarketCapacity::fits_maker_removal)
    /// needs: the public view rounds down and caps at `u128::MAX`, so the
    /// whole-atom depth shifted left 64 bits is only a conservative lower
    /// limit there.
    pub async fn physical_close_capacity_x64(&self) -> Result<PerSide<U256>> {
        self.require_read_era(true, "physical_close_capacity_x64")
            .await?;
        let chain = &self.market.chain;
        let [long, short] = v1_perp_physical_cap_slots();
        let context = "physical close capacity at the market's slots";
        let (perp, block) = (self.market.perp, self.id());
        let pinned = |e| pinned_read_error(e, self.block.number);
        let (long, short) = tokio::try_join!(
            chain.storage_word_at(perp, long, block, context),
            chain.storage_word_at(perp, short, block, context)
        )
        .map_err(pinned)?;
        Ok(PerSide::new(long, short))
    }

    /// Total outstanding maker liquidity at this block,
    /// `fundingState().makerLiquidity`. A removal of exactly this much is
    /// the last one, the `last` argument of
    /// [`MarketCapacity::fits_maker_removal`](crate::MarketCapacity::fits_maker_removal).
    pub async fn maker_liquidity(&self) -> Result<U256> {
        self.require_read_era(true, "maker_liquidity").await?;
        let state = v1::Perp::new(self.market.perp, self.market.chain.provider())
            .fundingState()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        Ok(state.makerLiquidity)
    }

    /// Read the two collateral totals at this block.
    pub async fn tvl_cap(&self) -> Result<TvlCap> {
        self.require_read_era(true, "tvl_cap").await?;
        let perp = v1::Perp::new(self.market.perp, self.market.chain.provider());
        let (cap, accounted) = self
            .market
            .chain
            .multicall_at(self.id())
            .add(perp.tvlCap())
            .add(perp.accountedTvl())
            .aggregate()
            .await
            .map_err(|e| self.multicall_read_error(e))?;
        Ok(TvlCap { cap, accounted })
    }
}
