// Copyright 2017 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Parachain statement table meant to be shared with a message router
//! and a consensus proposer.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use table::{self, Table, Context as TableContextTrait};
use polkadot_primitives::{Hash, SessionKey};
use polkadot_primitives::parachain::{Id as ParaId, BlockData, Collation, Extrinsic, CandidateReceipt};

use parking_lot::Mutex;
use futures::{future, prelude::*};

use super::{GroupInfo, TableRouter};
use self::includable::IncludabilitySender;

mod includable;

pub use self::includable::Includable;
pub use table::{SignedStatement, Statement};
pub use table::generic::Statement as GenericStatement;

struct TableContext {
	parent_hash: Hash,
	key: Arc<::ed25519::Pair>,
	groups: HashMap<ParaId, GroupInfo>,
}

impl table::Context for TableContext {
	fn is_member_of(&self, authority: &SessionKey, group: &ParaId) -> bool {
		self.groups.get(group).map_or(false, |g| g.validity_guarantors.contains(authority))
	}

	fn is_availability_guarantor_of(&self, authority: &SessionKey, group: &ParaId) -> bool {
		self.groups.get(group).map_or(false, |g| g.availability_guarantors.contains(authority))
	}

	fn requisite_votes(&self, group: &ParaId) -> (usize, usize) {
		self.groups.get(group).map_or(
			(usize::max_value(), usize::max_value()),
			|g| (g.needed_validity, g.needed_availability),
		)
	}
}

impl TableContext {
	fn local_id(&self) -> SessionKey {
		self.key.public().into()
	}

	fn sign_statement(&self, statement: table::Statement) -> table::SignedStatement {
		let signature = ::sign_table_statement(&statement, &self.key, &self.parent_hash).into();

		table::SignedStatement {
			statement,
			signature,
			sender: self.local_id(),
		}
	}
}

// A shared table object.
struct SharedTableInner {
	table: Table<TableContext>,
	proposed_digest: Option<Hash>,
	checked_validity: HashSet<Hash>,
	checked_availability: HashSet<Hash>,
	trackers: Vec<IncludabilitySender>,
}

impl SharedTableInner {
	// Import a single statement. Provide a handle to a table router and a function
	// used to determine if a referenced candidate is valid.
	//
	// the statement producer, if any, will produce only statements concerning the same candidate
	// as the one just imported
	fn import_remote_statement<R: TableRouter>(
		&mut self,
		context: &TableContext,
		router: &R,
		statement: table::SignedStatement,
	) -> Option<StatementProducer<
		<R::FetchCandidate as IntoFuture>::Future,
		<R::FetchExtrinsic as IntoFuture>::Future,
	>> {
		let summary = match self.table.import_statement(context, statement) {
			Some(summary) => summary,
			None => return None,
		};

		self.update_trackers(&summary.candidate, context);

		let local_id = context.local_id();

		let is_validity_member = context.is_member_of(&local_id, &summary.group_id);
		let is_availability_member =
			context.is_availability_guarantor_of(&local_id, &summary.group_id);

		let digest = &summary.candidate;

		// TODO: consider a strategy based on the number of candidate votes as well.
		// only check validity if this wasn't locally proposed.
		let checking_validity = is_validity_member
			&& self.proposed_digest.as_ref().map_or(true, |d| d != digest)
			&& self.checked_validity.insert(digest.clone());

		let checking_availability = is_availability_member
			&& self.checked_availability.insert(digest.clone());

		let work = if checking_validity || checking_availability {
			match self.table.get_candidate(&digest) {
				None => None, // TODO: handle table inconsistency somehow?
				Some(candidate) => {
					let fetch_block_data =
						router.fetch_block_data(candidate).into_future().fuse();

					let fetch_extrinsic = if checking_availability {
						Some(
							router.fetch_extrinsic_data(candidate).into_future().fuse()
						)
					} else {
						None
					};

					Some(Work {
						candidate_receipt: candidate.clone(),
						fetch_block_data,
						fetch_extrinsic,
						evaluate: checking_validity,
					})
				}
			}
		} else {
			None
		};

		work.map(|work| StatementProducer {
			produced_statements: Default::default(),
			work
		})
	}

	fn update_trackers(&mut self, candidate: &Hash, context: &TableContext) {
		let includable = self.table.candidate_includable(candidate, context);
		for i in (0..self.trackers.len()).rev() {
			if self.trackers[i].update_candidate(candidate.clone(), includable) {
				self.trackers.swap_remove(i);
			}
		}
	}
}

/// Produced statements about a specific candidate.
/// Both may be `None`.
#[derive(Default)]
pub struct ProducedStatements {
	/// A statement about the validity of the candidate.
	pub validity: Option<table::Statement>,
	/// A statement about availability of data. If this is `Some`,
	/// then `block_data` and `extrinsic` should be `Some` as well.
	pub availability: Option<table::Statement>,
	/// Block data to ensure availability of.
	pub block_data: Option<BlockData>,
	/// Extrinsic data to ensure availability of.
	pub extrinsic: Option<Extrinsic>,
}

/// Future that produces statements about a specific candidate.
pub struct StatementProducer<D: Future, E: Future> {
	produced_statements: ProducedStatements,
	work: Work<D, E>,
}

impl<D: Future, E: Future> StatementProducer<D, E> {
	/// Attach a function for verifying fetched collation to the statement producer.
	/// This will transform it into a future.
	///
	/// The collation-checking function should return `true` if known to be valid,
	/// `false` if known to be invalid, and `None` if unable to determine.
	pub fn prime<C: FnMut(Collation) -> Option<bool>>(self, check_candidate: C) -> PrimedStatementProducer<D, E, C> {
		PrimedStatementProducer {
			inner: self,
			check_candidate,
		}
	}
}

struct Work<D: Future, E: Future> {
	candidate_receipt: CandidateReceipt,
	fetch_block_data: future::Fuse<D>,
	fetch_extrinsic: Option<future::Fuse<E>>,
	evaluate: bool,
}

/// Primed statement producer.
pub struct PrimedStatementProducer<D: Future, E: Future, C> {
	inner: StatementProducer<D, E>,
	check_candidate: C,
}

impl<D, E, C, Err> Future for PrimedStatementProducer<D, E, C>
	where
		D: Future<Item=BlockData,Error=Err>,
		E: Future<Item=Extrinsic,Error=Err>,
		C: FnMut(Collation) -> Option<bool>,
{
	type Item = ProducedStatements;
	type Error = Err;

	fn poll(&mut self) -> Poll<ProducedStatements, Err> {
		let work = &mut self.inner.work;

		if let Async::Ready(block_data) = work.fetch_block_data.poll()? {
			self.inner.produced_statements.block_data = Some(block_data.clone());
			if work.evaluate {
				let is_good = (self.check_candidate)(Collation {
					block_data,
					receipt: work.candidate_receipt.clone(),
				});

				let hash = work.candidate_receipt.hash();
				self.inner.produced_statements.validity = match is_good {
					Some(true) => Some(GenericStatement::Valid(hash)),
					Some(false) => Some(GenericStatement::Invalid(hash)),
					None => None,
				};
			}
		}

		if let Some(ref mut fetch_extrinsic) = work.fetch_extrinsic {
			if let Async::Ready(extrinsic) = fetch_extrinsic.poll()? {
				self.inner.produced_statements.extrinsic = Some(extrinsic);
			}
		}

		let done = self.inner.produced_statements.block_data.is_some() && {
			if work.evaluate {
				true
			} else if self.inner.produced_statements.extrinsic.is_some() {
				self.inner.produced_statements.availability =
					Some(GenericStatement::Available(work.candidate_receipt.hash()));

				true
			} else {
				false
			}
		};

		if done {
			Ok(Async::Ready(::std::mem::replace(&mut self.inner.produced_statements, Default::default())))
		} else {
			Ok(Async::NotReady)
		}
	}
}

/// A shared table object.
pub struct SharedTable {
	context: Arc<TableContext>,
	inner: Arc<Mutex<SharedTableInner>>,
}

impl Clone for SharedTable {
	fn clone(&self) -> Self {
		SharedTable {
			context: self.context.clone(),
			inner: self.inner.clone(),
		}
	}
}

impl SharedTable {
	/// Create a new shared table.
	///
	/// Provide the key to sign with, and the parent hash of the relay chain
	/// block being built.
	pub fn new(groups: HashMap<ParaId, GroupInfo>, key: Arc<::ed25519::Pair>, parent_hash: Hash) -> Self {
		SharedTable {
			context: Arc::new(TableContext { groups, key, parent_hash }),
			inner: Arc::new(Mutex::new(SharedTableInner {
				table: Table::default(),
				proposed_digest: None,
				checked_validity: HashSet::new(),
				checked_availability: HashSet::new(),
				trackers: Vec::new(),
			}))
		}
	}

	/// Get the parent hash this table should hold statements localized to.
	pub fn consensus_parent_hash(&self) -> &Hash {
		&self.context.parent_hash
	}

	/// Get the local validator session key.
	pub fn session_key(&self) -> SessionKey {
		self.context.local_id()
	}

	/// Get group info.
	pub fn group_info(&self) -> &HashMap<ParaId, GroupInfo> {
		&self.context.groups
	}

	/// Import a single statement with remote source, whose signature has already been checked.
	///
	/// The statement producer, if any, will produce only statements concerning the same candidate
	/// as the one just imported
	pub fn import_remote_statement<R: TableRouter>(
		&self,
		router: &R,
		statement: table::SignedStatement,
	) -> Option<StatementProducer<
		<R::FetchCandidate as IntoFuture>::Future,
		<R::FetchExtrinsic as IntoFuture>::Future,
	>> {
		self.inner.lock().import_remote_statement(&*self.context, router, statement)
	}

	/// Import many statements at once.
	///
	/// Provide an iterator yielding remote, pre-checked statements.
	///
	/// The statement producer, if any, will produce only statements concerning the same candidate
	/// as the one just imported
	pub fn import_remote_statements<R, I, U>(&self, router: &R, iterable: I) -> U
		where
			R: TableRouter,
			I: IntoIterator<Item=table::SignedStatement>,
			U: ::std::iter::FromIterator<Option<StatementProducer<
				<R::FetchCandidate as IntoFuture>::Future,
				<R::FetchExtrinsic as IntoFuture>::Future,
			>>>,
	{
		let mut inner = self.inner.lock();

		iterable.into_iter().map(move |statement| {
			inner.import_remote_statement(&*self.context, router, statement)
		}).collect()
	}

	/// Sign and import a local statement.
	pub fn sign_and_import(&self, statement: table::Statement) -> SignedStatement {
		let proposed_digest = match statement {
			GenericStatement::Candidate(ref c) => Some(c.hash()),
			_ => None,
		};

		let signed_statement = self.context.sign_statement(statement);

		let mut inner = self.inner.lock();
		if proposed_digest.is_some() {
			inner.proposed_digest = proposed_digest;
		}

		inner.table.import_statement(&*self.context, signed_statement.clone());
		signed_statement
	}

	/// Execute a closure using a specific candidate.
	///
	/// Deadlocks if called recursively.
	pub fn with_candidate<F, U>(&self, digest: &Hash, f: F) -> U
		where F: FnOnce(Option<&CandidateReceipt>) -> U
	{
		let inner = self.inner.lock();
		f(inner.table.get_candidate(digest))
	}

	/// Execute a closure using the current proposed set.
	///
	/// Deadlocks if called recursively.
	pub fn with_proposal<F, U>(&self, f: F) -> U
		where F: FnOnce(Vec<&CandidateReceipt>) -> U
	{
		let inner = self.inner.lock();
		f(inner.table.proposed_candidates(&*self.context))
	}

	/// Get the number of parachains which have available candidates.
	pub fn includable_count(&self) -> usize {
		self.inner.lock().table.includable_count()
	}

	/// Get all witnessed misbehavior.
	pub fn get_misbehavior(&self) -> HashMap<SessionKey, table::Misbehavior> {
		self.inner.lock().table.get_misbehavior().clone()
	}

	/// Track includability  of a given set of candidate hashes.
	pub fn track_includability<I>(&self, iterable: I) -> Includable
		where I: IntoIterator<Item=Hash>
	{
		let mut inner = self.inner.lock();

		let (tx, rx) = includable::track(iterable.into_iter().map(|x| {
			let includable = inner.table.candidate_includable(&x, &*self.context);
			(x, includable)
		}));

		if !tx.is_complete() {
			inner.trackers.push(tx);
		}

		rx
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use substrate_keyring::Keyring;

	#[derive(Clone)]
	struct DummyRouter;
	impl TableRouter for DummyRouter {
		type Error = ();
		type FetchCandidate = ::futures::future::Empty<BlockData,()>;
		type FetchExtrinsic = ::futures::future::Empty<Extrinsic,()>;

		fn local_candidate(&self, _candidate: CandidateReceipt, _block_data: BlockData, _extrinsic: Extrinsic) {

		}
		fn fetch_block_data(&self, _candidate: &CandidateReceipt) -> Self::FetchCandidate {
			::futures::future::empty()
		}
		fn fetch_extrinsic_data(&self, _candidate: &CandidateReceipt) -> Self::FetchExtrinsic {
			::futures::future::empty()
		}
	}

	#[test]
	fn statement_triggers_fetch_and_evaluate() {
		let mut groups = HashMap::new();

		let para_id = ParaId::from(1);
		let local_id = Keyring::Alice.to_raw_public().into();
		let local_key = Arc::new(Keyring::Alice.pair());

		let validity_other = Keyring::Bob.to_raw_public().into();
		let validity_other_key = Keyring::Bob.pair();
		let parent_hash = Default::default();

		groups.insert(para_id, GroupInfo {
			validity_guarantors: [local_id, validity_other].iter().cloned().collect(),
			availability_guarantors: Default::default(),
			needed_validity: 2,
			needed_availability: 0,
		});

		let shared_table = SharedTable::new(groups, local_key.clone(), parent_hash);

		let candidate = CandidateReceipt {
			parachain_index: para_id,
			collator: [1; 32].into(),
			signature: Default::default(),
			head_data: ::polkadot_primitives::parachain::HeadData(vec![1, 2, 3, 4]),
			balance_uploads: Vec::new(),
			egress_queue_roots: Vec::new(),
			fees: 1_000_000,
			block_data_hash: [2; 32].into(),
		};

		let candidate_statement = GenericStatement::Candidate(candidate);

		let signature = ::sign_table_statement(&candidate_statement, &validity_other_key, &parent_hash);
		let signed_statement = ::table::generic::SignedStatement {
			statement: candidate_statement,
			signature: signature.into(),
			sender: validity_other,
		};

		let producer = shared_table.import_remote_statement(
			&DummyRouter,
			signed_statement,
		).expect("candidate and local validity group are same");

		assert!(producer.work.evaluate, "should evaluate validity");
		assert!(producer.work.fetch_extrinsic.is_none(), "should not fetch extrinsic");
	}

	#[test]
	fn statement_triggers_fetch_and_availability() {
		let mut groups = HashMap::new();

		let para_id = ParaId::from(1);
		let local_id = Keyring::Alice.to_raw_public().into();
		let local_key = Arc::new(Keyring::Alice.pair());

		let validity_other = Keyring::Bob.to_raw_public().into();
		let validity_other_key = Keyring::Bob.pair();
		let parent_hash = Default::default();

		groups.insert(para_id, GroupInfo {
			validity_guarantors: [validity_other].iter().cloned().collect(),
			availability_guarantors: [local_id].iter().cloned().collect(),
			needed_validity: 1,
			needed_availability: 1,
		});

		let shared_table = SharedTable::new(groups, local_key.clone(), parent_hash);

		let candidate = CandidateReceipt {
			parachain_index: para_id,
			collator: [1; 32].into(),
			signature: Default::default(),
			head_data: ::polkadot_primitives::parachain::HeadData(vec![1, 2, 3, 4]),
			balance_uploads: Vec::new(),
			egress_queue_roots: Vec::new(),
			fees: 1_000_000,
			block_data_hash: [2; 32].into(),
		};

		let candidate_statement = GenericStatement::Candidate(candidate);

		let signature = ::sign_table_statement(&candidate_statement, &validity_other_key, &parent_hash);
		let signed_statement = ::table::generic::SignedStatement {
			statement: candidate_statement,
			signature: signature.into(),
			sender: validity_other,
		};

		let producer = shared_table.import_remote_statement(
			&DummyRouter,
			signed_statement,
		).expect("should produce work");

		assert!(producer.work.fetch_extrinsic.is_some(), "should fetch extrinsic when guaranteeing availability");
		assert!(!producer.work.evaluate, "should not evaluate validity");
	}
}
