mod index;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use ckb_types::{
    bytes::Bytes,
    core::{BlockView, Capacity, ScriptHashType, TransactionView},
    h256,
    packed::{Byte32, CellOutput, OutPoint, Script},
    prelude::*,
    H160, H256,
};
use clap::{App, AppSettings, Arg, ArgMatches, SubCommand};
use serde::{Deserialize, Serialize};

use super::CliSubCommand;
use crate::utils::{
    arg,
    arg_parser::{
        AddressParser, ArgParser, CapacityParser, FixedHashParser, FromStrParser,
        PrivkeyPathParser, PrivkeyWrapper,
    },
    index::IndexController,
    other::{
        check_capacity, get_address, get_arg_value, get_live_cell_with_cache,
        get_max_mature_number, get_network_type, get_privkey_signer, get_to_data, is_mature,
        read_password, serialize_signature,
    },
    printer::{OutputFormat, Printable},
};
use ckb_index::{with_index_db, IndexDatabase, LiveCellInfo};
use ckb_sdk::{
    constants::{
        DAO_TYPE_HASH, MIN_SECP_CELL_CAPACITY, MULTISIG_TYPE_HASH, ONE_CKB, SIGHASH_TYPE_HASH,
    },
    wallet::{DerivationPath, KeyStore},
    Address, AddressPayload, GenesisInfo, HttpRpcClient, HumanCapacity, MultisigConfig, SignerFn,
    Since, SinceType, TxHelper, SECP256K1,
};
pub use index::start_index_thread;

// Max derived change address to search
const DERIVE_CHANGE_ADDRESS_MAX_LEN: u32 = 10000;

pub struct WalletSubCommand<'a> {
    rpc_client: &'a mut HttpRpcClient,
    key_store: &'a mut KeyStore,
    genesis_info: Option<GenesisInfo>,
    index_dir: PathBuf,
    index_controller: IndexController,
}

impl<'a> WalletSubCommand<'a> {
    pub fn new(
        rpc_client: &'a mut HttpRpcClient,
        key_store: &'a mut KeyStore,
        genesis_info: Option<GenesisInfo>,
        index_dir: PathBuf,
        index_controller: IndexController,
    ) -> WalletSubCommand<'a> {
        WalletSubCommand {
            rpc_client,
            key_store,
            genesis_info,
            index_dir,
            index_controller,
        }
    }

    fn genesis_info(&mut self) -> Result<GenesisInfo, String> {
        if self.genesis_info.is_none() {
            let genesis_block: BlockView = self
                .rpc_client
                .get_block_by_number(0)?
                .expect("Can not get genesis block?")
                .into();
            self.genesis_info = Some(GenesisInfo::from_block(&genesis_block)?);
        }
        Ok(self.genesis_info.clone().unwrap())
    }

    fn with_db<F, T>(&mut self, func: F) -> Result<T, String>
    where
        F: FnOnce(IndexDatabase) -> T,
    {
        let network_type = get_network_type(self.rpc_client)?;
        let genesis_info = self.genesis_info()?;
        let genesis_hash: H256 = genesis_info.header().hash().unpack();
        with_index_db(&self.index_dir, genesis_hash, |backend, cf| {
            let db = IndexDatabase::from_db(backend, cf, network_type, genesis_info, false)?;
            Ok(func(db))
        })
        .map_err(|_err| {
            format!(
                "Index database may not ready, sync process: {}",
                self.index_controller.state().read().to_string()
            )
        })
    }

    pub fn subcommand() -> App<'static, 'static> {
        SubCommand::with_name("wallet")
            .about("Transfer / query balance (with local index) / key utils")
            .subcommands(vec![
                SubCommand::with_name("transfer")
                    .about("Transfer capacity to an address (can have data)")
                    .arg(arg::privkey_path().required_unless(arg::from_account().b.name))
                    .arg(
                        arg::from_account()
                            .required_unless(arg::privkey_path().b.name)
                            .conflicts_with(arg::privkey_path().b.name),
                    )
                    .arg(arg::from_locked_address())
                    .arg(arg::to_address().required(true))
                    .arg(arg::to_data())
                    .arg(arg::to_data_path())
                    .arg(arg::capacity().required(true))
                    .arg(arg::tx_fee().required(true))
                    .arg(arg::derive_receiving_address_length())
                    .arg(arg::derive_change_address().conflicts_with(arg::privkey_path().b.name)),
                SubCommand::with_name("get-capacity")
                    .about("Get capacity by lock script hash or address or lock arg or pubkey")
                    .arg(arg::lock_hash())
                    .arg(arg::address())
                    .arg(arg::pubkey())
                    .arg(arg::lock_arg())
                    .arg(arg::derive_receiving_address_length())
                    .arg(arg::derive_change_address_length())
                    .arg(arg::derived().conflicts_with(arg::lock_hash().b.name)),
                SubCommand::with_name("get-live-cells")
                    .about("Get live cells by lock/type/code  hash")
                    .arg(arg::lock_hash())
                    .arg(arg::type_hash())
                    .arg(arg::code_hash())
                    .arg(arg::address())
                    .arg(arg::live_cells_limit())
                    .arg(arg::from_block_number())
                    .arg(arg::to_block_number())
                    .arg(
                        Arg::with_name("fast-mode")
                            .long("fast-mode")
                            .help("Only visit current range (by --from and --to) of live cells"),
                    ),
                // Move to index subcommand
                SubCommand::with_name("db-metrics")
                    .about("Show index database metrics")
                    .setting(AppSettings::Hidden),
                SubCommand::with_name("top-capacity")
                    .about("Show top n capacity owned by lock script hash")
                    .arg(arg::top_n()),
            ])
    }

    pub fn transfer(
        &mut self,
        args: TransferArgs,
        skip_check: bool,
    ) -> Result<TransactionView, String> {
        let TransferArgs {
            privkey_path,
            from_account,
            from_locked_address,
            password,
            derive_receiving_address_length,
            derive_change_address,
            capacity,
            tx_fee,
            to_address,
            to_data,
        } = args;

        let network_type = get_network_type(self.rpc_client)?;
        let from_privkey: Option<PrivkeyWrapper> = privkey_path
            .map(|input| PrivkeyPathParser.parse(&input))
            .transpose()?;
        let from_account: Option<H160> = from_account
            .map(|input| {
                FixedHashParser::<H160>::default()
                    .parse(&input)
                    .or_else(|err| {
                        let result: Result<Address, String> = AddressParser::new_sighash()
                            .set_network(network_type)
                            .parse(&input);
                        result
                            .map(|address| H160::from_slice(&address.payload().args()).unwrap())
                            .map_err(|_| err)
                    })
            })
            .transpose()?;
        let from_locked_address: Option<Address> = from_locked_address
            .map(|input| {
                AddressParser::default()
                    .set_network(network_type)
                    .set_full_type(MULTISIG_TYPE_HASH.clone())
                    .parse(&input)
            })
            .transpose()?;
        let to_capacity: u64 = CapacityParser.parse(&capacity)?.into();
        let tx_fee: u64 = CapacityParser.parse(&tx_fee)?.into();
        let receiving_address_length: u32 = derive_receiving_address_length
            .map(|input| FromStrParser::<u32>::default().parse(&input))
            .transpose()?
            .unwrap_or(1000);
        let last_change_address_opt: Option<Address> = derive_change_address
            .map(|input| {
                AddressParser::default()
                    .set_network(network_type)
                    .parse(&input)
            })
            .transpose()?;
        let to_address: Address = AddressParser::default()
            .set_network(network_type)
            .parse(&to_address)?;
        let to_data = to_data.unwrap_or_default();

        let (from_address_payload, password) = if let Some(from_privkey) = from_privkey.as_ref() {
            let from_pubkey = secp256k1::PublicKey::from_secret_key(&SECP256K1, from_privkey);
            (AddressPayload::from_pubkey(&from_pubkey), String::new())
        } else {
            let password = if let Some(password) = password {
                password
            } else {
                read_password(false, None)?
            };
            (
                AddressPayload::from_pubkey_hash(from_account.unwrap()),
                password,
            )
        };
        let from_address = Address::new(network_type, from_address_payload.clone());

        if let Some(from_locked_address) = from_locked_address.as_ref() {
            let args = from_locked_address.payload().args();
            let err_prefix = "Invalid from-locked-address's args";
            if args.len() != 28 {
                return Err(format!("{}: invalid {}", err_prefix, args.len()));
            }
            let mut since_bytes = [0u8; 8];
            since_bytes.copy_from_slice(&args[20..]);
            let since = Since::from_raw_value(u64::from_le_bytes(since_bytes));
            if !since.flags_is_valid() {
                return Err(format!("{}: invalid since flags", err_prefix));
            }
            if !since.is_absolute() {
                return Err(format!("{}: only support absolute since value", err_prefix));
            }
            if since.extract_metric().map(|(ty, _)| ty) != Some(SinceType::EpochNumberWithFraction)
            {
                return Err(format!("{}: only support epoch since value", err_prefix));
            }
        }

        let to_address_hash_type = to_address.payload().hash_type();
        let to_address_code_hash: H256 = to_address.payload().code_hash().unpack();
        let to_address_args_len = to_address.payload().args().len();
        if !(to_address_hash_type == ScriptHashType::Type
            && to_address_code_hash == SIGHASH_TYPE_HASH
            && to_address_args_len == 20)
            && !(to_address_hash_type == ScriptHashType::Type
                && to_address_code_hash == MULTISIG_TYPE_HASH
                && (to_address_args_len == 20 || to_address_args_len == 28))
        {
            return Err(format!("Invalid to-address: {}", to_address));
        }
        check_capacity(to_capacity, to_data.len())?;

        let genesis_info = self.genesis_info()?;

        // For check index database is ready
        self.with_db(|_| ())?;
        let index_dir = self.index_dir.clone();
        let genesis_hash = genesis_info.header().hash();
        let genesis_info_clone = genesis_info.clone();

        // The lock hashes for search live cells
        let mut lock_hashes = vec![Script::from(&from_address_payload).calc_script_hash()];
        let mut helper = TxHelper::default();

        let from_lock_arg = H160::from_slice(from_address.payload().args().as_ref()).unwrap();
        let mut path_map: HashMap<H160, DerivationPath> = Default::default();
        let change_address_payload = if let Some(last_change_address) = last_change_address_opt {
            // Behave like HD wallet
            let change_last =
                H160::from_slice(last_change_address.payload().args().as_ref()).unwrap();
            let key_set = self
                .key_store
                .derived_key_set_with_password(
                    &from_lock_arg,
                    password.as_bytes(),
                    receiving_address_length,
                    &change_last,
                    DERIVE_CHANGE_ADDRESS_MAX_LEN,
                )
                .map_err(|err| err.to_string())?;
            for (path, hash160) in key_set.external.iter().chain(key_set.change.iter()) {
                path_map.insert(hash160.clone(), path.clone());
                let payload = AddressPayload::from_pubkey_hash(hash160.clone());
                lock_hashes.push(Script::from(&payload).calc_script_hash());
            }
            last_change_address.payload().clone()
        } else {
            from_address.payload().clone()
        };

        if let Some(from_locked_address) = from_locked_address.as_ref() {
            lock_hashes.insert(
                0,
                Script::from(from_locked_address.payload()).calc_script_hash(),
            );
            for lock_arg in std::iter::once(&from_lock_arg).chain(path_map.keys()) {
                let mut sighash_addresses = Vec::default();
                sighash_addresses.push(AddressPayload::from_pubkey_hash(lock_arg.clone()));
                let require_first_n = 0;
                let threshold = 1;
                let cfg = MultisigConfig::new_with(sighash_addresses, require_first_n, threshold)?;
                if cfg.hash160().as_bytes() == &from_locked_address.payload().args()[0..20] {
                    helper.add_multisig_config(cfg);
                    break;
                }
            }
            if helper.multisig_configs().is_empty() {
                return Err(String::from(
                    "from-locked-address is not created from the key or derived keys",
                ));
            }
        }

        let max_mature_number = get_max_mature_number(self.rpc_client)?;
        let mut from_capacity = 0;
        let mut infos: Vec<LiveCellInfo> = Default::default();
        let mut terminator = |_, info: &LiveCellInfo| {
            if from_capacity >= to_capacity + tx_fee {
                (true, false)
            } else if info.type_hashes.is_none()
                && info.data_bytes == 0
                && is_mature(info, max_mature_number)
            {
                from_capacity += info.capacity;
                infos.push(info.clone());
                (from_capacity >= to_capacity + tx_fee, false)
            } else {
                (false, false)
            }
        };
        if let Err(err) = with_index_db(&index_dir, genesis_hash.unpack(), |backend, cf| {
            IndexDatabase::from_db(backend, cf, network_type, genesis_info_clone, false)
                .map(|db| {
                    for lock_hash in lock_hashes {
                        db.get_live_cells_by_lock(lock_hash, None, &mut terminator);
                    }
                })
                .map_err(Into::into)
        }) {
            return Err(format!(
                "Index database may not ready, sync process: {}, error: {}",
                self.index_controller.state().read().to_string(),
                err.to_string(),
            ));
        }

        if tx_fee > ONE_CKB {
            return Err("Transaction fee can not be more than 1.0 CKB".to_string());
        }
        if to_capacity + tx_fee > from_capacity {
            return Err(format!(
                "Capacity(mature) not enough: {} => {}",
                from_address, from_capacity,
            ));
        }

        let rest_capacity = from_capacity - to_capacity - tx_fee;
        if rest_capacity < MIN_SECP_CELL_CAPACITY && tx_fee + rest_capacity > ONE_CKB {
            return Err("Transaction fee can not be more than 1.0 CKB, please change to-capacity value to adjust".to_string());
        }

        let key_store = self.key_store.clone();
        let mut live_cell_cache: HashMap<(OutPoint, bool), (CellOutput, Bytes)> =
            Default::default();
        let mut get_live_cell_fn = |out_point: OutPoint, with_data: bool| {
            get_live_cell_with_cache(&mut live_cell_cache, self.rpc_client, out_point, with_data)
                .map(|(output, _)| output)
        };
        for info in &infos {
            helper.add_input(
                info.out_point(),
                None,
                &mut get_live_cell_fn,
                &genesis_info,
                skip_check,
            )?;
        }
        let to_output = CellOutput::new_builder()
            .capacity(Capacity::shannons(to_capacity).pack())
            .lock(to_address.payload().into())
            .build();
        helper.add_output(to_output, to_data);
        if rest_capacity >= MIN_SECP_CELL_CAPACITY {
            let change_output = CellOutput::new_builder()
                .capacity(Capacity::shannons(rest_capacity).pack())
                .lock((&change_address_payload).into())
                .build();
            helper.add_output(change_output, Bytes::default());
        }

        let signer = if let Some(from_privkey) = from_privkey {
            get_privkey_signer(from_privkey)
        } else {
            get_keystore_signer(key_store, path_map, from_lock_arg, password)
        };
        for (lock_arg, signature) in
            helper.sign_inputs(signer, &mut get_live_cell_fn, skip_check)?
        {
            helper.add_signature(lock_arg, signature)?;
        }
        let tx = helper.build_tx(&mut get_live_cell_fn, skip_check)?;
        let tx_hash = self
            .rpc_client
            .send_transaction(tx.data())
            .map_err(|err| format!("Send transaction error: {}", err))?;
        assert_eq!(tx.hash(), tx_hash.pack());
        Ok(tx)
    }

    pub fn get_capacity(&mut self, lock_hashes: Vec<Byte32>) -> Result<(u64, u64, u64), String> {
        let max_mature_number = get_max_mature_number(self.rpc_client)?;
        self.with_db(|db| {
            let mut total_capacity = 0;
            let mut dao_capacity = 0;
            let mut immature_capacity = 0;
            let mut terminator = |_idx: usize, info: &LiveCellInfo| {
                if !is_mature(info, max_mature_number) {
                    immature_capacity += info.capacity;
                }
                if info
                    .type_hashes
                    .as_ref()
                    .filter(|(code_hash, _)| code_hash == &DAO_TYPE_HASH)
                    .is_some()
                {
                    dao_capacity += info.capacity;
                }
                total_capacity += info.capacity;
                (false, false)
            };
            for lock_hash in lock_hashes {
                let _ = db.get_live_cells_by_lock(lock_hash, None, &mut terminator);
            }
            (total_capacity, immature_capacity, dao_capacity)
        })
    }

    pub fn get_live_cells<F>(
        &mut self,
        to_number: u64,
        limit: usize,
        mut func: F,
        fast_mode: bool,
    ) -> Result<(LiveCells, Option<(u32, u64)>), String>
    where
        F: FnMut(
            &IndexDatabase,
            &mut dyn FnMut(usize, &LiveCellInfo) -> (bool, bool),
        ) -> Vec<LiveCellInfo>,
    {
        let (infos, total_count, total_capacity, current_count, current_capacity) =
            self.with_db(|db| {
                let mut total_count: u32 = 0;
                let mut total_capacity: u64 = 0;
                let mut current_count: u32 = 0;
                let mut current_capacity: u64 = 0;
                let mut terminator = |idx, info: &LiveCellInfo| {
                    let stop = idx >= limit || info.number > to_number;
                    let push_info = !stop;
                    total_count += 1;
                    total_capacity += info.capacity;
                    if push_info {
                        current_count += 1;
                        current_capacity += info.capacity;
                    }
                    (fast_mode && stop, push_info)
                };
                let infos = func(&db, &mut terminator);
                (
                    infos,
                    total_count,
                    total_capacity,
                    current_count,
                    current_capacity,
                )
            })?;

        let max_mature_number = get_max_mature_number(self.rpc_client)?;
        let live_cells = infos
            .into_iter()
            .map(|info| {
                let mature = is_mature(&info, max_mature_number);
                LiveCell { info, mature }
            })
            .collect::<Vec<_>>();
        let total = if fast_mode {
            None
        } else {
            Some((total_count, total_capacity))
        };
        Ok((
            LiveCells {
                live_cells,
                current_count,
                current_capacity,
            },
            total,
        ))
    }
}

impl<'a> CliSubCommand for WalletSubCommand<'a> {
    fn process(
        &mut self,
        matches: &ArgMatches,
        format: OutputFormat,
        color: bool,
        debug: bool,
    ) -> Result<String, String> {
        match matches.subcommand() {
            ("transfer", Some(m)) => {
                let to_data = get_to_data(m)?;
                let args = TransferArgs {
                    privkey_path: m.value_of("privkey-path").map(|s| s.to_string()),
                    from_account: m.value_of("from-account").map(|s| s.to_string()),
                    from_locked_address: m.value_of("from-locked-address").map(|s| s.to_string()),
                    password: None,
                    capacity: get_arg_value(m, "capacity")?,
                    tx_fee: get_arg_value(m, "tx-fee")?,
                    derive_receiving_address_length: Some(get_arg_value(
                        m,
                        "derive-receiving-address-length",
                    )?),
                    derive_change_address: m
                        .value_of("derive-change-address")
                        .map(|s| s.to_string()),
                    to_address: get_arg_value(m, "to-address")?,
                    to_data: Some(to_data),
                };
                let tx = self.transfer(args, false)?;
                if debug {
                    Ok(ckb_jsonrpc_types::TransactionView::from(tx).render(format, color))
                } else {
                    let tx_hash: H256 = tx.hash().unpack();
                    Ok(tx_hash.render(format, color))
                }
            }
            ("get-capacity", Some(m)) => {
                let lock_hash_opt: Option<H256> =
                    FixedHashParser::<H256>::default().from_matches_opt(m, "lock-hash", false)?;
                let lock_hashes = if let Some(lock_hash) = lock_hash_opt {
                    vec![lock_hash.pack()]
                } else {
                    let network_type = get_network_type(self.rpc_client)?;

                    let receiving_address_length: u32 = FromStrParser::<u32>::default()
                        .from_matches(m, "derive-receiving-address-length")?;
                    let change_address_length: u32 = FromStrParser::<u32>::default()
                        .from_matches(m, "derive-change-address-length")?;
                    let address_payload = if let Some(address_str) = m.value_of("address") {
                        AddressParser::default()
                            .set_network(network_type)
                            .parse(address_str)?
                            .payload()
                            .clone()
                    } else {
                        get_address(Some(network_type), m)?
                    };
                    let mut lock_hashes = vec![Script::from(&address_payload).calc_script_hash()];
                    if m.is_present("derived") {
                        let password = read_password(false, None)?;
                        let lock_arg = H160::from_slice(address_payload.args().as_ref()).unwrap();
                        let key_set = self
                            .key_store
                            .derived_key_set_by_index_with_password(
                                &lock_arg,
                                password.as_bytes(),
                                0,
                                receiving_address_length,
                                0,
                                change_address_length,
                            )
                            .map_err(|err| err.to_string())?;
                        for (_, hash160) in key_set.external.iter().chain(key_set.change.iter()) {
                            let payload = AddressPayload::from_pubkey_hash(hash160.clone());
                            lock_hashes.push(Script::from(&payload).calc_script_hash());
                        }
                    }
                    lock_hashes
                };

                let (total, immature, dao) = self.get_capacity(lock_hashes)?;

                let mut resp =
                    serde_json::json!({ "total": format!("{:#}", HumanCapacity::from(total)) });
                if immature > 0 {
                    resp["immature"] =
                        serde_json::json!(format!("{:#}", HumanCapacity::from(immature)));
                }
                if dao > 0 {
                    let free = total - dao;
                    resp["dao"] = serde_json::json!(format!("{:#}", HumanCapacity::from(dao)));
                    resp["free"] = serde_json::json!(format!("{:#}", HumanCapacity::from(free)));
                }
                Ok(resp.render(format, color))
            }
            ("get-live-cells", Some(m)) => {
                let lock_hash_opt: Option<H256> =
                    FixedHashParser::<H256>::default().from_matches_opt(m, "lock-hash", false)?;
                let type_hash_opt: Option<H256> =
                    FixedHashParser::<H256>::default().from_matches_opt(m, "type-hash", false)?;
                let code_hash_opt: Option<H256> =
                    FixedHashParser::<H256>::default().from_matches_opt(m, "code-hash", false)?;
                let limit: usize = FromStrParser::<usize>::default().from_matches(m, "limit")?;
                let from_number_opt: Option<u64> =
                    FromStrParser::<u64>::default().from_matches_opt(m, "from", false)?;
                let to_number_opt: Option<u64> =
                    FromStrParser::<u64>::default().from_matches_opt(m, "to", false)?;
                let fast_mode = m.is_present("fast-mode");

                let network_type = get_network_type(self.rpc_client)?;
                let lock_hash_opt = if lock_hash_opt.is_none() {
                    let address_opt: Option<Address> = AddressParser::default()
                        .set_network_opt(Some(network_type))
                        .from_matches_opt(m, "address", false)?;
                    address_opt
                        .map(|address| Script::from(address.payload()).calc_script_hash().unpack())
                } else {
                    lock_hash_opt
                };

                if lock_hash_opt.is_none() && type_hash_opt.is_none() && code_hash_opt.is_none() {
                    return Err(
                        "lock-hash or type-hash or code-hash or address is required".to_owned()
                    );
                }

                let to_number = to_number_opt.unwrap_or(std::u64::MAX);
                let (
                    LiveCells {
                        live_cells,
                        current_count,
                        current_capacity,
                    },
                    total,
                ) = self.get_live_cells(
                    to_number,
                    limit,
                    |db, terminator| {
                        if let Some(lock_hash) = &lock_hash_opt {
                            db.get_live_cells_by_lock(
                                lock_hash.clone().pack(),
                                from_number_opt,
                                terminator,
                            )
                        } else if let Some(type_hash) = &type_hash_opt {
                            db.get_live_cells_by_type(
                                type_hash.clone().pack(),
                                from_number_opt,
                                terminator,
                            )
                        } else {
                            db.get_live_cells_by_code(
                                code_hash_opt.clone().unwrap().pack(),
                                from_number_opt,
                                terminator,
                            )
                        }
                    },
                    fast_mode,
                )?;
                let mut resp = serde_json::json!({
                    "live_cells": live_cells.into_iter().map(|live_cell| {
                        let LiveCell{ info, mature } = live_cell;
                        let mut value = serde_json::to_value(&info).unwrap();
                        let mature = serde_json::Value::Bool(mature);
                        let capacity_string = serde_json::Value::String(format!("{:#}", HumanCapacity::from(info.capacity)));
                        let map = value.as_object_mut().unwrap();
                        map.insert("capacity".to_string(), capacity_string);
                        map.insert("mature".to_string(), mature);
                        value
                    }).collect::<Vec<_>>(),
                    "current_count": current_count,
                    "current_capacity": format!("{:#}", HumanCapacity::from(current_capacity)),
                });
                if let Some((total_count, total_capacity)) = total {
                    resp["total_count"] = serde_json::json!(total_count);
                    resp["total_capacity"] =
                        serde_json::json!(format!("{:#}", HumanCapacity::from(total_capacity)));
                }

                Ok(resp.render(format, color))
            }
            ("top-capacity", Some(m)) => {
                let n: usize = m
                    .value_of("number")
                    .map(|n_str| n_str.parse().unwrap())
                    .unwrap();
                let network_type = get_network_type(self.rpc_client)?;
                let resp = self.with_db(|db| {
                    db.get_top_n(n)
                        .into_iter()
                        .map(|(lock_hash, payload_opt, capacity)| {
                            serde_json::json!({
                                "lock_hash": format!("{:#x}", lock_hash),
                                "address": payload_opt.map(|payload| Address::new(network_type, payload).to_string()),
                                "capacity": format!("{:#}", HumanCapacity::from(capacity)),
                            })
                        })
                        .collect::<Vec<_>>()
                })?;
                Ok(resp.render(format, color))
            }
            ("db-metrics", _) => {
                let metrcis = self.with_db(|db| db.get_metrics(None))?;
                let resp = serde_json::to_value(metrcis).map_err(|err| err.to_string())?;
                Ok(resp.render(format, color))
            }
            _ => Err(matches.usage().to_owned()),
        }
    }
}

fn get_keystore_signer(
    key_store: KeyStore,
    path_map: HashMap<H160, DerivationPath>,
    account: H160,
    password: String,
) -> SignerFn {
    Box::new(move |lock_args: &HashSet<H160>, message: &H256| {
        let path: &[_] = if lock_args.contains(&account) {
            &[]
        } else {
            match lock_args.iter().find_map(|lock_arg| path_map.get(lock_arg)) {
                None => return Ok(None),
                Some(path) => path.as_ref(),
            }
        };
        if message == &h256!("0x0") {
            return Ok(Some([0u8; 65]));
        }
        let signature = key_store
            .sign_recoverable_with_password(&account, path, message, password.as_bytes())
            .map_err(|err| err.to_string())?;
        Ok(Some(serialize_signature(&signature)))
    })
}

#[derive(Clone, Debug)]
pub struct TransferArgs {
    pub privkey_path: Option<String>,
    pub from_account: Option<String>,
    pub from_locked_address: Option<String>,
    pub password: Option<String>,
    pub derive_receiving_address_length: Option<String>,
    pub derive_change_address: Option<String>,
    pub capacity: String,
    pub tx_fee: String,
    pub to_address: String,
    pub to_data: Option<Bytes>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveCells {
    pub live_cells: Vec<LiveCell>,
    pub current_count: u32,
    pub current_capacity: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveCell {
    pub info: LiveCellInfo,
    pub mature: bool,
}
