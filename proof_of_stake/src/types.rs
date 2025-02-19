//! Proof of Stake data types

mod rev_order;

use core::fmt::Debug;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fmt::Display;
use std::hash::Hash;
use std::ops::Sub;

use borsh::{BorshDeserialize, BorshSchema, BorshSerialize};
use namada_core::ledger::storage_api::collections::lazy_map::NestedMap;
use namada_core::ledger::storage_api::collections::{
    LazyMap, LazySet, LazyVec,
};
use namada_core::ledger::storage_api::{self, StorageRead};
use namada_core::types::address::Address;
use namada_core::types::dec::Dec;
use namada_core::types::key::common;
use namada_core::types::storage::{Epoch, KeySeg};
use namada_core::types::token;
use namada_core::types::token::Amount;
pub use rev_order::ReverseOrdTokenAmount;

use crate::parameters::PosParams;

// TODO: replace `POS_MAX_DECIMAL_PLACES` with
// core::types::token::NATIVE_MAX_DECIMAL_PLACES??
const U64_MAX: u64 = u64::MAX;

/// Number of epochs below the current epoch for which validator deltas and
/// slashes are stored
const VALIDATOR_DELTAS_SLASHES_LEN: u64 = 23;

// TODO: add this to the spec
/// Stored positions of validators in validator sets
pub type ValidatorSetPositions = crate::epoched::NestedEpoched<
    LazyMap<Address, Position>,
    crate::epoched::OffsetPipelineLen,
>;

impl ValidatorSetPositions {
    /// TODO
    pub fn get_position<S>(
        &self,
        storage: &S,
        epoch: &Epoch,
        address: &Address,
        params: &PosParams,
    ) -> storage_api::Result<Option<Position>>
    where
        S: StorageRead,
    {
        let last_update = self.get_last_update(storage)?;
        // dbg!(&last_update);
        if last_update.is_none() {
            return Ok(None);
        }
        let last_update = last_update.unwrap();
        let future_most_epoch: Epoch = last_update + params.pipeline_len;
        // dbg!(future_most_epoch);
        let mut epoch = std::cmp::min(*epoch, future_most_epoch);
        loop {
            // dbg!(epoch);
            match self.at(&epoch).get(storage, address)? {
                Some(val) => return Ok(Some(val)),
                None => {
                    if epoch.0 > 0 && epoch > Self::sub_past_epochs(last_update)
                    {
                        epoch = Epoch(epoch.0 - 1);
                    } else {
                        return Ok(None);
                    }
                }
            }
        }
    }
}

// TODO: check the offsets for each epoched type!!

/// Epoched validator's consensus key.
pub type ValidatorConsensusKeys = crate::epoched::Epoched<
    common::PublicKey,
    crate::epoched::OffsetPipelineLen,
>;

/// Epoched validator's eth hot key.
pub type ValidatorEthHotKeys = crate::epoched::Epoched<
    common::PublicKey,
    crate::epoched::OffsetPipelineLen,
>;

/// Epoched validator's eth cold key.
pub type ValidatorEthColdKeys = crate::epoched::Epoched<
    common::PublicKey,
    crate::epoched::OffsetPipelineLen,
>;

/// Epoched validator's state.
pub type ValidatorStates =
    crate::epoched::Epoched<ValidatorState, crate::epoched::OffsetPipelineLen>;

/// A map from a position to an address in a Validator Set
pub type ValidatorPositionAddresses = LazyMap<Position, Address>;

/// New validator set construction, keyed by staked token amount
pub type ConsensusValidatorSet =
    NestedMap<token::Amount, ValidatorPositionAddresses>;

/// New validator set construction, keyed by staked token amount
pub type BelowCapacityValidatorSet =
    NestedMap<ReverseOrdTokenAmount, ValidatorPositionAddresses>;

/// Epoched consensus validator sets.
pub type ConsensusValidatorSets = crate::epoched::NestedEpoched<
    ConsensusValidatorSet,
    crate::epoched::OffsetPipelineLen,
>;

/// Epoched below-capacity validator sets.
pub type BelowCapacityValidatorSets = crate::epoched::NestedEpoched<
    BelowCapacityValidatorSet,
    crate::epoched::OffsetPipelineLen,
>;

/// Epoched total consensus validator stake
pub type TotalConsensusStakes =
    crate::epoched::Epoched<Amount, crate::epoched::OffsetZero, U64_MAX>;

/// Epoched validator's deltas.
pub type ValidatorDeltas = crate::epoched::EpochedDelta<
    token::Change,
    crate::epoched::OffsetUnbondingLen,
    VALIDATOR_DELTAS_SLASHES_LEN,
>;

/// Epoched total deltas.
pub type TotalDeltas = crate::epoched::EpochedDelta<
    token::Change,
    crate::epoched::OffsetUnbondingLen,
    VALIDATOR_DELTAS_SLASHES_LEN,
>;

/// Epoched validator commission rate
pub type CommissionRates =
    crate::epoched::Epoched<Dec, crate::epoched::OffsetPipelineLen>;

/// Epoched validator's bonds
pub type Bonds = crate::epoched::EpochedDelta<
    token::Change,
    crate::epoched::OffsetPipelineLen,
    U64_MAX,
>;

/// An epoched lazy set of all known active validator addresses (consensus,
/// below-capacity, jailed)
pub type ValidatorAddresses = crate::epoched::NestedEpoched<
    LazySet<Address>,
    crate::epoched::OffsetPipelineLen,
>;

/// Slashes indexed by validator address and then block height (for easier
/// retrieval and iteration when processing)
pub type ValidatorSlashes = NestedMap<Address, Slashes>;

/// Epoched slashes, where the outer epoch key is the epoch in which the slash
/// is processed
/// NOTE: the `enqueued_slashes_handle` this is used for shouldn't need these
/// slashes earlier than `cubic_window_width` epochs behind the current
pub type EpochedSlashes = crate::epoched::NestedEpoched<
    ValidatorSlashes,
    crate::epoched::OffsetUnbondingLen,
    VALIDATOR_DELTAS_SLASHES_LEN,
>;

/// Epoched validator's unbonds
pub type Unbonds = NestedMap<Epoch, LazyMap<Epoch, token::Amount>>;

/// Consensus keys set, used to ensure uniqueness
pub type ConsensusKeys = LazySet<common::PublicKey>;

/// Total unbonded for validators needed for slashing computations.
/// The outer `Epoch` corresponds to the epoch at which the unbond is active
/// (affects the deltas, pipeline after submission). The inner `Epoch`
/// corresponds to the epoch from which the underlying bond became active
/// (affected deltas).
pub type ValidatorUnbondRecords =
    NestedMap<Epoch, LazyMap<Epoch, token::Amount>>;

#[derive(
    Debug, Clone, BorshSerialize, BorshDeserialize, Eq, Hash, PartialEq,
)]
/// TODO: slashed amount for thing
pub struct SlashedAmount {
    /// Perlangus
    pub amount: token::Amount,
    /// Churms
    pub epoch: Epoch,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
/// Commission rate and max commission rate change per epoch for a validator
pub struct CommissionPair {
    /// Validator commission rate
    pub commission_rate: Dec,
    /// Validator max commission rate change per epoch
    pub max_commission_change_per_epoch: Dec,
}

/// Epoched rewards products
pub type RewardsProducts = LazyMap<Epoch, Dec>;

/// Consensus validator rewards accumulator (for tracking the fractional block
/// rewards owed over the course of an epoch)
pub type RewardsAccumulator = LazyMap<Address, Dec>;

// --------------------------------------------------------------------------------------------

/// A genesis validator definition.
#[derive(
    Debug,
    Clone,
    BorshSerialize,
    BorshSchema,
    BorshDeserialize,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
)]
pub struct GenesisValidator {
    /// Validator's address
    pub address: Address,
    /// Staked tokens are put into a self-bond
    pub tokens: token::Amount,
    /// A public key used for signing validator's consensus actions
    pub consensus_key: common::PublicKey,
    /// An Eth bridge governance public key
    pub eth_cold_key: common::PublicKey,
    /// An Eth bridge hot signing public key used for validator set updates and
    /// cross-chain transactions
    pub eth_hot_key: common::PublicKey,
    /// Commission rate charged on rewards for delegators (bounded inside 0-1)
    pub commission_rate: Dec,
    /// Maximum change in commission rate permitted per epoch
    pub max_commission_rate_change: Dec,
}

/// An update of the consensus and below-capacity validator set.
#[derive(Debug, Clone, PartialEq)]
pub enum ValidatorSetUpdate {
    /// A validator is consensus-participating
    Consensus(ConsensusValidator),
    /// A validator who was consensus-participating in the last update but now
    /// is not
    Deactivated(common::PublicKey),
}

/// Consensus validator's consensus key and its bonded stake.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsensusValidator {
    /// A public key used for signing validator's consensus actions
    pub consensus_key: common::PublicKey,
    /// Total bonded stake of the validator
    pub bonded_stake: token::Amount,
}

/// ID of a bond and/or an unbond.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    BorshDeserialize,
    BorshSerialize,
    BorshSchema,
)]
pub struct BondId {
    /// (Un)bond's source address is the owner of the bonded tokens.
    pub source: Address,
    /// (Un)bond's validator address.
    pub validator: Address,
}

/// Validator's address with its voting power.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    BorshDeserialize,
    BorshSerialize,
    BorshSchema,
)]
pub struct WeightedValidator {
    /// The `total_stake` field must be on top, because lexicographic ordering
    /// is based on the top-to-bottom declaration order and in the
    /// `ValidatorSet` the `WeightedValidator`s these need to be sorted by
    /// the `total_stake`.
    pub bonded_stake: token::Amount,
    /// Validator's address
    pub address: Address,
}

impl Display for WeightedValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} with bonded stake {}",
            self.address,
            self.bonded_stake.to_string_native()
        )
    }
}

/// A position in a validator set
#[derive(
    PartialEq,
    PartialOrd,
    Ord,
    Debug,
    Default,
    Eq,
    Hash,
    Clone,
    Copy,
    BorshDeserialize,
    BorshSchema,
    BorshSerialize,
)]
pub struct Position(pub u64);

impl KeySeg for Position {
    fn parse(string: String) -> namada_core::types::storage::Result<Self>
    where
        Self: Sized,
    {
        let raw = u64::parse(string)?;
        Ok(Self(raw))
    }

    fn raw(&self) -> String {
        self.0.raw()
    }

    fn to_db_key(&self) -> namada_core::types::storage::DbKeySeg {
        self.0.to_db_key()
    }
}

impl Sub<Position> for Position {
    type Output = Self;

    fn sub(self, rhs: Position) -> Self::Output {
        Position(self.0 - rhs.0)
    }
}

impl Position {
    /// Position value of 1
    pub const ONE: Position = Position(1_u64);

    /// Get the next Position (+1)
    pub fn next(&self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

/// Validator's state.
#[derive(
    Debug,
    Clone,
    Copy,
    BorshDeserialize,
    BorshSerialize,
    BorshSchema,
    PartialEq,
    Eq,
)]
pub enum ValidatorState {
    /// A validator who may participate in the consensus
    Consensus,
    /// A validator who does not have enough stake to be considered in the
    /// `Consensus` validator set but still may have active bonds and unbonds
    BelowCapacity,
    /// A validator who has stake less than the `validator_stake_threshold`
    /// parameter
    BelowThreshold,
    /// A validator who is deactivated via a tx when a validator no longer
    /// wants to be one (not implemented yet)
    Inactive,
    /// A `Jailed` validator has been prohibited from participating in
    /// consensus due to a misbehavior
    Jailed,
}

/// A slash applied to validator, to punish byzantine behavior by removing
/// their staked tokens at and before the epoch of the slash.
#[derive(
    Debug,
    Clone,
    BorshDeserialize,
    BorshSerialize,
    BorshSchema,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
pub struct Slash {
    /// Epoch at which the slashable event occurred.
    pub epoch: Epoch,
    /// Block height at which the slashable event occurred.
    pub block_height: u64,
    /// A type of slashable event.
    pub r#type: SlashType,
    /// The cubic slashing rate for this validator
    pub rate: Dec,
}

/// Slashes applied to validator, to punish byzantine behavior by removing
/// their staked tokens at and before the epoch of the slash.
pub type Slashes = LazyVec<Slash>;

/// A type of slashable event.
#[derive(
    Debug,
    Clone,
    Copy,
    BorshDeserialize,
    BorshSerialize,
    BorshSchema,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
pub enum SlashType {
    /// Duplicate block vote.
    DuplicateVote,
    /// Light client attack.
    LightClientAttack,
}

/// VoteInfo inspired from tendermint for validators whose signature was
/// included in the last block
#[derive(Debug, Clone, BorshDeserialize, BorshSerialize)]
pub struct VoteInfo {
    /// Validator address
    pub validator_address: Address,
    /// validator voting power
    pub validator_vp: u64,
}

/// Bonds and unbonds with all details (slashes and rewards, if any)
/// grouped by their bond IDs.
pub type BondsAndUnbondsDetails = HashMap<BondId, BondsAndUnbondsDetail>;

/// Bonds and unbonds with all details (slashes and rewards, if any)
#[derive(Debug, Clone, BorshDeserialize, BorshSerialize, BorshSchema)]
pub struct BondsAndUnbondsDetail {
    /// Bonds
    pub bonds: Vec<BondDetails>,
    /// Unbonds
    pub unbonds: Vec<UnbondDetails>,
    /// Slashes applied to any of the bonds and/or unbonds
    pub slashes: Vec<Slash>,
}

/// Bond with all its details
#[derive(
    Debug, Clone, BorshDeserialize, BorshSerialize, BorshSchema, PartialEq,
)]
pub struct BondDetails {
    /// The first epoch in which this bond contributed to a stake
    pub start: Epoch,
    /// Token amount
    pub amount: token::Amount,
    /// Token amount that has been slashed, if any
    pub slashed_amount: Option<token::Amount>,
}

/// Unbond with all its details
#[derive(
    Debug, Clone, BorshDeserialize, BorshSerialize, BorshSchema, PartialEq,
)]
pub struct UnbondDetails {
    /// The first epoch in which the source bond of this unbond contributed to
    /// a stake
    pub start: Epoch,
    /// The first epoch in which this unbond can be withdrawn. Note that the
    /// epoch in which the unbond stopped contributing to the stake is
    /// `unbonding_len` param value before this epoch
    pub withdraw: Epoch,
    /// Token amount
    pub amount: token::Amount,
    /// Token amount that has been slashed, if any
    pub slashed_amount: Option<token::Amount>,
}

impl Display for BondId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{source: {}, validator: {}}}",
            self.source, self.validator
        )
    }
}

impl SlashType {
    /// Get the slash rate applicable to the given slash type from the PoS
    /// parameters.
    pub fn get_slash_rate(&self, params: &PosParams) -> Dec {
        match self {
            SlashType::DuplicateVote => params.duplicate_vote_min_slash_rate,
            SlashType::LightClientAttack => {
                params.light_client_attack_min_slash_rate
            }
        }
    }
}

impl Display for SlashType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SlashType::DuplicateVote => write!(f, "Duplicate vote"),
            SlashType::LightClientAttack => write!(f, "Light client attack"),
        }
    }
}

/// Calculate voting power in the tendermint context (which is stored as i64)
/// from the number of tokens
pub fn into_tm_voting_power(votes_per_token: Dec, tokens: Amount) -> i64 {
    let pow = votes_per_token
        * u128::try_from(tokens).expect("Voting power out of bounds");
    i64::try_from(pow.to_uint().expect("Cant fail"))
        .expect("Invalid voting power")
}

#[cfg(test)]
pub mod tests {

    use std::ops::Range;

    use proptest::prelude::*;

    use super::*;

    /// Generate arbitrary epoch in given range
    pub fn arb_epoch(range: Range<u64>) -> impl Strategy<Value = Epoch> {
        range.prop_map(Epoch)
    }
}
