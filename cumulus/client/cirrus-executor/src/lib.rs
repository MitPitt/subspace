// Copyright 2019-2021 Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

//! # Subspace Executor
//!
//! Executors, a separate class of nodes in addition to the consensus nodes (farmers) in Subspace,
//! are designed to reduce the burden of maintaining the chain state for farmers by decoupling the
//! consensus and computation. As an execution layer, executor chain itself does no rely on any
//! typical blockchain consensus like PoW for producing blocks, the block production of executor
//! chain is totally driven by the consensus layer which are collectively maintained by Subspace
//! farmers. Please refer to the white paper [Computation section] for more in-depth description
//! and analysis.
//!
//! Specifically, executors are responsible for producing a [`Bundle`] on each slot from
//! the primary chain and producing an [`ExecutionReceipt`] on each primary block.
//!
//! On each new primary chain slot, executors will collect a set of extrinsics from the transaction
//! pool which are verified to be able to cover the transaction fee, and then use these extrinsics
//! to create a [`Bundle`], submitting it to the primary chain. The submitted bundles are mere blob
//! from the point of primary chain.
//!
//! On each imported primary block, executors will extract all the bundles from the primary block and
//! convert the bundles to a list of extrinsics, construct a custom [`BlockBuilder`] to build a secondary
//! block. The execution trace of all the extrinsics and hooks like
//! `initialize_block`/`finalize_block` will be recorded during the block execution. Once the
//! secondary block has been imported successfully, an executor that wins the election for producing
//! an execution receipt will publish the receipt over the executors network.
//!
//! The execution receipt of each block contains all the intermediate state roots during the block
//! execution, which will be gossiped in the executor network. All executors whether running as an
//! authority or a full node will compute each block and generate an execution receipt independently,
//! once the execution receipt received from the network does not match the one produced locally,
//! a [`FraudProof`] will be generated and reported to the primary chain accordingly.
//!
//! ## Notes
//!
//! Currently, the following terms are interexchangeable in the executor context:
//!
//! - Farmer, consensus node.
//! - Executor, execution/compute node.
//! - Primary chain, consensus layer.
//! - Secondary chain, execution layer.
//!
//! [Computation section]: https://subspace.network/news/subspace-network-whitepaper

mod aux_schema;
mod bundle_processor;
mod bundle_producer;
mod merkle_tree;
#[cfg(test)]
mod tests;
mod worker;

use crate::{
	bundle_processor::BundleProcessor, bundle_producer::BundleProducer, worker::BlockInfo,
};
use cirrus_block_builder::{BlockBuilder, RecordProof};
use cirrus_client_executor_gossip::{Action, GossipMessageHandler};
use cirrus_primitives::{AccountId, SecondaryApi};
use codec::{Decode, Encode};
use futures::{FutureExt, Stream};
use sc_client_api::{AuxStore, BlockBackend};
use sc_network::NetworkService;
use sc_utils::mpsc::TracingUnboundedSender;
use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sp_consensus::{BlockStatus, SelectChain};
use sp_consensus_slots::Slot;
use sp_core::{
	traits::{CodeExecutor, SpawnEssentialNamed, SpawnNamed},
	H256,
};
use sp_executor::{
	Bundle, BundleEquivocationProof, ExecutionPhase, ExecutionReceipt, ExecutorApi, ExecutorId,
	FraudProof, InvalidTransactionProof, OpaqueBundle, SignedBundle, SignedExecutionReceipt,
};
use sp_keystore::SyncCryptoStorePtr;
use sp_runtime::{
	generic::BlockId,
	traits::{Block as BlockT, HashFor, Header as HeaderT, NumberFor, One, Saturating, Zero},
	RuntimeAppPublic, SaturatedConversion,
};
use sp_trie::StorageProof;
use std::{borrow::Cow, sync::Arc};
use subspace_core_primitives::{BlockNumber, Randomness, Sha256Hash};

/// The logging target.
const LOG_TARGET: &str = "cirrus::executor";

/// The implementation of the Cirrus `Executor`.
pub struct Executor<Block, PBlock, Client, PClient, TransactionPool, Backend, E>
where
	Block: BlockT,
	PBlock: BlockT,
{
	// TODO: no longer used in executor, revisit this with ParachainBlockImport together.
	primary_chain_client: Arc<PClient>,
	client: Arc<Client>,
	spawner: Box<dyn SpawnNamed + Send + Sync>,
	transaction_pool: Arc<TransactionPool>,
	backend: Arc<Backend>,
	code_executor: Arc<E>,
	bundle_processor: BundleProcessor<Block, PBlock, Client, PClient, Backend>,
}

impl<Block, PBlock, Client, PClient, TransactionPool, Backend, E> Clone
	for Executor<Block, PBlock, Client, PClient, TransactionPool, Backend, E>
where
	Block: BlockT,
	PBlock: BlockT,
{
	fn clone(&self) -> Self {
		Self {
			primary_chain_client: self.primary_chain_client.clone(),
			client: self.client.clone(),
			spawner: self.spawner.clone(),
			transaction_pool: self.transaction_pool.clone(),
			backend: self.backend.clone(),
			code_executor: self.code_executor.clone(),
			bundle_processor: self.bundle_processor.clone(),
		}
	}
}

type ExecutionReceiptFor<PBlock, Hash> =
	ExecutionReceipt<NumberFor<PBlock>, <PBlock as BlockT>::Hash, Hash>;

type SignedExecutionReceiptFor<PBlock, Hash> =
	SignedExecutionReceipt<NumberFor<PBlock>, <PBlock as BlockT>::Hash, Hash>;

type TransactionFor<Backend, Block> =
	<<Backend as sc_client_api::Backend<Block>>::State as sc_client_api::backend::StateBackend<
		HashFor<Block>,
	>>::Transaction;

impl<Block, PBlock, Client, PClient, TransactionPool, Backend, E>
	Executor<Block, PBlock, Client, PClient, TransactionPool, Backend, E>
where
	Block: BlockT,
	PBlock: BlockT,
	Client:
		HeaderBackend<Block> + BlockBackend<Block> + AuxStore + ProvideRuntimeApi<Block> + 'static,
	Client::Api: SecondaryApi<Block, AccountId>
		+ sp_block_builder::BlockBuilder<Block>
		+ sp_api::ApiExt<
			Block,
			StateBackend = sc_client_api::backend::StateBackendFor<Backend, Block>,
		>,
	for<'b> &'b Client: sc_consensus::BlockImport<
		Block,
		Transaction = sp_api::TransactionFor<Client, Block>,
		Error = sp_consensus::Error,
	>,
	PClient: HeaderBackend<PBlock>
		+ BlockBackend<PBlock>
		+ ProvideRuntimeApi<PBlock>
		+ Send
		+ Sync
		+ 'static,
	PClient::Api: ExecutorApi<PBlock, Block::Hash>,
	Backend: sc_client_api::Backend<Block> + Send + Sync + 'static,
	TransactionFor<Backend, Block>: sp_trie::HashDBT<HashFor<Block>, sp_trie::DBValue>,
	TransactionPool: sc_transaction_pool_api::TransactionPool<Block = Block> + 'static,
	E: CodeExecutor,
{
	/// Create a new instance.
	#[allow(clippy::too_many_arguments)]
	pub async fn new<SE, SC, IBNS, NSNS>(
		primary_chain_client: Arc<PClient>,
		primary_network: Arc<NetworkService<PBlock, PBlock::Hash>>,
		spawn_essential: &SE,
		select_chain: &SC,
		imported_block_notification_stream: IBNS,
		new_slot_notification_stream: NSNS,
		client: Arc<Client>,
		spawner: Box<dyn SpawnNamed + Send + Sync>,
		transaction_pool: Arc<TransactionPool>,
		bundle_sender: Arc<TracingUnboundedSender<SignedBundle<Block::Extrinsic>>>,
		execution_receipt_sender: Arc<
			TracingUnboundedSender<SignedExecutionReceiptFor<PBlock, Block::Hash>>,
		>,
		backend: Arc<Backend>,
		code_executor: Arc<E>,
		is_authority: bool,
		keystore: SyncCryptoStorePtr,
	) -> Result<Self, sp_consensus::Error>
	where
		SE: SpawnEssentialNamed,
		SC: SelectChain<PBlock>,
		IBNS: Stream<Item = NumberFor<PBlock>> + Send + 'static,
		NSNS: Stream<Item = (Slot, Sha256Hash)> + Send + 'static,
	{
		let active_leaves = active_leaves(primary_chain_client.as_ref(), select_chain).await?;

		let bundle_producer = BundleProducer::new(
			primary_chain_client.clone(),
			client.clone(),
			transaction_pool.clone(),
			bundle_sender,
			is_authority,
			keystore.clone(),
		);

		let bundle_processor = BundleProcessor::new(
			primary_chain_client.clone(),
			primary_network,
			client.clone(),
			execution_receipt_sender,
			backend.clone(),
			is_authority,
			keystore,
		);

		spawn_essential.spawn_essential_blocking(
			"executor-worker",
			None,
			worker::start_worker(
				primary_chain_client.clone(),
				bundle_producer,
				bundle_processor.clone(),
				imported_block_notification_stream,
				new_slot_notification_stream,
				active_leaves,
			)
			.boxed(),
		);

		Ok(Self {
			primary_chain_client,
			client,
			spawner,
			transaction_pool,
			backend,
			code_executor,
			bundle_processor,
		})
	}

	/// Checks the status of the given block hash in the Parachain.
	///
	/// Returns `true` if the block could be found and is good to be build on.
	#[allow(unused)]
	fn check_block_status(
		&self,
		hash: Block::Hash,
		number: <Block::Header as HeaderT>::Number,
	) -> bool {
		match self.client.block_status(&BlockId::Hash(hash)) {
			Ok(BlockStatus::Queued) => {
				tracing::debug!(
					target: LOG_TARGET,
					block_hash = ?hash,
					"Skipping candidate production, because block is still queued for import.",
				);
				false
			},
			Ok(BlockStatus::InChainWithState) => true,
			Ok(BlockStatus::InChainPruned) => {
				tracing::error!(
					target: LOG_TARGET,
					"Skipping candidate production, because block `{:?}` is already pruned!",
					hash,
				);
				false
			},
			Ok(BlockStatus::KnownBad) => {
				tracing::error!(
					target: LOG_TARGET,
					block_hash = ?hash,
					"Block is tagged as known bad and is included in the relay chain! Skipping candidate production!",
				);
				false
			},
			Ok(BlockStatus::Unknown) => {
				if number.is_zero() {
					tracing::error!(
						target: LOG_TARGET,
						block_hash = ?hash,
						"Could not find the header of the genesis block in the database!",
					);
				} else {
					tracing::debug!(
						target: LOG_TARGET,
						block_hash = ?hash,
						"Skipping candidate production, because block is unknown.",
					);
				}
				false
			},
			Err(e) => {
				tracing::error!(
					target: LOG_TARGET,
					block_hash = ?hash,
					error = ?e,
					"Failed to get block status.",
				);
				false
			},
		}
	}

	fn submit_bundle_equivocation_proof(&self, bundle_equivocation_proof: BundleEquivocationProof) {
		let primary_chain_client = self.primary_chain_client.clone();
		// TODO: No backpressure
		self.spawner.spawn_blocking(
			"cirrus-submit-bundle-equivocation-proof",
			None,
			async move {
				tracing::debug!(
					target: LOG_TARGET,
					"Submitting bundle equivocation proof in a background task..."
				);
				if let Err(error) =
					primary_chain_client.runtime_api().submit_bundle_equivocation_proof_unsigned(
						&BlockId::Hash(primary_chain_client.info().best_hash),
						bundle_equivocation_proof,
					) {
					tracing::debug!(
						target: LOG_TARGET,
						error = ?error,
						"Failed to submit bundle equivocation proof"
					);
				}
			}
			.boxed(),
		);
	}

	fn submit_fraud_proof(&self, fraud_proof: FraudProof) {
		let primary_chain_client = self.primary_chain_client.clone();
		// TODO: No backpressure
		self.spawner.spawn_blocking(
			"cirrus-submit-fraud-proof",
			None,
			async move {
				tracing::debug!(
					target: LOG_TARGET,
					"Submitting fraud proof in a background task..."
				);
				if let Err(error) = primary_chain_client.runtime_api().submit_fraud_proof_unsigned(
					&BlockId::Hash(primary_chain_client.info().best_hash),
					fraud_proof,
				) {
					tracing::debug!(
						target: LOG_TARGET,
						error = ?error,
						"Failed to submit fraud proof"
					);
				}
			}
			.boxed(),
		);
	}

	fn submit_invalid_transaction_proof(&self, invalid_transaction_proof: InvalidTransactionProof) {
		let primary_chain_client = self.primary_chain_client.clone();
		// TODO: No backpressure
		self.spawner.spawn_blocking(
			"cirrus-submit-invalid-transaction-proof",
			None,
			async move {
				tracing::debug!(
					target: LOG_TARGET,
					"Submitting invalid transaction proof in a background task..."
				);
				if let Err(error) =
					primary_chain_client.runtime_api().submit_invalid_transaction_proof_unsigned(
						&BlockId::Hash(primary_chain_client.info().best_hash),
						invalid_transaction_proof,
					) {
					tracing::debug!(
						target: LOG_TARGET,
						error = ?error,
						"Failed to submit invalid transaction proof"
					);
				}
			}
			.boxed(),
		);
	}

	fn header(&self, at: Block::Hash) -> Result<Block::Header, sp_blockchain::Error> {
		self.client
			.header(BlockId::Hash(at))?
			.ok_or_else(|| sp_blockchain::Error::Backend(format!("Header not found for {:?}", at)))
	}

	fn block_body(&self, at: Block::Hash) -> Result<Vec<Block::Extrinsic>, sp_blockchain::Error> {
		self.client.block_body(&BlockId::Hash(at))?.ok_or_else(|| {
			sp_blockchain::Error::Backend(format!("Block body not found for {:?}", at))
		})
	}

	fn create_extrinsic_execution_proof(
		&self,
		extrinsic_index: usize,
		parent_header: &Block::Header,
		current_hash: Block::Hash,
		prover: &subspace_fraud_proof::ExecutionProver<Block, Backend, E>,
	) -> Result<(StorageProof, ExecutionPhase), GossipMessageError> {
		let extrinsics = self.block_body(current_hash)?;

		let encoded_extrinsic = extrinsics
			.get(extrinsic_index)
			.ok_or(GossipMessageError::InvalidExtrinsicIndex {
				index: extrinsic_index,
				max: extrinsics.len() - 1,
			})?
			.encode();

		let execution_phase = ExecutionPhase::ApplyExtrinsic { call_data: encoded_extrinsic };

		let block_builder = BlockBuilder::new(
			&*self.client,
			parent_header.hash(),
			*parent_header.number(),
			RecordProof::No,
			Default::default(),
			&*self.backend,
			extrinsics,
		)?;
		let storage_changes = block_builder.prepare_storage_changes_before(extrinsic_index)?;

		let delta = storage_changes.transaction;
		let post_delta_root = storage_changes.transaction_storage_root;
		let execution_proof = prover.prove_execution(
			BlockId::Hash(parent_header.hash()),
			&execution_phase,
			Some((delta, post_delta_root)),
		)?;

		Ok((execution_proof, execution_phase))
	}

	/// The background is that a receipt received from the network points to a future block
	/// from the local view, so we need to wait for the receipt for the block at the same
	/// height to be produced locally in order to check the validity of the external receipt.
	async fn wait_for_local_future_receipt(
		&self,
		secondary_block_hash: Block::Hash,
		secondary_block_number: <Block::Header as HeaderT>::Number,
		tx: crossbeam::channel::Sender<
			sp_blockchain::Result<ExecutionReceiptFor<PBlock, Block::Hash>>,
		>,
	) -> Result<(), GossipMessageError> {
		loop {
			match aux_schema::load_execution_receipt(&*self.client, secondary_block_hash) {
				Ok(Some(local_receipt)) =>
					return tx.send(Ok(local_receipt)).map_err(|_| GossipMessageError::SendError),
				Ok(None) => {
					// TODO: test how this works under the primary forks.
					//       ref https://github.com/subspace/subspace/pull/250#discussion_r804247551
					//
					// Whether or not the best execution chain number on primary chain has been
					// updated, the local client has proceeded to a higher block, that means the receipt
					// of `block_hash` received from the network does not match the local one,
					// we should just send back the local receipt at the same height.
					if self.client.info().best_number >= secondary_block_number {
						let local_block_hash = self
							.client
							.expect_block_hash_from_id(&BlockId::Number(secondary_block_number))?;
						let local_receipt_result =
							aux_schema::load_execution_receipt(&*self.client, local_block_hash)?
								.ok_or_else(|| {
									sp_blockchain::Error::Backend(format!(
										"Execution receipt not found for {:?}",
										local_block_hash
									))
								});
						return tx
							.send(local_receipt_result)
							.map_err(|_| GossipMessageError::SendError)
					} else {
						tokio::time::sleep(std::time::Duration::from_millis(100)).await;
					}
				},
				Err(e) => return tx.send(Err(e)).map_err(|_| GossipMessageError::SendError),
			}
		}
	}

	/// Processes the bundles extracted from the primary block.
	// TODO: Remove this whole method, `self.bundle_processor` as a property and fix
	// `set_new_code_should_work` test to do an actual runtime upgrade
	#[doc(hidden)]
	pub async fn process_bundles(
		self,
		primary_info: (PBlock::Hash, NumberFor<PBlock>),
		bundles: Vec<OpaqueBundle>,
		shuffling_seed: Randomness,
		maybe_new_runtime: Option<Cow<'static, [u8]>>,
	) {
		if let Err(err) = self
			.bundle_processor
			.process_bundles(primary_info, bundles, shuffling_seed, maybe_new_runtime)
			.await
		{
			tracing::error!(
				target: LOG_TARGET,
				?primary_info,
				error = ?err,
				"Error at processing bundles.",
			);
		}
	}
}

/// Error type for cirrus gossip handling.
#[derive(Debug, thiserror::Error)]
pub enum GossipMessageError {
	#[error("Bundle equivocation error")]
	BundleEquivocation,
	#[error("State root not using H256")]
	InvalidStateRootType,
	#[error("Invalid extrinsic index for creating the execution proof, got: {index}, max: {max}")]
	InvalidExtrinsicIndex { index: usize, max: usize },
	#[error(transparent)]
	Client(Box<sp_blockchain::Error>),
	#[error(transparent)]
	RuntimeApi(#[from] sp_api::ApiError),
	#[error(transparent)]
	RecvError(#[from] crossbeam::channel::RecvError),
	#[error("Failed to send local receipt result because the channel is disconnected")]
	SendError,
	#[error("The signature of bundle is invalid")]
	BadBundleSignature,
	#[error("Invalid bundle author, got: {got}, expected: {expected}")]
	InvalidBundleAuthor { got: ExecutorId, expected: ExecutorId },
	#[error("The signature of execution receipt is invalid")]
	BadExecutionReceiptSignature,
	#[error("Invalid execution receipt author, got: {got}, expected: {expected}")]
	InvalidExecutionReceiptAuthor { got: ExecutorId, expected: ExecutorId },
}

impl From<sp_blockchain::Error> for GossipMessageError {
	fn from(error: sp_blockchain::Error) -> Self {
		Self::Client(Box::new(error))
	}
}

impl<Block, PBlock, Client, PClient, TransactionPool, Backend, E>
	GossipMessageHandler<PBlock, Block>
	for Executor<Block, PBlock, Client, PClient, TransactionPool, Backend, E>
where
	Block: BlockT,
	PBlock: BlockT,
	Client: HeaderBackend<Block>
		+ BlockBackend<Block>
		+ ProvideRuntimeApi<Block>
		+ AuxStore
		+ Send
		+ Sync
		+ 'static,
	Client::Api: SecondaryApi<Block, AccountId>
		+ sp_block_builder::BlockBuilder<Block>
		+ sp_api::ApiExt<
			Block,
			StateBackend = sc_client_api::backend::StateBackendFor<Backend, Block>,
		>,
	for<'b> &'b Client: sc_consensus::BlockImport<
		Block,
		Transaction = sp_api::TransactionFor<Client, Block>,
		Error = sp_consensus::Error,
	>,
	PClient: HeaderBackend<PBlock>
		+ BlockBackend<PBlock>
		+ ProvideRuntimeApi<PBlock>
		+ Send
		+ Sync
		+ 'static,
	PClient::Api: ExecutorApi<PBlock, Block::Hash>,
	Backend: sc_client_api::Backend<Block> + Send + Sync + 'static,
	TransactionFor<Backend, Block>: sp_trie::HashDBT<HashFor<Block>, sp_trie::DBValue>,
	TransactionPool: sc_transaction_pool_api::TransactionPool<Block = Block> + 'static,
	E: CodeExecutor,
{
	type Error = GossipMessageError;

	fn on_bundle(
		&self,
		SignedBundle { bundle, signature, signer }: &SignedBundle<Block::Extrinsic>,
	) -> Result<Action, Self::Error> {
		let check_equivocation = |_bundle: &Bundle<Block::Extrinsic>| {
			// TODO: check bundle equivocation
			let bundle_is_an_equivocation = false;
			if bundle_is_an_equivocation {
				Some(BundleEquivocationProof::dummy_at(bundle.header.slot_number))
			} else {
				None
			}
		};

		// A bundle equivocation occurs.
		if let Some(equivocation_proof) = check_equivocation(bundle) {
			self.submit_bundle_equivocation_proof(equivocation_proof);
			return Err(GossipMessageError::BundleEquivocation)
		}

		let bundle_exists = false;

		if bundle_exists {
			Ok(Action::Empty)
		} else {
			let primary_hash =
				PBlock::Hash::decode(&mut bundle.header.primary_hash.encode().as_slice())
					.expect("Hash type must be correct");

			if !signer.verify(&bundle.hash(), signature) {
				return Err(Self::Error::BadBundleSignature)
			}

			let expected_executor_id = self
				.primary_chain_client
				.runtime_api()
				.executor_id(&BlockId::Hash(primary_hash))?;
			if *signer != expected_executor_id {
				// TODO: handle the misbehavior.

				return Err(Self::Error::InvalidBundleAuthor {
					got: signer.clone(),
					expected: expected_executor_id,
				})
			}

			for extrinsic in bundle.extrinsics.iter() {
				let tx_hash = self.transaction_pool.hash_of(extrinsic);

				if self.transaction_pool.ready_transaction(&tx_hash).is_some() {
					// TODO: Set the status of each tx in the bundle to seen
				} else {
					// TODO: check the legality
					//
					// if illegal => illegal tx proof
					let invalid_transaction_proof = InvalidTransactionProof;

					self.submit_invalid_transaction_proof(invalid_transaction_proof);
				}
			}

			// TODO: all checks pass, add to the bundle pool

			Ok(Action::RebroadcastBundle)
		}
	}

	/// Checks the execution receipt from the executor peers.
	fn on_execution_receipt(
		&self,
		signed_execution_receipt: &SignedExecutionReceiptFor<PBlock, Block::Hash>,
	) -> Result<Action, Self::Error> {
		let SignedExecutionReceipt { execution_receipt, signature, signer } =
			signed_execution_receipt;

		if !signer.verify(&execution_receipt.hash(), signature) {
			return Err(Self::Error::BadExecutionReceiptSignature)
		}

		let expected_executor_id = self
			.primary_chain_client
			.runtime_api()
			.executor_id(&BlockId::Hash(execution_receipt.primary_hash))?;
		if *signer != expected_executor_id {
			// TODO: handle the misbehavior.

			return Err(Self::Error::InvalidExecutionReceiptAuthor {
				got: signer.clone(),
				expected: expected_executor_id,
			})
		}

		let primary_number = execution_receipt.primary_number;
		let best_execution_chain_number = self
			.primary_chain_client
			.runtime_api()
			.best_execution_chain_number(&BlockId::Hash(
				self.primary_chain_client.info().best_hash,
			))?;

		// Just ignore it if the receipt is too old and has been pruned.
		if aux_schema::target_receipt_is_pruned(
			best_execution_chain_number.saturated_into(),
			primary_number.saturated_into(),
		) {
			return Ok(Action::Empty)
		}

		let block_hash = execution_receipt.secondary_hash;
		let block_number = <NumberFor<Block>>::decode(&mut primary_number.encode().as_slice())
			.expect("Primary number and secondary number must use the same type; qed");

		// TODO: more efficient execution receipt checking strategy?
		let local_receipt = if let Some(local_receipt) =
			aux_schema::load_execution_receipt(&*self.client, block_hash)?
		{
			local_receipt
		} else {
			// Wait for the local execution receipt until it's ready.
			let (tx, rx) = crossbeam::channel::bounded::<
				sp_blockchain::Result<ExecutionReceiptFor<PBlock, Block::Hash>>,
			>(1);
			let executor = self.clone();
			self.spawner.spawn(
				"wait-for-local-execution-receipt",
				None,
				async move {
					if let Err(err) =
						executor.wait_for_local_future_receipt(block_hash, block_number, tx).await
					{
						tracing::error!(
							target: LOG_TARGET,
							?err,
							"Error occurred while waiting for the local receipt"
						);
					}
				}
				.boxed(),
			);
			rx.recv()??
		};

		// TODO: What happens for this obvious error?
		if local_receipt.trace.len() != execution_receipt.trace.len() {}

		if let Some((local_trace_idx, local_root)) = local_receipt
			.trace
			.iter()
			.enumerate()
			.zip(execution_receipt.trace.iter().enumerate())
			.find_map(|((local_idx, local_root), (_, external_root))| {
				if local_root != external_root {
					Some((local_idx, local_root))
				} else {
					None
				}
			}) {
			let header = self.header(execution_receipt.secondary_hash)?;
			let parent_header = self.header(*header.parent_hash())?;

			// TODO: avoid the encode & decode?
			let as_h256 = |state_root: &Block::Hash| {
				H256::decode(&mut state_root.encode().as_slice())
					.map_err(|_| Self::Error::InvalidStateRootType)
			};

			let prover = subspace_fraud_proof::ExecutionProver::new(
				self.backend.clone(),
				self.code_executor.clone(),
				self.spawner.clone() as Box<dyn SpawnNamed>,
			);

			let parent_number = TryInto::<BlockNumber>::try_into(*parent_header.number())
				.unwrap_or_else(|_| panic!("Parent number must fit into u32; qed"));

			// TODO: abstract the execution proof impl to be reusable in the test.
			let fraud_proof = if local_trace_idx == 0 {
				// `initialize_block` execution proof.
				let pre_state_root = as_h256(parent_header.state_root())?;
				let post_state_root = as_h256(local_root)?;

				let new_header = Block::Header::new(
					block_number,
					Default::default(),
					Default::default(),
					parent_header.hash(),
					Default::default(),
				);
				let execution_phase =
					ExecutionPhase::InitializeBlock { call_data: new_header.encode() };

				let proof = prover.prove_execution::<TransactionFor<Backend, Block>>(
					BlockId::Hash(parent_header.hash()),
					&execution_phase,
					None,
				)?;

				FraudProof {
					parent_number,
					parent_hash: as_h256(&parent_header.hash())?,
					pre_state_root,
					post_state_root,
					proof,
					execution_phase,
				}
			} else if local_trace_idx == local_receipt.trace.len() - 1 {
				// `finalize_block` execution proof.
				let pre_state_root = as_h256(&execution_receipt.trace[local_trace_idx - 1])?;
				let post_state_root = as_h256(local_root)?;
				let execution_phase = ExecutionPhase::FinalizeBlock;

				let block_builder = BlockBuilder::new(
					&*self.client,
					parent_header.hash(),
					*parent_header.number(),
					RecordProof::No,
					Default::default(),
					&*self.backend,
					self.block_body(execution_receipt.secondary_hash)?,
				)?;
				let storage_changes =
					block_builder.prepare_storage_changes_before_finalize_block()?;

				let delta = storage_changes.transaction;
				let post_delta_root = storage_changes.transaction_storage_root;

				let proof = prover.prove_execution(
					BlockId::Hash(parent_header.hash()),
					&execution_phase,
					Some((delta, post_delta_root)),
				)?;

				FraudProof {
					parent_number,
					parent_hash: as_h256(&parent_header.hash())?,
					pre_state_root,
					post_state_root,
					proof,
					execution_phase,
				}
			} else {
				// Regular extrinsic execution proof.
				let pre_state_root = as_h256(&execution_receipt.trace[local_trace_idx - 1])?;
				let post_state_root = as_h256(local_root)?;

				let (proof, execution_phase) = self.create_extrinsic_execution_proof(
					local_trace_idx - 1,
					&parent_header,
					execution_receipt.secondary_hash,
					&prover,
				)?;

				// TODO: proof should be a CompactProof.
				FraudProof {
					parent_number,
					parent_hash: as_h256(&parent_header.hash())?,
					pre_state_root,
					post_state_root,
					proof,
					execution_phase,
				}
			};

			self.submit_fraud_proof(fraud_proof);

			Ok(Action::Empty)
		} else {
			Ok(Action::RebroadcastExecutionReceipt)
		}
	}
}

/// Returns the active leaves the overseer should start with.
async fn active_leaves<PBlock, PClient, SC>(
	client: &PClient,
	select_chain: &SC,
) -> Result<Vec<BlockInfo<PBlock>>, sp_consensus::Error>
where
	PBlock: BlockT,
	PClient: HeaderBackend<PBlock> + ProvideRuntimeApi<PBlock> + 'static,
	SC: SelectChain<PBlock>,
{
	let best_block = select_chain.best_chain().await?;

	// No leaves if starting from the genesis.
	if best_block.number().is_zero() {
		return Ok(Vec::new())
	}

	let mut leaves = select_chain
		.leaves()
		.await
		.unwrap_or_default()
		.into_iter()
		.filter_map(|hash| {
			let number = client.number(hash).ok()??;

			// Only consider leaves that are in maximum an uncle of the best block.
			if number < best_block.number().saturating_sub(One::one()) || hash == best_block.hash()
			{
				return None
			};

			let parent_hash = *client.header(BlockId::Hash(hash)).ok()??.parent_hash();

			Some(BlockInfo { hash, parent_hash, number })
		})
		.collect::<Vec<_>>();

	// Sort by block number and get the maximum number of leaves
	leaves.sort_by_key(|b| b.number);

	leaves.push(BlockInfo {
		hash: best_block.hash(),
		parent_hash: *best_block.parent_hash(),
		number: *best_block.number(),
	});

	/// The maximum number of active leaves we forward to the [`Overseer`] on startup.
	const MAX_ACTIVE_LEAVES: usize = 4;

	Ok(leaves.into_iter().rev().take(MAX_ACTIVE_LEAVES).collect())
}
