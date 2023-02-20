use crate::config::Config;
use bitcoincore_rpc::RpcApi;
use bitcoincore_rpc::{Auth, Client};
use chainhook_event_observer::chainhooks::bitcoin::{
    handle_bitcoin_hook_action, BitcoinChainhookOccurrence, BitcoinTriggerChainhook,
};
use chainhook_event_observer::chainhooks::types::BitcoinChainhookFullSpecification;
use chainhook_event_observer::indexer;
use chainhook_event_observer::utils::{file_append, send_request, Context};
use std::collections::HashMap;
use std::time::Duration;

pub async fn scan_bitcoin_chain_with_predicate(
    predicate: BitcoinChainhookFullSpecification,
    apply: bool,
    config: &Config,
    ctx: &Context,
) -> Result<(), String> {
    let auth = Auth::UserPass(
        config.network.bitcoin_node_rpc_username.clone(),
        config.network.bitcoin_node_rpc_password.clone(),
    );

    let bitcoin_rpc = match Client::new(&config.network.bitcoin_node_rpc_url, auth) {
        Ok(con) => con,
        Err(message) => {
            return Err(format!("Bitcoin RPC error: {}", message.to_string()));
        }
    };

    let predicate_uuid = predicate.uuid.clone();
    let predicate_spec =
        match predicate.into_selected_network_specification(&config.network.bitcoin_network) {
            Ok(predicate) => predicate,
            Err(e) => {
                return Err(format!(
                    "Specification missing for network {:?}: {e}",
                    config.network.bitcoin_network
                ));
            }
        };

    let start_block = match predicate_spec.start_block {
        Some(start_block) => start_block,
        None => {
            return Err(
                "Bitcoin chainhook specification must include a field start_block in replay mode"
                    .into(),
            );
        }
    };
    let tip_height = match bitcoin_rpc.get_blockchain_info() {
        Ok(result) => result.blocks,
        Err(e) => {
            return Err(format!(
                "unable to retrieve Bitcoin chain tip ({})",
                e.to_string()
            ));
        }
    };
    let end_block = predicate_spec.end_block.unwrap_or(tip_height);

    info!(
        ctx.expect_logger(),
        "Processing Bitcoin chainhook {}, will scan blocks [{}; {}] (apply = {})",
        predicate_uuid,
        start_block,
        end_block,
        apply
    );
    use reqwest::Client as HttpClient;

    let mut total_hits = vec![];
    for cursor in start_block..=end_block {
        debug!(
            ctx.expect_logger(),
            "Evaluating predicate #{} on block #{}", predicate_uuid, cursor
        );

        let body = json!({
            "jsonrpc": "1.0",
            "id": "chainhook-cli",
            "method": "getblockhash",
            "params": [cursor]
        });
        let http_client = HttpClient::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("Unable to build http client");
        let block_hash = http_client
            .post(&config.network.bitcoin_node_rpc_url)
            .basic_auth(
                &config.network.bitcoin_node_rpc_username,
                Some(&config.network.bitcoin_node_rpc_password),
            )
            .header("Content-Type", "application/json")
            .header("Host", &config.network.bitcoin_node_rpc_url[7..])
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("unable to send request ({})", e))?
            .json::<bitcoincore_rpc::jsonrpc::Response>()
            .await
            .map_err(|e| format!("unable to parse response ({})", e))?
            .result::<String>()
            .map_err(|e| format!("unable to parse response ({})", e))?;

        let body = json!({
            "jsonrpc": "1.0",
            "id": "chainhook-cli",
            "method": "getblock",
            "params": [block_hash, 2]
        });
        let http_client = HttpClient::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("Unable to build http client");
        let raw_block = http_client
            .post(&config.network.bitcoin_node_rpc_url)
            .basic_auth(
                &config.network.bitcoin_node_rpc_username,
                Some(&config.network.bitcoin_node_rpc_password),
            )
            .header("Content-Type", "application/json")
            .header("Host", &config.network.bitcoin_node_rpc_url[7..])
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("unable to send request ({})", e))?
            .json::<bitcoincore_rpc::jsonrpc::Response>()
            .await
            .map_err(|e| format!("unable to parse response ({})", e))?
            .result::<indexer::bitcoin::Block>()
            .map_err(|e| format!("unable to parse response ({})", e))?;

        let block =
            indexer::bitcoin::standardize_bitcoin_block(&config.network, cursor, raw_block, ctx)?;

        let mut hits = vec![];
        for tx in block.transactions.iter() {
            if predicate_spec.predicate.evaluate_transaction_predicate(&tx) {
                info!(
                    ctx.expect_logger(),
                    "Action #{} triggered by transaction {} (block #{})",
                    predicate_uuid,
                    tx.transaction_identifier.hash,
                    cursor
                );
                hits.push(tx);
                total_hits.push(tx.transaction_identifier.hash.to_string());
            }
        }

        if hits.len() > 0 {
            if apply {
                let trigger = BitcoinTriggerChainhook {
                    chainhook: &predicate_spec,
                    apply: vec![(hits, &block)],
                    rollback: vec![],
                };

                let proofs = HashMap::new();
                match handle_bitcoin_hook_action(trigger, &proofs) {
                    Err(e) => {
                        error!(ctx.expect_logger(), "unable to handle action {}", e);
                    }
                    Ok(BitcoinChainhookOccurrence::Http(request)) => {
                        send_request(request, &ctx).await;
                    }
                    Ok(BitcoinChainhookOccurrence::File(path, bytes)) => {
                        file_append(path, bytes, &ctx)
                    }
                    Ok(BitcoinChainhookOccurrence::Data(_payload)) => unreachable!(),
                }
            }
        }
    }
    // info!(ctx.expect_logger(), "Bitcoin chainhook {} scan completed and triggered by {} transactions {}", predicate.uuid, total_hits.len(), total_hits.join(","))

    Ok(())
}