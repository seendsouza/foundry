use crate::{
    cmd::ScriptSequence,
    utils::{get_http_provider, print_receipt},
};

use std::str::FromStr;

use ethers::{
    prelude::{k256::ecdsa::SigningKey, Http, Provider, SignerMiddleware, Wallet},
    providers::Middleware,
    signers::Signer,
    types::{transaction::eip2718::TypedTransaction, Address, Chain, TransactionReceipt},
};
use futures::future::join_all;

use super::*;

impl ScriptArgs {
    pub async fn send_transactions(
        &self,
        deployment_sequence: &mut ScriptSequence,
    ) -> eyre::Result<()> {
        // The user wants to actually send the transactions
        let mut local_wallets = vec![];
        if let Some(wallets) = self.wallets.private_keys()? {
            wallets.into_iter().for_each(|wallet| local_wallets.push(wallet));
        }

        if let Some(wallets) = self.wallets.interactives()? {
            wallets.into_iter().for_each(|wallet| local_wallets.push(wallet));
        }

        if let Some(wallets) = self.wallets.mnemonics()? {
            wallets.into_iter().for_each(|wallet| local_wallets.push(wallet));
        }

        if let Some(wallets) = self.wallets.keystores()? {
            wallets.into_iter().for_each(|wallet| local_wallets.push(wallet));
        }

        // TODO: Add trezor and ledger support (supported in multiwallet, just need to
        // add derivation + SignerMiddleware creation logic)
        // foundry/cli/src/opts/mod.rs:110
        if local_wallets.is_empty() {
            eyre::bail!("Error accessing local wallet when trying to send onchain transaction, did you set a private key, mnemonic or keystore?")
        }

        let fork_url = self
            .evm_opts
            .fork_url
            .as_ref()
            .expect("You must provide an RPC URL (see --fork-url).")
            .clone();

        let provider = get_http_provider(&fork_url);

        let chain = provider.get_chainid().await?.as_u64();
        let is_legacy =
            self.legacy || Chain::try_from(chain).map(|x| Chain::is_legacy(&x)).unwrap_or_default();
        local_wallets =
            local_wallets.into_iter().map(|wallet| wallet.with_chain_id(chain)).collect();

        // Iterate through transactions, matching the `from` field with the associated
        // wallet. Then send the transaction. Panics if we find a unknown `from`
        let transactions = deployment_sequence.transactions.clone();
        let sequence =
            transactions.into_iter().skip(deployment_sequence.receipts.len() as usize).map(|tx| {
                let from = *tx.from().expect("No sender for onchain transaction!");
                if let Some(wallet) =
                    local_wallets.iter().find(|wallet| (**wallet).address() == from)
                {
                    let signer = SignerMiddleware::new(provider.clone(), wallet.clone());
                    Ok((tx, signer))
                } else {
                    let mut err_msg = format!(
                        "No associated wallet for address: {:?}. Unlocked wallets: {:?}",
                        from,
                        local_wallets
                            .iter()
                            .map(|wallet| wallet.address())
                            .collect::<Vec<Address>>()
                    );

                    // This is an actual used address
                    if from == Address::from_str(Config::DEFAULT_SENDER).unwrap() {
                        err_msg += "\nYou seem to be using Foundry's default sender. Be sure to set your own --sender."
                    }

                    eyre::bail!(err_msg)
                }
            });

        let mut receipts = vec![];

        // We only wait for a transaction receipt before sending the next transaction, if there is
        // more than one signer. There would be no way of assuring their order otherwise.
        let wait = local_wallets.len() > 1;

        for payload in sequence {
            match payload {
                Ok((tx, signer)) => {
                    let receipt =
                        self.send_transaction(tx, signer, wait, chain, is_legacy, &fork_url);
                    if !wait {
                        receipts.push(receipt);
                    } else {
                        let (receipt, nonce) = receipt.await?;
                        print_receipt(&receipt, nonce)?;
                        deployment_sequence.add_receipt(receipt);
                    }
                }
                Err(e) => {
                    eyre::bail!("{e}");
                }
            }
        }

        self.wait_for_receipts(receipts, deployment_sequence).await?;

        println!("\n\n==========================");
        println!(
            "\nONCHAIN EXECUTION COMPLETE & SUCCESSFUL. Transaction receipts written to {:?}",
            deployment_sequence.path
        );
        Ok(())
    }

    pub async fn send_transaction(
        &self,
        tx: TypedTransaction,
        signer: SignerMiddleware<Provider<Http>, Wallet<SigningKey>>,
        wait: bool,
        chain: u64,
        is_legacy: bool,
        fork_url: &str,
    ) -> eyre::Result<(TransactionReceipt, U256)> {
        let mut legacy_or_1559 = if is_legacy {
            TypedTransaction::Legacy(tx.into())
        } else {
            TypedTransaction::Eip1559(tx.into())
        };
        legacy_or_1559.set_chain_id(chain);

        let from = *legacy_or_1559.from().expect("no sender");

        if wait {
            match foundry_utils::next_nonce(from, fork_url, None) {
                Ok(nonce) => {
                    let tx_nonce = *legacy_or_1559.nonce().expect("no nonce");

                    if nonce != tx_nonce {
                        eyre::bail!("EOA nonce changed unexpectedly while sending transactions.");
                    }
                }
                Err(_) => {
                    eyre::bail!("Not able to query the EOA nonce.");
                }
            }
        }

        async fn broadcast<T, U>(
            signer: SignerMiddleware<T, U>,
            legacy_or_1559: TypedTransaction,
        ) -> eyre::Result<Option<TransactionReceipt>>
        where
            SignerMiddleware<T, U>: Middleware,
        {
            tracing::debug!("sending transaction: {:?}", legacy_or_1559);
            match signer.send_transaction(legacy_or_1559, None).await {
                Ok(pending) => pending.await.map_err(|e| eyre::eyre!(e)),
                Err(e) => Err(eyre::eyre!(e.to_string())),
            }
        }

        let nonce = *legacy_or_1559.nonce().expect("no nonce");
        let receipt = match broadcast(signer, legacy_or_1559).await {
            Ok(Some(res)) => (res, nonce),

            Ok(None) => {
                // todo what if it has been actually sent
                eyre::bail!("Failed to get transaction receipt?")
            }
            Err(e) => {
                eyre::bail!("Aborting! A transaction failed to send: {:#?}", e)
            }
        };

        Ok(receipt)
    }

    pub async fn handle_broadcastable_transactions(
        &self,
        target: &ArtifactId,
        result: ScriptResult,
        decoder: &mut CallTraceDecoder,
        script_config: &ScriptConfig,
    ) -> eyre::Result<()> {
        if let Some(txs) = result.transactions {
            if script_config.evm_opts.fork_url.is_some() {
                if let Ok(gas_filled_txs) =
                    self.execute_transactions(txs, script_config, decoder).await
                {
                    if !result.success {
                        eyre::bail!("\nSIMULATION FAILED");
                    } else {
                        let txs = gas_filled_txs;
                        let mut deployment_sequence =
                            ScriptSequence::new(txs, &self.sig, target, &script_config.config)?;

                        if self.broadcast {
                            self.send_transactions(&mut deployment_sequence).await?;
                        } else {
                            println!("\nSIMULATION COMPLETE. To broadcast these transactions, add --broadcast and wallet configuration(s) to the previous command. See forge script --help for more.");
                        }
                    }
                } else {
                    eyre::bail!("One or more transactions failed when simulating the on-chain version. Check the trace by re-running with `-vvv`")
                }
            } else {
                println!("\nIf you wish to simulate on-chain transactions pass a RPC URL.");
            }
        } else if self.broadcast {
            eyre::bail!("No onchain transactions generated in script");
        }
        Ok(())
    }

    async fn wait_for_receipts(
        &self,
        tasks: Vec<impl futures::Future<Output = eyre::Result<(TransactionReceipt, U256)>>>,

        deployment_sequence: &mut ScriptSequence,
    ) -> eyre::Result<()> {
        let res = join_all(tasks).await;

        let mut err = None;
        let mut receipts = vec![];

        for receipt in res {
            match receipt {
                Ok(v) => receipts.push(v),
                Err(e) => {
                    err = Some(e);
                    break
                }
            };
        }

        // Receipts may have arrived out of order
        receipts.sort_by(|a, b| a.1.cmp(&b.1));
        for (receipt, nonce) in receipts {
            print_receipt(&receipt, nonce)?;
            deployment_sequence.add_receipt(receipt);
        }

        if let Some(err) = err {
            Err(err)
        } else {
            Ok(())
        }
    }
}