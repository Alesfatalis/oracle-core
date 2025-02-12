use std::vec;

use crate::oracle_types::BlockHeight;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBoxCandidate;
use ergo_lib::ergotree_ir::chain::token::Token;
use thiserror::Error;

use crate::spec_token::RewardTokenId;
use crate::spec_token::SpecToken;
use crate::spec_token::TokenIdKind;

#[derive(Debug, Error)]
pub enum BuybackBoxError {}

#[derive(Debug, Clone)]
pub struct BuybackBoxWrapper {
    ergo_box: ErgoBox,
    reward_token_id: RewardTokenId,
}

#[allow(clippy::todo)]
impl BuybackBoxWrapper {
    pub fn new(ergo_box: ErgoBox, reward_token_id: RewardTokenId) -> Self {
        Self {
            ergo_box,
            reward_token_id,
        }
    }

    pub fn get_box(&self) -> &ErgoBox {
        &self.ergo_box
    }

    pub fn reward_token(&self) -> Option<SpecToken<RewardTokenId>> {
        self.ergo_box
            .tokens
            .as_ref()
            .unwrap()
            .get(1)
            .map(|token| SpecToken {
                token_id: RewardTokenId::from_token_id_unchecked(token.token_id),
                amount: token.amount,
            })
    }

    pub fn new_with_one_reward_token(&self, creation_height: BlockHeight) -> ErgoBoxCandidate {
        let single_reward_token = Token {
            token_id: self.reward_token_id.token_id(),
            amount: 1.try_into().unwrap(),
        };

        // take buyback nft and at least one reward token
        let tokens = vec![
            self.ergo_box
                .tokens
                .as_ref()
                .unwrap()
                .get(0)
                .unwrap()
                .clone(),
            single_reward_token,
        ]
        .try_into()
        .unwrap();

        ErgoBoxCandidate {
            value: self.ergo_box.value,
            ergo_tree: self.ergo_box.ergo_tree.clone(),
            tokens: Some(tokens),
            additional_registers: self.ergo_box.additional_registers.clone(),
            creation_height: creation_height.0,
        }
    }
}
