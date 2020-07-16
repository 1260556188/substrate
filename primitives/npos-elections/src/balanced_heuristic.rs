// This file is part of Substrate.

// Copyright (C) 2020 Parity Technologies (UK) Ltd.
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

//! TODO:

use crate::{
	IdentifierT, ElectionResult, ExtendedBalance, setup_inputs, VoteWeight, Voter, CandidatePtr,
	balance,
};
use sp_arithmetic::{PerThing, InnerOf, Rational128};
use sp_std::{prelude::*, rc::Rc};

/// TODO:
pub fn balanced_heuristic<AccountId: IdentifierT, P: PerThing>(
	to_elect: usize,
	initial_candidates: Vec<AccountId>,
	initial_voters: Vec<(AccountId, VoteWeight, Vec<AccountId>)>,
) -> Result<ElectionResult<AccountId, P>, &'static str>
	where ExtendedBalance: From<InnerOf<P>>
{
	let (candidates, mut voters) = setup_inputs(initial_candidates, initial_voters);

	let mut winners = vec![];
	for round in 0..to_elect {
		let round_winner = calculate_max_score::<AccountId, P>(&candidates, &voters);
		apply_elected::<AccountId>(&mut voters, Rc::clone(&round_winner));

		round_winner.borrow_mut().round = round;
		round_winner.borrow_mut().elected = true;
		winners.push(round_winner);

		balance(&mut voters, 2, 0);
	}

	let mut assignments = voters.into_iter().filter_map(|v| v.into_assignment()).collect::<Vec<_>>();
	let _ = assignments.iter_mut().map(|a| a.try_normalize()).collect::<Result<(), _>>()?;
	let winners = winners.into_iter().map(|w_ptr|
		(w_ptr.borrow().who.clone(), w_ptr.borrow().backed_stake)
	).collect();

	Ok(ElectionResult { winners, assignments })
}

/// Find the candidate that can yield the maximum score for this round.
///
/// Returns a new `CandidatePtr` to the winner candidate. The score of the candidate is updated and
/// can be read from the returned pointer.
///
/// This is an internal part of the [`balanced_heuristic`].
pub(crate) fn calculate_max_score<AccountId: IdentifierT, P: PerThing>(
	candidates: &[CandidatePtr<AccountId>],
	voters: &[Voter<AccountId>],
) -> CandidatePtr<AccountId> where ExtendedBalance: From<InnerOf<P>> {
	for c_ptr in candidates.iter() {
		let mut candidate = c_ptr.borrow_mut();
		if !candidate.elected {
			candidate.score = Rational128::from(1, P::ACCURACY.into());
		}
	}

	// TODO: impl of compare for Rational128 need to be sound and fuzzed.
	for voter in voters.iter() {
		let mut denominator_contribution: ExtendedBalance = 0;

		// gather contribution from all elected edges.
		for edge in voter.edges.iter() {
			let edge_candidate = edge.candidate.borrow();
			if edge_candidate.elected {
				let edge_contribution: ExtendedBalance = P::from_rational_approximation(
					edge.weight,
					edge_candidate.backed_stake,
				).deconstruct().into();
				denominator_contribution += edge_contribution;
			}
		}

		// distribute to all _unelected_ edges.
		for edge in voter.edges.iter() {
			let mut edge_candidate = edge.candidate.borrow_mut();
			if !edge_candidate.elected {
				// TODO: make a fn for this. Something like accumulate numerator or add denominator.s
				let prev_d = edge_candidate.score.d();
				edge_candidate.score = Rational128::from(1, denominator_contribution + prev_d);
			}
		}
	}

	// finalise the score value, and find the best.
	let mut best_score = Rational128::zero();
	let mut best_candidate = Rc::clone(&candidates[0]);
	for c_ptr in candidates.iter() {
		let mut candidate = c_ptr.borrow_mut();
		if candidate.approval_stake > 0  {
			// finalise the score value.
			let score_d = candidate.score.d();
			let one: ExtendedBalance = P::ACCURACY.into();
			let score_n = candidate.approval_stake.checked_mul(one).unwrap_or_else(|| {
				println!("Failed to mul {:?} and {:?}", candidate.approval_stake, one);
				panic!();
				sp_arithmetic::traits::Bounded::max_value()
			});
			candidate.score = Rational128::from(score_n, score_d);

			// check if we have a new winner.
			if !candidate.elected && candidate.score > best_score {
				best_score = candidate.score;
				best_candidate = Rc::clone(&c_ptr);
			}
		} else {
			candidate.score = Rational128::zero();
		}
	}

	best_candidate
}

/// Update the weights of `voters` given that `elected_ptr` has been elected in the previous round.
///
/// Updates `voters` in place.
///
/// This is an internal part of the [`balanced_heuristic`] and should be called after
/// [`calculate_max_score`].
pub(crate) fn apply_elected<AccountId: IdentifierT>(
	voters: &mut Vec<Voter<AccountId>>,
	elected_ptr: CandidatePtr<AccountId>,
) {
	let mut elected = elected_ptr.borrow_mut();
	let cutoff = elected.score.to_den(1) // TODO: check this again.
		.expect("(n / d) < u128::max() and (n' / 1) == (n / d), thus n' < u128::max()' qed.")
		.n();

	for voter_index in 0..voters.len() {
		let voter = &mut voters[voter_index];

		for new_edge_index in 0..voter.edges.len() {
			let new_edge_immutable = &voter.edges[new_edge_index];


			// ideally, we'd have to do:
			// new_edge_immutable.candidate.borrow().who == elected.who
			// but this will fail because if equality is correct, then borrowing will panic. Hence,
			// we play with fire here and assume that if `try_borrow()` fails, then it was the same
			// candidate.
			if new_edge_immutable.candidate.try_borrow().is_err() {
				let used_budget: ExtendedBalance = voter.edges.iter().map(|e| e.weight).sum();
				let new_edge_weight = voter.budget.saturating_sub(used_budget);

				// just for brevity. We won't need this ref anymore and want to create a new mutable
				// one now.
				drop(new_edge_immutable);

				let mut new_edge_mut = &mut voter.edges[new_edge_index];
				new_edge_mut.weight = new_edge_weight;
				elected.backed_stake = elected.backed_stake.saturating_add(new_edge_weight);
				drop(new_edge_mut);

				for edge_index in 0..voter.edges.len() {
					let mut edge = &mut voter.edges[edge_index];

					// opposite of the above error. We need to ensure
					// `edge.candidate.borrow().who != elected.who`, thus if borrowing is okay they
					// are different.
					if edge.weight > 0 && edge.candidate.try_borrow().is_ok() {
						let mut edge_candidate = edge.candidate.borrow_mut();

						if edge_candidate.backed_stake > cutoff {
							let stake_to_take = edge.weight * cutoff / edge_candidate.backed_stake;
							edge.weight -= stake_to_take;
							edge_candidate.backed_stake -= stake_to_take;
							elected.backed_stake += stake_to_take;

							// drop previous borrows of `voter.edges`.
							drop(edge_candidate);
							drop(edge);

							// make another temporary borrow of `voter.edges`.
							voter.edges[new_edge_index].weight += stake_to_take;
						}
					}
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{ElectionResult, Assignment};
	use sp_runtime::{Perbill, Percent};
	use sp_std::rc::Rc;

	#[test]
	fn basic_election_manual_works() {
		let candidates = vec![1, 2, 3];
		let voters = vec![
			(10, 10, vec![1, 2]),
			(20, 20, vec![1, 3]),
			(30, 30, vec![2, 3]),
		];

		let (candidates, mut voters) = setup_inputs(candidates, voters);

		// Round 1
		let winner = calculate_max_score::<u32, Percent>(candidates.as_ref(), voters.as_ref());
		assert_eq!(winner.borrow().who, 3);
		assert_eq!(winner.borrow().score, 50u32.into());

		apply_elected(&mut voters, Rc::clone(&winner));
		assert_eq!(
			voters.iter().find(|x| x.who == 30).map(|v| (
				v.who,
				v.edges.iter().map(|e| (e.who, e.weight)).collect::<Vec<_>>()
			)).unwrap(),
			(30, vec![(2, 0), (3, 30)]),
		);
		assert_eq!(
			voters.iter().find(|x| x.who == 20).map(|v| (
				v.who,
				v.edges.iter().map(|e| (e.who, e.weight)).collect::<Vec<_>>()
			)).unwrap(),
			(20, vec![(1, 0), (3, 20)]),
		);

		// finish the round.
		winner.borrow_mut().elected = true;
		winner.borrow_mut().round = 0;
		drop(winner);

		// balancing makes no difference here but anyhow.
		balance(&mut voters, 10, 0);

		// round 2
		let winner = calculate_max_score::<u32, Percent>(candidates.as_ref(), voters.as_ref());
		assert_eq!(winner.borrow().who, 2);
		assert_eq!(winner.borrow().score, 25u32.into());

		apply_elected(&mut voters, Rc::clone(&winner));
		assert_eq!(
			voters.iter().find(|x| x.who == 30).map(|v| (
				v.who,
				v.edges.iter().map(|e| (e.who, e.weight)).collect::<Vec<_>>()
			)).unwrap(),
			(30, vec![(2, 15), (3, 15)]),
		);
		assert_eq!(
			voters.iter().find(|x| x.who == 20).map(|v| (
				v.who,
				v.edges.iter().map(|e| (e.who, e.weight)).collect::<Vec<_>>()
			)).unwrap(),
			(20, vec![(1, 0), (3, 20)]),
		);
		assert_eq!(
			voters.iter().find(|x| x.who == 10).map(|v| (
				v.who,
				v.edges.iter().map(|e| (e.who, e.weight)).collect::<Vec<_>>()
			)).unwrap(),
			(10, vec![(1, 0), (2, 10)]),
		);

		// finish the round.
		winner.borrow_mut().elected = true;
		winner.borrow_mut().round = 0;
		drop(winner);

		// balancing will improve stuff here.
		balance(&mut voters, 10, 0);

		assert_eq!(
			voters.iter().find(|x| x.who == 30).map(|v| (
				v.who,
				v.edges.iter().map(|e| (e.who, e.weight)).collect::<Vec<_>>()
			)).unwrap(),
			(30, vec![(2, 20), (3, 10)]),
		);
		assert_eq!(
			voters.iter().find(|x| x.who == 20).map(|v| (
				v.who,
				v.edges.iter().map(|e| (e.who, e.weight)).collect::<Vec<_>>()
			)).unwrap(),
			(20, vec![(1, 0), (3, 20)]),
		);
		assert_eq!(
			voters.iter().find(|x| x.who == 10).map(|v| (
				v.who,
				v.edges.iter().map(|e| (e.who, e.weight)).collect::<Vec<_>>()
			)).unwrap(),
			(10, vec![(1, 0), (2, 10)]),
		);
	}

	#[test]
	fn basic_election_works() {
		let candidates = vec![1, 2, 3];
		let voters = vec![
			(10, 10, vec![1, 2]),
			(20, 20, vec![1, 3]),
			(30, 30, vec![2, 3]),
		];

		let ElectionResult { winners, assignments } = balanced_heuristic::<_, Perbill>(2, candidates, voters).unwrap();
		assert_eq!(winners, vec![(3, 30), (2, 30)]);
		assert_eq!(
			assignments,
			vec![
				Assignment {
					who: 10u64,
					distribution: vec![(2, Perbill::one())],
				},
				Assignment {
					who: 20,
					distribution: vec![(3, Perbill::one())],
				},
				Assignment {
					who: 30,
					distribution: vec![
						(2, Perbill::from_parts(666666666)),
						(3, Perbill::from_parts(333333334)),
					],
				},
			]
		)
	}

	#[test]
	fn linear_voting_example_works() {
		let _ = env_logger::try_init();
		let candidates = vec![11, 21, 31, 41, 51, 61, 71];
		let voters = vec![
			(2, 2000, vec![11]),
			(4, 1000, vec![11, 21]),
			(6, 1000, vec![21, 31]),
			(8, 1000, vec![31, 41]),
			(110, 1000, vec![41, 51]),
			(120, 1000, vec![51, 61]),
			(130, 1000, vec![61, 71]),
		];

		let ElectionResult { winners, assignments: _ } = balanced_heuristic::<_, Perbill>(4, candidates, voters).unwrap();
		assert_eq!(winners, vec![
			(11, 3000),
			(31, 2000),
			(51, 1500),
			(61, 1500),
		]);

	}
}
