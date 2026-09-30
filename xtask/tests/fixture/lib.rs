//! A crate shaped like the SDK with one planted violation per enforced
//! invariant, and beside each a neighbour that must not fire. The test in
//! `tests/invariants.rs` documents this file with rustdoc and asserts that
//! every plant, and nothing else, is reported. This file is the readable
//! specification of what each invariant means.
#![allow(dead_code, unused_variables, clippy::all)]

pub mod errors {
    /// Every variant states its transience, except the plant.
    pub enum ContractError {
        /// The replica lags. Transient.
        BlockUnavailable { number: u64 },
        /// The node pruned it. Not transient.
        StateUnavailable { number: u64 },
        /// PLANT: says nothing about transience.
        EventNotFound { event_name: String },
    }

    /// The send's stages.
    pub enum TransactionError {
        /// Signed bytes were refused. Not transient.
        SigningFailed { reason: String },
    }

    /// The caller's input refused. Never transient.
    pub enum ValidationError {
        /// A range that is not a range.
        InvalidRange,
    }

    /// The umbrella.
    pub enum PerpCityError {
        /// The chain's answer.
        Contract(ContractError),
        /// The send's stage.
        Transaction(TransactionError),
        /// The caller's mistake.
        Validation(ValidationError),
    }

    /// The crate's result.
    pub type Result<T> = core::result::Result<T, PerpCityError>;
}

pub mod math {
    use crate::errors::ValidationError;

    /// One header.
    pub struct BlockContext {
        pub number: u64,
    }

    /// A snapshot that carries its block, as every snapshot must.
    pub struct PoolSnapshot {
        pub block: BlockContext,
        pub liquidity: u128,
    }

    /// PLANT: a snapshot with no `BlockContext`.
    pub struct MarketCapacity {
        pub long_atoms: u128,
    }

    /// A validated value: private fields, one fallible door.
    pub struct TickRange {
        lower: i32,
        upper: i32,
    }

    impl TickRange {
        /// The door: checked, fallible.
        pub fn new(lower: i32, upper: i32) -> Result<Self, ValidationError> {
            if lower < upper {
                Ok(Self { lower, upper })
            } else {
                Err(ValidationError::InvalidRange)
            }
        }

        /// PLANT: a bare constructor that skips the check.
        pub fn raw(lower: i32, upper: i32) -> Self {
            Self { lower, upper }
        }

        /// A method on the value itself, not a constructor: must not fire.
        pub fn widened(self, by: i32) -> Self {
            Self {
                lower: self.lower - by,
                upper: self.upper + by,
            }
        }
    }

    /// A pair of prices with public fields: not a validated value.
    pub struct PricePair {
        pub amm: u128,
        pub index: u128,
    }

    impl PricePair {
        /// Pure: takes nothing of the chain's.
        pub fn advanced(self, dt: u64) -> Self {
            self
        }

        /// PLANT: pure math taking a handle.
        pub fn from_reader(reader: &crate::client::MarketReader) -> Self {
            Self { amm: 0, index: 0 }
        }
    }

    /// PLANT: a public type no node's table names.
    pub struct Orphan;

    /// PLANT: a public type two nodes' tables name.
    pub struct Twice;
}

pub mod client {
    use crate::errors::Result;
    use crate::math::{MarketCapacity, PoolSnapshot};

    /// A market, read now.
    pub struct MarketReader {
        perp: [u8; 20],
    }

    impl MarketReader {
        /// The one door to a named block: must not fire.
        pub fn state_at(&self, number: u64) -> Result<StateAt> {
            Ok(StateAt { number })
        }

        /// A read now: must not fire.
        pub fn get_pool_snapshot(&self) -> Result<PoolSnapshot> {
            Err(crate::errors::PerpCityError::Validation(
                crate::errors::ValidationError::InvalidRange,
            ))
        }

        /// PLANT: a read that takes a block argument.
        pub fn get_capacity_at(&self, block: u64) -> Result<MarketCapacity> {
            Ok(MarketCapacity { long_atoms: 0 })
        }

        /// PLANT: a fallible function returning a foreign error.
        pub fn dump(&self) -> core::result::Result<(), std::io::Error> {
            Ok(())
        }
    }

    /// A market at one block.
    pub struct StateAt {
        number: u64,
    }
}
