pub use ark_bls12_381::Bls12_381 as EllipticCurve;

/// Integration of Ferveo cryptographic primitives
/// to enable decrypting txs.
/// *Not wasm compatible*
pub mod decrypted_tx {
    #[cfg(feature = "ferveo-tpke")]
    use ark_ec::PairingEngine;
    use borsh::{BorshDeserialize, BorshSchema, BorshSerialize};
    use borsh_ext::BorshSerializeExt;
    use sha2::{Digest, Sha256};

    #[derive(
        Clone,
        Debug,
        BorshSerialize,
        BorshDeserialize,
        BorshSchema,
        serde::Serialize,
        serde::Deserialize,
    )]
    #[allow(clippy::large_enum_variant)]
    /// Holds the result of attempting to decrypt
    /// a transaction and the data necessary for
    /// other validators to verify
    pub enum DecryptedTx {
        /// The decrypted payload
        Decrypted {
            #[cfg(not(feature = "mainnet"))]
            /// A PoW solution can be used to allow zero-fee testnet
            /// transactions.
            /// This is true when the wrapper of this tx contains a valid
            /// `testnet_pow::Solution`.
            // For some reason, we get `warning: fields `tx` and
            // `has_valid_pow` are never read` even though they are being used!
            #[allow(dead_code)]
            has_valid_pow: bool,
        },
        /// The wrapper whose payload could not be decrypted
        Undecryptable,
    }

    impl DecryptedTx {
        /// Produce a SHA-256 hash of this header
        pub fn hash<'a>(&self, hasher: &'a mut Sha256) -> &'a mut Sha256 {
            hasher.update(self.serialize_to_vec());
            hasher
        }
    }

    /// Verify that if the encrypted payload was marked
    /// "undecryptable", we should not be able to decrypt
    /// it
    #[cfg(feature = "ferveo-tpke")]
    pub fn verify_decrypted_correctly(
        decrypted: &DecryptedTx,
        mut otx: crate::proto::Tx,
        privkey: <super::EllipticCurve as PairingEngine>::G2Affine,
    ) -> bool {
        match decrypted {
            DecryptedTx::Decrypted { .. } => true,
            DecryptedTx::Undecryptable => otx.decrypt(privkey).is_err(),
        }
    }
}

pub use decrypted_tx::*;
