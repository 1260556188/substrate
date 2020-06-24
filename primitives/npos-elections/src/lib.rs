// This file is part of Substrate.

// Copyright (C) 2019-2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A set of election algorithms to be used with a substrate runtime, typically within the staking
//! sub-system. Notable implementation include:
//!
//! - [`seq_phragmen`]: Implements the Phragmén Sequential Method. An un-ranked, relatively fast
//!   election method that ensures PJR, but does not provide a constant factor approximation of the
//!   maximin problem.
//! - [`balance_solution`]: Implements the star balancing algorithm. This iterative process can
//!   increase a solutions score.
//!
//! ### Terminology
//!
//! TODO
//!
//! More information can be found at: https://arxiv.org/abs/2004.12990

#![cfg_attr(not(feature = "std"), no_std)]

use sp_std::{
	prelude::*,
	collections::btree_map::BTreeMap,
	fmt::Debug,
	cmp::Ordering,
	rc::Rc,
	cell::RefCell,
};
use sp_arithmetic::{
	PerThing, Rational128, ThresholdOrd, InnerOf, Normalizable,
	traits::{Zero, Saturating, Bounded},
};

#[cfg(feature = "std")]
use serde::{Serialize, Deserialize};
#[cfg(feature = "std")]
use codec::{Encode, Decode};

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

mod sequential_phragmen;
mod balancing;
mod balanced_heuristic;
mod node;
mod reduce;
mod helpers;

pub use reduce::reduce;
pub use helpers::*;
pub use sequential_phragmen::*;
pub use balancing::*;
pub use balanced_heuristic::*;


// re-export the compact macro, with the dependencies of the macro.
#[doc(hidden)]
pub use codec;
#[doc(hidden)]
pub use sp_arithmetic;
pub use sp_npos_elections_compact::generate_compact_solution_type;

/// A trait to limit the number of votes per voter. The generated compact type will implement this.
pub trait VotingLimit {
	const LIMIT: usize;
}

// TODO: we need an assertion to make sure `LIMIT` * PerThing::max() will fit into upper. And check
// that all the expects that we have here make sense.

/// an aggregator trait for a generic type of a voter/target identifier. This usually maps to
/// substrate's account id.
pub trait IdentifierT: Clone + Eq + Default + Ord + Debug + codec::Codec {}

impl<T: Clone + Eq + Default + Ord + Debug + codec::Codec> IdentifierT for T {}

/// The errors that might occur in the this crate and compact.
#[derive(Debug, Eq, PartialEq)]
pub enum Error {
	/// While going from compact to staked, the stake of all the edges has gone above the
	/// total and the last stake cannot be assigned.
	CompactStakeOverflow,
	/// The compact type has a voter who's number of targets is out of bound.
	CompactTargetOverflow,
	/// One of the index functions returned none.
	CompactInvalidIndex,
	/// An error occurred in some arithmetic operation.
	ArithmeticError(&'static str),
}

/// A type which is used in the API of this crate as a numeric weight of a vote, most often the
/// stake of the voter. It is always converted to [`ExtendedBalance`] for computation.
pub type VoteWeight = u64;

/// A type in which performing operations on vote weights are safe.
pub type ExtendedBalance = u128;

/// The score of an assignment. This can be computed from the support map via [`evaluate_support`].
pub type ElectionScore = [ExtendedBalance; 3];

/// A winner, with their respective approval stake.
pub type WithApprovalOf<A> = (A, ExtendedBalance);

/// A mutable pointer to a candidate.
pub type CandidatePtr<A> = Rc<RefCell<Candidate<A>>>;

/// A candidate entity for the election.
#[derive(Debug, Clone, Default)]
pub struct Candidate<AccountId> {
	/// Identifier.
	who: AccountId,
	/// Intermediary value used to sort candidates.
	score: Rational128,
	/// Sum of the stake of this candidate based on received votes.
	approval_stake: ExtendedBalance,
	/// The final stake of this candidate.
	backed_stake: ExtendedBalance,
	/// Flag for being elected.
	elected: bool,
	/// The round index at which this candidate was elected.
	round: usize,
}

/// A voter entity.
#[derive(Clone, Default, Debug)]
pub struct Voter<AccountId> {
	/// Identifier.
	who: AccountId,
	/// List of candidates proposed by this voter.
	edges: Vec<Edge<AccountId>>,
	/// The stake of this voter.
	budget: ExtendedBalance,
	/// Incremented each time a candidate that this voter voted for has been elected.
	load: Rational128,
}

impl<AccountId: IdentifierT> Voter<AccountId> {
	/// Returns none if this voter does not have any non-zero distributions.
	///
	/// Note that this might create _un-normalized_ assignments, due to accuracy loss of `P`. Call
	/// site might compensate by calling `normalize()` on the returned `Assignment` as a
	/// post-precessing.
	pub fn into_assignment<P: PerThing>(self) -> Option<Assignment<AccountId, P>>
	where
		ExtendedBalance: From<InnerOf<P>>,
	{
		let who = self.who;
		let budget = self.budget;
		let distribution = self.edges.into_iter().filter_map(|e| {
			let per_thing = P::from_rational_approximation(e.weight, budget);
			// trim zero edges.
			if per_thing.is_zero() { None } else { Some((e.who, per_thing)) }
		}).collect::<Vec<_>>();

		if distribution.len() > 0 {
			Some(Assignment { who, distribution })
		} else {
			None
		}
	}

	/// Try and normalize the votes of self.
	///
	/// If the normalization is successful then `true` is returned.
	pub fn try_normalize(&mut self) -> Result<(), &'static str> {
		let edge_weights = self.edges.iter().map(|e| e.weight).collect::<Vec<_>>();
		edge_weights.normalize(self.budget).map(|normalized| {
			// here we count on the fact that normalize does not change the order.
			for (edge, corrected) in self.edges.iter_mut().zip(normalized.into_iter()) {
				let mut candidate = edge.candidate.borrow_mut();
				// first, subtract the incorrect weight
				candidate.backed_stake = candidate.backed_stake.saturating_sub(edge.weight);
				edge.weight = corrected;
				// Then add the correct one again.
				candidate.backed_stake = candidate.backed_stake.saturating_add(edge.weight);
			}
		})
	}
}

/// A candidate being backed by a voter.
#[derive(Clone, Default, Debug)]
pub struct Edge<AccountId> {
	/// Identifier.
	// TODO: this is redundant; remove it and use candidate.who.
	who: AccountId,
	/// Load of this vote.
	load: Rational128,
	/// pointer to the candidate.
	candidate: CandidatePtr<AccountId>,
	/// The weight (i.e. stake given to `who`) of this edge. Only used in [`balanced_heuristic`] for
	/// now.
	weight: ExtendedBalance,
}

/// Final result of the election.
#[derive(Debug)]
pub struct ElectionResult<AccountId, P: PerThing> {
	/// Just winners zipped with their approval stake. Note that the approval stake is merely the
	/// sub of their received stake and could be used for very basic sorting and approval voting.
	pub winners: Vec<WithApprovalOf<AccountId>>,
	/// Individual assignments. for each tuple, the first elements is a voter and the second
	/// is the list of candidates that it supports.
	pub assignments: Vec<Assignment<AccountId, P>>,
}

/// A voter's stake assignment among a set of targets, represented as ratios.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "std", derive(PartialEq, Eq, Encode, Decode))]
pub struct Assignment<AccountId, P: PerThing> {
	/// Voter's identifier.
	pub who: AccountId,
	/// The distribution of the voter's stake.
	pub distribution: Vec<(AccountId, P)>,
}

impl<AccountId: IdentifierT, P: PerThing> Assignment<AccountId, P>
where
	ExtendedBalance: From<InnerOf<P>>,
{
	/// Convert from a ratio assignment into one with absolute values aka. [`StakedAssignment`].
	///
	/// It needs `stake` which is the total budget of the voter. If `fill` is set to true,
	/// it _tries_ to ensure that all the potential rounding errors are compensated and the
	/// distribution's sum is exactly equal to the total budget, by adding or subtracting the
	/// remainder from the last distribution.
	///
	/// If an edge ratio is [`Bounded::min_value()`], it is dropped. This edge can never mean
	/// anything useful.
	pub fn into_staked(self, stake: ExtendedBalance) -> StakedAssignment<AccountId>
	where
		P: sp_std::ops::Mul<ExtendedBalance, Output = ExtendedBalance>,
	{
		let distribution = self.distribution
			.into_iter()
			.filter_map(|(target, p)| {
				// if this ratio is zero, then skip it.
				if p.is_zero() {
					None
				} else {
					// NOTE: this mul impl will always round to the nearest number, so we might both
					// overflow and underflow.
					let distribution_stake = p * stake;
					Some((target, distribution_stake))
				}
			})
			.collect::<Vec<(AccountId, ExtendedBalance)>>();

		StakedAssignment {
			who: self.who,
			distribution,
		}
	}

	/// Try and normalize this assignment.
	///
	/// If `Ok(())` is returned, then the assignment MUST have been successfully normalized to 100%.
	pub fn try_normalize(&mut self) -> Result<(), &'static str> {
		self.distribution
			.iter()
			.map(|(_, p)| *p)
			.collect::<Vec<_>>()
			.normalize(P::one())
			.map(|normalized_ratios|
				self.distribution
					.iter_mut()
					.zip(normalized_ratios)
					.for_each(|((_, old), corrected)| { *old = corrected; })
			)
	}
}

/// A voter's stake assignment among a set of targets, represented as absolute values in the scale
/// of [`ExtendedBalance`].
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "std", derive(PartialEq, Eq, Encode, Decode))]
pub struct StakedAssignment<AccountId> {
	/// Voter's identifier
	pub who: AccountId,
	/// The distribution of the voter's stake.
	pub distribution: Vec<(AccountId, ExtendedBalance)>,
}

impl<AccountId> StakedAssignment<AccountId> {
	/// Converts self into the normal [`Assignment`] type.
	///
	/// If `fill` is set to true, it _tries_ to ensure that all the potential rounding errors are
	/// compensated and the distribution's sum is exactly equal to 100%, by adding or subtracting
	/// the remainder from the last distribution.
	///
	/// NOTE: it is quite critical that this attempt always works. The data type returned here will
	/// potentially get used to create a compact type; a compact type requires sum of ratios to be
	/// less than 100% upon un-compacting.
	///
	/// If an edge stake is so small that it cannot be represented in `T`, it is ignored. This edge
	/// can never be re-created and does not mean anything useful anymore.
	pub fn into_assignment<P: PerThing>(self) -> Assignment<AccountId, P>
	where
		ExtendedBalance: From<InnerOf<P>>,
		AccountId: IdentifierT,
	{
		let stake = self.total();
		let distribution = self.distribution
			.into_iter()
			.filter_map(|(target, w)| {
				let per_thing = P::from_rational_approximation(w, stake);
				if per_thing == Bounded::min_value() {
					None
				} else {
					Some((target, per_thing))
				}
			})
			.collect::<Vec<(AccountId, P)>>();

		Assignment {
			who: self.who,
			distribution,
		}
	}

	/// Try and normalize this assignment.
	///
	/// If `Ok(())` is returned, then the assignment MUST have been successfully normalized to
	/// `stake`.
	///
	/// NOTE: current implementation of `.normalize` is almost safe to `expect()` upon. The only
	/// error case is when the input cannot fit in `T`, or the sum of input cannot fit in `T`.
	/// Sadly, both of these are dependent upon the implementation of `VoteLimit`, i.e. the limit
	/// of edges per voter which is enforced from upstream. Hence, at this crate, we prefer
	/// returning a result and a use the name prefix `try_`.
	pub fn try_normalize(&mut self, stake: ExtendedBalance) -> Result<(), &'static str> {
		self.distribution
			.iter()
			.map(|(_, ref weight)| *weight)
			.collect::<Vec<_>>()
			.normalize(stake)
			.map(|normalized_weights|
				self.distribution
					.iter_mut()
					.zip(normalized_weights.into_iter())
					.for_each(|((_, weight), corrected)| { *weight = corrected; })
			)
	}

	/// Get the total stake of this assignment (aka voter budget).
	pub fn total(&self) -> ExtendedBalance {
		self.distribution.iter().fold(Zero::zero(), |a, b| a.saturating_add(b.1))
	}
}

/// A structure to demonstrate the election result from the perspective of the candidate, i.e. how
/// much support each candidate is receiving.
///
/// This complements the [`ElectionResult`] and is needed to run the balancing post-processing.
///
/// This, at the current version, resembles the `Exposure` defined in the Staking pallet, yet
/// they do not necessarily have to be the same.
#[derive(Default, Debug)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize, Eq, PartialEq))]
pub struct Support<AccountId> {
	/// Total support.
	pub total: ExtendedBalance,
	/// Support from voters.
	pub voters: Vec<(AccountId, ExtendedBalance)>,
}

/// A linkage from a candidate and its [`Support`].
pub type SupportMap<A> = BTreeMap<A, Support<A>>;

/// Build the support map from the given election result. It maps a flat structure like
///
/// ```nocompile
/// assignments: vec![
/// 	voter1, vec![(candidate1, w11), (candidate2, w12)],
/// 	voter2, vec![(candidate1, w21), (candidate2, w22)]
/// ]
/// ```
///
/// into a mapping of candidates and their respective support:
///
/// ```nocompile
///  SupportMap {
/// 	candidate1: Support {
/// 		own:0,
/// 		total: w11 + w21,
/// 		others: vec![(candidate1, w11), (candidate2, w21)]
///		},
/// 	candidate2: Support {
/// 		own:0,
/// 		total: w12 + w22,
/// 		others: vec![(candidate1, w12), (candidate2, w22)]
///		},
/// }
/// ```
///
/// The second returned flag indicates the number of edges who didn't corresponded to an actual
/// winner from the given winner set. A value in this place larger than 0 indicates a potentially
/// faulty assignment.
///
/// `O(E)` where `E` is the total number of edges.
pub fn build_support_map<AccountId>(
	winners: &[AccountId],
	assignments: &[StakedAssignment<AccountId>],
) -> (SupportMap<AccountId>, u32) where
	AccountId: IdentifierT,
{
	let mut errors = 0;
	// Initialize the support of each candidate.
	let mut supports = <SupportMap<AccountId>>::new();
	winners
		.iter()
		.for_each(|e| { supports.insert(e.clone(), Default::default()); });

	// build support struct.
	for StakedAssignment { who, distribution } in assignments.iter() {
		for (c, weight_extended) in distribution.iter() {
			if let Some(support) = supports.get_mut(c) {
				support.total = support.total.saturating_add(*weight_extended);
				support.voters.push((who.clone(), *weight_extended));
			} else {
				errors = errors.saturating_add(1);
			}
		}
	}
	(supports, errors)
}

/// Evaluate a support map. The returned tuple contains:
///
/// - Minimum support. This value must be **maximized**.
/// - Sum of all supports. This value must be **maximized**.
/// - Sum of all supports squared. This value must be **minimized**.
///
/// `O(E)` where `E` is the total number of edges.
pub fn evaluate_support<AccountId>(
	support: &SupportMap<AccountId>,
) -> ElectionScore {
	let mut min_support = ExtendedBalance::max_value();
	let mut sum: ExtendedBalance = Zero::zero();
	// NOTE: The third element might saturate but fine for now since this will run on-chain and need
	// to be fast.
	let mut sum_squared: ExtendedBalance = Zero::zero();
	for (_, support) in support.iter() {
		sum = sum.saturating_add(support.total);
		let squared = support.total.saturating_mul(support.total);
		sum_squared = sum_squared.saturating_add(squared);
		if support.total < min_support {
			min_support = support.total;
		}
	}
	[min_support, sum, sum_squared]
}

/// Compares two sets of election scores based on desirability and returns true if `this` is
/// better than `that`.
///
/// Evaluation is done in a lexicographic manner, and if each element of `this` is `that * epsilon`
/// greater or less than `that`.
///
/// Note that the third component should be minimized.
pub fn is_score_better<P: PerThing>(this: ElectionScore, that: ElectionScore, epsilon: P) -> bool
	where ExtendedBalance: From<sp_arithmetic::InnerOf<P>>
{
	match this
		.iter()
		.enumerate()
		.map(|(i, e)| (
			e.ge(&that[i]),
			e.tcmp(&that[i], epsilon.mul_ceil(that[i])),
		))
		.collect::<Vec<(bool, Ordering)>>()
		.as_slice()
	{
		// epsilon better in the score[0], accept.
		[(_, Ordering::Greater), _, _] => true,

		// less than epsilon better in score[0], but more than epsilon better in the second.
		[(true, Ordering::Equal), (_, Ordering::Greater), _] => true,

		// less than epsilon better in score[0, 1], but more than epsilon better in the third
		[(true, Ordering::Equal), (true, Ordering::Equal), (_, Ordering::Less)] => true,

		// anything else is not a good score.
		_ => false,
	}
}

/// Converts raw inputs to types used in this crate.
///
/// This drops any votes that are pointing to non-candidates.
pub(crate) fn setup_inputs<AccountId: IdentifierT>(
	initial_candidates: Vec<AccountId>,
	initial_voters: Vec<(AccountId, VoteWeight, Vec<AccountId>)>,
) -> (Vec<CandidatePtr<AccountId>>, Vec<Voter<AccountId>>) {
	// used to cache and access candidates index.
	let mut c_idx_cache = BTreeMap::<AccountId, usize>::new();

	let candidates = initial_candidates
		.into_iter()
		.enumerate()
		.map(|(idx, who)| {
			c_idx_cache.insert(who.clone(), idx);
			Rc::new(RefCell::new(Candidate { who, ..Default::default() }))
		})
		.collect::<Vec<CandidatePtr<AccountId>>>();

	let voters = initial_voters.into_iter().map(|(who, voter_stake, votes)| {
		let mut edges: Vec<Edge<AccountId>> = Vec::with_capacity(votes.len());
		for v in votes {
			if let Some(idx) = c_idx_cache.get(&v) {
				// This candidate is valid + already cached.
				let mut candidate = candidates[*idx].borrow_mut();
				candidate.approval_stake =
					candidate.approval_stake.saturating_add(voter_stake.into());
				edges.push(
					Edge {
						who: v.clone(),
						candidate: Rc::clone(&candidates[*idx]),
						..Default::default()
					}
				);
			} // else {} would be wrong votes. We don't really care about it.
		}
		Voter {
			who,
			edges: edges,
			budget: voter_stake.into(),
			load: Rational128::zero(),
		}
	}).collect::<Vec<_>>();

	(candidates, voters,)
}
