// Copyright 2019-2022 PureStake Inc.
// Copyright 2022      Stake Technologies
// Copyright 2022      TraceLabs
// This file is part of Utils package, originally developed by Purestake Inc.
// Utils package used in NeuroWeb Parachain Network in terms of GPLv3.
//
// Utils is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Utils is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Utils.  If not, see <http://www.gnu.org/licenses/>.
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use crate::alloc::borrow::ToOwned;
use fp_evm::{
    Context, ExitError, ExitRevert, ExitSucceed, PrecompileFailure, PrecompileHandle,
    PrecompileOutput,
};
use frame_support::{
    dispatch::{Dispatchable, GetDispatchInfo, PostDispatchInfo},
    traits::Get,
};
use pallet_evm::{GasWeightMapping, Log};
use sp_core::{H160, H256, U256};
use sp_std::{marker::PhantomData, vec, vec::Vec};

mod data;

pub use data::{Address, Bytes, EvmData, EvmDataReader, EvmDataWriter};
pub use precompile_utils_macro::{generate_function_selector, keccak256};

#[cfg(feature = "testing")]
pub mod testing;
#[cfg(test)]
mod tests;

/// Alias for Result returning an EVM precompile error.
pub type EvmResult<T = ()> = Result<T, PrecompileFailure>;

/// Return an error with provided (static) text.
/// Using the `revert` function of `Gasometer` is preferred as erroring
/// consumed all the gas limit and the error message is not easily
/// retrievable.
pub fn error<T: Into<alloc::borrow::Cow<'static, str>>>(text: T) -> PrecompileFailure {
    PrecompileFailure::Error {
        exit_status: ExitError::Other(text.into()),
    }
}

/// Builder for PrecompileOutput.
#[derive(Clone, Debug)]
pub struct LogsBuilder {
    address: H160,
}

impl LogsBuilder {
    /// Create a new builder with no logs.
    /// Takes the address of the precompile (usually `context.address`).
    pub fn new(address: H160) -> Self {
        Self { address }
    }

    /// Create a 0-topic log.
    #[must_use]
    pub fn log0(&self, data: impl Into<Vec<u8>>) -> Log {
        Log {
            address: self.address,
            topics: vec![],
            data: data.into(),
        }
    }

    /// Create a 1-topic log.
    #[must_use]
    pub fn log1(&self, topic0: impl Into<H256>, data: impl Into<Vec<u8>>) -> Log {
        Log {
            address: self.address,
            topics: vec![topic0.into()],
            data: data.into(),
        }
    }

    /// Create a 2-topics log.
    #[must_use]
    pub fn log2(
        &self,
        topic0: impl Into<H256>,
        topic1: impl Into<H256>,
        data: impl Into<Vec<u8>>,
    ) -> Log {
        Log {
            address: self.address,
            topics: vec![topic0.into(), topic1.into()],
            data: data.into(),
        }
    }

    /// Create a 3-topics log.
    #[must_use]
    pub fn log3(
        &self,
        topic0: impl Into<H256>,
        topic1: impl Into<H256>,
        topic2: impl Into<H256>,
        data: impl Into<Vec<u8>>,
    ) -> Log {
        Log {
            address: self.address,
            topics: vec![topic0.into(), topic1.into(), topic2.into()],
            data: data.into(),
        }
    }

    /// Create a 4-topics log.
    #[must_use]
    pub fn log4(
        &self,
        topic0: impl Into<H256>,
        topic1: impl Into<H256>,
        topic2: impl Into<H256>,
        topic3: impl Into<H256>,
        data: impl Into<Vec<u8>>,
    ) -> Log {
        Log {
            address: self.address,
            topics: vec![topic0.into(), topic1.into(), topic2.into(), topic3.into()],
            data: data.into(),
        }
    }
}

/// Extension trait allowing to record logs into a PrecompileHandle.
pub trait LogExt {
    fn record(self, handle: &mut impl PrecompileHandle) -> EvmResult;

    fn compute_cost(&self) -> EvmResult<u64>;
}

impl LogExt for Log {
    fn record(self, handle: &mut impl PrecompileHandle) -> EvmResult {
        handle.log(self.address, self.topics, self.data)?;
        Ok(())
    }

    fn compute_cost(&self) -> EvmResult<u64> {
        log_costs(self.topics.len(), self.data.len())
    }
}

/// Helper functions requiring a Runtime.
/// This runtime must of course implement `pallet_evm::Config`.
#[derive(Clone, Copy, Debug)]
pub struct RuntimeHelper<Runtime>(PhantomData<Runtime>);

impl<Runtime> RuntimeHelper<Runtime>
where
    Runtime: pallet_evm::Config,
    Runtime::RuntimeCall: Dispatchable<PostInfo = PostDispatchInfo> + GetDispatchInfo,
{
    /// Try to dispatch a Substrate call.
    /// Return an error if there are not enough gas, or if the call fails.
    /// If successful returns the used gas using the Runtime GasWeightMapping.
    pub fn try_dispatch<Call>(
        handle: &mut impl PrecompileHandleExt,
        origin: <Runtime::RuntimeCall as Dispatchable>::RuntimeOrigin,
        call: Call,
    ) -> EvmResult<()>
    where
        Runtime::RuntimeCall: From<Call>,
    {
        let call = Runtime::RuntimeCall::from(call);
        let dispatch_info = call.get_dispatch_info();

        // Make sure there is enough gas.
        let remaining_gas = handle.remaining_gas();
        let required_gas = Runtime::GasWeightMapping::weight_to_gas(dispatch_info.weight);
        if required_gas > remaining_gas {
            return Err(PrecompileFailure::Error {
                exit_status: ExitError::OutOfGas,
            });
        }

        // Dispatch call.
        // It may be possible to not record gas cost if the call returns Pays::No.
        // However while Substrate handle checking weight while not making the sender pay for it,
        // the EVM doesn't. It seems this safer to always record the costs to avoid unmetered
        // computations.
        let result = call
            .dispatch(origin)
            .map_err(|e| revert(alloc::format!("Dispatched call failed with error: {:?}", e)))?;

        let used_weight = result.actual_weight;

        let used_gas =
            Runtime::GasWeightMapping::weight_to_gas(used_weight.unwrap_or(dispatch_info.weight));

        handle.record_cost(used_gas)?;

        Ok(())
    }
}

impl<Runtime> RuntimeHelper<Runtime>
where
    Runtime: pallet_evm::Config,
{
    /// Cost of a Substrate DB write in gas.
    pub fn db_write_gas_cost() -> u64 {
        <Runtime as pallet_evm::Config>::GasWeightMapping::weight_to_gas(
            <Runtime as frame_system::Config>::DbWeight::get().writes(1),
        )
    }

    /// Cost of a Substrate DB read in gas.
    pub fn db_read_gas_cost() -> u64 {
        <Runtime as pallet_evm::Config>::GasWeightMapping::weight_to_gas(
            <Runtime as frame_system::Config>::DbWeight::get().reads(1),
        )
    }
}

/// Represents modifiers a Solidity function can be annotated with.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum FunctionModifier {
    /// Function that doesn't modify the state.
    View,
    /// Function that modifies the state but refuse receiving funds.
    /// Correspond to a Solidity function with no modifiers.
    NonPayable,
    /// Function that modifies the state and accept funds.
    Payable,
}

pub trait PrecompileHandleExt: PrecompileHandle {
    #[must_use]
    /// Record cost of a log manually.
    /// This can be useful to record log costs early when their content have static size.
    fn record_log_costs_manual(&mut self, topics: usize, data_len: usize) -> EvmResult;

    #[must_use]
    /// Record cost of logs.
    fn record_log_costs(&mut self, logs: &[&Log]) -> EvmResult;

    #[must_use]
    /// Check that a function call is compatible with the context it is
    /// called into.
    fn check_function_modifier(&self, modifier: FunctionModifier) -> EvmResult;

    #[must_use]
    /// Read the selector from the input data.
    fn read_selector<T>(&self) -> EvmResult<T>
    where
        T: num_enum::TryFromPrimitive<Primitive = u32>;

    #[must_use]
    /// Returns a reader of the input, skipping the selector.
    fn read_input(&self) -> EvmResult<EvmDataReader>;
}

pub fn log_costs(topics: usize, data_len: usize) -> EvmResult<u64> {
    // Cost calculation is copied from EVM code that is not publicly exposed by the crates.
    // https://github.com/rust-blockchain/evm/blob/master/gasometer/src/costs.rs#L148

    const G_LOG: u64 = 375;
    const G_LOGDATA: u64 = 8;
    const G_LOGTOPIC: u64 = 375;

    let topic_cost = G_LOGTOPIC
        .checked_mul(topics as u64)
        .ok_or(PrecompileFailure::Error {
            exit_status: ExitError::OutOfGas,
        })?;

    let data_cost = G_LOGDATA
        .checked_mul(data_len as u64)
        .ok_or(PrecompileFailure::Error {
            exit_status: ExitError::OutOfGas,
        })?;

    G_LOG
        .checked_add(topic_cost)
        .ok_or(PrecompileFailure::Error {
            exit_status: ExitError::OutOfGas,
        })?
        .checked_add(data_cost)
        .ok_or(PrecompileFailure::Error {
            exit_status: ExitError::OutOfGas,
        })
}

impl<T: PrecompileHandle> PrecompileHandleExt for T {
    #[must_use]
    /// Record cost of a log manualy.
    /// This can be useful to record log costs early when their content have static size.
    fn record_log_costs_manual(&mut self, topics: usize, data_len: usize) -> EvmResult {
        self.record_cost(log_costs(topics, data_len)?)?;

        Ok(())
    }

    #[must_use]
    /// Record cost of logs.
    fn record_log_costs(&mut self, logs: &[&Log]) -> EvmResult {
        for log in logs {
            self.record_log_costs_manual(log.topics.len(), log.data.len())?;
        }

        Ok(())
    }

    #[must_use]
    /// Check that a function call is compatible with the context it is
    /// called into.
    fn check_function_modifier(&self, modifier: FunctionModifier) -> EvmResult {
        check_function_modifier(self.context(), self.is_static(), modifier)
    }

    #[must_use]
    /// Read the selector from the input data.
    fn read_selector<S>(&self) -> EvmResult<S>
    where
        S: num_enum::TryFromPrimitive<Primitive = u32>,
    {
        EvmDataReader::read_selector(self.input())
    }

    #[must_use]
    /// Returns a reader of the input, skipping the selector.
    fn read_input(&self) -> EvmResult<EvmDataReader> {
        EvmDataReader::new_skip_selector(self.input())
    }
}

#[must_use]
pub fn revert(output: impl AsRef<[u8]>) -> PrecompileFailure {
    PrecompileFailure::Revert {
        exit_status: ExitRevert::Reverted,
        output: output.as_ref().to_owned(),
    }
}

#[must_use]
pub fn succeed(output: impl AsRef<[u8]>) -> PrecompileOutput {
    PrecompileOutput {
        exit_status: ExitSucceed::Returned,
        output: output.as_ref().to_owned(),
    }
}

#[must_use]
/// Check that a function call is compatible with the context it is
/// called into.
fn check_function_modifier(
    context: &Context,
    is_static: bool,
    modifier: FunctionModifier,
) -> EvmResult {
    if is_static && modifier != FunctionModifier::View {
        return Err(revert("can't call non-static function in static context"));
    }

    if modifier != FunctionModifier::Payable && context.apparent_value > U256::zero() {
        return Err(revert("function is not payable"));
    }

    Ok(())
}