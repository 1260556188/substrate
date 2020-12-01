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

//! Lottery pallet benchmarking.

#![cfg(feature = "runtime-benchmarks")]

use super::*;

use frame_system::RawOrigin;
use frame_support::traits::{OnInitialize, UnfilteredDispatchable};
use frame_benchmarking::{benchmarks, account, whitelisted_caller};
use sp_runtime::traits::Bounded;

use crate::Module as Lottery;

// Set up and start a lottery
fn setup_lottery<T: Config>() -> Result<(), &'static str> {
	let price = T::Currency::minimum_balance();
	let length = 10u32.into();
	let delay = 5u32.into();
	// Calls will be maximum length...
	let mut calls = vec![
		frame_system::Call::<T>::set_code(vec![]).into();
		T::MaxCalls::get().saturating_sub(1)
	];
	// Last call will be the match for worst case scenario.
	calls.push(frame_system::Call::<T>::remark(vec![]).into());
	let origin = T::ManagerOrigin::successful_origin();
	Lottery::<T>::start_lottery(origin, price, length, delay, calls, false)?;
	Ok(())
}

benchmarks! {
	_ { }

	start_lottery {
		let n in 0 .. T::MaxCalls::get() as u32;
		let price = BalanceOf::<T>::max_value();
		let end = 10u32.into();
		let payout = 5u32.into();
		let calls = vec![frame_system::Call::<T>::remark(vec![]).into(); n as usize];

		let call = Call::<T>::start_lottery(price, end, payout, calls, true);
		let origin = T::ManagerOrigin::successful_origin();
	}: { call.dispatch_bypass_filter(origin)? }
	verify {
		assert!(crate::Lottery::<T>::get().is_some());
	}

	buy_ticket {
		let caller = whitelisted_caller();
		T::Currency::make_free_balance_be(&caller, BalanceOf::<T>::max_value());
		setup_lottery::<T>()?;
		// force user to have a long vec of calls participating
		let set_code_index: CallIndex = Lottery::<T>::call_to_index(
			&frame_system::Call::<T>::set_code(vec![]).into()
		)?;
		let already_called: (Index, Vec<CallIndex>) = (
			LotteryIndex::get(),
			vec![
				set_code_index;
				T::MaxCalls::get().saturating_sub(1)
			],
		);
		Participants::<T>::insert(&caller, already_called);

		let call = frame_system::Call::<T>::remark(vec![]);
	}: _(RawOrigin::Signed(caller), Box::new(call.into()))
	verify {
		assert_eq!(TicketsCount::get(), 1);
	}

	on_initialize {
		setup_lottery::<T>()?;
		let winner = account("winner", 0, 0);
		// User needs more than min balance to get ticket
		T::Currency::make_free_balance_be(&winner, T::Currency::minimum_balance() * 10u32.into());
		// Make sure lottery account has at least min balance too
		let lottery_account = Lottery::<T>::account_id();
		T::Currency::make_free_balance_be(&lottery_account, T::Currency::minimum_balance() * 10u32.into());
		// Buy a ticket
		let call = frame_system::Call::<T>::remark(vec![]);
		Lottery::<T>::buy_ticket(RawOrigin::Signed(winner.clone()).into(), Box::new(call.into()))?;
		// Kill user account for worst case
		T::Currency::make_free_balance_be(&winner, 0u32.into());
		// Assert that lotto is set up for winner
		assert_eq!(TicketsCount::get(), 1);
		assert!(!Lottery::<T>::pot().1.is_zero());
	}: {
		// Start lottery has block 15 configured for payout
		Lottery::<T>::on_initialize(15u32.into());
	}
	verify {
		assert!(crate::Lottery::<T>::get().is_none());
		assert_eq!(TicketsCount::get(), 0);
		assert_eq!(Lottery::<T>::pot().1, 0u32.into());
		assert!(!T::Currency::free_balance(&winner).is_zero())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::mock::{new_test_ext, Test};
	use frame_support::assert_ok;

	#[test]
	fn test_benchmarks() {
		new_test_ext().execute_with(|| {
			assert_ok!(test_benchmark_start_lottery::<Test>());
			assert_ok!(test_benchmark_buy_ticket::<Test>());
			assert_ok!(test_benchmark_on_initialize::<Test>());
		});
	}
}
