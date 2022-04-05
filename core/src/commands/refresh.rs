use crate::actions::RefreshAction;
use crate::box_kind::OracleBox;
use crate::contracts::refresh::RefreshContract;
use crate::oracle_state::DatapointStage;
use crate::oracle_state::LiveEpochStage;
use crate::wallet::WalletDataSource;

use ergo_lib::chain::ergo_box::box_builder::ErgoBoxCandidateBuilder;
use ergo_lib::ergotree_ir::chain::address::Address;
use ergo_lib::ergotree_ir::chain::ergo_box::box_value::BoxValue;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBoxCandidate;
use ergo_lib::ergotree_ir::chain::ergo_box::NonMandatoryRegisterId::R4;
use ergo_lib::wallet::box_selector::BoxSelection;
use ergo_lib::wallet::box_selector::BoxSelector;
use ergo_lib::wallet::box_selector::SimpleBoxSelector;
use ergo_lib::wallet::tx_builder::TxBuilder;

use std::convert::TryInto;

use bounded_vec::BoundedVec;

use super::PoolCommandError;

pub fn build_refresh_action<A: LiveEpochStage, B: DatapointStage, C: WalletDataSource>(
    live_epoch_stage_src: A,
    datapoint_stage_src: B,
    wallet: C,
    height: u32,
    change_address: Address,
) -> Result<RefreshAction, PoolCommandError> {
    let tx_fee = BoxValue::SAFE_USER_MIN;

    let in_pool_box = live_epoch_stage_src.get_pool_box()?;
    let in_refresh_box = live_epoch_stage_src.get_refresh_box()?;
    let mut in_oracle_boxes = datapoint_stage_src.get_oracle_datapoint_boxes()?;
    let valid_in_oracle_boxes = filtered_oracle_boxes(in_oracle_boxes);
    if valid_in_oracle_boxes.len() as u32 >= RefreshContract::new().min_data_points() {
        return Err(PoolCommandError::NotEnoughOracleBoxes {
            found: valid_in_oracle_boxes.len() as u32,
            expected: RefreshContract::new().min_data_points(),
        });
    }
    let rate = calc_pool_rate(valid_in_oracle_boxes.clone());
    let reward_decrement = valid_in_oracle_boxes.len() as u32 * 2;
    let out_pool_box = build_out_pool_box(in_pool_box.clone(), height, rate)?;
    let out_refresh_box = build_out_refresh_box(in_refresh_box.clone(), height, reward_decrement)?;
    let mut out_oracle_boxes = build_out_oracle_boxes(&valid_in_oracle_boxes, height)?;

    let unspent_boxes = wallet.get_unspent_wallet_boxes()?;
    let box_selector = SimpleBoxSelector::new();
    let selection = box_selector.select(unspent_boxes, tx_fee, &[])?;

    let mut input_boxes = vec![in_pool_box, in_refresh_box];
    let mut valid_in_oracle_raw_boxes = valid_in_oracle_boxes
        .into_iter()
        .map(|ob| ob.get_box())
        .collect();
    input_boxes.append(&mut valid_in_oracle_raw_boxes);
    input_boxes.append(selection.boxes.as_vec().clone().as_mut());
    let box_selection = BoxSelection {
        boxes: input_boxes.try_into().unwrap(),
        change_boxes: selection.change_boxes,
    };

    let mut output_candidates = vec![out_pool_box, out_refresh_box];
    output_candidates.append(&mut out_oracle_boxes);

    let b = TxBuilder::new(
        box_selection,
        output_candidates,
        height as u32,
        tx_fee,
        change_address,
        BoxValue::MIN,
    );
    let tx = b.build()?;
    Ok(RefreshAction { tx })
}

fn filtered_oracle_boxes(oracle_boxes: Vec<&dyn OracleBox>) -> Vec<&dyn OracleBox> {
    // The oracle boxes must be arranged in increasing order of their R6 values (rate).
    let mut successful_boxes = oracle_boxes.clone();
    successful_boxes.sort_by_key(|b| b.rate());
    // The first oracle box's rate must be within 5% of that of the last, and must be > 0
    // TODO: get from parameters? why? it's hardcoded in the contract anyway (as maxDeviationPercent in Refresh contract)
    let refresh_contract = RefreshContract::new();
    let deviation_range = refresh_contract.max_deviation_percent();
    while !deviation_check(deviation_range, &successful_boxes) {
        // Removing largest deviation outlier
        successful_boxes = remove_largest_local_deviation_datapoint(&successful_boxes)?;
        if (successful_boxes.len() as u32) < refresh_contract.min_data_points() {
            return Err(PoolCommandError::FailedToReachConsensus().into());
        }
    }
    successful_boxes
}

fn deviation_check(deviation_range: u32, datapoint_boxes: &Vec<&dyn OracleBox>) -> bool {
    let num = datapoint_boxes.len();
    // expected sorted datapoint_boxes
    let max_datapoint = datapoint_boxes[0].rate();
    let min_datapoint = datapoint_boxes[num - 1].rate();
    let deviation_delta = max_datapoint * (deviation_range as u64) / 100;
    min_datapoint >= max_datapoint - deviation_delta
}

/// Finds whether the first or the last value in a list of sorted Datapoint boxes
/// deviates more compared to their adjacted datapoint, and then removes
/// said datapoint which deviates further.
fn remove_largest_local_deviation_datapoint<'a>(
    datapoint_boxes: &'a BoundedVec<&dyn OracleBox, 2, 255>,
) -> Result<Vec<&'a dyn OracleBox>, PoolCommandError> {
    let mut processed_boxes = datapoint_boxes.clone();

    // Check if sufficient number of datapoint boxes to start removing
    if datapoint_boxes.len() <= 2 {
        Err(PoolCommandError::FailedToReachConsensus().into())
    } else {
        // Deserialize all the datapoints in a list
        let dp_len = datapoint_boxes.len();
        let datapoints: Vec<i64> = datapoint_boxes
            .iter()
            .map(|_| datapoint_boxes[0].rate() as i64)
            .collect();
        // Check deviation by subtracting largest value by 2nd largest
        let front_deviation = datapoints[0] - datapoints[1];
        // Check deviation by subtracting 2nd smallest value by smallest
        let back_deviation = datapoints[dp_len - 2] - datapoints[dp_len - 1];

        // Remove largest datapoint if front deviation is greater
        if front_deviation >= back_deviation {
            processed_boxes.drain(0..1);
        }
        // Remove smallest datapoint if back deviation is greater
        else {
            processed_boxes.pop();
        }
        Ok(processed_boxes)
    }
}

fn calc_pool_rate(oracle_boxes: Vec<&dyn OracleBox>) -> u64 {
    todo!()
}

fn build_out_pool_box(
    in_pool_box: ErgoBox,
    creation_height: u32,
    rate: u64,
) -> Result<ErgoBoxCandidate, PoolCommandError> {
    todo!()
}

fn build_out_refresh_box(
    in_refresh_box: ErgoBox,
    creation_height: u32,
    reward_decrement: u32,
) -> Result<ErgoBoxCandidate, PoolCommandError> {
    todo!()
}

fn build_out_oracle_boxes(
    valid_oracle_boxes: &Vec<&dyn OracleBox>,
    creation_height: u32,
) -> Result<Vec<ErgoBoxCandidate>, PoolCommandError> {
    valid_oracle_boxes
        .iter()
        .map(|in_ob| {
            let mut builder = ErgoBoxCandidateBuilder::new(
                in_ob.value(),
                in_ob.ergo_tree().clone(),
                creation_height,
            );
            builder.set_register_value(R4, in_ob.public_key().into());
            builder.add_token(in_ob.oracle_token().clone());
            let mut reward_token_new = in_ob.reward_token();
            reward_token_new.amount = reward_token_new
                .amount
                .checked_add(&1u64.try_into().unwrap())
                .unwrap();
            builder.add_token(reward_token_new.clone());
            builder.build().map_err(Into::into)
        })
        .collect::<Result<Vec<ErgoBoxCandidate>, PoolCommandError>>()
}

#[cfg(test)]
mod tests {
    use std::convert::TryInto;

    use ergo_lib::chain::ergo_state_context::ErgoStateContext;
    use ergo_lib::chain::transaction::unsigned::UnsignedTransaction;
    use ergo_lib::chain::transaction::TxId;
    use ergo_lib::chain::transaction::TxIoVec;
    use ergo_lib::ergotree_ir::chain::address::AddressEncoder;
    use ergo_lib::ergotree_ir::chain::ergo_box::box_value::BoxValue;
    use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
    use ergo_lib::ergotree_ir::chain::ergo_box::NonMandatoryRegisterId;
    use ergo_lib::ergotree_ir::chain::ergo_box::NonMandatoryRegisters;
    use ergo_lib::ergotree_ir::chain::token::Token;
    use ergo_lib::ergotree_ir::chain::token::TokenId;
    use ergo_lib::ergotree_ir::mir::constant::Constant;
    use ergo_lib::ergotree_ir::sigma_protocol::dlog_group::EcPoint;
    use ergo_lib::wallet::signing::TransactionContext;
    use ergo_lib::wallet::Wallet;
    use ergo_node_interface::node_interface::NodeError;
    use sigma_test_util::force_any_val;
    use sigma_test_util::force_any_val_with;

    use crate::box_kind::OracleBoxWrapper;
    use crate::contracts::oracle::OracleContract;
    use crate::contracts::pool::PoolContract;
    use crate::oracle_state::StageError;

    use super::*;

    #[derive(Clone)]
    struct LiveEpochStageMock {
        refresh_box: ErgoBox,
        pool_box: ErgoBox,
    }

    impl LiveEpochStage for LiveEpochStageMock {
        fn get_refresh_box(&self) -> std::result::Result<ErgoBox, StageError> {
            Ok(self.refresh_box.clone())
        }

        fn get_pool_box(&self) -> std::result::Result<ErgoBox, StageError> {
            Ok(self.pool_box.clone())
        }
    }

    #[derive(Clone)]
    struct DatapointStageMock {
        datapoints: Vec<OracleBoxWrapper>,
    }

    impl DatapointStage for DatapointStageMock {
        fn get_oracle_datapoint_boxes(
            &self,
        ) -> std::result::Result<Vec<&dyn OracleBox>, StageError> {
            let wrapped_boxes = self
                .datapoints
                .iter()
                .map(|b| b as &dyn OracleBox)
                .collect();
            Ok(wrapped_boxes)
        }
    }

    #[derive(Clone)]
    struct WalletDataMock {
        unspent_boxes: Vec<ErgoBox>,
    }

    impl WalletDataSource for WalletDataMock {
        fn get_unspent_wallet_boxes(&self) -> Result<Vec<ErgoBox>, NodeError> {
            Ok(self.unspent_boxes.clone())
        }
    }

    fn make_refresh_box(
        refresh_nft: &TokenId,
        reward_token: Token,
        value: BoxValue,
        creation_height: u32,
    ) -> ErgoBox {
        let tokens = vec![
            Token::from((refresh_nft.clone(), 1u64.try_into().unwrap())),
            reward_token,
        ]
        .try_into()
        .unwrap();
        ErgoBox::new(
            value,
            RefreshContract::new().ergo_tree(),
            Some(tokens),
            NonMandatoryRegisters::empty(),
            creation_height,
            force_any_val::<TxId>(),
            0,
        )
        .unwrap()
    }

    fn make_pool_box(
        datapoint: i64,
        epoch_counter: i32,
        refresh_nft: TokenId,
        value: BoxValue,
        creation_height: u32,
    ) -> ErgoBox {
        let tokens = [Token::from((refresh_nft.clone(), 1u64.try_into().unwrap()))].into();
        ErgoBox::new(
            value,
            PoolContract::new().ergo_tree(),
            Some(tokens),
            NonMandatoryRegisters::new(
                vec![
                    (NonMandatoryRegisterId::R4, Constant::from(datapoint)),
                    (NonMandatoryRegisterId::R5, Constant::from(epoch_counter)),
                ]
                .into_iter()
                .collect(),
            )
            .unwrap(),
            creation_height,
            force_any_val::<TxId>(),
            0,
        )
        .unwrap()
    }

    fn make_datapoint_box(
        pub_key: EcPoint,
        datapoint: i64,
        epoch_counter: i32,
        oracle_token_id: TokenId,
        reward_token: Token,
        value: BoxValue,
        creation_height: u32,
    ) -> ErgoBox {
        let tokens = vec![
            Token::from((oracle_token_id.clone(), 1u64.try_into().unwrap())),
            reward_token,
        ]
        .try_into()
        .unwrap();
        ErgoBox::new(
            value,
            OracleContract::new().ergo_tree(),
            Some(tokens),
            NonMandatoryRegisters::new(
                vec![
                    (NonMandatoryRegisterId::R4, Constant::from(pub_key)),
                    (NonMandatoryRegisterId::R5, Constant::from(epoch_counter)),
                    (NonMandatoryRegisterId::R6, Constant::from(datapoint)),
                ]
                .into_iter()
                .collect(),
            )
            .unwrap(),
            creation_height,
            force_any_val::<TxId>(),
            0,
        )
        .unwrap()
    }

    fn find_input_boxes(
        tx: UnsignedTransaction,
        available_boxes: Vec<ErgoBox>,
    ) -> TxIoVec<ErgoBox> {
        todo!()
    }

    #[test]
    fn test_refresh_pool() {
        let height = 100u32;
        let refresh_contract = RefreshContract::new();
        let reward_token_id =
            TokenId::from_base64("RytLYlBlU2hWbVlxM3Q2dzl6JEMmRilKQE1jUWZUalc=").unwrap();
        let refresh_nft =
            TokenId::from_base64("VGpXblpyNHU3eCFBJUQqRy1LYU5kUmdVa1hwMnM1djg=").unwrap();
        let in_refresh_box = make_refresh_box(
            &refresh_nft,
            Token::from((reward_token_id.clone(), 100u64.try_into().unwrap())).clone(),
            BoxValue::SAFE_USER_MIN,
            height - 10,
        );
        let in_pool_box = make_pool_box(1, 1, refresh_nft, BoxValue::SAFE_USER_MIN, height - 10);
        let oracle_pub_key = force_any_val::<EcPoint>();
        let in_oracle_box = make_datapoint_box(
            oracle_pub_key,
            1,
            1,
            refresh_contract.oracle_nft_token_id(),
            Token::from((reward_token_id, 5u64.try_into().unwrap())),
            BoxValue::SAFE_USER_MIN,
            height - 9, // right after the pool+oracle boxes block
        );
        let live_epoch_stage_mock = LiveEpochStageMock {
            refresh_box: in_refresh_box,
            pool_box: in_pool_box.clone(),
        };
        let change_address =
            AddressEncoder::new(ergo_lib::ergotree_ir::chain::address::NetworkPrefix::Mainnet)
                .parse_address_from_str("9iHyKxXs2ZNLMp9N9gbUT9V8gTbsV7HED1C1VhttMfBUMPDyF7r")
                .unwrap();
        let datapoint_stage_mock = DatapointStageMock {
            datapoints: vec![OracleBoxWrapper::new(in_oracle_box.clone()).unwrap()],
        };
        let wallet_mock = WalletDataMock {
            unspent_boxes: vec![force_any_val_with::<ErgoBox>(
                (BoxValue::MIN_RAW * 5000..BoxValue::MIN_RAW * 10000).into(),
            )],
        };
        let action = build_refresh_action(
            live_epoch_stage_mock.clone(),
            datapoint_stage_mock.clone(),
            wallet_mock.clone(),
            100,
            change_address,
        )
        .unwrap();

        let ctx = force_any_val::<ErgoStateContext>();
        let wallet = Wallet::from_mnemonic("", "").unwrap();

        let mut possible_input_boxes = vec![
            live_epoch_stage_mock.get_pool_box().unwrap(),
            live_epoch_stage_mock.get_refresh_box().unwrap(),
        ];
        possible_input_boxes.append(&mut vec![in_oracle_box]);
        possible_input_boxes.append(&mut wallet_mock.get_unspent_wallet_boxes().unwrap());

        let tx_context = TransactionContext::new(
            action.tx.clone(),
            find_input_boxes(action.tx, possible_input_boxes),
            None,
        )
        .unwrap();
        assert!(wallet.sign_transaction(tx_context, &ctx, None).is_ok());
    }
}
