// Copyright 2018 Parity Technologies (UK) Ltd.
// This file is part of finality-afg.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with finality-afg. If not, see <http://www.gnu.org/licenses/>.

//! Maintains the vote-graph of the blockchain.
//!
//! See docs on `VoteGraph` for more information.

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::Hash;
use std::ops::AddAssign;

use super::{Chain, Error};

#[derive(Debug)]
struct Entry<H, V> {
	number: usize,
	// ancestor hashes in reverse order, e.g. ancestors[0] is the parent
	// and the last entry is the hash of the parent vote-node.
	ancestors: Vec<H>,
	descendents: Vec<H>, // descendent vote-nodes
	cumulative_vote: V,
}

impl<H: Hash + PartialEq + Clone, V> Entry<H, V> {
	// whether the given hash, number pair is a direct ancestor of this node.
	// `None` signifies that the graph must be traversed further back.
	fn in_direct_ancestry(&self, hash: &H, number: usize) -> Option<bool> {
		self.ancestor_block(number).map(|h| h == hash)
	}

	// Get ancestor block by number. Returns `None` if there is no block
	// by that number in the direct ancestry.
	fn ancestor_block(&self, number: usize) -> Option<&H> {
		if number >= self.number { return None }
		let offset = self.number - number - 1;

		self.ancestors.get(offset)
	}

	// get ancestor vote-node.
	fn ancestor_node(&self) -> Option<H> {
		self.ancestors.last().map(|x| x.clone())
	}
}

// a subchain of blocks by hash.
struct Subchain<H> {
	hashes: Vec<H>, // forward order.
	best_number: usize,
}

impl<H: Clone> Subchain<H> {
	fn blocks_reverse<'a>(&'a self) -> impl Iterator<Item = (H, usize)> + 'a {
		let best = self.best_number;
		self.hashes.iter().rev().cloned().enumerate().map(move |(i, x)| (x, best - i))
	}

	fn block_at(&self, number: usize) -> Option<&H> {
		let rev_off = self.best_number.checked_sub(number)?;
		self.hashes.len().checked_sub(rev_off + 1).map(|i| &self.hashes[i])
	}

	fn best(&self) -> Option<(H, usize)> {
		self.hashes.last().map(|x| (x.clone(), self.best_number))
	}
}

/// Maintains a DAG of blocks in the chain which have votes attached to them,
/// and vote data which is accumulated along edges.
pub struct VoteGraph<H: Hash + Eq, V> {
	entries: HashMap<H, Entry<H, V>>,
	heads: HashSet<H>,
	base: H,
}

impl<H, V> VoteGraph<H, V> where
	H: Hash + Eq + Clone + Ord + Debug,
	V: AddAssign + Default + Clone + Debug,
{
	/// Create a new `VoteGraph` with base node as given.
	pub fn new(base_hash: H, base_number: usize) -> Self {
		let mut entries = HashMap::new();
		entries.insert(base_hash.clone(), Entry {
			number: base_number,
			ancestors: Vec::new(),
			descendents: Vec::new(),
			cumulative_vote: V::default(),
		});

		let mut heads = HashSet::new();
		heads.insert(base_hash.clone());

		VoteGraph {
			entries,
			heads,
			base: base_hash,
		}
	}

	/// Insert a vote with given value into the graph at given hash and number.
	pub fn insert<C: Chain<H>>(&mut self, hash: H, number: usize, vote: V, chain: &C) -> Result<(), Error> {
		match self.find_containing_nodes(hash.clone(), number) {
			Some(containing) => if containing.is_empty() {
				self.append(hash.clone(), number, chain)?;
			} else {
				self.introduce_branch(containing, hash.clone(), number);
			},
			None => {}, // this entry already exists
		}

		// update cumulative vote data.
		// NOTE: below this point, there always exists a node with the given hash and number.
		let mut inspecting_hash = hash;
		loop {
			let active_entry = self.entries.get_mut(&inspecting_hash)
				.expect("vote-node and its ancestry always exist after initial phase; qed");

			active_entry.cumulative_vote += vote.clone();

			match active_entry.ancestor_node() {
				Some(parent) => { inspecting_hash = parent },
				None => break,
			}
		}

		Ok(())
	}

	/// Find the highest block which is either an ancestor of or equal to the given, which fulfills a
	/// condition.
	pub fn find_ancestor<'a, F>(&'a self, hash: H, number: usize, condition: F) -> Option<(H, usize)>
		where F: Fn(&V) -> bool
	{
		let entries = &self.entries;
		let get_node = |hash: &_| -> &'a _ {
			entries.get(hash)
				.expect("node either base or referenced by other in graph; qed")
		};

		// we store two nodes with an edge between them that is the canonical
		// chain.
		// the `node_key` always points to the ancestor node, and the `canonical_node`
		// points to the higher node.
		let (mut node_key, mut canonical_node) = match self.find_containing_nodes(hash.clone(), number) {
			None =>	{
				let node = get_node(&hash);
				if condition(&node.cumulative_vote) {
					return Some((hash, number))
				}

				(node.ancestor_node()?, node)
			}
			Some(ref x) if !x.is_empty() => {
				let node = get_node(&x[0]);
				let key = node.ancestor_node()
					.expect("node containing block in ancestry has ancestor node; qed");

				(key, node)
			}
			Some(_) => return None,
		};

		// search backwards until we find the first vote-node that
		// meets the condition.
		let mut active_node = get_node(&node_key);
		while !condition(&active_node.cumulative_vote) {
			node_key = match active_node.ancestor_node() {
				Some(n) => n,
				None => return None,
			};

			canonical_node = active_node;
			active_node = get_node(&node_key);
		}

		// find the GHOST merge-point after the active_node.
		// constrain it to be within the canonical chain.
		let good_subchain = self.ghost_find_merge_point(node_key, active_node, None, condition);

		// TODO: binding is required for some reason.
		let x = good_subchain.blocks_reverse().find(|&(ref good_hash, good_number)|
			canonical_node.in_direct_ancestry(good_hash, good_number).unwrap_or(false)
		);

		x
	}

	/// Find the best GHOST descendent of the given block.
	/// Pass a closure used to evaluate the cumulative vote value.
	///
	/// The GHOST (hash, number) returned will be the block with highest number for which the
	/// cumulative votes of descendents and itself causes the closure to evaluate to true.
	///
	/// This assumes that the evaluation closure is one which returns true for at most a single
	/// descendent of a block, in that only one fork of a block can be "heavy"
	/// enough to trigger the threshold.
	pub fn find_ghost<'a, F>(&'a self, current_best: Option<(H, usize)>, condition: F) -> Option<(H, usize)>
		where F: Fn(&V) -> bool
	{
		let entries = &self.entries;
		let get_node = |hash: &_| -> &'a _ {
			entries.get(hash)
				.expect("node either base or referenced by other in graph; qed")
		};

		let (mut node_key, mut force_constrain) = current_best
			.clone()
			.and_then(|(hash, number)| match self.find_containing_nodes(hash.clone(), number) {
				None => Some((hash, false)),
				Some(ref x) if !x.is_empty() => {
					let ancestor = get_node(&x[0]).ancestor_node()
						.expect("node containing non-node in history always has ancestor; qed");

					Some((ancestor, true))
				}
				Some(_) => None,
			})
			.unwrap_or_else(|| (self.base.clone(), false));

		let mut active_node = get_node(&node_key);

		if !condition(&active_node.cumulative_vote) { return None }

		// breadth-first search starting from this node.
		loop {
			let next_descendent = active_node.descendents
				.iter()
				.map(|d| (d.clone(), get_node(d)))
				.filter(|&(_, ref node)| {
					// take only descendents with our block in the ancestry.
					if let (true, Some(&(ref h, n))) = (force_constrain, current_best.as_ref()) {
						node.in_direct_ancestry(h, n).unwrap_or(false)
					} else {
						true
					}
				})
				.filter(|&(_, ref node)| condition(&node.cumulative_vote))
				.next();

			match next_descendent {
				Some((key, node)) => {
					// once we've made at least one hop, we don't need to constrain
					// ancestry anymore.
					force_constrain = false;
					node_key = key;
					active_node = node;
				}
				None => break,
			}
		}

		// active_node and node_key now correspond to the vote-node with enough cumulative votes.
		// its descendents comprise frontier of vote-nodes which individually don't have enough votes
		// to pass the threshold but some subset of them join either at `active_node`'s block or at some
		// descendent block of it, giving that block sufficient votes.
		self.ghost_find_merge_point(
			node_key,
			active_node,
			if force_constrain { current_best } else { None },
			condition,
		).best()
	}

	// given a key, node pair (which must correspond), assuming this node fulfills the condition,
	// this function will find the highest point at which its descendents merge, which may be the
	// node itself.
	fn ghost_find_merge_point<'a, F>(
		&'a self,
		node_key: H,
		active_node: &'a Entry<H, V>,
		force_constrain: Option<(H, usize)>,
		condition: F,
	) -> Subchain<H>
		where F: Fn(&V) -> bool
	{
		let mut descendent_nodes: Vec<_> = active_node.descendents.iter()
			.map(|h| self.entries.get(h).expect("descendents always present in node storage; qed"))
			.filter(|n| if let Some((ref h, num)) = force_constrain {
				n.in_direct_ancestry(h, num).unwrap_or(false)
			} else {
				true
			})
			.collect();

		let base_number = active_node.number;
		let mut best_number = active_node.number;
		let mut descendent_blocks = Vec::with_capacity(descendent_nodes.len());
		let mut hashes = vec![node_key];

		// TODO: for long ranges of blocks this could get inefficient
		for offset in 1usize.. {
			let mut new_best = None;
			for d_node in descendent_nodes.iter() {
				if let Some(d_block) = d_node.ancestor_block(base_number + offset) {
					match descendent_blocks.binary_search_by_key(&d_block, |&(ref x, _)| x) {
						Ok(idx) => {
							descendent_blocks[idx].1 += d_node.cumulative_vote.clone();
							if condition(&descendent_blocks[idx].1) {
								new_best = Some(d_block.clone());
								break
							}
						}
						Err(idx) => descendent_blocks.insert(idx, (
							d_block.clone(),
							d_node.cumulative_vote.clone()
						)),
					}
				}
			}

			match new_best {
				Some(new_best) => {
					best_number += 1;

					descendent_blocks.clear();
					descendent_nodes.retain(
						|n| n.in_direct_ancestry(&new_best, best_number).unwrap_or(false)
					);

					hashes.push(new_best);
				}
				None => break,
			}
		}

		Subchain {
			hashes,
			best_number,
		}
	}

	// attempts to find the containing node keys for the given hash and number.
	//
	// returns `None` if there is a node by that key already, and a vector
	// (potentially empty) of nodes with the given block in its ancestor-edge
	// otherwise.
	fn find_containing_nodes(&self, hash: H, number: usize) -> Option<Vec<H>> {
		if self.entries.contains_key(&hash) {
			return None
		}

		let mut containing_keys = Vec::new();
		let mut visited = HashSet::new();

		// iterate vote-heads and their ancestry backwards until we find the one with
		// this target hash in that chain.
		for mut head in self.heads.iter().cloned() {
			let mut active_entry;

			loop {
				active_entry = match self.entries.get(&head) {
					Some(e) => e,
					None => break,
				};

				// if node has been checked already, break
				if !visited.insert(head.clone()) { break }

				match active_entry.in_direct_ancestry(&hash, number) {
					Some(true) => {
						// set containing node and continue search.
						containing_keys.push(head.clone());
					}
					Some(false) => {}, // nothing in this branch. continue search.
					None => if let Some(prev) = active_entry.ancestor_node() {
						head = prev;
						continue // iterate backwards
					},
				}

				break
			}
		}

		Some(containing_keys)
	}

	// introduce a branch to given vote-nodes.
	//
	// `descendents` is a list of nodes with ancestor-edges containing the given ancestor.
	//
	// This function panics if any member of `descendents` is not a vote-node
	// or does not have ancestor with given hash and number OR if `ancestor_hash`
	// is already a known entry.
	fn introduce_branch(&mut self, descendents: Vec<H>, ancestor_hash: H, ancestor_number: usize) {
		let produced_entry = descendents.into_iter().fold(None, |mut maybe_entry, descendent| {
			let entry = self.entries.get_mut(&descendent)
				.expect("this function only invoked with keys of vote-nodes; qed");

			debug_assert!(entry.in_direct_ancestry(&ancestor_hash, ancestor_number).unwrap());

			// example: splitting number 10 at ancestor 4
			// before: [9 8 7 6 5 4 3 2 1]
			// after: [9 8 7 6 5 4], [3 2 1]
			// we ensure the `entry.ancestors` is drained regardless of whether
			// the `new_entry` has already been constructed.
			{
				let offset = entry.number.checked_sub(ancestor_number)
					.expect("this function only invoked with direct ancestors; qed");
				let prev_ancestor  = entry.ancestor_node();
				let new_ancestors = entry.ancestors.drain(offset..);

				let &mut (ref mut new_entry, _) = maybe_entry.get_or_insert_with(move || {
					let new_entry = Entry {
						number: ancestor_number,
						ancestors: new_ancestors.collect(),
						descendents: vec![],
						cumulative_vote: V::default(),
					};

					(new_entry, prev_ancestor)
				});

				new_entry.descendents.push(descendent);
				new_entry.cumulative_vote += entry.cumulative_vote.clone();
			}

			maybe_entry
		});

		if let Some((new_entry, prev_ancestor)) = produced_entry {
			if let Some(prev_ancestor) = prev_ancestor {
				let mut prev_ancestor_node = self.entries.get_mut(&prev_ancestor)
					.expect("Prior ancestor is referenced from a node; qed");

				prev_ancestor_node.descendents.retain(|h| !new_entry.descendents.contains(&h));
				prev_ancestor_node.descendents.push(ancestor_hash.clone());
			}

			assert!(
				self.entries.insert(ancestor_hash, new_entry).is_none(),
				"thus function is only invoked when there is no entry for the ancestor already; qed",
			)
		}
	}

	// append a vote-node onto the chain-tree. This should only be called if
	// no node in the tree keeps the target anyway.
	fn append<C: Chain<H>>(&mut self, hash: H, number: usize, chain: &C) -> Result<(), Error> {
		let mut ancestry = chain.ancestry(self.base.clone(), hash.clone())?;

		let mut ancestor_index = None;
		for (i, ancestor) in ancestry.iter().enumerate() {
			if let Some(entry) = self.entries.get_mut(ancestor) {
				entry.descendents.push(hash.clone());
				ancestor_index = Some(i);
				break;
			}
		}

		let ancestor_index = ancestor_index.expect("base is kept; \
			chain returns ancestry only if the block is a descendent of base; qed");

		let ancestor_hash = ancestry[ancestor_index].clone();
		ancestry.truncate(ancestor_index + 1);

		self.entries.insert(hash.clone(), Entry {
			number,
			ancestors: ancestry,
			descendents: Vec::new(),
			cumulative_vote: V::default(),
		});

		self.heads.remove(&ancestor_hash);
		self.heads.insert(hash);

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use testing::{GENESIS_HASH, DummyChain};

	#[test]
	fn graph_fork_not_at_node() {
		let mut chain = DummyChain::new();
		let mut tracker = VoteGraph::new(GENESIS_HASH, 1);

		chain.push_blocks(GENESIS_HASH, &["A", "B", "C"]);
		chain.push_blocks("C", &["D1", "E1", "F1"]);
		chain.push_blocks("C", &["D2", "E2", "F2"]);

		tracker.insert("A", 2, 100usize, &chain).unwrap();
		tracker.insert("E1", 6, 100, &chain).unwrap();
		tracker.insert("F2", 7, 100, &chain).unwrap();

		assert!(tracker.heads.contains("E1"));
		assert!(tracker.heads.contains("F2"));
		assert!(!tracker.heads.contains("A"));

		let a_entry = tracker.entries.get("A").unwrap();
		assert_eq!(a_entry.descendents, vec!["E1", "F2"]);
		assert_eq!(a_entry.cumulative_vote, 300);


		let e_entry = tracker.entries.get("E1").unwrap();
		assert_eq!(e_entry.ancestor_node().unwrap(), "A");
		assert_eq!(e_entry.cumulative_vote, 100);

		let f_entry = tracker.entries.get("F2").unwrap();
		assert_eq!(f_entry.ancestor_node().unwrap(), "A");
		assert_eq!(f_entry.cumulative_vote, 100);
	}

	#[test]
	fn graph_fork_at_node() {
		let mut chain = DummyChain::new();
		let mut tracker1 = VoteGraph::new(GENESIS_HASH, 1);
		let mut tracker2 = VoteGraph::new(GENESIS_HASH, 1);

		chain.push_blocks(GENESIS_HASH, &["A", "B", "C"]);
		chain.push_blocks("C", &["D1", "E1", "F1"]);
		chain.push_blocks("C", &["D2", "E2", "F2"]);

		tracker1.insert("C", 4, 100usize, &chain).unwrap();
		tracker1.insert("E1", 6, 100, &chain).unwrap();
		tracker1.insert("F2", 7, 100, &chain).unwrap();

		tracker2.insert("E1", 6, 100usize, &chain).unwrap();
		tracker2.insert("F2", 7, 100, &chain).unwrap();
		tracker2.insert("C", 4, 100, &chain).unwrap();

		for tracker in &[&tracker2] {
			assert!(tracker.heads.contains("E1"));
			assert!(tracker.heads.contains("F2"));
			assert!(!tracker.heads.contains("C"));

			let c_entry = tracker.entries.get("C").unwrap();
			assert!(c_entry.descendents.contains(&"E1"));
			assert!(c_entry.descendents.contains(&"F2"));
			assert_eq!(c_entry.ancestor_node().unwrap(), GENESIS_HASH);
			assert_eq!(c_entry.cumulative_vote, 300);

			let e_entry = tracker.entries.get("E1").unwrap();
			assert_eq!(e_entry.ancestor_node().unwrap(), "C");
			assert_eq!(e_entry.cumulative_vote, 100);

			let f_entry = tracker.entries.get("F2").unwrap();
			assert_eq!(f_entry.ancestor_node().unwrap(), "C");
			assert_eq!(f_entry.cumulative_vote, 100);
		}
	}

	#[test]
	fn ghost_merge_at_node() {
		let mut chain = DummyChain::new();
		let mut tracker = VoteGraph::new(GENESIS_HASH, 1);

		chain.push_blocks(GENESIS_HASH, &["A", "B", "C"]);
		chain.push_blocks("C", &["D1", "E1", "F1"]);
		chain.push_blocks("C", &["D2", "E2", "F2"]);

		tracker.insert("B", 3, 0usize, &chain).unwrap();
		tracker.insert("C", 4, 100, &chain).unwrap();
		tracker.insert("E1", 6, 100, &chain).unwrap();
		tracker.insert("F2", 7, 100, &chain).unwrap();

		assert_eq!(tracker.find_ghost(None, |&x| x >= 250), Some(("C", 4)));
		assert_eq!(tracker.find_ghost(Some(("C", 4)), |&x| x >= 250), Some(("C", 4)));
		assert_eq!(tracker.find_ghost(Some(("B", 3)), |&x| x >= 250), Some(("C", 4)));
	}

	#[test]
	fn ghost_merge_not_at_node_one_side_weighted() {
		let mut chain = DummyChain::new();
		let mut tracker = VoteGraph::new(GENESIS_HASH, 1);

		chain.push_blocks(GENESIS_HASH, &["A", "B", "C", "D", "E", "F"]);
		chain.push_blocks("F", &["G1", "H1", "I1"]);
		chain.push_blocks("F", &["G2", "H2", "I2"]);

		tracker.insert("B", 3, 0usize, &chain).unwrap();
		tracker.insert("G1", 8, 100, &chain).unwrap();
		tracker.insert("H2", 9, 150, &chain).unwrap();

		assert_eq!(tracker.find_ghost(None, |&x| x >= 250), Some(("F", 7)));
		assert_eq!(tracker.find_ghost(Some(("F", 7)), |&x| x >= 250), Some(("F", 7)));
		assert_eq!(tracker.find_ghost(Some(("C", 4)), |&x| x >= 250), Some(("F", 7)));
		assert_eq!(tracker.find_ghost(Some(("B", 3)), |&x| x >= 250), Some(("F", 7)));
	}

	#[test]
	fn ghost_introduce_branch() {
		let mut chain = DummyChain::new();
		let mut tracker = VoteGraph::new(GENESIS_HASH, 1);

		chain.push_blocks(GENESIS_HASH, &["A", "B", "C", "D", "E", "F"]);
		chain.push_blocks("E", &["EA", "EB", "EC", "ED"]);
		chain.push_blocks("F", &["FA", "FB", "FC"]);

		tracker.insert("FC", 10, 5usize, &chain).unwrap();
		tracker.insert("ED", 10, 7, &chain).unwrap();

		assert_eq!(tracker.find_ghost(None, |&x| x >= 10), Some(("E", 6)));

		assert_eq!(tracker.entries.get(GENESIS_HASH).unwrap().descendents, vec!["FC", "ED"]);

		// introduce a branch in the middle.
		tracker.insert("E", 6, 3, &chain).unwrap();

		assert_eq!(tracker.entries.get(GENESIS_HASH).unwrap().descendents, vec!["E"]);
		let descendents = &tracker.entries.get("E").unwrap().descendents;
		assert_eq!(descendents.len(), 2);
		assert!(descendents.contains(&"ED"));
		assert!(descendents.contains(&"FC"));

		assert_eq!(tracker.find_ghost(None, |&x| x >= 10), Some(("E", 6)));
		assert_eq!(tracker.find_ghost(Some(("C", 4)), |&x| x >= 10), Some(("E", 6)));
		assert_eq!(tracker.find_ghost(Some(("E", 6)), |&x| x >= 10), Some(("E", 6)));
	}

	#[test]
	fn walk_back_from_block_in_edge_fork_below() {
		let mut chain = DummyChain::new();
		let mut tracker = VoteGraph::new(GENESIS_HASH, 1);

		chain.push_blocks(GENESIS_HASH, &["A", "B", "C"]);
		chain.push_blocks("C", &["D1", "E1", "F1", "G1", "H1", "I1"]);
		chain.push_blocks("C", &["D2", "E2", "F2", "G2", "H2", "I2"]);

		tracker.insert("B", 3, 10usize, &chain).unwrap();
		tracker.insert("F1", 7, 5usize, &chain).unwrap();
		tracker.insert("G2", 8, 5usize, &chain).unwrap();

		let test_cases = &[
			"D1",
			"D2",
			"E1",
			"E2",
			"F1",
			"F2",
			"G2",
		];

		for block in test_cases {
			let number = chain.number(block);
			assert_eq!(tracker.find_ancestor(block, number, |&x| x > 5).unwrap(), ("C", 4));
		}
	}

	#[test]
	fn walk_back_from_fork_block_node_below() {
		let mut chain = DummyChain::new();
		let mut tracker = VoteGraph::new(GENESIS_HASH, 1);

		chain.push_blocks(GENESIS_HASH, &["A", "B", "C", "D"]);
		chain.push_blocks("D", &["E1", "F1", "G1", "H1", "I1"]);
		chain.push_blocks("D", &["E2", "F2", "G2", "H2", "I2"]);

		tracker.insert("B", 3, 10usize, &chain).unwrap();
		tracker.insert("F1", 7, 5usize, &chain).unwrap();
		tracker.insert("G2", 8, 5usize, &chain).unwrap();

		assert_eq!(tracker.find_ancestor("G2", 8, |&x| x > 5).unwrap(), ("D", 5));
		let test_cases = &[
			"E1",
			"E2",
			"F1",
			"F2",
			"G2",
		];

		for block in test_cases {
			let number = chain.number(block);
			assert_eq!(tracker.find_ancestor(block, number, |&x| x > 5).unwrap(), ("D", 5));
		}
	}

	#[test]
	fn walk_back_at_node() {
		let mut chain = DummyChain::new();
		let mut tracker = VoteGraph::new(GENESIS_HASH, 1);

		chain.push_blocks(GENESIS_HASH, &["A", "B", "C"]);
		chain.push_blocks("C", &["D1", "E1", "F1", "G1", "H1", "I1"]);
		chain.push_blocks("C", &["D2", "E2", "F2"]);

		tracker.insert("C", 4, 10usize, &chain).unwrap();
		tracker.insert("F1", 7, 5usize, &chain).unwrap();
		tracker.insert("F2", 7, 5usize, &chain).unwrap();
		tracker.insert("I1", 10, 1usize, &chain).unwrap();

		let test_cases = &[
			"C",
			"D1",
			"D2",
			"E1",
			"E2",
			"F1",
			"F2",
			"I1",
		];

		for block in test_cases {
			let number = chain.number(block);
			assert_eq!(tracker.find_ancestor(block, number, |&x| x >= 20).unwrap(), ("C", 4));
		}
	}
}