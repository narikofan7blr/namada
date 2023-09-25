use borsh_ext::BorshSerializeExt;
use criterion::{criterion_group, criterion_main, Criterion};
use namada::core::types::account::AccountPublicKeysMap;
use namada::core::types::address;
use namada::core::types::token::{Amount, Transfer};
use namada::proto::{Data, MultiSignature, Section};
use namada_apps::wallet::defaults;

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
    let section = Section::Data(Data::new(transfer_data.serialize_to_vec()));
    let section_hash = section.get_hash();

    let pkim = AccountPublicKeysMap::from_iter([
        defaults::albert_keypair().to_public()
    ]);

    let multisig = MultiSignature::new(
        vec![section_hash],
        &[defaults::albert_keypair()],
        &pkim,
    );
    let signature_index = multisig.signatures.first().unwrap().clone();

    c.bench_function("tx_section_signature_validation", |b| {
        b.iter(|| {
            signature_index
                .verify(&pkim, &multisig.get_raw_hash())
                .unwrap()
        })
    });
}

criterion_group!(host_env, tx_section_signature_validation);
criterion_main!(host_env);
