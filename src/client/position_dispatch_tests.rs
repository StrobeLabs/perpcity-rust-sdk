//! The position reads' dispatch on each real chain (plan U4): the pool
//! key's hook against [`ChainDeployments::ARBITRUM`] on chain 42161 and
//! [`ChainDeployments::ARBITRUM_SEPOLIA`] on 421614. The zero hook and the
//! chain's `v0.2.2` hook read the earlier builds' shape and `ownerOf`; the
//! chain's `v1.0.0` hook reads the `v1.0.0` shape and `positionOwner`; any
//! other hook, the other chain's hooks included, is `UnknownBuild` with
//! nothing read after the pool key.

use alloy::primitives::{Address, U256, address};
use alloy::sol_types::SolCall;

use crate::contracts::{Perp, v1};
use crate::errors::{ContractError, PerpCityError, Result};
use crate::math::range::{MakerBand, TickRange};
use crate::units::{LUnits, UsdcAtoms};

use super::mock::{self, POOL_ID, Rpc, ok_row, returns, v1_maker_words, v1_position_words};
use super::state::PositionRow;
use super::{
    ARBITRUM_CHAIN_ID, ARBITRUM_SEPOLIA_CHAIN_ID, ChainDeployments, Era, MarketReader, PerpClient,
};

const HOLDER: Address = Address::repeat_byte(0xaa);
const ID: U256 = U256::ONE;
const MARGIN: u128 = 5_000_000;
const TIMESTAMP: u64 = 1_700_000_000;

/// The row and owner shape a known build reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Build `58b42b7` and `v0.2.2`: the five-word `Position`, the ten-word
    /// `Maker`, the NFT's `ownerOf`.
    Earlier,
    /// `v1.0.0`: the three-word `Position`, the seven-word `Maker`,
    /// `positionOwner`.
    V1,
}

impl Shape {
    fn of(era: Era) -> Self {
        match era {
            Era::Legacy | Era::Upgradeable => Self::Earlier,
            Era::V1 => Self::V1,
        }
    }

    fn position(self) -> Vec<u8> {
        match self {
            Self::Earlier => returns::<Perp::positionsCall>(&mock::position(MARGIN)),
            Self::V1 => v1_position_words(-400_000, 9_000_000, MARGIN, 0),
        }
    }

    fn owner(self) -> Vec<u8> {
        match self {
            Self::Earlier => returns::<Perp::ownerOfCall>(&HOLDER),
            Self::V1 => returns::<v1::Perp::positionOwnerCall>(&HOLDER),
        }
    }

    fn maker(self) -> Vec<u8> {
        match self {
            Self::Earlier => returns::<Perp::makerDetailsCall>(&mock::maker(-600, 1_200, 7)),
            Self::V1 => v1_maker_words(-600, 1_200, 7),
        }
    }

    fn owner_selector(self) -> [u8; 4] {
        match self {
            Self::Earlier => Perp::ownerOfCall::SELECTOR,
            Self::V1 => v1::Perp::positionOwnerCall::SELECTOR,
        }
    }

    fn pool_id_selector(self) -> [u8; 4] {
        match self {
            Self::Earlier => Perp::POOL_IDCall::SELECTOR,
            Self::V1 => v1::Perp::poolIdCall::SELECTOR,
        }
    }

    /// A row of this shape: the margin read, and the stored ratios only on
    /// the earlier builds.
    fn check_row(self, row: PositionRow) {
        assert_eq!(row.margin, UsdcAtoms::new(MARGIN));
        assert_eq!(
            row.stored_margin_ratios.is_some(),
            self == Self::Earlier,
            "{self:?}"
        );
    }
}

/// Every position read this PR dispatches by build.
#[derive(Debug, Clone, Copy)]
enum Read {
    GetPosition,
    GetPositionsByOwner,
    Position,
    Positions,
    Owners,
    MakerBand,
    MakerBands,
}

impl Read {
    const ALL: [Self; 7] = [
        Self::GetPosition,
        Self::GetPositionsByOwner,
        Self::Position,
        Self::Positions,
        Self::Owners,
        Self::MakerBand,
        Self::MakerBands,
    ];

    fn is_pinned(self) -> bool {
        !matches!(self, Self::GetPosition | Self::GetPositionsByOwner)
    }

    /// The answers to the read's own calls, in `shape`.
    fn answer(self, rpc: &Rpc, shape: Shape) {
        match self {
            Self::GetPosition | Self::Position | Self::Positions if shape == Shape::V1 => {
                rpc.aggregate3(vec![ok_row(shape.position()), ok_row(shape.owner())])
            }
            Self::GetPosition | Self::Position => rpc.raw(&shape.position()),
            Self::GetPositionsByOwner => {
                rpc.call::<Perp::nextPosIdCall>(&U256::from(2u8));
                rpc.raw(&shape.owner());
            }
            Self::Positions => rpc.aggregate3(vec![ok_row(shape.position())]),
            Self::Owners => rpc.aggregate3(vec![ok_row(shape.owner())]),
            Self::MakerBand => rpc.raw(&shape.maker()),
            Self::MakerBands => rpc.aggregate3(vec![ok_row(shape.maker())]),
        }
    }

    /// The selectors of the read's own calls in `shape`.
    fn selectors(self, shape: Shape) -> Vec<[u8; 4]> {
        match self {
            Self::GetPosition | Self::Position | Self::Positions => {
                if shape == Shape::V1 {
                    vec![Perp::positionsCall::SELECTOR, shape.owner_selector()]
                } else {
                    vec![Perp::positionsCall::SELECTOR]
                }
            }
            Self::GetPositionsByOwner => {
                vec![Perp::nextPosIdCall::SELECTOR, shape.owner_selector()]
            }
            Self::Owners => vec![shape.owner_selector()],
            Self::MakerBand | Self::MakerBands => vec![Perp::makerDetailsCall::SELECTOR],
        }
    }

    /// Runs the read for [`ID`]; each id's outcome, its value checked
    /// against `shape` when it is a value.
    async fn run(self, market: &MarketReader, shape: Option<Shape>) -> Vec<Result<()>> {
        let state = if self.is_pinned() {
            Some(market.state_at(92).await.unwrap())
        } else {
            None
        };
        let state = || state.as_ref().unwrap();
        let shape = || shape.unwrap();
        let band = Some(MakerBand::new(
            TickRange::new(-600, 1_200).unwrap(),
            LUnits::new(7),
        ));
        match self {
            Self::GetPosition => {
                vec![market.get_position(ID).await.map(|r| shape().check_row(r))]
            }
            Self::GetPositionsByOwner => vec![
                market
                    .get_positions_by_owner(HOLDER)
                    .await
                    .map(|ids| assert_eq!(ids, [ID])),
            ],
            Self::Position => {
                vec![
                    state()
                        .position(ID)
                        .await
                        .map(|r| shape().check_row(r.unwrap())),
                ]
            }
            Self::Positions => state()
                .positions(&[ID])
                .await
                .into_iter()
                .map(|o| o.row.map(|r| shape().check_row(r.unwrap())))
                .collect(),
            Self::Owners => state()
                .owners(&[ID])
                .await
                .into_iter()
                .map(|o| o.row.map(|holder| assert_eq!(holder, Some(HOLDER))))
                .collect(),
            Self::MakerBand => vec![state().maker_band(ID).await.map(|b| assert_eq!(b, band))],
            Self::MakerBands => state()
                .maker_bands(&[ID])
                .await
                .into_iter()
                .map(|o| o.row.map(|b| assert_eq!(b, band)))
                .collect(),
        }
    }
}

/// One chain's hook cases: each hook and the build it must name, `None`
/// for an unknown build.
fn hook_cases(chain: &ChainDeployments, other: &ChainDeployments) -> [(Address, Option<Era>); 6] {
    [
        (Address::ZERO, Some(Era::Legacy)),
        (chain.v022_perp_guard_hook, Some(Era::Upgradeable)),
        (chain.v1_perp_guard_hook, Some(Era::V1)),
        (other.v1_perp_guard_hook, None),
        (other.v022_perp_guard_hook, None),
        (Address::repeat_byte(0x0e), None),
    ]
}

/// Builds a client on `chain`, names `hook` in the pool key, and runs
/// `read` on it: the earlier builds' shape for the zero and `v0.2.2`
/// hooks, the `v1.0.0` shape for the `v1.0.0` hook, and for any other
/// hook the typed refusal after the pool key and no other call.
async fn check(
    chain: ChainDeployments,
    chain_id: u64,
    hook: Address,
    era: Option<Era>,
    read: Read,
) {
    let case = format!("chain {chain_id}, hook {hook}, {read:?}");
    let (client, rpc): (PerpClient, Rpc) = mock::client_on(chain, chain_id);
    if read.is_pinned() {
        rpc.block(92, TIMESTAMP);
    }
    rpc.call::<Perp::poolKeyCall>(&mock::pool_key_with_hook(30, hook));
    let shape = era.map(Shape::of);
    if let Some(shape) = shape {
        match shape {
            Shape::Earlier => rpc.call::<Perp::POOL_IDCall>(&POOL_ID),
            Shape::V1 => rpc.call::<v1::Perp::poolIdCall>(&POOL_ID),
        }
        read.answer(&rpc, shape);
    }

    let outcomes = read.run(client.market(), shape).await;
    assert_eq!(outcomes.len(), 1, "{case}");
    let mut expected = vec![Perp::poolKeyCall::SELECTOR];
    match shape {
        Some(shape) => {
            for outcome in &outcomes {
                assert!(outcome.is_ok(), "{case}: {outcome:?}");
            }
            expected.push(shape.pool_id_selector());
            expected.extend(read.selectors(shape));
        }
        None => {
            for outcome in outcomes {
                assert!(
                    matches!(
                        outcome,
                        Err(PerpCityError::Contract(ContractError::UnknownBuild { perp, hook: named }))
                            if perp == mock::PERP && named == hook
                    ),
                    "{case}: {outcome:?}"
                );
            }
        }
    }
    assert_eq!(rpc.selectors(), expected, "{case}");
    assert!(rpc.is_drained(), "{case}");
}

#[tokio::test]
async fn every_position_read_dispatches_by_each_real_chain_s_hooks() {
    let one = ChainDeployments::ARBITRUM;
    let sepolia = ChainDeployments::ARBITRUM_SEPOLIA;
    assert_eq!(
        one.v022_perp_guard_hook,
        address!("561Ae0C8d4f3AeeaeB92Fa892281A4dA97De0AA0")
    );
    assert_eq!(
        sepolia.v022_perp_guard_hook,
        address!("f03fF5e350EA455B452a83ef89a3bcbcD6F98aa0")
    );
    for (chain, chain_id, other) in [
        (one, ARBITRUM_CHAIN_ID, sepolia),
        (sepolia, ARBITRUM_SEPOLIA_CHAIN_ID, one),
    ] {
        for (hook, era) in hook_cases(&chain, &other) {
            assert_eq!(chain.era_of(hook), era, "chain {chain_id}, hook {hook}");
            for read in Read::ALL {
                check(chain, chain_id, hook, era, read).await;
            }
        }
    }
}
