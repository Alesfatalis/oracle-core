//! Bootstrap a new oracle pool
use std::{
    convert::{TryFrom, TryInto},
    io::Write,
};

use derive_more::From;
use ergo_lib::{
    chain::{
        ergo_box::box_builder::{ErgoBoxCandidateBuilder, ErgoBoxCandidateBuilderError},
        transaction::Transaction,
    },
    ergotree_ir::{
        chain::{
            address::{
                Address, AddressEncoder, AddressEncoderError, NetworkAddress, NetworkPrefix,
            },
            ergo_box::{
                box_value::{BoxValue, BoxValueError},
                ErgoBox,
            },
            token::{Token, TokenId},
        },
        ergo_tree::ErgoTree,
        serialization::SigmaParsingError,
    },
    wallet::{
        box_selector::{BoxSelector, BoxSelectorError, SimpleBoxSelector},
        tx_builder::{TxBuilder, TxBuilderError},
    },
};
use ergo_node_interface::{node_interface::NodeError, NodeInterface};
use log::{debug, info};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use yaml_rust::{Yaml, YamlEmitter, YamlLoader};

use crate::{
    box_kind::{make_pool_box_candidate, make_refresh_box_candidate},
    contracts::{
        pool::{PoolContract, PoolContractParameters},
        refresh::{RefreshContract, RefreshContractError, RefreshContractParameters},
        update::{UpdateContract, UpdateContractError, UpdateContractParameters},
    },
    node_interface::{assert_wallet_unlocked, SignTransaction, SubmitTransaction},
    oracle_config::TokenIds,
    wallet::WalletDataSource,
};

/// Loads bootstrap configuration file and performs the chain-transactions for minting of tokens and
/// box creations. An oracle configuration file is then created which contains the `TokenId`s of the
/// minted tokens.
pub fn bootstrap(yaml_config_file_name: String) -> Result<(), BootstrapError> {
    let s = std::fs::read_to_string(yaml_config_file_name.clone())?;
    let yaml = &YamlLoader::load_from_str(&s).unwrap()[0];
    let config = bootstrap_config_from_yaml(yaml)?;

    info!("{} loaded", yaml_config_file_name);
    // We can't call any functions from the `crate::node_interface` module because we don't have an
    // `oracle_config.yaml` file to work from here.
    let node = NodeInterface::new(&config.node_api_key, &config.node_ip, &config.node_port);
    assert_wallet_unlocked(&node);
    let prefix = if config.is_mainnet {
        NetworkPrefix::Mainnet
    } else {
        NetworkPrefix::Testnet
    };
    let change_address_str = node
        .wallet_status()?
        .change_address
        .ok_or(BootstrapError::NoChangeAddressSetInNode)?;
    debug!("Change address: {}", change_address_str);

    let change_address = AddressEncoder::new(prefix).parse_address_from_str(&change_address_str)?;
    let input = BootstrapInput {
        config,
        wallet: &node as &dyn WalletDataSource,
        tx_signer: &node as &dyn SignTransaction,
        submit_tx: &node as &dyn SubmitTransaction,
        tx_fee: BoxValue::SAFE_USER_MIN,
        erg_value_per_box: BoxValue::SAFE_USER_MIN,
        change_address,
        height: node.current_block_height()? as u32,
    };
    let oracle_config = perform_bootstrap_chained_transaction(input)?;
    info!("Bootstrap chain-transaction complete");
    let s = serde_yaml::to_string(&oracle_config)?;
    let mut file = std::fs::File::create(crate::oracle_config::DEFAULT_CONFIG_FILE_NAME)?;
    file.write_all(s.as_bytes())?;
    Ok(())
}

pub struct BootstrapInput<'a> {
    pub config: BootstrapConfig,
    pub wallet: &'a dyn WalletDataSource,
    pub tx_signer: &'a dyn SignTransaction,
    pub submit_tx: &'a dyn SubmitTransaction,
    pub tx_fee: BoxValue,
    pub erg_value_per_box: BoxValue,
    pub change_address: Address,
    pub height: u32,
}

/// Perform and submit to the mempool the chained-transaction to boostrap the oracle pool. We first
/// mint the oracle-pool tokens then create the pool and refresh boxes as described in EIP-23:
/// https://github.com/ergoplatform/eips/blob/eip23/eip-0023.md#tokens
pub(crate) fn perform_bootstrap_chained_transaction(
    input: BootstrapInput,
) -> Result<OracleConfigFields, BootstrapError> {
    let BootstrapInput {
        config,
        wallet,
        tx_signer: wallet_sign,
        submit_tx,
        tx_fee,
        erg_value_per_box,
        change_address,
        height,
        ..
    } = input;

    // We can calculate the amount of ERGs necessary to effect this chained-transaction upfront.
    // We're going to mint 6 distinct types of tokens and create the pool and refresh boxes as
    // described in EIP-23. The minting of each type of token requires a distinct transaction, so we
    // need 8 transactions in total. We assume that the resulting token-holding boxes generated from
    // these transactions each has a box value of `erg_value_per_box`. Similarly the pool and
    // refresh boxes will also hold `erg_value_per_box`.
    //
    // Now define `E_i = i*(erg_value_per_box + tx_fee)` for `i = 1,2,.., 8`. `E_i` represents the
    // amount of ERGs necessary to effect `i` remaining transactions.
    //
    // So we require a total ERG value of `E_8 = 8*(erg_value_per_box + tx_fee)`
    //
    // The chain transaction is structured as follows:
    //   * First sweep the unspent boxes of the wallet for a target balance of `E_8`. Denote these
    //     input boxes by `I_1`.
    //
    //   * Mint the first token with `I_1` as input, resulting in two output boxes:
    //      - `B_1_token` containing the minted token and `ergo_value_per_box`
    //      - `B_1_remaining` containing `E_7` in ERG value.
    //
    //   * Mint the second token with input boxes containing `B_1_remaining`, resulting in two
    //     output boxes:
    //      - `B_2_token` containing the minted token and `ergo_value_per_box`
    //      - `B_2_remaining` containing `E_6` in ERG value.
    //
    // And so on.

    // This variable represents the index `i` described above.
    let mut num_transactions_left = 8;

    let wallet_pk_ergo_tree = config
        .addresses
        .wallet_address_for_chain_transaction
        .script()?;
    let guard = wallet_pk_ergo_tree.clone();

    // Since we're building a chain of transactions, we need to filter the output boxes of each
    // constituent transaction to be only those that are guarded by our wallet's key.
    let filter_tx_outputs = move |outputs: Vec<ErgoBox>| -> Vec<ErgoBox> {
        outputs
            .clone()
            .into_iter()
            .filter(|b| b.ergo_tree == guard)
            .collect()
    };

    // This closure computes `E_{num_transactions_left}`.
    let calc_target_balance = |num_transactions_left| {
        let b = erg_value_per_box.checked_mul_u32(num_transactions_left)?;
        let fees = tx_fee.checked_mul_u32(num_transactions_left)?;
        b.checked_add(&fees)
    };

    // Effect a single transaction that mints a token with given details, as described in comments
    // at the beginning. By default it uses `wallet_pk_ergo_tree` as the guard for the token box,
    // but this can be overriden with `different_token_box_guard`.
    let mint_token = |input_boxes: Vec<ErgoBox>,
                      num_transactions_left: &mut u32,
                      token_name,
                      token_desc,
                      token_amount,
                      different_token_box_guard: Option<ErgoTree>|
     -> Result<(Token, Transaction), BootstrapError> {
        let target_balance = calc_target_balance(*num_transactions_left)?;
        let box_selector = SimpleBoxSelector::new();
        let box_selection = box_selector.select(input_boxes, target_balance, &[])?;
        let token = Token {
            token_id: box_selection.boxes.first().box_id().into(),
            amount: token_amount,
        };
        let token_box_guard =
            different_token_box_guard.unwrap_or_else(|| wallet_pk_ergo_tree.clone());
        let mut builder = ErgoBoxCandidateBuilder::new(erg_value_per_box, token_box_guard, height);
        builder.mint_token(token.clone(), token_name, token_desc, 1);
        let mut output_candidates = vec![builder.build()?];

        let remaining_funds = ErgoBoxCandidateBuilder::new(
            calc_target_balance(*num_transactions_left - 1)?,
            wallet_pk_ergo_tree.clone(),
            height,
        )
        .build()?;
        output_candidates.push(remaining_funds.clone());

        let inputs = box_selection.boxes.clone();
        let tx_builder = TxBuilder::new(
            box_selection,
            output_candidates,
            height,
            tx_fee,
            change_address.clone(),
            BoxValue::MIN,
        );
        let mint_token_tx = tx_builder.build()?;
        debug!("Mint token unsigned transaction: {:?}", mint_token_tx);
        let signed_tx = wallet_sign.sign_transaction_with_inputs(&mint_token_tx, inputs, None)?;
        *num_transactions_left -= 1;
        Ok((token, signed_tx))
    };

    // Mint pool NFT token --------------------------------------------------------------------------
    info!("Minting pool NFT tx");
    let unspent_boxes = wallet.get_unspent_wallet_boxes()?;
    debug!("unspent boxes: {:?}", unspent_boxes);
    let target_balance = calc_target_balance(num_transactions_left)?;
    debug!("target_balance: {:?}", target_balance);
    let box_selector = SimpleBoxSelector::new();
    let box_selection = box_selector.select(unspent_boxes.clone(), target_balance, &[])?;
    debug!("box selection: {:?}", box_selection);

    let (pool_nft_token, signed_mint_pool_nft_tx) = mint_token(
        box_selection.boxes.as_vec().clone(),
        &mut num_transactions_left,
        config.tokens_to_mint.pool_nft.name.clone(),
        config.tokens_to_mint.pool_nft.description.clone(),
        1.try_into().unwrap(),
        None,
    )?;
    debug!("signed_mint_pool_nft_tx: {:?}", signed_mint_pool_nft_tx);

    // Mint refresh NFT token ----------------------------------------------------------------------
    info!("Minting refresh NFT tx");
    let inputs = filter_tx_outputs(signed_mint_pool_nft_tx.outputs.clone());
    debug!("inputs for refresh NFT mint: {:?}", inputs);
    let (refresh_nft_token, signed_mint_refresh_nft_tx) = mint_token(
        inputs,
        &mut num_transactions_left,
        config.tokens_to_mint.refresh_nft.name.clone(),
        config.tokens_to_mint.refresh_nft.description.clone(),
        1.try_into().unwrap(),
        None,
    )?;
    debug!(
        "signed_mint_refresh_nft_tx: {:?}",
        signed_mint_refresh_nft_tx
    );

    // Mint ballot tokens --------------------------------------------------------------------------
    info!("Minting ballot tokens tx");
    let inputs = filter_tx_outputs(signed_mint_refresh_nft_tx.outputs.clone());
    debug!("inputs for ballot tokens mint: {:?}", inputs);
    let (ballot_token, signed_mint_ballot_tokens_tx) = mint_token(
        inputs,
        &mut num_transactions_left,
        config.tokens_to_mint.ballot_tokens.name.clone(),
        config.tokens_to_mint.ballot_tokens.description.clone(),
        config
            .tokens_to_mint
            .ballot_tokens
            .quantity
            .try_into()
            .unwrap(),
        None,
    )?;
    debug!(
        "signed_mint_ballot_tokens_tx: {:?}",
        signed_mint_ballot_tokens_tx
    );

    // Mint update NFT token -----------------------------------------------------------------------

    // We need to create an instance of `UpdateContract`, which requires the token IDs of the pool
    // NFT and the ballot token.
    let token_ids = TokenIds {
        pool_nft_token_id: pool_nft_token.token_id.clone(),
        refresh_nft_token_id: refresh_nft_token.token_id.clone(), // Not strictly needed for `UpdateContract`
        update_nft_token_id: TokenId::from_base64("P0QORY1LYVBKU2dWa1lwM3M2djl5JEImRSlIQE1iUWU")
            .unwrap(), // dummy value
        oracle_token_id: TokenId::from_base64("P0QORY1LYVBKU2dWa1lwM3M2djl5JEImRSlIQE1iUWU")
            .unwrap(), // dummy value
        reward_token_id: TokenId::from_base64("P0QORY1LYVBKU2dWa1lwM3M2djl5JEImRSlIQE1iUWU")
            .unwrap(), // dummy value
        ballot_token_id: ballot_token.token_id.clone(),
    };
    let update_contract = UpdateContract::new(&config.update_contract_parameters, &token_ids)?;

    info!("Minting update NFT tx");
    let inputs = filter_tx_outputs(signed_mint_ballot_tokens_tx.outputs.clone());
    debug!("inputs for update NFT mint: {:?}", inputs);
    let (update_nft_token, signed_mint_update_nft_tx) = mint_token(
        inputs,
        &mut num_transactions_left,
        config.tokens_to_mint.update_nft.name.clone(),
        config.tokens_to_mint.update_nft.description.clone(),
        1.try_into().unwrap(),
        Some(update_contract.ergo_tree()),
    )?;
    debug!("signed_mint_update_nft_tx: {:?}", signed_mint_update_nft_tx);

    // Mint oracle tokens --------------------------------------------------------------------------
    info!("Minting oracle tokens tx");
    let inputs = filter_tx_outputs(signed_mint_update_nft_tx.outputs.clone());
    debug!("inputs for oracle tokens mint: {:?}", inputs);
    let oracle_tokens_pk_ergo_tree = config.addresses.address_for_oracle_tokens.script()?;
    let (oracle_token, signed_mint_oracle_tokens_tx) = mint_token(
        inputs,
        &mut num_transactions_left,
        config.tokens_to_mint.oracle_tokens.name.clone(),
        config.tokens_to_mint.oracle_tokens.description.clone(),
        config
            .tokens_to_mint
            .oracle_tokens
            .quantity
            .try_into()
            .unwrap(),
        Some(oracle_tokens_pk_ergo_tree),
    )?;
    debug!(
        "signed_mint_oracle_tokens_tx: {:?}",
        signed_mint_oracle_tokens_tx
    );

    // Mint reward tokens --------------------------------------------------------------------------
    info!("Minting reward tokens tx");
    let inputs = filter_tx_outputs(signed_mint_oracle_tokens_tx.outputs.clone());
    debug!("inputs for reward tokens mint: {:?}", inputs);
    let (reward_token, signed_mint_reward_tokens_tx) = mint_token(
        inputs,
        &mut num_transactions_left,
        config.tokens_to_mint.reward_tokens.name.clone(),
        config.tokens_to_mint.reward_tokens.description.clone(),
        config
            .tokens_to_mint
            .reward_tokens
            .quantity
            .try_into()
            .unwrap(),
        None,
    )?;

    // Create pool box -----------------------------------------------------------------------------
    info!("Create pool box tx");

    let token_ids = TokenIds {
        pool_nft_token_id: pool_nft_token.token_id.clone(),
        refresh_nft_token_id: refresh_nft_token.token_id.clone(),
        update_nft_token_id: update_nft_token.token_id.clone(),
        oracle_token_id: oracle_token.token_id.clone(),
        reward_token_id: reward_token.token_id.clone(),
        ballot_token_id: ballot_token.token_id.clone(),
    };

    let pool_contract_parameters = PoolContractParameters {
        p2s: config.pool_contract_parameters.p2s,
        refresh_nft_index: config.pool_contract_parameters.refresh_nft_index,
        update_nft_index: config.pool_contract_parameters.update_nft_index,
    };
    let pool_contract = PoolContract::new(&pool_contract_parameters, &token_ids).unwrap();

    let reward_tokens_for_pool_box = Token {
        token_id: reward_token.token_id.clone(),
        amount: reward_token
            .amount
            // we must leave one reward token per oracle for their first datapoint box
            .checked_sub(&oracle_token.amount)
            .unwrap(),
    };
    let pool_box_candidate = make_pool_box_candidate(
        &pool_contract,
        // We intentionally set the initial datapoint to be 0, as it's treated as 'undefined' during bootstrap.
        0,
        1,
        pool_nft_token.clone(),
        reward_tokens_for_pool_box,
        erg_value_per_box,
        height,
    )?;
    let mut output_candidates = vec![pool_box_candidate];

    // Build box for remaining funds
    let builder = ErgoBoxCandidateBuilder::new(
        calc_target_balance(num_transactions_left - 1)?,
        wallet_pk_ergo_tree.clone(),
        height,
    );
    output_candidates.push(builder.build()?);

    let target_balance = calc_target_balance(num_transactions_left)?;
    let box_selector = SimpleBoxSelector::new();
    let mut inputs = filter_tx_outputs(signed_mint_reward_tokens_tx.outputs.clone());

    // Need to find the box containing the pool NFT, and transfer this token to the pool box.
    let box_with_pool_nft = signed_mint_pool_nft_tx
        .outputs
        .iter()
        .find(|b| {
            if let Some(tokens) = &b.tokens {
                tokens.iter().any(|t| t.token_id == pool_nft_token.token_id)
            } else {
                false
            }
        })
        .unwrap()
        .clone();
    inputs.push(box_with_pool_nft);

    let box_selection = box_selector.select(
        inputs,
        target_balance,
        &[pool_nft_token.clone(), reward_token.clone()],
    )?;
    let inputs = box_selection.boxes.clone();
    let tx_builder = TxBuilder::new(
        box_selection,
        output_candidates,
        height,
        tx_fee,
        change_address.clone(),
        BoxValue::MIN,
    );
    let pool_box_tx = tx_builder.build()?;
    debug!("unsigned pool_box_tx: {:?}", pool_box_tx);
    let signed_pool_box_tx =
        wallet_sign.sign_transaction_with_inputs(&pool_box_tx, inputs, None)?;
    num_transactions_left -= 1;

    // Create refresh box --------------------------------------------------------------------------
    info!("Create refresh box tx");
    let BootstrapRefreshContractParameters {
        p2s,
        pool_nft_index,
        oracle_token_id_index,
        min_data_points_index,
        min_data_points,
        buffer_index,
        buffer_length,
        max_deviation_percent_index,
        max_deviation_percent,
        epoch_length_index,
        epoch_length,
        ..
    } = config.refresh_contract_parameters;

    let parameters = RefreshContractParameters {
        p2s,
        pool_nft_index,
        oracle_token_id_index,
        min_data_points_index,
        min_data_points,
        buffer_index,
        buffer_length,
        max_deviation_percent_index,
        max_deviation_percent,
        epoch_length_index,
        epoch_length,
    };

    let refresh_contract = RefreshContract::new(&parameters, &token_ids)?;

    let refresh_box_candidate = make_refresh_box_candidate(
        &refresh_contract,
        refresh_nft_token.clone(),
        erg_value_per_box,
        height,
    )?;

    let output_candidates = vec![refresh_box_candidate];

    let target_balance = calc_target_balance(num_transactions_left)?;
    let box_selector = SimpleBoxSelector::new();
    let mut inputs = filter_tx_outputs(signed_pool_box_tx.outputs.clone());

    // Need to find the box containing the refresh NFT, and transfer this token to the refresh box.
    let box_with_refresh_nft = signed_mint_refresh_nft_tx
        .outputs
        .iter()
        .find(|b| {
            if let Some(tokens) = &b.tokens {
                tokens
                    .iter()
                    .any(|t| t.token_id == refresh_nft_token.token_id)
            } else {
                false
            }
        })
        .unwrap()
        .clone();
    inputs.push(box_with_refresh_nft);

    let box_selection =
        box_selector.select(inputs, target_balance, &[refresh_nft_token.clone()])?;
    let inputs = box_selection.boxes.clone();
    let tx_builder = TxBuilder::new(
        box_selection,
        output_candidates,
        height,
        tx_fee,
        change_address.clone(),
        BoxValue::MIN,
    );
    let refresh_box_tx = tx_builder.build()?;
    debug!("unsigned refresh_box_tx: {:?}", refresh_box_tx);
    let signed_refresh_box_tx =
        wallet_sign.sign_transaction_with_inputs(&refresh_box_tx, inputs, None)?;

    // ---------------------------------------------------------------------------------------------
    let tx_id = submit_tx.submit_transaction(&signed_mint_pool_nft_tx)?;
    info!("Minting pool NFT TxId: {}", tx_id);
    let tx_id = submit_tx.submit_transaction(&signed_mint_refresh_nft_tx)?;
    info!("Minting refresh NFT TxId: {}", tx_id);
    let tx_id = submit_tx.submit_transaction(&signed_mint_ballot_tokens_tx)?;
    info!("Minting ballot tokens TxId: {}", tx_id);
    let tx_id = submit_tx.submit_transaction(&signed_mint_update_nft_tx)?;
    info!("Minting update NFT TxId: {}", tx_id);
    let tx_id = submit_tx.submit_transaction(&signed_mint_oracle_tokens_tx)?;
    info!("Minting oracle tokens TxId: {}", tx_id);
    let tx_id = submit_tx.submit_transaction(&signed_mint_reward_tokens_tx)?;
    info!("Minting reward tokens TxId: {}", tx_id);
    let tx_id = submit_tx.submit_transaction(&signed_pool_box_tx)?;
    info!("Creating initial pool box TxId: {}", tx_id);
    let tx_id = submit_tx.submit_transaction(&signed_refresh_box_tx)?;
    info!("Creating initial refresh box TxId: {}", tx_id);

    Ok(OracleConfigFields {
        pool_nft: pool_nft_token.token_id,
        refresh_nft: refresh_nft_token.token_id,
        update_nft: update_nft_token.token_id,
        oracle_token: oracle_token.token_id,
        ballot_token: ballot_token.token_id,
        reward_token: reward_token.token_id,
        node_ip: config.node_ip,
        node_port: config.node_port,
        node_api_key: config.node_api_key,
    })
}

fn bootstrap_config_from_yaml(yaml: &Yaml) -> Result<BootstrapConfig, BootstrapError> {
    let is_mainnet = yaml["is_mainnet"]
        .as_bool()
        .ok_or_else(|| BootstrapError::YamlRust("`is_mainnet` missing".into()))?;

    let network_prefix = if is_mainnet {
        NetworkPrefix::Mainnet
    } else {
        NetworkPrefix::Testnet
    };

    let tokens_to_mint: TokensToMint = {
        // We'd like to use `serde_yaml` to deserialize `TokensToMint`. Since we're committed to
        // using `yaml-rust` we extract out the contents of the `tokens_to_mint` field in the YAML
        // file, convert it back to a YAML string, then pass it to `serde_yaml`.
        let hash = yaml["tokens_to_mint"]
            .as_hash()
            .ok_or_else(|| BootstrapError::YamlRust("`tokens_to_mint` missing".into()))?
            .clone();
        let mut out = String::new();
        let mut emitter = YamlEmitter::new(&mut out);
        emitter.dump(&Yaml::Hash(hash)).unwrap();
        serde_yaml::from_str(&out)?
    };

    let address_for_minted_tokens_str = yaml["addresses"]["address_for_oracle_tokens"]
        .as_str()
        .ok_or_else(|| BootstrapError::YamlRust("`address_for_oracle_tokens` missing".into()))?;
    let address_for_minted_tokens = AddressEncoder::new(network_prefix)
        .parse_address_from_str(address_for_minted_tokens_str)?;

    let wallet_address_for_chain_transaction_str = yaml["addresses"]
        ["wallet_address_for_chain_transaction"]
        .as_str()
        .ok_or_else(|| {
            BootstrapError::YamlRust("`wallet_address_for_chain_transaction` missing".into())
        })?;
    let wallet_address_for_chain_transaction = AddressEncoder::new(network_prefix)
        .parse_address_from_str(wallet_address_for_chain_transaction_str)?;

    let addresses = Addresses {
        address_for_oracle_tokens: address_for_minted_tokens,
        wallet_address_for_chain_transaction,
    };

    let refresh_contract_parameters: BootstrapRefreshContractParameters = {
        // The struct is created via the same process as `tokens_to_mint` above.
        let hash = yaml["refresh_contract_parameters"]
            .as_hash()
            .ok_or_else(|| {
                BootstrapError::YamlRust("`refresh_contract_parameters` missing".into())
            })?
            .clone();
        let mut out = String::new();
        let mut emitter = YamlEmitter::new(&mut out);
        emitter.dump(&Yaml::Hash(hash)).unwrap();
        serde_yaml::from_str(&out)?
    };

    let pool_contract_parameters: BootstrapPoolContractParameters = {
        // The struct is created via the same process as `tokens_to_mint` above.
        let hash = yaml["pool_contract_parameters"]
            .as_hash()
            .ok_or_else(|| BootstrapError::YamlRust("`pool_contract_parameters` missing".into()))?
            .clone();
        let mut out = String::new();
        let mut emitter = YamlEmitter::new(&mut out);
        emitter.dump(&Yaml::Hash(hash)).unwrap();
        serde_yaml::from_str(&out)?
    };
    let update_contract_parameters: UpdateContractParameters = {
        // The struct is created via the same process as `tokens_to_mint` above.
        let hash = yaml["update_contract_parameters"]
            .as_hash()
            .ok_or_else(|| BootstrapError::YamlRust("`update_contract_parameters` missing".into()))?
            .clone();
        let mut out = String::new();
        let mut emitter = YamlEmitter::new(&mut out);
        emitter.dump(&Yaml::Hash(hash)).unwrap();
        serde_yaml::from_str(&out)?
    };
    let node_ip = yaml["node_ip"]
        .as_str()
        .ok_or_else(|| BootstrapError::YamlRust("`node_ip` missing".into()))?
        .into();

    let node_port = yaml["node_port"]
        .as_str()
        .ok_or_else(|| BootstrapError::YamlRust("`node_port` missing".into()))?
        .into();

    let node_api_key = yaml["node_api_key"]
        .as_str()
        .ok_or_else(|| BootstrapError::YamlRust("`node_api_key` missing".into()))?
        .into();

    Ok(BootstrapConfig {
        refresh_contract_parameters,
        pool_contract_parameters,
        update_contract_parameters,
        tokens_to_mint,
        node_ip,
        node_port,
        node_api_key,
        is_mainnet,
        addresses,
    })
}

/// An instance of this struct is created from an operator-provided YAML file. Note that we don't
/// derive `Deserialize` here since we need to verify the address types against the `is_mainnet`
/// field.
#[derive(Clone)]
pub struct BootstrapConfig {
    pub refresh_contract_parameters: BootstrapRefreshContractParameters,
    pub pool_contract_parameters: BootstrapPoolContractParameters,
    pub update_contract_parameters: UpdateContractParameters,
    pub tokens_to_mint: TokensToMint,
    pub node_ip: String,
    pub node_port: String,
    pub node_api_key: String,
    pub is_mainnet: bool,
    pub addresses: Addresses,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(
    try_from = "BootstrapPoolContractParametersYaml",
    into = "BootstrapPoolContractParametersYaml"
)]
/// Parameters for the pool contract that are needed for oracle bootstrap.
pub struct BootstrapPoolContractParameters {
    pub p2s: NetworkAddress,
    pub refresh_nft_index: usize,
    pub update_nft_index: usize,
}

/// Used to (de)serialize `BootstrapPoolContractParameters` instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BootstrapPoolContractParametersYaml {
    p2s: String,
    on_mainnet: bool,
    pub refresh_nft_index: usize,
    pub update_nft_index: usize,
}

impl TryFrom<BootstrapPoolContractParametersYaml> for BootstrapPoolContractParameters {
    type Error = AddressEncoderError;

    fn try_from(p: BootstrapPoolContractParametersYaml) -> Result<Self, Self::Error> {
        let prefix = if p.on_mainnet {
            NetworkPrefix::Mainnet
        } else {
            NetworkPrefix::Testnet
        };
        let address = AddressEncoder::new(prefix).parse_address_from_str(&p.p2s)?;
        Ok(BootstrapPoolContractParameters {
            p2s: NetworkAddress::new(prefix, &address),
            refresh_nft_index: p.refresh_nft_index,
            update_nft_index: p.update_nft_index,
        })
    }
}

impl From<BootstrapPoolContractParameters> for BootstrapPoolContractParametersYaml {
    fn from(val: BootstrapPoolContractParameters) -> Self {
        BootstrapPoolContractParametersYaml {
            p2s: val.p2s.to_base58(),
            on_mainnet: val.p2s.network() == NetworkPrefix::Mainnet,
            refresh_nft_index: val.refresh_nft_index,
            update_nft_index: val.update_nft_index,
        }
    }
}
#[derive(Clone)]
pub struct Addresses {
    pub address_for_oracle_tokens: Address,
    pub wallet_address_for_chain_transaction: Address,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct TokensToMint {
    pub pool_nft: NftMintDetails,
    pub refresh_nft: NftMintDetails,
    pub update_nft: NftMintDetails,
    pub oracle_tokens: TokenMintDetails,
    pub ballot_tokens: TokenMintDetails,
    pub reward_tokens: TokenMintDetails,
}

#[derive(Deserialize, Serialize, Clone)]
#[serde(
    try_from = "BootstrapRefreshContractParametersYaml",
    into = "BootstrapRefreshContractParametersYaml"
)]
pub struct BootstrapRefreshContractParameters {
    pub p2s: NetworkAddress,
    pub pool_nft_index: usize,
    pub oracle_token_id_index: usize,
    pub min_data_points_index: usize,
    pub min_data_points: u64,
    pub buffer_index: usize,
    pub buffer_length: u64,
    pub max_deviation_percent_index: usize,
    pub max_deviation_percent: u64,
    pub epoch_length_index: usize,
    pub epoch_length: u64,
    pub min_votes: u32,
    pub total_oracles: u32,
    pub total_ballots: u32,
}

/// Used to (de)serialize `BootstrapRefreshContractParameters` instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BootstrapRefreshContractParametersYaml {
    p2s: String,
    on_mainnet: bool,
    pool_nft_index: usize,
    oracle_token_id_index: usize,
    min_data_points_index: usize,
    min_data_points: u64,
    buffer_index: usize,
    buffer_length: u64,
    max_deviation_percent_index: usize,
    max_deviation_percent: u64,
    epoch_length_index: usize,
    epoch_length: u64,
    min_votes: u32,
    total_oracles: u32,
    total_ballots: u32,
}

impl TryFrom<BootstrapRefreshContractParametersYaml> for BootstrapRefreshContractParameters {
    type Error = AddressEncoderError;

    fn try_from(p: BootstrapRefreshContractParametersYaml) -> Result<Self, Self::Error> {
        let prefix = if p.on_mainnet {
            NetworkPrefix::Mainnet
        } else {
            NetworkPrefix::Testnet
        };
        let address = AddressEncoder::new(prefix).parse_address_from_str(&p.p2s)?;
        Ok(BootstrapRefreshContractParameters {
            p2s: NetworkAddress::new(prefix, &address),
            pool_nft_index: p.pool_nft_index,
            oracle_token_id_index: p.oracle_token_id_index,
            min_data_points_index: p.min_data_points_index,
            min_data_points: p.min_data_points,
            buffer_index: p.buffer_index,
            buffer_length: p.buffer_length,
            max_deviation_percent_index: p.max_deviation_percent_index,
            max_deviation_percent: p.max_deviation_percent,
            epoch_length_index: p.epoch_length_index,
            epoch_length: p.epoch_length,
            min_votes: p.min_votes,
            total_oracles: p.total_oracles,
            total_ballots: p.total_ballots,
        })
    }
}

impl From<BootstrapRefreshContractParameters> for BootstrapRefreshContractParametersYaml {
    fn from(p: BootstrapRefreshContractParameters) -> Self {
        BootstrapRefreshContractParametersYaml {
            p2s: p.p2s.to_base58(),
            on_mainnet: p.p2s.network() == NetworkPrefix::Mainnet,
            pool_nft_index: p.pool_nft_index,
            oracle_token_id_index: p.oracle_token_id_index,
            min_data_points_index: p.min_data_points_index,
            min_data_points: p.min_data_points,
            buffer_index: p.buffer_index,
            buffer_length: p.buffer_length,
            max_deviation_percent_index: p.max_deviation_percent_index,
            max_deviation_percent: p.max_deviation_percent,
            epoch_length_index: p.epoch_length_index,
            epoch_length: p.epoch_length,
            min_votes: p.min_votes,
            total_oracles: p.total_oracles,
            total_ballots: p.total_ballots,
        }
    }
}
#[derive(Deserialize, Serialize, Clone)]
pub struct TokenMintDetails {
    pub name: String,
    pub description: String,
    pub quantity: u64,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct NftMintDetails {
    pub name: String,
    pub description: String,
}

#[derive(Serialize)]
pub struct OracleConfigFields {
    #[serde(serialize_with = "token_id_as_base64_string")]
    pub pool_nft: TokenId,
    #[serde(serialize_with = "token_id_as_base64_string")]
    pub refresh_nft: TokenId,
    #[serde(serialize_with = "token_id_as_base64_string")]
    pub update_nft: TokenId,
    #[serde(serialize_with = "token_id_as_base64_string")]
    pub oracle_token: TokenId,
    #[serde(serialize_with = "token_id_as_base64_string")]
    pub ballot_token: TokenId,
    #[serde(serialize_with = "token_id_as_base64_string")]
    pub reward_token: TokenId,
    pub node_ip: String,
    pub node_port: String,
    pub node_api_key: String,
}

#[derive(Debug, Error, From)]
pub enum BootstrapError {
    #[error("tx builder error: {0}")]
    TxBuilder(TxBuilderError),
    #[error("box builder error: {0}")]
    ErgoBoxCandidateBuilder(ErgoBoxCandidateBuilderError),
    #[error("node error: {0}")]
    Node(NodeError),
    #[error("box selector error: {0}")]
    BoxSelector(BoxSelectorError),
    #[error("box value error: {0}")]
    BoxValue(BoxValueError),
    #[error("IO error: {0}")]
    Io(std::io::Error),
    #[error("serde-yaml error: {0}")]
    SerdeYaml(serde_yaml::Error),
    #[error("yaml-rust error: {0}")]
    YamlRust(String),
    #[error("AddressEncoder error: {0}")]
    AddressEncoder(AddressEncoderError),
    #[error("SigmaParsing error: {0}")]
    SigmaParse(SigmaParsingError),
    #[error("Node doesn't have a change address set")]
    NoChangeAddressSetInNode,
    #[error("Node doesn't have a change address set")]
    RefreshContract(RefreshContractError),
    #[error("Update contract error: {0}")]
    UpdateContract(UpdateContractError),
}

fn token_id_as_base64_string<S>(value: &TokenId, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let bytes: Vec<u8> = value.clone().into();
    serializer.serialize_str(&base64::encode(bytes))
}

#[cfg(test)]
mod tests {
    use ergo_lib::{
        chain::{ergo_state_context::ErgoStateContext, transaction::TxId},
        ergotree_interpreter::sigma_protocol::private_input::DlogProverInput,
        ergotree_ir::chain::{
            address::AddressEncoder,
            ergo_box::{ErgoBox, NonMandatoryRegisters},
        },
        wallet::Wallet,
    };
    use sigma_test_util::force_any_val;

    use super::*;
    use crate::pool_commands::test_utils::{
        make_refresh_contract_parameters, make_update_contract_parameters, LocalTxSigner,
        WalletDataMock,
    };
    use std::cell::RefCell;
    #[derive(Default)]
    struct SubmitTxMock {
        transactions: RefCell<Vec<ergo_lib::chain::transaction::Transaction>>,
    }

    impl SubmitTransaction for SubmitTxMock {
        fn submit_transaction(
            &self,
            tx: &ergo_lib::chain::transaction::Transaction,
        ) -> crate::node_interface::Result<String> {
            self.transactions.borrow_mut().push(tx.clone());
            // Return empty string as TxId
            Ok("".into())
        }
    }

    #[test]
    fn test_bootstrap() {
        let ctx = force_any_val::<ErgoStateContext>();
        let height = ctx.pre_header.height;
        let secret = force_any_val::<DlogProverInput>();
        let address = Address::P2Pk(secret.public_image());
        let is_mainnet = address.content_bytes()[0] < NetworkPrefix::Testnet as u8;
        let wallet = Wallet::from_secrets(vec![secret.clone().into()]);
        let ergo_tree = address.script().unwrap();

        let value = BoxValue::SAFE_USER_MIN.checked_mul_u32(10000).unwrap();
        let unspent_boxes = vec![ErgoBox::new(
            value,
            ergo_tree.clone(),
            None,
            NonMandatoryRegisters::empty(),
            height - 9,
            force_any_val::<TxId>(),
            0,
        )
        .unwrap()];
        let change_address =
            AddressEncoder::new(ergo_lib::ergotree_ir::chain::address::NetworkPrefix::Mainnet)
                .parse_address_from_str("9iHyKxXs2ZNLMp9N9gbUT9V8gTbsV7HED1C1VhttMfBUMPDyF7r")
                .unwrap();

        let network_prefix = if is_mainnet {
            NetworkPrefix::Mainnet
        } else {
            NetworkPrefix::Testnet
        };
        let p2s = NetworkAddress::new(
            network_prefix,
            &AddressEncoder::new(network_prefix)
                .parse_address_from_str("PViBL5acX6PoP6BQPsYtyNzW9aPXwxpRaUkXo4nE7RkxcBbZXJECUEBQm4g3MQCb2QsQALqPkrDN9TvsKuQkChF8sZSfnH5fifgKAkXhW8ifAcAE1qA67n9mabB3Mb2R8xT2v3SN49eN8mQ8HN95")
                .unwrap(),
        );
        let refresh_params = make_refresh_contract_parameters();
        let state = BootstrapConfig {
            tokens_to_mint: TokensToMint {
                pool_nft: NftMintDetails {
                    name: "pool NFT".into(),
                    description: "Pool NFT".into(),
                },
                refresh_nft: NftMintDetails {
                    name: "refresh NFT".into(),
                    description: "refresh NFT".into(),
                },
                update_nft: NftMintDetails {
                    name: "update NFT".into(),
                    description: "update NFT".into(),
                },
                oracle_tokens: TokenMintDetails {
                    name: "oracle token".into(),
                    description: "oracle token".into(),
                    quantity: 15,
                },
                ballot_tokens: TokenMintDetails {
                    name: "ballot token".into(),
                    description: "ballot token".into(),
                    quantity: 15,
                },
                reward_tokens: TokenMintDetails {
                    name: "reward token".into(),
                    description: "reward token".into(),
                    quantity: 100_000_000,
                },
            },
            refresh_contract_parameters: BootstrapRefreshContractParameters {
                p2s: refresh_params.p2s,
                epoch_length_index: refresh_params.epoch_length_index,
                epoch_length: refresh_params.epoch_length,
                buffer_index: refresh_params.buffer_index,
                buffer_length: refresh_params.buffer_length,
                min_data_points_index: refresh_params.min_data_points_index,
                min_data_points: refresh_params.min_data_points,
                max_deviation_percent_index: refresh_params.max_deviation_percent_index,
                max_deviation_percent: refresh_params.max_deviation_percent,
                pool_nft_index: refresh_params.pool_nft_index,
                oracle_token_id_index: refresh_params.oracle_token_id_index,
                total_oracles: 15,
                total_ballots: 15,
                min_votes: 6,
            },
            pool_contract_parameters: BootstrapPoolContractParameters {
                p2s,
                refresh_nft_index: 2,
                update_nft_index: 3,
            },
            update_contract_parameters: make_update_contract_parameters(),
            addresses: Addresses {
                address_for_oracle_tokens: address.clone(),
                wallet_address_for_chain_transaction: address.clone(),
            },
            node_ip: "127.0.0.1".into(),
            node_port: "9053".into(),
            node_api_key: "hello".into(),
            is_mainnet,
        };

        let height = ctx.pre_header.height;
        let submit_tx = SubmitTxMock::default();
        let oracle_config = perform_bootstrap_chained_transaction(BootstrapInput {
            config: state.clone(),
            wallet: &WalletDataMock {
                unspent_boxes: unspent_boxes.clone(),
            },
            tx_signer: &mut LocalTxSigner {
                ctx: &ctx,
                wallet: &wallet,
            },
            submit_tx: &submit_tx,
            tx_fee: BoxValue::SAFE_USER_MIN,
            erg_value_per_box: BoxValue::SAFE_USER_MIN,
            change_address,
            height,
        })
        .unwrap();

        // Find output box guarding the Update NFT
        let txs = submit_tx.transactions.borrow();
        let update_nft_box = txs
            .iter()
            .flat_map(|tx| tx.outputs.iter())
            .find(|output| {
                output
                    .tokens
                    .clone()
                    .into_iter()
                    .flatten()
                    .any(|token| token.token_id == oracle_config.update_nft)
            })
            .unwrap();
        // Check that Update NFT is guarded by UpdateContract, and parameters are correct

        let token_ids = TokenIds {
            pool_nft_token_id: oracle_config.pool_nft.clone(),
            refresh_nft_token_id: oracle_config.refresh_nft.clone(),
            update_nft_token_id: oracle_config.update_nft.clone(),
            oracle_token_id: oracle_config.oracle_token.clone(),
            reward_token_id: oracle_config.reward_token.clone(),
            ballot_token_id: oracle_config.ballot_token.clone(),
        };
        let parameters = make_update_contract_parameters();

        let update_contract = crate::contracts::update::UpdateContract::from_ergo_tree(
            update_nft_box.ergo_tree.clone(),
            &parameters,
            &token_ids,
        )
        .unwrap();
        assert!(update_contract.min_votes() as u32 == state.refresh_contract_parameters.min_votes);
        assert!(update_contract.pool_nft_token_id() == oracle_config.pool_nft);
        assert!(update_contract.ballot_token_id() == oracle_config.ballot_token);
        let s = serde_yaml::to_string(&oracle_config).unwrap();
        println!("{}", s);

        // Quickly check an encoding
        let bytes: Vec<u8> = oracle_config.ballot_token.clone().into();
        let encoded = base64::encode(bytes);
        let ballot_id = TokenId::from_base64(&encoded).unwrap();
        assert_eq!(oracle_config.ballot_token, ballot_id);
    }
}
