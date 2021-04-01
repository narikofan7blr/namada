pub mod host_env;
mod memory;

use std::ffi::c_void;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use anoma_vm_env::memory::{TxInput, VpInput};
use parity_wasm::elements;
use pwasm_utils::{self, rules};
use thiserror::Error;
use wasmer::Instance;

use self::host_env::write_log::WriteLog;
use crate::shell::gas::BlockGasMeter;
use crate::shell::storage::{Address, Storage};

const TX_ENTRYPOINT: &str = "apply_tx";
const VP_ENTRYPOINT: &str = "validate_tx";
const WASM_STACK_LIMIT: u32 = u16::MAX as u32;

/// This is used to attach the Ledger's host structures to transaction, which is
/// used for implementing some host calls. It's not thread-safe, we're assuming
/// single-threaded Tx runner.
pub struct TxEnvHostWrapper<T>(*mut c_void, PhantomData<T>);
unsafe impl<T> Send for TxEnvHostWrapper<T> {}
unsafe impl<T> Sync for TxEnvHostWrapper<T> {}

// Have to manually implement [`Clone`], because the derived [`Clone`] for
// [`PhantomData<T>`] puts the bound on [`T: Clone`]. Relevant issue: <https://github.com/rust-lang/rust/issues/26925>
impl<T> Clone for TxEnvHostWrapper<T> {
    fn clone(&self) -> Self {
        Self(self.0, PhantomData)
    }
}

impl<T> TxEnvHostWrapper<T> {
    /// This is not thread-safe, see [`TxEnvHostWrapper`]
    unsafe fn new(host_structure: *mut c_void) -> Self {
        Self(host_structure, PhantomData)
    }

    /// This is not thread-safe, see [`TxEnvHostWrapper`]
    pub unsafe fn get(&self) -> *mut T {
        self.0 as *mut T
    }
}
/// This is used to attach the Ledger's host structures to validity predicate
/// environment, which is used for implementing some host calls. It's not
/// thread-safe, we're assuming read-only access from parallel Vp runners.
pub struct VpEnvHostWrapper<T>(*const c_void, PhantomData<T>);
unsafe impl<T> Send for VpEnvHostWrapper<T> {}
unsafe impl<T> Sync for VpEnvHostWrapper<T> {}

// Same as for [`TxEnvHostWrapper`], we have to manually implement [`Clone`],
// because the derived [`Clone`] for [`PhantomData<T>`] puts the bound on [`T:
// Clone`].
impl<T> Clone for VpEnvHostWrapper<T> {
    fn clone(&self) -> Self {
        Self(self.0, PhantomData)
    }
}

impl<T> VpEnvHostWrapper<T> {
    /// This is not thread-safe, see [`VpEnvHostWrapper`]
    unsafe fn new(host_structure: *const c_void) -> Self {
        Self(host_structure, PhantomData)
    }

    /// This is not thread-safe, see [`VpEnvHostWrapper`]
    #[allow(dead_code)]
    pub unsafe fn get(&self) -> *const T {
        self.0 as *const T
    }
}

#[derive(Clone, Debug)]
pub struct TxRunner {
    wasm_store: wasmer::Store,
}

#[derive(Error, Debug)]
pub enum Error {
    // 1. Common error types
    #[error("Memory error: {0}")]
    MemoryError(memory::Error),
    #[error("Unable to inject gas meter")]
    StackLimiterInjection,
    #[error("Wasm deserialization error: {0}")]
    DeserializationError(elements::Error),
    #[error("Wasm serialization error: {0}")]
    SerializationError(elements::Error),
    #[error("Unable to inject gas meter")]
    GasMeterInjection,
    #[error("Wasm compilation error: {0}")]
    CompileError(wasmer::CompileError),
    #[error("Missing wasm memory export, failed with: {0}")]
    MissingModuleMemory(wasmer::ExportError),
    #[error("Missing wasm entrypoint: {0}")]
    MissingModuleEntrypoint(wasmer::ExportError),
    #[error("Failed running wasm with: {0}")]
    RuntimeError(wasmer::RuntimeError),
    #[error("Failed instantiating wasm module with: {0}")]
    InstantiationError(wasmer::InstantiationError),
    #[error(
        "Unexpected module entrypoint interface {entrypoint}, \
         failed with: {error}"
    )]
    UnexpectedModuleEntrypointInterface {
        entrypoint: &'static str,
        error: wasmer::RuntimeError,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

impl TxRunner {
    pub fn new() -> Self {
        // Use Singlepass compiler with the default settings
        let compiler = wasmer_compiler_singlepass::Singlepass::default();
        // TODO Could we pass the modified accounts sub-spaces via WASM store
        // directly to VPs' wasm scripts to avoid passing it through the
        // host?
        let wasm_store =
            wasmer::Store::new(&wasmer_engine_jit::JIT::new(compiler).engine());
        Self { wasm_store }
    }

    pub fn run(
        &self,
        storage: &mut Storage,
        write_log: &mut WriteLog,
        gas_meter: &mut BlockGasMeter,
        tx_code: Vec<u8>,
        tx_data: &Vec<u8>,
    ) -> Result<()> {
        // This is not thread-safe, we're assuming single-threaded Tx runner.
        let storage =
            unsafe { TxEnvHostWrapper::new(storage as *mut _ as *mut c_void) };
        // This is also not thread-safe, we're assuming single-threaded Tx
        // runner.
        let write_log = unsafe {
            TxEnvHostWrapper::new(write_log as *mut _ as *mut c_void)
        };
        // This is also not thread-safe, we're assuming single-threaded Tx
        // runner.
        let gas_meter = unsafe {
            TxEnvHostWrapper::new(gas_meter as *mut _ as *mut c_void)
        };

        let tx_code = Self::prepare_tx_code(tx_code)?;

        let tx_module = wasmer::Module::new(&self.wasm_store, &tx_code)
            .map_err(Error::CompileError)?;
        let initial_memory = memory::prepare_tx_memory(&self.wasm_store)
            .map_err(Error::MemoryError)?;
        let tx_imports = host_env::prepare_tx_imports(
            &self.wasm_store,
            storage,
            write_log,
            gas_meter,
            initial_memory,
        );

        // compile and run the transaction wasm code
        let tx_code = wasmer::Instance::new(&tx_module, &tx_imports)
            .map_err(Error::InstantiationError)?;
        Self::run_with_input(tx_code, tx_data)
    }

    fn prepare_tx_code(tx_code: Vec<u8>) -> Result<Vec<u8>> {
        let module: elements::Module = elements::deserialize_buffer(&tx_code)
            .map_err(Error::DeserializationError)?;
        let module =
            pwasm_utils::inject_gas_counter(module, &get_gas_rules(), "env")
                .map_err(|_original_module| Error::GasMeterInjection)?;
        let module =
            pwasm_utils::stack_height::inject_limiter(module, WASM_STACK_LIMIT)
                .map_err(|_original_module| Error::StackLimiterInjection)?;
        elements::serialize(module).map_err(Error::SerializationError)
    }

    fn run_with_input(tx_code: Instance, tx_data: &TxInput) -> Result<()> {
        // We need to write the inputs in the memory exported from the wasm
        // module
        let memory = tx_code
            .exports
            .get_memory("memory")
            .map_err(Error::MissingModuleMemory)?;
        let memory::TxCallInput {
            tx_data_ptr,
            tx_data_len,
        } = memory::write_tx_inputs(memory, tx_data)
            .map_err(Error::MemoryError)?;

        // Get the module's entrypoint to be called
        let apply_tx = tx_code
            .exports
            .get_function(TX_ENTRYPOINT)
            .map_err(Error::MissingModuleEntrypoint)?
            .native::<(u64, u64), ()>()
            .map_err(|error| Error::UnexpectedModuleEntrypointInterface {
                entrypoint: TX_ENTRYPOINT,
                error,
            })?;
        apply_tx
            .call(tx_data_ptr, tx_data_len)
            .map_err(Error::RuntimeError)
    }
}

#[derive(Clone, Debug)]
pub struct VpRunner {
    wasm_store: wasmer::Store,
}

impl VpRunner {
    pub fn new() -> Self {
        // Use Singlepass compiler with the default settings
        let compiler = wasmer_compiler_singlepass::Singlepass::default();
        let wasm_store =
            wasmer::Store::new(&wasmer_engine_jit::JIT::new(compiler).engine());
        Self { wasm_store }
    }

    pub fn run<T: AsRef<[u8]>>(
        &self,
        vp_code: T,
        tx_data: &Vec<u8>,
        addr: Address,
        storage: &Storage,
        write_log: &WriteLog,
        gas_meter: Arc<Mutex<BlockGasMeter>>,
        keys_changed: &Vec<String>,
    ) -> Result<bool> {
        // This is not thread-safe, we're assuming read-only access from
        // parallel Vp runners.
        let storage = unsafe {
            VpEnvHostWrapper::new(storage as *const _ as *const c_void)
        };
        // This is also not thread-safe, we're assuming read-only access from
        // parallel Vp runners.
        let write_log = unsafe {
            VpEnvHostWrapper::new(write_log as *const _ as *const c_void)
        };

        let vp_code = Self::prepare_vp_code(vp_code)?;

        let vp_module = wasmer::Module::new(&self.wasm_store, &vp_code)
            .map_err(Error::CompileError)?;
        let initial_memory = memory::prepare_vp_memory(&self.wasm_store)
            .map_err(Error::MemoryError)?;
        let input: VpInput = (addr.to_string(), tx_data, keys_changed);
        let vp_imports = host_env::prepare_vp_imports(
            &self.wasm_store,
            addr,
            storage,
            write_log,
            gas_meter,
            initial_memory,
        );

        // compile and run the transaction wasm code
        let vp_code = wasmer::Instance::new(&vp_module, &vp_imports)
            .map_err(Error::InstantiationError)?;
        VpRunner::run_with_input(vp_code, input)
    }

    fn prepare_vp_code<T: AsRef<[u8]>>(vp_code: T) -> Result<Vec<u8>> {
        let module: elements::Module =
            elements::deserialize_buffer(vp_code.as_ref())
                .map_err(Error::DeserializationError)?;
        let module =
            pwasm_utils::inject_gas_counter(module, &get_gas_rules(), "env")
                .map_err(|_original_module| Error::GasMeterInjection)?;
        let module =
            pwasm_utils::stack_height::inject_limiter(module, WASM_STACK_LIMIT)
                .map_err(|_original_module| Error::StackLimiterInjection)?;
        elements::serialize(module).map_err(Error::SerializationError)
    }

    fn run_with_input(vp_code: Instance, input: VpInput) -> Result<bool> {
        // We need to write the inputs in the memory exported from the wasm
        // module
        let memory = vp_code
            .exports
            .get_memory("memory")
            .map_err(Error::MissingModuleMemory)?;
        let memory::VpCallInput {
            addr_ptr,
            addr_len,
            tx_data_ptr,
            tx_data_len,
            keys_changed_ptr,
            keys_changed_len,
        } = memory::write_vp_inputs(memory, input)
            .map_err(Error::MemoryError)?;

        // Get the module's entrypoint to be called
        let validate_tx = vp_code
            .exports
            .get_function(VP_ENTRYPOINT)
            .map_err(Error::MissingModuleEntrypoint)?
            .native::<(u64, u64, u64, u64, u64, u64), u64>()
            .map_err(|error| Error::UnexpectedModuleEntrypointInterface {
                entrypoint: VP_ENTRYPOINT,
                error,
            })?;
        let is_valid = validate_tx
            .call(
                addr_ptr,
                addr_len,
                tx_data_ptr,
                tx_data_len,
                keys_changed_ptr,
                keys_changed_len,
            )
            .map_err(Error::RuntimeError)?;
        Ok(is_valid == 1)
    }
}

/// Get the gas rules used to meter wasm operations
fn get_gas_rules() -> rules::Set {
    rules::Set::default().with_grow_cost(1)
}
