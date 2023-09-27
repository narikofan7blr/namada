use std::collections::HashSet;

use criterion::{criterion_group, criterion_main, Criterion};
use namada::core::types::account::AccountPublicKeysMap;
use namada::core::types::address;
use namada::core::types::token::{Amount, Transfer};
use namada::proto::Signature;
use namada_apps::wallet::defaults;
use namada_benches::{generate_tx, TX_TRANSFER_WASM};

/// Benchmarks the validation of a single signature on a single `Section` of a
/// transaction
fn tx_section_signature_validation(c: &mut Criterion) {
    let transfer_data = Transfer {
        source: defaults::albert_address(),
        target: defaults::bertha_address(),
        token: address::nam(),
        amount: Amount::native_whole(500).native_denominated(),
        key: None,
        shielded: None,
    };
    let tx = generate_tx(TX_TRANSFER_WASM, transfer_data, None, None, None);

    let pkim = AccountPublicKeysMap::from_iter([
        defaults::albert_keypair().to_public()
    ]);

    let multisig = Signature::new(
        tx.header_hash(),
        pkim.index_secret_keys(vec![defaults::albert_keypair()]),
        None,
    );

    c.bench_function("tx_section_signature_validation", |b| {
        b.iter(|| {
            multisig
                .verify_signature(&mut HashSet::new(), &pkim, &None)
                .unwrap()
        })
    });
}

criterion_group!(host_env, tx_section_signature_validation);
criterion_main!(host_env);
