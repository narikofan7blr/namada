//! Benchmarks module based on criterion.
//!
//! Measurements are taken on the elapsed wall-time.
//!
//! The benchmarks only focus on sucessfull transactions and vps: in case of
//! failure, the bench function shall panic to avoid timing incomplete execution
//! paths.
//!
//! In addition, this module also contains benchmarks for
//! [`WrapperTx`][`namada::core::types::transaction::wrapper::WrapperTx`]
//! validation and [`host_env`][`namada::vm::host_env`] exposed functions that
//! define the gas constants of [`gas`][`namada::core::ledger::gas`].
//!
//! For more realistic results these benchmarks should be run on all the
//! combination of supported OS/architecture.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::Once;

use borsh::{BorshDeserialize, BorshSerialize};
use masp_primitives::transaction::Transaction;
use masp_primitives::zip32::ExtendedFullViewingKey;
use masp_proofs::prover::LocalTxProver;
use namada::core::ledger::governance::storage::proposal::ProposalType;
use namada::core::ledger::ibc::storage::port_key;
use namada::core::types::address::{self, Address};
use namada::core::types::key::common::SecretKey;
use namada::core::types::storage::Key;
use namada::core::types::token::{Amount, Transfer};
use namada::ibc::applications::transfer::msgs::transfer::MsgTransfer;
use namada::ibc::applications::transfer::packet::PacketData;
use namada::ibc::applications::transfer::PrefixedCoin;
use namada::ibc::clients::ics07_tendermint::client_state::{
    AllowUpdate, ClientState,
};
use namada::ibc::clients::ics07_tendermint::consensus_state::ConsensusState;
use namada::ibc::clients::ics07_tendermint::trust_threshold::TrustThreshold;
use namada::ibc::core::ics02_client::client_type::ClientType;
use namada::ibc::core::ics03_connection::connection::{
    ConnectionEnd, Counterparty, State as ConnectionState,
};
use namada::ibc::core::ics03_connection::version::Version;
use namada::ibc::core::ics04_channel::channel::{
    ChannelEnd, Counterparty as ChannelCounterparty, Order, State,
};
use namada::ibc::core::ics04_channel::timeout::TimeoutHeight;
use namada::ibc::core::ics04_channel::Version as ChannelVersion;
use namada::ibc::core::ics23_commitment::commitment::{
    CommitmentPrefix, CommitmentRoot,
};
use namada::ibc::core::ics23_commitment::specs::ProofSpecs;
use namada::ibc::core::ics24_host::identifier::{
    ChainId as IbcChainId, ChannelId as NamadaChannelId, ChannelId, ClientId,
    ConnectionId, ConnectionId as NamadaConnectionId, PortId as NamadaPortId,
    PortId,
};
use namada::ibc::core::ics24_host::path::Path as IbcPath;
use namada::ibc::core::timestamp::Timestamp as IbcTimestamp;
use namada::ibc::core::Msg;
use namada::ibc::Height as IbcHeight;
use namada::ibc_proto::google::protobuf::Any;
use namada::ibc_proto::protobuf::Protobuf;
use namada::ledger::gas::TxGasMeter;
use namada::ledger::ibc::storage::{channel_key, connection_key};
use namada::ledger::queries::{
    Client, EncodedResponseQuery, RequestCtx, RequestQuery, Router, RPC,
};
use namada::ledger::storage_api::StorageRead;
use namada::proof_of_stake;
use namada::proto::{Code, Data, Section, Signature, Tx};
use namada::sdk::args::InputAmount;
use namada::sdk::masp::{
    self, ShieldedContext, ShieldedTransfer, ShieldedUtils,
};
use namada::sdk::wallet::Wallet;
use namada::tendermint::Hash;
use namada::tendermint_rpc::{self};
use namada::types::address::InternalAddress;
use namada::types::chain::ChainId;
use namada::types::io::DefaultIo;
use namada::types::masp::{
    ExtendedViewingKey, PaymentAddress, TransferSource, TransferTarget,
};
use namada::types::storage::{BlockHeight, Epoch, KeySeg, TxIndex};
use namada::types::time::DateTimeUtc;
use namada::types::token::DenominatedAmount;
use namada::types::transaction::governance::InitProposalData;
use namada::types::transaction::pos::Bond;
use namada::types::transaction::GasLimit;
use namada::vm::wasm::run;
use namada_apps::cli::args::{Tx as TxArgs, TxTransfer};
use namada_apps::cli::context::FromContext;
use namada_apps::cli::Context;
use namada_apps::config::TendermintMode;
use namada_apps::facade::tendermint_proto::abci::RequestInitChain;
use namada_apps::facade::tendermint_proto::google::protobuf::Timestamp;
use namada_apps::node::ledger::shell::Shell;
use namada_apps::wallet::{defaults, CliWalletUtils};
use namada_apps::{config, wasm_loader};
use namada_test_utils::tx_data::TxWriteData;
use rand_core::OsRng;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

pub const WASM_DIR: &str = "../wasm";
pub const TX_BOND_WASM: &str = "tx_bond.wasm";
pub const TX_TRANSFER_WASM: &str = "tx_transfer.wasm";
pub const TX_UPDATE_ACCOUNT_WASM: &str = "tx_update_account.wasm";
pub const TX_VOTE_PROPOSAL_WASM: &str = "tx_vote_proposal.wasm";
pub const TX_UNBOND_WASM: &str = "tx_unbond.wasm";
pub const TX_INIT_PROPOSAL_WASM: &str = "tx_init_proposal.wasm";
pub const TX_REVEAL_PK_WASM: &str = "tx_reveal_pk.wasm";
pub const TX_CHANGE_VALIDATOR_COMMISSION_WASM: &str =
    "tx_change_validator_commission.wasm";
pub const TX_IBC_WASM: &str = "tx_ibc.wasm";
pub const TX_UNJAIL_VALIDATOR_WASM: &str = "tx_unjail_validator.wasm";
pub const VP_VALIDATOR_WASM: &str = "vp_validator.wasm";

pub const ALBERT_PAYMENT_ADDRESS: &str = "albert_payment";
pub const ALBERT_SPENDING_KEY: &str = "albert_spending";
pub const BERTHA_PAYMENT_ADDRESS: &str = "bertha_payment";
const BERTHA_SPENDING_KEY: &str = "bertha_spending";

const FILE_NAME: &str = "shielded.dat";
const TMP_FILE_NAME: &str = "shielded.tmp";

/// For `tracing_subscriber`, which fails if called more than once in the same
/// process
static SHELL_INIT: Once = Once::new();

pub struct BenchShell {
    pub inner: Shell,
    /// NOTE: Temporary directory should be dropped last since Shell need to
    /// flush data on drop
    tempdir: TempDir,
}

impl Deref for BenchShell {
    type Target = Shell;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for BenchShell {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Default for BenchShell {
    fn default() -> Self {
        SHELL_INIT.call_once(|| {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::from_default_env(),
                )
                .init();
        });

        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().canonicalize().unwrap();

        let mut shell = Shell::new(
            config::Ledger::new(path, Default::default(), TendermintMode::Full),
            WASM_DIR.into(),
            sender,
            None,
            None,
            50 * 1024 * 1024, // 50 kiB
            50 * 1024 * 1024, // 50 kiB
            address::nam(),
        );

        shell
            .init_chain(
                RequestInitChain {
                    time: Some(Timestamp {
                        seconds: 0,
                        nanos: 0,
                    }),
                    chain_id: ChainId::default().to_string(),
                    ..Default::default()
                },
                2,
            )
            .unwrap();

        // Bond from Albert to validator
        let bond = Bond {
            validator: defaults::validator_address(),
            amount: Amount::native_whole(1000),
            source: Some(defaults::albert_address()),
        };
        let signed_tx = generate_tx(
            TX_BOND_WASM,
            bond,
            None,
            None,
            Some(&defaults::albert_keypair()),
        );

        let params =
            proof_of_stake::read_pos_params(&shell.wl_storage).unwrap();
        let mut bench_shell = BenchShell {
            inner: shell,
            tempdir,
        };

        bench_shell.execute_tx(&signed_tx);
        bench_shell.wl_storage.commit_tx();

        // Initialize governance proposal
        let content_section = Section::ExtraData(Code::new(vec![]));
        let voting_start_epoch = Epoch(25);
        let signed_tx = generate_tx(
            TX_INIT_PROPOSAL_WASM,
            InitProposalData {
                id: None,
                content: content_section.get_hash(),
                author: defaults::albert_address(),
                r#type: ProposalType::Default(None),
                voting_start_epoch,
                voting_end_epoch: 28.into(),
                grace_epoch: 34.into(),
            },
            None,
            Some(vec![content_section]),
            Some(&defaults::albert_keypair()),
        );

        bench_shell.execute_tx(&signed_tx);
        bench_shell.wl_storage.commit_tx();
        bench_shell.inner.commit();

        // Advance epoch for pos benches
        for _ in 0..=(params.pipeline_len + params.unbonding_len) {
            bench_shell.advance_epoch();
        }
        // Must start after current epoch
        debug_assert_eq!(
            bench_shell.wl_storage.get_block_epoch().unwrap().next(),
            voting_start_epoch
        );

        bench_shell
    }
}

impl BenchShell {
    pub fn execute_tx(&mut self, tx: &Tx) {
        run::tx(
            &self.inner.wl_storage.storage,
            &mut self.inner.wl_storage.write_log,
            &mut TxGasMeter::new_from_sub_limit(u64::MAX.into()),
            &TxIndex(0),
            tx,
            &mut self.inner.vp_wasm_cache,
            &mut self.inner.tx_wasm_cache,
        )
        .unwrap();
    }

    pub fn advance_epoch(&mut self) {
        let pipeline_len =
            proof_of_stake::read_pos_params(&self.inner.wl_storage)
                .unwrap()
                .pipeline_len;

        self.wl_storage.storage.block.epoch =
            self.wl_storage.storage.block.epoch.next();
        let current_epoch = self.wl_storage.storage.block.epoch;

        proof_of_stake::copy_validator_sets_and_positions(
            &mut self.wl_storage,
            current_epoch,
            current_epoch + pipeline_len,
        )
        .unwrap();
    }

    pub fn init_ibc_channel(&mut self) {
        // Set connection open
        let client_id = ClientId::new(
            ClientType::new("01-tendermint".to_string()).unwrap(),
            1,
        )
        .unwrap();
        let connection = ConnectionEnd::new(
            ConnectionState::Open,
            client_id.clone(),
            Counterparty::new(
                client_id,
                Some(ConnectionId::new(1)),
                CommitmentPrefix::try_from(b"ibc".to_vec()).unwrap(),
            ),
            vec![Version::default()],
            std::time::Duration::new(100, 0),
        )
        .unwrap();

        let addr_key =
            Key::from(Address::Internal(InternalAddress::Ibc).to_db_key());

        let connection_key = connection_key(&NamadaConnectionId::new(1));
        self.wl_storage
            .storage
            .write(&connection_key, connection.encode_vec())
            .unwrap();

        // Set port
        let port_key = port_key(&NamadaPortId::transfer());

        let index_key = addr_key
            .join(&Key::from("capabilities/index".to_string().to_db_key()));
        self.wl_storage
            .storage
            .write(&index_key, 1u64.to_be_bytes())
            .unwrap();
        self.wl_storage
            .storage
            .write(&port_key, 1u64.to_be_bytes())
            .unwrap();
        let cap_key =
            addr_key.join(&Key::from("capabilities/1".to_string().to_db_key()));
        self.wl_storage
            .storage
            .write(&cap_key, PortId::transfer().as_bytes())
            .unwrap();

        // Set Channel open
        let counterparty = ChannelCounterparty::new(
            PortId::transfer(),
            Some(ChannelId::new(5)),
        );
        let channel = ChannelEnd::new(
            State::Open,
            Order::Unordered,
            counterparty,
            vec![ConnectionId::new(1)],
            ChannelVersion::new("ics20-1".to_string()),
        )
        .unwrap();
        let channel_key =
            channel_key(&NamadaPortId::transfer(), &NamadaChannelId::new(5));
        self.wl_storage
            .storage
            .write(&channel_key, channel.encode_vec())
            .unwrap();

        // Set client state
        let client_id = ClientId::new(
            ClientType::new("01-tendermint".to_string()).unwrap(),
            1,
        )
        .unwrap();
        let client_state_key = addr_key.join(&Key::from(
            IbcPath::ClientState(
                namada::ibc::core::ics24_host::path::ClientStatePath(
                    client_id.clone(),
                ),
            )
            .to_string()
            .to_db_key(),
        ));
        let client_state = ClientState::new(
            IbcChainId::from(ChainId::default().to_string()),
            TrustThreshold::ONE_THIRD,
            std::time::Duration::new(1, 0),
            std::time::Duration::new(2, 0),
            std::time::Duration::new(1, 0),
            IbcHeight::new(0, 1).unwrap(),
            ProofSpecs::cosmos(),
            vec![],
            AllowUpdate {
                after_expiry: true,
                after_misbehaviour: true,
            },
        )
        .unwrap();
        let bytes = <ClientState as Protobuf<Any>>::encode_vec(&client_state);
        self.wl_storage
            .storage
            .write(&client_state_key, bytes)
            .expect("write failed");

        // Set consensus state
        let now: namada::tendermint::Time =
            DateTimeUtc::now().try_into().unwrap();
        let consensus_key = addr_key.join(&Key::from(
            IbcPath::ClientConsensusState(
                namada::ibc::core::ics24_host::path::ClientConsensusStatePath {
                    client_id,
                    epoch: 0,
                    height: 1,
                },
            )
            .to_string()
            .to_db_key(),
        ));

        let consensus_state = ConsensusState {
            timestamp: now,
            root: CommitmentRoot::from_bytes(&[]),
            next_validators_hash: Hash::Sha256([0u8; 32]),
        };

        let bytes =
            <ConsensusState as Protobuf<Any>>::encode_vec(&consensus_state);
        self.wl_storage
            .storage
            .write(&consensus_key, bytes)
            .unwrap();
    }
}

pub fn generate_tx(
    wasm_code_path: &str,
    data: impl BorshSerialize,
    shielded: Option<Transaction>,
    extra_section: Option<Vec<Section>>,
    signer: Option<&SecretKey>,
) -> Tx {
    let mut tx = Tx::from_type(namada::types::transaction::TxType::Decrypted(
        namada::types::transaction::DecryptedTx::Decrypted,
    ));

    // NOTE: don't use the hash to avoid computing the cost of loading the wasm
    // code
    tx.set_code(Code::new(wasm_loader::read_wasm_or_exit(
        WASM_DIR,
        wasm_code_path,
    )));
    tx.set_data(Data::new(data.try_to_vec().unwrap()));

    if let Some(transaction) = shielded {
        tx.add_section(Section::MaspTx(transaction));
    }

    if let Some(sections) = extra_section {
        for section in sections {
            if let Section::ExtraData(_) = section {
                tx.add_section(section);
            }
        }
    }

    if let Some(signer) = signer {
        tx.add_section(Section::Signature(Signature::new(
            tx.sechashes(),
            [(0, signer.clone())].into_iter().collect(),
            None,
        )));
    }

    tx
}

pub fn generate_ibc_tx(wasm_code_path: &str, msg: impl Msg) -> Tx {
    // This function avoid serializaing the tx data with Borsh
    let mut tx = Tx::from_type(namada::types::transaction::TxType::Decrypted(
        namada::types::transaction::DecryptedTx::Decrypted,
    ));
    tx.set_code(Code::new(wasm_loader::read_wasm_or_exit(
        WASM_DIR,
        wasm_code_path,
    )));

    let mut data = vec![];
    prost::Message::encode(&msg.to_any(), &mut data).unwrap();
    tx.set_data(Data::new(data));

    // NOTE: the Ibc VP doesn't actually check the signature
    tx
}

pub fn generate_foreign_key_tx(signer: &SecretKey) -> Tx {
    let wasm_code = std::fs::read("../wasm_for_tests/tx_write.wasm").unwrap();

    let mut tx = Tx::from_type(namada::types::transaction::TxType::Decrypted(
        namada::types::transaction::DecryptedTx::Decrypted,
    ));
    tx.set_code(Code::new(wasm_code));
    tx.set_data(Data::new(
        TxWriteData {
            key: Key::from("bench_foreign_key".to_string().to_db_key()),
            value: vec![0; 64],
        }
        .try_to_vec()
        .unwrap(),
    ));
    tx.add_section(Section::Signature(Signature::new(
        tx.sechashes(),
        [(0, signer.clone())].into_iter().collect(),
        None,
    )));

    tx
}

pub fn generate_ibc_transfer_tx() -> Tx {
    let token = PrefixedCoin {
        denom: address::nam().to_string().parse().unwrap(),
        amount: Amount::native_whole(1000)
            .to_string_native()
            .split('.')
            .next()
            .unwrap()
            .to_string()
            .parse()
            .unwrap(),
    };

    let timeout_height = TimeoutHeight::At(IbcHeight::new(0, 100).unwrap());

    let now: namada::tendermint::Time = DateTimeUtc::now().try_into().unwrap();
    let now: IbcTimestamp = now.into();
    let timeout_timestamp = (now + std::time::Duration::new(3600, 0)).unwrap();

    let msg = MsgTransfer {
        port_id_on_a: PortId::transfer(),
        chan_id_on_a: ChannelId::new(5),
        packet_data: PacketData {
            token,
            sender: defaults::albert_address().to_string().into(),
            receiver: defaults::bertha_address().to_string().into(),
            memo: "".parse().unwrap(),
        },
        timeout_height_on_b: timeout_height,
        timeout_timestamp_on_b: timeout_timestamp,
    };

    generate_ibc_tx(TX_IBC_WASM, msg)
}

pub struct BenchShieldedCtx {
    pub shielded: ShieldedContext<BenchShieldedUtils>,
    pub shell: BenchShell,
    pub wallet: Wallet<CliWalletUtils>,
}

#[derive(Debug)]
struct WrapperTempDir(TempDir);

// Mock the required traits for ShieldedUtils

impl Default for WrapperTempDir {
    fn default() -> Self {
        Self(TempDir::new().unwrap())
    }
}

impl Clone for WrapperTempDir {
    fn clone(&self) -> Self {
        Self(TempDir::new().unwrap())
    }
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Default)]
pub struct BenchShieldedUtils {
    #[borsh_skip]
    context_dir: WrapperTempDir,
}

#[cfg_attr(feature = "async-send", async_trait::async_trait)]
#[cfg_attr(not(feature = "async-send"), async_trait::async_trait(?Send))]
impl ShieldedUtils for BenchShieldedUtils {
    fn local_tx_prover(&self) -> LocalTxProver {
        if let Ok(params_dir) = std::env::var(masp::ENV_VAR_MASP_PARAMS_DIR) {
            let params_dir = PathBuf::from(params_dir);
            let spend_path = params_dir.join(masp::SPEND_NAME);
            let convert_path = params_dir.join(masp::CONVERT_NAME);
            let output_path = params_dir.join(masp::OUTPUT_NAME);
            LocalTxProver::new(&spend_path, &output_path, &convert_path)
        } else {
            LocalTxProver::with_default_location()
                .expect("unable to load MASP Parameters")
        }
    }

    /// Try to load the last saved shielded context from the given context
    /// directory. If this fails, then leave the current context unchanged.
    async fn load(self) -> std::io::Result<ShieldedContext<Self>> {
        // Try to load shielded context from file
        let mut ctx_file = File::open(
            self.context_dir.0.path().to_path_buf().join(FILE_NAME),
        )?;
        let mut bytes = Vec::new();
        ctx_file.read_to_end(&mut bytes)?;
        let mut new_ctx = ShieldedContext::deserialize(&mut &bytes[..])?;
        // Associate the originating context directory with the
        // shielded context under construction
        new_ctx.utils = self;
        Ok(new_ctx)
    }

    /// Save this shielded context into its associated context directory
    async fn save(&self, ctx: &ShieldedContext<Self>) -> std::io::Result<()> {
        let tmp_path =
            self.context_dir.0.path().to_path_buf().join(TMP_FILE_NAME);
        {
            // First serialize the shielded context into a temporary file.
            // Inability to create this file implies a simultaneuous write is in
            // progress. In this case, immediately fail. This is unproblematic
            // because the data intended to be stored can always be re-fetched
            // from the blockchain.
            let mut ctx_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(tmp_path.clone())?;
            let mut bytes = Vec::new();
            ctx.serialize(&mut bytes)
                .expect("cannot serialize shielded context");
            ctx_file.write_all(&bytes[..])?;
        }
        // Atomically update the old shielded context file with new data.
        // Atomicity is required to prevent other client instances from reading
        // corrupt data.
        std::fs::rename(
            tmp_path.clone(),
            self.context_dir.0.path().to_path_buf().join(FILE_NAME),
        )?;
        // Finally, remove our temporary file to allow future saving of shielded
        // contexts.
        std::fs::remove_file(tmp_path)?;
        Ok(())
    }
}

#[cfg_attr(feature = "async-send", async_trait::async_trait)]
#[cfg_attr(not(feature = "async-send"), async_trait::async_trait(?Send))]
impl Client for BenchShell {
    type Error = std::io::Error;

    async fn request(
        &self,
        path: String,
        data: Option<Vec<u8>>,
        height: Option<BlockHeight>,
        prove: bool,
    ) -> Result<EncodedResponseQuery, Self::Error> {
        let data = data.unwrap_or_default();
        let height = height.unwrap_or_default();

        let request = RequestQuery {
            data,
            path,
            height,
            prove,
        };

        let ctx = RequestCtx {
            wl_storage: &self.wl_storage,
            event_log: self.event_log(),
            vp_wasm_cache: self.vp_wasm_cache.read_only(),
            tx_wasm_cache: self.tx_wasm_cache.read_only(),
            storage_read_past_height_limit: None,
        };

        RPC.handle(ctx, &request)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::NotFound))
    }

    async fn perform<R>(
        &self,
        _request: R,
    ) -> Result<R::Response, tendermint_rpc::Error>
    where
        R: tendermint_rpc::SimpleRequest,
    {
        tendermint_rpc::Response::from_string("MOCK RESPONSE")
    }
}

impl Default for BenchShieldedCtx {
    fn default() -> Self {
        let mut shell = BenchShell::default();

        let mut ctx =
            Context::new::<DefaultIo>(namada_apps::cli::args::Global {
                chain_id: None,
                base_dir: shell.tempdir.as_ref().canonicalize().unwrap(),
                wasm_dir: Some(WASM_DIR.into()),
            })
            .unwrap();

        // Generate spending key for Albert and Bertha
        ctx.wallet.gen_spending_key(
            ALBERT_SPENDING_KEY.to_string(),
            None,
            true,
        );
        ctx.wallet.gen_spending_key(
            BERTHA_SPENDING_KEY.to_string(),
            None,
            true,
        );
        namada_apps::wallet::save(&ctx.wallet).unwrap();

        // Generate payment addresses for both Albert and Bertha
        for (alias, viewing_alias) in [
            (ALBERT_PAYMENT_ADDRESS, ALBERT_SPENDING_KEY),
            (BERTHA_PAYMENT_ADDRESS, BERTHA_SPENDING_KEY),
        ]
        .map(|(p, s)| (p.to_owned(), s.to_owned()))
        {
            let viewing_key: FromContext<ExtendedViewingKey> = FromContext::new(
                ctx.wallet
                    .find_viewing_key(viewing_alias)
                    .unwrap()
                    .to_string(),
            );
            let viewing_key =
                ExtendedFullViewingKey::from(ctx.get_cached(&viewing_key))
                    .fvk
                    .vk;
            let (div, _g_d) =
                namada::sdk::masp::find_valid_diversifier(&mut OsRng);
            let payment_addr = viewing_key.to_payment_address(div).unwrap();
            let _ = ctx
                .wallet
                .insert_payment_addr(
                    alias,
                    PaymentAddress::from(payment_addr).pinned(false),
                    true,
                )
                .unwrap();
        }

        namada_apps::wallet::save(&ctx.wallet).unwrap();
        namada::ledger::storage::update_allowed_conversions(
            &mut shell.wl_storage,
        )
        .unwrap();

        Self {
            shielded: ShieldedContext::default(),
            shell,
            wallet: ctx.wallet,
        }
    }
}

impl BenchShieldedCtx {
    pub fn generate_masp_tx(
        &mut self,
        amount: Amount,
        source: TransferSource,
        target: TransferTarget,
    ) -> Tx {
        let mock_args = TxArgs {
            dry_run: false,
            dry_run_wrapper: false,
            dump_tx: false,
            force: false,
            broadcast_only: false,
            ledger_address: (),
            initialized_account_alias: None,
            fee_amount: None,
            fee_token: address::nam(),
            fee_unshield: None,
            gas_limit: GasLimit::from(u64::MAX),
            expiration: None,
            disposable_signing_key: false,
            signing_keys: vec![defaults::albert_keypair()],
            signatures: vec![],
            wallet_alias_force: true,
            chain_id: None,
            tx_reveal_code_path: TX_REVEAL_PK_WASM.into(),
            verification_key: None,
            password: None,
            wrapper_fee_payer: None,
            output_folder: None,
        };

        let args = TxTransfer {
            tx: mock_args,
            source: source.clone(),
            target: target.clone(),
            token: address::nam(),
            amount: InputAmount::Validated(DenominatedAmount {
                amount,
                denom: 0.into(),
            }),
            native_token: self.shell.wl_storage.storage.native_token.clone(),
            tx_code_path: TX_TRANSFER_WASM.into(),
        };

        let async_runtime = tokio::runtime::Runtime::new().unwrap();
        let spending_key = self
            .wallet
            .find_spending_key(ALBERT_SPENDING_KEY, None)
            .unwrap();
        async_runtime
            .block_on(self.shielded.fetch(
                &self.shell,
                &[spending_key.into()],
                &[],
            ))
            .unwrap();
        let shielded = async_runtime
            .block_on(
                self.shielded
                    .gen_shielded_transfer::<_, DefaultIo>(&self.shell, args),
            )
            .unwrap()
            .map(
                |ShieldedTransfer {
                     builder: _,
                     masp_tx,
                     metadata: _,
                     epoch: _,
                 }| masp_tx,
            );

        let mut hasher = Sha256::new();
        let shielded_section_hash = shielded.clone().map(|transaction| {
            namada::core::types::hash::Hash(
                Section::MaspTx(transaction)
                    .hash(&mut hasher)
                    .finalize_reset()
                    .into(),
            )
        });

        generate_tx(
            TX_TRANSFER_WASM,
            Transfer {
                source: source.effective_address(),
                target: target.effective_address(),
                token: address::nam(),
                amount: DenominatedAmount::native(amount),
                key: None,
                shielded: shielded_section_hash,
            },
            shielded,
            None,
            Some(&defaults::albert_keypair()),
        )
    }
}
