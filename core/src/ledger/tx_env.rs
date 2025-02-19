//! Transaction environment contains functions that can be called from
//! inside a tx.

use borsh::BorshSerialize;

use crate::ledger::storage_api::{self, StorageRead, StorageWrite};
use crate::types::address::Address;
use crate::types::ibc::IbcEvent;
use crate::types::storage;

/// Transaction host functions
pub trait TxEnv: StorageRead + StorageWrite {
    /// Write a temporary value to be encoded with Borsh at the given key to
    /// storage.
    fn write_temp<T: BorshSerialize>(
        &mut self,
        key: &storage::Key,
        val: T,
    ) -> Result<(), storage_api::Error>;

    /// Write a temporary value as bytes at the given key to storage.
    fn write_bytes_temp(
        &mut self,
        key: &storage::Key,
        val: impl AsRef<[u8]>,
    ) -> Result<(), storage_api::Error>;

    /// Insert a verifier address. This address must exist on chain, otherwise
    /// the transaction will be rejected.
    ///
    /// Validity predicates of each verifier addresses inserted in the
    /// transaction will validate the transaction and will receive all the
    /// changed storage keys and initialized accounts in their inputs.
    fn insert_verifier(
        &mut self,
        addr: &Address,
    ) -> Result<(), storage_api::Error>;

    /// Initialize a new account generates a new established address and
    /// writes the given code as its validity predicate into the storage.
    fn init_account(
        &mut self,
        code: impl AsRef<[u8]>,
    ) -> Result<Address, storage_api::Error>;

    /// Update a validity predicate
    fn update_validity_predicate(
        &mut self,
        addr: &Address,
        code: impl AsRef<[u8]>,
    ) -> Result<(), storage_api::Error>;

    /// Emit an IBC event. On multiple calls, these emitted event will be added.
    fn emit_ibc_event(
        &mut self,
        event: &IbcEvent,
    ) -> Result<(), storage_api::Error>;

    /// Request to charge the provided amount of gas for the current transaction
    fn charge_gas(&mut self, used_gas: u64) -> Result<(), storage_api::Error>;

    /// Get an IBC event with a event type
    fn get_ibc_event(
        &self,
        event_type: impl AsRef<str>,
    ) -> Result<Option<IbcEvent>, storage_api::Error>;
}
