// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::command::Component;
use crate::mock_account::{batch_create_account_and_gas, Account};
use crate::mock_storage::InMemoryObjectStore;
use crate::single_node::SingleValidator;
use crate::tx_generator::{RootObjectCreateTxGenerator, TxGenerator};
use crate::workload::Workload;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use std::collections::{BTreeMap, HashMap};
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;
use sui_types::base_types::{ObjectID, ObjectRef, SuiAddress};
use sui_types::effects::{TransactionEffects, TransactionEffectsAPI};
use sui_types::messages_grpc::HandleTransactionResponse;
use sui_types::mock_checkpoint_builder::ValidatorKeypairProvider;
use sui_types::transaction::{CertifiedTransaction, SignedTransaction, Transaction};
use tracing::info;

pub struct BenchmarkContext {
    validator: SingleValidator,
    user_accounts: BTreeMap<SuiAddress, Account>,
    admin_account: Account,
    benchmark_component: Component,
}

impl BenchmarkContext {
    pub(crate) async fn new(
        workload: Workload,
        benchmark_component: Component,
        checkpoint_size: usize,
    ) -> Self {
        // Increase by 2 so that we could generate one extra sample transaction before benchmarking.
        // as well as reserve 1 account for package publishing.
        let num_accounts = workload.num_accounts() + 2;
        let gas_object_num_per_account = workload.gas_object_num_per_account();
        let total = num_accounts * gas_object_num_per_account;

        info!(
            "Creating {} accounts and {} gas objects",
            num_accounts, total
        );
        let (mut user_accounts, genesis_gas_objects) =
            batch_create_account_and_gas(num_accounts, gas_object_num_per_account).await;
        assert_eq!(genesis_gas_objects.len() as u64, total);
        let (_, admin_account) = user_accounts.pop_last().unwrap();

        info!("Initializing validator");
        let validator =
            SingleValidator::new(&genesis_gas_objects, benchmark_component, checkpoint_size).await;

        Self {
            validator,
            user_accounts,
            admin_account,
            benchmark_component,
        }
    }

    pub(crate) fn validator(&self) -> SingleValidator {
        self.validator.clone()
    }

    pub(crate) async fn publish_package(&mut self) -> ObjectRef {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.extend(["move_package"]);
        let mut gas_objects = self.admin_account.gas_objects.deref().clone();
        let (package, updated_gas) = self
            .validator
            .publish_package(
                path,
                self.admin_account.sender,
                &self.admin_account.keypair,
                gas_objects[0],
            )
            .await;
        gas_objects[0] = updated_gas;
        self.admin_account.gas_objects = Arc::new(gas_objects);
        package
    }

    /// In order to benchmark transactions that can read dynamic fields, we must first create
    /// a root object with dynamic fields for each account address.
    pub(crate) async fn preparing_dynamic_fields(
        &mut self,
        move_package: ObjectID,
        num_dynamic_fields: u64,
    ) -> HashMap<SuiAddress, ObjectRef> {
        let mut root_objects = HashMap::new();

        if num_dynamic_fields == 0 {
            return root_objects;
        }

        info!("Preparing root object with dynamic fields");
        let root_object_create_transactions = self
            .generate_transactions(Arc::new(RootObjectCreateTxGenerator::new(
                move_package,
                num_dynamic_fields,
            )))
            .await;
        let results = self
            .execute_raw_transactions(root_object_create_transactions)
            .await;
        let mut new_gas_objects = HashMap::new();
        for effects in results {
            let (owner, root_object) = effects
                .created()
                .into_iter()
                .filter_map(|(oref, owner)| {
                    owner
                        .get_address_owner_address()
                        .ok()
                        .map(|owner| (owner, oref))
                })
                .next()
                .unwrap();
            root_objects.insert(owner, root_object);
            let gas_object = effects.gas_object().0;
            new_gas_objects.insert(gas_object.0, gas_object);
        }
        self.refresh_gas_objects(new_gas_objects);
        info!("Finished preparing root object with dynamic fields");
        root_objects
    }

    pub(crate) async fn generate_transactions(
        &self,
        tx_generator: Arc<dyn TxGenerator>,
    ) -> Vec<Transaction> {
        info!(
            "{}: Creating {} transactions",
            tx_generator.name(),
            self.user_accounts.len()
        );
        let tasks: FuturesUnordered<_> = self
            .user_accounts
            .values()
            .map(|account| {
                let account = account.clone();
                let tx_generator = tx_generator.clone();
                tokio::spawn(async move { tx_generator.generate_tx(account) })
            })
            .collect();
        let results: Vec<_> = tasks.collect().await;
        results.into_iter().map(|r| r.unwrap()).collect()
    }

    pub(crate) async fn certify_transactions(
        &self,
        transactions: Vec<Transaction>,
    ) -> Vec<CertifiedTransaction> {
        info!("Creating transaction certificates");
        let tasks: FuturesUnordered<_> = transactions
            .into_iter()
            .map(|tx| {
                let validator = self.validator();
                tokio::spawn(async move {
                    let committee = validator.get_committee();
                    let validator = validator.get_validator();
                    let sig = SignedTransaction::sign(0, &tx, &*validator.secret, validator.name);
                    CertifiedTransaction::new(tx.into_data(), vec![sig], committee).unwrap()
                })
            })
            .collect();
        let results: Vec<_> = tasks.collect().await;
        results.into_iter().map(|r| r.unwrap()).collect()
    }

    pub(crate) async fn benchmark_transaction_execution(&self, transactions: Vec<Transaction>) {
        let mut transactions = self.certify_transactions(transactions).await;
        self.execute_sample_transaction(transactions.pop().unwrap().into_unsigned())
            .await;

        let tx_count = transactions.len();
        let start_time = std::time::Instant::now();
        info!(
            "Started executing {} transactions. You can now attach a profiler",
            transactions.len()
        );

        let tasks: FuturesUnordered<_> = transactions
            .into_iter()
            .map(|tx| {
                let validator = self.validator();
                let component = self.benchmark_component;
                tokio::spawn(async move { validator.execute_certificate(tx, component).await })
            })
            .collect();
        let results: Vec<_> = tasks.collect().await;
        results.into_iter().for_each(|r| {
            r.unwrap();
        });

        let elapsed = start_time.elapsed().as_millis() as f64 / 1000f64;
        info!(
            "Execution finished in {}s, TPS={}",
            elapsed,
            tx_count as f64 / elapsed
        );
    }

    pub(crate) async fn benchmark_transaction_execution_in_memory(
        &self,
        mut transactions: Vec<Transaction>,
    ) {
        self.execute_sample_transaction(transactions.pop().unwrap())
            .await;

        let tx_count = transactions.len();
        let in_memory_store = self.validator.create_in_memory_store();
        let start_time = std::time::Instant::now();
        info!(
            "Started executing {} transactions. You can now attach a profiler",
            transactions.len()
        );

        let tasks: FuturesUnordered<_> = transactions
            .into_iter()
            .map(|tx| {
                let validator = self.validator();
                let in_memory_store = in_memory_store.clone();
                tokio::spawn(async move {
                    validator
                        .execute_transaction_in_memory(in_memory_store, tx)
                        .await
                })
            })
            .collect();
        let results: Vec<_> = tasks.collect().await;
        results.into_iter().for_each(|r| {
            r.unwrap();
        });

        let elapsed = start_time.elapsed().as_millis() as f64 / 1000f64;
        info!(
            "Execution finished in {}s, TPS={}, number of DB object reads per transaction: {}",
            elapsed,
            tx_count as f64 / elapsed,
            in_memory_store.get_num_object_reads() as f64 / tx_count as f64
        );
    }

    /// Print out a sample transaction and its effects so that we can get a rough idea
    /// what we are measuring.
    async fn execute_sample_transaction(&self, sample_transaction: Transaction) {
        info!("Sample transaction: {:?}", sample_transaction.data());
        let effects = self
            .validator()
            .execute_raw_transaction(sample_transaction)
            .await;
        info!("Sample effects: {:?}\n\n", effects);
        assert!(effects.status().is_ok());
    }

    /// Benchmark parallel signing a vector of transactions and measure the TPS.
    pub(crate) async fn benchmark_transaction_signing(&self, transactions: Vec<Transaction>) {
        let sample_transaction = &transactions[0];
        info!("Sample transaction: {:?}", sample_transaction.data());

        let tx_count = transactions.len();
        let start_time = std::time::Instant::now();
        self.validator_sign_transactions(transactions).await;
        let elapsed = start_time.elapsed().as_millis() as f64 / 1000f64;
        info!(
            "Transaction signing finished in {}s, TPS={}.",
            elapsed,
            tx_count as f64 / elapsed,
        );
    }

    pub(crate) async fn benchmark_checkpoint_executor(
        &self,
        mut transactions: Vec<Transaction>,
        checkpoint_size: usize,
    ) {
        self.execute_sample_transaction(transactions.pop().unwrap())
            .await;

        info!("Executing all transactions to generate effects");
        let tx_count = transactions.len();
        let in_memory_store = self.validator.create_in_memory_store();
        let effects: BTreeMap<_, _> = self
            .execute_transactions_in_memory(in_memory_store.clone(), transactions.clone())
            .await
            .into_iter()
            .map(|e| (*e.transaction_digest(), e))
            .collect();

        info!("Building checkpoints");
        let validator = self.validator();
        let checkpoints = validator
            .build_checkpoints(in_memory_store, transactions, effects, checkpoint_size)
            .await;
        info!("Built {} checkpoints", checkpoints.len());
        let (mut checkpoint_executor, checkpoint_sender) = validator.create_checkpoint_executor();
        for (checkpoint, contents) in checkpoints {
            let state = validator.get_validator();
            state
                .get_checkpoint_store()
                .insert_verified_checkpoint(&checkpoint)
                .unwrap();
            state
                .database
                .multi_insert_transaction_and_effects(contents.iter())
                .unwrap();
            state
                .get_checkpoint_store()
                .insert_verified_checkpoint_contents(&checkpoint, contents)
                .unwrap();
            state
                .get_checkpoint_store()
                .update_highest_synced_checkpoint(&checkpoint)
                .unwrap();
            checkpoint_sender.send(checkpoint).unwrap();
        }
        let start_time = std::time::Instant::now();
        info!("Starting checkpoint execution. You can now attach a profiler");
        checkpoint_executor
            .run_epoch(validator.get_epoch_store().clone())
            .await;
        let elapsed = start_time.elapsed().as_millis() as f64 / 1000f64;
        info!(
            "Checkpoint execution finished in {}s, TPS={}.",
            elapsed,
            tx_count as f64 / elapsed,
        );
    }

    async fn execute_raw_transactions(
        &self,
        transactions: Vec<Transaction>,
    ) -> Vec<TransactionEffects> {
        let tasks: FuturesUnordered<_> = transactions
            .into_iter()
            .map(|tx| {
                let validator = self.validator();
                tokio::spawn(async move { validator.execute_raw_transaction(tx).await })
            })
            .collect();
        let results: Vec<_> = tasks.collect().await;
        results.into_iter().map(|r| r.unwrap()).collect()
    }

    async fn execute_transactions_in_memory(
        &self,
        store: InMemoryObjectStore,
        transactions: Vec<Transaction>,
    ) -> Vec<TransactionEffects> {
        let tasks: FuturesUnordered<_> = transactions
            .into_iter()
            .map(|tx| {
                let store = store.clone();
                let validator = self.validator();
                tokio::spawn(
                    async move { validator.execute_transaction_in_memory(store, tx).await },
                )
            })
            .collect();
        let results: Vec<_> = tasks.collect().await;
        results.into_iter().map(|r| r.unwrap()).collect()
    }

    fn refresh_gas_objects(&mut self, mut new_gas_objects: HashMap<ObjectID, ObjectRef>) {
        info!("Refreshing gas objects");
        for account in self.user_accounts.values_mut() {
            let refreshed_gas_objects: Vec<_> = account
                .gas_objects
                .iter()
                .map(|oref| {
                    if let Some(new_oref) = new_gas_objects.remove(&oref.0) {
                        new_oref
                    } else {
                        *oref
                    }
                })
                .collect();
            account.gas_objects = Arc::new(refreshed_gas_objects);
        }
    }
    pub(crate) async fn validator_sign_transactions(
        &self,
        transactions: Vec<Transaction>,
    ) -> Vec<HandleTransactionResponse> {
        info!(
            "Started signing {} transactions. You can now attach a profiler",
            transactions.len(),
        );
        let tasks: FuturesUnordered<_> = transactions
            .into_iter()
            .map(|tx| {
                let validator = self.validator();
                tokio::spawn(async move { validator.sign_transaction(tx).await })
            })
            .collect();
        let results: Vec<_> = tasks.collect().await;
        results.into_iter().map(|r| r.unwrap()).collect()
    }
}
