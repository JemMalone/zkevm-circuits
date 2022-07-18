use super::Opcode;
use crate::{
    circuit_input_builder::{CircuitInputStateRef, ExecStep},
    operation::{AccountField, CallContextField, TxAccessListAccountOp, RW},
    Error,
};
use eth_types::{
    evm_types::{
        gas_utils::{eip150_gas, memory_expansion_gas_cost},
        GasCost, OpcodeId,
    },
    GethExecStep, ToWord,
};
use keccak256::EMPTY_HASH;
use log::warn;
use std::cmp::max;

/// Placeholder structure used to implement [`Opcode`] trait over it
/// corresponding to the `OpcodeId::CALL` `OpcodeId`.
#[derive(Debug, Copy, Clone)]
pub(crate) struct Call;

pub fn call_reconstruct_memory(
    state: &mut CircuitInputStateRef,
    geth_steps: &[GethExecStep],
) -> Result<usize, Error> {
    let geth_step = &geth_steps[0];

    let [args_offset, args_length, ret_offset, ret_length]: [usize; 4] = match geth_step.op {
        OpcodeId::DELEGATECALL | OpcodeId::STATICCALL => {
            vec![2, 3, 4, 5]
        }
        OpcodeId::CALL | OpcodeId::CALLCODE => {
            vec![3, 4, 5, 6]
        }
        _ => {
            unreachable!()
        }
    }
    .into_iter()
    .map(|i| -> Result<usize, Error> { Ok(geth_step.stack.nth_last(i)?.as_usize()) })
    .collect::<Result<Vec<usize>, Error>>()?
    .try_into()
    .unwrap();

    let args_minimal = if args_length != 0 {
        args_offset + args_length
    } else {
        0
    };
    let ret_minimal = if ret_length != 0 {
        ret_offset + ret_length
    } else {
        0
    };
    let minimal_length = max(args_minimal, ret_minimal);
    Ok(minimal_length)
}
impl Opcode for Call {
    fn gen_associated_ops(
        &self,
        state: &mut CircuitInputStateRef,
        geth_steps: &[GethExecStep],
    ) -> Result<Vec<ExecStep>, Error> {
        let minimal_length = call_reconstruct_memory(state, geth_steps)?;
        // we need to keep the memory until parse_call complete
        let call_ctx = state.call_ctx_mut()?;
        call_ctx.memory.extend_at_least(minimal_length);

        let geth_step = &geth_steps[0];
        let mut exec_step = state.new_step(geth_step)?;

        let tx_id = state.tx_ctx.id();
        let call = state.parse_call(geth_step)?;
        let current_call = state.call()?.clone();

        // NOTE: For `RwCounterEndOfReversion` we use the `0` value as a placeholder,
        // and later set the proper value in
        // `CircuitInputBuilder::set_value_ops_call_context_rwc_eor`
        for (field, value) in [
            (CallContextField::TxId, tx_id.into()),
            (CallContextField::RwCounterEndOfReversion, 0.into()),
            (
                CallContextField::IsPersistent,
                (current_call.is_persistent as u64).into(),
            ),
            (
                CallContextField::CalleeAddress,
                current_call.address.to_word(),
            ),
            (
                CallContextField::IsStatic,
                (current_call.is_static as u64).into(),
            ),
            (CallContextField::Depth, current_call.depth.into()),
        ] {
            state.call_context_read(&mut exec_step, current_call.call_id, field, value);
        }

        for i in 0..7 {
            state.stack_read(
                &mut exec_step,
                geth_step.stack.nth_last_filled(i),
                geth_step.stack.nth_last(i)?,
            )?;
        }

        state.stack_write(
            &mut exec_step,
            geth_step.stack.nth_last_filled(6),
            (call.is_success as u64).into(),
        )?;

        let is_warm = state.sdb.check_account_in_access_list(&call.address);
        state.push_op_reversible(
            &mut exec_step,
            RW::WRITE,
            TxAccessListAccountOp {
                tx_id,
                address: call.address,
                is_warm: true,
                is_warm_prev: is_warm,
            },
        )?;

        let curr_memory_word_size = state.call_ctx()?.memory.word_size() as u64;

        // Switch to callee's call context
        state.push_call(call.clone());

        for (field, value) in [
            (CallContextField::RwCounterEndOfReversion, 0.into()),
            (
                CallContextField::IsPersistent,
                (call.is_persistent as u64).into(),
            ),
        ] {
            state.call_context_read(&mut exec_step, call.call_id, field, value);
        }

        state.transfer(
            &mut exec_step,
            call.caller_address,
            call.address,
            call.value,
        )?;
        let (_, callee_account) = state.sdb.get_account(&call.address);
        let is_account_empty = callee_account.is_empty();
        let callee_nonce = callee_account.nonce;
        let callee_code_hash = callee_account.code_hash;
        for (field, value) in [
            (AccountField::Nonce, callee_nonce),
            (AccountField::CodeHash, callee_code_hash.to_word()),
        ] {
            state.account_read(&mut exec_step, call.address, field, value, value)?;
        }

        // Calculate next_memory_word_size and callee_gas_left manually in case
        // there isn't next geth_step (e.g. callee doesn't have code).
        let next_memory_word_size = [
            curr_memory_word_size,
            (call.call_data_offset + call.call_data_length + 31) / 32,
            (call.return_data_offset + call.return_data_length + 31) / 32,
        ]
        .into_iter()
        .max()
        .unwrap();

        let has_value = !call.value.is_zero();

        let memory_expansion_gas_cost =
            memory_expansion_gas_cost(curr_memory_word_size, next_memory_word_size);
        let gas_cost = if is_warm {
            GasCost::WARM_ACCESS.as_u64()
        } else {
            GasCost::COLD_ACCOUNT_ACCESS.as_u64()
        } + if has_value {
            GasCost::CALL_WITH_VALUE.as_u64()
                + if is_account_empty {
                    GasCost::NEW_ACCOUNT.as_u64()
                } else {
                    0
                }
        } else {
            0
        } + memory_expansion_gas_cost;
        let gas_specified = geth_step.stack.last()?;
        let callee_gas_left = eip150_gas(geth_step.gas.0 - gas_cost, gas_specified);

        if geth_steps[1].depth == geth_steps[0].depth + 1 {
            if geth_steps[1].gas.0 != callee_gas_left + if has_value { 2300 } else { 0 } {
                // panic with full info

                let info1 = format!("callee_gas_left {} gas_specified {} gas_cost {} is_warm {} has_value {} is_account_empty {} current_memory_word_size {} next_memory_word_size {}, memory_expansion_gas_cost {}",
                        callee_gas_left, gas_specified, gas_cost, is_warm, has_value, is_account_empty, curr_memory_word_size, next_memory_word_size, memory_expansion_gas_cost);
                let info2 = format!("args gas:{:?} addr:{:?} value:{:?} cd_pos:{:?} cd_len:{:?} rd_pos:{:?} rd_len:{:?}",
                            geth_step.stack.nth_last(0),
                            geth_step.stack.nth_last(1),
                            geth_step.stack.nth_last(2),
                            geth_step.stack.nth_last(3),
                            geth_step.stack.nth_last(4),
                            geth_step.stack.nth_last(5),
                            geth_step.stack.nth_last(6)
                        );
                let full_ctx = format!(
                    "step0 {:?} step1 {:?} call {:?}, {} {}",
                    geth_steps[0], geth_steps[1], call, info1, info2
                );
                debug_assert_eq!(
                    geth_steps[1].gas.0,
                    callee_gas_left + if has_value { 2300 } else { 0 },
                    "{}",
                    full_ctx
                );
            }
        }

        // There are 3 branches from here.
        match (
            state.is_precompiled(&call.address),
            callee_code_hash.to_fixed_bytes() == *EMPTY_HASH,
        ) {
            // 1. Call to precompiled.
            (true, _) => {
                warn!("Call to precompiled is left unimplemented");
                state.handle_return(geth_step)?;
                Ok(vec![exec_step])
            }
            // 2. Call to account with empty code.
            (_, true) => {
                log::warn!("Call to account with empty code is not supported yet.");
                for (field, value) in [
                    (CallContextField::LastCalleeId, 0.into()),
                    (CallContextField::LastCalleeReturnDataOffset, 0.into()),
                    (CallContextField::LastCalleeReturnDataLength, 0.into()),
                ] {
                    state.call_context_write(&mut exec_step, current_call.call_id, field, value);
                }
                state.handle_return(geth_step)?;
                Ok(vec![exec_step])
            }
            // 3. Call to account with non-empty code.
            (_, false) => {
                for (field, value) in [
                    (
                        CallContextField::ProgramCounter,
                        (geth_step.pc.0 + 1).into(),
                    ),
                    (
                        CallContextField::StackPointer,
                        (geth_step.stack.stack_pointer().0 + 6).into(),
                    ),
                    (
                        CallContextField::GasLeft,
                        (geth_step.gas.0 - gas_cost - callee_gas_left).into(),
                    ),
                    (CallContextField::MemorySize, next_memory_word_size.into()),
                    (
                        CallContextField::ReversibleWriteCounter,
                        (exec_step.reversible_write_counter + 1).into(),
                    ),
                ] {
                    state.call_context_write(&mut exec_step, current_call.call_id, field, value);
                }

                for (field, value) in [
                    (CallContextField::CallerId, current_call.call_id.into()),
                    (CallContextField::TxId, tx_id.into()),
                    (CallContextField::Depth, call.depth.into()),
                    (
                        CallContextField::CallerAddress,
                        call.caller_address.to_word(),
                    ),
                    (CallContextField::CalleeAddress, call.address.to_word()),
                    (
                        CallContextField::CallDataOffset,
                        call.call_data_offset.into(),
                    ),
                    (
                        CallContextField::CallDataLength,
                        call.call_data_length.into(),
                    ),
                    (
                        CallContextField::ReturnDataOffset,
                        call.return_data_offset.into(),
                    ),
                    (
                        CallContextField::ReturnDataLength,
                        call.return_data_length.into(),
                    ),
                    (CallContextField::Value, call.value),
                    (CallContextField::IsSuccess, (call.is_success as u64).into()),
                    (CallContextField::IsStatic, (call.is_static as u64).into()),
                    (CallContextField::LastCalleeId, 0.into()),
                    (CallContextField::LastCalleeReturnDataOffset, 0.into()),
                    (CallContextField::LastCalleeReturnDataLength, 0.into()),
                    (CallContextField::IsRoot, 0.into()),
                    (CallContextField::IsCreate, 0.into()),
                    (CallContextField::CodeHash, call.code_hash.to_word()),
                ] {
                    state.call_context_read(&mut exec_step, call.call_id, field, value);
                }

                Ok(vec![exec_step])
            }
        }
    }
}
