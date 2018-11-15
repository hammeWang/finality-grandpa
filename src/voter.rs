// Copyright 2018 Parity Technologies (UK) Ltd.
// This file is part of finality-grandpa.

// finality-grandpa is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// finality-grandpa is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with finality-grandpa. If not, see <http://www.gnu.org/licenses/>.

//! A voter in GRANDPA. This transitions between rounds and casts votes.
//!
//! Voters rely on some external context to function:
//!   - setting timers to cast votes
//!   - incoming vote streams
//!   - providing voter weights.

use futures::prelude::*;
use futures::task;
use futures::stream::futures_unordered::FuturesUnordered;
use futures::sync::mpsc::{self, UnboundedSender, UnboundedReceiver};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::sync::Arc;
use parking_lot::Mutex;

use round::{Round, State as RoundState};
use vote_graph::VoteGraph;
use ::{Chain, Commit, CompactCommit, Equivocation, Message, Prevote, Precommit, SignedMessage, SignedPrecommit, BlockNumberOps, threshold};

/// Necessary environment for a voter.
///
/// This encapsulates the database and networking layers of the chain.
pub trait Environment<H: Eq, N: BlockNumberOps>: Chain<H, N> {
	type Timer: Future<Item=(),Error=Self::Error>;
	type Id: Hash + Clone + Eq + ::std::fmt::Debug;
	type Signature: Eq + Clone;
	type In: Stream<Item=SignedMessage<H, N, Self::Signature, Self::Id>,Error=Self::Error>;
	type Out: Sink<SinkItem=Message<H, N>,SinkError=Self::Error>;
	type CommitIn: Stream<Item=(u64, CompactCommit<H, N, Self::Signature, Self::Id>), Error=Self::Error>;
	type CommitOut: Sink<SinkItem=(u64, Commit<H, N, Self::Signature, Self::Id>), SinkError=Self::Error>;
	type Error: From<::Error>;

	/// Produce data necessary to start a round of voting.
	///
	/// The input stream should provide messages which correspond to known blocks
	/// only.
	///
	/// The voting logic will push unsigned messages over-eagerly into the
	/// output stream. It is the job of this stream to determine if those messages
	/// should be sent (for example, if the process actually controls a permissioned key)
	/// and then to sign the message, multicast it to peers, and schedule it to be
	/// returned by the `In` stream.
	///
	/// This allows the voting logic to maintain the invariant that only incoming messages
	/// may alter the state, and the logic remains the same regardless of whether a node
	/// is a regular voter, the proposer, or simply an observer.
	///
	/// Furthermore, this means that actual logic of creating and verifying
	/// signatures is flexible and can be maintained outside this crate.
	fn round_data(&self, round: u64) -> RoundData<
		Self::Timer,
		Self::Id,
		Self::In,
		Self::Out
	>;

	/// Produce the input and output streams used for the commit protocol.
	///
	/// The input stream should provide commits which correspond to known blocks
	/// only (including all its precommits).
	///
	/// The input stream is also responsible for validating the signature data
	/// in commit messages.
	fn committer_data(&self) -> (Self::CommitIn, Self::CommitOut);

	/// Return a timer that will be used to delay the broadcast of a commit
	/// message. This delay should not be static to minimize the amount of
	/// commit messages that are sent (e.g. random value in [0, 1] seconds).
	fn round_commit_timer(&self) -> Self::Timer;

	/// Return the voters and respective weights for a given round.
	fn voters(&self, round: u64) -> &HashMap<Self::Id, u64>;

	/// Note that a round was completed. This is called when a round has been
	/// voted in. Should return an error when something fatal occurs.
	fn completed(&self, round: u64, state: RoundState<H, N>) -> Result<(), Self::Error>;

	/// Called when a block should be finalized.
	/// May be called more than once with the same block.
	// TODO: make this a future that resolves when it's e.g. written to disk?
	fn finalize_block(&self, hash: H, number: N) -> Result<(), Self::Error>;

	// Note that an equivocation in prevotes has occurred.s
	fn prevote_equivocation(&self, round: u64, equivocation: Equivocation<Self::Id, Prevote<H, N>, Self::Signature>);
	// Note that an equivocation in precommits has occurred.
	fn precommit_equivocation(&self, round: u64, equivocation: Equivocation<Self::Id, Precommit<H, N>, Self::Signature>);
}

/// Data necessary to participate in a round.
pub struct RoundData<Timer, Id, Input, Output> {
	/// Timer before prevotes can be cast. This should be Start + 2T
	/// where T is the gossip time estimate.
	pub prevote_timer: Timer,
	/// Timer before precommits can be cast. This should be Start + 4T
	pub precommit_timer: Timer,
	/// All voters in this round.
	pub voters: HashMap<Id, u64>,
	/// Incoming messages.
	pub incoming: Input,
	/// Outgoing messages.
	pub outgoing: Output,
}

enum State<T> {
	Start(T, T),
	Prevoted(T),
	Precommitted,
}

impl<T> std::fmt::Debug for State<T> {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
		match self {
			State::Start(..) => write!(f, "Start"),
			State::Prevoted(_) => write!(f, "Prevoted"),
			State::Precommitted => write!(f, "Precommitted"),
		}
	}
}

struct Buffered<S: Sink> {
	inner: S,
	buffer: VecDeque<S::SinkItem>,
}

impl<S: Sink> Buffered<S> {
	fn new(inner: S) -> Buffered<S> {
		Buffered {
			buffer: VecDeque::new(),
			inner
		}
	}

	// push an item into the buffered sink.
	// the sink _must_ be driven to completion with `poll` afterwards.
	fn push(&mut self, item: S::SinkItem) {
		self.buffer.push_back(item);
	}

	// returns ready when the sink and the buffer are completely flushed.
	fn poll(&mut self) -> Poll<(), S::SinkError> {
		let polled = self.schedule_all()?;

		match polled {
			Async::Ready(()) => self.inner.poll_complete(),
			Async::NotReady => {
				self.inner.poll_complete()?;
				Ok(Async::NotReady)
			}
		}
	}

	fn schedule_all(&mut self) -> Poll<(), S::SinkError> {
		while let Some(front) = self.buffer.pop_front() {
			match self.inner.start_send(front) {
				Ok(AsyncSink::Ready) => continue,
				Ok(AsyncSink::NotReady(front)) => {
					self.buffer.push_front(front);
					break;
				}
				Err(e) => return Err(e),
			}
		}

		if self.buffer.is_empty() {
			Ok(Async::Ready(()))
		} else {
			Ok(Async::NotReady)
		}
	}
}

/// Logic for a voter on a specific round.
pub struct VotingRound<H, N, E: Environment<H, N>> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	env: Arc<E>,
	votes: Round<E::Id, H, N, E::Signature>,
	incoming: E::In,
	outgoing: Buffered<E::Out>,
	state: Option<State<E::Timer>>, // state machine driving votes.
	bridged_round_state: Option<::bridge_state::PriorView<H, N>>, // updates to later round
	last_round_state: ::bridge_state::LatterView<H, N>, // updates from prior round
	primary_block: Option<(H, N)>, // a block posted by primary as a hint. TODO: implement
	finalized_sender: UnboundedSender<(H, N)>,
	//best_finalized: N,
}

impl<H, N, E: Environment<H, N>> VotingRound<H, N, E> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	// Poll the round. When the round is completable and messages have been flushed, it will return `Async::Ready` but
	// can continue to be polled.
	fn poll(&mut self) -> Poll<(), E::Error> {
		trace!(target: "afg", "Polling round {}, state = {:?}, step = {:?}", self.votes.number(), self.votes.state(), self.state);

		let pre_state = self.votes.state();

		self.process_incoming()?;
		let last_round_state = self.last_round_state.get().clone();
		self.prevote(&last_round_state)?;
		self.precommit(&last_round_state)?;

		try_ready!(self.outgoing.poll());
		self.process_incoming()?; // in case we got a new message signed locally.

		// broadcast finality notifications after attempting to cast votes
		let post_state = self.votes.state();
		self.notify(pre_state, post_state);

		if self.votes.completable() {
			Ok(Async::Ready(()))
		} else {
			Ok(Async::NotReady)
		}
	}

	fn process_incoming(&mut self) -> Result<(), E::Error> {
		while let Async::Ready(Some(incoming)) = self.incoming.poll()? {
			trace!(target: "afg", "Got incoming message");
			let SignedMessage { message, signature, id } = incoming;

			match message {
				Message::Prevote(prevote) => {
					if let Some(e) = self.votes.import_prevote(&*self.env, prevote, id, signature)? {
						self.env.prevote_equivocation(self.votes.number(), e);
					}
				}
				Message::Precommit(precommit) => {
					if let Some(e) = self.votes.import_precommit(&*self.env, precommit, id, signature)? {
						self.env.precommit_equivocation(self.votes.number(), e);
					}
				}
			};
		}

		Ok(())
	}

	fn prevote(&mut self, last_round_state: &RoundState<H, N>) -> Result<(), E::Error> {
		match self.state.take() {
			Some(State::Start(mut prevote_timer, precommit_timer)) => {
				let should_prevote = match prevote_timer.poll() {
					Err(e) => return Err(e),
					Ok(Async::Ready(())) => true,
					Ok(Async::NotReady) => self.votes.completable(),
				};

				if should_prevote {
					if let Some(prevote) = self.construct_prevote(last_round_state)? {
						debug!(target: "afg", "Casting prevote for round {}", self.votes.number());
						self.outgoing.push(Message::Prevote(prevote));
					}
					self.state = Some(State::Prevoted(precommit_timer));
				} else {
					self.state = Some(State::Start(prevote_timer, precommit_timer));
				}
			}
			x => { self.state = x; }
		}

		Ok(())
	}

	fn precommit(&mut self, last_round_state: &RoundState<H, N>) -> Result<(), E::Error> {
		match self.state.take() {
			Some(State::Prevoted(mut precommit_timer)) => {
				let last_round_estimate = last_round_state.estimate.clone()
					.expect("Rounds only started when prior round completable; qed");

				let should_precommit = {
					// we wait for the last round's estimate to be equal to or
					// the ancestor of the current round's p-Ghost before precommitting.
					self.votes.state().prevote_ghost.as_ref().map_or(false, |p_g| {
						p_g == &last_round_estimate ||
							self.env.is_equal_or_descendent_of(last_round_estimate.0, p_g.0.clone())
					})
				} && match precommit_timer.poll() {
					Err(e) => return Err(e),
					Ok(Async::Ready(())) => true,
					Ok(Async::NotReady) => self.votes.completable(),
				};

				if should_precommit {
					debug!(target: "afg", "Casting precommit for round {}", self.votes.number());
					let precommit = self.construct_precommit();
					self.outgoing.push(Message::Precommit(precommit));
					self.state = Some(State::Precommitted);
				} else {
					self.state = Some(State::Prevoted(precommit_timer));
				}
			}
			x => { self.state = x; }
		}

		Ok(())
	}

	// construct a prevote message based on local state.
	fn construct_prevote(&self, last_round_state: &RoundState<H, N>) -> Result<Option<Prevote<H, N>>, E::Error> {
		let last_round_estimate = last_round_state.estimate.clone()
			.expect("Rounds only started when prior round completable; qed");

		let find_descendent_of = match self.primary_block {
			None => {
				// vote for best chain containing prior round-estimate.
				last_round_estimate.0
			}
			Some(ref primary_block) => {
				// we will vote for the best chain containing `p_hash` iff
				// the last round's prevote-GHOST included that block and
				// that block is a strict descendent of the last round-estimate that we are
				// aware of.
				let last_prevote_g = last_round_state.prevote_ghost.clone()
					.expect("Rounds only started when prior round completable; qed");

				// if the blocks are equal, we don't check ancestry.
				if primary_block == &last_prevote_g {
					primary_block.0.clone()
				} else if primary_block.1 >= last_prevote_g.1 {
					last_round_estimate.0
				} else {
					// from this point onwards, the number of the primary-broadcasted
					// block is less than the last prevote-GHOST's number.
					// if the primary block is in the ancestry of p-G we vote for the
					// best chain containing it.
					let &(ref p_hash, p_num) = primary_block;
					match self.env.ancestry(last_round_estimate.0.clone(), last_prevote_g.0) {
						Ok(ancestry) => {
							let to_sub = p_num + N::one();

							let offset: usize = if last_prevote_g.1 < to_sub {
								0
							} else {
								(last_prevote_g.1 - to_sub).as_()
							};

							if ancestry.get(offset).map_or(false, |b| b == p_hash) {
								p_hash.clone()
							} else {
								last_round_estimate.0
							}
						}
						Err(::Error::NotDescendent) => last_round_estimate.0,
					}
				}
			}
		};

		let best_chain = self.env.best_chain_containing(find_descendent_of.clone());
		debug_assert!(best_chain.is_some(), "Previously known block {:?} has disappeared from chain", find_descendent_of);

		let t = match best_chain {
			Some(target) => target,
			None => {
				// If this block is considered unknown, something has gone wrong.
				// log and handle, but skip casting a vote.
				warn!(target: "afg", "Could not cast prevote: previously known block {:?} has disappeared", find_descendent_of);
				return Ok(None)
			}
		};

		Ok(Some(Prevote {
			target_hash: t.0,
			target_number: t.1,
		}))
	}

	// construct a precommit message based on local state.
	fn construct_precommit(&self) -> Precommit<H, N> {
		let t = match self.votes.state().prevote_ghost {
			Some(target) => target,
			None => self.votes.base(),
		};

		Precommit {
			target_hash: t.0,
			target_number: t.1,
		}
	}

	// notify when new blocks are finalized or when the round-estimate is updated
	fn notify(&self, last_state: RoundState<H, N>, new_state: RoundState<H, N>) {
		if last_state == new_state { return }

		if let Some(ref b) = self.bridged_round_state {
			b.update(new_state.clone());
		}

		if last_state.finalized != new_state.finalized && new_state.completable {
			// send notification only when the round is completable and we've cast votes.
			// this is a workaround that ensures when we re-instantiate the voter after
			// a shutdown, we never re-create the same round with a base that was finalized
			// in this round or after.
			match (&self.state, new_state.finalized) {
				(&Some(State::Precommitted), Some(ref f)) => {
					let _ = self.finalized_sender.unbounded_send(f.clone());
				}
				_ => {}
			}
		}
	}

	// call this when we build on top of a given round in order to get a handle
	// to updates to the latest round-state.
	fn bridge_state(&mut self) -> ::bridge_state::LatterView<H, N> {
		let (prior_view, latter_view) = ::bridge_state::bridge_state(self.votes.state());
		if self.bridged_round_state.is_some() {
			warn!(target: "afg", "Bridged state from round {} more than once.",
				self.votes.number());
		}

		self.bridged_round_state = Some(prior_view);
		latter_view
	}
}

// wraps a voting round with a new future that resolves when the round can
// be discarded from the working set.
//
// that point is when the round-estimate is finalized.
struct BackgroundRound<H, N, E: Environment<H, N>> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	inner: Arc<Mutex<VotingRound<H, N, E>>>,
	task: Option<task::Task>,
	finalized_number: N,
}

impl<H, N, E: Environment<H, N>> BackgroundRound<H, N, E> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	fn is_done(&self, voting_round: &VotingRound<H, N, E>) -> bool {
		// no need to listen on a round anymore once the estimate is finalized.
		voting_round.votes.state().estimate
			.map_or(false, |x| (x.1) <= self.finalized_number)
	}

	fn update_finalized(&mut self, new_finalized: N) {
		self.finalized_number = ::std::cmp::max(self.finalized_number, new_finalized);

		// wake up the future to be polled if done.
		if self.is_done(&self.inner.lock()) {
			if let Some(ref task) = self.task {
				task.notify();
			}
		}
	}
}

impl<H, N, E: Environment<H, N>> Future for BackgroundRound<H, N, E> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	type Item = u64; // round number
	type Error = E::Error;

	fn poll(&mut self) -> Poll<u64, E::Error> {
		self.task = Some(::futures::task::current());

		let mut voting_round = self.inner.lock();
		voting_round.poll()?;

		if self.is_done(&voting_round) {
			Ok(Async::Ready(voting_round.votes.number()))
		} else {
			Ok(Async::NotReady)
		}
	}
}

struct RoundCommitter<H, N, E: Environment<H, N>> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	voting_round: Arc<Mutex<VotingRound<H, N, E>>>,
	commit_timer: E::Timer,
	last_commit: Option<Commit<H, N, E::Signature, E::Id>>,
}

impl<H, N, E: Environment<H, N>> RoundCommitter<H, N, E> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	fn import_commit(
		&mut self,
		env: &E,
		commit: Commit<H, N, E::Signature, E::Id>,
	) -> Result<bool, E::Error> {
		let mut voting_round = self.voting_round.lock();

		// ignore commits for a block lower than we already finalized
		if commit.target_number < voting_round.votes.finalized().map(|(_, n)| *n).unwrap_or(N::zero()) {
			return Ok(true);
		}

		if validate_commit(
			&commit,
			voting_round.votes.voters(),
			voting_round.votes.threshold(),
			env,
		)?.is_none() {
			return Ok(false);
		}

		// the commit is valid, import all precommits into current round
		for SignedPrecommit { precommit, signature, id } in commit.precommits.clone() {
			if let Some(e) = voting_round.votes.import_precommit(env, precommit, id, signature)? {
				env.precommit_equivocation(voting_round.votes.number(), e);
			}
		}

		self.last_commit = Some(commit);

		Ok(true)
	}

	fn commit(&mut self, env: &E) -> Poll<Option<Commit<H, N, E::Signature, E::Id>>, E::Error> {
		try_ready!(self.commit_timer.poll());

		let voting_round = self.voting_round.lock();
		let commit = || -> Option<Commit<H, N, E::Signature, E::Id>> {
			let (target_hash, target_number) = voting_round.votes.finalized().cloned()?;

			let mut ids = HashSet::new();
			let precommits =
				voting_round.votes.precommits().into_iter().filter_map(|(id, precommit, signature)| {
					if env.is_equal_or_descendent_of(target_hash.clone(), precommit.target_hash.clone()) &&
						ids.insert(id.clone()) {
							// if an authority equivocated then only include one of its
							// votes that justify the commit
							Some(SignedPrecommit {
								precommit: precommit,
								signature: signature,
								id: id,
							})
						} else {
							None
						}
				}).collect();

			Some(Commit {
				target_hash,
				target_number,
				precommits,
			})
		};

		match (self.last_commit.take(), voting_round.votes.finalized()) {
			(None, Some(_)) => Ok(Async::Ready(commit())),
			(Some(Commit { target_number, .. }), Some((_, finalized_number)))
				if target_number < *finalized_number => Ok(Async::Ready(commit())),
			_ => Ok(Async::Ready(None))
		}
	}
}

/// Implements the commit protocol.
///
/// The commit protocol allows a node to broadcast a message that finalizes a
/// given block and includes a set of precommits as proof.
///
/// - When a round is completable and we precommitted we start a commit timer
/// and start accepting commit messages;
/// - When we receive a commit message if it targets a block higher than what
/// we've finalized we validate it and import its precommits if valid;
/// - When our commit timer triggers we check if we've received any commit
/// message for a block equal to what we've finalized, if we haven't then we
/// broadcast a commit.
///
/// Additionally, we also listen to commit messages from rounds that aren't
/// currently running, we validate the commit and dispatch a finalization
/// notification (if any) to the environment.
struct Committer<H, N, E: Environment<H, N>> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	env: Arc<E>,
	rounds: HashMap<u64, RoundCommitter<H, N, E>>,
	incoming: E::CommitIn,
	outgoing: Buffered<E::CommitOut>,
}

impl<H, N, E: Environment<H, N>> Committer<H, N, E> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	fn new(env: Arc<E>, incoming: E::CommitIn, outgoing: E::CommitOut) -> Committer<H, N, E> {
		Committer {
			env,
			rounds: HashMap::new(),
			outgoing: Buffered::new(outgoing),
			incoming,
		}
	}

	fn process_incoming(&mut self) -> Result<(), E::Error> {
		while let Async::Ready(Some(incoming)) = self.incoming.poll()? {
			let (round_number, commit) = incoming;

			trace!(target: "afg", "Got commit for round_number {:?}: target_number: {:?}, target_hash: {:?}",
				round_number,
				commit.target_number,
				commit.target_hash,
			);

			// if the commit is for a running round dispatch to round committer
			if let Some(round) = self.rounds.get_mut(&round_number) {
				if !round.import_commit(&*self.env, commit.into())? {
					trace!(target: "afg", "Ignoring invalid commit");
				};
			} else {
				// otherwise validate the commit and signal the finalized block
				// (if any) to the environment
				let voters = self.env.voters(round_number);
				let threshold = threshold(voters.values().sum());

				if let Some((finalized_hash, finalized_number)) = validate_commit(
					&commit.into(),
					voters,
					threshold,
					&*self.env,
				)? {
					// TODO: should we check if > last finalized to avoid
					// finalizing backwards?
					self.env.finalize_block(finalized_hash, finalized_number)?;
				}
			}
		}

		Ok(())
	}

	fn process_timers(&mut self) -> Result<(), E::Error> {
		let env = self.env.clone();
		let mut commits = Vec::new();

		self.rounds.retain(|round_number, committer| {
			// FIXME: shouldn't swallow commit errors
			match committer.commit(&env) {
				Ok(Async::NotReady) => true,
				Ok(Async::Ready(Some(commit))) => {
					commits.push((*round_number, commit));
					false
				},
				_ => false,
			}
		});

		for (round_number, commit) in commits {
			debug!(target: "afg", "Committing: round_number = {}, target_number = {:?}, target_hash = {:?}",
				round_number,
				commit.target_number,
				commit.target_hash,
			);
			self.outgoing.push((round_number, commit));
		}

		Ok(())
	}

	fn push(&mut self, round_number: u64, voting_round: Arc<Mutex<VotingRound<H, N, E>>>) {
		assert!(!self.rounds.contains_key(&round_number));

		self.rounds.insert(round_number, RoundCommitter {
			commit_timer: self.env.round_commit_timer(),
			last_commit: None,
			voting_round,
		});
	}

	fn poll(&mut self) -> Poll<(), E::Error> {
		self.process_incoming()?;
		self.process_timers()?;
		try_ready!(self.outgoing.poll());
		Ok(Async::NotReady)
	}
}

/// A future that maintains and multiplexes between different rounds,
/// and caches votes.
pub struct Voter<H, N, E: Environment<H, N>> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	env: Arc<E>,
	best_round: VotingRound<H, N, E>,
	past_rounds: FuturesUnordered<BackgroundRound<H, N, E>>,
	committer: Committer<H, N, E>,
	finalized_notifications: UnboundedReceiver<(H, N)>,
	last_finalized: (H, N),
}

impl<H, N, E: Environment<H, N>> Voter<H, N, E> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,

{
	/// Create new `Voter` tracker with given round number and base block.
	///
	/// Provide data about the last completed round. If there is no
	/// known last completed round, the genesis state (round number 0),
	/// should be provided.
	pub fn new(
		env: Arc<E>,
		last_round: u64,
		last_round_state: RoundState<H, N>,
		last_finalized: (H, N),
	) -> Self {
		let (finalized_sender, finalized_notifications) = mpsc::unbounded();

		let next_number = last_round + 1;
		let round_data = env.round_data(next_number);

		let round_params = ::round::RoundParams {
			round_number: next_number,
			voters: round_data.voters,
			base: last_finalized.clone(),
		};

		let (_, last_round_state) = ::bridge_state::bridge_state(last_round_state);
		let best_round = VotingRound {
			env: env.clone(),
			votes: Round::new(round_params),
			incoming: round_data.incoming,
			outgoing: Buffered::new(round_data.outgoing),
			state: Some(
				State::Start(round_data.prevote_timer, round_data.precommit_timer)
			),
			bridged_round_state: None,
			last_round_state,
			primary_block: None,
			finalized_sender,
		};

		let (committer_incoming, committer_outgoing) = env.committer_data();
		let committer = Committer::new(env.clone(), committer_incoming, committer_outgoing);

		// TODO: load last round (or more), re-process all votes from them,
		// and background until irrelevant

		Voter {
			env,
			best_round,
			past_rounds: FuturesUnordered::new(),
			committer,
			finalized_notifications,
			last_finalized,
		}
	}

	fn prune_background(&mut self) -> Result<(), E::Error> {
		// Do work on all rounds, pumping out any that are complete.
		while let Async::Ready(Some(_)) = self.past_rounds.poll()? { }

		while let Async::Ready(res) = self.finalized_notifications.poll()
			.expect("unbounded receivers do not have spurious errors; qed")
		{
			let (f_hash, f_num) = res.expect("one sender always kept alive in self.best_round; qed");

			// have the task check if it should be pruned.
			// if so, this future will be re-polled
			for bg in self.past_rounds.iter_mut() {
				bg.update_finalized(f_num);
			}

			if f_num > self.last_finalized.1 {
				// TODO: handle safety violations and check ancestry.
				self.last_finalized = (f_hash.clone(), f_num);
				self.env.finalize_block(f_hash, f_num)?;
			}
		}

		Ok(())
	}
}

impl<H, N, E: Environment<H, N>> Future for Voter<H, N, E> where
	H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
	N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	type Item = ();
	type Error = E::Error;

	fn poll(&mut self) -> Poll<(), E::Error> {
		self.prune_background()?;
		self.committer.poll()?;

		let should_start_next = match self.best_round.poll()? {
			Async::Ready(()) => match self.best_round.state {
				Some(State::Precommitted) => true, // start when we've cast all votes.
				_ => false,
			},
			Async::NotReady => false,
		};

		if !should_start_next { return Ok(Async::NotReady) }

		self.env.completed(self.best_round.votes.number(), self.best_round.votes.state())?;

		let old_number = self.best_round.votes.number();
		let next_number = old_number + 1;
		let next_round_data = self.env.round_data(next_number);

		let round_params = ::round::RoundParams {
			round_number: next_number,
			voters: next_round_data.voters,
			base: self.last_finalized.clone(),
		};

		let next_round = VotingRound {
			env: self.env.clone(),
			votes: Round::new(round_params),
			incoming: next_round_data.incoming,
			outgoing: Buffered::new(next_round_data.outgoing),
			state: Some(
				State::Start(next_round_data.prevote_timer, next_round_data.precommit_timer)
			),
			bridged_round_state: None,
			last_round_state: self.best_round.bridge_state(),
			primary_block: None,
			finalized_sender: self.best_round.finalized_sender.clone(),
		};

		let old_round = Arc::new(Mutex::new(::std::mem::replace(&mut self.best_round, next_round)));
		let background = BackgroundRound {
			inner: old_round.clone(),
			task: None,
			finalized_number: N::zero(), // TODO: do that right.
		};

		self.past_rounds.push(background);
		self.committer.push(old_number, old_round.clone());

		// round has been updated. so we need to re-poll.
		self.poll()
	}
}

fn validate_commit<H, N, E: Environment<H, N>>(
	commit: &Commit<H, N, E::Signature, E::Id>,
	voters: &HashMap<E::Id, u64>,
	threshold: u64,
	env: &E,
) -> Result<Option<(H, N)>, E::Error>
	where H: Hash + Clone + Eq + Ord + ::std::fmt::Debug,
		  N: Copy + BlockNumberOps + ::std::fmt::Debug,
{
	// check that all precommits are for blocks higher than the target
	// commit block, and that they're its descendents
	if !commit.precommits.iter().all(|signed| {
		signed.precommit.target_number >= commit.target_number &&
			env.is_equal_or_descendent_of(
				commit.target_hash.clone(),
				signed.precommit.target_hash.clone(),
			)
	}) {
		return Ok(None);
	}

	// check that the precommits don't include equivocations
	let mut ids = HashSet::new();
	if !commit.precommits.iter().all(|signed| ids.insert(signed.id.clone())) {
		return Ok(None);
	}

	// check all precommits are from authorities
	if !commit.precommits.iter().all(|signed| voters.contains_key(&signed.id)) {
		return Ok(None);
	}

	// add all precommits to an empty vote graph with the commit target as the base
	let mut vote_graph = VoteGraph::new(commit.target_hash.clone(), commit.target_number.clone());
	for SignedPrecommit { precommit, id, .. } in commit.precommits.iter() {
		let weight = voters.get(id).expect("previously verified that all ids are voters; qed");
		vote_graph.insert(precommit.target_hash.clone(), precommit.target_number.clone(), *weight, env)?;
	}

	// find ghost using commit target as current best
	let ghost = vote_graph.find_ghost(
		Some((commit.target_hash.clone(), commit.target_number.clone())),
		|w| *w >= threshold,
	);

	// if a ghost is found then it must be equal or higher than the commit
	// target, otherwise the commit is invalid
	Ok(ghost)
}

#[cfg(test)]
mod tests {
	use super::*;
	use tokio::prelude::FutureExt;
	use tokio::runtime::current_thread;
	use testing::{self, GENESIS_HASH, Environment, Id};
	use std::collections::HashMap;
	use std::time::Duration;

	#[test]
	fn talking_to_myself() {
		let local_id = Id(5);
		let voters = {
			let mut map = HashMap::new();
			map.insert(local_id, 100);
			map
		};

		let (network, routing_task) = testing::make_network();
		let (signal, exit) = ::exit_future::signal();

		let env = Arc::new(Environment::new(voters, network, local_id));
		current_thread::block_on_all(::futures::future::lazy(move || {
			// initialize chain
			let last_finalized = env.with_chain(|chain| {
				chain.push_blocks(GENESIS_HASH, &["A", "B", "C", "D", "E"]);
				chain.last_finalized()
			});

			let last_round_state = RoundState::genesis((GENESIS_HASH, 1));

			// run voter in background. scheduling it to shut down at the end.
			let finalized = env.finalized_stream();
			let voter = Voter::new(env.clone(), 0, last_round_state, last_finalized);
			::tokio::spawn(exit.clone()
				.until(voter.map_err(|_| panic!("Error voting"))).map(|_| ()));

			::tokio::spawn(exit.until(routing_task).map(|_| ()));

			// wait for the best block to finalize.
			finalized
				.take_while(|&(_, n)| Ok(n < 6))
				.for_each(|_| Ok(()))
				.map(|_| signal.fire())
		})).unwrap();
	}

	#[test]
	fn finalizing_at_fault_threshold() {
		// 10 voters
		let voters = {
			let mut map = HashMap::new();
			for i in 0..10 {
				map.insert(Id(i), 1);
			}
			map
		};

		let (network, routing_task) = testing::make_network();
		let (signal, exit) = ::exit_future::signal();

		current_thread::block_on_all(::futures::future::lazy(move || {
			::tokio::spawn(exit.clone().until(routing_task).map(|_| ()));

			// 3 voters offline.
			let finalized_streams = (0..7).map(move |i| {
				let local_id = Id(i);
				// initialize chain
				let env = Arc::new(Environment::new(voters.clone(), network.clone(), local_id));
				let last_finalized = env.with_chain(|chain| {
					chain.push_blocks(GENESIS_HASH, &["A", "B", "C", "D", "E"]);
					chain.last_finalized()
				});

				let last_round_state = RoundState::genesis((GENESIS_HASH, 1));

				// run voter in background. scheduling it to shut down at the end.
				let finalized = env.finalized_stream();
				let voter = Voter::new(env.clone(), 0, last_round_state, last_finalized);
				::tokio::spawn(exit.clone()
					.until(voter.map_err(|_| panic!("Error voting"))).map(|_| ()));

				// wait for the best block to be finalized by all honest voters
				finalized
					.take_while(|&(_, n)| Ok(n < 6))
					.for_each(|_| Ok(()))
			});

			::futures::future::join_all(finalized_streams).map(|_| signal.fire())
		})).unwrap();
	}

	#[test]
	fn broadcast_commit() {
		let local_id = Id(5);
		let voters = {
			let mut map = HashMap::new();
			map.insert(local_id, 100);
			map
		};

		let (network, routing_task) = testing::make_network();
		let (commits, _) = network.make_commits_comms();

		let (signal, exit) = ::exit_future::signal();

		let env = Arc::new(Environment::new(voters, network, local_id));
		current_thread::block_on_all(::futures::future::lazy(move || {
			// initialize chain
			let last_finalized = env.with_chain(|chain| {
				chain.push_blocks(GENESIS_HASH, &["A", "B", "C", "D", "E"]);
				chain.last_finalized()
			});

			let last_round_state = RoundState::genesis((GENESIS_HASH, 1));

			// run voter in background. scheduling it to shut down at the end.
			let voter = Voter::new(env.clone(), 0, last_round_state, last_finalized);
			::tokio::spawn(exit.clone()
				.until(voter.map_err(|_| panic!("Error voting"))).map(|_| ()));

			::tokio::spawn(exit.until(routing_task).map(|_| ()));

			// wait for the node to broadcast a commit message
			commits.take(1).for_each(|_| Ok(())).map(|_| signal.fire())
		})).unwrap();
	}

	#[test]
	fn broadcast_commit_only_if_newer() {
		let local_id = Id(5);
		let test_id = Id(42);
		let voters = {
			let mut map = HashMap::new();
			map.insert(local_id, 100);
			map.insert(test_id, 201);
			map
		};

		let (network, routing_task) = testing::make_network();
		let (commits_stream, commits_sink) = network.make_commits_comms();
		let (round_stream, round_sink) = network.make_round_comms(1, test_id);

		let prevote = Message::Prevote(Prevote {
			target_hash: "E",
			target_number: 6,
		});

		let precommit = Message::Precommit(Precommit {
			target_hash: "E",
			target_number: 6,
		});

		let commit = (1, Commit {
			target_hash: "E",
			target_number: 6,
			precommits: vec![SignedPrecommit {
				precommit: Precommit { target_hash: "E", target_number: 6 },
				signature: testing::Signature(test_id.0),
				id: test_id
			}],
		});

		let (signal, exit) = ::exit_future::signal();

		let env = Arc::new(Environment::new(voters, network, local_id));
		current_thread::block_on_all(::futures::future::lazy(move || {
			// initialize chain
			let last_finalized = env.with_chain(|chain| {
				chain.push_blocks(GENESIS_HASH, &["A", "B", "C", "D", "E"]);
				chain.last_finalized()
			});

			let last_round_state = RoundState::genesis((GENESIS_HASH, 1));

			// run voter in background. scheduling it to shut down at the end.
			let voter = Voter::new(env.clone(), 0, last_round_state, last_finalized);
			::tokio::spawn(exit.clone()
				.until(voter.map_err(|e| panic!("Error voting: {:?}", e))).map(|_| ()));

			::tokio::spawn(exit.clone().until(routing_task).map(|_| ()));

			::tokio::spawn(exit.until(::futures::future::lazy(|| {
				round_stream.into_future().map_err(|(e, _)| e)
					.and_then(|(value, stream)| { // wait for a prevote
						assert!(match value {
							Some(SignedMessage { message: Message::Prevote(_), id: Id(5), .. }) => true,
							_ => false,
						});
						let votes = vec![prevote, precommit].into_iter().map(Result::Ok);
						round_sink.send_all(futures::stream::iter_result(votes)).map(|_| stream) // send our prevote
					})
					.and_then(|stream| {
						stream.take_while(|value| match value { // wait for a precommit
							SignedMessage { message: Message::Precommit(_), id: Id(5), .. } => Ok(false),
							_ => Ok(true),
						}).for_each(|_| Ok(()))
					})
					.and_then(|_| {
						commits_sink.send(commit) // send our commit
					})
					.map_err(|_| ())
			})).map(|_| ()));

			// wait for the first commit (ours)
			commits_stream.into_future().map_err(|_| ())
				.and_then(|(_, stream)| {
					stream.take(1).for_each(|_| Ok(())) // the second commit should never arrive
						.timeout(Duration::from_millis(500)).map_err(|_| ())
				})
				.then(|res| {
					assert!(res.is_err()); // so the previous future times out
					signal.fire();
					futures::future::ok::<(), ()>(())
				})
		})).unwrap();
	}

	#[test]
	fn import_commit_for_any_round() {
		let local_id = Id(5);
		let test_id = Id(42);
		let voters = {
			let mut map = HashMap::new();
			map.insert(local_id, 100);
			map.insert(test_id, 201);
			map
		};

		let (network, routing_task) = testing::make_network();
		let (_, commits_sink) = network.make_commits_comms();

		let (signal, exit) = ::exit_future::signal();

		// this is a commit for a previous round
		let commit = (0, Commit {
			target_hash: "E",
			target_number: 6,
			precommits: vec![SignedPrecommit {
				precommit: Precommit { target_hash: "E", target_number: 6 },
				signature: testing::Signature(test_id.0),
				id: test_id
			}],
		});

		let env = Arc::new(Environment::new(voters, network, local_id));
		current_thread::block_on_all(::futures::future::lazy(move || {
			// initialize chain
			let last_finalized = env.with_chain(|chain| {
				chain.push_blocks(GENESIS_HASH, &["A", "B", "C", "D", "E"]);
				chain.last_finalized()
			});

			let last_round_state = RoundState::genesis((GENESIS_HASH, 1));

			// run voter in background. scheduling it to shut down at the end.
			let voter = Voter::new(env.clone(), 1, last_round_state, last_finalized);
			::tokio::spawn(exit.clone()
				.until(voter.map_err(|_| panic!("Error voting"))).map(|_| ()));

			::tokio::spawn(exit.until(routing_task).map(|_| ()));

			::tokio::spawn(commits_sink.send(commit).map_err(|_| ()).map(|_| ()));

			// wait for the commit message to be processed which finalized block 6
			env.finalized_stream()
				.take_while(|&(_, n)| Ok(n < 6))
				.for_each(|_| Ok(()))
				.map(|_| signal.fire())
		})).unwrap();
	}
}
