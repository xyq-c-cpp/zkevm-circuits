use crate::{
    evm_circuit::{
        execution::ExecutionGadget,
        param::{N_BYTES_ACCOUNT_ADDRESS, N_BYTES_GAS, N_BYTES_U64, N_BYTES_WORD},
        step::ExecutionState,
        util::{
            common_gadget::TransferWithGasFeeGadget,
            constraint_builder::{
                ConstraintBuilder, ReversionInfo, StepStateTransition,
                Transition::{Delta, To},
            },
            math_gadget::{
                ByteSizeGadget, IsEqualGadget, IsZeroGadget, LtGadget, MulWordByU64Gadget,
                RangeCheckGadget,
            },
            not, CachedRegion, Cell, Word,
        },
        witness::{Block, Call, ExecStep, Transaction},
    },
    table::{AccountFieldTag, CallContextFieldTag, TxFieldTag as TxContextFieldTag},
    util::Expr,
};
use eth_types::{evm_types::GasCost, Field, ToLittleEndian, ToScalar};
use ethers_core::utils::get_contract_address;
use gadgets::util::{expr_from_bytes, or, select};
use halo2_proofs::plonk::Error;
use halo2_proofs::{circuit::Value, plonk::Expression};

#[derive(Clone, Debug)]
pub(crate) struct BeginTxGadget<F> {
    tx_id: Cell<F>,
    tx_nonce: Cell<F>,
    tx_gas: Cell<F>,
    tx_gas_price: Word<F>,
    mul_gas_fee_by_gas: MulWordByU64Gadget<F>,
    tx_caller_address: Cell<F>,
    tx_caller_address_is_zero: IsZeroGadget<F>,
    tx_callee_address: Cell<F>,
    call_callee_address: Cell<F>,
    tx_is_create: Cell<F>,
    tx_value: Word<F>,
    tx_value_is_zero: IsZeroGadget<F>,
    tx_call_data_length: Cell<F>,
    tx_call_data_gas_cost: Cell<F>,
    reversion_info: ReversionInfo<F>,
    intrinsic_gas_cost: Cell<F>,
    sufficient_gas_left: RangeCheckGadget<F, N_BYTES_GAS>,
    transfer_with_gas_fee: TransferWithGasFeeGadget<F>,
    phase2_code_hash: Cell<F>,
    is_empty_code_hash: IsEqualGadget<F>,
    is_zero_code_hash: IsZeroGadget<F>,
    tx_caller_address_bytes: [Cell<F>; N_BYTES_ACCOUNT_ADDRESS],
    tx_callee_address_bytes: [Cell<F>; N_BYTES_ACCOUNT_ADDRESS],
    tx_nonce_bytes: [Cell<F>; N_BYTES_U64],
    tx_nonce_is_zero: IsZeroGadget<F>,
    tx_nonce_lt_128: LtGadget<F, 1>,
    tx_nonce_byte_size: ByteSizeGadget<F>,
}

impl<F: Field> ExecutionGadget<F> for BeginTxGadget<F> {
    const NAME: &'static str = "BeginTx";

    const EXECUTION_STATE: ExecutionState = ExecutionState::BeginTx;

    fn configure(cb: &mut ConstraintBuilder<F>) -> Self {
        // Use rw_counter of the step which triggers next call as its call_id.
        let call_id = cb.curr.state.rw_counter.clone();

        let tx_id = cb.query_cell();
        cb.call_context_lookup(
            1.expr(),
            Some(call_id.expr()),
            CallContextFieldTag::TxId,
            tx_id.expr(),
        );
        let mut reversion_info = cb.reversion_info_write(None);
        cb.call_context_lookup(
            1.expr(),
            Some(call_id.expr()),
            CallContextFieldTag::IsSuccess,
            reversion_info.is_persistent(),
        );

        let [tx_nonce, tx_gas, tx_caller_address, tx_callee_address, tx_is_create, tx_call_data_length, tx_call_data_gas_cost] =
            [
                TxContextFieldTag::Nonce,
                TxContextFieldTag::Gas,
                TxContextFieldTag::CallerAddress,
                TxContextFieldTag::CalleeAddress,
                TxContextFieldTag::IsCreate,
                TxContextFieldTag::CallDataLength,
                TxContextFieldTag::CallDataGasCost,
            ]
            .map(|field_tag| cb.tx_context(tx_id.expr(), field_tag, None));
        let tx_nonce_is_zero = IsZeroGadget::construct(cb, tx_nonce.expr());
        let tx_nonce_lt_128 = LtGadget::construct(cb, tx_nonce.expr(), 128.expr());

        let call_callee_address = cb.query_cell();
        cb.condition(not::expr(tx_is_create.expr()), |cb| {
            cb.require_equal(
                "Tx to non-zero address",
                tx_callee_address.expr(),
                call_callee_address.expr(),
            );
        });

        let tx_caller_address_is_zero = IsZeroGadget::construct(cb, tx_caller_address.expr());
        cb.require_equal(
            "CallerAddress != 0 (not a padding tx)",
            tx_caller_address_is_zero.expr(),
            false.expr(),
        );
        let [tx_gas_price, tx_value] = [TxContextFieldTag::GasPrice, TxContextFieldTag::Value]
            .map(|field_tag| cb.tx_context_as_word(tx_id.expr(), field_tag, None));
        let tx_value_is_zero = IsZeroGadget::construct(cb, tx_value.expr());

        // Add first BeginTx step constraint to have tx_id == 1
        cb.step_first(|cb| {
            cb.require_equal("tx_id is initialized to be 1", tx_id.expr(), 1.expr());
        });

        // Increase caller's nonce.
        // (tx caller's nonce always increases even when tx ends with error)
        cb.account_write(
            tx_caller_address.expr(),
            AccountFieldTag::Nonce,
            tx_nonce.expr() + 1.expr(),
            tx_nonce.expr(),
            None,
        );

        // Prepare access list of caller and callee
        cb.account_access_list_write(
            tx_id.expr(),
            tx_caller_address.expr(),
            1.expr(),
            0.expr(),
            None,
        );
        cb.account_access_list_write(
            tx_id.expr(),
            call_callee_address.expr(),
            1.expr(),
            0.expr(),
            None,
        );

        // TODO: Implement EIP 1559 (currently it only supports legacy
        // transaction format)
        // Calculate transaction gas fee
        let mul_gas_fee_by_gas =
            MulWordByU64Gadget::construct(cb, tx_gas_price.clone(), tx_gas.expr());

        // TODO: Take gas cost of access list (EIP 2930) into consideration.
        // Use intrinsic gas
        // Check gas_left is sufficient
        let intrinsic_gas_cost = cb.query_cell();
        cb.require_equal(
            "calculate intrinsic gas cost",
            intrinsic_gas_cost.expr(),
            select::expr(
                tx_is_create.expr(),
                GasCost::CREATION_TX.expr(),
                GasCost::TX.expr(),
            ) + tx_call_data_gas_cost.expr(),
        );
        let gas_left = tx_gas.expr() - intrinsic_gas_cost.expr();
        let sufficient_gas_left = RangeCheckGadget::construct(cb, gas_left.clone());

        // Read code_hash of callee
        let phase2_code_hash = cb.query_cell_phase2();
        let (tx_caller_address_bytes, tx_callee_address_bytes, tx_nonce_bytes) = (
            array_init::array_init(|_| cb.query_byte()),
            array_init::array_init(|_| cb.query_byte()),
            array_init::array_init(|_| cb.query_byte()),
        );
        let tx_nonce_byte_size = ByteSizeGadget::construct(
            cb,
            tx_nonce_bytes
                .iter()
                .map(Expr::expr)
                .chain(std::iter::repeat(0.expr()).take(N_BYTES_WORD - N_BYTES_U64))
                .collect::<Vec<_>>()
                .try_into()
                .unwrap(),
        );
        cb.require_equal(
            "tx caller address equivalence",
            tx_caller_address.expr(),
            expr_from_bytes(&tx_caller_address_bytes),
        );
        cb.require_equal(
            "tx callee address equivalence",
            tx_callee_address.expr(),
            expr_from_bytes(&tx_callee_address_bytes),
        );
        cb.require_equal(
            "tx nonce equivalence",
            tx_nonce.expr(),
            expr_from_bytes(&tx_nonce_bytes),
        );
        cb.condition(tx_is_create.expr(), |cb| {
            // 1. calculate output_rlc
            // this is simply RLC(tx_callee_address_bytes)
            let tx_callee_exprs: [Expression<F>; N_BYTES_ACCOUNT_ADDRESS] = tx_callee_address_bytes
                .iter()
                .map(Expr::expr)
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();
            let _output_rlc = cb.word_rlc(tx_callee_exprs);

            // 2. calculate input_len:
            // if nonce in [0, 127]: input_len == 23
            // else: input_len == 23 + byte_size(nonce)
            let _input_len = select::expr(
                tx_nonce_lt_128.expr(),
                23.expr(),
                23.expr() + tx_nonce_byte_size.byte_size(),
            );

            // TODO: verify input_rlc:
            // RLP([tx_caller_address, tx_nonce])
            //
            // | prefix | addr-prefix | address  | nonce-prefix | nonce          |
            // |--------|-------------|----------|--------------|----------------|
            // | 1-byte | 1-byte      | 20-bytes | 0 or 1 byte  | n_bytes(nonce) |
            //
            // prefix == 192 + 21 + (1 if nonce < 128 else (1 + byte_size(nonce)))
            // address-prefix == 148
            // address == tx_caller_address_bytes
            // nonce-prefix == if nonce >= 128: 128 + byte_size(nonce)
            // nonce == tx_nonce_bytes

            // TODO: cb.keccak_table_lookup(input_rlc, input_len, output_rlc);

            cb.account_write(
                call_callee_address.expr(),
                AccountFieldTag::CodeHash,
                phase2_code_hash.expr(),
                0.expr(),
                None,
            );
        });
        cb.condition(not::expr(tx_is_create.expr()), |cb| {
            cb.account_read(
                call_callee_address.expr(),
                AccountFieldTag::CodeHash,
                phase2_code_hash.expr(),
            );
        });
        let is_empty_code_hash =
            IsEqualGadget::construct(cb, phase2_code_hash.expr(), cb.empty_hash_rlc());
        let is_zero_code_hash = IsZeroGadget::construct(cb, phase2_code_hash.expr());
        let is_empty_code = or::expr([is_empty_code_hash.expr(), is_zero_code_hash.expr()]);

        cb.condition(not::expr(is_empty_code.expr()), |cb| {
            cb.require_equal(
                "code hash equivalence",
                cb.curr.state.code_hash.expr(),
                phase2_code_hash.expr(),
            );
        });

        // TODO: If value is 0, skip transfer, just like callop.
        // Transfer value from caller to callee
        let transfer_with_gas_fee = TransferWithGasFeeGadget::construct(
            cb,
            tx_caller_address.expr(),
            call_callee_address.expr(),
            tx_value.clone(),
            mul_gas_fee_by_gas.product().clone(),
            &mut reversion_info,
        );

        // TODO: Handle precompiled

        // Handle contract creation transaction
        // tx_is_create = true when tx.to = none
        cb.condition(tx_is_create.expr(), |cb| {
            cb.account_write(
                call_callee_address.expr(),
                AccountFieldTag::Nonce,
                1.expr(),
                0.expr(),
                None,
            );
            for (field_tag, value) in [
                (CallContextFieldTag::Depth, 1.expr()),
                (CallContextFieldTag::CallerAddress, tx_caller_address.expr()),
                (
                    CallContextFieldTag::CalleeAddress,
                    call_callee_address.expr(),
                ),
                (CallContextFieldTag::CallDataOffset, 0.expr()),
                (
                    CallContextFieldTag::CallDataLength,
                    tx_call_data_length.expr(),
                ),
                (CallContextFieldTag::Value, tx_value.expr()),
                (CallContextFieldTag::IsStatic, 0.expr()),
                (CallContextFieldTag::LastCalleeId, 0.expr()),
                (CallContextFieldTag::LastCalleeReturnDataOffset, 0.expr()),
                (CallContextFieldTag::LastCalleeReturnDataLength, 0.expr()),
                (CallContextFieldTag::IsRoot, 1.expr()),
                (CallContextFieldTag::IsCreate, 1.expr()),
                //(CallContextFieldTag::CodeHash, phase2_code_hash.expr()),
                (
                    CallContextFieldTag::CodeHash,
                    cb.curr.state.code_hash.expr(),
                ),
            ] {
                cb.call_context_lookup(true.expr(), Some(call_id.expr()), field_tag, value);
            }

            cb.require_step_state_transition(StepStateTransition {
                rw_counter: Delta(24.expr()),
                call_id: To(call_id.expr()),
                is_root: To(true.expr()),
                is_create: To(tx_is_create.expr()),
                code_hash: To(cb.curr.state.code_hash.expr()),
                gas_left: To(gas_left.clone()),
                reversible_write_counter: To(3.expr()),
                log_id: To(0.expr()),
                ..StepStateTransition::new_context()
            });
        });

        // TODO: we should use "!tx_is_create && is_empty_code && !(1 <= addr <= 9)".
        // check callop.rs
        let native_transfer = not::expr(tx_is_create.expr()) * is_empty_code.expr();
        cb.condition(
            native_transfer.expr() * not::expr(tx_value_is_zero.expr()),
            |cb| {
                cb.account_write(
                    call_callee_address.expr(),
                    AccountFieldTag::CodeHash,
                    cb.empty_hash_rlc(),
                    cb.empty_hash_rlc(),
                    None, // native transfer cannot fail
                );
            },
        );
        cb.condition(native_transfer, |cb| {
            cb.require_equal(
                "Tx to account with empty code should be persistent",
                reversion_info.is_persistent(),
                1.expr(),
            );
            cb.require_equal(
                "Go to EndTx when Tx to account with empty code",
                cb.next.execution_state_selector([ExecutionState::EndTx]),
                1.expr(),
            );

            cb.require_step_state_transition(StepStateTransition {
                // 10 reads and writes:
                //   - Write CallContext TxId
                //   - Write CallContext RwCounterEndOfReversion
                //   - Write CallContext IsPersistent
                //   - Write CallContext IsSuccess
                //   - Write Account Nonce
                //   - Write TxAccessListAccount
                //   - Write TxAccessListAccount
                //   - Write Account Balance
                //   - Write Account Balance
                //   - Read Account CodeHash
                rw_counter: Delta(10.expr() + not::expr(tx_value_is_zero.expr())),
                call_id: To(call_id.expr()),
                ..StepStateTransition::any()
            });
        });

        let normal_contract_call = not::expr(tx_is_create.expr()) * not::expr(is_empty_code.expr());

        cb.condition(normal_contract_call, |cb| {
            // Setup first call's context.
            for (field_tag, value) in [
                (CallContextFieldTag::Depth, 1.expr()),
                (CallContextFieldTag::CallerAddress, tx_caller_address.expr()),
                (
                    CallContextFieldTag::CalleeAddress,
                    call_callee_address.expr(),
                ),
                (CallContextFieldTag::CallDataOffset, 0.expr()),
                (
                    CallContextFieldTag::CallDataLength,
                    tx_call_data_length.expr(),
                ),
                (CallContextFieldTag::Value, tx_value.expr()),
                (CallContextFieldTag::IsStatic, 0.expr()),
                (CallContextFieldTag::LastCalleeId, 0.expr()),
                (CallContextFieldTag::LastCalleeReturnDataOffset, 0.expr()),
                (CallContextFieldTag::LastCalleeReturnDataLength, 0.expr()),
                (CallContextFieldTag::IsRoot, 1.expr()),
                (CallContextFieldTag::IsCreate, tx_is_create.expr()),
                (CallContextFieldTag::CodeHash, phase2_code_hash.expr()),
            ] {
                cb.call_context_lookup(true.expr(), Some(call_id.expr()), field_tag, value);
            }

            cb.require_step_state_transition(StepStateTransition {
                // 23 reads and writes:
                //   - Write CallContext TxId
                //   - Write CallContext RwCounterEndOfReversion
                //   - Write CallContext IsPersistent
                //   - Write CallContext IsSuccess
                //   - Write Account Nonce
                //   - Write TxAccessListAccount
                //   - Write TxAccessListAccount
                //   - Write Account Balance
                //   - Write Account Balance
                //   - Read Account CodeHash
                //   - Write CallContext Depth
                //   - Write CallContext CallerAddress
                //   - Write CallContext CalleeAddress
                //   - Write CallContext CallDataOffset
                //   - Write CallContext CallDataLength
                //   - Write CallContext Value
                //   - Write CallContext IsStatic
                //   - Write CallContext LastCalleeId
                //   - Write CallContext LastCalleeReturnDataOffset
                //   - Write CallContext LastCalleeReturnDataLength
                //   - Write CallContext IsRoot
                //   - Write CallContext IsCreate
                //   - Write CallContext CodeHash
                rw_counter: Delta(23.expr()),
                call_id: To(call_id.expr()),
                is_root: To(true.expr()),
                is_create: To(tx_is_create.expr()),
                code_hash: To(phase2_code_hash.expr()),
                gas_left: To(gas_left),
                reversible_write_counter: To(2.expr()),
                log_id: To(0.expr()),
                ..StepStateTransition::new_context()
            });
        });

        Self {
            tx_id,
            tx_nonce,
            tx_gas,
            tx_gas_price,
            mul_gas_fee_by_gas,
            tx_caller_address,
            tx_caller_address_is_zero,
            tx_callee_address,
            call_callee_address,
            tx_is_create,
            tx_value,
            tx_value_is_zero,
            tx_call_data_length,
            tx_call_data_gas_cost,
            reversion_info,
            sufficient_gas_left,
            transfer_with_gas_fee,
            phase2_code_hash,
            intrinsic_gas_cost,
            is_empty_code_hash,
            is_zero_code_hash,
            // fields to support keccak lookup.
            tx_caller_address_bytes,
            tx_callee_address_bytes,
            tx_nonce_bytes,
            tx_nonce_is_zero,
            tx_nonce_lt_128,
            tx_nonce_byte_size,
        }
    }

    fn assign_exec_step(
        &self,
        region: &mut CachedRegion<'_, '_, F>,
        offset: usize,
        block: &Block<F>,
        tx: &Transaction,
        call: &Call,
        step: &ExecStep,
    ) -> Result<(), Error> {
        let gas_fee = tx.gas_price * tx.gas;

        let [caller_balance_pair, callee_balance_pair] =
            [step.rw_indices[8], step.rw_indices[9]].map(|idx| block.rws[idx].account_value_pair());
        let callee_code_hash = if tx.is_create {
            call.code_hash
        } else {
            block.rws[step.rw_indices[7]].account_value_pair().0
        };

        self.tx_id
            .assign(region, offset, Value::known(F::from(tx.id as u64)))?;
        self.tx_nonce
            .assign(region, offset, Value::known(F::from(tx.nonce)))?;
        self.tx_gas
            .assign(region, offset, Value::known(F::from(tx.gas)))?;
        self.tx_gas_price
            .assign(region, offset, Some(tx.gas_price.to_le_bytes()))?;
        self.tx_value
            .assign(region, offset, Some(tx.value.to_le_bytes()))?;
        self.tx_value_is_zero
            .assign_value(region, offset, region.word_rlc(tx.value))?;
        self.mul_gas_fee_by_gas
            .assign(region, offset, tx.gas_price, tx.gas, gas_fee)?;
        let caller_address = tx
            .caller_address
            .to_scalar()
            .expect("unexpected Address -> Scalar conversion failure");
        self.tx_caller_address
            .assign(region, offset, Value::known(caller_address))?;
        self.tx_caller_address_is_zero
            .assign(region, offset, caller_address)?;
        self.tx_callee_address.assign(
            region,
            offset,
            Value::known(
                tx.callee_address
                    .to_scalar()
                    .expect("unexpected Address -> Scalar conversion failure"),
            ),
        )?;
        self.call_callee_address.assign(
            region,
            offset,
            Value::known(
                if tx.is_create {
                    get_contract_address(tx.caller_address, tx.nonce)
                } else {
                    tx.callee_address
                }
                .to_scalar()
                .expect("unexpected Address -> Scalar conversion failure"),
            ),
        )?;
        self.tx_is_create
            .assign(region, offset, Value::known(F::from(tx.is_create as u64)))?;
        self.tx_call_data_length.assign(
            region,
            offset,
            Value::known(F::from(tx.call_data_length as u64)),
        )?;
        self.tx_call_data_gas_cost.assign(
            region,
            offset,
            Value::known(F::from(tx.call_data_gas_cost)),
        )?;
        self.reversion_info.assign(
            region,
            offset,
            call.rw_counter_end_of_reversion,
            call.is_persistent,
        )?;
        self.intrinsic_gas_cost
            .assign(region, offset, Value::known(F::from(step.gas_cost)))?;
        self.sufficient_gas_left
            .assign(region, offset, F::from(tx.gas - step.gas_cost))?;
        self.transfer_with_gas_fee.assign(
            region,
            offset,
            caller_balance_pair,
            callee_balance_pair,
            tx.value,
            gas_fee,
        )?;
        self.phase2_code_hash
            .assign(region, offset, region.word_rlc(callee_code_hash))?;
        for (c, v) in self
            .tx_caller_address_bytes
            .iter()
            .rev()
            .zip(tx.caller_address.as_bytes().iter())
        {
            c.assign(region, offset, Value::known(F::from(*v as u64)))?;
        }
        for (c, v) in self
            .tx_callee_address_bytes
            .iter()
            .rev()
            .zip(tx.callee_address.as_bytes().iter())
        {
            c.assign(region, offset, Value::known(F::from(*v as u64)))?;
        }
        for (c, v) in self
            .tx_nonce_bytes
            .iter()
            .zip(tx.nonce.to_le_bytes().iter())
        {
            c.assign(region, offset, Value::known(F::from(*v as u64)))?;
        }
        self.tx_nonce_is_zero
            .assign(region, offset, F::from(tx.nonce))?;
        self.tx_nonce_lt_128
            .assign(region, offset, F::from(tx.nonce), F::from(128u64))?;
        self.tx_nonce_byte_size
            .assign(region, offset, tx.nonce.into())?;
        self.is_empty_code_hash.assign_value(
            region,
            offset,
            region.word_rlc(callee_code_hash),
            region.empty_hash_rlc(),
        )?;
        self.is_zero_code_hash
            .assign_value(region, offset, region.word_rlc(callee_code_hash))?;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use crate::evm_circuit::test::{rand_bytes, run_test_circuit_geth_data_default};
    use bus_mapping::evm::OpcodeId;
    use eth_types::{
        self, bytecode, evm_types::GasCost, geth_types::GethData, word, Bytecode, Word,
    };
    use halo2_proofs::halo2curves::bn256::Fr;
    use mock::{eth, gwei, TestContext, MOCK_ACCOUNTS};

    fn gas(call_data: &[u8]) -> Word {
        Word::from(
            GasCost::TX.as_u64()
                + 2 * OpcodeId::PUSH32.constant_gas_cost().as_u64()
                + call_data
                    .iter()
                    .map(|&x| if x == 0 { 4 } else { 16 })
                    .sum::<u64>(),
        )
    }

    fn code_with_return() -> Bytecode {
        bytecode! {
            PUSH1(0)
            PUSH1(0)
            RETURN
        }
    }

    fn code_with_revert() -> Bytecode {
        bytecode! {
            PUSH1(0)
            PUSH1(0)
            REVERT
        }
    }

    fn test_ok(tx: eth_types::Transaction, code: Option<Bytecode>) {
        // Get the execution steps from the external tracer
        let block: GethData = TestContext::<2, 1>::new(
            None,
            |accs| {
                accs[0].address(MOCK_ACCOUNTS[0]).balance(eth(10));
                if let Some(code) = code {
                    accs[0].code(code);
                }
                accs[1].address(MOCK_ACCOUNTS[1]).balance(eth(10));
            },
            |mut txs, _accs| {
                txs[0]
                    .to(tx.to.unwrap())
                    .from(tx.from)
                    .gas_price(tx.gas_price.unwrap())
                    .gas(tx.gas)
                    .input(tx.input)
                    .value(tx.value);
            },
            |block, _tx| block.number(0xcafeu64),
        )
        .unwrap()
        .into();

        assert_eq!(run_test_circuit_geth_data_default::<Fr>(block), Ok(()));
    }

    fn mock_tx(value: Word, gas_price: Word, calldata: Vec<u8>) -> eth_types::Transaction {
        let from = MOCK_ACCOUNTS[1];
        let to = MOCK_ACCOUNTS[0];
        eth_types::Transaction {
            from,
            to: Some(to),
            value,
            gas: gas(&calldata),
            gas_price: Some(gas_price),
            input: calldata.into(),
            ..Default::default()
        }
    }

    #[test]
    fn begin_tx_gadget_simple() {
        // Transfer 1 ether to account with empty code, successfully
        test_ok(mock_tx(eth(1), gwei(2), vec![]), None);

        // Transfer 1 ether, successfully
        test_ok(mock_tx(eth(1), gwei(2), vec![]), Some(code_with_return()));

        // Transfer 1 ether, tx reverts
        test_ok(mock_tx(eth(1), gwei(2), vec![]), Some(code_with_revert()));

        // Transfer nothing with some calldata
        test_ok(
            mock_tx(eth(0), gwei(2), vec![1, 2, 3, 4, 0, 0, 0, 0]),
            Some(code_with_return()),
        );
    }

    #[test]
    fn begin_tx_large_nonce() {
        // This test checks that the rw table assignment and evm circuit are consistent
        // in not applying an RLC to account and tx nonces.
        // https://github.com/privacy-scaling-explorations/zkevm-circuits/issues/592
        let multibyte_nonce = Word::from(700);

        let to = MOCK_ACCOUNTS[0];
        let from = MOCK_ACCOUNTS[1];

        let code = bytecode! {
            STOP
        };

        let block: GethData = TestContext::<2, 1>::new(
            None,
            |accs| {
                accs[0].address(to).balance(eth(1)).code(code);
                accs[1].address(from).balance(eth(1)).nonce(multibyte_nonce);
            },
            |mut txs, _| {
                txs[0].to(to).from(from).nonce(multibyte_nonce);
            },
            |block, _| block,
        )
        .unwrap()
        .into();

        assert_eq!(run_test_circuit_geth_data_default::<Fr>(block), Ok(()));
    }

    #[test]
    fn begin_tx_gadget_rand() {
        let random_amount = Word::from_little_endian(&rand_bytes(32)) % eth(1);
        let random_gas_price = Word::from_little_endian(&rand_bytes(32)) % gwei(2);
        // If this test fails, we want these values to appear in the CI logs.
        dbg!(random_amount, random_gas_price);

        for (value, gas_price, calldata, code) in [
            // Transfer random ether to account with empty code, successfully
            (random_amount, gwei(2), vec![], None),
            // Transfer nothing with random gas_price to account with empty code, successfully
            (eth(0), random_gas_price, vec![], None),
            // Transfer random ether, successfully
            (random_amount, gwei(2), vec![], Some(code_with_return())),
            // Transfer nothing with random gas_price, successfully
            (eth(0), random_gas_price, vec![], Some(code_with_return())),
            // Transfer random ether, tx reverts
            (random_amount, gwei(2), vec![], Some(code_with_revert())),
            // Transfer nothing with random gas_price, tx reverts
            (eth(0), random_gas_price, vec![], Some(code_with_revert())),
        ] {
            test_ok(mock_tx(value, gas_price, calldata), code);
        }
    }

    #[test]
    fn begin_tx_no_code() {
        let block: GethData = TestContext::<2, 1>::new(
            None,
            |accs| {
                accs[0].address(MOCK_ACCOUNTS[0]).balance(eth(20));
                accs[1].address(MOCK_ACCOUNTS[1]).balance(eth(10));
            },
            |mut txs, _accs| {
                txs[0]
                    .from(MOCK_ACCOUNTS[0])
                    .to(MOCK_ACCOUNTS[1])
                    .gas_price(gwei(2))
                    .gas(Word::from(0x10000))
                    .value(eth(2));
            },
            |block, _tx| block.number(0xcafeu64),
        )
        .unwrap()
        .into();

        assert_eq!(run_test_circuit_geth_data_default::<Fr>(block), Ok(()));
    }

    #[test]
    fn begin_tx_no_account() {
        let block: GethData = TestContext::<1, 1>::new(
            None,
            |accs| {
                accs[0].address(MOCK_ACCOUNTS[0]).balance(eth(20));
            },
            |mut txs, _accs| {
                txs[0]
                    .from(MOCK_ACCOUNTS[0])
                    .to(MOCK_ACCOUNTS[1])
                    .gas_price(gwei(2))
                    .gas(Word::from(0x10000))
                    .value(eth(2));
            },
            |block, _tx| block.number(0xcafeu64),
        )
        .unwrap()
        .into();

        assert_eq!(run_test_circuit_geth_data_default::<Fr>(block), Ok(()));
    }

    // Enable this test once we have support for contract deployment from
    // BeginTx.
    #[test]
    fn begin_tx_deploy() {
        let code = bytecode! {
            // [ADDRESS, STOP]
            PUSH32(word!("3000000000000000000000000000000000000000000000000000000000000000"))
            PUSH1(0)
            MSTORE

            PUSH1(2)
            PUSH1(0)
            RETURN
        };
        let block: GethData = TestContext::<1, 1>::new(
            None,
            |accs| {
                accs[0].address(MOCK_ACCOUNTS[0]).balance(eth(20));
            },
            |mut txs, _accs| {
                txs[0]
                    .from(MOCK_ACCOUNTS[0])
                    .gas_price(gwei(2))
                    .gas(Word::from(0x10000))
                    .value(eth(2))
                    .input(code.into());
            },
            |block, _tx| block.number(0xcafeu64),
        )
        .unwrap()
        .into();

        assert_eq!(run_test_circuit_geth_data_default::<Fr>(block), Ok(()));
    }
}
