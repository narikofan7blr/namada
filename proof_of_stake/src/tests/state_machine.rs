//! Test PoS transitions with a state machine

use std::cmp;
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::ops::Deref;

use assert_matches::assert_matches;
use itertools::Itertools;
use namada_core::ledger::storage::testing::TestWlStorage;
use namada_core::ledger::storage_api::collections::lazy_map::{
    Collectable, NestedSubKey,
};
use namada_core::ledger::storage_api::token::read_balance;
use namada_core::ledger::storage_api::{token, StorageRead};
use namada_core::types::address::{self, Address};
use namada_core::types::dec::Dec;
use namada_core::types::key;
use namada_core::types::key::common::PublicKey;
use namada_core::types::storage::Epoch;
use namada_core::types::token::Change;
use proptest::prelude::*;
use proptest::test_runner::Config;
use proptest_state_machine::{
    prop_state_machine, ReferenceStateMachine, StateMachineTest,
};
// Use `RUST_LOG=info` (or another tracing level) and `--nocapture` to see
// `tracing` logs from tests
use test_log::test;

use crate::parameters::testing::arb_rate;
use crate::parameters::PosParams;
use crate::tests::arb_params_and_genesis_validators;
use crate::types::{
    BondId, EagerRedelegatedBondsMap, GenesisValidator, ReverseOrdTokenAmount,
    Slash, SlashType, ValidatorState, WeightedValidator,
};
use crate::{
    below_capacity_validator_set_handle, consensus_validator_set_handle,
    enqueued_slashes_handle, read_below_threshold_validator_set_addresses,
    read_pos_params, redelegate_tokens, validator_deltas_handle,
    validator_incoming_redelegations_handle,
    validator_outgoing_redelegations_handle, validator_slashes_handle,
    validator_state_handle, BondsForRemovalRes, EagerRedelegatedUnbonds,
    FoldRedelegatedBondsResult, ModifiedRedelegation, RedelegationError,
    ResultSlashing,
};

prop_state_machine! {
    #![proptest_config(Config {
        cases: 2,
        .. Config::default()
    })]
    #[test]
    /// A `StateMachineTest` implemented on `PosState`
    fn pos_state_machine_test(sequential 500 => ConcretePosState);
}

type AbstractDelegatorRedelegatedBonded = BTreeMap<
    Address,
    BTreeMap<
        Address,
        BTreeMap<Epoch, BTreeMap<Address, BTreeMap<Epoch, token::Change>>>,
    >,
>;

type AbstractDelegatorRedelegatedUnbonded = BTreeMap<
    Address,
    BTreeMap<
        Address,
        BTreeMap<
            (Epoch, Epoch),
            BTreeMap<Address, BTreeMap<Epoch, token::Amount>>,
        >,
    >,
>;

type AbstractValidatorTotalRedelegatedBonded = BTreeMap<
    Address,
    BTreeMap<Epoch, BTreeMap<Address, BTreeMap<Epoch, token::Amount>>>,
>;

type AbstractTotalRedelegatedUnbonded = BTreeMap<
    Epoch,
    BTreeMap<Epoch, BTreeMap<Address, BTreeMap<Epoch, token::Amount>>>,
>;

type AbstractValidatorTotalRedelegatedUnbonded = BTreeMap<
    Address,
    BTreeMap<
        Epoch,
        BTreeMap<Epoch, BTreeMap<Address, BTreeMap<Epoch, token::Amount>>>,
    >,
>;

type AbstractIncomingRedelegations =
    BTreeMap<Address, BTreeMap<Address, Epoch>>;
type AbstractOutgoingRedelegations = BTreeMap<
    Address,
    BTreeMap<Address, BTreeMap<(Epoch, Epoch), token::Amount>>,
>;

/// Abstract representation of a state of PoS system
#[derive(Clone, Debug)]
struct AbstractPosState {
    /// Current epoch
    epoch: Epoch,
    /// Parameters
    params: PosParams,
    /// Genesis validators
    genesis_validators: Vec<GenesisValidator>,
    /// Bonds delta values. The outer key for Epoch is pipeline offset from
    /// epoch in which the bond is applied
    bonds: BTreeMap<BondId, BTreeMap<Epoch, token::Change>>,
    /// Total bonded tokens to a validator in each epoch. This is never
    /// decremented and used for slashing computations.
    total_bonded: BTreeMap<Address, BTreeMap<Epoch, token::Change>>,
    /// Validator stakes. These are NOT deltas.
    /// Pipelined.
    validator_stakes: BTreeMap<Epoch, BTreeMap<Address, token::Change>>,
    /// Consensus validator set. Pipelined.
    consensus_set: BTreeMap<Epoch, BTreeMap<token::Amount, VecDeque<Address>>>,
    /// Below-capacity validator set. Pipelined.
    below_capacity_set:
        BTreeMap<Epoch, BTreeMap<ReverseOrdTokenAmount, VecDeque<Address>>>,
    /// Below-threshold validator set. Pipelined.
    below_threshold_set: BTreeMap<Epoch, HashSet<Address>>,
    /// Validator states. Pipelined.
    validator_states: BTreeMap<Epoch, BTreeMap<Address, ValidatorState>>,
    /// Unbonded bonds. The outer key for Epoch is pipeline + unbonding +
    /// cubic_window offset from epoch in which the unbond transition
    /// occurs.
    unbonds: BTreeMap<(Epoch, Epoch), BTreeMap<BondId, token::Amount>>,
    /// Validator slashes post-processing
    validator_slashes: BTreeMap<Address, Vec<Slash>>,
    /// Enqueued slashes pre-processing
    enqueued_slashes: BTreeMap<Epoch, BTreeMap<Address, Vec<Slash>>>,
    /// The last epoch in which a validator committed an infraction
    validator_last_slash_epochs: BTreeMap<Address, Epoch>,
    /// Validator's total unbonded required for slashing.
    /// Inner `Epoch` is the epoch in which the unbond became active.
    /// Outer `Epoch` is the epoch in which the underlying bond became active.
    total_unbonded:
        BTreeMap<Address, BTreeMap<Epoch, BTreeMap<Epoch, token::Amount>>>,
    /// The outer key is the epoch in which redelegation became active
    /// (pipeline offset). The next key is the address of the delegator.
    delegator_redelegated_bonded: AbstractDelegatorRedelegatedBonded,
    delegator_redelegated_unbonded: AbstractDelegatorRedelegatedUnbonded,
    validator_total_redelegated_bonded: AbstractValidatorTotalRedelegatedBonded,
    validator_total_redelegated_unbonded:
        AbstractValidatorTotalRedelegatedUnbonded,
    incoming_redelegations: AbstractIncomingRedelegations,
    outgoing_redelegations: AbstractOutgoingRedelegations,
}

/// The PoS system under test
#[derive(Debug)]
struct ConcretePosState {
    /// Storage - contains all the PoS state
    s: TestWlStorage,
}

/// State machine transitions
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
enum Transition {
    NextEpoch,
    InitValidator {
        address: Address,
        consensus_key: PublicKey,
        eth_cold_key: PublicKey,
        eth_hot_key: PublicKey,
        commission_rate: Dec,
        max_commission_rate_change: Dec,
    },
    Bond {
        id: BondId,
        amount: token::Amount,
    },
    Unbond {
        id: BondId,
        amount: token::Amount,
    },
    Withdraw {
        id: BondId,
    },
    Redelegate {
        /// A chained redelegation must fail
        is_chained: bool,
        id: BondId,
        new_validator: Address,
        amount: token::Amount,
    },
    Misbehavior {
        address: Address,
        slash_type: SlashType,
        infraction_epoch: Epoch,
        height: u64,
    },
    UnjailValidator {
        address: Address,
    },
}

impl StateMachineTest for ConcretePosState {
    type Reference = AbstractPosState;
    type SystemUnderTest = Self;

    fn init_test(
        initial_state: &<Self::Reference as ReferenceStateMachine>::State,
    ) -> Self::SystemUnderTest {
        tracing::debug!("New test case");
        tracing::debug!(
            "Genesis validators: {:#?}",
            initial_state
                .genesis_validators
                .iter()
                .map(|val| &val.address)
                .collect::<Vec<_>>()
        );
        let mut s = TestWlStorage::default();
        crate::init_genesis(
            &mut s,
            &initial_state.params,
            initial_state.genesis_validators.clone().into_iter(),
            initial_state.epoch,
        )
        .unwrap();
        Self { s }
    }

    fn apply(
        mut state: Self::SystemUnderTest,
        ref_state: &<Self::Reference as ReferenceStateMachine>::State,
        transition: <Self::Reference as ReferenceStateMachine>::Transition,
    ) -> Self::SystemUnderTest {
        let params = crate::read_pos_params(&state.s).unwrap();
        let pos_balance = read_balance(
            &state.s,
            &state.s.storage.native_token,
            &crate::ADDRESS,
        )
        .unwrap();
        tracing::debug!("PoS balance: {}", pos_balance.to_string_native());
        match transition {
            Transition::NextEpoch => {
                tracing::debug!("\nCONCRETE Next epoch");
                super::advance_epoch(&mut state.s, &params);

                // Need to apply some slashing
                let current_epoch = state.s.storage.block.epoch;
                super::process_slashes(&mut state.s, current_epoch).unwrap();

                let params = read_pos_params(&state.s).unwrap();
                state.check_next_epoch_post_conditions(&params);
            }
            Transition::InitValidator {
                address,
                consensus_key,
                eth_cold_key,
                eth_hot_key,
                commission_rate,
                max_commission_rate_change,
            } => {
                tracing::debug!("\nCONCRETE Init validator");
                let current_epoch = state.current_epoch();

                super::become_validator(super::BecomeValidator {
                    storage: &mut state.s,
                    params: &params,
                    address: &address,
                    consensus_key: &consensus_key,
                    eth_cold_key: &eth_cold_key,
                    eth_hot_key: &eth_hot_key,
                    current_epoch,
                    commission_rate,
                    max_commission_rate_change,
                })
                .unwrap();

                let params = read_pos_params(&state.s).unwrap();
                state.check_init_validator_post_conditions(
                    current_epoch,
                    &params,
                    &address,
                )
            }
            Transition::Bond { id, amount } => {
                tracing::debug!("\nCONCRETE Bond");
                let current_epoch = state.current_epoch();
                let pipeline = current_epoch + params.pipeline_len;
                let validator_stake_before_bond_cur =
                    crate::read_validator_stake(
                        &state.s,
                        &params,
                        &id.validator,
                        current_epoch,
                    )
                    .unwrap();
                let validator_stake_before_bond_pipeline =
                    crate::read_validator_stake(
                        &state.s,
                        &params,
                        &id.validator,
                        pipeline,
                    )
                    .unwrap();

                // Credit tokens to ensure we can apply the bond
                let native_token = state.s.get_native_token().unwrap();
                let pos = address::POS;
                token::credit_tokens(
                    &mut state.s,
                    &native_token,
                    &id.source,
                    amount,
                )
                .unwrap();

                let src_balance_pre =
                    token::read_balance(&state.s, &native_token, &id.source)
                        .unwrap();
                let pos_balance_pre =
                    token::read_balance(&state.s, &native_token, &pos).unwrap();

                // This must be ensured by both transitions generator and
                // pre-conditions!
                assert!(
                    crate::is_validator(&state.s, &id.validator).unwrap(),
                    "{} is not a validator",
                    id.validator
                );

                // Apply the bond
                super::bond_tokens(
                    &mut state.s,
                    Some(&id.source),
                    &id.validator,
                    amount,
                    current_epoch,
                )
                .unwrap();

                let params = read_pos_params(&state.s).unwrap();
                state.check_bond_post_conditions(
                    current_epoch,
                    &params,
                    id.clone(),
                    amount,
                    validator_stake_before_bond_cur,
                    validator_stake_before_bond_pipeline,
                );

                let src_balance_post =
                    token::read_balance(&state.s, &native_token, &id.source)
                        .unwrap();
                let pos_balance_post =
                    token::read_balance(&state.s, &native_token, &pos).unwrap();

                // Post-condition: PoS balance should increase
                assert!(pos_balance_pre < pos_balance_post);
                // Post-condition: The difference in PoS balance should be the
                // same as in the source
                assert_eq!(
                    pos_balance_post - pos_balance_pre,
                    src_balance_pre - src_balance_post
                );

                // Check that the bonds are the same
                let abs_bonds = ref_state.bonds.get(&id).cloned().unwrap();
                let conc_bonds = crate::bond_handle(&id.source, &id.validator)
                    .get_data_handler()
                    .collect_map(&state.s)
                    .unwrap();
                assert_eq!(abs_bonds, conc_bonds);
            }
            Transition::Unbond { id, amount } => {
                tracing::debug!("\nCONCRETE Unbond");
                let current_epoch = state.current_epoch();
                let pipeline = current_epoch + params.pipeline_len;
                let native_token = state.s.get_native_token().unwrap();
                let pos = address::POS;
                let src_balance_pre =
                    token::read_balance(&state.s, &native_token, &id.source)
                        .unwrap();
                let pos_balance_pre =
                    token::read_balance(&state.s, &native_token, &pos).unwrap();

                let validator_stake_before_unbond_cur =
                    crate::read_validator_stake(
                        &state.s,
                        &params,
                        &id.validator,
                        current_epoch,
                    )
                    .unwrap();
                let validator_stake_before_unbond_pipeline =
                    crate::read_validator_stake(
                        &state.s,
                        &params,
                        &id.validator,
                        pipeline,
                    )
                    .unwrap();

                // Apply the unbond
                super::unbond_tokens(
                    &mut state.s,
                    Some(&id.source),
                    &id.validator,
                    amount,
                    current_epoch,
                    false,
                )
                .unwrap();

                let params = read_pos_params(&state.s).unwrap();
                state.check_unbond_post_conditions(
                    current_epoch,
                    &params,
                    id.clone(),
                    amount,
                    validator_stake_before_unbond_cur,
                    validator_stake_before_unbond_pipeline,
                );

                let src_balance_post =
                    token::read_balance(&state.s, &native_token, &id.source)
                        .unwrap();
                let pos_balance_post =
                    token::read_balance(&state.s, &native_token, &pos).unwrap();

                // Post-condition: PoS balance should not change
                assert_eq!(pos_balance_pre, pos_balance_post);
                // Post-condition: Source balance should not change
                assert_eq!(src_balance_post, src_balance_pre);

                // Check that the bonds are the same
                let abs_bonds = ref_state.bonds.get(&id).cloned().unwrap();
                let conc_bonds = crate::bond_handle(&id.source, &id.validator)
                    .get_data_handler()
                    .collect_map(&state.s)
                    .unwrap();
                assert_eq!(abs_bonds, conc_bonds);

                // Check that the unbond records are the same
                // TODO: figure out how we get entries with 0 amount in the
                // abstract version (and prevent)
                let mut abs_total_unbonded = ref_state
                    .total_unbonded
                    .get(&id.validator)
                    .cloned()
                    .unwrap();
                abs_total_unbonded.retain(|_, inner_map| {
                    inner_map.retain(|_, value| !value.is_zero());
                    !inner_map.is_empty()
                });
                let conc_total_unbonded =
                    crate::total_unbonded_handle(&id.validator)
                        .collect_map(&state.s)
                        .unwrap();
                assert_eq!(abs_total_unbonded, conc_total_unbonded);
            }
            Transition::Withdraw {
                id: BondId { source, validator },
            } => {
                tracing::debug!("\nCONCRETE Withdraw");
                let current_epoch = state.current_epoch();
                let native_token = state.s.get_native_token().unwrap();
                let pos = address::POS;
                let slash_pool = address::POS_SLASH_POOL;
                let src_balance_pre =
                    token::read_balance(&state.s, &native_token, &source)
                        .unwrap();
                let pos_balance_pre =
                    token::read_balance(&state.s, &native_token, &pos).unwrap();
                let slash_balance_pre =
                    token::read_balance(&state.s, &native_token, &slash_pool)
                        .unwrap();

                // Apply the withdrawal
                let withdrawn = super::withdraw_tokens(
                    &mut state.s,
                    Some(&source),
                    &validator,
                    current_epoch,
                )
                .unwrap();

                let src_balance_post =
                    token::read_balance(&state.s, &native_token, &source)
                        .unwrap();
                let pos_balance_post =
                    token::read_balance(&state.s, &native_token, &pos).unwrap();
                let slash_balance_post =
                    token::read_balance(&state.s, &native_token, &slash_pool)
                        .unwrap();

                // Post-condition: PoS balance should decrease or not change if
                // nothing was withdrawn
                assert!(pos_balance_pre >= pos_balance_post);
                // Post-condition: The difference in PoS balance should be equal
                // to the sum of the difference in the source and the difference
                // in the slash pool
                assert_eq!(
                    pos_balance_pre - pos_balance_post,
                    src_balance_post - src_balance_pre + slash_balance_post
                        - slash_balance_pre
                );
                // Post-condition: The increment in source balance should be
                // equal to the withdrawn amount
                assert_eq!(src_balance_post - src_balance_pre, withdrawn);
            }
            Transition::Redelegate {
                is_chained,
                id,
                new_validator,
                amount,
            } => {
                tracing::debug!("\nCONCRETE Redelegate");

                let current_epoch = state.current_epoch();
                let pipeline = current_epoch + params.pipeline_len;

                // Read data prior to applying the transition
                let native_token = state.s.get_native_token().unwrap();
                let pos = address::POS;
                let pos_balance_pre =
                    token::read_balance(&state.s, &native_token, &pos).unwrap();
                let slash_pool = address::POS_SLASH_POOL;
                let slash_balance_pre =
                    token::read_balance(&state.s, &native_token, &slash_pool)
                        .unwrap();

                // Read src validator stakes
                let src_validator_stake_cur_pre = crate::read_validator_stake(
                    &state.s,
                    &params,
                    &id.validator,
                    current_epoch,
                )
                .unwrap();
                let _src_validator_stake_pipeline_pre =
                    crate::read_validator_stake(
                        &state.s,
                        &params,
                        &id.validator,
                        pipeline,
                    )
                    .unwrap();

                // Read dest validator stakes
                let dest_validator_stake_cur_pre = crate::read_validator_stake(
                    &state.s,
                    &params,
                    &new_validator,
                    current_epoch,
                )
                .unwrap();
                let _dest_validator_stake_pipeline_pre =
                    crate::read_validator_stake(
                        &state.s,
                        &params,
                        &new_validator,
                        pipeline,
                    )
                    .unwrap();

                // Find delegations
                let delegations_pre =
                    crate::find_delegations(&state.s, &id.source, &pipeline)
                        .unwrap();

                // Apply redelegation
                let result = redelegate_tokens(
                    &mut state.s,
                    &id.source,
                    &id.validator,
                    &new_validator,
                    current_epoch,
                    amount,
                );

                let conc_incoming_src =
                    validator_incoming_redelegations_handle(&id.validator)
                        .collect_map(&state.s)
                        .unwrap();
                let conc_incoming_dest =
                    validator_incoming_redelegations_handle(&new_validator)
                        .collect_map(&state.s)
                        .unwrap();
                let conc_outgoing_src =
                    validator_outgoing_redelegations_handle(&id.validator)
                        .collect_map(&state.s)
                        .unwrap();
                let conc_outgoing_dest =
                    validator_outgoing_redelegations_handle(&new_validator)
                        .collect_map(&state.s)
                        .unwrap();

                let abs_src_incoming = ref_state
                    .incoming_redelegations
                    .get(&id.validator)
                    .cloned()
                    .unwrap_or_default();
                let abs_dest_incoming = ref_state
                    .incoming_redelegations
                    .get(&new_validator)
                    .cloned()
                    .unwrap_or_default();
                let abs_src_outgoing = ref_state
                    .outgoing_redelegations
                    .get(&id.validator)
                    .cloned()
                    .unwrap_or_default();
                let abs_dest_outgoing = ref_state
                    .outgoing_redelegations
                    .get(&new_validator)
                    .cloned()
                    .unwrap_or_default();

                for (del, epoch) in conc_incoming_src {
                    assert_eq!(
                        abs_src_incoming.get(&del).cloned().unwrap(),
                        epoch
                    );
                }
                for (del, epoch) in conc_incoming_dest {
                    assert_eq!(
                        abs_dest_incoming.get(&del).cloned().unwrap(),
                        epoch
                    );
                }

                for (dest, map) in conc_outgoing_src {
                    for (bond_start, redel_map) in map {
                        for (redel_start, amount) in redel_map {
                            let a = abs_src_outgoing.get(&dest).unwrap();
                            // dbg!(&a);
                            assert_eq!(
                                a.get(&(bond_start, redel_start))
                                    .cloned()
                                    .unwrap(),
                                amount
                            );
                        }
                    }
                }
                for (dest, map) in conc_outgoing_dest {
                    for (bond_start, redel_map) in map {
                        for (redel_start, amount) in redel_map {
                            assert_eq!(
                                abs_dest_outgoing
                                    .get(&dest)
                                    .unwrap()
                                    .get(&(bond_start, redel_start))
                                    .cloned()
                                    .unwrap(),
                                amount
                            );
                        }
                    }
                }

                if is_chained && !amount.is_zero() {
                    assert!(result.is_err());
                    let err = result.unwrap_err();
                    let err_str = err.to_string();
                    assert_matches!(
                        err.downcast::<RedelegationError>().unwrap().deref(),
                        RedelegationError::IsChainedRedelegation,
                        "A chained redelegation must be rejected, got \
                         {err_str}",
                    );
                } else {
                    result.unwrap();

                    // Post-condition: PoS balance is unchanged
                    let pos_balance_post =
                        token::read_balance(&state.s, &native_token, &pos)
                            .unwrap();
                    assert_eq!(pos_balance_pre, pos_balance_post);

                    // Find slash pool balance difference
                    let slash_balance_post = token::read_balance(
                        &state.s,
                        &native_token,
                        &slash_pool,
                    )
                    .unwrap();
                    let slashed = slash_balance_post - slash_balance_pre;

                    // Post-condition: Source validator stake at current epoch
                    // is unchanged
                    let src_validator_stake_cur_post =
                        crate::read_validator_stake(
                            &state.s,
                            &params,
                            &id.validator,
                            current_epoch,
                        )
                        .unwrap();
                    assert_eq!(
                        src_validator_stake_cur_pre,
                        src_validator_stake_cur_post
                    );

                    // Post-condition: Source validator stake at pipeline epoch
                    // is reduced by the redelegation amount

                    // TODO: shouldn't this be reduced by the redelegation
                    // amount post-slashing tho?
                    //   NOTE: We changed it to reduce it, check again later
                    let _amount_after_slash = amount - slashed;
                    let _src_validator_stake_pipeline_post =
                        crate::read_validator_stake(
                            &state.s,
                            &params,
                            &id.validator,
                            pipeline,
                        )
                        .unwrap();
                    // assert_eq!(
                    //     src_validator_stake_pipeline_pre -
                    // amount_after_slash,
                    //     src_validator_stake_pipeline_post
                    // );

                    // Post-condition: Destination validator stake at current
                    // epoch is unchanged
                    let dest_validator_stake_cur_post =
                        crate::read_validator_stake(
                            &state.s,
                            &params,
                            &new_validator,
                            current_epoch,
                        )
                        .unwrap();
                    assert_eq!(
                        dest_validator_stake_cur_pre,
                        dest_validator_stake_cur_post
                    );

                    // Post-condition: Destination validator stake at pipeline
                    // epoch is increased by the redelegation amount, less any
                    // slashes
                    let _dest_validator_stake_pipeline_post =
                        crate::read_validator_stake(
                            &state.s,
                            &params,
                            &new_validator,
                            pipeline,
                        )
                        .unwrap();
                    // assert_eq!(
                    //     dest_validator_stake_pipeline_pre +
                    // amount_after_slash,
                    //     dest_validator_stake_pipeline_post
                    // );

                    // Post-condition: The delegator's delegations should be
                    // updated with redelegation. For the source reduced by the
                    // redelegation amount and for the destination increased by
                    // the redelegation amount, less any slashes.
                    let delegations_post = crate::find_delegations(
                        &state.s, &id.source, &pipeline,
                    )
                    .unwrap();
                    let src_delegation_pre = delegations_pre
                        .get(&id.validator)
                        .cloned()
                        .unwrap_or_default();
                    let src_delegation_post = delegations_post
                        .get(&id.validator)
                        .cloned()
                        .unwrap_or_default();
                    assert_eq!(
                        src_delegation_pre - src_delegation_post,
                        amount
                    );
                    let _dest_delegation_pre = delegations_pre
                        .get(&new_validator)
                        .cloned()
                        .unwrap_or_default();
                    let _dest_delegation_post = delegations_post
                        .get(&new_validator)
                        .cloned()
                        .unwrap_or_default();
                    // assert_eq!(
                    //     dest_delegation_post - dest_delegation_pre,
                    //     amount_after_slash
                    // );
                }
            }
            Transition::Misbehavior {
                address,
                slash_type,
                infraction_epoch,
                height,
            } => {
                tracing::debug!("\nCONCRETE Misbehavior");
                let current_epoch = state.current_epoch();
                // Record the slash evidence
                super::slash(
                    &mut state.s,
                    &params,
                    current_epoch,
                    infraction_epoch,
                    height,
                    slash_type,
                    &address,
                    current_epoch.next(),
                )
                .unwrap();

                // Apply some post-conditions
                let params = read_pos_params(&state.s).unwrap();
                state.check_misbehavior_post_conditions(
                    &params,
                    current_epoch,
                    infraction_epoch,
                    slash_type,
                    &address,
                );

                // TODO: Any others?
            }
            Transition::UnjailValidator { address } => {
                tracing::debug!("\nCONCRETE UnjailValidator");
                let current_epoch = state.current_epoch();

                // Unjail the validator
                super::unjail_validator(&mut state.s, &address, current_epoch)
                    .unwrap();

                // Post-conditions
                let params = read_pos_params(&state.s).unwrap();
                state.check_unjail_validator_post_conditions(&params, &address);
            }
        }
        state
    }

    fn check_invariants(
        state: &Self::SystemUnderTest,
        ref_state: &<Self::Reference as ReferenceStateMachine>::State,
    ) {
        let current_epoch = state.current_epoch();
        let params = read_pos_params(&state.s).unwrap();
        state.check_global_post_conditions(&params, current_epoch, ref_state);
    }
}

impl ConcretePosState {
    fn current_epoch(&self) -> Epoch {
        self.s.storage.block.epoch
    }

    fn check_next_epoch_post_conditions(&self, params: &PosParams) {
        let pipeline = self.current_epoch() + params.pipeline_len;
        let before_pipeline = pipeline.prev();

        // Post-condition: Consensus validator sets at pipeline offset
        // must be the same as at the epoch before it.
        let consensus_set_before_pipeline =
            crate::read_consensus_validator_set_addresses_with_stake(
                &self.s,
                before_pipeline,
            )
            .unwrap();
        let consensus_set_at_pipeline =
            crate::read_consensus_validator_set_addresses_with_stake(
                &self.s, pipeline,
            )
            .unwrap();
        itertools::assert_equal(
            consensus_set_before_pipeline.into_iter().sorted(),
            consensus_set_at_pipeline.into_iter().sorted(),
        );

        // Post-condition: Below-capacity validator sets at pipeline
        // offset must be the same as at the epoch before it.
        let below_cap_before_pipeline =
            crate::read_below_capacity_validator_set_addresses_with_stake(
                &self.s,
                before_pipeline,
            )
            .unwrap();
        let below_cap_at_pipeline =
            crate::read_below_capacity_validator_set_addresses_with_stake(
                &self.s, pipeline,
            )
            .unwrap();
        itertools::assert_equal(
            below_cap_before_pipeline.into_iter().sorted(),
            below_cap_at_pipeline.into_iter().sorted(),
        );

        // TODO: post-conditions for processing of slashes, just throwing things
        // here atm
        let slashed_validators = enqueued_slashes_handle()
            .at(&self.current_epoch())
            .iter(&self.s)
            .unwrap()
            .map(|a| {
                let (
                    NestedSubKey::Data {
                        key: address,
                        nested_sub_key: _,
                    },
                    _b,
                ) = a.unwrap();
                address
            })
            .collect::<HashSet<Address>>();

        for validator in &slashed_validators {
            assert!(
                !validator_slashes_handle(validator)
                    .is_empty(&self.s)
                    .unwrap()
            );
            assert_eq!(
                validator_state_handle(validator)
                    .get(&self.s, self.current_epoch(), params)
                    .unwrap(),
                Some(ValidatorState::Jailed)
            );
        }
    }

    fn check_bond_post_conditions(
        &self,
        submit_epoch: Epoch,
        params: &PosParams,
        id: BondId,
        amount: token::Amount,
        validator_stake_before_bond_cur: token::Amount,
        validator_stake_before_bond_pipeline: token::Amount,
    ) {
        let pipeline = submit_epoch + params.pipeline_len;

        let cur_stake = super::read_validator_stake(
            &self.s,
            params,
            &id.validator,
            submit_epoch,
        )
        .unwrap();

        // Post-condition: the validator stake at the current epoch should not
        // change
        assert_eq!(cur_stake, validator_stake_before_bond_cur);

        let stake_at_pipeline = super::read_validator_stake(
            &self.s,
            params,
            &id.validator,
            pipeline,
        )
        .unwrap();

        // Post-condition: the validator stake at the pipeline should be
        // incremented by the bond amount
        assert_eq!(
            stake_at_pipeline,
            validator_stake_before_bond_pipeline + amount
        );

        self.check_bond_and_unbond_post_conditions(
            submit_epoch,
            params,
            id,
            stake_at_pipeline,
        );
    }

    fn check_unbond_post_conditions(
        &self,
        submit_epoch: Epoch,
        params: &PosParams,
        id: BondId,
        amount: token::Amount,
        validator_stake_before_unbond_cur: token::Amount,
        validator_stake_before_unbond_pipeline: token::Amount,
    ) {
        let pipeline = submit_epoch + params.pipeline_len;

        let cur_stake = super::read_validator_stake(
            &self.s,
            params,
            &id.validator,
            submit_epoch,
        )
        .unwrap();

        // Post-condition: the validator stake at the current epoch should not
        // change
        assert_eq!(cur_stake, validator_stake_before_unbond_cur);

        let stake_at_pipeline = super::read_validator_stake(
            &self.s,
            params,
            &id.validator,
            pipeline,
        )
        .unwrap();

        // Post-condition: the validator stake at the pipeline should be
        // decremented at most by the bond amount (because slashing can reduce
        // the actual amount unbonded)
        //
        // TODO: is this a weak assertion here? Seems cumbersome to calculate
        // the exact amount considering the slashing applied can be complicated
        assert!(
            stake_at_pipeline
                >= validator_stake_before_unbond_pipeline
                    .checked_sub(amount)
                    .unwrap_or_default()
        );

        self.check_bond_and_unbond_post_conditions(
            submit_epoch,
            params,
            id,
            stake_at_pipeline,
        );
    }

    /// These post-conditions apply to bonding and unbonding
    fn check_bond_and_unbond_post_conditions(
        &self,
        submit_epoch: Epoch,
        params: &PosParams,
        id: BondId,
        stake_at_pipeline: token::Amount,
    ) {
        let pipeline = submit_epoch + params.pipeline_len;
        // Read the consensus sets data using iterator
        let num_in_consensus = crate::consensus_validator_set_handle()
            .at(&pipeline)
            .iter(&self.s)
            .unwrap()
            .map(|res| res.unwrap())
            .filter(|(_keys, addr)| addr == &id.validator)
            .count();

        let num_in_below_cap = crate::below_capacity_validator_set_handle()
            .at(&pipeline)
            .iter(&self.s)
            .unwrap()
            .map(|res| res.unwrap())
            .filter(|(_keys, addr)| addr == &id.validator)
            .count();

        let num_in_below_thresh =
            read_below_threshold_validator_set_addresses(&self.s, pipeline)
                .unwrap()
                .into_iter()
                .filter(|addr| addr == &id.validator)
                .count();

        let num_occurrences =
            num_in_consensus + num_in_below_cap + num_in_below_thresh;
        let validator_is_jailed = crate::validator_state_handle(&id.validator)
            .get(&self.s, pipeline, params)
            .unwrap()
            == Some(ValidatorState::Jailed);

        // Post-condition: There must only be one instance of this validator in
        // the consensus + below-cap sets with some stake across all
        // validator sets, OR there are no instances and this validator is
        // jailed
        assert!(
            num_occurrences == 1
                || (num_occurrences == 0 && validator_is_jailed)
        );

        let consensus_set =
            crate::read_consensus_validator_set_addresses_with_stake(
                &self.s, pipeline,
            )
            .unwrap();
        let below_cap_set =
            crate::read_below_capacity_validator_set_addresses_with_stake(
                &self.s, pipeline,
            )
            .unwrap();
        let below_thresh_set =
            crate::read_below_threshold_validator_set_addresses(
                &self.s, pipeline,
            )
            .unwrap();
        let weighted = WeightedValidator {
            bonded_stake: stake_at_pipeline,
            address: id.validator,
        };
        let consensus_val = consensus_set.get(&weighted);
        let below_cap_val = below_cap_set.get(&weighted);
        let below_thresh_val = below_thresh_set.get(&weighted.address);

        // Post-condition: The validator should be updated in exactly once in
        // the validator sets
        let jailed_condition = validator_is_jailed
            && consensus_val.is_none()
            && below_cap_val.is_none()
            && below_thresh_val.is_none();

        let mut num_sets = i32::from(consensus_val.is_some());
        num_sets += i32::from(below_cap_val.is_some());
        num_sets += i32::from(below_thresh_val.is_some());

        assert!(num_sets == 1 || jailed_condition);

        // Post-condition: The stake of the validators in the consensus set is
        // greater than or equal to below-capacity validators
        for WeightedValidator {
            bonded_stake: consensus_stake,
            address: consensus_addr,
        } in consensus_set.iter()
        {
            for WeightedValidator {
                bonded_stake: below_cap_stake,
                address: below_cap_addr,
            } in below_cap_set.iter()
            {
                assert!(
                    consensus_stake >= below_cap_stake,
                    "Consensus validator {consensus_addr} with stake {} and \
                     below-capacity {below_cap_addr} with stake {} should be \
                     swapped.",
                    consensus_stake.to_string_native(),
                    below_cap_stake.to_string_native()
                );
            }
        }
    }

    fn check_init_validator_post_conditions(
        &self,
        submit_epoch: Epoch,
        params: &PosParams,
        address: &Address,
    ) {
        let pipeline = submit_epoch + params.pipeline_len;

        // Post-condition: the validator should not be in the validator set
        // until the pipeline epoch
        for epoch in submit_epoch.iter_range(params.pipeline_len) {
            assert!(
                !crate::read_consensus_validator_set_addresses(&self.s, epoch)
                    .unwrap()
                    .contains(address)
            );
            assert!(
                !crate::read_below_capacity_validator_set_addresses(
                    &self.s, epoch
                )
                .unwrap()
                .contains(address)
            );
            assert!(
                !crate::read_below_threshold_validator_set_addresses(
                    &self.s, epoch
                )
                .unwrap()
                .contains(address)
            );
            assert!(
                !crate::read_all_validator_addresses(&self.s, epoch)
                    .unwrap()
                    .contains(address)
            );
        }
        let in_consensus =
            crate::read_consensus_validator_set_addresses(&self.s, pipeline)
                .unwrap()
                .contains(address);
        let in_bc = crate::read_below_capacity_validator_set_addresses(
            &self.s, pipeline,
        )
        .unwrap()
        .contains(address);
        let in_below_thresh =
            crate::read_below_threshold_validator_set_addresses(
                &self.s, pipeline,
            )
            .unwrap()
            .contains(address);

        assert!(in_below_thresh && !in_consensus && !in_bc);
    }

    fn check_misbehavior_post_conditions(
        &self,
        params: &PosParams,
        current_epoch: Epoch,
        infraction_epoch: Epoch,
        slash_type: SlashType,
        validator: &Address,
    ) {
        tracing::debug!(
            "\nChecking misbehavior post conditions for validator: \n{}",
            validator
        );

        // Validator state jailed and validator removed from the consensus set
        // starting at the next epoch
        for offset in 1..=params.pipeline_len {
            // dbg!(
            //     crate::read_consensus_validator_set_addresses_with_stake(
            //         &self.s,
            //         current_epoch + offset
            //     )
            //     .unwrap()
            // );
            assert_eq!(
                validator_state_handle(validator)
                    .get(&self.s, current_epoch + offset, params)
                    .unwrap(),
                Some(ValidatorState::Jailed)
            );
            let in_consensus = consensus_validator_set_handle()
                .at(&(current_epoch + offset))
                .iter(&self.s)
                .unwrap()
                .any(|res| {
                    let (_, val_address) = res.unwrap();
                    // dbg!(&val_address);
                    val_address == validator.clone()
                });
            assert!(!in_consensus);
        }

        // `enqueued_slashes` contains the slash element just added
        let processing_epoch = infraction_epoch
            + params.unbonding_len
            + 1_u64
            + params.cubic_slashing_window_length;
        let slash = enqueued_slashes_handle()
            .at(&processing_epoch)
            .at(validator)
            .back(&self.s)
            .unwrap();
        if let Some(slash) = slash {
            assert_eq!(slash.epoch, infraction_epoch);
            assert_eq!(slash.r#type, slash_type);
            assert_eq!(slash.rate, Dec::zero());
        } else {
            panic!("Could not find the slash enqueued");
        }

        // TODO: Any others?
    }

    fn check_unjail_validator_post_conditions(
        &self,
        params: &PosParams,
        validator: &Address,
    ) {
        let current_epoch = self.s.storage.block.epoch;

        // Make sure the validator is not in either set until the pipeline epoch
        for epoch in current_epoch.iter_range(params.pipeline_len) {
            let in_consensus = consensus_validator_set_handle()
                .at(&epoch)
                .iter(&self.s)
                .unwrap()
                .any(|res| {
                    let (_, val_address) = res.unwrap();
                    val_address == validator.clone()
                });

            let in_bc = below_capacity_validator_set_handle()
                .at(&epoch)
                .iter(&self.s)
                .unwrap()
                .any(|res| {
                    let (_, val_address) = res.unwrap();
                    val_address == validator.clone()
                });
            assert!(!in_consensus && !in_bc);

            let val_state = validator_state_handle(validator)
                .get(&self.s, epoch, params)
                .unwrap();
            assert_eq!(val_state, Some(ValidatorState::Jailed));
        }
        let pipeline_epoch = current_epoch + params.pipeline_len;

        let num_in_consensus = consensus_validator_set_handle()
            .at(&pipeline_epoch)
            .iter(&self.s)
            .unwrap()
            .map(|res| res.unwrap())
            .filter(|(_keys, addr)| addr == validator)
            .count();

        let num_in_bc = below_capacity_validator_set_handle()
            .at(&pipeline_epoch)
            .iter(&self.s)
            .unwrap()
            .map(|res| res.unwrap())
            .filter(|(_keys, addr)| addr == validator)
            .count();

        let num_in_bt = read_below_threshold_validator_set_addresses(
            &self.s,
            pipeline_epoch,
        )
        .unwrap()
        .into_iter()
        .filter(|addr| addr == validator)
        .count();

        let num_occurrences = num_in_consensus + num_in_bc + num_in_bt;
        assert_eq!(num_occurrences, 1);

        let val_state = validator_state_handle(validator)
            .get(&self.s, current_epoch + params.pipeline_len, params)
            .unwrap();
        assert!(
            val_state == Some(ValidatorState::Consensus)
                || val_state == Some(ValidatorState::BelowCapacity)
                || val_state == Some(ValidatorState::BelowThreshold)
        );
    }

    fn check_global_post_conditions(
        &self,
        params: &PosParams,
        current_epoch: Epoch,
        ref_state: &AbstractPosState,
    ) {
        for epoch in Epoch::iter_bounds_inclusive(
            current_epoch,
            current_epoch + params.pipeline_len,
        ) {
            tracing::debug!("Epoch {epoch}");
            let mut vals = HashSet::<Address>::new();

            // Check consistency of all the bonds
            for (id, abs_bonds) in &ref_state.bonds {
                let conc_bonds = crate::bond_handle(&id.source, &id.validator)
                    .get_data_handler()
                    .collect_map(&self.s)
                    .unwrap();

                assert_eq!(abs_bonds.clone(), conc_bonds);
            }

            // Check consistency of outgoing redelegations
            for (src_validator, map1) in &ref_state.outgoing_redelegations {
                for (dest_validator, map2) in map1 {
                    for ((bond_start, redel_start), amount) in map2 {
                        let conc =
                            crate::validator_outgoing_redelegations_handle(
                                src_validator,
                            )
                            .at(dest_validator)
                            .at(bond_start)
                            .get(&self.s, redel_start)
                            .unwrap();
                        assert_eq!(*amount, conc.unwrap_or_default());
                    }
                }
            }

            // Consensus validators
            for WeightedValidator {
                bonded_stake,
                address: validator,
            } in crate::read_consensus_validator_set_addresses_with_stake(
                &self.s, epoch,
            )
            .unwrap()
            {
                let deltas_stake = validator_deltas_handle(&validator)
                    .get_sum(&self.s, epoch, params)
                    .unwrap()
                    .unwrap_or_default();
                tracing::debug!(
                    "Consensus val {}, stake: {} ({})",
                    &validator,
                    bonded_stake.to_string_native(),
                    deltas_stake.to_string_native(),
                );
                assert!(!deltas_stake.is_negative());

                // Checks on stake
                assert_eq!(
                    bonded_stake,
                    token::Amount::from_change(deltas_stake)
                );
                assert_eq!(
                    bonded_stake.change(),
                    ref_state
                        .validator_stakes
                        .get(&epoch)
                        .unwrap()
                        .get(&validator)
                        .cloned()
                        .unwrap()
                );

                // Checks on validator state
                let state = crate::validator_state_handle(&validator)
                    .get(&self.s, epoch, params)
                    .unwrap();
                assert_eq!(state, Some(ValidatorState::Consensus));
                assert_eq!(
                    state.unwrap(),
                    ref_state
                        .validator_states
                        .get(&epoch)
                        .unwrap()
                        .get(&validator)
                        .cloned()
                        .unwrap()
                );

                // Check total_bonded
                let conc_total_bonded = crate::total_bonded_handle(&validator)
                    .get_data_handler()
                    .collect_map(&self.s)
                    .unwrap();
                let abs_total_bonded = ref_state
                    .total_bonded
                    .get(&validator)
                    .cloned()
                    .unwrap_or_default();
                assert_eq!(conc_total_bonded, abs_total_bonded);

                // Check total_unbonded
                let conc_total_unbonded =
                    crate::total_unbonded_handle(&validator)
                        .collect_map(&self.s)
                        .unwrap();
                let abs_total_unbonded = ref_state
                    .total_unbonded
                    .get(&validator)
                    .cloned()
                    .unwrap_or_default();
                assert_eq!(conc_total_unbonded, abs_total_unbonded);

                assert!(!vals.contains(&validator));
                vals.insert(validator);
            }

            // Below-capacity validators
            for WeightedValidator {
                bonded_stake,
                address: validator,
            } in
                crate::read_below_capacity_validator_set_addresses_with_stake(
                    &self.s, epoch,
                )
                .unwrap()
            {
                let deltas_stake = validator_deltas_handle(&validator)
                    .get_sum(&self.s, epoch, params)
                    .unwrap()
                    .unwrap_or_default();
                tracing::debug!(
                    "Below-cap val {}, stake: {} ({})",
                    &validator,
                    bonded_stake.to_string_native(),
                    deltas_stake.to_string_native(),
                );
                assert_eq!(
                    bonded_stake,
                    token::Amount::from_change(deltas_stake)
                );
                assert_eq!(
                    bonded_stake.change(),
                    ref_state
                        .validator_stakes
                        .get(&epoch)
                        .unwrap()
                        .get(&validator)
                        .cloned()
                        .unwrap()
                );

                let state = crate::validator_state_handle(&validator)
                    .get(&self.s, epoch, params)
                    .unwrap();
                // if state.is_none() {
                //     dbg!(
                //         crate::validator_state_handle(&validator)
                //             .get(&self.s, current_epoch, params)
                //             .unwrap()
                //     );
                //     dbg!(
                //         crate::validator_state_handle(&validator)
                //             .get(&self.s, current_epoch.next(), params)
                //             .unwrap()
                //     );
                //     dbg!(
                //         crate::validator_state_handle(&validator)
                //             .get(&self.s, current_epoch.next(), params)
                //             .unwrap()
                //     );
                // }
                assert_eq!(state, Some(ValidatorState::BelowCapacity));
                assert_eq!(
                    state.unwrap(),
                    ref_state
                        .validator_states
                        .get(&epoch)
                        .unwrap()
                        .get(&validator)
                        .cloned()
                        .unwrap()
                );

                // Check total_bonded
                let conc_total_bonded = crate::total_bonded_handle(&validator)
                    .get_data_handler()
                    .collect_map(&self.s)
                    .unwrap();
                let abs_total_bonded = ref_state
                    .total_bonded
                    .get(&validator)
                    .cloned()
                    .unwrap_or_default();
                assert_eq!(conc_total_bonded, abs_total_bonded);

                // Check total_unbonded
                let conc_total_unbonded =
                    crate::total_unbonded_handle(&validator)
                        .collect_map(&self.s)
                        .unwrap();
                let abs_total_unbonded = ref_state
                    .total_unbonded
                    .get(&validator)
                    .cloned()
                    .unwrap_or_default();
                assert_eq!(conc_total_unbonded, abs_total_unbonded);

                assert!(!vals.contains(&validator));
                vals.insert(validator);
            }

            for validator in
                crate::read_below_threshold_validator_set_addresses(
                    &self.s, epoch,
                )
                .unwrap()
            {
                let stake = validator_deltas_handle(&validator)
                    .get_sum(&self.s, epoch, params)
                    .unwrap()
                    .unwrap_or_default();
                tracing::debug!(
                    "Below-thresh val {}, stake {}",
                    &validator,
                    stake.to_string_native(),
                );

                let state = crate::validator_state_handle(&validator)
                    .get(&self.s, epoch, params)
                    .unwrap()
                    .unwrap();

                assert_eq!(state, ValidatorState::BelowThreshold);
                assert_eq!(
                    state,
                    ref_state
                        .validator_states
                        .get(&epoch)
                        .unwrap()
                        .get(&validator)
                        .cloned()
                        .unwrap()
                );
                assert_eq!(
                    stake,
                    ref_state
                        .validator_stakes
                        .get(&epoch)
                        .unwrap()
                        .get(&validator)
                        .cloned()
                        .unwrap()
                );

                // Check total_bonded
                let conc_total_bonded = crate::total_bonded_handle(&validator)
                    .get_data_handler()
                    .collect_map(&self.s)
                    .unwrap();
                let abs_total_bonded = ref_state
                    .total_bonded
                    .get(&validator)
                    .cloned()
                    .unwrap_or_default();
                assert_eq!(conc_total_bonded, abs_total_bonded);

                // Check total_unbonded
                let conc_total_unbonded =
                    crate::total_unbonded_handle(&validator)
                        .collect_map(&self.s)
                        .unwrap();
                let abs_total_unbonded = ref_state
                    .total_unbonded
                    .get(&validator)
                    .cloned()
                    .unwrap_or_default();
                assert_eq!(conc_total_unbonded, abs_total_unbonded);

                assert!(!vals.contains(&validator));
                vals.insert(validator);
            }

            // Jailed validators not in a set
            let all_validators =
                crate::read_all_validator_addresses(&self.s, epoch).unwrap();

            for validator in all_validators {
                let state = validator_state_handle(&validator)
                    .get(&self.s, epoch, params)
                    .unwrap()
                    .unwrap();

                if state == ValidatorState::Jailed {
                    assert_eq!(
                        state,
                        ref_state
                            .validator_states
                            .get(&epoch)
                            .unwrap()
                            .get(&validator)
                            .cloned()
                            .unwrap()
                    );
                    let stake = validator_deltas_handle(&validator)
                        .get_sum(&self.s, epoch, params)
                        .unwrap()
                        .unwrap_or_default();
                    tracing::debug!(
                        "Jailed val {}, stake {}",
                        &validator,
                        stake.to_string_native()
                    );

                    assert_eq!(
                        state,
                        ref_state
                            .validator_states
                            .get(&epoch)
                            .unwrap()
                            .get(&validator)
                            .cloned()
                            .unwrap()
                    );
                    assert_eq!(
                        stake,
                        ref_state
                            .validator_stakes
                            .get(&epoch)
                            .unwrap()
                            .get(&validator)
                            .cloned()
                            .unwrap()
                    );

                    // Check total_bonded
                    let conc_total_bonded =
                        crate::total_bonded_handle(&validator)
                            .get_data_handler()
                            .collect_map(&self.s)
                            .unwrap();
                    let abs_total_bonded = ref_state
                        .total_bonded
                        .get(&validator)
                        .cloned()
                        .unwrap_or_default();
                    assert_eq!(conc_total_bonded, abs_total_bonded);

                    // Check total_unbonded
                    let conc_total_unbonded =
                        crate::total_unbonded_handle(&validator)
                            .collect_map(&self.s)
                            .unwrap();
                    let abs_total_unbonded = ref_state
                        .total_unbonded
                        .get(&validator)
                        .cloned()
                        .unwrap_or_default();
                    assert_eq!(conc_total_unbonded, abs_total_unbonded);

                    assert!(!vals.contains(&validator));
                }
            }
        }
        // TODO: expand this to include jailed validators
    }
}

impl ReferenceStateMachine for AbstractPosState {
    type State = Self;
    type Transition = Transition;

    fn init_state() -> BoxedStrategy<Self::State> {
        tracing::debug!("\nInitializing abstract state machine");
        arb_params_and_genesis_validators(Some(8), 8..10)
            .prop_map(|(params, genesis_validators)| {
                let epoch = Epoch::default();
                let mut state = Self {
                    epoch,
                    params,
                    genesis_validators: genesis_validators
                        .into_iter()
                        // Sorted by stake to fill in the consensus set first
                        .sorted_by(|a, b| Ord::cmp(&a.tokens, &b.tokens))
                        .rev()
                        .collect(),
                    bonds: Default::default(),
                    total_bonded: Default::default(),
                    unbonds: Default::default(),
                    validator_stakes: Default::default(),
                    consensus_set: Default::default(),
                    below_capacity_set: Default::default(),
                    below_threshold_set: Default::default(),
                    validator_states: Default::default(),
                    validator_slashes: Default::default(),
                    enqueued_slashes: Default::default(),
                    validator_last_slash_epochs: Default::default(),
                    total_unbonded: Default::default(),
                    delegator_redelegated_bonded: Default::default(),
                    delegator_redelegated_unbonded: Default::default(),
                    validator_total_redelegated_bonded: Default::default(),
                    validator_total_redelegated_unbonded: Default::default(),
                    incoming_redelegations: Default::default(),
                    outgoing_redelegations: Default::default(),
                };

                for GenesisValidator {
                    address,
                    tokens,
                    consensus_key: _,
                    eth_cold_key: _,
                    eth_hot_key: _,
                    commission_rate: _,
                    max_commission_rate_change: _,
                } in state.genesis_validators.clone()
                {
                    let bonds = state
                        .bonds
                        .entry(BondId {
                            source: address.clone(),
                            validator: address.clone(),
                        })
                        .or_default();
                    bonds.insert(epoch, token::Change::from(tokens));

                    let total_bonded =
                        state.total_bonded.entry(address.clone()).or_default();
                    total_bonded.insert(epoch, tokens.change());

                    let total_stakes =
                        state.validator_stakes.entry(epoch).or_default();
                    total_stakes
                        .insert(address.clone(), token::Change::from(tokens));

                    let consensus_set =
                        state.consensus_set.entry(epoch).or_default();
                    let consensus_vals_len = consensus_set
                        .iter()
                        .map(|(_stake, validators)| validators.len() as u64)
                        .sum();

                    if tokens < state.params.validator_stake_threshold {
                        state
                            .below_threshold_set
                            .entry(epoch)
                            .or_default()
                            .insert(address.clone());
                        state
                            .validator_states
                            .entry(epoch)
                            .or_default()
                            .insert(address, ValidatorState::BelowThreshold);
                    } else if state.params.max_validator_slots
                        > consensus_vals_len
                    {
                        state
                            .validator_states
                            .entry(epoch)
                            .or_default()
                            .insert(address.clone(), ValidatorState::Consensus);
                        consensus_set
                            .entry(tokens)
                            .or_default()
                            .push_back(address);
                    } else {
                        state
                            .validator_states
                            .entry(epoch)
                            .or_default()
                            .insert(
                                address.clone(),
                                ValidatorState::BelowCapacity,
                            );
                        let below_cap_set =
                            state.below_capacity_set.entry(epoch).or_default();
                        below_cap_set
                            .entry(ReverseOrdTokenAmount(tokens))
                            .or_default()
                            .push_back(address)
                    };
                }
                // Ensure that below-capacity and below-threshold sets are
                // initialized even if empty
                state.below_capacity_set.entry(epoch).or_default();
                state.below_threshold_set.entry(epoch).or_default();

                // Copy validator sets up to pipeline epoch
                for epoch in epoch.next().iter_range(state.params.pipeline_len)
                {
                    state.copy_discrete_epoched_data(epoch)
                }
                // dbg!(&state);
                state
            })
            .boxed()
    }

    // TODO: allow bonding to jailed val
    fn transitions(state: &Self::State) -> BoxedStrategy<Self::Transition> {
        // Let preconditions filter out what unbonds are not allowed
        let unbondable = state.bond_sums().into_iter().collect::<Vec<_>>();
        let redelegatable = unbondable
            .iter()
            // Self-bonds cannot be redelegated
            .filter(|(id, _)| id.source != id.validator)
            .cloned()
            .collect::<Vec<_>>();

        for (id, amt) in &redelegatable {
            if *amt <= 0.into() {
                tracing::debug!(
                    "Source: {}\nValidator: {}\nAmount: {}",
                    &id.source,
                    &id.validator,
                    amt.to_string_native()
                );
                panic!("Should have no bonds with 0 amount or less!");
            }
        }

        let withdrawable =
            state.withdrawable_unbonds().into_iter().collect::<Vec<_>>();

        let eligible_for_unjail = state
            .validator_states
            .get(&state.pipeline())
            .unwrap()
            .iter()
            .filter_map(|(addr, &val_state)| {
                let last_slash_epoch =
                    state.validator_last_slash_epochs.get(addr);

                if let Some(last_slash_epoch) = last_slash_epoch {
                    if val_state == ValidatorState::Jailed
                        // `last_slash_epoch` must be unbonding_len + window_width or more epochs
                        // before the current
                        && state.epoch.0 - last_slash_epoch.0
                            > state.params.unbonding_len + state.params.cubic_slashing_window_length
                    {
                        return Some(addr.clone());
                    }
                }
                None
            })
            .collect::<Vec<_>>();

        // Transitions that can be applied if there are no bonds and unbonds
        let basic = prop_oneof![
            4 => Just(Transition::NextEpoch),
            6 => add_arb_bond_amount(state),
            5 => arb_delegation(state),
            3 => arb_self_bond(state),
            1 => (
                address::testing::arb_established_address(),
                key::testing::arb_common_keypair(),
                key::testing::arb_common_secp256k1_keypair(),
                key::testing::arb_common_secp256k1_keypair(),
                arb_rate(),
                arb_rate(),
            )
                .prop_map(
                    |(
                        addr,
                        consensus_key,
                        eth_hot_key,
                        eth_cold_key,
                        commission_rate,
                        max_commission_rate_change,
                    )| {
                        Transition::InitValidator {
                            address: Address::Established(addr),
                            consensus_key: consensus_key.to_public(),
                            eth_hot_key: eth_hot_key.to_public(),
                            eth_cold_key: eth_cold_key.to_public(),
                            commission_rate,
                            max_commission_rate_change,
                        }
                    },
                ),
            1 => arb_slash(state),
        ];

        // Add unjailing, if any eligible
        let transitions = if eligible_for_unjail.is_empty() {
            basic.boxed()
        } else {
            prop_oneof![
                basic,
                prop::sample::select(eligible_for_unjail).prop_map(|address| {
                    Transition::UnjailValidator { address }
                })
            ]
            .boxed()
        };

        // Add unbonds, if any
        let transitions = if unbondable.is_empty() {
            transitions
        } else {
            let arb_unbondable = prop::sample::select(unbondable);
            let arb_unbond =
                arb_unbondable.prop_flat_map(move |(id, deltas_sum)| {
                    let deltas_sum = i128::try_from(deltas_sum).unwrap();
                    // Generate an amount to unbond, up to the sum
                    assert!(
                        deltas_sum > 0,
                        "Bond {id} deltas_sum must be non-zero"
                    );
                    (0..deltas_sum).prop_map(move |to_unbond| {
                        let id = id.clone();
                        let amount =
                            token::Amount::from_change(Change::from(to_unbond));
                        Transition::Unbond { id, amount }
                    })
                });
            prop_oneof![transitions, arb_unbond].boxed()
        };

        // Add withdrawals, if any
        let transitions = if withdrawable.is_empty() {
            transitions
        } else {
            let arb_withdrawable = prop::sample::select(withdrawable);
            let arb_withdrawal = arb_withdrawable
                .prop_map(|(id, _)| Transition::Withdraw { id });

            prop_oneof![transitions, arb_withdrawal].boxed()
        };

        // Add redelegations, if any
        if redelegatable.is_empty() {
            transitions
        } else {
            let arb_redelegatable = prop::sample::select(redelegatable);
            let validators = state
                .validator_states
                .get(&state.pipeline())
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            let epoch = state.epoch;
            let params = state.params.clone();
            let incoming_redelegations = state.incoming_redelegations.clone();
            let arb_redelegation =
                arb_redelegatable.prop_flat_map(move |(id, deltas_sum)| {
                    let deltas_sum = i128::try_from(deltas_sum).unwrap();
                    // Generate an amount to redelegate, up to the sum
                    assert!(
                        deltas_sum > 0,
                        "Bond {id} deltas_sum must be non-zero"
                    );
                    let arb_amount = (0..deltas_sum).prop_map(|to_unbond| {
                        token::Amount::from_change(Change::from(to_unbond))
                    });
                    // Generate a new validator for redelegation
                    let current_validator = id.validator.clone();
                    let new_validators = validators
                        .iter()
                        // The validator must be other than the current
                        .filter(|validator| *validator != &current_validator)
                        .cloned()
                        .collect::<Vec<_>>();
                    let arb_new_validator =
                        prop::sample::select(new_validators);
                    let params = params.clone();
                    let incoming_redelegations = incoming_redelegations.clone();
                    (arb_amount, arb_new_validator).prop_map(
                        move |(amount, new_validator)| Transition::Redelegate {
                            is_chained: Self::is_chained_redelegation(
                                epoch,
                                &params,
                                &incoming_redelegations,
                                &id.source,
                                &id.validator,
                            ),
                            id: id.clone(),
                            new_validator,
                            amount,
                        },
                    )
                });
            prop_oneof![transitions, arb_redelegation].boxed()
        }
    }

    fn apply(
        mut state: Self::State,
        transition: &Self::Transition,
    ) -> Self::State {
        match transition {
            Transition::NextEpoch => {
                tracing::debug!("\nABSTRACT Next Epoch");

                state.epoch = state.epoch.next();

                // Copy the non-delta data into pipeline epoch from its pred.
                state.copy_discrete_epoched_data(state.pipeline());

                // Process slashes enqueued for the new epoch
                state.process_enqueued_slashes();

                // print-out the state
                state.debug_validators();
            }
            Transition::InitValidator {
                address,
                consensus_key: _,
                eth_cold_key: _,
                eth_hot_key: _,
                commission_rate: _,
                max_commission_rate_change: _,
            } => {
                tracing::debug!(
                    "\nABSTRACT Init Validator {} in epoch {}",
                    address,
                    state.epoch
                );
                let pipeline: Epoch = state.pipeline();

                // Initialize the stake at pipeline
                state
                    .validator_stakes
                    .entry(pipeline)
                    .or_default()
                    .insert(address.clone(), 0_i128.into());

                // Insert into the below-threshold set at pipeline since the
                // initial stake is 0
                state
                    .below_threshold_set
                    .entry(pipeline)
                    .or_default()
                    .insert(address.clone());
                state
                    .validator_states
                    .entry(pipeline)
                    .or_default()
                    .insert(address.clone(), ValidatorState::BelowThreshold);

                state.debug_validators();
            }
            Transition::Bond { id, amount } => {
                tracing::debug!(
                    "\nABSTRACT Bond {} tokens, id = {}",
                    amount.to_string_native(),
                    id
                );

                if !amount.is_zero() {
                    let change = token::Change::from(*amount);
                    let pipeline_state = state
                        .validator_states
                        .get(&state.pipeline())
                        .unwrap()
                        .get(&id.validator)
                        .unwrap();

                    // Validator sets need to be updated first!!
                    if *pipeline_state != ValidatorState::Jailed {
                        state.update_validator_sets(
                            state.pipeline(),
                            &id.validator,
                            change,
                        );
                    }
                    state.update_bond(id, change);
                    state.update_validator_total_stake(&id.validator, change);
                }
                state.debug_validators();
            }
            Transition::Unbond { id, amount } => {
                tracing::debug!(
                    "\nABSTRACT Unbond {} tokens, id = {}",
                    amount.to_string_native(),
                    id
                );

                // `totalBonded`
                let sum_bonded = state
                    .bonds
                    .get(id)
                    .map(|a| {
                        a.iter()
                            .fold(token::Change::zero(), |acc, (_, amount)| {
                                acc + *amount
                            })
                    })
                    .unwrap_or_default();

                if !amount.is_zero() && amount.change() <= sum_bonded {
                    let change = token::Change::from(*amount);
                    state.update_state_with_unbond(id, change);
                }
                state.debug_validators();
            }
            Transition::Withdraw { id } => {
                tracing::debug!("\nABSTRACT Withdraw, id = {}", id);

                // Remove all withdrawable unbonds with this bond ID
                for ((_start_epoch, withdraw_epoch), unbonds) in
                    state.unbonds.iter_mut()
                {
                    // let redelegated_unbonds = state
                    //     .delegator_redelegated_unbonded
                    //     .entry(id.source.clone())
                    //     .or_default()
                    //     .entry(id.validator.clone())
                    //     .or_default()
                    //     .entry((*start_epoch, withdraw_epoch*))
                    //     .or_default();
                    if *withdraw_epoch <= state.epoch {
                        unbonds.remove(id);
                    }
                }
                // Remove any epochs that have no unbonds left
                state.unbonds.retain(|_epochs, unbonds| !unbonds.is_empty());

                // TODO: should we do anything here for slashing?
            }
            Transition::Redelegate {
                is_chained,
                id,
                new_validator,
                amount,
            } => {
                tracing::debug!(
                    "\nABSTRACT Redelegation, id = {id}, new validator = \
                     {new_validator}, amount = {}, is_chained = {is_chained}",
                    amount.to_string_native(),
                );
                if *is_chained {
                    return state;
                }
                if !amount.is_zero() {
                    // Remove the amount from source validator
                    let change = token::Change::from(*amount);
                    state.update_state_with_redelegation(
                        id,
                        new_validator,
                        change,
                    );
                }
                state.debug_validators();
            }
            Transition::Misbehavior {
                address,
                slash_type,
                infraction_epoch,
                height,
            } => {
                let current_epoch = state.epoch;
                tracing::debug!(
                    "\nABSTRACT Misbehavior in epoch {} by validator {}, \
                     found in epoch {}",
                    infraction_epoch,
                    address,
                    current_epoch
                );

                let processing_epoch = *infraction_epoch
                    + state.params.unbonding_len
                    + 1_u64
                    + state.params.cubic_slashing_window_length;
                let slash = Slash {
                    epoch: *infraction_epoch,
                    block_height: *height,
                    r#type: *slash_type,
                    rate: Dec::zero(),
                };

                // Enqueue the slash for future processing
                state
                    .enqueued_slashes
                    .entry(processing_epoch)
                    .or_default()
                    .entry(address.clone())
                    .or_default()
                    .push(slash);

                // Remove the validator from either the consensus or
                // below-capacity set and place it into the jailed validator set

                // Remove from the validator set starting at the next epoch and
                // up thru the pipeline
                for offset in 1..=state.params.pipeline_len {
                    let real_stake = token::Amount::from_change(
                        state
                            .validator_stakes
                            .get(&(current_epoch + offset))
                            .unwrap()
                            .get(address)
                            .cloned()
                            .unwrap_or_default(),
                    );

                    if let Some((index, stake)) = state
                        .is_in_consensus_w_info(address, current_epoch + offset)
                    {
                        debug_assert_eq!(stake, real_stake);

                        let vals = state
                            .consensus_set
                            .entry(current_epoch + offset)
                            .or_default()
                            .entry(stake)
                            .or_default();
                        let removed = vals.remove(index);
                        debug_assert_eq!(removed, Some(address.clone()));
                        if vals.is_empty() {
                            state
                                .consensus_set
                                .entry(current_epoch + offset)
                                .or_default()
                                .remove(&stake);
                        }

                        // At pipeline epoch, if was consensus, replace it with
                        // a below-capacity validator
                        if offset == state.params.pipeline_len {
                            let below_cap_pipeline = state
                                .below_capacity_set
                                .entry(current_epoch + offset)
                                .or_default();

                            if let Some(mut max_below_cap) =
                                below_cap_pipeline.last_entry()
                            {
                                let max_bc_stake = *max_below_cap.key();
                                let vals = max_below_cap.get_mut();
                                let first_val = vals.pop_front().unwrap();
                                if vals.is_empty() {
                                    below_cap_pipeline.remove(&max_bc_stake);
                                }
                                state
                                    .consensus_set
                                    .entry(current_epoch + offset)
                                    .or_default()
                                    .entry(max_bc_stake.into())
                                    .or_default()
                                    .push_back(first_val.clone());
                                state
                                    .validator_states
                                    .entry(current_epoch + offset)
                                    .or_default()
                                    .insert(
                                        first_val.clone(),
                                        ValidatorState::Consensus,
                                    );
                            }
                        }
                    } else if let Some((index, stake)) = state
                        .is_in_below_capacity_w_info(
                            address,
                            current_epoch + offset,
                        )
                    {
                        debug_assert_eq!(stake, real_stake);

                        let vals = state
                            .below_capacity_set
                            .entry(current_epoch + offset)
                            .or_default()
                            .entry(stake.into())
                            .or_default();

                        let removed = vals.remove(index);
                        debug_assert_eq!(removed, Some(address.clone()));
                        if vals.is_empty() {
                            state
                                .below_capacity_set
                                .entry(current_epoch + offset)
                                .or_default()
                                .remove(&stake.into());
                        }
                    } else if state
                        .is_in_below_threshold(address, current_epoch + offset)
                    {
                        let removed = state
                            .below_threshold_set
                            .entry(current_epoch + offset)
                            .or_default()
                            .remove(address);
                        debug_assert!(removed);
                    } else {
                        // Just make sure the validator is already jailed
                        debug_assert_eq!(
                            state
                                .validator_states
                                .get(&(current_epoch + offset))
                                .unwrap()
                                .get(address)
                                .cloned()
                                .unwrap(),
                            ValidatorState::Jailed
                        );
                    }

                    state
                        .validator_states
                        .entry(current_epoch + offset)
                        .or_default()
                        .insert(address.clone(), ValidatorState::Jailed);
                }

                // Update the most recent infraction epoch for the validator
                if let Some(last_epoch) =
                    state.validator_last_slash_epochs.get(address)
                {
                    if infraction_epoch > last_epoch {
                        state
                            .validator_last_slash_epochs
                            .insert(address.clone(), *infraction_epoch);
                    }
                } else {
                    state
                        .validator_last_slash_epochs
                        .insert(address.clone(), *infraction_epoch);
                }

                state.debug_validators();
            }
            Transition::UnjailValidator { address } => {
                let pipeline_epoch = state.pipeline();

                tracing::debug!(
                    "\nABSTRACT Unjail validator {} starting in epoch {}",
                    address.clone(),
                    pipeline_epoch
                );

                let consensus_set_pipeline =
                    state.consensus_set.entry(pipeline_epoch).or_default();
                let pipeline_stake = state
                    .validator_stakes
                    .get(&pipeline_epoch)
                    .unwrap()
                    .get(address)
                    .cloned()
                    .unwrap_or_default();
                let validator_states_pipeline =
                    state.validator_states.entry(pipeline_epoch).or_default();

                // Insert the validator back into the appropriate validator set
                // and update its state
                let num_consensus = consensus_set_pipeline
                    .iter()
                    .fold(0, |sum, (_, validators)| {
                        sum + validators.len() as u64
                    });

                if pipeline_stake
                    < state.params.validator_stake_threshold.change()
                {
                    // Place into the below-threshold set
                    let below_threshold_set_pipeline = state
                        .below_threshold_set
                        .entry(pipeline_epoch)
                        .or_default();
                    below_threshold_set_pipeline.insert(address.clone());
                    validator_states_pipeline.insert(
                        address.clone(),
                        ValidatorState::BelowThreshold,
                    );
                } else if num_consensus < state.params.max_validator_slots {
                    // Place directly into the consensus set
                    debug_assert!(
                        state
                            .below_capacity_set
                            .get(&pipeline_epoch)
                            .unwrap()
                            .is_empty()
                    );
                    consensus_set_pipeline
                        .entry(token::Amount::from_change(pipeline_stake))
                        .or_default()
                        .push_back(address.clone());
                    validator_states_pipeline
                        .insert(address.clone(), ValidatorState::Consensus);
                } else if let Some(mut min_consensus) =
                    consensus_set_pipeline.first_entry()
                {
                    let below_capacity_set_pipeline = state
                        .below_capacity_set
                        .entry(pipeline_epoch)
                        .or_default();

                    let min_consensus_stake = *min_consensus.key();
                    if pipeline_stake > min_consensus_stake.change() {
                        // Place into the consensus set and demote the last
                        // min_consensus validator
                        let min_validators = min_consensus.get_mut();
                        let last_val = min_validators.pop_back().unwrap();
                        // Remove the key if there's nothing left
                        if min_validators.is_empty() {
                            consensus_set_pipeline.remove(&min_consensus_stake);
                        }
                        // Do the swap
                        below_capacity_set_pipeline
                            .entry(min_consensus_stake.into())
                            .or_default()
                            .push_back(last_val.clone());
                        validator_states_pipeline
                            .insert(last_val, ValidatorState::BelowCapacity);

                        consensus_set_pipeline
                            .entry(token::Amount::from_change(pipeline_stake))
                            .or_default()
                            .push_back(address.clone());
                        validator_states_pipeline
                            .insert(address.clone(), ValidatorState::Consensus);
                    } else {
                        // Just place into the below-capacity set
                        below_capacity_set_pipeline
                            .entry(
                                token::Amount::from_change(pipeline_stake)
                                    .into(),
                            )
                            .or_default()
                            .push_back(address.clone());
                        validator_states_pipeline.insert(
                            address.clone(),
                            ValidatorState::BelowCapacity,
                        );
                    }
                } else {
                    panic!("Should not reach here I don't think")
                }
                state.debug_validators();
            }
        }
        state
    }

    fn preconditions(
        state: &Self::State,
        transition: &Self::Transition,
    ) -> bool {
        match transition {
            // TODO: should there be any slashing preconditions for `NextEpoch`?
            Transition::NextEpoch => true,
            Transition::InitValidator {
                address,
                consensus_key: _,
                eth_cold_key: _,
                eth_hot_key: _,
                commission_rate: _,
                max_commission_rate_change: _,
            } => {
                let pipeline = state.pipeline();
                // The address must not belong to an existing validator
                !state.is_validator(address, pipeline) &&
                   // There must be no delegations from this address
                   !state.bond_sums().into_iter().any(|(id, _sum)|
                        &id.source == address)
            }
            Transition::Bond { id, amount: _ } => {
                let pipeline = state.pipeline();
                // The validator must be known
                if !state.is_validator(&id.validator, pipeline) {
                    return false;
                }

                id.validator == id.source
                        // If it's not a self-bond, the source must not be a validator
                        || !state.is_validator(&id.source, pipeline)
            }
            Transition::Unbond { id, amount } => {
                let pipeline = state.pipeline();

                let is_unbondable = state
                    .bond_sums()
                    .get(id)
                    .map(|sum| *sum >= token::Change::from(*amount))
                    .unwrap_or_default();

                // The validator must not be frozen currently
                let is_frozen = if let Some(last_epoch) =
                    state.validator_last_slash_epochs.get(&id.validator)
                {
                    *last_epoch
                        + state.params.unbonding_len
                        + 1u64
                        + state.params.cubic_slashing_window_length
                        > state.epoch
                } else {
                    false
                };

                // if is_frozen {
                //     tracing::debug!(
                //         "\nVALIDATOR {} IS FROZEN - CANNOT UNBOND\n",
                //         &id.validator
                //     );
                // }

                // The validator must be known
                state.is_validator(&id.validator, pipeline)
                    // The amount must be available to unbond and the validator not jailed
                    && is_unbondable && !is_frozen
            }
            Transition::Withdraw { id } => {
                let pipeline = state.pipeline();

                let is_withdrawable = state
                    .withdrawable_unbonds()
                    .get(id)
                    .map(|amount| *amount >= token::Amount::zero())
                    .unwrap_or_default();

                // The validator must not be jailed currently
                let is_jailed = state
                    .validator_states
                    .get(&state.epoch)
                    .unwrap()
                    .get(&id.validator)
                    .cloned()
                    == Some(ValidatorState::Jailed);

                // The validator must be known
                state.is_validator(&id.validator, pipeline)
                    // The amount must be available to unbond
                    && is_withdrawable && !is_jailed
            }
            Transition::Redelegate {
                is_chained,
                id,
                new_validator,
                amount,
            } => {
                let pipeline = state.pipeline();

                // dbg!(is_chained);
                if *is_chained {
                    Self::is_chained_redelegation(
                        state.epoch,
                        &state.params,
                        &state.incoming_redelegations,
                        &id.source,
                        new_validator,
                    )
                } else {
                    // The src and dest validator must be known
                    if !state.is_validator(&id.validator, pipeline)
                        || !state.is_validator(new_validator, pipeline)
                    {
                        return false;
                    }

                    // The amount must be available to redelegate
                    if !state
                        .bond_sums()
                        .get(id)
                        .map(|sum| *sum >= token::Change::from(*amount))
                        .unwrap_or_default()
                    {
                        return false;
                    }

                    // The src validator must not be frozen
                    if let Some(last_epoch) =
                        state.validator_last_slash_epochs.get(&id.validator)
                    {
                        if *last_epoch
                            + state.params.unbonding_len
                            + 1u64
                            + state.params.cubic_slashing_window_length
                            > state.epoch
                        {
                            return false;
                        }
                    }

                    // The dest validator must not be frozen
                    if let Some(last_epoch) =
                        state.validator_last_slash_epochs.get(new_validator)
                    {
                        if *last_epoch
                            + state.params.unbonding_len
                            + 1u64
                            + state.params.cubic_slashing_window_length
                            > state.epoch
                        {
                            return false;
                        }
                    }

                    true
                }
            }
            Transition::Misbehavior {
                address,
                slash_type: _,
                infraction_epoch,
                height: _,
            } => {
                let is_validator =
                    state.is_validator(address, *infraction_epoch);

                // The infraction epoch cannot be in the future or more than
                // unbonding_len epochs in the past
                let current_epoch = state.epoch;
                let valid_epoch = *infraction_epoch <= current_epoch
                    && current_epoch.0 - infraction_epoch.0
                        <= state.params.unbonding_len;

                // Only misbehave when there is more than 3 validators that's
                // not jailed or about to be slashed, so there's always at least
                // one honest left
                let enough_honest_validators = || {
                    let num_of_honest = state
                        .validator_states
                        .get(&state.pipeline())
                        .unwrap()
                        .iter()
                        .filter(|(_addr, val_state)| match val_state {
                            ValidatorState::Consensus
                            | ValidatorState::BelowCapacity => true,
                            ValidatorState::Inactive
                            | ValidatorState::Jailed
                            // Below threshold cannot be in consensus
                            | ValidatorState::BelowThreshold => false,
                        })
                        .count();

                    // Find the number of enqueued slashes to unique validators
                    let num_of_enquequed_slashes = state
                        .enqueued_slashes
                        .iter()
                        // find all validators with any enqueued slashes
                        .fold(BTreeSet::new(), |mut acc, (&epoch, slashes)| {
                            if epoch > current_epoch {
                                acc.extend(slashes.keys().cloned());
                            }
                            acc
                        })
                        .len();

                    num_of_honest - num_of_enquequed_slashes > 3
                };

                // Ensure that the validator is in consensus when it misbehaves
                // TODO: possibly also test allowing below-capacity validators
                // tracing::debug!("\nVal to possibly misbehave: {}", &address);
                let state_at_infraction = state
                    .validator_states
                    .get(infraction_epoch)
                    .unwrap()
                    .get(address);
                if state_at_infraction.is_none() {
                    // Figure out why this happening
                    tracing::debug!(
                        "State is None at Infraction epoch {}",
                        infraction_epoch
                    );
                    for epoch in Epoch::iter_bounds_inclusive(
                        infraction_epoch.next(),
                        state.epoch,
                    ) {
                        let state_ep = state
                            .validator_states
                            .get(infraction_epoch)
                            .unwrap()
                            .get(address)
                            .cloned();
                        tracing::debug!(
                            "State at epoch {} is {:?}",
                            epoch,
                            state_ep
                        );
                    }
                }

                let can_misbehave = state_at_infraction.cloned()
                    == Some(ValidatorState::Consensus);

                is_validator
                    && valid_epoch
                    && enough_honest_validators()
                    && can_misbehave

                // TODO: any others conditions?
            }
            Transition::UnjailValidator { address } => {
                // Validator address must be jailed thru the pipeline epoch
                for epoch in
                    Epoch::iter_bounds_inclusive(state.epoch, state.pipeline())
                {
                    if state
                        .validator_states
                        .get(&epoch)
                        .unwrap()
                        .get(address)
                        .cloned()
                        .unwrap()
                        != ValidatorState::Jailed
                    {
                        return false;
                    }
                }
                // Most recent misbehavior is >= unbonding_len epochs away from
                // current epoch
                if let Some(last_slash_epoch) =
                    state.validator_last_slash_epochs.get(address)
                {
                    if state.epoch.0 - last_slash_epoch.0
                        < state.params.unbonding_len
                    {
                        return false;
                    }
                }

                true
                // TODO: any others?
            }
        }
    }
}

impl AbstractPosState {
    /// Copy validator sets and validator states at the given epoch from its
    /// predecessor
    fn copy_discrete_epoched_data(&mut self, epoch: Epoch) {
        let prev_epoch = epoch.prev();
        // Copy the non-delta data from the last epoch into the new one
        self.consensus_set.insert(
            epoch,
            self.consensus_set.get(&prev_epoch).unwrap().clone(),
        );
        self.below_capacity_set.insert(
            epoch,
            self.below_capacity_set.get(&prev_epoch).unwrap().clone(),
        );
        self.below_threshold_set.insert(
            epoch,
            self.below_threshold_set.get(&prev_epoch).unwrap().clone(),
        );
        self.validator_states.insert(
            epoch,
            self.validator_states.get(&prev_epoch).unwrap().clone(),
        );
        self.validator_stakes.insert(
            epoch,
            self.validator_stakes.get(&prev_epoch).unwrap().clone(),
        );
    }

    /// Update a bond with bonded or unbonded change at the pipeline epoch
    fn update_bond(&mut self, id: &BondId, change: token::Change) {
        let pipeline_epoch = self.pipeline();
        let bonds = self.bonds.entry(id.clone()).or_default();
        let bond = bonds.entry(pipeline_epoch).or_default();
        *bond += change;
        // Remove fully unbonded entries
        if bond.is_zero() {
            bonds.remove(&pipeline_epoch);
        }
        // Update total_bonded
        let total_bonded = self
            .total_bonded
            .entry(id.validator.clone())
            .or_default()
            .entry(pipeline_epoch)
            .or_default();
        *total_bonded += change;
    }

    fn update_state_with_unbond(&mut self, id: &BondId, change: token::Change) {
        self.unbond_tokens(id, change, false);

        // let pipeline_epoch = self.pipeline();
        // let withdraw_epoch = pipeline_epoch
        //     + self.params.unbonding_len
        //     + self.params.cubic_slashing_window_length;
        // let bonds = self.bonds.entry(id.clone()).or_default();
        // let total_unbonded = self
        //     .total_unbonded
        //     .entry(id.validator.clone())
        //     .or_default()
        //     .entry(pipeline_epoch)
        //     .or_default();
        // let total_bonded =
        //     self.total_bonded.entry(id.validator.clone()).or_default();

        // let delegator_redelegated_bonds = self
        //     .delegator_redelegated_bonded
        //     .entry(id.source.clone())
        //     .or_default()
        //     .entry(id.validator.clone())
        //     .or_default();
        // let delegator_redelegated_unbonds = self
        //     .delegator_redelegated_unbonded
        //     .entry(id.source.clone())
        //     .or_default()
        //     .entry(id.validator.clone())
        //     .or_default();
        // let validator_total_redelegated_bonded = self
        //     .validator_total_redelegated_bonded
        //     .entry(id.validator.clone())
        //     .or_default();
        // let validator_total_redelegated_unbonded = self
        //     .validator_total_redelegated_unbonded
        //     .entry(id.validator.clone())
        //     .or_default()
        //     .entry(pipeline_epoch)
        //     .or_default();

        // let validator_slashes = &self.validator_slashes;

        // tracing::debug!("Bonds before decrementing");
        // for (start, amnt) in bonds.iter() {
        //     tracing::debug!(
        //         "Bond epoch {} - amnt {}",
        //         start,
        //         amnt.to_string_native()
        //     );
        // }

        // // `resultUnbonding`
        // // Get the bonds for removal
        // let bonds_to_remove = Self::find_bonds_to_remove(bonds, change);

        // // `modifiedRedelegation`
        // // Modified redelegation
        // // The unbond may need to partially unbond redelegated tokens, so
        // // compute if necessary
        // let modified_redelegation = match bonds_to_remove.new_entry {
        //     Some((bond_epoch, new_bond_amount)) => {
        //         // tracing::debug!(
        //         //     "\n New redel entry for epoch {} -> amount {}",
        //         //     bond_epoch, new_bond_amount
        //         // );
        //         let cur_bond_amount =
        //             bonds.get(&bond_epoch).cloned().unwrap_or_default();
        //         if delegator_redelegated_bonds.contains_key(&bond_epoch) {
        //             Self::compute_modified_redelegation(
        //                 delegator_redelegated_bonds,
        //                 bond_epoch,
        //                 cur_bond_amount - new_bond_amount,
        //             )
        //         } else {
        //             ModifiedRedelegation::default()
        //         }
        //     }
        //     None => ModifiedRedelegation::default(),
        // };

        // // `keysUnbonds`
        // // New unbonds. This will be needed for a couple things
        // let unbonded_bond_starts =
        //     if let Some((start_epoch, _)) = bonds_to_remove.new_entry {
        //         let mut to_remove = bonds_to_remove.epochs.clone();
        //         to_remove.insert(start_epoch);
        //         to_remove
        //     } else {
        //         bonds_to_remove.epochs.clone()
        //     };
        // // `newUnbonds`
        // let new_unbonds = unbonded_bond_starts
        //     .into_iter()
        //     .map(|start| {
        //         let cur_bond_amnt = bonds.get(&start).cloned().unwrap();
        //         let new_value = if let Some((start_epoch, new_bond_amount)) =
        //             bonds_to_remove.new_entry
        //         {
        //             if start_epoch == start {
        //                 cur_bond_amnt - new_bond_amount
        //             } else {
        //                 cur_bond_amnt
        //             }
        //         } else {
        //             cur_bond_amnt
        //         };
        //         ((start, withdraw_epoch), new_value)
        //     })
        //     .collect::<BTreeMap<(Epoch, Epoch), token::Change>>();

        // // Update the bonds and unbonds in the AbstractState
        // // `updatedBonded` + updates to `updatedDelegator`
        // for bond_epoch in &bonds_to_remove.epochs {
        //     bonds.remove(bond_epoch);
        // }
        // if let Some((bond_epoch, new_bond_amt)) = bonds_to_remove.new_entry {
        //     bonds.insert(bond_epoch, new_bond_amt);
        // }
        // // `updatedUnbonded` + updates to `updatedDelegator`
        // for (epoch_pair, amount) in &new_unbonds {
        //     let unbonds = self
        //         .unbonds
        //         .entry(*epoch_pair)
        //         .or_default()
        //         .entry(id.clone())
        //         .or_default();
        //     *unbonds += token::Amount::from(*amount)
        // }

        // tracing::debug!("Bonds after decrementing");
        // for (start, amnt) in bonds.iter() {
        //     tracing::debug!(
        //         "Bond epoch {} - amnt {}",
        //         start,
        //         amnt.to_string_native()
        //     );
        // }

        // // Compute new redelegated unbonds (which requires unmodified
        // // redelegated bonds)
        // let new_redelegated_unbonds = Self::compute_new_redelegated_unbonds(
        //     delegator_redelegated_bonds,
        //     &bonds_to_remove.epochs,
        //     &modified_redelegation,
        // );

        // // Update the delegator's redelegated bonds in the state
        // for epoch_to_remove in &bonds_to_remove.epochs {
        //     delegator_redelegated_bonds.remove(epoch_to_remove);
        // }
        // if let Some(epoch) = modified_redelegation.epoch {
        //     if modified_redelegation.validators_to_remove.is_empty() {
        //         delegator_redelegated_bonds.remove(&epoch);
        //     } else {
        //         let rbonds =
        //             delegator_redelegated_bonds.entry(epoch).or_default();

        //         if let Some(val_to_modify) =
        //             &modified_redelegation.validator_to_modify
        //         {
        //             let mut updated_vals_to_remove =
        //                 modified_redelegation.validators_to_remove.clone();
        //             updated_vals_to_remove.remove(val_to_modify);

        //             // Remove the updated_vals_to_remove keys from the
        //             // redelegated_bonds map first
        //             for val in &updated_vals_to_remove {
        //                 rbonds.remove(val);
        //             }

        //             if let Some(epoch_to_modify) =
        //                 modified_redelegation.epoch_to_modify
        //             {
        //                 let mut updated_epochs_to_remove =
        //                     modified_redelegation.epochs_to_remove.clone();
        //                 updated_epochs_to_remove.remove(&epoch_to_modify);
        //                 let val_bonds_to_modify =
        //                     rbonds.entry(val_to_modify.clone()).or_default();
        //                 for epoch in updated_epochs_to_remove {
        //                     val_bonds_to_modify.remove(&epoch);
        //                 }
        //                 val_bonds_to_modify.insert(
        //                     epoch_to_modify,
        //                     modified_redelegation.new_amount.unwrap(),
        //                 );
        //             } else {
        //                 // Then remove to epochs_to_remove from the
        // redelegated                 // bonds of the val_to_modify
        //                 let val_bonds_to_modify =
        //                     rbonds.entry(val_to_modify.clone()).or_default();
        //                 for epoch in &modified_redelegation.epochs_to_remove
        // {                     val_bonds_to_modify.remove(epoch);
        //                 }
        //             }
        //         } else {
        //             // Remove all validators in
        //             // modified_redelegation.validators_to_remove
        //             // from redelegated_bonds
        //             for val in &modified_redelegation.validators_to_remove {
        //                 rbonds.remove(val);
        //             }
        //         }
        //     }
        // }

        // // Update the delegator's redelegated unbonds
        // // ----------------------------

        // // Get all the epoch pairs that should exist in the state now
        // // TODO: may only need the first of these
        // let new_unbond_epoch_pairs = new_redelegated_unbonds
        //     .keys()
        //     .map(|start_epoch| (*start_epoch, withdraw_epoch))
        //     .collect::<HashSet<_>>();

        // let existing_unbond_epoch_pairs = delegator_redelegated_unbonds
        //     .iter()
        //     .map(|(unbond_pair, _)| *unbond_pair)
        //     .collect::<HashSet<_>>();

        // let _all_epoch_pairs = new_unbond_epoch_pairs
        //     .union(&existing_unbond_epoch_pairs)
        //     .cloned()
        //     .collect::<HashSet<_>>();

        // // Update the state for delegator's redelegated unbonds now
        // // NOTE: can maybe do this by only looking at those inside the new
        // epoch // pairs?
        // for unbond_pair in new_unbond_epoch_pairs {
        //     for (src_val, redel_unbonds) in
        //         new_redelegated_unbonds.get(&unbond_pair.0).unwrap()
        //     {
        //         for (src_start, unbonded) in redel_unbonds {
        //             let existing_unbonded = delegator_redelegated_unbonds
        //                 .entry(unbond_pair)
        //                 .or_default()
        //                 .entry(src_val.clone())
        //                 .or_default()
        //                 .entry(*src_start)
        //                 .or_default();
        //             *existing_unbonded += token::Amount::from(*unbonded);
        //         }
        //     }
        // }

        // // Update the validator's total bonded and total unbonded
        // for ((start_epoch, _), unbonded) in &new_unbonds {
        //     let cur_total_bonded =
        //         total_bonded.entry(*start_epoch).or_default();
        //     *cur_total_bonded -= *unbonded;
        //     let cur_total_unbonded =
        //         total_unbonded.entry(*start_epoch).or_default();
        //     *cur_total_unbonded += token::Amount::from(*unbonded);
        // }

        // // Update the validator's total redelegated bonded and unbonded
        // for (dest_start, r_unbonds) in &new_redelegated_unbonds {
        //     for (src_val, changes) in r_unbonds {
        //         for (bond_start, change) in changes {
        //             let cur_total_bonded = validator_total_redelegated_bonded
        //                 .entry(*dest_start)
        //                 .or_default()
        //                 .entry(src_val.clone())
        //                 .or_default()
        //                 .entry(*bond_start)
        //                 .or_default();
        //             *cur_total_bonded -= token::Amount::from(*change);

        //             let cur_total_unbonded =
        //                 validator_total_redelegated_unbonded
        //                     .entry(*dest_start)
        //                     .or_default()
        //                     .entry(src_val.clone())
        //                     .or_default()
        //                     .entry(*bond_start)
        //                     .or_default();
        //             *cur_total_unbonded += token::Amount::from(*change);
        //         }
        //     }
        // }

        // // Get the slashed amount of the unbond now
        // let result_slashing = Self::compute_amount_after_slashing_unbond(
        //     &self.params,
        //     validator_slashes,
        //     &id.validator,
        //     &new_unbonds,
        //     &new_redelegated_unbonds,
        // );

        // let amount_after_slashing = result_slashing.sum.change();

        // let pipeline_state = self
        //     .validator_states
        //     .get(&self.pipeline())
        //     .unwrap()
        //     .get(&id.validator)
        //     .unwrap();
        // // let pipeline_stake = self
        // //     .validator_stakes
        // //     .get(&self.pipeline())
        // //     .unwrap()
        // //     .get(&id.validator)
        // //     .unwrap();
        // // let token_change = cmp::min(*pipeline_stake,
        // amount_after_slashing);

        // if *pipeline_state != ValidatorState::Jailed {
        //     self.update_validator_sets(
        //         self.pipeline(),
        //         &id.validator,
        //         -amount_after_slashing,
        //     );
        // }
        // self.update_validator_total_stake(
        //     &id.validator,
        //     -amount_after_slashing,
        // );
    }

    fn unbond_tokens(
        &mut self,
        id: &BondId,
        change: token::Change,
        is_redelegation: bool,
    ) -> ResultSlashing {
        // TODO: check in here too that the amount is less or equal to bond sum

        let pipeline_epoch = self.pipeline();
        let withdraw_epoch = pipeline_epoch
            + self.params.unbonding_len
            + self.params.cubic_slashing_window_length;

        let bonds = self.bonds.entry(id.clone()).or_default();

        let total_bonded =
            self.total_bonded.entry(id.validator.clone()).or_default();
        let total_unbonded = self
            .total_unbonded
            .entry(id.validator.clone())
            .or_default()
            .entry(pipeline_epoch)
            .or_default();

        let delegator_redelegated_bonds = self
            .delegator_redelegated_bonded
            .entry(id.source.clone())
            .or_default()
            .entry(id.validator.clone())
            .or_default();
        let delegator_redelegated_unbonds = self
            .delegator_redelegated_unbonded
            .entry(id.source.clone())
            .or_default()
            .entry(id.validator.clone())
            .or_default();

        let validator_total_redelegated_bonded = self
            .validator_total_redelegated_bonded
            .entry(id.validator.clone())
            .or_default();
        let validator_total_redelegated_unbonded = self
            .validator_total_redelegated_unbonded
            .entry(id.validator.clone())
            .or_default()
            .entry(pipeline_epoch)
            .or_default();

        let validator_slashes = &self.validator_slashes;

        tracing::debug!("Bonds before decrementing");
        for (start, amnt) in bonds.iter() {
            tracing::debug!(
                "Bond epoch {} - amnt {}",
                start,
                amnt.to_string_native()
            );
        }

        // `resultUnbonding`
        // Get the bonds for removal
        let bonds_to_remove = Self::find_bonds_to_remove(bonds, change);

        // `modifiedRedelegation`
        // Modified redelegation
        // The unbond may need to partially unbond redelegated tokens, so
        // compute if necessary
        let modified_redelegation = match bonds_to_remove.new_entry {
            Some((bond_epoch, new_bond_amount)) => {
                // tracing::debug!(
                //     "\n New redel entry for epoch {} -> amount {}",
                //     bond_epoch, new_bond_amount
                // );
                let cur_bond_amount =
                    bonds.get(&bond_epoch).cloned().unwrap_or_default();
                if delegator_redelegated_bonds.contains_key(&bond_epoch) {
                    Self::compute_modified_redelegation(
                        delegator_redelegated_bonds,
                        bond_epoch,
                        cur_bond_amount - new_bond_amount,
                    )
                } else {
                    ModifiedRedelegation::default()
                }
            }
            None => ModifiedRedelegation::default(),
        };

        // `keysUnbonds`
        // New unbonds. This will be needed for a couple things
        let unbonded_bond_starts =
            if let Some((start_epoch, _)) = bonds_to_remove.new_entry {
                let mut to_remove = bonds_to_remove.epochs.clone();
                to_remove.insert(start_epoch);
                to_remove
            } else {
                bonds_to_remove.epochs.clone()
            };
        // `newUnbonds`
        let new_unbonds = unbonded_bond_starts
            .into_iter()
            .map(|start| {
                let cur_bond_amnt = bonds.get(&start).cloned().unwrap();
                let new_value = if let Some((start_epoch, new_bond_amount)) =
                    bonds_to_remove.new_entry
                {
                    if start_epoch == start {
                        cur_bond_amnt - new_bond_amount
                    } else {
                        cur_bond_amnt
                    }
                } else {
                    cur_bond_amnt
                };
                ((start, withdraw_epoch), new_value)
            })
            .collect::<BTreeMap<(Epoch, Epoch), token::Change>>();

        // Update the bonds and unbonds in the AbstractState
        // `updatedBonded` + updates to `updatedDelegator`
        for bond_epoch in &bonds_to_remove.epochs {
            bonds.remove(bond_epoch);
        }
        if let Some((bond_epoch, new_bond_amt)) = bonds_to_remove.new_entry {
            bonds.insert(bond_epoch, new_bond_amt);
        }
        // `updatedUnbonded` + updates to `updatedDelegator`
        if !is_redelegation {
            for (epoch_pair, amount) in &new_unbonds {
                let unbonds = self
                    .unbonds
                    .entry(*epoch_pair)
                    .or_default()
                    .entry(id.clone())
                    .or_default();
                *unbonds += token::Amount::from(*amount)
            }
        }

        tracing::debug!("Bonds after decrementing");
        for (start, amnt) in bonds.iter() {
            tracing::debug!(
                "Bond epoch {} - amnt {}",
                start,
                amnt.to_string_native()
            );
        }

        // `newRedelegatedUnbonds`
        // Compute new redelegated unbonds (which requires unmodified
        // redelegated bonds)
        let new_redelegated_unbonds = Self::compute_new_redelegated_unbonds(
            delegator_redelegated_bonds,
            &bonds_to_remove.epochs,
            &modified_redelegation,
        );

        // `updatedRedelegatedBonded`
        // Update the delegator's redelegated bonds in the state
        for epoch_to_remove in &bonds_to_remove.epochs {
            delegator_redelegated_bonds.remove(epoch_to_remove);
        }
        if let Some(epoch) = modified_redelegation.epoch {
            if modified_redelegation.validators_to_remove.is_empty() {
                delegator_redelegated_bonds.remove(&epoch);
            } else {
                let rbonds =
                    delegator_redelegated_bonds.entry(epoch).or_default();

                if let Some(val_to_modify) =
                    &modified_redelegation.validator_to_modify
                {
                    let mut updated_vals_to_remove =
                        modified_redelegation.validators_to_remove.clone();
                    updated_vals_to_remove.remove(val_to_modify);

                    // Remove the updated_vals_to_remove keys from the
                    // redelegated_bonds map first
                    for val in &updated_vals_to_remove {
                        rbonds.remove(val);
                    }

                    if let Some(epoch_to_modify) =
                        modified_redelegation.epoch_to_modify
                    {
                        let mut updated_epochs_to_remove =
                            modified_redelegation.epochs_to_remove.clone();
                        updated_epochs_to_remove.remove(&epoch_to_modify);
                        let val_bonds_to_modify =
                            rbonds.entry(val_to_modify.clone()).or_default();
                        for epoch in updated_epochs_to_remove {
                            val_bonds_to_modify.remove(&epoch);
                        }
                        val_bonds_to_modify.insert(
                            epoch_to_modify,
                            modified_redelegation.new_amount.unwrap(),
                        );
                    } else {
                        // Then remove to epochs_to_remove from the redelegated
                        // bonds of the val_to_modify
                        let val_bonds_to_modify =
                            rbonds.entry(val_to_modify.clone()).or_default();
                        for epoch in &modified_redelegation.epochs_to_remove {
                            val_bonds_to_modify.remove(epoch);
                        }
                    }
                } else {
                    // Remove all validators in
                    // modified_redelegation.validators_to_remove
                    // from redelegated_bonds
                    for val in &modified_redelegation.validators_to_remove {
                        rbonds.remove(val);
                    }
                }
            }
        }

        // `updatedRedelegatedUnbonded
        if !is_redelegation {
            // Get all the epoch pairs that should exist in the state now
            let new_unbond_epoch_pairs = new_redelegated_unbonds
                .keys()
                .map(|start_epoch| (*start_epoch, withdraw_epoch))
                .collect::<HashSet<_>>();

            // Update the state for delegator's redelegated unbonds now
            // NOTE: can maybe do this by only looking at those inside the new
            // epoch pairs?
            for unbond_pair in new_unbond_epoch_pairs {
                for (src_val, redel_unbonds) in
                    new_redelegated_unbonds.get(&unbond_pair.0).unwrap()
                {
                    for (src_start, unbonded) in redel_unbonds {
                        let existing_unbonded = delegator_redelegated_unbonds
                            .entry(unbond_pair)
                            .or_default()
                            .entry(src_val.clone())
                            .or_default()
                            .entry(*src_start)
                            .or_default();
                        *existing_unbonded += token::Amount::from(*unbonded);
                    }
                }
            }
        }

        // `updatedTotalBonded` and `updatedTotalUnbonded`
        // Update the validator's total bonded and total unbonded
        for ((start_epoch, _), unbonded) in &new_unbonds {
            let cur_total_bonded =
                total_bonded.entry(*start_epoch).or_default();
            *cur_total_bonded -= *unbonded;
            let cur_total_unbonded =
                total_unbonded.entry(*start_epoch).or_default();
            *cur_total_unbonded += token::Amount::from(*unbonded);
        }

        // `updatedTotalRedelegatedBonded` and `updatedTotalRedelegatedUnbonded`
        // Update the validator's total redelegated bonded and unbonded
        for (dest_start, r_unbonds) in &new_redelegated_unbonds {
            for (src_val, changes) in r_unbonds {
                for (bond_start, change) in changes {
                    let cur_total_bonded = validator_total_redelegated_bonded
                        .entry(*dest_start)
                        .or_default()
                        .entry(src_val.clone())
                        .or_default()
                        .entry(*bond_start)
                        .or_default();
                    *cur_total_bonded -= token::Amount::from(*change);

                    let cur_total_unbonded =
                        validator_total_redelegated_unbonded
                            .entry(*dest_start)
                            .or_default()
                            .entry(src_val.clone())
                            .or_default()
                            .entry(*bond_start)
                            .or_default();
                    *cur_total_unbonded += token::Amount::from(*change);
                }
            }
        }

        // `resultSlashing`
        // Get the slashed amount of the unbond now
        let result_slashing = Self::compute_amount_after_slashing_unbond(
            &self.params,
            validator_slashes,
            &id.validator,
            &new_unbonds,
            &new_redelegated_unbonds,
        );
        // `amountAfterSlashing`
        let amount_after_slashing = result_slashing.sum.change();

        let pipeline_state = self
            .validator_states
            .get(&self.pipeline())
            .unwrap()
            .get(&id.validator)
            .unwrap();

        if *pipeline_state != ValidatorState::Jailed {
            self.update_validator_sets(
                self.pipeline(),
                &id.validator,
                -amount_after_slashing,
            );
        }
        self.update_validator_total_stake(
            &id.validator,
            -amount_after_slashing,
        );

        result_slashing
    }

    fn update_state_with_redelegation(
        &mut self,
        id: &BondId,
        new_validator: &Address,
        change: token::Change,
    ) {
        // First need to unbond the redelegated tokens
        // NOTE: same logic as unbond transition but with some things left out
        let pipeline_epoch = self.pipeline();

        // `resultUnbond`
        let result_slashing = self.unbond_tokens(id, change, true);

        // `amountAfterSlashing`
        let amount_after_slashing = result_slashing.sum.change();
        debug_assert!(amount_after_slashing.non_negative());

        // `updatedRedelegatedBonds`
        // Update the delegator's redelegated bonded
        let delegator_redelegated_bonded = self
            .delegator_redelegated_bonded
            .entry(id.source.clone())
            .or_default()
            .entry(new_validator.clone())
            .or_default()
            .entry(pipeline_epoch)
            .or_default()
            .entry(id.validator.clone())
            .or_default();
        for (start_epoch, bonded) in &result_slashing.epoch_map {
            *delegator_redelegated_bonded
                .entry(*start_epoch)
                .or_default() += (*bonded).into();
        }

        if !amount_after_slashing.is_zero() {
            // `updatedDelegator` --> `with("bonded")`
            // Update the delegator's bonds
            let bonds = self
                .bonds
                .entry(BondId {
                    source: id.source.clone(),
                    validator: new_validator.clone(),
                })
                .or_default();
            *bonds.entry(pipeline_epoch).or_default() += amount_after_slashing;

            // `updatedDestValidator` --> `with("totalBonded")`
            // Update the dest validator's total bonded
            let dest_total_bonded = self
                .total_bonded
                .entry(new_validator.clone())
                .or_default()
                .entry(pipeline_epoch)
                .or_default();
            *dest_total_bonded += amount_after_slashing;
        }

        // `updatedOutgoingRedelegations` and `updatedSrcValidator`
        // Update the src validator's outgoing redelegations
        let outgoing_redelegations = self
            .outgoing_redelegations
            .entry(id.validator.clone())
            .or_default()
            .entry(new_validator.clone())
            .or_default();
        for (start_epoch, bonded) in &result_slashing.epoch_map {
            let cur_outgoing = outgoing_redelegations
                .entry((*start_epoch, self.epoch))
                .or_default();
            *cur_outgoing += *bonded;
        }

        // `updatedDestValidator` --> `with("totalRedelegatedBonded")`
        // Update the dest validator's total redelegated bonded
        let total_redelegated_bonded = self
            .validator_total_redelegated_bonded
            .entry(new_validator.clone())
            .or_default()
            .entry(pipeline_epoch)
            .or_default()
            .entry(id.validator.clone())
            .or_default();
        for (start_epoch, bonded) in &result_slashing.epoch_map {
            let cur_tot_bonded =
                total_redelegated_bonded.entry(*start_epoch).or_default();
            *cur_tot_bonded += *bonded;
        }

        // `updatedDestValidator` --> `with("incomingRedelegations")`
        // Update the dest validator's incoming redelegations
        let incoming_redelegations = self
            .incoming_redelegations
            .entry(new_validator.clone())
            .or_default();
        incoming_redelegations.insert(id.source.clone(), pipeline_epoch);

        // `updatedDestValidator` --> `with("stake")`
        // Update validator set and stake
        let pipeline_state = self
            .validator_states
            .get(&self.pipeline())
            .unwrap()
            .get(new_validator)
            .unwrap();

        if !amount_after_slashing.is_zero() {
            if *pipeline_state != ValidatorState::Jailed {
                self.update_validator_sets(
                    self.pipeline(),
                    new_validator,
                    amount_after_slashing,
                );
            }
            self.update_validator_total_stake(
                new_validator,
                amount_after_slashing,
            );
        }

        // let withdraw_epoch = pipeline_epoch
        //     + self.params.unbonding_len
        //     + self.params.cubic_slashing_window_length;
        // let bonds = self.bonds.entry(id.clone()).or_default();
        // let src_total_unbonded = self
        //     .total_unbonded
        //     .entry(id.validator.clone())
        //     .or_default()
        //     .entry(pipeline_epoch)
        //     .or_default();
        // let src_total_bonded =
        //     self.total_bonded.entry(id.validator.clone()).or_default();

        // let delegator_redelegated_bonds = self
        //     .delegator_redelegated_bonded
        //     .entry(id.source.clone())
        //     .or_default()
        //     .entry(id.validator.clone())
        //     .or_default();

        // let src_validator_total_redelegated_bonded = self
        //     .validator_total_redelegated_bonded
        //     .entry(id.validator.clone())
        //     .or_default();

        // let validator_slashes = &self.validator_slashes;

        // tracing::debug!("Bonds before decrementing");
        // for (start, amnt) in bonds.iter() {
        //     tracing::debug!(
        //         "Bond epoch {} - amnt {}",
        //         start,
        //         amnt.to_string_native()
        //     );
        // }

        // // Get the bonds for removal
        // let bonds_to_remove = Self::find_bonds_to_remove(bonds, change);

        // // Modified redelegation
        // // The unbond may need to partially unbond redelegated tokens, so
        // // compute if necessary
        // let modified_redelegation = match bonds_to_remove.new_entry {
        //     Some((bond_epoch, new_bond_amount)) => {
        //         // tracing::debug!(
        //         //     "\n New redel entry for epoch {} -> amount {}",
        //         //     bond_epoch, new_bond_amount
        //         // );
        //         let cur_bond_amount =
        //             bonds.get(&bond_epoch).cloned().unwrap_or_default();
        //         if delegator_redelegated_bonds.contains_key(&bond_epoch) {
        //             Self::compute_modified_redelegation(
        //                 delegator_redelegated_bonds,
        //                 bond_epoch,
        //                 cur_bond_amount - new_bond_amount,
        //             )
        //         } else {
        //             ModifiedRedelegation::default()
        //         }
        //     }
        //     None => ModifiedRedelegation::default(),
        // };

        // // New unbonds. This will be needed for a couple things
        // let unbonded_bond_starts =
        //     if let Some((start_epoch, _)) = bonds_to_remove.new_entry {
        //         let mut to_remove = bonds_to_remove.epochs.clone();
        //         to_remove.insert(start_epoch);
        //         to_remove
        //     } else {
        //         bonds_to_remove.epochs.clone()
        //     };

        // let new_unbonds = unbonded_bond_starts
        //     .into_iter()
        //     .map(|start| {
        //         let cur_bond_amnt = bonds.get(&start).cloned().unwrap();
        //         let new_value = if let Some((start_epoch, new_bond_amount)) =
        //             bonds_to_remove.new_entry
        //         {
        //             if start_epoch == start {
        //                 cur_bond_amnt - new_bond_amount
        //             } else {
        //                 cur_bond_amnt
        //             }
        //         } else {
        //             cur_bond_amnt
        //         };
        //         ((start, withdraw_epoch), new_value)
        //     })
        //     .collect::<BTreeMap<(Epoch, Epoch), token::Change>>();

        // // Update the bonds and unbonds in the AbstractState
        // for bond_epoch in &bonds_to_remove.epochs {
        //     bonds.remove(bond_epoch);
        // }
        // if let Some((bond_epoch, new_bond_amt)) = bonds_to_remove.new_entry {
        //     bonds.insert(bond_epoch, new_bond_amt);
        // }

        // // NOTE: DO NOT UPDATE THE UNBONDS --------------------------

        // tracing::debug!("Bonds after decrementing");
        // for (start, amnt) in bonds.iter() {
        //     tracing::debug!(
        //         "Bond epoch {} - amnt {}",
        //         start,
        //         amnt.to_string_native()
        //     );
        // }

        // // Compute new redelegated unbonds (which requires unmodified
        // // redelegated bonds)
        // let new_redelegated_unbonds = Self::compute_new_redelegated_unbonds(
        //     delegator_redelegated_bonds,
        //     &bonds_to_remove.epochs,
        //     &modified_redelegation,
        // );
        // println!("Computed new redelegated unbonds");
        // dbg!(&new_redelegated_unbonds);
        // dbg!(&src_validator_total_redelegated_bonded);

        // // Update the delegator's redelegated bonds in the state
        // for epoch_to_remove in &bonds_to_remove.epochs {
        //     delegator_redelegated_bonds.remove(epoch_to_remove);
        // }
        // if let Some(epoch) = modified_redelegation.epoch {
        //     let rbonds =
        // delegator_redelegated_bonds.entry(epoch).or_default();

        //     if let Some(val_to_modify) =
        //         &modified_redelegation.validator_to_modify
        //     {
        //         let mut updated_vals_to_remove =
        //             modified_redelegation.validators_to_remove.clone();
        //         updated_vals_to_remove.remove(val_to_modify);

        //         if let Some(epoch_to_modify) =
        //             modified_redelegation.epoch_to_modify
        //         {
        //             let mut updated_epochs_to_remove =
        //                 modified_redelegation.epochs_to_remove.clone();
        //             updated_epochs_to_remove.remove(&epoch_to_modify);
        //             let val_bonds_to_modify =
        //                 rbonds.entry(val_to_modify.clone()).or_default();
        //             for epoch in updated_epochs_to_remove {
        //                 val_bonds_to_modify.remove(&epoch);
        //             }
        //             val_bonds_to_modify.insert(
        //                 epoch_to_modify,
        //                 modified_redelegation.new_amount.unwrap(),
        //             );
        //             // Then, remove the updated_vals_to_remove keys from the
        //             // redelegated_bonds map before doing...
        //         } else {
        //             // Remove the updated_vals_to_remove keys from the
        //             // redelegated_bonds map first
        //             for val in &updated_vals_to_remove {
        //                 rbonds.remove(val);
        //             }
        //             // Then remove to epochs_to_remove from the redelegated
        //             // bonds of the val_to_modify
        //             let val_bonds_to_modify =
        //                 rbonds.entry(val_to_modify.clone()).or_default();
        //             for epoch in &modified_redelegation.epochs_to_remove {
        //                 val_bonds_to_modify.remove(epoch);
        //             }
        //         }
        //     } else {
        //         // Remove all validators in
        //         // modified_redelegation.validators_to_remove
        //         // from redelegated_bonds
        //         for val in &modified_redelegation.validators_to_remove {
        //             rbonds.remove(val);
        //         }
        //     }
        // }
        // println!("Updated delegator's redelegated bonds");

        // // NOTE: DO NOT UPDATE THE DELEGATOR REDELEGATED UNBONDS
        // //

        // // Update the validator's total bonded and unbonded
        // for ((start_epoch, _), unbonded) in &new_unbonds {
        //     let cur_total_bonded =
        //         src_total_bonded.entry(*start_epoch).or_default();
        //     *cur_total_bonded -= *unbonded;
        //     debug_assert!(cur_total_bonded.non_negative());

        //     let cur_total_unbonded =
        //         src_total_unbonded.entry(*start_epoch).or_default();
        //     *cur_total_unbonded += token::Amount::from(*unbonded);
        // }
        // println!("Updated src validator's total bonded and unbonded");

        // // Update the src validator's total redelegated bonded
        // for (dest_start, r_unbonds) in &new_redelegated_unbonds {
        //     for (src_val, changes) in r_unbonds {
        //         for (bond_start, change) in changes {
        //             let cur_total_redel_bonded =
        //                 src_validator_total_redelegated_bonded
        //                     .entry(*dest_start)
        //                     .or_default()
        //                     .entry(src_val.clone())
        //                     .or_default()
        //                     .entry(*bond_start)
        //                     .or_default();
        //             *cur_total_redel_bonded -= token::Amount::from(*change);
        //         }
        //     }
        // }
        // println!("Updated src validator's total redelegated bonded");

        // // NOTE: DO NOT UPDATE THE VALIDATOR'S TOTAL REDELEGATED UNBONDED

        // // Get the slashed amount of the unbond now
        // let result_slashing = Self::compute_amount_after_slashing_unbond(
        //     &self.params,
        //     validator_slashes,
        //     &id.validator,
        //     &new_unbonds,
        //     &new_redelegated_unbonds,
        // );
        // let amount_after_slashing = result_slashing.sum.change();
        // println!("result_slashing");

        // // TODO: do I want to adjust the validator set here now or all at
        // end? let pipeline_state = self
        //     .validator_states
        //     .get(&self.pipeline())
        //     .unwrap()
        //     .get(&id.validator)
        //     .unwrap();

        // if *pipeline_state != ValidatorState::Jailed {
        //     self.update_validator_sets(
        //         self.pipeline(),
        //         &id.validator,
        //         -amount_after_slashing,
        //     );
        // }
        // self.update_validator_total_stake(
        //     &id.validator,
        //     -amount_after_slashing,
        // );

        // ----- Now the rest of redelegation

        // // Update the delegator's redelegated bonded
        // let delegator_redelegated_bonded = self
        //     .delegator_redelegated_bonded
        //     .entry(id.source.clone())
        //     .or_default()
        //     .entry(new_validator.clone())
        //     .or_default()
        //     .entry(pipeline_epoch)
        //     .or_default()
        //     .entry(id.validator.clone())
        //     .or_default();
        // for (start_epoch, bonded) in &result_slashing.epoch_map {
        //     *delegator_redelegated_bonded
        //         .entry(*start_epoch)
        //         .or_default() += (*bonded).into();
        // }

        // // Update the delegator's bonds
        // let bonds = self
        //     .bonds
        //     .entry(BondId {
        //         source: id.source.clone(),
        //         validator: new_validator.clone(),
        //     })
        //     .or_default();
        // if !amount_after_slashing.is_zero() {
        //     *bonds.entry(pipeline_epoch).or_default() +=
        // amount_after_slashing; }

        // // Update the src validator's outgoing redelegations
        // let outgoing_redelegations = self
        //     .outgoing_redelegations
        //     .entry(id.validator.clone())
        //     .or_default()
        //     .entry(new_validator.clone())
        //     .or_default();
        // for (start_epoch, bonded) in &result_slashing.epoch_map {
        //     let cur_outgoing = outgoing_redelegations
        //         .entry((*start_epoch, self.epoch))
        //         .or_default();
        //     *cur_outgoing += *bonded;
        // }

        // // Update the dest validator's total bonded
        // let dest_total_bonded = self
        //     .total_bonded
        //     .entry(new_validator.clone())
        //     .or_default()
        //     .entry(pipeline_epoch)
        //     .or_default();
        // *dest_total_bonded += amount_after_slashing;

        // // Update the dest validator's total redelegated bonded
        // let total_redelegated_bonded = self
        //     .validator_total_redelegated_bonded
        //     .entry(new_validator.clone())
        //     .or_default()
        //     .entry(pipeline_epoch)
        //     .or_default()
        //     .entry(id.validator.clone())
        //     .or_default();
        // for (start_epoch, bonded) in &result_slashing.epoch_map {
        //     let cur_tot_bonded =
        //         total_redelegated_bonded.entry(*start_epoch).or_default();
        //     *cur_tot_bonded += *bonded;
        // }

        // // Update the dest validator's incoming redelegations
        // let incoming_redelegations = self
        //     .incoming_redelegations
        //     .entry(new_validator.clone())
        //     .or_default();
        // incoming_redelegations.insert(id.source.clone(), pipeline_epoch);

        // // Update validator set and stake
        // let pipeline_state = self
        //     .validator_states
        //     .get(&self.pipeline())
        //     .unwrap()
        //     .get(new_validator)
        //     .unwrap();

        // if *pipeline_state != ValidatorState::Jailed {
        //     self.update_validator_sets(
        //         self.pipeline(),
        //         new_validator,
        //         amount_after_slashing,
        //     );
        // }
        // self.update_validator_total_stake(new_validator,
        // amount_after_slashing);
    }

    /// Update validator's total stake with bonded or unbonded change at the
    /// pipeline epoch
    fn update_validator_total_stake(
        &mut self,
        validator: &Address,
        change: token::Change,
    ) {
        let total_stakes = self
            .validator_stakes
            .entry(self.pipeline())
            .or_default()
            .entry(validator.clone())
            .or_default();
        *total_stakes += change;
    }

    /// Update validator in sets with bonded or unbonded change
    fn update_validator_sets(
        &mut self,
        epoch: Epoch,
        validator: &Address,
        change: token::Change,
    ) {
        tracing::debug!(
            "\nUpdating set for validator {} in epoch {} with amount {}\n",
            validator,
            epoch,
            change
        );
        if change.is_zero() {
            return;
        }
        // let pipeline = self.pipeline();
        let consensus_set = self.consensus_set.entry(epoch).or_default();
        let below_cap_set = self.below_capacity_set.entry(epoch).or_default();
        let below_thresh_set =
            self.below_threshold_set.entry(epoch).or_default();

        let validator_stakes = self.validator_stakes.get(&epoch).unwrap();
        let validator_states = self.validator_states.get_mut(&epoch).unwrap();

        let state_pre = validator_states.get(validator).unwrap();

        let this_val_stake_pre = *validator_stakes.get(validator).unwrap();
        let this_val_stake_post =
            token::Amount::from_change(this_val_stake_pre + change);
        let this_val_stake_pre = token::Amount::from_change(this_val_stake_pre);

        let threshold = self.params.validator_stake_threshold;
        if this_val_stake_pre < threshold && this_val_stake_post < threshold {
            // Validator is already below-threshold and will remain there, so do
            // nothing
            debug_assert!(below_thresh_set.contains(validator));
            return;
        }

        match state_pre {
            ValidatorState::Consensus => {
                // tracing::debug!("Validator initially in consensus");
                // Remove from the prior stake
                let vals = consensus_set.entry(this_val_stake_pre).or_default();
                // dbg!(&vals);
                vals.retain(|addr| addr != validator);
                // dbg!(&vals);

                if vals.is_empty() {
                    consensus_set.remove(&this_val_stake_pre);
                }

                // If posterior stake is below threshold, place into the
                // below-threshold set
                if this_val_stake_post < threshold {
                    below_thresh_set.insert(validator.clone());
                    validator_states.insert(
                        validator.clone(),
                        ValidatorState::BelowThreshold,
                    );

                    // Promote the next below-cap validator if there is one
                    if let Some(mut max_below_cap) = below_cap_set.last_entry()
                    {
                        let max_below_cap_stake = *max_below_cap.key();
                        let vals = max_below_cap.get_mut();
                        let promoted_val = vals.pop_front().unwrap();
                        // Remove the key if there's nothing left
                        if vals.is_empty() {
                            below_cap_set.remove(&max_below_cap_stake);
                        }

                        consensus_set
                            .entry(max_below_cap_stake.0)
                            .or_default()
                            .push_back(promoted_val.clone());
                        validator_states
                            .insert(promoted_val, ValidatorState::Consensus);
                    }

                    return;
                }

                // If unbonding, check the max below-cap validator's state if we
                // need to do a swap
                if change < token::Change::zero() {
                    if let Some(mut max_below_cap) = below_cap_set.last_entry()
                    {
                        let max_below_cap_stake = *max_below_cap.key();
                        if max_below_cap_stake.0 > this_val_stake_post {
                            // Swap this validator with the max below-cap
                            let vals = max_below_cap.get_mut();
                            let first_val = vals.pop_front().unwrap();
                            // Remove the key if there's nothing left
                            if vals.is_empty() {
                                below_cap_set.remove(&max_below_cap_stake);
                            }
                            // Do the swap in the validator sets
                            consensus_set
                                .entry(max_below_cap_stake.0)
                                .or_default()
                                .push_back(first_val.clone());
                            below_cap_set
                                .entry(this_val_stake_post.into())
                                .or_default()
                                .push_back(validator.clone());

                            // Change the validator states
                            validator_states
                                .insert(first_val, ValidatorState::Consensus);
                            validator_states.insert(
                                validator.clone(),
                                ValidatorState::BelowCapacity,
                            );

                            // And we're done here
                            return;
                        }
                    }
                }

                // Insert with the posterior stake
                consensus_set
                    .entry(this_val_stake_post)
                    .or_default()
                    .push_back(validator.clone());
            }
            ValidatorState::BelowCapacity => {
                // tracing::debug!("Validator initially in below-cap");

                // Remove from the prior stake
                let vals =
                    below_cap_set.entry(this_val_stake_pre.into()).or_default();
                vals.retain(|addr| addr != validator);
                if vals.is_empty() {
                    below_cap_set.remove(&this_val_stake_pre.into());
                }

                // If posterior stake is below threshold, place into the
                // below-threshold set
                if this_val_stake_post < threshold {
                    below_thresh_set.insert(validator.clone());
                    validator_states.insert(
                        validator.clone(),
                        ValidatorState::BelowThreshold,
                    );
                    return;
                }

                // If bonding, check the min consensus validator's state if we
                // need to do a swap
                if change >= token::Change::zero() {
                    // dbg!(&consensus_set);
                    if let Some(mut min_consensus) = consensus_set.first_entry()
                    {
                        // dbg!(&min_consensus);
                        let min_consensus_stake = *min_consensus.key();
                        if this_val_stake_post > min_consensus_stake {
                            // Swap this validator with the max consensus
                            let vals = min_consensus.get_mut();
                            let last_val = vals.pop_back().unwrap();
                            // Remove the key if there's nothing left
                            if vals.is_empty() {
                                consensus_set.remove(&min_consensus_stake);
                            }
                            // Do the swap in the validator sets
                            below_cap_set
                                .entry(min_consensus_stake.into())
                                .or_default()
                                .push_back(last_val.clone());
                            consensus_set
                                .entry(this_val_stake_post)
                                .or_default()
                                .push_back(validator.clone());

                            // Change the validator states
                            validator_states.insert(
                                validator.clone(),
                                ValidatorState::Consensus,
                            );
                            validator_states.insert(
                                last_val,
                                ValidatorState::BelowCapacity,
                            );

                            // And we're done here
                            return;
                        }
                    }
                }

                // Insert with the posterior stake
                below_cap_set
                    .entry(this_val_stake_post.into())
                    .or_default()
                    .push_back(validator.clone());
            }
            ValidatorState::BelowThreshold => {
                // We know that this validator will be promoted into one of the
                // higher sets, so first remove from the below-threshold set.
                below_thresh_set.remove(validator);

                let num_consensus =
                    consensus_set.iter().fold(0, |sum, (_, validators)| {
                        sum + validators.len() as u64
                    });
                if num_consensus < self.params.max_validator_slots {
                    // Place the validator directly into the consensus set
                    consensus_set
                        .entry(this_val_stake_post)
                        .or_default()
                        .push_back(validator.clone());
                    validator_states
                        .insert(validator.clone(), ValidatorState::Consensus);
                    return;
                }
                // Determine which set to place the validator into
                if let Some(mut min_consensus) = consensus_set.first_entry() {
                    // dbg!(&min_consensus);
                    let min_consensus_stake = *min_consensus.key();
                    if this_val_stake_post > min_consensus_stake {
                        // Swap this validator with the max consensus
                        let vals = min_consensus.get_mut();
                        let last_val = vals.pop_back().unwrap();
                        // Remove the key if there's nothing left
                        if vals.is_empty() {
                            consensus_set.remove(&min_consensus_stake);
                        }
                        // Do the swap in the validator sets
                        below_cap_set
                            .entry(min_consensus_stake.into())
                            .or_default()
                            .push_back(last_val.clone());
                        consensus_set
                            .entry(this_val_stake_post)
                            .or_default()
                            .push_back(validator.clone());

                        // Change the validator states
                        validator_states.insert(
                            validator.clone(),
                            ValidatorState::Consensus,
                        );
                        validator_states
                            .insert(last_val, ValidatorState::BelowCapacity);
                    } else {
                        // Place the validator into the below-capacity set
                        below_cap_set
                            .entry(this_val_stake_post.into())
                            .or_default()
                            .push_back(validator.clone());
                        validator_states.insert(
                            validator.clone(),
                            ValidatorState::BelowCapacity,
                        );
                    }
                }
            }
            ValidatorState::Inactive => {
                panic!("unexpected state")
            }
            ValidatorState::Jailed => {
                panic!("unexpected state (jailed)")
            }
        }
    }

    fn process_enqueued_slashes(&mut self) {
        let slashes_this_epoch = self
            .enqueued_slashes
            .get(&self.epoch)
            .cloned()
            .unwrap_or_default();

        if slashes_this_epoch.is_empty() {
            return;
        }

        let infraction_epoch =
            self.epoch - self.params.slash_processing_epoch_offset();
        let cubic_rate = self.cubic_slash_rate();

        // Get effective slash rate per validator and update the slashes in the
        // Abstract state
        let slash_rates = slashes_this_epoch.iter().fold(
            BTreeMap::<Address, Dec>::new(),
            |mut acc, (validator, slashes)| {
                let mut tot_rate =
                    acc.get(validator).cloned().unwrap_or_default();
                for slash in slashes {
                    debug_assert_eq!(slash.epoch, infraction_epoch);
                    let rate = cmp::max(
                        slash.r#type.get_slash_rate(&self.params),
                        cubic_rate,
                    );
                    tot_rate = cmp::min(Dec::one(), tot_rate + rate);
                }
                acc.insert(validator.clone(), tot_rate);
                acc
            },
        );

        let mut map_validator_slash: BTreeMap<
            Address,
            BTreeMap<Epoch, token::Change>,
        > = BTreeMap::new();
        for (validator, rate) in slash_rates {
            self.process_validator_slash(
                &validator,
                rate,
                &mut map_validator_slash,
            );
            // map_validator_slash.insert(validator, slash_res);
        }

        for (validator, slash_amounts) in map_validator_slash {
            for (update_epoch, delta) in slash_amounts {
                let state = self
                    .validator_states
                    .get(&update_epoch)
                    .unwrap()
                    .get(&validator)
                    .unwrap();
                if *state != ValidatorState::Jailed {
                    self.update_validator_sets(
                        update_epoch,
                        &validator,
                        -delta,
                    );
                }

                let stake = self
                    .validator_stakes
                    .entry(update_epoch)
                    .or_default()
                    .entry(validator.clone())
                    .or_default();
                *stake -= delta;
            }

            let next_state = self
                .validator_states
                .get(&self.epoch.next())
                .unwrap()
                .get(&validator)
                .cloned()
                .unwrap();

            let pipeline_state = self
                .validator_states
                .get(&self.pipeline())
                .unwrap()
                .get(&validator)
                .cloned()
                .unwrap();

            debug_assert_eq!(next_state, pipeline_state);
        }

        // Update the slashes in the Abstract state ONLY AFTER processing them
        for (validator, slashes) in slashes_this_epoch {
            let cur_slashes =
                self.validator_slashes.entry(validator.clone()).or_default();

            for slash in slashes {
                let rate = cmp::max(
                    slash.r#type.get_slash_rate(&self.params),
                    cubic_rate,
                );
                cur_slashes.push(Slash {
                    epoch: slash.epoch,
                    block_height: Default::default(),
                    r#type: SlashType::DuplicateVote,
                    rate,
                });
            }
        }
    }

    fn process_validator_slash(
        &mut self,
        validator: &Address,
        slash_rate: Dec,
        val_slash_amounts: &mut BTreeMap<
            Address,
            BTreeMap<Epoch, token::Change>,
        >,
    ) {
        let slash_amounts = val_slash_amounts
            .get(validator)
            .cloned()
            .unwrap_or_default();
        let result_slash =
            self.slash_validator(validator, slash_rate, &slash_amounts);

        // dbg!(&val_slash_amounts);

        // `updatedSlashedAmountMap`
        let validator_slashes =
            val_slash_amounts.entry(validator.clone()).or_default();
        for (epoch, slash) in result_slash {
            *validator_slashes.entry(epoch).or_default() += slash;
        }

        // dbg!(&val_slash_amounts);

        let dest_validators = self
            .outgoing_redelegations
            .get(validator)
            .cloned()
            .unwrap_or_default()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();

        for dest_val in dest_validators {
            let to_modify =
                val_slash_amounts.entry(dest_val.clone()).or_default();

            // `slashValidatorRedelegation`
            self.slash_validator_redelegation(
                validator, &dest_val, slash_rate, to_modify,
            );

            if to_modify.is_empty() {
                val_slash_amounts.remove(&dest_val);
            };
        }
    }

    fn slash_validator(
        &self,
        validator: &Address,
        slash_rate: Dec,
        val_slash_amounts: &BTreeMap<Epoch, token::Change>,
    ) -> BTreeMap<Epoch, token::Change> {
        tracing::debug!(
            "Slashing validator {} at rate {}",
            validator,
            slash_rate
        );

        let infraction_epoch =
            self.epoch - self.params.slash_processing_epoch_offset();

        let total_unbonded = self
            .total_unbonded
            .get(validator)
            .cloned()
            .unwrap_or_default();
        let total_redelegated_unbonded = self
            .validator_total_redelegated_unbonded
            .get(validator)
            .cloned()
            .unwrap_or_default();

        // `val bonds`
        let mut total_bonded = self
            .total_bonded
            .get(validator)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|&(epoch, _amount)| epoch <= infraction_epoch)
            .collect::<BTreeMap<_, _>>();

        // `val redelegatedBonds`
        let mut total_redelegated_bonded = total_bonded
            .keys()
            .filter(|&epoch| {
                self.validator_total_redelegated_bonded
                    .get(validator)
                    .cloned()
                    .unwrap_or_default()
                    .contains_key(epoch)
            })
            .map(|epoch| {
                (
                    *epoch,
                    self.validator_total_redelegated_bonded
                        .get(validator)
                        .unwrap()
                        .get(epoch)
                        .cloned()
                        .unwrap(),
                )
            })
            .collect::<BTreeMap<_, _>>();

        let mut slashed_amounts = val_slash_amounts.clone();
        let mut sum = token::Amount::zero();

        // NEW ---------------------------------
        let eps = self
            .epoch
            .iter_range(self.params.pipeline_len)
            .collect::<Vec<_>>();
        for epoch in eps.into_iter().rev() {
            let amount = total_bonded.iter().fold(
                token::Change::zero(),
                |acc, (bond_start, bond_amount)| {
                    let redel_bonds = total_redelegated_bonded
                        .get(bond_start)
                        .cloned()
                        .unwrap_or_default();
                    acc + self.compute_slash_bond_at_epoch(
                        epoch,
                        infraction_epoch,
                        *bond_start,
                        token::Amount::from(*bond_amount),
                        &redel_bonds,
                        slash_rate,
                        validator,
                    )
                },
            );
            let new_bonds = total_unbonded
                .get(&epoch)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|(ep, _)| *ep <= infraction_epoch)
                .map(|(ep, amount)| (ep, amount.change()))
                .collect::<BTreeMap<_, _>>();
            let new_redelegated_bonds = new_bonds
                .keys()
                .filter(|&ep| {
                    total_redelegated_unbonded
                        .get(&epoch)
                        .cloned()
                        .unwrap_or_default()
                        .contains_key(ep)
                })
                .map(|ep| {
                    (
                        *ep,
                        total_redelegated_unbonded
                            .get(&epoch)
                            .unwrap()
                            .get(ep)
                            .cloned()
                            .unwrap(),
                    )
                })
                .collect::<BTreeMap<_, _>>();

            total_bonded = new_bonds;
            total_redelegated_bonded = new_redelegated_bonds;
            sum += token::Amount::from(amount);

            let cur = slashed_amounts.entry(epoch).or_default();
            *cur += sum.change();
        }
        // Hack - should this be done differently? (think this is safe)
        let last_amt = slashed_amounts
            .get(&self.pipeline().prev())
            .cloned()
            .unwrap();
        slashed_amounts.insert(self.pipeline(), last_amt);

        slashed_amounts

        // let mut init_total_unbonded = token::Change::zero();
        // let mut init_bond_balance = token::Change::zero();
        // let mut init_redelegated_bond_balance =
        //     BTreeMap::<Epoch, token::Change>::new();

        // for epoch in Epoch::iter_bounds_inclusive(
        //     infraction_epoch.next(),
        //     self.epoch.prev(),
        // ) {
        //     println!("\nInit epoch {}", epoch);

        //     init_total_unbonded += self.compute_total_unbonded(
        //         validator,
        //         infraction_epoch,
        //         &total_unbonded.get(&epoch).cloned().unwrap_or_default(),
        //         &total_redelegated_unbonded
        //             .get(&epoch)
        //             .cloned()
        //             .unwrap_or_default(),
        //     );

        //     let bond_amount =
        //         total_bonded.get(&epoch).cloned().unwrap_or_default();
        //     dbg!(&bond_amount);
        //     init_bond_balance += bond_amount
        //         - Self::compute_recent_total_unbonded( infraction_epoch,
        //           &total_unbonded.get(&epoch).cloned().unwrap_or_default(),
        //           &total_redelegated_unbonded .get(&epoch) .cloned()
        //           .unwrap_or_default(),
        //         );
        //     dbg!(&init_bond_balance);

        //     self.compute_redelegated_bonds_balance(
        //         epoch,
        //         &total_redelegated_bonded
        //             .get(&epoch)
        //             .cloned()
        //             .unwrap_or_default(),
        //         &total_redelegated_unbonded,
        //         &mut init_redelegated_bond_balance,
        //     );
        // }

        // let mut slashed_amounts = val_slash_amounts.clone();
        // dbg!(&slashed_amounts);

        // println!(
        //     "CUR INIT TOTAL UNBONDED = {}",
        //     init_total_unbonded.to_string_native()
        // );
        // println!(
        //     "CUR INIT BOND BALANCE = {}",
        //     init_bond_balance.to_string_native()
        // );
        // println!(
        //     "CUR INIT REDELEGATED BOND BALANCE = {:?}",
        //     init_redelegated_bond_balance
        // );
        // for epoch in Epoch::iter_range(self.epoch, self.params.pipeline_len +
        // 1) {
        //     println!("Updating epoch {}", epoch);
        //     let updated_total_unbonded = init_total_unbonded
        //         + self.compute_total_unbonded(
        //             validator,
        //             infraction_epoch,
        //             &total_unbonded.get(&epoch).cloned().unwrap_or_default(),
        //             &total_redelegated_unbonded
        //                 .get(&epoch)
        //                 .cloned()
        //                 .unwrap_or_default(),
        //         );
        //     let updated_bonds_balance = init_bond_balance
        //         + total_bonded.get(&epoch).cloned().unwrap_or_default()
        //         - Self::compute_recent_total_unbonded( infraction_epoch,
        //           &total_unbonded.get(&epoch).cloned().unwrap_or_default(),
        //           &total_redelegated_unbonded .get(&epoch) .cloned()
        //           .unwrap_or_default(),
        //         );

        //     self.compute_redelegated_bonds_balance(
        //         epoch,
        //         &total_redelegated_bonded
        //             .get(&epoch)
        //             .cloned()
        //             .unwrap_or_default(),
        //         &total_redelegated_unbonded,
        //         &mut init_redelegated_bond_balance,
        //     );

        //     dbg!(&updated_total_unbonded);
        //     dbg!(&updated_bonds_balance);
        //     dbg!(&init_redelegated_bond_balance);

        //     let slashed_amount = (infraction_stake - updated_total_unbonded)
        //         .mul_ceil(slash_rate);
        //     let nom_stake = self
        //         .validator_stakes
        //         .get(&epoch)
        //         .unwrap()
        //         .get(validator)
        //         .cloned()
        //         .unwrap_or_default();
        //     let current_stake = nom_stake
        //         - slashed_amounts.get(&epoch).cloned().unwrap_or_default();

        //     dbg!(&slashed_amount);
        //     dbg!(&current_stake);

        //     let slashable_stake = current_stake
        //         - updated_bonds_balance
        //         - init_redelegated_bond_balance .get(&epoch) .cloned()
        //           .unwrap_or_default();

        //     dbg!(&slashable_stake);

        //     let cur_amount = slashed_amounts.entry(epoch).or_default();
        //     *cur_amount += cmp::min(slashed_amount, slashable_stake);

        //     init_total_unbonded = updated_total_unbonded;
        //     init_bond_balance = updated_bonds_balance;

        //     println!("After updating:");
        //     dbg!(&init_total_unbonded);
        //     dbg!(&init_bond_balance);
        //     dbg!(&init_redelegated_bond_balance);
        //     dbg!(&slashed_amounts);
        // }

        // slashed_amounts
    }

    // fn compute_redelegated_bonds_balance(
    //     &self,
    //     redel_bond_start: Epoch,
    //     total_redelegated_bonded: &BTreeMap<
    //         Address,
    //         BTreeMap<Epoch, token::Amount>,
    //     >,
    //     total_redelegated_unbonded: &AbstractTotalRedelegatedUnbonded,
    //     redelegated_balances: &mut BTreeMap<Epoch, token::Change>,
    // ) {
    //     println!("COMPUTE REDELEGATED BONDS BALANCE");
    //     for (src_validator, bonds) in total_redelegated_bonded {
    //         let slashes = self
    //             .validator_slashes
    //             .get(src_validator)
    //             .cloned()
    //             .unwrap_or_default();
    //         for (&bond_start, &amount) in bonds {
    //             let redelegation = crate::types::Redelegation {
    //                 redel_bond_start,
    //                 src_validator: src_validator.clone(),
    //                 bond_start,
    //                 amount,
    //             };
    //             dbg!(&redelegation);

    //             self.compute_remainder_redelegation(
    //                 &redelegation,
    //                 &slashes,
    //                 total_redelegated_unbonded,
    //                 redelegated_balances,
    //             );
    //         }
    //     }
    // }

    // fn compute_remainder_redelegation(
    //     &self,
    //     redelegation: &crate::types::Redelegation,
    //     slashes: &[Slash],
    //     total_redelegated_unbonded: &AbstractTotalRedelegatedUnbonded,
    //     redelegated_balances: &mut BTreeMap<Epoch, token::Change>,
    // ) {
    //     println!("\nCOMPUTE REMAINDER REDELEGATION");
    //     dbg!(&slashes);
    //     let mut total = token::Change::zero();
    //     for epoch in Epoch::iter_bounds_inclusive(
    //         redelegation.redel_bond_start,
    //         self.pipeline(),
    //     ) {
    //         println!("ep {}", epoch);
    //         let updated_total_unbonded = if let Some(tot_redel_unbonded) =
    //             total_redelegated_unbonded.get(&epoch)
    //         {
    //             if !Self::has_redelegation(
    //                 tot_redel_unbonded,
    //                 redelegation.bond_start,
    //                 redelegation.redel_bond_start,
    //                 &redelegation.src_validator,
    //             ) {
    //                 total
    //             } else {
    //                 total
    //                     + tot_redel_unbonded
    //                         .get(&redelegation.redel_bond_start)
    //                         .unwrap()
    //                         .get(&redelegation.src_validator)
    //                         .unwrap()
    //                         .get(&redelegation.bond_start)
    //                         .cloned()
    //                         .unwrap()
    //                         .change()
    //             }
    //         } else {
    //             total
    //         };
    //         dbg!(&updated_total_unbonded);

    //         if epoch <= self.epoch {
    //             total = updated_total_unbonded;
    //         } else {
    //             let list_slashes = slashes
    //                 .iter()
    //                 .filter(|&slash| {
    //                     self.params.in_redelegation_slashing_window(
    //                         slash.epoch,
    //                         self.params.redelegation_end_epoch_from_start(
    //                             redelegation.redel_bond_start,
    //                         ),
    //                         redelegation.redel_bond_start,
    //                     ) && redelegation.bond_start <= slash.epoch
    //                 })
    //                 .cloned()
    //                 .collect::<Vec<_>>();
    //             dbg!(&list_slashes);
    //             let stake_left = Self::apply_slashes_to_amount(
    //                 &self.params,
    //                 &list_slashes,
    //                 redelegation.amount.change() - updated_total_unbonded,
    //             );
    //             dbg!(&stake_left);

    //             total = updated_total_unbonded;
    //             let cur_bal = redelegated_balances.entry(epoch).or_default();
    //             *cur_bal += stake_left;
    //         }
    //     }
    // }

    // fn compute_recent_total_unbonded(
    //     infraction_epoch: Epoch,
    //     total_unbonded: &BTreeMap<Epoch, token::Amount>,
    //     total_redelegated_unbonded: &BTreeMap<
    //         Epoch,
    //         BTreeMap<Address, BTreeMap<Epoch, token::Amount>>,
    //     >,
    // ) -> token::Change {
    //     println!("\nCOMPUTE RECENT TOTAL UNBONDED");
    //     dbg!(&total_unbonded);
    //     dbg!(&total_redelegated_unbonded);
    //     println!("NOW ITERATING OVER TOTAL_UNBONDED");
    //     total_unbonded.iter().fold(
    //         token::Change::zero(),
    //         |acc, (start_epoch, amount)| {
    //             println!(
    //                 "Epoch {}, Amt {}",
    //                 start_epoch,
    //                 amount.to_string_native()
    //             );
    //             if *start_epoch > infraction_epoch
    //                 && *amount > token::Amount::zero()
    //             {
    //                 let non_redelegated_amount = if let Some(redel_unbonds) =
    //                     total_redelegated_unbonded.get(start_epoch)
    //                 {
    //                     *amount
    //                         - redel_unbonds.iter().fold(
    //                           token::Amount::zero(), |sum, (_, unbonded_map)|
    //                           { sum + unbonded_map .iter() .map(|(_epoch,
    //                           amount)| *amount) .sum::<token::Amount>() },
    //                         )
    //                 } else {
    //                     *amount
    //                 };
    //                 dbg!(&non_redelegated_amount.change());

    //                 acc + non_redelegated_amount.change()
    //             } else {
    //                 dbg!(token::Change::zero());
    //                 acc
    //             }
    //         },
    //     )
    // }

    fn fold_and_slash_redelegated_bonds(
        &self,
        redel_bonds: &BTreeMap<Address, BTreeMap<Epoch, token::Amount>>,
        start: Epoch,
        list_slashes: &[Slash],
        slash_epoch_filter: impl Fn(Epoch) -> bool,
    ) -> FoldRedelegatedBondsResult {
        let mut result = FoldRedelegatedBondsResult::default();
        for (src_validator, bonds) in redel_bonds {
            for (bond_start, bonded) in bonds {
                let src_slashes = self
                    .validator_slashes
                    .get(src_validator)
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter(|&s| {
                        self.params.in_redelegation_slashing_window(
                            s.epoch,
                            self.params
                                .redelegation_start_epoch_from_end(start),
                            start,
                        ) && *bond_start <= s.epoch
                            && slash_epoch_filter(s.epoch)
                    })
                    .cloned()
                    .collect::<Vec<_>>();

                let mut merged = list_slashes
                    .iter()
                    .chain(src_slashes.iter())
                    .cloned()
                    .collect::<Vec<_>>();
                merged
                    .sort_by(|s1, s2| s1.epoch.partial_cmp(&s2.epoch).unwrap());

                result.total_redelegated += *bonded;
                result.total_after_slashing +=
                    token::Amount::from(Self::apply_slashes_to_amount(
                        &self.params,
                        &merged,
                        bonded.change(),
                    ));
            }
        }
        result
    }

    fn compute_bond_at_epoch(
        &self,
        epoch: Epoch,
        start: Epoch,
        amount: token::Amount,
        redel_bonds: &BTreeMap<Address, BTreeMap<Epoch, token::Amount>>,
        validator: &Address,
    ) -> token::Change {
        // `val list_slashes`
        let list_slashes = self
            .validator_slashes
            .get(validator)
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter(|&slash| {
                // TODO: check bounds!
                start <= slash.epoch
                    && slash.epoch + self.params.slash_processing_epoch_offset()
                        <= epoch
            })
            .cloned()
            .collect::<Vec<_>>();

        // `val filteredSlashMap` and `val resultFold`
        // `fold_and_slash_redelegated_bonds`
        let slash_epoch_filter =
            |e: Epoch| e + self.params.slash_processing_epoch_offset() <= epoch;
        let result_fold = self.fold_and_slash_redelegated_bonds(
            redel_bonds,
            start,
            &list_slashes,
            slash_epoch_filter,
        );

        // `val totalNoRedelegated`
        let total_not_redelegated = amount - result_fold.total_redelegated;
        // `val afterNoRedelegated`
        let after_not_redelegated = Self::apply_slashes_to_amount(
            &self.params,
            &list_slashes,
            total_not_redelegated.change(),
        );

        after_not_redelegated + result_fold.total_after_slashing.change()
    }

    #[allow(clippy::too_many_arguments)]
    fn compute_slash_bond_at_epoch(
        &self,
        epoch: Epoch,
        infraction_epoch: Epoch,
        bond_start: Epoch,
        bond_amount: token::Amount,
        redel_bonds: &BTreeMap<Address, BTreeMap<Epoch, token::Amount>>,
        slash_rate: Dec,
        validator: &Address,
    ) -> token::Change {
        let amount_due = self
            .compute_bond_at_epoch(
                infraction_epoch,
                bond_start,
                bond_amount,
                redel_bonds,
                validator,
            )
            .mul_ceil(slash_rate);
        let slashable_amount = self.compute_bond_at_epoch(
            epoch,
            bond_start,
            bond_amount,
            redel_bonds,
            validator,
        );

        cmp::min(amount_due, slashable_amount)
    }

    fn slash_validator_redelegation(
        &self,
        validator: &Address,
        dest_validator: &Address,
        slash_rate: Dec,
        slash_amounts: &mut BTreeMap<Epoch, token::Change>,
    ) {
        tracing::debug!("\nSLASHING VAL REDELEGATIONS\n");

        let infraction_epoch =
            self.epoch - self.params.slash_processing_epoch_offset();

        let dest_total_redelegated_unbonded = self
            .validator_total_redelegated_unbonded
            .get(dest_validator)
            .cloned()
            .unwrap_or_default();
        let validator_slashes = self
            .validator_slashes
            .get(validator)
            .cloned()
            .unwrap_or_default();

        // Loop over outgoing redelegations of validator -> dest_validator
        let outgoing_redelegations = if let Some(outgoing_redels) =
            self.outgoing_redelegations.get(validator)
        {
            outgoing_redels
                .get(dest_validator)
                .cloned()
                .unwrap_or_default()
        } else {
            BTreeMap::<(Epoch, Epoch), token::Amount>::new()
        };

        // dbg!(&slash_amounts);

        if !outgoing_redelegations.is_empty() {
            tracing::debug!("OUTGOING REDELEGATIONS:");
        }
        for ((src_start_epoch, redel_start), amount) in outgoing_redelegations {
            if self.params.in_redelegation_slashing_window(
                infraction_epoch,
                redel_start,
                self.params.redelegation_end_epoch_from_start(redel_start),
            ) && src_start_epoch <= infraction_epoch
            {
                tracing::debug!(
                    "bond_start = {}, redel_start = {}",
                    src_start_epoch,
                    redel_start
                );
                self.slash_redelegation(
                    amount.change(),
                    src_start_epoch,
                    self.params.redelegation_end_epoch_from_start(redel_start),
                    validator,
                    slash_rate,
                    &validator_slashes,
                    &dest_total_redelegated_unbonded,
                    slash_amounts,
                );
                // dbg!(&slash_amounts);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn slash_redelegation(
        &self,
        amount: token::Change,
        bond_start: Epoch,
        redel_bond_start: Epoch,
        src_validator: &Address,
        slash_rate: Dec,
        slashes: &[Slash],
        dest_total_redelegated_unbonded: &AbstractTotalRedelegatedUnbonded,
        slash_amounts: &mut BTreeMap<Epoch, token::Change>,
    ) {
        tracing::debug!(
            "\nSLASH REDELEGATION OF BOND START {bond_start} and \
             redel_bond_start {redel_bond_start}\n"
        );
        // dbg!(&slashes);

        let infraction_epoch =
            self.epoch - self.params.slash_processing_epoch_offset();

        // Slash redelegation destination validator from the next epoch only
        // as they won't be jailed
        let set_update_epoch = self.epoch.next();

        // Do initial computation of total unbonded
        let mut tot_unbonded = token::Change::zero();
        for epoch in Epoch::iter_bounds_inclusive(
            infraction_epoch.next(),
            set_update_epoch,
        ) {
            let total_redelegated_unbonded =
                dest_total_redelegated_unbonded.get(&epoch);
            if let Some(tot_redel_unbonded) = total_redelegated_unbonded {
                if Self::has_redelegation(
                    tot_redel_unbonded,
                    bond_start,
                    redel_bond_start,
                    src_validator,
                ) {
                    tot_unbonded += tot_redel_unbonded
                        .get(&redel_bond_start)
                        .unwrap()
                        .get(src_validator)
                        .unwrap()
                        .get(&bond_start)
                        .cloned()
                        .unwrap()
                        .change();
                }
            }
        }
        // dbg!(&slash_amounts);
        tracing::debug!("init_tot_unbonded = {}", tot_unbonded);

        for epoch in
            Epoch::iter_range(set_update_epoch, self.params.pipeline_len)
        {
            tracing::debug!("\nEpoch: {}", epoch);
            let total_redelegated_unbonded = dest_total_redelegated_unbonded
                .get(&epoch)
                .cloned()
                .unwrap_or_default();
            let updated_total_unbonded = if !Self::has_redelegation(
                &total_redelegated_unbonded,
                bond_start,
                redel_bond_start,
                src_validator,
            ) {
                tot_unbonded
            } else {
                tot_unbonded
                    + total_redelegated_unbonded
                        .get(&redel_bond_start)
                        .unwrap()
                        .get(src_validator)
                        .unwrap()
                        .get(&bond_start)
                        .cloned()
                        .unwrap()
                        .change()
            };
            tracing::debug!(
                "updated_total_unbonded = {}",
                updated_total_unbonded
            );
            let list_slashes = slashes
                .iter()
                .filter(|&slash| {
                    self.params.in_redelegation_slashing_window(
                        slash.epoch,
                        self.params.redelegation_start_epoch_from_end(
                            redel_bond_start,
                        ),
                        redel_bond_start,
                    ) && bond_start <= slash.epoch
                        && slash.epoch
                            + self.params.slash_processing_epoch_offset()
                            <= infraction_epoch
                })
                .cloned()
                .collect::<Vec<_>>();
            // dbg!(&list_slashes);

            let slashable_amount = cmp::max(
                token::Change::zero(),
                amount - updated_total_unbonded,
            );
            let slashed = Self::apply_slashes_to_amount(
                &self.params,
                &list_slashes,
                slashable_amount,
            )
            .mul_ceil(slash_rate);
            tracing::debug!("slashed = {}", slashed);
            let list_slashes = slashes
                .iter()
                .filter(|&slash| {
                    // dbg!(&slash);
                    self.params.in_redelegation_slashing_window(
                        slash.epoch,
                        self.params.redelegation_start_epoch_from_end(
                            redel_bond_start,
                        ),
                        redel_bond_start,
                    ) && bond_start <= slash.epoch
                })
                .cloned()
                .collect::<Vec<_>>();
            // dbg!(&list_slashes);
            // dbg!(&slash_rate);

            let slashable_stake = Self::apply_slashes_to_amount(
                &self.params,
                &list_slashes,
                slashable_amount,
            )
            .mul_ceil(slash_rate);

            tot_unbonded = updated_total_unbonded;
            tracing::debug!("slashable stake = {}", slashable_stake);

            let to_slash = cmp::min(slashed, slashable_stake);
            if !to_slash.is_zero() {
                let slashed_amt = slash_amounts.entry(epoch).or_default();
                // dbg!(&slashed_amt);

                *slashed_amt += to_slash;
            }
        }
        // dbg!(&slash_amounts);
    }

    fn has_redelegation(
        total_redelegated_unbonded: &BTreeMap<
            Epoch,
            BTreeMap<Address, BTreeMap<Epoch, token::Amount>>,
        >,
        bond_start: Epoch,
        redel_start: Epoch,
        src_validator: &Address,
    ) -> bool {
        if let Some(redel_unbonded) =
            total_redelegated_unbonded.get(&redel_start)
        {
            if let Some(unbonded) = redel_unbonded.get(src_validator) {
                if unbonded.contains_key(&bond_start) {
                    return true;
                }
            }
        }
        false
    }

    // fn slash_a_validator(
    //     &mut self,
    //     validator: &Address,
    //     slashes: &[Slash],
    //     slashable_stake: token::Change,
    //     infraction_epoch: Epoch,
    //     cubic_rate: Dec,
    // ) {
    //     tracing::debug!(
    //         "Val {} slashable stake at infraction {}",
    //         validator,
    //         slashable_stake.to_string_native(),
    //     );

    //     let mut total_rate = Dec::zero();

    //     for slash in slashes {
    //         debug_assert_eq!(slash.epoch, infraction_epoch);
    //         let rate =
    //             cmp::max(slash.r#type.get_slash_rate(&self.params),
    // cubic_rate);         let processed_slash = Slash {
    //             epoch: slash.epoch,
    //             block_height: slash.block_height,
    //             r#type: slash.r#type,
    //             rate,
    //         };
    //         let cur_slashes =
    //             self.validator_slashes.entry(validator.clone()).or_default();
    //         cur_slashes.push(processed_slash.clone());

    //         total_rate += rate;
    //     }
    //     total_rate = cmp::min(total_rate, Dec::one());
    //     tracing::debug!("Total rate: {}", total_rate);

    //     let mut total_unbonded = token::Amount::zero();
    //     let mut sum_post_bonds = token::Change::zero();

    //     for epoch in (infraction_epoch.0 + 1)..self.epoch.0 {
    //         tracing::debug!("\nEpoch {}", epoch);
    //         let mut recent_unbonds = token::Change::zero();
    //         let unbond_records = self
    //             .total_unbonded
    //             .entry(validator.clone())
    //             .or_default()
    //             .get(&Epoch(epoch))
    //             .cloned()
    //             .unwrap_or_default();
    //         for (start, unbond_amount) in unbond_records {
    //             tracing::debug!(
    //                 "UnbondRecord: amount = {}, start_epoch {}",
    //                 &unbond_amount.to_string_native(),
    //                 &start
    //             );
    //             if start <= infraction_epoch {
    //                 let slashes_for_this_unbond = self
    //                     .validator_slashes
    //                     .get(validator)
    //                     .cloned()
    //                     .unwrap_or_default()
    //                     .iter()
    //                     .filter(|&s| {
    //                         start <= s.epoch
    //                             && s.epoch
    //                                 + self.params.unbonding_len
    //                                 +
    // self.params.cubic_slashing_window_length
    // < infraction_epoch                     })
    //                     .cloned()
    //                     .fold(BTreeMap::<Epoch, Dec>::new(), |mut acc, s| {
    //                         let cur = acc.entry(s.epoch).or_default();
    //                         *cur += s.rate;
    //                         acc
    //                     });
    //                 tracing::debug!(
    //                     "Slashes for this unbond: {:?}",
    //                     slashes_for_this_unbond
    //                 );
    //                 total_unbonded += compute_amount_after_slashing(
    //                     &slashes_for_this_unbond,
    //                     unbond_amount,
    //                     self.params.unbonding_len,
    //                     self.params.cubic_slashing_window_length,
    //                 );
    //             } else {
    //                 recent_unbonds += unbond_amount.change();
    //             }

    //             tracing::debug!(
    //                 "Total unbonded (epoch {}) w slashing = {}",
    //                 epoch,
    //                 total_unbonded.to_string_native()
    //             );
    //         }
    //         sum_post_bonds += self
    //             .total_bonded
    //             .get(validator)
    //             .and_then(|bonded| bonded.get(&Epoch(epoch)))
    //             .cloned()
    //             .unwrap_or_default()
    //             - recent_unbonds;
    //     }
    //     tracing::debug!("Computing adjusted amounts now");

    //     let mut last_slash = token::Change::zero();
    //     for offset in 0..self.params.pipeline_len {
    //         tracing::debug!(
    //             "Epoch {}\nLast slash = {}",
    //             self.epoch + offset,
    //             last_slash.to_string_native(),
    //         );
    //         let mut recent_unbonds = token::Change::zero();
    //         let unbond_records = self
    //             .total_unbonded
    //             .get(validator)
    //             .unwrap()
    //             .get(&(self.epoch + offset))
    //             .cloned()
    //             .unwrap_or_default();
    //         for (start, unbond_amount) in unbond_records {
    //             tracing::debug!(
    //                 "UnbondRecord: amount = {}, start_epoch {}",
    //                 unbond_amount.to_string_native(),
    //                 &start
    //             );
    //             if start <= infraction_epoch {
    //                 let slashes_for_this_unbond = self
    //                     .validator_slashes
    //                     .get(validator)
    //                     .cloned()
    //                     .unwrap_or_default()
    //                     .iter()
    //                     .filter(|&s| {
    //                         start <= s.epoch
    //                             && s.epoch
    //                                 + self.params.unbonding_len
    //                                 +
    // self.params.cubic_slashing_window_length
    // < infraction_epoch                     })
    //                     .cloned()
    //                     .fold(BTreeMap::<Epoch, Dec>::new(), |mut acc, s| {
    //                         let cur = acc.entry(s.epoch).or_default();
    //                         *cur += s.rate;
    //                         acc
    //                     });
    //                 tracing::debug!(
    //                     "Slashes for this unbond: {:?}",
    //                     slashes_for_this_unbond
    //                 );

    //                 total_unbonded += compute_amount_after_slashing(
    //                     &slashes_for_this_unbond,
    //                     unbond_amount,
    //                     self.params.unbonding_len,
    //                     self.params.cubic_slashing_window_length,
    //                 );
    //             } else {
    //                 recent_unbonds += unbond_amount.change();
    //             }

    //             tracing::debug!(
    //                 "Total unbonded (offset {}) w slashing = {}",
    //                 offset,
    //                 total_unbonded.to_string_native()
    //             );
    //         }
    //         tracing::debug!(
    //             "slashable stake {}, total unbonded {}",
    //             slashable_stake.to_string_native(),
    //             total_unbonded.to_string_native(),
    //         );
    //         let amount_to_slash = if slashable_stake >
    // total_unbonded.change() {             slashable_stake -
    // total_unbonded.change()         } else {
    //             token::Change::zero()
    //         };
    //         let this_slash = amount_to_slash.mul_ceil(total_rate);
    //         let diff_slashed_amount = last_slash - this_slash;
    //         tracing::debug!(
    //             "Offset {} diff_slashed_amount {}",
    //             offset,
    //             diff_slashed_amount.to_string_native(),
    //         );
    //         last_slash = this_slash;
    //         // total_unbonded = token::Amount::zero();

    //         // Update the voting powers (consider that the stake
    //         // is discrete) let
    //         // validator_stake = state
    //         //     .validator_stakes
    //         //     .entry(state.epoch + offset)
    //         //     .or_default()
    //         //     .entry(validator.clone())
    //         //     .or_default();
    //         // *validator_stake -= diff_slashed_amount;

    //         tracing::debug!("Updating ABSTRACT voting powers");
    //         sum_post_bonds += self
    //             .total_bonded
    //             .get(validator)
    //             .and_then(|bonded| bonded.get(&(self.epoch + offset)))
    //             .cloned()
    //             .unwrap_or_default()
    //             - recent_unbonds;

    //         tracing::debug!(
    //             "\nUnslashable bonds = {}",
    //             sum_post_bonds.to_string_native()
    //         );
    //         let validator_stake_at_offset = self
    //             .validator_stakes
    //             .entry(self.epoch + offset)
    //             .or_default()
    //             .entry(validator.clone())
    //             .or_default();

    //         let slashable_stake_at_offset =
    //             *validator_stake_at_offset - sum_post_bonds;
    //         tracing::debug!(
    //             "Val stake pre (epoch {}) = {}",
    //             self.epoch + offset,
    //             validator_stake_at_offset.to_string_native(),
    //         );
    //         tracing::debug!(
    //             "Slashable stake at offset = {}",
    //             slashable_stake_at_offset.to_string_native(),
    //         );
    //         let change =
    //             cmp::max(-slashable_stake_at_offset, diff_slashed_amount);

    //         tracing::debug!("Change = {}", change.to_string_native());
    //         *validator_stake_at_offset += change;

    //         for os in (offset + 1)..=self.params.pipeline_len {
    //             tracing::debug!("Adjust epoch {}", self.epoch + os);
    //             let offset_stake = self
    //                 .validator_stakes
    //                 .entry(self.epoch + os)
    //                 .or_default()
    //                 .entry(validator.clone())
    //                 .or_default();
    //             *offset_stake += change;
    //             // let mut new_stake =
    //             //     *validator_stake - diff_slashed_amount;
    //             // if new_stake < 0_i128 {
    //             //     new_stake = 0_i128;
    //             // }

    //             // *validator_stake = new_stake;
    //             tracing::debug!(
    //                 "New stake at epoch {} = {}",
    //                 self.epoch + os,
    //                 offset_stake.to_string_native()
    //             );
    //         }
    //     }
    // }

    /// Get the pipeline epoch
    fn pipeline(&self) -> Epoch {
        self.epoch + self.params.pipeline_len
    }

    /// Check if the given address is of a known validator
    fn is_validator(&self, validator: &Address, epoch: Epoch) -> bool {
        // let is_in_consensus = self
        //     .consensus_set
        //     .get(&epoch)
        //     .unwrap()
        //     .iter()
        //     .any(|(_stake, vals)| vals.iter().any(|val| val == validator));
        // if is_in_consensus {
        //     return true;
        // }
        // self.below_capacity_set
        //     .get(&epoch)
        //     .unwrap()
        //     .iter()
        //     .any(|(_stake, vals)| vals.iter().any(|val| val == validator))

        self.validator_states
            .get(&epoch)
            .unwrap()
            .keys()
            .any(|val| val == validator)
    }

    fn is_in_consensus_w_info(
        &self,
        validator: &Address,
        epoch: Epoch,
    ) -> Option<(usize, token::Amount)> {
        for (stake, vals) in self.consensus_set.get(&epoch).unwrap() {
            if let Some(index) = vals.iter().position(|val| val == validator) {
                return Some((index, *stake));
            }
        }
        None
    }

    fn is_in_below_capacity_w_info(
        &self,
        validator: &Address,
        epoch: Epoch,
    ) -> Option<(usize, token::Amount)> {
        for (stake, vals) in self.below_capacity_set.get(&epoch).unwrap() {
            if let Some(index) = vals.iter().position(|val| val == validator) {
                return Some((index, (*stake).into()));
            }
        }
        None
    }

    fn is_in_below_threshold(&self, validator: &Address, epoch: Epoch) -> bool {
        self.below_threshold_set
            .get(&epoch)
            .unwrap()
            .iter()
            .any(|val| val == validator)
    }

    /// Find the sums of the bonds across all epochs
    fn bond_sums(&self) -> BTreeMap<BondId, token::Change> {
        self.bonds.iter().fold(
            BTreeMap::<BondId, token::Change>::new(),
            |mut acc, (id, bonds)| {
                for delta in bonds.values() {
                    let entry = acc.entry(id.clone()).or_default();
                    *entry += *delta;
                }
                acc
            },
        )
    }

    /// Find the sums of withdrawable unbonds
    fn withdrawable_unbonds(&self) -> BTreeMap<BondId, token::Amount> {
        self.unbonds.iter().fold(
            BTreeMap::<BondId, token::Amount>::new(),
            |mut acc, ((_start_epoch, withdraw_epoch), unbonds)| {
                if *withdraw_epoch <= self.epoch {
                    for (id, amount) in unbonds {
                        if *amount > token::Amount::zero() {
                            *acc.entry(id.clone()).or_default() += *amount;
                        }
                    }
                }
                acc
            },
        )
    }

    /// Compute the cubic slashing rate for the current epoch
    fn cubic_slash_rate(&self) -> Dec {
        let infraction_epoch =
            self.epoch - self.params.slash_processing_epoch_offset();
        tracing::debug!(
            "Infraction epoch: {}, Current epoch: {}",
            infraction_epoch,
            self.epoch
        );
        let window_width = self.params.cubic_slashing_window_length;
        let epoch_start = Epoch::from(
            infraction_epoch
                .0
                .checked_sub(window_width)
                .unwrap_or_default(),
        );
        let epoch_end = infraction_epoch + window_width;

        // Calculate cubic slashing rate with the abstract state
        let mut vp_frac_sum = Dec::zero();
        for epoch in Epoch::iter_bounds_inclusive(epoch_start, epoch_end) {
            let consensus_stake =
                self.consensus_set.get(&epoch).unwrap().iter().fold(
                    token::Amount::zero(),
                    |sum, (val_stake, validators)| {
                        sum + *val_stake * validators.len() as u64
                    },
                );
            tracing::debug!(
                "Consensus stake in epoch {}: {}",
                epoch,
                consensus_stake.to_string_native()
            );

            let processing_epoch = epoch
                + self.params.unbonding_len
                + 1_u64
                + self.params.cubic_slashing_window_length;
            let enqueued_slashes = self.enqueued_slashes.get(&processing_epoch);
            if let Some(enqueued_slashes) = enqueued_slashes {
                for (validator, slashes) in enqueued_slashes.iter() {
                    let val_stake = token::Amount::from_change(
                        self.validator_stakes
                            .get(&epoch)
                            .unwrap()
                            .get(validator)
                            .cloned()
                            .unwrap_or_default(),
                    );
                    tracing::debug!(
                        "Val {} stake epoch {}: {}",
                        &validator,
                        epoch,
                        val_stake.to_string_native(),
                    );
                    vp_frac_sum += Dec::from(slashes.len())
                        * Dec::from(val_stake)
                        / Dec::from(consensus_stake);
                }
            }
        }
        let vp_frac_sum = cmp::min(Dec::one(), vp_frac_sum);
        tracing::debug!("vp_frac_sum: {}", vp_frac_sum);

        cmp::min(
            Dec::new(9, 0).unwrap() * vp_frac_sum * vp_frac_sum,
            Dec::one(),
        )
    }

    fn debug_validators(&self) {
        tracing::debug!("DEBUG ABSTRACT VALIDATOR");
        let current_epoch = self.epoch;
        for epoch in
            Epoch::iter_bounds_inclusive(current_epoch, self.pipeline())
        {
            tracing::debug!("Epoch {}", epoch);
            let mut min_consensus = token::Amount::from(u64::MAX);
            let consensus = self.consensus_set.get(&epoch).unwrap();
            for (amount, vals) in consensus {
                if *amount < min_consensus {
                    min_consensus = *amount;
                }
                for val in vals {
                    let deltas_stake = self
                        .validator_stakes
                        .get(&epoch)
                        .unwrap()
                        .get(val)
                        .unwrap();
                    let val_state = self
                        .validator_states
                        .get(&epoch)
                        .unwrap()
                        .get(val)
                        .unwrap();
                    tracing::debug!(
                        "Consensus val {}, stake {} ({}) - ({:?})",
                        val,
                        amount.to_string_native(),
                        deltas_stake.to_string_native(),
                        val_state
                    );
                    debug_assert_eq!(
                        *amount,
                        token::Amount::from_change(*deltas_stake)
                    );
                    debug_assert_eq!(*val_state, ValidatorState::Consensus);
                }
            }
            let mut max_bc = token::Amount::zero();
            let bc = self.below_capacity_set.get(&epoch).unwrap();
            for (amount, vals) in bc {
                if token::Amount::from(*amount) > max_bc {
                    max_bc = token::Amount::from(*amount);
                }
                for val in vals {
                    let deltas_stake = self
                        .validator_stakes
                        .get(&epoch)
                        .unwrap()
                        .get(val)
                        .cloned()
                        .unwrap_or_default();
                    let val_state = self
                        .validator_states
                        .get(&epoch)
                        .unwrap()
                        .get(val)
                        .unwrap();
                    tracing::debug!(
                        "Below-cap val {}, stake {} ({}) - ({:?})",
                        val,
                        token::Amount::from(*amount).to_string_native(),
                        deltas_stake.to_string_native(),
                        val_state
                    );
                    debug_assert_eq!(
                        token::Amount::from(*amount),
                        token::Amount::from_change(deltas_stake)
                    );
                    debug_assert_eq!(*val_state, ValidatorState::BelowCapacity);
                }
            }
            if max_bc > min_consensus {
                tracing::debug!(
                    "min_consensus = {}, max_bc = {}",
                    min_consensus.to_string_native(),
                    max_bc.to_string_native()
                );
            }
            assert!(min_consensus >= max_bc);

            for addr in self.below_threshold_set.get(&epoch).unwrap() {
                let state = self
                    .validator_states
                    .get(&epoch)
                    .unwrap()
                    .get(addr)
                    .unwrap();

                let stake = self
                    .validator_stakes
                    .get(&epoch)
                    .unwrap()
                    .get(addr)
                    .cloned()
                    .unwrap_or_default();
                tracing::debug!(
                    "Below-thresh val {}, stake {} - ({:?})",
                    addr,
                    stake.to_string_native(),
                    state
                );

                assert_eq!(*state, ValidatorState::BelowThreshold);
            }

            for addr in self
                .validator_states
                .get(&epoch)
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>()
            {
                if let (None, None, false) = (
                    self.is_in_consensus_w_info(&addr, epoch),
                    self.is_in_below_capacity_w_info(&addr, epoch),
                    self.is_in_below_threshold(&addr, epoch),
                ) {
                    assert_eq!(
                        self.validator_states
                            .get(&epoch)
                            .unwrap()
                            .get(&addr)
                            .cloned(),
                        Some(ValidatorState::Jailed)
                    );
                    let stake = self
                        .validator_stakes
                        .get(&epoch)
                        .unwrap()
                        .get(&addr)
                        .cloned()
                        .unwrap_or_default();
                    tracing::debug!(
                        "Jailed val {}, stake {}",
                        &addr,
                        &stake.to_string_native()
                    );
                }
            }
        }
    }

    fn is_chained_redelegation(
        current_epoch: Epoch,
        params: &PosParams,
        incoming_redelegations: &AbstractIncomingRedelegations,
        delegator: &Address,
        src_validator: &Address,
    ) -> bool {
        let src_incoming_redelegations =
            incoming_redelegations.get(src_validator);
        if let Some(incoming) = src_incoming_redelegations {
            if let Some(redel_end_epoch) = incoming.get(delegator) {
                return redel_end_epoch.prev()
                    + params.slash_processing_epoch_offset()
                    > current_epoch;
            }
        }
        false
    }

    fn find_bonds_to_remove(
        bonds: &BTreeMap<Epoch, token::Change>,
        amount: token::Change,
    ) -> BondsForRemovalRes {
        let mut bonds_for_removal = BondsForRemovalRes::default();
        let mut remaining = amount;

        for (&bond_epoch, &bond_amount) in bonds.iter().rev() {
            let to_unbond = cmp::min(bond_amount, remaining);
            if to_unbond == bond_amount {
                bonds_for_removal.epochs.insert(bond_epoch);
            } else {
                bonds_for_removal.new_entry =
                    Some((bond_epoch, bond_amount - to_unbond));
            }
            remaining -= to_unbond;
            if remaining.is_zero() {
                break;
            }
        }
        bonds_for_removal
    }

    fn compute_modified_redelegation(
        delegator_redelegated_bonds: &mut BTreeMap<
            Epoch,
            BTreeMap<Address, BTreeMap<Epoch, token::Change>>,
        >,
        bond_epoch: Epoch,
        amount: token::Change,
    ) -> ModifiedRedelegation {
        let mut modified_redelegation = ModifiedRedelegation::default();

        let redelegated_bonds =
            delegator_redelegated_bonds.entry(bond_epoch).or_default();
        let (src_validators, total_redelegated) =
            redelegated_bonds.iter().fold(
                (HashSet::<Address>::new(), token::Change::zero()),
                |mut acc, (src_val, redel_bonds)| {
                    acc.0.insert(src_val.clone());
                    acc.1 += redel_bonds
                        .values()
                        .fold(token::Change::zero(), |sum, val| sum + *val);
                    acc
                },
            );

        modified_redelegation.epoch = Some(bond_epoch);

        if total_redelegated <= amount {
            return modified_redelegation;
        }

        let mut remaining = amount;
        for src_val in src_validators {
            if remaining.is_zero() {
                break;
            }
            let bonds = redelegated_bonds.get(&src_val).unwrap();
            let total_src_amount =
                bonds.values().cloned().sum::<token::Change>();

            modified_redelegation
                .validators_to_remove
                .insert(src_val.clone());

            if total_src_amount <= remaining {
                remaining -= total_src_amount;
            } else {
                let src_bonds_to_remove =
                    Self::find_bonds_to_remove(bonds, remaining);

                remaining = token::Change::zero();

                if let Some((bond_epoch, new_bond_amount)) =
                    src_bonds_to_remove.new_entry
                {
                    modified_redelegation.validator_to_modify = Some(src_val);
                    modified_redelegation.epochs_to_remove = {
                        let mut epochs = src_bonds_to_remove.epochs;
                        epochs.insert(bond_epoch);
                        epochs
                    };
                    modified_redelegation.epoch_to_modify = Some(bond_epoch);
                    modified_redelegation.new_amount = Some(new_bond_amount);
                } else {
                    modified_redelegation.validator_to_modify = Some(src_val);
                    modified_redelegation.epochs_to_remove =
                        src_bonds_to_remove.epochs;
                }
            }
        }

        modified_redelegation
    }

    fn compute_new_redelegated_unbonds(
        redelegated_bonds: &mut BTreeMap<
            Epoch,
            BTreeMap<Address, BTreeMap<Epoch, token::Change>>,
        >,
        epochs_to_remove: &BTreeSet<Epoch>,
        modified_redelegation: &ModifiedRedelegation,
    ) -> BTreeMap<Epoch, BTreeMap<Address, BTreeMap<Epoch, token::Change>>>
    {
        let unbonded_epochs = if let Some(epoch) = modified_redelegation.epoch {
            let mut epochs = epochs_to_remove.clone();
            epochs.insert(epoch);
            epochs
                .iter()
                .cloned()
                .filter(|e| redelegated_bonds.contains_key(e))
                .collect::<BTreeSet<Epoch>>()
        } else {
            epochs_to_remove
                .iter()
                .cloned()
                .filter(|e| redelegated_bonds.contains_key(e))
                .collect::<BTreeSet<Epoch>>()
        };

        let new_redelegated_unbonds: EagerRedelegatedUnbonds = unbonded_epochs
            .into_iter()
            .map(|start| {
                let mut rbonds = EagerRedelegatedBondsMap::default();
                if modified_redelegation
                    .epoch
                    .map(|redelegation_epoch| start != redelegation_epoch)
                    .unwrap_or(true)
                    || modified_redelegation.validators_to_remove.is_empty()
                {
                    for (src_val, bonds) in
                        redelegated_bonds.get(&start).unwrap()
                    {
                        for (bond_epoch, bond_amount) in bonds {
                            rbonds
                                .entry(src_val.clone())
                                .or_default()
                                .insert(*bond_epoch, *bond_amount);
                        }
                    }
                    (start, rbonds)
                } else {
                    for src_validator in
                        &modified_redelegation.validators_to_remove
                    {
                        if modified_redelegation
                            .validator_to_modify
                            .as_ref()
                            .map(|validator| src_validator != validator)
                            .unwrap_or(true)
                        {
                            let raw_bonds = redelegated_bonds
                                .entry(start)
                                .or_default()
                                .entry(src_validator.clone())
                                .or_default();
                            for (bond_epoch, bond_amount) in raw_bonds {
                                rbonds
                                    .entry(src_validator.clone())
                                    .or_default()
                                    .insert(*bond_epoch, *bond_amount);
                            }
                        } else {
                            for bond_start in
                                &modified_redelegation.epochs_to_remove
                            {
                                let cur_redel_bond_amount = redelegated_bonds
                                    .entry(start)
                                    .or_default()
                                    .entry(src_validator.clone())
                                    .or_default()
                                    .entry(*bond_start)
                                    .or_default();

                                let raw_bonds = rbonds
                                    .entry(src_validator.clone())
                                    .or_default();
                                if modified_redelegation
                                    .epoch_to_modify
                                    .as_ref()
                                    .map(|epoch| bond_start != epoch)
                                    .unwrap_or(true)
                                {
                                    raw_bonds.insert(
                                        *bond_start,
                                        *cur_redel_bond_amount,
                                    );
                                } else {
                                    raw_bonds.insert(
                                        *bond_start,
                                        *cur_redel_bond_amount
                                            - modified_redelegation
                                                .new_amount
                                                // Safe unwrap - it shouldn't
                                                // get to
                                                // this if it's None
                                                .unwrap(),
                                    );
                                }
                            }
                        }
                    }
                    (start, rbonds)
                }
            })
            .collect();
        new_redelegated_unbonds
    }

    fn compute_amount_after_slashing_unbond(
        params: &PosParams,
        all_slashes: &BTreeMap<Address, Vec<Slash>>,
        validator: &Address,
        new_unbonds: &BTreeMap<(Epoch, Epoch), token::Change>,
        new_redelegated_unbonded: &BTreeMap<
            Epoch,
            BTreeMap<Address, BTreeMap<Epoch, token::Change>>,
        >,
    ) -> ResultSlashing {
        let mut result_slashing = ResultSlashing::default();
        let validator_slashes =
            all_slashes.get(validator).cloned().unwrap_or_default();
        for ((start_epoch, _withdraw_epoch), to_unbond) in new_unbonds {
            let slashes = validator_slashes
                .iter()
                .filter(|&s| s.epoch >= *start_epoch)
                .cloned()
                .collect::<Vec<_>>();

            // Begin the logic for `fold_and_slash_redelegated_bonds`
            let result_fold = {
                let (mut total_redelegated, mut total_after_slashing) =
                    (token::Change::zero(), token::Change::zero());

                for (src_validator, unbonded_map) in new_redelegated_unbonded
                    .get(start_epoch)
                    .cloned()
                    .unwrap_or_default()
                {
                    for (bond_start, unbonded) in unbonded_map {
                        let src_slashes = all_slashes
                            .get(&src_validator)
                            .cloned()
                            .unwrap_or_default()
                            .iter()
                            .filter(|&s| {
                                params.in_redelegation_slashing_window(
                                    s.epoch,
                                    params.redelegation_start_epoch_from_end(
                                        *start_epoch,
                                    ),
                                    *start_epoch,
                                ) && bond_start <= s.epoch
                            })
                            .cloned()
                            .collect::<Vec<_>>();

                        let mut merged = slashes
                            .iter()
                            .chain(src_slashes.iter())
                            .cloned()
                            .collect::<Vec<_>>();
                        merged.sort_by(|s1, s2| {
                            s1.epoch.partial_cmp(&s2.epoch).unwrap()
                        });

                        total_redelegated += unbonded;
                        total_after_slashing += Self::apply_slashes_to_amount(
                            params, &merged, unbonded,
                        );
                    }
                }

                FoldRedelegatedBondsResult {
                    total_redelegated: token::Amount::from(total_redelegated),
                    total_after_slashing: token::Amount::from(
                        total_after_slashing,
                    ),
                }
            };

            let total_not_redelegated =
                *to_unbond - result_fold.total_redelegated.change();
            let after_not_redelegated = Self::apply_slashes_to_amount(
                params,
                &slashes,
                total_not_redelegated,
            );
            let amount_after_slashing = after_not_redelegated
                + result_fold.total_after_slashing.change();
            result_slashing.sum += token::Amount::from(amount_after_slashing);
            result_slashing.epoch_map.insert(
                *start_epoch,
                token::Amount::from(amount_after_slashing),
            );
        }

        result_slashing
    }

    fn apply_slashes_to_amount(
        params: &PosParams,
        slashes: &[Slash],
        amount: token::Change,
    ) -> token::Change {
        let mut final_amount = amount;
        let mut computed_slashes = BTreeMap::<Epoch, token::Change>::new();
        for slash in slashes {
            let slashed_amount = Self::compute_slashable_amount(
                params,
                slash,
                amount,
                &computed_slashes,
            );
            final_amount =
                cmp::max(token::Change::zero(), final_amount - slashed_amount);
            computed_slashes.insert(slash.epoch, slashed_amount);
        }
        final_amount
    }

    fn compute_slashable_amount(
        params: &PosParams,
        slash: &Slash,
        amount: token::Change,
        computed_slashes: &BTreeMap<Epoch, token::Change>,
    ) -> token::Change {
        let updated_amount = computed_slashes
            .iter()
            .filter(|(&epoch, _)| {
                // TODO: check if bounds correct!
                // slashes that have already been applied and processed
                epoch + params.slash_processing_epoch_offset() <= slash.epoch
            })
            .fold(amount, |acc, (_, amnt)| {
                cmp::max(token::Change::zero(), acc - *amnt)
            });
        updated_amount.mul_ceil(slash.rate)
    }
}

/// Arbitrary bond transition that adds tokens to an existing bond
fn add_arb_bond_amount(
    state: &AbstractPosState,
) -> impl Strategy<Value = Transition> {
    let bond_ids = state
        .bonds
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let arb_bond_id = prop::sample::select(bond_ids);
    (arb_bond_id, arb_bond_amount())
        .prop_map(|(id, amount)| Transition::Bond { id, amount })
}

/// Arbitrary delegation to one of the validators
fn arb_delegation(
    state: &AbstractPosState,
) -> impl Strategy<Value = Transition> {
    // Bond is allowed to any validator in any set - including jailed validators
    let validators = state
        .validator_states
        .get(&state.pipeline())
        .unwrap()
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    let validator_vec = validators.clone().into_iter().collect::<Vec<_>>();
    let arb_source = address::testing::arb_non_internal_address()
        .prop_filter("Must be a non-validator address", move |addr| {
            !validators.contains(addr)
        });
    let arb_validator = prop::sample::select(validator_vec);
    (arb_source, arb_validator, arb_bond_amount()).prop_map(
        |(source, validator, amount)| Transition::Bond {
            id: BondId { source, validator },
            amount,
        },
    )
}

/// Arbitrary validator self-bond
fn arb_self_bond(
    state: &AbstractPosState,
) -> impl Strategy<Value = Transition> {
    // Bond is allowed to any validator in any set - including jailed validators
    let validator_vec = state
        .validator_states
        .get(&state.pipeline())
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let arb_validator = prop::sample::select(validator_vec);
    (arb_validator, arb_bond_amount()).prop_map(|(validator, amount)| {
        Transition::Bond {
            id: BondId {
                source: validator.clone(),
                validator,
            },
            amount,
        }
    })
}

// Bond up to 10 tokens (10M micro units) to avoid overflows
pub fn arb_bond_amount() -> impl Strategy<Value = token::Amount> {
    (1_u64..10_000_000)
        .prop_map(|val| token::Amount::from_uint(val, 0).unwrap())
}

/// Arbitrary validator misbehavior
fn arb_slash(state: &AbstractPosState) -> impl Strategy<Value = Transition> {
    let validators = state.consensus_set.iter().fold(
        Vec::new(),
        |mut acc, (_epoch, vals)| {
            for vals in vals.values() {
                for validator in vals {
                    acc.push(validator.clone());
                }
            }
            acc
        },
    );
    let current_epoch = state.epoch.0;

    let arb_validator = prop::sample::select(validators);
    let slash_types =
        vec![SlashType::LightClientAttack, SlashType::DuplicateVote];
    let arb_type = prop::sample::select(slash_types);
    let arb_epoch = (current_epoch
        .checked_sub(state.params.unbonding_len)
        .unwrap_or_default()..=current_epoch)
        .prop_map(Epoch::from);
    (arb_validator, arb_type, arb_epoch).prop_map(
        |(validator, slash_type, infraction_epoch)| Transition::Misbehavior {
            address: validator,
            slash_type,
            infraction_epoch,
            height: 0,
        },
    )
}

// fn compute_amount_after_slashing(
//     slashes: &BTreeMap<Epoch, Dec>,
//     amount: token::Amount,
//     unbonding_len: u64,
//     cubic_slash_window_len: u64,
// ) -> token::Amount {
//     let mut computed_amounts = Vec::<SlashedAmount>::new();
//     let mut updated_amount = amount;

//     for (&infraction_epoch, &slash_rate) in slashes {
//         let mut indices_to_remove = BTreeSet::<usize>::new();

//         for (idx, slashed_amount) in computed_amounts.iter().enumerate() {
//             if slashed_amount.epoch + unbonding_len + cubic_slash_window_len
//                 < infraction_epoch
//             {
//                 updated_amount = updated_amount
//                     .checked_sub(slashed_amount.amount)
//                     .unwrap_or_default();
//                 indices_to_remove.insert(idx);
//             }
//         }
//         for idx in indices_to_remove.into_iter().rev() {
//             computed_amounts.remove(idx);
//         }
//         computed_amounts.push(SlashedAmount {
//             amount: updated_amount.mul_ceil(slash_rate),
//             epoch: infraction_epoch,
//         });
//     }
//     updated_amount
//         .checked_sub(
//             computed_amounts
//                 .iter()
//                 .fold(token::Amount::zero(), |sum, computed| {
//                     sum + computed.amount
//                 }),
//         )
//         .unwrap_or_default()
// }
