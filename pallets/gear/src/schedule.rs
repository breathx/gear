// This file is part of Gear.

// Copyright (C) 2022 Gear Technologies Inc.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! This module contains the cost schedule and supporting code that constructs a
//! sane default schedule from a `WeightInfo` implementation.

#![allow(unused_parens)]

use crate::{weights::WeightInfo, Config};

use codec::{Decode, Encode};
use frame_support::DefaultNoBound;
use gear_core::{code, costs::HostFnWeights as CoreHostFnWeights};
use pallet_gear_proc_macro::{ScheduleDebug, WeightDebug};
use scale_info::TypeInfo;
#[cfg(feature = "std")]
use serde::{Deserialize, Serialize};
use sp_runtime::RuntimeDebug;
use sp_std::{marker::PhantomData, vec::Vec};
use wasm_instrument::{gas_metering, parity_wasm::elements};

/// How many API calls are executed in a single batch. The reason for increasing the amount
/// of API calls in batches (per benchmark component increase) is so that the linear regression
/// has an easier time determining the contribution of that component.
pub const API_BENCHMARK_BATCH_SIZE: u32 = 100;

/// How many instructions are executed in a single batch.
pub const INSTR_BENCHMARK_BATCH_SIZE: u32 = 100;

/// Definition of the cost schedule and other parameterization for the wasm vm.
///
/// Its [`Default`] implementation is the designated way to initialize this type. It uses
/// the benchmarked information supplied by [`Config::WeightInfo`]. All of its fields are
/// public and can therefore be modified. For example in order to change some of the limits
/// and set a custom instruction weight version the following code could be used:
/// ```rust
/// use pallet_gear::{Schedule, Limits, InstructionWeights, Config};
///
/// fn create_schedule<T: Config>() -> Schedule<T> {
///     Schedule {
///         limits: Limits {
///                 globals: 3,
///                 parameters: 3,
///                 memory_pages: 16,
///                 table_size: 3,
///                 br_table_size: 3,
///                 .. Default::default()
///             },
///         instruction_weights: InstructionWeights {
///                 version: 5,
///             .. Default::default()
///         },
///             .. Default::default()
///     }
/// }
/// ```
///
/// # Note
///
/// Please make sure to bump the [`InstructionWeights::version`] whenever substantial
/// changes are made to its values.
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "std", serde(bound(serialize = "", deserialize = "")))]
#[derive(Clone, Encode, Decode, PartialEq, Eq, ScheduleDebug, DefaultNoBound, TypeInfo)]
#[scale_info(skip_type_params(T))]
pub struct Schedule<T: Config> {
    /// Describes the upper limits on various metrics.
    pub limits: Limits,

    /// The weights for individual wasm instructions.
    pub instruction_weights: InstructionWeights<T>,

    /// The weights for each imported function a program is allowed to call.
    pub host_fn_weights: HostFnWeights<T>,

    /// The weights for memory interaction.
    pub memory_weights: MemoryWeights<T>,
}

/// Describes the upper limits on various metrics.
///
/// # Note
///
/// The values in this struct should never be decreased. The reason is that decreasing those
/// values will break existing programs which are above the new limits when a
/// re-instrumentation is triggered.
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
#[derive(Clone, Encode, Decode, PartialEq, Eq, RuntimeDebug, TypeInfo)]
pub struct Limits {
    /// The maximum number of topics supported by an event.
    pub event_topics: u32,

    /// Maximum allowed stack height in number of elements.
    ///
    /// See <https://wiki.parity.io/WebAssembly-StackHeight> to find out
    /// how the stack frame cost is calculated. Each element can be of one of the
    /// wasm value types. This means the maximum size per element is 64bit.
    pub stack_height: u32,

    /// Maximum number of globals a module is allowed to declare.
    ///
    /// Globals are not limited through the `stack_height` as locals are. Neither does
    /// the linear memory limit `memory_pages` applies to them.
    pub globals: u32,

    /// Maximum numbers of parameters a function can have.
    ///
    /// Those need to be limited to prevent a potentially exploitable interaction with
    /// the stack height instrumentation: The costs of executing the stack height
    /// instrumentation for an indirectly called function scales linearly with the amount
    /// of parameters of this function. Because the stack height instrumentation itself is
    /// is not weight metered its costs must be static (via this limit) and included in
    /// the costs of the instructions that cause them (call, call_indirect).
    pub parameters: u32,

    /// Maximum number of memory pages allowed for a program.
    pub memory_pages: u32,

    /// Maximum number of elements allowed in a table.
    ///
    /// Currently, the only type of element that is allowed in a table is funcref.
    pub table_size: u32,

    /// Maximum number of elements that can appear as immediate value to the br_table instruction.
    pub br_table_size: u32,

    /// The maximum length of a subject in bytes used for PRNG generation.
    pub subject_len: u32,

    /// The maximum nesting level of the call stack.
    pub call_depth: u32,

    /// The maximum size of a message payload in bytes.
    pub payload_len: u32,

    /// The maximum length of a program code in bytes. This limit applies to the instrumented
    /// version of the code. Therefore `instantiate_with_code` can fail even when supplying
    /// a wasm binary below this maximum size.
    pub code_len: u32,
}

impl Limits {
    /// The maximum memory size in bytes that a program can occupy.
    pub fn max_memory_size(&self) -> u32 {
        self.memory_pages * 64 * 1024
    }
}

/// Describes the weight for all categories of supported wasm instructions.
///
/// There there is one field for each wasm instruction that describes the weight to
/// execute one instruction of that name. There are a few exceptions:
///
/// 1. If there is a i64 and a i32 variant of an instruction we use the weight
///    of the former for both.
/// 2. The following instructions are free of charge because they merely structure the
///    wasm module and cannot be spammed without making the module invalid (and rejected):
///    End, Unreachable, Return, Else
/// 3. The following instructions cannot be benchmarked because they are removed by any
///    real world execution engine as a preprocessing step and therefore don't yield a
///    meaningful benchmark result. However, in contrast to the instructions mentioned
///    in 2. they can be spammed. We price them with the same weight as the "default"
///    instruction (i64.const): Block, Loop, Nop
/// 4. We price both i64.const and drop as InstructionWeights.i64const / 2. The reason
///    for that is that we cannot benchmark either of them on its own but we need their
///    individual values to derive (by subtraction) the weight of all other instructions
///    that use them as supporting instructions. Supporting means mainly pushing arguments
///    and dropping return values in order to maintain a valid module.
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
#[derive(Clone, Encode, Decode, PartialEq, Eq, WeightDebug, TypeInfo)]
#[scale_info(skip_type_params(T))]
pub struct InstructionWeights<T: Config> {
    /// Version of the instruction weights.
    ///
    /// # Note
    ///
    /// Should be incremented whenever any instruction weight is changed. The
    /// reason is that changes to instruction weights require a re-instrumentation
    /// in order to apply the changes to an already deployed code. The re-instrumentation
    /// is triggered by comparing the version of the current schedule with the version the code was
    /// instrumented with. Changes usually happen when pallet_gear is re-benchmarked.
    ///
    /// Changes to other parts of the schedule should not increment the version in
    /// order to avoid unnecessary re-instrumentations.
    pub version: u32,
    pub i64const: u32,
    pub i64load: u32,
    pub i64store: u32,
    pub select: u32,
    pub r#if: u32,
    pub br: u32,
    pub br_if: u32,
    pub br_table: u32,
    pub br_table_per_entry: u32,
    pub call: u32,
    pub call_indirect: u32,
    pub call_indirect_per_param: u32,
    pub local_get: u32,
    pub local_set: u32,
    pub local_tee: u32,
    pub global_get: u32,
    pub global_set: u32,
    pub memory_current: u32,
    pub i64clz: u32,
    pub i64ctz: u32,
    pub i64popcnt: u32,
    pub i64eqz: u32,
    pub i64extendsi32: u32,
    pub i64extendui32: u32,
    pub i32wrapi64: u32,
    pub i64eq: u32,
    pub i64ne: u32,
    pub i64lts: u32,
    pub i64ltu: u32,
    pub i64gts: u32,
    pub i64gtu: u32,
    pub i64les: u32,
    pub i64leu: u32,
    pub i64ges: u32,
    pub i64geu: u32,
    pub i64add: u32,
    pub i64sub: u32,
    pub i64mul: u32,
    pub i64divs: u32,
    pub i64divu: u32,
    pub i64rems: u32,
    pub i64remu: u32,
    pub i64and: u32,
    pub i64or: u32,
    pub i64xor: u32,
    pub i64shl: u32,
    pub i64shrs: u32,
    pub i64shru: u32,
    pub i64rotl: u32,
    pub i64rotr: u32,
    /// The type parameter is used in the default implementation.
    #[codec(skip)]
    pub _phantom: PhantomData<T>,
}

/// Describes the weight for each imported function that a program is allowed to call.
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
#[derive(Clone, Encode, Decode, PartialEq, Eq, WeightDebug, TypeInfo)]
#[scale_info(skip_type_params(T))]
pub struct HostFnWeights<T: Config> {
    /// Weight of calling `alloc`.
    pub alloc: u64,

    /// Weight of calling `gr_gas_available`.
    pub gr_gas_available: u64,

    /// Weight of calling `gr_message_id`.
    pub gr_message_id: u64,

    /// Weight of calling `gr_origin`.
    pub gr_origin: u64,

    /// Weight of calling `gr_program_id`.
    pub gr_program_id: u64,

    /// Weight of calling `gr_source`.
    pub gr_source: u64,

    /// Weight of calling `gr_value`.
    pub gr_value: u64,

    /// Weight of calling `gr_value_available`.
    pub gr_value_available: u64,

    /// Weight of calling `gr_size`.
    pub gr_size: u64,

    /// Weight of calling `gr_read`.
    pub gr_read: u64,

    /// Weight per payload byte by `gr_read`.
    pub gr_read_per_byte: u64,

    /// Weight of calling `gr_block_height`.
    pub gr_block_height: u64,

    /// Weight of calling `gr_block_timestamp`.
    pub gr_block_timestamp: u64,

    /// Weight of calling `gr_value_available`.
    pub gr_send_init: u64,

    /// Weight of calling `gr_send_push`.
    pub gr_send_push: u64,

    /// Weight per payload byte by `gr_send_push`.
    pub gr_send_push_per_byte: u64,

    /// Weight of calling `gr_send_commit`.
    pub gr_send_commit: u64,

    /// Weight per payload byte by `gr_send_commit`.
    pub gr_send_commit_per_byte: u64,

    /// Weight of calling `gr_reply_commit`.
    pub gr_reply_commit: u64,

    /// Weight per payload byte by `gr_reply_commit`.
    pub gr_reply_commit_per_byte: u64,

    /// Weight of calling `gr_reply_push`.
    pub gr_reply_push: u64,

    /// Weight per payload byte by `gr_reply_push`.
    pub gr_reply_push_per_byte: u64,

    /// Weight of calling `gr_reply_to`.
    pub gr_reply_to: u64,

    /// Weight of calling `gr_debug`.
    pub gr_debug: u64,

    /// Weight of calling `gr_exit_code`.
    pub gr_exit_code: u64,

    /// Weight of calling `gr_exit`.
    pub gr_exit: u64,

    /// Weight of calling `gr_leave`.
    pub gr_leave: u64,

    /// Weight of calling `gr_wait`.
    pub gr_wait: u64,

    /// Weight of calling `gr_wait_for`.
    pub gr_wait_for: u64,

    /// Weight of calling `gr_wait_up_to`.
    pub gr_wait_up_to: u64,

    /// Weight of calling `gr_wake`.
    pub gr_wake: u64,

    /// Weight of calling `create_program_wgas`.
    pub gr_create_program_wgas: u64,

    /// Weight per payload byte by `create_program_wgas`.
    pub gr_create_program_wgas_per_byte: u64,

    /// Weight of calling `gas`.
    pub gas: u64,

    /// The type parameter is used in the default implementation.
    #[codec(skip)]
    pub _phantom: PhantomData<T>,
}

/// Describes the weight for memory interaction.
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
#[derive(Clone, Encode, Decode, PartialEq, Eq, WeightDebug, TypeInfo)]
#[scale_info(skip_type_params(T))]
pub struct MemoryWeights<T: Config> {
    /// Weight of initial page.
    pub initial_cost: u64,

    /// Weight of allocated page.
    pub allocation_cost: u64,

    /// Weight of growing page.
    pub grow_cost: u64,

    /// Weight of loading page.
    pub load_cost: u64,

    /// The type parameter is used in the default implementation.
    #[codec(skip)]
    pub _phantom: PhantomData<T>,
}

macro_rules! replace_token {
    ($_in:tt $replacement:tt) => {
        $replacement
    };
}

macro_rules! call_zero {
	($name:ident, $( $arg:expr ),*) => {
		<T as Config>::WeightInfo::$name($( replace_token!($arg 0) ),*)
	};
}

macro_rules! cost_args {
	($name:ident, $( $arg: expr ),+) => {
		(<T as Config>::WeightInfo::$name($( $arg ),+).saturating_sub(call_zero!($name, $( $arg ),+))).ref_time()
	}
}

macro_rules! cost_batched_args {
	($name:ident, $( $arg: expr ),+) => {
		cost_args!($name, $( $arg ),+) / u64::from(API_BENCHMARK_BATCH_SIZE)
	}
}

macro_rules! cost_instr_no_params_with_batch_size {
    ($name:ident, $batch_size:expr) => {
        (cost_args!($name, 1) / u64::from($batch_size)) as u32
    };
}

macro_rules! cost_instr_with_batch_size {
    ($name:ident, $num_params:expr, $batch_size:expr) => {
        cost_instr_no_params_with_batch_size!($name, $batch_size).saturating_sub(
            (cost_instr_no_params_with_batch_size!(instr_i64const, $batch_size) / 2)
                .saturating_mul($num_params),
        )
    };
}

macro_rules! cost_instr {
    ($name:ident, $num_params:expr) => {
        cost_instr_with_batch_size!($name, $num_params, INSTR_BENCHMARK_BATCH_SIZE)
    };
}

macro_rules! cost_byte_args {
	($name:ident, $( $arg: expr ),+) => {
		cost_args!($name, $( $arg ),+) / 1024
	}
}

macro_rules! cost_byte_batched_args {
	($name:ident, $( $arg: expr ),+) => {
		cost_batched_args!($name, $( $arg ),+) / 1024
	}
}

macro_rules! cost {
    ($name:ident) => {
        cost_args!($name, 1)
    };
}

macro_rules! cost_batched {
    ($name:ident) => {
        cost_batched_args!($name, 1)
    };
}

macro_rules! cost_byte {
    ($name:ident) => {
        cost_byte_args!($name, 1)
    };
}

macro_rules! cost_byte_batched {
    ($name:ident) => {
        cost_byte_batched_args!($name, 1)
    };
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            event_topics: 4,
            // 512 * sizeof(i64) will give us a 4k stack.
            stack_height: 512,
            globals: 256,
            parameters: 128,
            memory_pages: code::MAX_WASM_PAGE_COUNT,
            // 4k function pointers (This is in count not bytes).
            table_size: 4096,
            br_table_size: 256,
            subject_len: 32,
            call_depth: 32,
            payload_len: 64 * 1024,
            code_len: 512 * 1024,
        }
    }
}

impl<T: Config> Default for InstructionWeights<T> {
    fn default() -> Self {
        Self {
            version: 4,
            i64const: cost_instr!(instr_i64const, 1),
            i64load: cost_instr!(instr_i64load, 2),
            i64store: cost_instr!(instr_i64store, 2),
            select: cost_instr!(instr_select, 4),
            r#if: cost_instr!(instr_if, 3),
            br: cost_instr!(instr_br, 2),
            br_if: cost_instr!(instr_br_if, 3),
            br_table: cost_instr!(instr_br_table, 3),
            br_table_per_entry: cost_instr!(instr_br_table_per_entry, 0),
            call: cost_instr!(instr_call, 2),
            call_indirect: cost_instr!(instr_call_indirect, 3),
            call_indirect_per_param: cost_instr!(instr_call_indirect_per_param, 1),
            local_get: cost_instr!(instr_local_get, 1),
            local_set: cost_instr!(instr_local_set, 1),
            local_tee: cost_instr!(instr_local_tee, 2),
            global_get: cost_instr!(instr_global_get, 1),
            global_set: cost_instr!(instr_global_set, 1),
            memory_current: cost_instr!(instr_memory_current, 1),
            i64clz: cost_instr!(instr_i64clz, 2),
            i64ctz: cost_instr!(instr_i64ctz, 2),
            i64popcnt: cost_instr!(instr_i64popcnt, 2),
            i64eqz: cost_instr!(instr_i64eqz, 2),
            i64extendsi32: cost_instr!(instr_i64extendsi32, 2),
            i64extendui32: cost_instr!(instr_i64extendui32, 2),
            i32wrapi64: cost_instr!(instr_i32wrapi64, 2),
            i64eq: cost_instr!(instr_i64eq, 3),
            i64ne: cost_instr!(instr_i64ne, 3),
            i64lts: cost_instr!(instr_i64lts, 3),
            i64ltu: cost_instr!(instr_i64ltu, 3),
            i64gts: cost_instr!(instr_i64gts, 3),
            i64gtu: cost_instr!(instr_i64gtu, 3),
            i64les: cost_instr!(instr_i64les, 3),
            i64leu: cost_instr!(instr_i64leu, 3),
            i64ges: cost_instr!(instr_i64ges, 3),
            i64geu: cost_instr!(instr_i64geu, 3),
            i64add: cost_instr!(instr_i64add, 3),
            i64sub: cost_instr!(instr_i64sub, 3),
            i64mul: cost_instr!(instr_i64mul, 3),
            i64divs: cost_instr!(instr_i64divs, 3),
            i64divu: cost_instr!(instr_i64divu, 3),
            i64rems: cost_instr!(instr_i64rems, 3),
            i64remu: cost_instr!(instr_i64remu, 3),
            i64and: cost_instr!(instr_i64and, 3),
            i64or: cost_instr!(instr_i64or, 3),
            i64xor: cost_instr!(instr_i64xor, 3),
            i64shl: cost_instr!(instr_i64shl, 3),
            i64shrs: cost_instr!(instr_i64shrs, 3),
            i64shru: cost_instr!(instr_i64shru, 3),
            i64rotl: cost_instr!(instr_i64rotl, 3),
            i64rotr: cost_instr!(instr_i64rotr, 3),
            _phantom: PhantomData,
        }
    }
}

impl<T: Config> HostFnWeights<T> {
    pub fn into_core(self) -> CoreHostFnWeights {
        CoreHostFnWeights {
            alloc: self.alloc,
            gr_gas_available: self.gr_gas_available,
            gr_message_id: self.gr_message_id,
            gr_origin: self.gr_origin,
            gr_program_id: self.gr_program_id,
            gr_source: self.gr_source,
            gr_value: self.gr_value,
            gr_value_available: self.gr_value_available,
            gr_size: self.gr_size,
            gr_read: self.gr_read,
            gr_read_per_byte: self.gr_read_per_byte,
            gr_block_height: self.gr_block_height,
            gr_block_timestamp: self.gr_block_timestamp,
            gr_send_init: self.gr_send_init,
            gr_send_push: self.gr_send_push,
            gr_send_push_per_byte: self.gr_send_push_per_byte,
            gr_send_commit: self.gr_send_commit,
            gr_send_commit_per_byte: self.gr_send_commit_per_byte,
            gr_reply_commit: self.gr_reply_commit,
            gr_reply_commit_per_byte: self.gr_reply_commit_per_byte,
            gr_reply_push: self.gr_reply_push,
            gr_reply_push_per_byte: self.gr_reply_push_per_byte,
            gr_debug: self.gr_debug,
            gr_reply_to: self.gr_reply_to,
            gr_exit_code: self.gr_exit_code,
            gr_exit: self.gr_exit,
            gr_leave: self.gr_leave,
            gr_wait: self.gr_wait,
            gr_wait_for: self.gr_wait_for,
            gr_wait_up_to: self.gr_wait_up_to,
            gr_wake: self.gr_wake,
            gr_create_program_wgas: self.gr_create_program_wgas,
            gr_create_program_wgas_per_byte: self.gr_create_program_wgas_per_byte,
            gas: self.gas,
        }
    }
}

impl<T: Config> Default for HostFnWeights<T> {
    fn default() -> Self {
        Self {
            alloc: cost_batched!(alloc),
            gr_gas_available: cost_batched!(gr_gas_available),
            gr_message_id: cost_batched!(gr_message_id),
            gr_origin: cost_batched!(gr_origin),
            gr_program_id: cost_batched!(gr_program_id),
            gr_source: cost_batched!(gr_source),
            gr_value: cost_batched!(gr_value),
            gr_value_available: cost_batched!(gr_value_available),
            gr_size: cost_batched!(gr_size),
            gr_read: cost_batched!(gr_read),
            gr_read_per_byte: cost_byte_batched!(gr_read_per_kb),
            gr_block_height: cost_batched!(gr_block_height),
            gr_block_timestamp: cost_batched!(gr_block_timestamp),
            gr_send_init: cost_batched!(gr_send_init),
            gr_send_push: cost_batched!(gr_send_push),
            gr_send_push_per_byte: cost_byte_batched!(gr_send_push_per_kb),
            gr_send_commit: cost_batched!(gr_send_commit),
            gr_send_commit_per_byte: cost_byte!(gr_send_commit_per_kb),
            gr_reply_commit: cost_batched!(gr_reply_commit),
            gr_reply_commit_per_byte: cost_byte_batched!(gr_reply_commit_per_kb),
            gr_reply_push: cost_batched!(gr_reply_push),
            gr_reply_push_per_byte: cost_byte_batched!(gr_reply_push_per_kb),
            gr_debug: cost_batched!(gr_debug),
            gr_reply_to: cost_batched!(gr_reply_to),
            gr_exit_code: cost_batched!(gr_exit_code),
            gr_exit: cost!(gr_exit),
            gr_leave: cost!(gr_leave),
            gr_wait: cost!(gr_wait),
            gr_wait_for: cost!(gr_wait_for),
            gr_wait_up_to: cost!(gr_wait_up_to),
            gr_wake: cost_batched!(gr_wake),
            gr_create_program_wgas: cost!(gr_create_program_wgas),
            gr_create_program_wgas_per_byte: cost_byte_batched!(gr_create_program_wgas_per_kb),
            gas: cost_batched!(gas),
            _phantom: PhantomData,
        }
    }
}

impl<T: Config> Default for MemoryWeights<T> {
    fn default() -> Self {
        Self {
            initial_cost: <T as Config>::WeightInfo::initial_cost().ref_time(),
            allocation_cost: <T as Config>::WeightInfo::allocation_cost().ref_time(),
            grow_cost: <T as Config>::WeightInfo::grow_cost().ref_time(),
            load_cost: <T as Config>::WeightInfo::load_cost().ref_time(),
            _phantom: PhantomData,
        }
    }
}

struct ScheduleRules<'a, T: Config> {
    schedule: &'a Schedule<T>,
    params: Vec<u32>,
}

impl<T: Config> Schedule<T> {
    pub fn rules(&self, module: &elements::Module) -> impl gas_metering::Rules + '_ {
        ScheduleRules {
            schedule: self,
            params: module
                .type_section()
                .iter()
                .flat_map(|section| section.types())
                .map(|func| {
                    let elements::Type::Function(func) = func;
                    func.params().len() as u32
                })
                .collect(),
        }
    }
}

impl<'a, T: Config> gas_metering::Rules for ScheduleRules<'a, T> {
    fn instruction_cost(&self, instruction: &elements::Instruction) -> Option<u32> {
        use self::elements::Instruction::*;
        let w = &self.schedule.instruction_weights;
        let max_params = self.schedule.limits.parameters;

        let weight = match *instruction {
            End | Unreachable | Return | Else => 0,
            I32Const(_) | I64Const(_) | Block(_) | Loop(_) | Nop | Drop => w.i64const,
            I32Load(_, _)
            | I32Load8S(_, _)
            | I32Load8U(_, _)
            | I32Load16S(_, _)
            | I32Load16U(_, _)
            | I64Load(_, _)
            | I64Load8S(_, _)
            | I64Load8U(_, _)
            | I64Load16S(_, _)
            | I64Load16U(_, _)
            | I64Load32S(_, _)
            | I64Load32U(_, _) => w.i64load,
            I32Store(_, _)
            | I32Store8(_, _)
            | I32Store16(_, _)
            | I64Store(_, _)
            | I64Store8(_, _)
            | I64Store16(_, _)
            | I64Store32(_, _) => w.i64store,
            Select => w.select,
            If(_) => w.r#if,
            Br(_) => w.br,
            BrIf(_) => w.br_if,
            Call(_) => w.call,
            GetLocal(_) => w.local_get,
            SetLocal(_) => w.local_set,
            TeeLocal(_) => w.local_tee,
            GetGlobal(_) => w.global_get,
            SetGlobal(_) => w.global_set,
            CurrentMemory(_) => w.memory_current,
            CallIndirect(idx, _) => *self.params.get(idx as usize).unwrap_or(&max_params),
            BrTable(ref data) => w
                .br_table
                .saturating_add(w.br_table_per_entry.saturating_mul(data.table.len() as u32)),
            I32Clz | I64Clz => w.i64clz,
            I32Ctz | I64Ctz => w.i64ctz,
            I32Popcnt | I64Popcnt => w.i64popcnt,
            I32Eqz | I64Eqz => w.i64eqz,
            I64ExtendSI32 => w.i64extendsi32,
            I64ExtendUI32 => w.i64extendui32,
            I32WrapI64 => w.i32wrapi64,
            I32Eq | I64Eq => w.i64eq,
            I32Ne | I64Ne => w.i64ne,
            I32LtS | I64LtS => w.i64lts,
            I32LtU | I64LtU => w.i64ltu,
            I32GtS | I64GtS => w.i64gts,
            I32GtU | I64GtU => w.i64gtu,
            I32LeS | I64LeS => w.i64les,
            I32LeU | I64LeU => w.i64leu,
            I32GeS | I64GeS => w.i64ges,
            I32GeU | I64GeU => w.i64geu,
            I32Add | I64Add => w.i64add,
            I32Sub | I64Sub => w.i64sub,
            I32Mul | I64Mul => w.i64mul,
            I32DivS | I64DivS => w.i64divs,
            I32DivU | I64DivU => w.i64divu,
            I32RemS | I64RemS => w.i64rems,
            I32RemU | I64RemU => w.i64remu,
            I32And | I64And => w.i64and,
            I32Or | I64Or => w.i64or,
            I32Xor | I64Xor => w.i64xor,
            I32Shl | I64Shl => w.i64shl,
            I32ShrS | I64ShrS => w.i64shrs,
            I32ShrU | I64ShrU => w.i64shru,
            I32Rotl | I64Rotl => w.i64rotl,
            I32Rotr | I64Rotr => w.i64rotr,

            // Returning None makes the gas instrumentation fail which we intend for
            // unsupported or unknown instructions.
            _ => return None,
        };
        Some(weight)
    }

    fn memory_grow_cost(&self) -> gas_metering::MemoryGrowCost {
        gas_metering::MemoryGrowCost::Free
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::mock::Test;
    use gas_metering::Rules;

    fn all_measured_instructions() -> Vec<elements::Instruction> {
        use elements::{BlockType, BrTableData, Instruction::*};
        let default_table_data = BrTableData {
            table: Default::default(),
            default: 0,
        };

        // A set of instructions weights for which the Gear provides.
        // Instruction must not be removed (!), but can be added.
        vec![
            End,
            Unreachable,
            Return,
            Else,
            I32Const(0),
            I64Const(0),
            Block(BlockType::NoResult),
            Loop(BlockType::NoResult),
            Nop,
            Drop,
            I32Load(0, 0),
            I32Load8S(0, 0),
            I32Load8U(0, 0),
            I32Load16S(0, 0),
            I32Load16U(0, 0),
            I64Load(0, 0),
            I64Load8S(0, 0),
            I64Load8U(0, 0),
            I64Load16S(0, 0),
            I64Load16U(0, 0),
            I64Load32S(0, 0),
            I64Load32U(0, 0),
            I32Store(0, 0),
            I32Store8(0, 0),
            I32Store16(0, 0),
            I64Store(0, 0),
            I64Store8(0, 0),
            I64Store16(0, 0),
            I64Store32(0, 0),
            Select,
            If(BlockType::NoResult),
            Br(0),
            BrIf(0),
            Call(0),
            GetLocal(0),
            SetLocal(0),
            TeeLocal(0),
            GetGlobal(0),
            SetGlobal(0),
            CurrentMemory(0),
            CallIndirect(0, 0),
            BrTable(default_table_data.into()),
            I32Clz,
            I64Clz,
            I32Ctz,
            I64Ctz,
            I32Popcnt,
            I64Popcnt,
            I32Eqz,
            I64Eqz,
            I64ExtendSI32,
            I64ExtendUI32,
            I32WrapI64,
            I32Eq,
            I64Eq,
            I32Ne,
            I64Ne,
            I32LtS,
            I64LtS,
            I32LtU,
            I64LtU,
            I32GtS,
            I64GtS,
            I32GtU,
            I64GtU,
            I32LeS,
            I64LeS,
            I32LeU,
            I64LeU,
            I32GeS,
            I64GeS,
            I32GeU,
            I64GeU,
            I32Add,
            I64Add,
            I32Sub,
            I64Sub,
            I32Mul,
            I64Mul,
            I32DivS,
            I64DivS,
            I32DivU,
            I64DivU,
            I32RemS,
            I64RemS,
            I32RemU,
            I64RemU,
            I32And,
            I64And,
            I32Or,
            I64Or,
            I32Xor,
            I64Xor,
            I32Shl,
            I64Shl,
            I32ShrS,
            I64ShrS,
            I32ShrU,
            I64ShrU,
            I32Rotl,
            I64Rotl,
            I32Rotr,
            I64Rotr,
        ]
    }

    fn default_wasm_module() -> elements::Module {
        let simple_wat = r#"
        (module
            (import "env" "memory" (memory 1))
            (export "handle" (func $handle))
            (export "init" (func $init))
            (func $handle)
            (func $init)
        )"#;
        elements::Module::from_bytes(
            wabt::Wat2Wasm::new()
                .validate(false)
                .convert(simple_wat)
                .expect("failed to parse module"),
        )
        .expect("module instantiation failed")
    }

    // This test must never fail during local development/release.
    //
    // The instruction set in the test mustn't be changed. Test checks
    // whether no instruction weight was removed from Rules, so backward
    // compatibility is reached.
    #[test]
    fn instructions_backward_compatibility() {
        let schedule = Schedule::<Test>::default();
        let rules = schedule.rules(&default_wasm_module());
        all_measured_instructions()
            .iter()
            .for_each(|i| assert!(rules.instruction_cost(i).is_some()))
    }
}
