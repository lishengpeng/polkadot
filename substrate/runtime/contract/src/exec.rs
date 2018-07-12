// Copyright 2018 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate. If not, see <http://www.gnu.org/licenses/>.

use super::{CodeOf, Trait};
use account_db::{AccountDb, OverlayAccountDb};
use double_map::StorageDoubleMap;
use rstd::prelude::*;
use runtime_support::StorageMap;
use vm;

pub struct TransactionData {
	// tx_origin
	// tx_gas_price
	// block_number
	// timestamp
	// etc
}

pub struct ExecutionContext<'a, T: Trait + 'a> {
	// aux for the first transaction.
	pub _caller: T::AccountId,
	// typically should be dest
	pub self_account: T::AccountId,
	pub account_db: &'a mut OverlayAccountDb<'a, T>,
	pub gas_price: u64,
	pub depth: usize,
}

impl<'a, T: Trait> ExecutionContext<'a, T> {
	/// Make a call to the specified address.
	pub fn call(
		&mut self,
		dest: T::AccountId,
		_value: T::Balance,
		gas_limit: u64,
		_data: Vec<u8>,
	) -> Result<vm::ExecutionResult, ()> {
		let dest_code = <CodeOf<T>>::get(&dest);

		let mut overlay = OverlayAccountDb::new(self.account_db);

		// TODO: transfer `_value` using `overlay`. Return an error if failed.

		if !dest_code.is_empty() {
			let mut nested = ExecutionContext {
				account_db: &mut overlay,
				_caller: self.self_account.clone(),
				self_account: dest.clone(),
				gas_price: self.gas_price,
				depth: self.depth + 1,
			};

			let exec_result = vm::execute(&dest_code, &mut nested, gas_limit).map_err(|_| ())?;

			// TODO: Need to propagate gas_left.
			// TODO: Need to return result buffer.

			Ok(exec_result)
		} else {
			// that was a plain transfer
			Ok(vm::ExecutionResult {
				gas_left: gas_limit,
				return_data: Vec::new(),
			})
		}
	}

	// TODO: fn create
}

impl<'a, T: Trait + 'a> vm::Ext for ExecutionContext<'a, T> {
	type AccountId = T::AccountId;
	type Balance = T::Balance;

	fn get_storage(&self, key: &[u8]) -> Option<Vec<u8>> {
		self.account_db.get_storage(&self.self_account, key)
	}

	fn set_storage(&mut self, key: &[u8], value: Option<Vec<u8>>) {
		self.account_db
			.set_storage(&self.self_account, key.to_vec(), value)
	}

	fn create(&mut self, code: &[u8], value: Self::Balance) {
		panic!()
	}

	fn call(
		&mut self,
		to: &Self::AccountId,
		value: Self::Balance,
		gas_limit: u64,
		input_data: Vec<u8>,
	) -> Result<vm::ExecutionResult, ()> {
		self.call(to.clone(), value, gas_limit, input_data)
	}
}