pub mod prefix_iter;
pub mod write_log;

use std::convert::TryInto;
use std::sync::{Arc, Mutex};

use anoma::protobuf::types::Tx;
use anoma_vm_env::memory::KeyVal;
use borsh::BorshSerialize;
use tokio::sync::mpsc::Sender;
use wasmer::{
    HostEnvInitError, ImportObject, Instance, Memory, Store, WasmerEnv,
};

use self::prefix_iter::{PrefixIteratorId, PrefixIterators};
use self::write_log::WriteLog;
use super::memory::AnomaMemory;
use super::{TxEnvHostWrapper, VpEnvHostWrapper};
use crate::shell::gas::BlockGasMeter;
use crate::shell::storage::{Address, Key, Storage};

#[derive(Clone)]
struct TxEnv<'a> {
    // not thread-safe, assuming single-threaded Tx runner
    storage: TxEnvHostWrapper<Storage>,
    // not thread-safe, assuming single-threaded Tx runner
    write_log: TxEnvHostWrapper<WriteLog>,
    // not thread-safe, assuming single-threaded Tx runner
    iterators: TxEnvHostWrapper<PrefixIterators<'a>>,
    // not thread-safe, assuming single-threaded Tx runner
    gas_meter: TxEnvHostWrapper<BlockGasMeter>,
    memory: AnomaMemory,
}

impl<'a> WasmerEnv for TxEnv<'a> {
    fn init_with_instance(
        &mut self,
        instance: &Instance,
    ) -> std::result::Result<(), HostEnvInitError> {
        self.memory.init_env_memory(&instance.exports)
    }
}

#[derive(Clone)]
struct VpEnv<'a> {
    /// The address of the account that owns the VP
    addr: Address,
    // not thread-safe, assuming read-only access from parallel Vp runners
    storage: VpEnvHostWrapper<Storage>,
    // not thread-safe, assuming read-only access from parallel Vp runners
    write_log: VpEnvHostWrapper<WriteLog>,
    // TODO: tentatively use TxEnvHostWrapper, please replace it with MutEnvHostWrapper
    iterators: TxEnvHostWrapper<PrefixIterators<'a>>,
    // TODO In parallel runs, we can change only the maximum used gas of all
    // the VPs that we ran.
    gas_meter: Arc<Mutex<BlockGasMeter>>,
    memory: AnomaMemory,
}

impl<'a> WasmerEnv for VpEnv<'a> {
    fn init_with_instance(
        &mut self,
        instance: &Instance,
    ) -> std::result::Result<(), HostEnvInitError> {
        self.memory.init_env_memory(&instance.exports)
    }
}

#[derive(Clone)]
pub struct MatchmakerEnv {
    pub tx_code: Vec<u8>,
    pub inject_tx: Sender<Tx>,
    pub memory: AnomaMemory,
}

impl WasmerEnv for MatchmakerEnv {
    fn init_with_instance(
        &mut self,
        instance: &Instance,
    ) -> std::result::Result<(), HostEnvInitError> {
        self.memory.init_env_memory(&instance.exports)
    }
}

/// Prepare imports (memory and host functions) exposed to the vm guest running
/// transaction code
pub fn prepare_tx_imports(
    wasm_store: &Store,
    storage: TxEnvHostWrapper<Storage>,
    write_log: TxEnvHostWrapper<WriteLog>,
    iterators: TxEnvHostWrapper<PrefixIterators<'static>>,
    gas_meter: TxEnvHostWrapper<BlockGasMeter>,
    initial_memory: Memory,
) -> ImportObject {
    let env = TxEnv {
        storage,
        write_log,
        iterators,
        gas_meter,
        memory: AnomaMemory::default(),
    };
    wasmer::imports! {
        // default namespace
        "env" => {
            "memory" => initial_memory,
            "gas" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), tx_charge_gas),
            "_read" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), tx_storage_read),
            "_write" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), tx_storage_write),
            "_delete" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), tx_storage_delete),
            "_read_varlen" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), tx_storage_read_varlen),
            "_iter_prefix" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), tx_storage_iter_prefix),
            "_iter_next" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), tx_storage_iter_next),
            "_iter_next_varlen" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), tx_storage_iter_next_varlen),
            "_log_string" => wasmer::Function::new_native_with_env(wasm_store, env, tx_log_string),
        },
    }
}

/// Prepare imports (memory and host functions) exposed to the vm guest running
/// validity predicate code
pub fn prepare_vp_imports(
    wasm_store: &Store,
    addr: Address,
    storage: VpEnvHostWrapper<Storage>,
    write_log: VpEnvHostWrapper<WriteLog>,
    // TODO: tentatively use TxEnvHostWrapper, please replace it with MutEnvHostWrapper
    iterators: TxEnvHostWrapper<PrefixIterators<'static>>,
    gas_meter: Arc<Mutex<BlockGasMeter>>,
    initial_memory: Memory,
) -> ImportObject {
    let env = VpEnv {
        addr,
        storage,
        write_log,
        iterators,
        gas_meter,
        memory: AnomaMemory::default(),
    };
    wasmer::imports! {
        // default namespace
        "env" => {
            "memory" => initial_memory,
            "gas" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), vp_charge_gas),
            "_read_pre" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), vp_storage_read_pre),
            "_read_post" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), vp_storage_read_post),
            "_read_pre_varlen" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), vp_storage_read_pre_varlen),
            "_read_post_varlen" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), vp_storage_read_post_varlen),
            "_iter_prefix" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), vp_storage_iter_prefix),
            "_iter_pre_next" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), vp_storage_iter_pre_next),
            "_iter_post_next" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), vp_storage_iter_post_next),
            "_iter_pre_next_varlen" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), vp_storage_iter_pre_next_varlen),
            "_iter_post_next_varlen" => wasmer::Function::new_native_with_env(wasm_store, env.clone(), vp_storage_iter_post_next_varlen),
            "_log_string" => wasmer::Function::new_native_with_env(wasm_store, env, vp_log_string),
        },
    }
}

/// Prepare imports (memory and host functions) exposed to the vm guest running
/// transaction code
pub fn prepare_matchmaker_imports(
    wasm_store: &Store,
    initial_memory: Memory,
    tx_code: impl AsRef<[u8]>,
    inject_tx: Sender<Tx>,
) -> ImportObject {
    let env = MatchmakerEnv {
        memory: AnomaMemory::default(),
        inject_tx,
        tx_code: tx_code.as_ref().to_vec(),
    };
    wasmer::imports! {
        // default namespace
        "env" => {
            "memory" => initial_memory,
            "_send_match" => wasmer::Function::new_native_with_env(wasm_store,
                                                                  env.clone(),
                                                                  send_match),
            "_log_string" => wasmer::Function::new_native_with_env(wasm_store,
                                                                  env,
                                                                  matchmaker_log_string),
        },
    }
}

/// Called from tx wasm to request to use the given gas amount
fn tx_charge_gas(env: &TxEnv, used_gas: i32) {
    let gas_meter: &mut BlockGasMeter = unsafe { &mut *(env.gas_meter.get()) };
    // if we run out of gas, we need to stop the execution
    match gas_meter.add(used_gas as _) {
        Err(err) => {
            log::warn!(
                "Stopping transaction execution because of gas error: {}",
                err
            );
            unreachable!()
        }
        _ => {}
    }
}

/// Called from VP wasm to request to use the given gas amount
fn vp_charge_gas(env: &VpEnv, used_gas: i32) {
    let mut gas_meter = env
        .gas_meter
        .lock()
        .expect("Cannot get lock on the gas meter");
    // if we run out of gas, we need to stop the execution
    match gas_meter.add(used_gas as _) {
        Err(err) => {
            log::warn!(
                "Stopping validity predicate execution because of gas error: \
                 {}",
                err
            );
            unreachable!()
        }
        _ => {}
    }
}

/// Storage read function exposed to the wasm VM Tx environment. It will try to
/// read from the write log first and if no entry found then from the storage.
fn tx_storage_read(
    env: &TxEnv,
    key_ptr: u64,
    key_len: u64,
    result_ptr: u64,
) -> u64 {
    let key = env
        .memory
        .read_string(key_ptr, key_len as _)
        .expect("Cannot read the key from memory");

    log::debug!(
        "tx_storage_read {}, key {}, result_ptr {}",
        key,
        key_ptr,
        result_ptr,
    );

    let key = Key::parse(key).expect("Cannot parse the key string");

    // try to read from the write log first
    let write_log: &WriteLog = unsafe { &*(env.write_log.get()) };
    match write_log.read(&key) {
        Some(&write_log::StorageModification::Write { ref value }) => {
            env.memory
                .write_bytes(result_ptr, value)
                .expect("cannot write to memory");
            return 1;
        }
        Some(&write_log::StorageModification::Delete) => {
            // fail, given key has been deleted
            return 0;
        }
        None => {
            // when not found in write log, try to read from the storage
            let storage: &Storage = unsafe { &*(env.storage.get()) };
            let (value, _gas) =
                storage.read(&key).expect("storage read failed");
            match value {
                Some(value) => {
                    env.memory
                        .write_bytes(result_ptr, value)
                        .expect("cannot write to memory");
                    return 1;
                }
                None => {
                    // fail, key not found
                    return 0;
                }
            }
        }
    }
}

/// Storage read function exposed to the wasm VM Tx environment. It will try to
/// read from the write log first and if no entry found then from the storage.
///
/// Returns [`-1`] when the key is not present, or the length of the data when
/// the key is present (the length may be [`0`]).
fn tx_storage_read_varlen(
    env: &TxEnv,
    key_ptr: u64,
    key_len: u64,
    result_ptr: u64,
) -> i64 {
    let key = env
        .memory
        .read_string(key_ptr, key_len as _)
        .expect("Cannot read the key from memory");

    log::debug!(
        "tx_storage_read {}, key {}, result_ptr {}",
        key,
        key_ptr,
        result_ptr,
    );

    let key = Key::parse(key).expect("Cannot parse the key string");

    // try to read from the write log first
    let write_log: &WriteLog = unsafe { &*(env.write_log.get()) };
    match write_log.read(&key) {
        Some(&write_log::StorageModification::Write { ref value }) => {
            let len: i64 =
                value.len().try_into().expect("data length overflow");
            env.memory
                .write_bytes(result_ptr, value)
                .expect("cannot write to memory");
            len
        }
        Some(&write_log::StorageModification::Delete) => {
            // fail, given key has been deleted
            -1
        }
        None => {
            // when not found in write log, try to read from the storage
            let storage: &Storage = unsafe { &*(env.storage.get()) };
            let (value, _gas) =
                storage.read(&key).expect("storage read failed");
            match value {
                Some(value) => {
                    let len: i64 =
                        value.len().try_into().expect("data length overflow");
                    env.memory
                        .write_bytes(result_ptr, value)
                        .expect("cannot write to memory");
                    len
                }
                None => {
                    // fail, key not found
                    -1
                }
            }
        }
    }
}

/// Storage prefix iterator function exposed to the wasm VM Tx environment.
/// It will try to get an iterator from the storage and return the corresponding
/// ID of the interator.
fn tx_storage_iter_prefix(
    env: &TxEnv,
    prefix_ptr: u64,
    prefix_len: u64,
) -> u64 {
    let prefix = env
        .memory
        .read_string(prefix_ptr, prefix_len as _)
        .expect("Cannot read the prefix from memory");

    log::debug!("tx_storage_iter_prefix {}, prefix {}", prefix, prefix_ptr);

    let prefix = Key::parse(prefix).expect("Cannot parse the prefix string");

    let storage: &Storage = unsafe { &*(env.storage.get()) };
    let iterators: &mut PrefixIterators =
        unsafe { &mut *(env.iterators.get()) };
    let iter = storage.iter_prefix(&prefix);
    iterators.insert(iter).id()
}

/// Storage prefix iterator next function exposed to the wasm VM Tx environment.
/// It will read a key value pair from the write log first and if no entry found
/// then from the storage.
fn tx_storage_iter_next(env: &TxEnv, iter_id: u64, result_ptr: u64) -> u64 {
    log::debug!(
        "tx_storage_iter_next iter_id {}, result_ptr {}",
        iter_id,
        result_ptr,
    );

    let write_log: &WriteLog = unsafe { &*(env.write_log.get()) };
    let iterators: &mut PrefixIterators =
        unsafe { &mut *(env.iterators.get()) };
    let iter_id = PrefixIteratorId::new(iter_id);
    while let Some((key, val)) = iterators.next(iter_id) {
        let key = String::from_utf8(key)
            .expect("Cannot convert from bytes to key string");
        match write_log.read(
            &Key::parse(key.clone()).expect("Cannot parse the key string"),
        ) {
            Some(&write_log::StorageModification::Write { ref value }) => {
                let key_val = KeyVal {
                    key,
                    val: value.clone(),
                }
                .try_to_vec()
                .expect("cannot serialize the key value pair");
                env.memory
                    .write_bytes(result_ptr, key_val)
                    .expect("cannot write to memory");
                return 1;
            }
            Some(&write_log::StorageModification::Delete) => {
                // check the next because the key has already deleted
                continue;
            }
            None => {
                let key_val = KeyVal { key, val }
                    .try_to_vec()
                    .expect("cannot serialize the key value pair");
                env.memory
                    .write_bytes(result_ptr, key_val)
                    .expect("cannot write to memory");
                return 1;
            }
        }
    }
    // fail, key not found
    0
}

/// Storage prefix iterator next function exposed to the wasm VM Tx environment.
/// It will try to read from the write log first and if no entry found then from
/// the storage.
///
/// Returns [`-1`] when the key is not present, or the length of the data when
/// the key is present (the length may be [`0`]).
fn tx_storage_iter_next_varlen(
    env: &TxEnv,
    iter_id: u64,
    result_ptr: u64,
) -> i64 {
    log::debug!(
        "tx_storage_iter_next iter_id {}, result_ptr {}",
        iter_id,
        result_ptr,
    );

    let write_log: &WriteLog = unsafe { &*(env.write_log.get()) };
    let iterators: &mut PrefixIterators =
        unsafe { &mut *(env.iterators.get()) };
    let iter_id = PrefixIteratorId::new(iter_id);
    while let Some((key, val)) = iterators.next(iter_id) {
        let key = String::from_utf8(key)
            .expect("Cannot convert from bytes to key string");
        match write_log.read(
            &Key::parse(key.clone()).expect("Cannot parse the key string"),
        ) {
            Some(&write_log::StorageModification::Write { ref value }) => {
                let key_val = KeyVal {
                    key,
                    val: value.clone(),
                }
                .try_to_vec()
                .expect("cannot serialize the key value pair");
                let len: i64 =
                    key_val.len().try_into().expect("data length overflow");
                env.memory
                    .write_bytes(result_ptr, key_val)
                    .expect("cannot write to memory");
                return len;
            }
            Some(&write_log::StorageModification::Delete) => {
                // check the next because the key has already deleted
                continue;
            }
            None => {
                let key_val = KeyVal { key, val }
                    .try_to_vec()
                    .expect("cannot serialize the key value pair");
                let len: i64 =
                    key_val.len().try_into().expect("data length overflow");
                env.memory
                    .write_bytes(result_ptr, key_val)
                    .expect("cannot write to memory");
                return len;
            }
        }
    }
    // key not found
    -1
}

/// Storage write function exposed to the wasm VM Tx environment. The given
/// key/value will be written to the write log.
fn tx_storage_write(
    env: &TxEnv,
    key_ptr: u64,
    key_len: u64,
    val_ptr: u64,
    val_len: u64,
) {
    let key = env
        .memory
        .read_string(key_ptr, key_len as _)
        .expect("Cannot read the key from memory");
    let value = env
        .memory
        .read_bytes(val_ptr, val_len as _)
        .expect("Cannot read the value from memory");

    log::debug!("tx_storage_update {}, {:#?}", key, value);

    let key = Key::parse(key).expect("Cannot parse the key string");

    let write_log: &mut WriteLog = unsafe { &mut *(env.write_log.get()) };
    write_log.write(&key, value);
}

/// Storage delete function exposed to the wasm VM Tx environment. The given
/// key/value will be written as deleted to the write log.
fn tx_storage_delete(env: &TxEnv, key_ptr: u64, key_len: u64) -> u64 {
    let key = env
        .memory
        .read_string(key_ptr, key_len as _)
        .expect("Cannot read the key from memory");

    log::debug!("tx_storage_delete {}", key);

    let key = Key::parse(key).expect("Cannot parse the key string");

    let write_log: &mut WriteLog = unsafe { &mut *(env.write_log.get()) };
    write_log.delete(&key);

    1
}

/// Storage read prior state (before tx execution) function exposed to the wasm
/// VM VP environment. It will try to read from the storage.
fn vp_storage_read_pre(
    env: &VpEnv,
    key_ptr: u64,
    key_len: u64,
    result_ptr: u64,
) -> u64 {
    let key = env
        .memory
        .read_string(key_ptr, key_len as _)
        .expect("Cannot read the key from memory");

    // try to read from the storage
    let key = Key::parse(key).expect("Cannot parse the key string");
    let storage: &Storage = unsafe { &*(env.storage.get()) };
    let (value, _gas) = storage.read(&key).expect("storage read failed");
    log::debug!(
        "vp_storage_read_pre addr {}, key {}, value {:#?}",
        env.addr,
        key,
        value,
    );
    match value {
        Some(value) => {
            env.memory
                .write_bytes(result_ptr, value)
                .expect("cannot write to memory");
            return 1;
        }
        None => {
            // fail, key not found
            return 0;
        }
    }
}

/// Storage read posterior state (after tx execution) function exposed to the
/// wasm VM VP environment. It will try to read from the write log first and if
/// no entry found then from the storage.
fn vp_storage_read_post(
    env: &VpEnv,
    key_ptr: u64,
    key_len: u64,
    result_ptr: u64,
) -> u64 {
    let key = env
        .memory
        .read_string(key_ptr, key_len as _)
        .expect("Cannot read the key from memory");

    log::debug!(
        "vp_storage_read_post {}, key {}, result_ptr {}",
        key,
        key_ptr,
        result_ptr,
    );

    // try to read from the write log first
    let key = Key::parse(key).expect("Cannot parse the key string");
    let write_log: &WriteLog = unsafe { &*(env.write_log.get()) };
    match write_log.read(&key) {
        Some(&write_log::StorageModification::Write { ref value }) => {
            env.memory
                .write_bytes(result_ptr, value)
                .expect("cannot write to memory");
            return 1;
        }
        Some(&write_log::StorageModification::Delete) => {
            // fail, given key has been deleted
            return 0;
        }
        None => {
            // when not found in write log, try to read from the storage
            let storage: &Storage = unsafe { &*(env.storage.get()) };
            let (value, _gas) =
                storage.read(&key).expect("storage read failed");
            match value {
                Some(value) => {
                    env.memory
                        .write_bytes(result_ptr, value)
                        .expect("cannot write to memory");
                    return 1;
                }
                None => {
                    // fail, key not found
                    return 0;
                }
            }
        }
    }
}

/// Storage read prior state (before tx execution) function exposed to the wasm
/// VM VP environment. It will try to read from the storage.
///
/// Returns [`-1`] when the key is not present, or the length of the data when
/// the key is present (the length may be [`0`]).
fn vp_storage_read_pre_varlen(
    env: &VpEnv,
    key_ptr: u64,
    key_len: u64,
    result_ptr: u64,
) -> i64 {
    let key = env
        .memory
        .read_string(key_ptr, key_len as _)
        .expect("Cannot read the key from memory");

    // try to read from the storage
    let key = Key::parse(key).expect("Cannot parse the key string");
    let storage: &Storage = unsafe { &*(env.storage.get()) };
    let (value, _gas) = storage.read(&key).expect("storage read failed");
    log::debug!(
        "vp_storage_read_pre addr {}, key {}, value {:#?}",
        env.addr,
        key,
        value,
    );
    match value {
        Some(value) => {
            let len: i64 =
                value.len().try_into().expect("data length overflow");
            env.memory
                .write_bytes(result_ptr, value)
                .expect("cannot write to memory");
            len
        }
        None => {
            // fail, key not found
            -1
        }
    }
}

/// Storage read posterior state (after tx execution) function exposed to the
/// wasm VM VP environment. It will try to read from the write log first and if
/// no entry found then from the storage.
///
/// Returns [`-1`] when the key is not present, or the length of the data when
/// the key is present (the length may be [`0`]).
fn vp_storage_read_post_varlen(
    env: &VpEnv,
    key_ptr: u64,
    key_len: u64,
    result_ptr: u64,
) -> i64 {
    let key = env
        .memory
        .read_string(key_ptr, key_len as _)
        .expect("Cannot read the key from memory");

    log::debug!(
        "vp_storage_read_post {}, key {}, result_ptr {}",
        key,
        key_ptr,
        result_ptr,
    );

    // try to read from the write log first
    let key = Key::parse(key).expect("Cannot parse the key string");
    let write_log: &WriteLog = unsafe { &*(env.write_log.get()) };
    match write_log.read(&key) {
        Some(&write_log::StorageModification::Write { ref value }) => {
            let len: i64 =
                value.len().try_into().expect("data length overflow");
            env.memory
                .write_bytes(result_ptr, value)
                .expect("cannot write to memory");
            len
        }
        Some(&write_log::StorageModification::Delete) => {
            // fail, given key has been deleted
            -1
        }
        None => {
            // when not found in write log, try to read from the storage
            let storage: &Storage = unsafe { &*(env.storage.get()) };
            let (value, _gas) =
                storage.read(&key).expect("storage read failed");
            match value {
                Some(value) => {
                    let len: i64 =
                        value.len().try_into().expect("data length overflow");
                    env.memory
                        .write_bytes(result_ptr, value)
                        .expect("cannot write to memory");
                    len
                }
                None => {
                    // fail, key not found
                    -1
                }
            }
        }
    }
}

/// Storage prefix iterator function exposed to the wasm VM VP environment.
/// It will try to get an iterator from the storage and return the corresponding
/// ID of the interator.
fn vp_storage_iter_prefix(
    env: &VpEnv,
    prefix_ptr: u64,
    prefix_len: u64,
) -> u64 {
    let prefix = env
        .memory
        .read_string(prefix_ptr, prefix_len as _)
        .expect("Cannot read the prefix from memory");

    log::debug!("vp_storage_iter_prefix {}, prefix {}", prefix, prefix_ptr);

    let prefix = Key::parse(prefix).expect("Cannot parse the prefix string");

    let storage: &Storage = unsafe { &*(env.storage.get()) };
    let iterators: &mut PrefixIterators =
        unsafe { &mut *(env.iterators.get()) };
    let iter = storage.iter_prefix(&prefix);
    iterators.insert(iter).id()
}

/// Storage prefix iterator next (before tx execution) function exposed to the
/// wasm VM VP environment. It will read a key value pair from the storage.
fn vp_storage_iter_pre_next(env: &VpEnv, iter_id: u64, result_ptr: u64) -> u64 {
    log::debug!(
        "vp_storage_iter_pre_next iter_id {}, result_ptr {}",
        iter_id,
        result_ptr,
    );

    let iterators: &mut PrefixIterators =
        unsafe { &mut *(env.iterators.get()) };
    let iter_id = PrefixIteratorId::new(iter_id);
    if let Some((key, val)) = iterators.next(iter_id) {
        let key = String::from_utf8(key)
            .expect("Cannot convert from bytes to key string");
        let key_val = KeyVal { key, val }
            .try_to_vec()
            .expect("cannot serialize the key value pair");
        env.memory
            .write_bytes(result_ptr, key_val)
            .expect("cannot write to memory");
        return 1;
    }
    // key not found
    0
}

/// Storage prefix iterator next (after tx execution) function exposed to the
/// wasm VM VP environment. It will read a key value pair from the write log
/// first and if no entry found then from the storage.
fn vp_storage_iter_post_next(
    env: &VpEnv,
    iter_id: u64,
    result_ptr: u64,
) -> u64 {
    log::debug!(
        "vp_storage_iter_post_next iter_id {}, result_ptr {}",
        iter_id,
        result_ptr,
    );

    let write_log: &WriteLog = unsafe { &*(env.write_log.get()) };
    let iterators: &mut PrefixIterators =
        unsafe { &mut *(env.iterators.get()) };
    let iter_id = PrefixIteratorId::new(iter_id);
    while let Some((key, val)) = iterators.next(iter_id) {
        let key = String::from_utf8(key)
            .expect("Cannot convert from bytes to key string");
        match write_log.read(
            &Key::parse(key.clone()).expect("Cannot parse the key string"),
        ) {
            Some(&write_log::StorageModification::Write { ref value }) => {
                let key_val = KeyVal {
                    key,
                    val: value.clone(),
                }
                .try_to_vec()
                .expect("cannot serialize the key value pair");
                env.memory
                    .write_bytes(result_ptr, key_val)
                    .expect("cannot write to memory");
                return 1;
            }
            Some(&write_log::StorageModification::Delete) => {
                // check the next because the key has already deleted
                continue;
            }
            None => {
                let key_val = KeyVal { key, val }
                    .try_to_vec()
                    .expect("cannot serialize the key value pair");
                env.memory
                    .write_bytes(result_ptr, key_val)
                    .expect("cannot write to memory");
                return 1;
            }
        }
    }
    // key not found
    0
}

/// Storage prefix iterator for prior state (before tx execution) function
/// exposed to the wasm VM VP environment. It will try to read from the storage.
///
/// Returns [`-1`] when the key is not present, or the length of the data when
/// the key is present (the length may be [`0`]).
fn vp_storage_iter_pre_next_varlen(
    env: &VpEnv,
    iter_id: u64,
    result_ptr: u64,
) -> i64 {
    log::debug!(
        "vp_storage_iter_pre_next_varlen iter_id {}, result_ptr {}",
        iter_id,
        result_ptr,
    );

    let iterators: &mut PrefixIterators =
        unsafe { &mut *(env.iterators.get()) };
    let iter_id = PrefixIteratorId::new(iter_id);
    if let Some((key, val)) = iterators.next(iter_id) {
        let key = String::from_utf8(key)
            .expect("Cannot convert from bytes to key string");
        let key_val = KeyVal { key, val }
            .try_to_vec()
            .expect("cannot serialize the key value pair");
        let len: i64 = key_val.len().try_into().expect("data length overflow");
        env.memory
            .write_bytes(result_ptr, key_val)
            .expect("cannot write to memory");
        return len;
    }
    // key not found
    -1
}

/// Storage prefix iterator next for posterior state (after tx execution)
/// function exposed to the wasm VM VP environment. It will try to read from the
/// write log first and if no entry found then from the storage.
///
/// Returns [`-1`] when the key is not present, or the length of the data when
/// the key is present (the length may be [`0`]).
fn vp_storage_iter_post_next_varlen(
    env: &VpEnv,
    iter_id: u64,
    result_ptr: u64,
) -> i64 {
    log::debug!(
        "vp_storage_iter_post_next_varlen iter_id {}, result_ptr {}",
        iter_id,
        result_ptr,
    );

    let write_log: &WriteLog = unsafe { &*(env.write_log.get()) };
    let iterators: &mut PrefixIterators =
        unsafe { &mut *(env.iterators.get()) };
    let iter_id = PrefixIteratorId::new(iter_id);
    while let Some((key, val)) = iterators.next(iter_id) {
        let key = String::from_utf8(key)
            .expect("Cannot convert from bytes to key string");
        match write_log.read(
            &Key::parse(key.clone()).expect("Cannot parse the key string"),
        ) {
            Some(&write_log::StorageModification::Write { ref value }) => {
                let key_val = KeyVal {
                    key,
                    val: value.clone(),
                }
                .try_to_vec()
                .expect("cannot serialize the key value pair");
                let len: i64 =
                    key_val.len().try_into().expect("data length overflow");
                env.memory
                    .write_bytes(result_ptr, key_val)
                    .expect("cannot write to memory");
                return len;
            }
            Some(&write_log::StorageModification::Delete) => {
                // check the next because the key has already deleted
                continue;
            }
            None => {
                let key_val = KeyVal { key, val }
                    .try_to_vec()
                    .expect("cannot serialize the key value pair");
                let len: i64 =
                    key_val.len().try_into().expect("data length overflow");
                env.memory
                    .write_bytes(result_ptr, key_val)
                    .expect("cannot write to memory");
                return len;
            }
        }
    }
    // key not found
    -1
}

/// Log a string from exposed to the wasm VM Tx environment. The message will be
/// printed at the [`log::Level::Info`].
fn tx_log_string(env: &TxEnv, str_ptr: u64, str_len: u64) {
    let str = env
        .memory
        .read_string(str_ptr, str_len as _)
        .expect("Cannot read the string from memory");

    log::info!("WASM Transaction log: {}", str);
}

/// Log a string from exposed to the wasm VM VP environment. The message will be
/// printed at the [`log::Level::Info`].
fn vp_log_string(env: &VpEnv, str_ptr: u64, str_len: u64) {
    let str = env
        .memory
        .read_string(str_ptr, str_len as _)
        .expect("Cannot read the string from memory");

    log::info!("WASM Validity predicate log: {}", str);
}

/// Log a string from exposed to the wasm VM matchmaker environment. The message
/// will be printed at the [`log::Level::Info`].
fn matchmaker_log_string(env: &MatchmakerEnv, str_ptr: u64, str_len: u64) {
    let str = env
        .memory
        .read_string(str_ptr, str_len as _)
        .expect("Cannot read the string from memory");

    log::info!("WASM Matchmaker log: {}", str);
}

/// Inject a transaction from matchmaker's matched intents to the ledger
fn send_match(env: &MatchmakerEnv, data_ptr: u64, data_len: u64) {
    let inject_tx: &Sender<Tx> = &env.inject_tx;
    let tx_data = env
        .memory
        .read_bytes(data_ptr, data_len as _)
        .expect("Cannot read the key from memory");
    let tx = Tx {
        code: env.tx_code.clone(),
        data: Some(tx_data),
    };
    inject_tx.try_send(tx).expect("failed to send tx")
}
