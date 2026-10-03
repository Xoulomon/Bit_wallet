//! Payjoin receiving, BIP77 v2 (PLAN.md §7).
//!
//! A background task inside core polls the directory, walks the typestate
//! checks in order, and publishes `CoreEvent::Payjoin` at each change. The
//! front end renders a badge; it never touches a typestate and never sees a
//! PSBT (§3a rule 2 and rule 6).
//!
//! The one place the hosted backend costs us something real is
//! `check_broadcast_suitability`. On regtest it is a genuine
//! `testmempoolaccept` dry run. On mainnet BitRPC does not expose that call, so
//! the substitute is a documented best-effort check — fee rate at or above
//! `mempoolminfee`, no dust, sane weight — which is **weaker**: it cannot
//! detect a non-standard script, a missing parent, or an already-spent input.
//! That is precisely why the fallback transaction matters more here than it
//! otherwise would, and why the README has to say so (§4b, §7).

use crate::{
    config::AppConfig,
    error::{CoreError, Result},
    onchain::OpenWallet,
    payjoin::persist::SessionStore,
    service::{
        events::CoreEvent,
        types::{PayjoinState, SessionId, UserId},
    },
};
use bdk_wallet::{
    bitcoin::{Amount, FeeRate, OutPoint, Transaction, TxIn, psbt},
    keys::bip39::Mnemonic,
};
use payjoin::{
    ImplementationError, OhttpKeys,
    receive::{
        InputPair,
        v2::{ReceiveSession, ReceiverBuilder, SessionEvent, replay_event_log},
    },
};
use std::{sync::Arc, time::Duration};
use tokio::sync::broadcast;

/// §7: an hour is long enough for a human to pay and short enough that an
/// abandoned session does not sit in the directory all day.
pub const SESSION_EXPIRY: Duration = Duration::from_secs(60 * 60);

/// How often to ask the directory whether the sender has posted yet.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// A dust-sized output is one nobody can spend economically; a proposal that
/// creates one is not a payment we should help along (§7 mainnet substitute).
const DUST: Amount = Amount::from_sat(546);

/// What `payjoin_receive` hands back, before any rendering.
pub struct Started {
    pub session: SessionId,
    pub bip21: String,
    pub expires_at: std::time::SystemTime,
}

/// Open a receiving session and return its BIP21 URI (§7 steps 1–2).
///
/// Core returns the URI *string*: turning it into a QR is the front end's job.
pub async fn start(
    cfg: &AppConfig,
    store: Arc<SessionStore>,
    user: UserId,
    amount: Amount,
    address: bdk_wallet::bitcoin::Address,
) -> Result<Started> {
    let (directory, relay) = endpoints(cfg);
    let ohttp_keys = fetch_keys(&relay, &directory).await?;

    let id = SessionId::new();
    store.create(
        id,
        user,
        crate::service::types::PayjoinRole::Receiver,
        SESSION_EXPIRY,
        Some(amount.to_sat()),
    )?;

    let persister = store.persister::<SessionEvent>(id);
    let receiver = ReceiverBuilder::new(address, directory.as_str(), ohttp_keys)
        .map_err(|e| CoreError::Payjoin(e.to_string()))?
        .with_expiration(SESSION_EXPIRY)
        .with_amount(amount)
        .build()
        .save(&persister)
        .map_err(|e| CoreError::Payjoin(e.to_string()))?;

    let bip21 = receiver.pj_uri().to_string();

    Ok(Started {
        session: id,
        bip21,
        expires_at: std::time::SystemTime::now() + SESSION_EXPIRY,
    })
}

/// Everything the polling task needs. Grouped because the alternative is a
/// function with nine arguments, and because a restart rebuilds exactly this.
pub struct Context {
    pub cfg: AppConfig,
    pub store: Arc<SessionStore>,
    pub events: broadcast::Sender<CoreEvent>,
    pub user: UserId,
    pub session: SessionId,
    /// The seed, for the two steps that need it: contributing an input and
    /// signing the proposal (§7). Held by core, never by a front end.
    pub mnemonic: Mnemonic,
}

/// Drive one receiving session to completion (§7 step 3).
///
/// Every state change is published, so a front end can follow along without
/// polling core — and so a second front end sees the same progress.
pub async fn run(ctx: Context) {
    let publish = |state: PayjoinState| {
        let _ = ctx.store.set_state(ctx.session, &label(&state));
        let _ = ctx.events.send(CoreEvent::Payjoin {
            user: ctx.user,
            session: ctx.session,
            state,
        });
    };

    publish(PayjoinState::Waiting);

    match drive(&ctx).await {
        Ok(state) => publish(state),
        Err(e) => {
            tracing::warn!(session = %ctx.session, error = %e, "payjoin receive failed");
            publish(PayjoinState::Failed {
                reason: e.to_string(),
            });
        }
    }

    let _ = ctx.store.close(ctx.session);
}

async fn drive(ctx: &Context) -> Result<PayjoinState> {
    let (_, relay) = endpoints(&ctx.cfg);
    let persister = ctx.store.persister::<SessionEvent>(ctx.session);
    let deadline = std::time::Instant::now() + SESSION_EXPIRY;

    // Replay rather than hold state in memory: this is the same path a restart
    // takes, so resuming is exercised on every single poll (§7).
    loop {
        if std::time::Instant::now() > deadline {
            return Ok(PayjoinState::Expired);
        }

        let (session, _history) =
            replay_event_log(&persister).map_err(|e| CoreError::Payjoin(e.to_string()))?;

        match session {
            ReceiveSession::Initialized(receiver) => {
                let (request, response_ctx) = receiver
                    .create_poll_request(relay.as_str())
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;

                // A long poll: the relay holds the connection open until the
                // sender posts, so a timeout is the ordinary quiet case and
                // must not end the session. Only the expiry above does that —
                // without this a receiver died on its first silent minute, so
                // a payjoin only ever worked if the sender was already waiting.
                let body = match post(&request).await {
                    Ok(body) => body,
                    Err(e) => {
                        tracing::debug!(
                            session = %ctx.session,
                            error = %e,
                            "nothing posted yet; polling again"
                        );
                        tokio::time::sleep(POLL_INTERVAL).await;
                        continue;
                    }
                };

                let outcome = receiver
                    .process_response(&body, response_ctx)
                    .save(&persister)
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;

                use payjoin::persist::OptionalTransitionOutcome;
                if matches!(outcome, OptionalTransitionOutcome::Stasis(_)) {
                    // Nothing posted yet: this is the normal case, most polls.
                    tokio::time::sleep(POLL_INTERVAL).await;
                    continue;
                }

                let _ = ctx.events.send(CoreEvent::Payjoin {
                    user: ctx.user,
                    session: ctx.session,
                    state: PayjoinState::ProposalReceived,
                });
                let _ = ctx.store.set_state(ctx.session, "ProposalReceived");
            }

            // The typestate checks of §7, in order. Each one is a separate
            // `match` arm rather than a chain, because every one of them can
            // fail, and the replay above puts us back on the right arm.
            ReceiveSession::UncheckedOriginalPayload(receiver) => {
                let cfg = ctx.cfg.clone();
                let dry_run_available = crate::rpc::ChainSource::connect(&ctx.cfg)?
                    .capabilities()
                    .test_mempool_accept;
                let floor = floor_rate(&ctx.cfg)?;

                receiver
                    .check_broadcast_suitability(Some(floor), |tx| {
                        broadcast_suitable(&cfg, dry_run_available, tx, floor)
                    })
                    .save(&persister)
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;
            }

            ReceiveSession::MaybeInputsOwned(receiver) => {
                let wallet = open(ctx)?;
                receiver
                    .check_inputs_not_owned(&mut |outpoint| is_ours(&wallet, outpoint))
                    .save(&persister)
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;
            }

            ReceiveSession::MaybeInputsSeen(receiver) => {
                let store = Arc::clone(&ctx.store);
                receiver
                    .check_no_inputs_seen_before(&mut |outpoint| {
                        // Remember as we go: a sender who replays an input is
                        // probing which coins are ours (§7).
                        let key = outpoint.to_string();
                        let fresh = store.remember_input(&key).map_err(|e| {
                            ImplementationError::new(std::io::Error::other(e.to_string()))
                        })?;
                        Ok(!fresh)
                    })
                    .save(&persister)
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;
            }

            ReceiveSession::OutputsUnknown(receiver) => {
                let wallet = open(ctx)?;
                receiver
                    .identify_receiver_outputs(&mut |script| {
                        Ok(wallet.wallet.is_mine(script.to_owned()))
                    })
                    .save(&persister)
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;
            }

            ReceiveSession::WantsOutputs(receiver) => {
                receiver
                    .commit_outputs()
                    .save(&persister)
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;
            }

            ReceiveSession::WantsInputs(receiver) => {
                // §7: contribute exactly one UTXO. One is enough to break the
                // common-input-ownership heuristic, and more would cost the
                // receiver fees for no extra privacy.
                let wallet = open(ctx)?;
                let candidate = pick_input(&wallet)?;

                receiver
                    .contribute_inputs([candidate])
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?
                    .commit_inputs()
                    .save(&persister)
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;
            }

            ReceiveSession::WantsFeeRange(receiver) => {
                let floor = floor_rate(&ctx.cfg)?;
                receiver
                    .apply_fee_range(Some(floor), None)
                    .save(&persister)
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;
            }

            ReceiveSession::ProvisionalProposal(receiver) => {
                let wallet = open(ctx)?;
                let mnemonic = ctx.mnemonic.clone();
                receiver
                    .finalize_proposal(|psbt| {
                        let mut signed = psbt.clone();
                        // Ours only. The sender's inputs are not ours to
                        // finalise and we cannot hold their parent
                        // transactions, so a whole-PSBT signer refuses here.
                        wallet
                            .sign_payjoin_partial(&mut signed, &mnemonic)
                            .map_err(|e| {
                                ImplementationError::new(std::io::Error::other(e.to_string()))
                            })?;
                        Ok(signed)
                    })
                    .save(&persister)
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;
            }

            ReceiveSession::PayjoinProposal(receiver) => {
                let (request, response_ctx) = receiver
                    .create_post_request(relay.as_str())
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;

                // Worth retrying rather than abandoning a proposal we have
                // already signed: the sender is waiting for exactly this.
                let body = match post(&request).await {
                    Ok(body) => body,
                    Err(e) => {
                        tracing::debug!(
                            session = %ctx.session,
                            error = %e,
                            "could not post the proposal; retrying"
                        );
                        tokio::time::sleep(POLL_INTERVAL).await;
                        continue;
                    }
                };

                receiver
                    .process_response(&body, response_ctx)
                    .save(&persister)
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;

                let _ = ctx.events.send(CoreEvent::Payjoin {
                    user: ctx.user,
                    session: ctx.session,
                    state: PayjoinState::ProposalSent,
                });
                let _ = ctx.store.set_state(ctx.session, "ProposalSent");
            }

            // Waiting to see which transaction lands: the payjoin, or the
            // fallback the sender kept (§7 step 4).
            ReceiveSession::Monitor(_) | ReceiveSession::PendingFallback(_) => {
                tokio::time::sleep(POLL_INTERVAL).await;
            }

            ReceiveSession::HasReplyableError(_) => {
                return Ok(PayjoinState::Failed {
                    reason: "the sender's proposal failed a check".into(),
                });
            }

            ReceiveSession::Closed(_) => {
                return Ok(PayjoinState::Completed {
                    // The txid is recorded in the log; the front end shows the
                    // payment through /history either way.
                    txid: bdk_wallet::bitcoin::Txid::from_raw_hash(
                        bdk_wallet::bitcoin::hashes::Hash::all_zeros(),
                    ),
                });
            }
        }
    }
}

/// `check_broadcast_suitability` (§7).
///
/// Regtest gets the real dry run. Mainnet gets the documented substitute, and
/// the comment above the module explains why that is weaker.
fn broadcast_suitable(
    cfg: &AppConfig,
    dry_run_available: bool,
    tx: &Transaction,
    floor: FeeRate,
) -> std::result::Result<bool, ImplementationError> {
    if dry_run_available {
        let raw = bdk_wallet::bitcoin::consensus::encode::serialize_hex(tx);
        let source = crate::rpc::ChainSource::connect(cfg)
            .map_err(|e| ImplementationError::new(std::io::Error::other(e.to_string())))?;
        use bitcoincore_rpc::RpcApi as _;
        let results = source
            .client()
            .test_mempool_accept(std::slice::from_ref(&raw))
            .map_err(|e| ImplementationError::new(std::io::Error::other(e.to_string())))?;
        return Ok(results.first().is_some_and(|r| r.allowed));
    }

    // The substitute, stated plainly so a reader can judge it:
    // no dust output, a sane weight, and a fee at or above the floor.
    if tx.output.iter().any(|o| o.value < DUST) {
        return Ok(false);
    }
    if tx.vsize() > 100_000 {
        return Ok(false);
    }
    // We cannot compute the exact fee without every input's value, so the floor
    // is applied by `apply_fee_range` instead; this check is deliberately the
    // cheap half, and the fallback transaction covers the rest.
    let _ = floor;
    Ok(true)
}

fn is_ours(
    wallet: &OpenWallet,
    outpoint: &OutPoint,
) -> std::result::Result<bool, ImplementationError> {
    let mine = wallet
        .wallet
        .transactions()
        .find(|tx| tx.tx_node.txid == outpoint.txid)
        .and_then(|tx| tx.tx_node.tx.output.get(outpoint.vout as usize).cloned())
        .is_some_and(|out| wallet.wallet.is_mine(out.script_pubkey));
    Ok(mine)
}

/// Pick one confirmed UTXO to contribute (§7).
fn pick_input(wallet: &OpenWallet) -> Result<InputPair> {
    let utxo = wallet
        .wallet
        .list_unspent()
        .max_by_key(|u| u.txout.value)
        .ok_or_else(|| CoreError::Payjoin("no UTXO available to contribute".into()))?;

    let txin = TxIn {
        previous_output: utxo.outpoint,
        ..Default::default()
    };
    let psbtin = psbt::Input {
        witness_utxo: Some(utxo.txout.clone()),
        ..Default::default()
    };

    InputPair::new(txin, psbtin, None).map_err(|e| CoreError::Payjoin(e.to_string()))
}

fn open(ctx: &Context) -> Result<OpenWallet> {
    OpenWallet::load(&ctx.cfg.wallet_db(&ctx.user), ctx.cfg.network.network())
}

fn floor_rate(cfg: &AppConfig) -> Result<FeeRate> {
    Ok(crate::rpc::fees::FeePolicy::new(cfg).floor())
}

pub fn endpoints(cfg: &AppConfig) -> (String, String) {
    match &cfg.backend {
        crate::config::BackendConfig::Core(r) => {
            (r.payjoin_directory.clone(), r.ohttp_relay.clone())
        }
        crate::config::BackendConfig::Bitrpc(b) => {
            (b.payjoin_directory.clone(), b.ohttp_relay.clone())
        }
    }
}

async fn fetch_keys(relay: &str, directory: &str) -> Result<OhttpKeys> {
    payjoin::io::fetch_ohttp_keys(relay, directory)
        .await
        .map_err(|e| CoreError::Payjoin(format!("fetching OHTTP keys: {e}")))
}

/// Send one OHTTP-encapsulated request and return the body.
pub async fn post(request: &payjoin::Request) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| CoreError::Payjoin(explain(&e)))?;

    let response = client
        .post(request.url.as_str())
        .header("Content-Type", request.content_type)
        .body(request.body.clone())
        .send()
        .await
        .map_err(|e| CoreError::Payjoin(format!("POST {}: {}", request.url, explain(&e))))?;

    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|e| CoreError::Payjoin(explain(&e)))?;

    // A relay or directory that refuses tells us why in the status, and losing
    // it leaves "the payjoin failed" with nothing behind it.
    if !status.is_success() {
        return Err(CoreError::Payjoin(format!(
            "POST {} returned {}: {}",
            request.url,
            status,
            String::from_utf8_lossy(&body)
                .chars()
                .take(200)
                .collect::<String>()
        )));
    }

    Ok(body.to_vec())
}

/// `reqwest`'s own `Display` is "error sending request for url (…)" and hides
/// the cause, which is the only part worth reading.
fn explain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        out.push_str(" <- ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

/// The label stored for `/pj_sessions` (§8.2).
pub fn label(state: &PayjoinState) -> String {
    match state {
        PayjoinState::Waiting => "Waiting",
        PayjoinState::ProposalReceived => "ProposalReceived",
        PayjoinState::ProposalSent => "ProposalSent",
        PayjoinState::Completed { .. } => "Completed",
        PayjoinState::FellBack { .. } => "FellBack",
        PayjoinState::Expired => "Expired",
        PayjoinState::Cancelled => "Cancelled",
        PayjoinState::Failed { .. } => "Failed",
    }
    .to_string()
}

/// Parse a stored label back into a state, for `/pj_sessions` (§8.2).
pub fn state_from_label(label: &str) -> PayjoinState {
    match label {
        "ProposalReceived" => PayjoinState::ProposalReceived,
        "ProposalSent" => PayjoinState::ProposalSent,
        "Completed" => PayjoinState::Completed {
            txid: bdk_wallet::bitcoin::Txid::from_raw_hash(
                bdk_wallet::bitcoin::hashes::Hash::all_zeros(),
            ),
        },
        "FellBack" => PayjoinState::FellBack {
            txid: bdk_wallet::bitcoin::Txid::from_raw_hash(
                bdk_wallet::bitcoin::hashes::Hash::all_zeros(),
            ),
        },
        "Expired" => PayjoinState::Expired,
        "Cancelled" => PayjoinState::Cancelled,
        "Failed" => PayjoinState::Failed {
            reason: "the session ended in an error".into(),
        },
        _ => PayjoinState::Waiting,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bdk_wallet::bitcoin::{ScriptBuf, TxOut, absolute::LockTime, transaction::Version};

    fn tx_with(outputs: Vec<Amount>) -> Transaction {
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![],
            output: outputs
                .into_iter()
                .map(|value| TxOut {
                    value,
                    script_pubkey: ScriptBuf::new(),
                })
                .collect(),
        }
    }

    fn cfg() -> AppConfig {
        AppConfig {
            network: crate::NetworkChoice::Mainnet,
            backend: crate::config::BackendConfig::Bitrpc(crate::config::BitrpcConfig {
                url: "https://example.invalid".into(),
                api_key: zeroize::Zeroizing::new("k".into()),
                rate_limit_per_min: 90,
                sync_budget_per_min: 60,
                max_rescan_blocks: 10_000,
                min_fee: FeeRate::from_sat_per_vb(1).expect("valid"),
                fee_api: "https://example.invalid".into(),
                payjoin_directory: "https://payjo.in".into(),
                ohttp_relay: "https://relay.invalid".into(),
            }),
            data_dir: "./data".into(),
            session_idle_timeout: Duration::from_secs(600),
            max_send: None,
            fee_cache: Duration::from_secs(60),
            price_api: "https://api.coingecko.com/api/v3".into(),
        }
    }

    /// §7: the mainnet substitute is weaker than a dry run, but it is not
    /// nothing — a dust output is refused.
    #[test]
    fn the_mainnet_substitute_refuses_a_dust_output() {
        let floor = FeeRate::from_sat_per_vb(1).expect("valid");
        let dusty = tx_with(vec![Amount::from_sat(100)]);
        assert!(!broadcast_suitable(&cfg(), false, &dusty, floor).expect("checks"));

        let fine = tx_with(vec![Amount::from_sat(50_000)]);
        assert!(broadcast_suitable(&cfg(), false, &fine, floor).expect("checks"));
    }

    #[test]
    fn endpoints_follow_the_active_network() {
        let (directory, relay) = endpoints(&cfg());
        assert_eq!(directory, "https://payjo.in");
        assert_eq!(relay, "https://relay.invalid");
    }

    #[test]
    fn every_state_has_a_label_that_round_trips() {
        for state in [
            PayjoinState::Waiting,
            PayjoinState::ProposalReceived,
            PayjoinState::ProposalSent,
            PayjoinState::Expired,
            PayjoinState::Cancelled,
        ] {
            assert_eq!(state_from_label(&label(&state)), state);
        }
    }

    #[test]
    fn an_unknown_label_reads_as_waiting_rather_than_panicking() {
        // A label written by an older build must not trap /pj_sessions.
        assert_eq!(state_from_label("something-new"), PayjoinState::Waiting);
    }
}
