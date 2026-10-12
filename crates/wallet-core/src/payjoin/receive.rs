//! Payjoin receiving, BIP77 v2 (PLAN.md §7).
//!
//! A background task inside core polls the directory, walks the typestate
//! checks in order, and publishes `CoreEvent::Payjoin` at each change. The
//! front end renders a badge; it never touches a typestate and never sees a
//! PSBT (§3a rule 2 and rule 6).
//!
//! The session does **not** end when the proposal is posted back. The payjoin
//! crate hands us a `Monitor` typestate at that point, and the only way out of
//! it is `check_for_transaction`, which asks the network which of the two
//! transactions it knows about actually settled: the payjoin, or the fallback
//! the sender kept. Until that question is answered the session has no result
//! to report — and skipping it is why a completed payjoin used to be shown to
//! the user as expired.
//!
//! That answer comes from the receiver's own wallet rather than from
//! `getrawtransaction`: both candidate transactions pay this receiver, so its
//! wallet can recognise either one, and nothing here depends on `txindex` at
//! the shared node (§4b). On a backend with no mempool the answer arrives with
//! the first block instead, which is why settlement gets a much longer deadline
//! than the directory session does.

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
    bitcoin::{Amount, FeeRate, OutPoint, Transaction, TxIn, Txid, psbt},
    keys::bip39::Mnemonic,
};
use payjoin::{
    ImplementationError, OhttpKeys,
    receive::{
        InputPair,
        v2::{ReceiveSession, ReceiverBuilder, SessionEvent, SessionOutcome, replay_event_log},
    },
};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::sync::{broadcast, watch};

/// §7: an hour is long enough for a human to pay and short enough that an
/// abandoned session does not sit in the directory all day.
pub const SESSION_EXPIRY: Duration = Duration::from_secs(60 * 60);

/// How long to keep asking which transaction settled the session.
///
/// Much longer than `SESSION_EXPIRY`, and deliberately so: once the proposal is
/// posted the directory is out of the picture and the remaining question is a
/// chain question. On a backend with no mempool (`Capabilities::BITRPC`) the
/// answer cannot arrive until a block does, so an hour here would abandon
/// almost every mainnet session one minute before it settled.
pub const SETTLE_DEADLINE: Duration = Duration::from_secs(24 * 60 * 60);

/// How often to ask the directory whether the sender has posted yet.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How often to ask the chain which transaction settled, where there is a
/// mempool to read. Slower than the directory poll because every check costs a
/// wallet sync.
const SETTLE_INTERVAL: Duration = Duration::from_secs(60);

/// The same, where there is not (`Capabilities::BITRPC`).
///
/// Nothing can change until a block does, so a minute's cadence would spend a
/// budget shared by every user of the bot 1 440 times a day to learn nothing
/// on all but a handful of those passes (§4b).
const SETTLE_INTERVAL_NO_MEMPOOL: Duration = Duration::from_secs(10 * 60);

/// How far above the fastest current estimate the receiver will go when paying
/// for its own contributed input.
///
/// Some headroom is needed or a rate that moved between quoting and proposing
/// would decline a perfectly good payjoin; too much headroom and a sender can
/// push the receiver's own money into miner fees.
const MAX_FEE_RATE_MULTIPLIER: u64 = 2;

/// Clamps for the computed cap, so a wild estimate cannot make the receiver
/// either absurdly generous or unable to accept anything at all.
const MIN_MAX_FEE_RATE: u64 = 5;
const MAX_MAX_FEE_RATE: u64 = 200;

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

    let cap = receiver_max_fee_rate(cfg);
    let persister = store.persister::<SessionEvent>(id);
    let receiver = ReceiverBuilder::new(address, directory.as_str(), ohttp_keys)
        .map_err(|e| CoreError::Payjoin(e.to_string()))?
        .with_expiration(SESSION_EXPIRY)
        .with_amount(amount)
        // Without this the builder's default is `FeeRate::BROADCAST_MIN`,
        // which says the receiver will pay essentially nothing toward its own
        // contributed input. Our own sender absorbs that cost, so the default
        // looks fine until a sender from another wallet declines to — and then
        // every proposal is refused as `FeeTooHigh` (§7).
        .with_max_fee_rate(cap)
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
    /// The seed, for the one step that needs it: signing the proposal (§7).
    /// Held by core, never by a front end.
    ///
    /// `None` is for a session resumed past that point. From `Monitor` onwards
    /// the only question left is which transaction settled, which is a chain
    /// lookup — so making this optional is what lets a restart resume those
    /// sessions straight away instead of waiting for their owner to unlock.
    /// `needs_seed` is the gate that keeps the two cases apart.
    pub mnemonic: Option<Mnemonic>,
    /// Goes true when `/pj_sessions` cancels this session.
    ///
    /// Without it `payjoin_cancel` only marked the row — the task kept polling
    /// for the full hour and then overwrote the user's `Cancelled` with
    /// `Expired`.
    pub cancel: watch::Receiver<bool>,
}

/// Drive one receiving session to completion (§7 step 3).
///
/// Every state change is published, so a front end can follow along without
/// polling core — and so a second front end sees the same progress.
pub async fn run(mut ctx: Context) {
    // A resumed session is already past `Waiting`, and announcing it again
    // would walk the badge backwards on every restart.
    if matches!(
        ctx.store.row(ctx.session).ok().flatten().map(|r| r.state),
        Some(ref s) if s == "Waiting"
    ) {
        let _ = ctx.events.send(CoreEvent::Payjoin {
            user: ctx.user,
            session: ctx.session,
            state: PayjoinState::Waiting,
        });
    }

    let state = match drive(&mut ctx).await {
        Ok(Some(state)) => state,
        // The service that owns this session is going away. Recording an
        // outcome now would invent one — and writing `Expired` over a healthy
        // session on every shutdown is exactly the sort of lie this change
        // exists to remove. Leave the row as it is for a restart to resume.
        Ok(None) => {
            tracing::info!(session = %ctx.session, "stopping; the session stays open");
            return;
        }
        Err(e) => {
            tracing::warn!(session = %ctx.session, error = %e, "payjoin receive failed");
            PayjoinState::Failed {
                reason: e.to_string(),
            }
        }
    };

    let _ = ctx
        .store
        .set_outcome(ctx.session, &label(&state), outcome_txid(&state));
    let _ = ctx.events.send(CoreEvent::Payjoin {
        user: ctx.user,
        session: ctx.session,
        state: state.clone(),
    });

    // `ProposalSent` is the one outcome that is not an ending: the proposal is
    // out and we simply ran out of patience waiting to see what settled.
    // Leaving the row open is what lets a restart pick the watch back up.
    if is_terminal(&state) {
        let _ = ctx.store.close(ctx.session);
    } else {
        tracing::info!(
            session = %ctx.session,
            "settlement still unknown; leaving the session open for a restart to resume"
        );
    }
}

/// Returns `None` when the service is shutting down, which is not an outcome.
async fn drive(ctx: &mut Context) -> Result<Option<PayjoinState>> {
    let (_, relay) = endpoints(&ctx.cfg);
    let persister = ctx.store.persister::<SessionEvent>(ctx.session);

    // Both deadlines are absolute, taken from the row rather than from this
    // moment. Measured from `now` instead, a resumed session would start its
    // whole budget over on every restart, so one that could never settle would
    // be watched for ever.
    let row = ctx
        .store
        .row(ctx.session)?
        .ok_or(CoreError::NoSuchSession)?;
    let directory_deadline = row.expires_at;
    let settle_deadline = row.created_at + SETTLE_DEADLINE;

    // Both of these reach the fee API or the node, and both are blocking, so
    // they are looked up once rather than on every pass of a loop that can
    // run for hours.
    let floor = floor_rate(&ctx.cfg)?;
    let cap = receiver_max_fee_rate(&ctx.cfg);

    let capabilities = crate::rpc::ChainSource::connect(&ctx.cfg)?.capabilities();
    let settle_interval = if capabilities.mempool {
        SETTLE_INTERVAL
    } else {
        SETTLE_INTERVAL_NO_MEMPOOL
    };

    // Why the last check refused, kept because `SessionOutcome::Aborted` does
    // not carry it and "the sender's proposal failed a check" is not a
    // diagnosis.
    let mut failure: Option<String> = None;

    // Replay rather than hold state in memory: this is the same path a restart
    // takes, so resuming is exercised on every single poll (§7).
    loop {
        match stop_reason(&ctx.cancel) {
            Some(Stop::Cancelled) => {
                tracing::info!(session = %ctx.session, "cancelled by the user");
                return Ok(Some(PayjoinState::Cancelled));
            }
            // `sleep_or_cancel` and `unless_cancelled` both return at once once
            // the sender is gone, so without this the loop would spin as fast
            // as it could until a deadline it could no longer reach.
            Some(Stop::ServiceGone) => return Ok(None),
            None => {}
        }

        let (session, history) =
            replay_event_log(&persister).map_err(|e| CoreError::Payjoin(e.to_string()))?;

        // Two clocks, because the two halves of a session wait on different
        // things: the first half waits on a human at the directory, the second
        // on a transaction reaching the chain.
        let settling = matches!(
            &session,
            ReceiveSession::Monitor(_) | ReceiveSession::PendingFallback(_)
        );
        if !matches!(&session, ReceiveSession::Closed(_)) {
            let deadline = if settling {
                settle_deadline
            } else {
                directory_deadline
            };
            if std::time::SystemTime::now() > deadline {
                return Ok(Some(if settling {
                    PayjoinState::ProposalSent
                } else {
                    PayjoinState::Expired
                }));
            }
        }

        tracing::debug!(
            session = %ctx.session,
            state = ?std::mem::discriminant(&session),
            "receiver state"
        );

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
                let body = match unless_cancelled(&mut ctx.cancel, post(&request)).await {
                    None => continue,
                    Some(Ok(body)) => body,
                    Some(Err(e)) => {
                        tracing::debug!(
                            session = %ctx.session,
                            error = %e,
                            "nothing posted yet; polling again"
                        );
                        sleep_or_cancel(&mut ctx.cancel, POLL_INTERVAL).await;
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
                    sleep_or_cancel(&mut ctx.cancel, POLL_INTERVAL).await;
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
            //
            // A refused check is *not* returned from here. The crate persists
            // a `GotReplyableError` on the way out, so replaying lands on
            // `HasReplyableError`, which is where the sender gets told — see
            // `advanced` and that arm below.
            ReceiveSession::UncheckedOriginalPayload(receiver) => {
                // Where the backend has `testmempoolaccept` this is a genuine
                // dry run. Where it does not, there is no honest way to answer
                // "is this broadcastable" — the check that used to run here
                // returned true for everything that was not dust, while its
                // own doc comment claimed to enforce a fee floor. The check
                // guards non-interactive receivers against probing, and a
                // `/pj_receive` URI is interactive by construction: the user
                // asked for it and handed it to one payer. So say that instead
                // of pretending to check. Cake Wallet makes the same call.
                let saved = if capabilities.test_mempool_accept {
                    let cfg = ctx.cfg.clone();
                    receiver
                        .check_broadcast_suitability(Some(floor), |tx| dry_run(&cfg, tx))
                        .save(&persister)
                        .map(|_| ())
                        .map_err(|e| (e.is_transient(), e.storage_error_ref().is_some(), e.to_string()))
                } else {
                    receiver
                        .assume_interactive_receiver()
                        .save(&persister)
                        .map(|_| ())
                        .map_err(|e| (false, true, e.to_string()))
                };

                if !advanced(ctx, "check_broadcast_suitability", saved, &mut failure).await? {
                    continue;
                }
            }

            ReceiveSession::MaybeInputsOwned(receiver) => {
                // Sync first. `owns_outpoint` answers from the keychain's
                // output index, so it can only recognise what the wallet has
                // actually seen — and this is the check whose wrong answer
                // lets a sender get us to co-sign our own coin.
                if let Some(Err(e)) = unless_cancelled(
                    &mut ctx.cancel,
                    crate::onchain::sync::sync_now(&ctx.cfg, ctx.user),
                )
                .await
                {
                    tracing::debug!(
                        session = %ctx.session,
                        error = %e,
                        "could not sync before the ownership check; retrying"
                    );
                    sleep_or_cancel(&mut ctx.cancel, POLL_INTERVAL).await;
                    continue;
                }

                let wallet = open(ctx)?;
                let saved = receiver
                    .check_inputs_not_owned(&mut |outpoint| is_ours(&wallet, outpoint))
                    .save(&persister)
                    .map(|_| ())
                    .map_err(classify);
                if !advanced(ctx, "check_inputs_not_owned", saved, &mut failure).await? {
                    continue;
                }
            }

            ReceiveSession::MaybeInputsSeen(receiver) => {
                let store = Arc::clone(&ctx.store);
                let saved = receiver
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
                    .map(|_| ())
                    .map_err(classify);
                if !advanced(ctx, "check_no_inputs_seen_before", saved, &mut failure).await? {
                    continue;
                }
            }

            ReceiveSession::OutputsUnknown(receiver) => {
                let wallet = open(ctx)?;
                let saved = receiver
                    .identify_receiver_outputs(&mut |script| {
                        Ok(wallet.wallet.is_mine(script.to_owned()))
                    })
                    .save(&persister)
                    .map(|_| ())
                    .map_err(classify);
                if !advanced(ctx, "identify_receiver_outputs", saved, &mut failure).await? {
                    continue;
                }
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
                //
                // *Which* one is a privacy decision, not a size decision. The
                // crate's `try_preserving_privacy` picks the candidate that
                // avoids Unnecessary Input Heuristic 2; always contributing the
                // largest UTXO, as this used to, is a deterministic fingerprint
                // that can mark the transaction as a payjoin — the opposite of
                // the point.
                let wallet = open(ctx)?;
                let candidates = candidate_inputs(&wallet)?;
                let chosen = receiver
                    .try_preserving_privacy(candidates.clone())
                    .unwrap_or_else(|e| {
                        tracing::debug!(
                            session = %ctx.session,
                            error = %e,
                            "no candidate avoids the unnecessary-input heuristic; taking the first"
                        );
                        candidates
                            .first()
                            .cloned()
                            .expect("candidate_inputs refuses an empty set")
                    });

                receiver
                    .contribute_inputs([chosen])
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?
                    .commit_inputs()
                    .save(&persister)
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;
            }

            ReceiveSession::WantsFeeRange(receiver) => {
                let saved = receiver
                    .apply_fee_range(Some(floor), Some(cap))
                    .save(&persister)
                    .map(|_| ())
                    .map_err(classify);
                if !advanced(ctx, "apply_fee_range", saved, &mut failure).await? {
                    continue;
                }
            }

            ReceiveSession::ProvisionalProposal(receiver) => {
                let wallet = open(ctx)?;
                // Only reachable with a seed: `needs_seed` reports any session
                // short of `Monitor` as needing one, so a session resumed
                // without it is never on this arm.
                let mnemonic = ctx
                    .mnemonic
                    .clone()
                    .ok_or_else(|| CoreError::Payjoin(
                        "the proposal needs signing, but this session was resumed without the seed"
                            .into(),
                    ))?;
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
                let body = match unless_cancelled(&mut ctx.cancel, post(&request)).await {
                    None => continue,
                    Some(Ok(body)) => body,
                    Some(Err(e)) => {
                        tracing::debug!(
                            session = %ctx.session,
                            error = %e,
                            "could not post the proposal; retrying"
                        );
                        sleep_or_cancel(&mut ctx.cancel, POLL_INTERVAL).await;
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

            // The proposal is out. Which transaction settles the session — the
            // payjoin or the fallback the sender kept — is now a question for
            // the chain, and `check_for_transaction` is the only way to ask it.
            // Sleeping here instead, as this used to, left every successful
            // receive to time out and report itself as expired (§7 step 4).
            ReceiveSession::Monitor(receiver) => {
                // The receiver's own wallet is the lookup: both candidates pay
                // it, so neither needs `getrawtransaction` or `txindex`.
                match unless_cancelled(
                    &mut ctx.cancel,
                    crate::onchain::sync::sync_now(&ctx.cfg, ctx.user),
                )
                .await
                {
                    None => continue,
                    Some(Err(e)) => {
                        tracing::debug!(
                            session = %ctx.session,
                            error = %e,
                            "could not sync while watching for settlement; retrying"
                        );
                        sleep_or_cancel(&mut ctx.cancel, settle_interval).await;
                        continue;
                    }
                    Some(Ok(_)) => {}
                }

                let wallet = open(ctx)?;
                let found = receiver
                    .check_for_transaction(|txid| {
                        Ok(wallet
                            .wallet
                            .get_tx(txid)
                            .map(|tx| tx.tx_node.tx.as_ref().clone()))
                    })
                    .save(&persister);

                use payjoin::persist::OptionalTransitionOutcome;
                match found {
                    // Settled. The next replay is `Closed`, which carries the
                    // outcome and, for a payjoin, the txid.
                    Ok(OptionalTransitionOutcome::Progress(())) => continue,
                    Ok(OptionalTransitionOutcome::Stasis(_)) => {
                        sleep_or_cancel(&mut ctx.cancel, settle_interval).await;
                    }
                    Err(e) => {
                        tracing::debug!(
                            session = %ctx.session,
                            error = %e,
                            "settlement check was refused; retrying"
                        );
                        sleep_or_cancel(&mut ctx.cancel, settle_interval).await;
                    }
                }
            }

            // The payjoin is off the table and the Original is the payment.
            // The sender is supposed to broadcast it, but we hold it too, and
            // putting it on the wire ourselves is what stops a sender buying a
            // look at our UTXOs and then simply walking away (§7).
            ReceiveSession::PendingFallback(receiver) => {
                let fallback = receiver.fallback_tx().clone();
                let txid = fallback.compute_txid();

                match crate::payjoin::send::broadcast(&ctx.cfg, &fallback).await {
                    Ok(_) => tracing::info!(
                        session = %ctx.session,
                        %txid,
                        "broadcast the sender's fallback transaction"
                    ),
                    // Already in the mempool is the common case here, and it
                    // is a success: the payment is on the wire either way.
                    Err(e) => tracing::debug!(
                        session = %ctx.session,
                        %txid,
                        error = %e,
                        "could not broadcast the fallback; it is most likely already known"
                    ),
                }

                receiver
                    .close()
                    .save(&persister)
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;
                return Ok(Some(PayjoinState::FellBack { txid }));
            }

            // A check refused the proposal. Tell the sender so, rather than
            // leaving it to poll until its own deadline, then let the fallback
            // be handled by the state this moves us to.
            ReceiveSession::HasReplyableError(receiver) => {
                let (request, response_ctx) = receiver
                    .create_error_request(relay.as_str())
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;

                let body = match unless_cancelled(&mut ctx.cancel, post(&request)).await {
                    None => continue,
                    Some(Ok(body)) => body,
                    Some(Err(e)) => {
                        tracing::debug!(
                            session = %ctx.session,
                            error = %e,
                            "could not post the error reply; retrying"
                        );
                        sleep_or_cancel(&mut ctx.cancel, POLL_INTERVAL).await;
                        continue;
                    }
                };

                if let Err(e) = receiver
                    .process_error_response(&body, response_ctx)
                    .save(&persister)
                {
                    // Fatal or not, the event is persisted and the replay
                    // moves us on; a transient one retries from here.
                    tracing::debug!(
                        session = %ctx.session,
                        error = %e,
                        "posting the error reply was refused"
                    );
                    sleep_or_cancel(&mut ctx.cancel, POLL_INTERVAL).await;
                }
            }

            ReceiveSession::Closed(outcome) => {
                return Ok(Some(from_outcome(
                    outcome,
                    history.fallback_tx().as_ref().map(Transaction::compute_txid),
                    failure.take(),
                )));
            }
        }
    }
}

/// Turn the crate's own verdict into the state a front end renders.
///
/// Every variant is spelled out: the old code collapsed all four into
/// `Completed` with an all-zeros txid, which reported a fallback — a payjoin
/// that did *not* happen — as a payjoin that did.
fn from_outcome(
    outcome: SessionOutcome,
    fallback_txid: Option<Txid>,
    failure: Option<String>,
) -> PayjoinState {
    match outcome {
        SessionOutcome::Success(txid) => PayjoinState::Completed { txid },
        SessionOutcome::FallbackBroadcasted => match fallback_txid {
            Some(txid) => PayjoinState::FellBack { txid },
            // The log has no fallback to point at, which should not happen
            // once a proposal has been checked. Saying so beats inventing a
            // txid.
            None => PayjoinState::Failed {
                reason: "the fallback was broadcast, but its transaction is not in the log".into(),
            },
        },
        // The sender's non-SegWit inputs change the txid when they sign, so the
        // proposal we posted can never be found on the network by id. The
        // payjoin may well have happened; we simply cannot name it.
        SessionOutcome::PayjoinProposalSent => PayjoinState::ProposalSent,
        SessionOutcome::Aborted => PayjoinState::Failed {
            reason: failure
                .unwrap_or_else(|| "the sender's proposal failed a check".into()),
        },
        SessionOutcome::Unrecognized(txid) => PayjoinState::Failed {
            reason: format!("an unrecognised transaction ({txid}) settled the session"),
        },
    }
}

/// Whether this state ends the session, or only ends our patience.
fn is_terminal(state: &PayjoinState) -> bool {
    match state {
        PayjoinState::Completed { .. }
        | PayjoinState::FellBack { .. }
        | PayjoinState::Expired
        | PayjoinState::Cancelled
        | PayjoinState::Failed { .. } => true,
        // The proposal is out but nothing has settled yet, so there is still
        // something to come back for.
        PayjoinState::Waiting
        | PayjoinState::ProposalReceived
        | PayjoinState::ProposalSent => false,
    }
}

/// The transaction a finished session should record, if it has one.
fn outcome_txid(state: &PayjoinState) -> Option<Txid> {
    match state {
        PayjoinState::Completed { txid } | PayjoinState::FellBack { txid } => Some(*txid),
        _ => None,
    }
}

/// `(is_transient, is_storage, message)` — everything `advanced` needs from a
/// rejected transition, flattened so the four generic parameters of
/// `PersistedError` do not have to be named at every call site.
type Rejection = (bool, bool, String);

fn classify<A, S, E, C>(e: payjoin::persist::PersistedError<A, S, E, C>) -> Rejection
where
    A: std::error::Error,
    S: std::error::Error,
    E: std::fmt::Debug,
    C: std::fmt::Debug,
{
    (
        e.is_transient(),
        e.storage_error_ref().is_some(),
        e.to_string(),
    )
}

/// Persist the result of one typestate check.
///
/// Returns `true` when the session advanced. `false` means "look again": the
/// crate has already persisted whatever it persisted, so the next replay puts
/// us on the right arm — `HasReplyableError` for a refusal, or the same state
/// for a transient failure worth retrying.
///
/// A storage failure is the one kind we cannot continue from. The outcome of
/// the transition is then unknown, so the event log has stopped being a safe
/// guide and the session has to end.
async fn advanced(
    ctx: &mut Context,
    step: &str,
    saved: std::result::Result<(), Rejection>,
    failure: &mut Option<String>,
) -> Result<bool> {
    match saved {
        Ok(()) => Ok(true),
        Err((_, true, message)) => Err(CoreError::Payjoin(format!("{step}: {message}"))),
        Err((transient, _, message)) => {
            if transient {
                tracing::debug!(session = %ctx.session, step, error = %message, "retrying");
                sleep_or_cancel(&mut ctx.cancel, POLL_INTERVAL).await;
            } else {
                tracing::warn!(session = %ctx.session, step, error = %message, "refused the proposal");
                *failure = Some(message);
            }
            Ok(false)
        }
    }
}

/// Why a session should stop before it has an outcome.
enum Stop {
    /// The user cancelled it, which *is* an outcome worth recording.
    Cancelled,
    /// The service is shutting down. Nothing to record.
    ServiceGone,
}

/// Whether this session should stop, and which kind of stop it is.
///
/// Both halves matter. A cancel is a verdict; a dropped sender is the process
/// going away and must not be written down as one.
fn stop_reason(cancel: &watch::Receiver<bool>) -> Option<Stop> {
    if *cancel.borrow() {
        return Some(Stop::Cancelled);
    }
    // `has_changed` is the only way to notice that every sender has been
    // dropped: `borrow` keeps answering with the last value it was sent.
    if cancel.has_changed().is_err() {
        return Some(Stop::ServiceGone);
    }
    None
}

/// Sleep, returning early if the session should stop.
///
/// The caller does not act on the return value: it `continue`s, and the check
/// at the top of the loop decides what the stop means.
async fn sleep_or_cancel(cancel: &mut watch::Receiver<bool>, how_long: Duration) {
    tokio::select! {
        () = tokio::time::sleep(how_long) => {}
        _ = cancel.changed() => {}
    }
}

/// Await `work`, giving up if cancellation arrives first.
async fn unless_cancelled<T>(
    cancel: &mut watch::Receiver<bool>,
    work: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        out = work => Some(out),
        _ = cancel.changed() => None,
    }
}

/// `check_broadcast_suitability`, where the backend can really answer it (§7).
fn dry_run(
    cfg: &AppConfig,
    tx: &Transaction,
) -> std::result::Result<bool, ImplementationError> {
    let raw = bdk_wallet::bitcoin::consensus::encode::serialize_hex(tx);
    let source = crate::rpc::ChainSource::connect(cfg)
        .map_err(|e| ImplementationError::new(std::io::Error::other(e.to_string())))?;
    use bitcoincore_rpc::RpcApi as _;
    let results = source
        .client()
        .test_mempool_accept(std::slice::from_ref(&raw))
        .map_err(|e| ImplementationError::new(std::io::Error::other(e.to_string())))?;
    Ok(results.first().is_some_and(|r| r.allowed))
}

/// Whether an outpoint in the sender's PSBT is one of ours (§7).
///
/// This is the check that stops a sender getting the receiver to co-sign the
/// receiver's own coin, so a wrong "no" is unrecoverable — and
/// `sign_payjoin_partial` trusts the witness UTXO, so it *would* sign such an
/// input if asked. It therefore asks the wallet's own output index, which knows
/// every script this wallet has ever handed out, rather than scanning the
/// transactions the wallet happens to have seen: an outpoint whose funding
/// transaction is simply not in the local graph used to answer "not mine", and
/// the check then passed.
fn is_ours(
    wallet: &OpenWallet,
    outpoint: &OutPoint,
) -> std::result::Result<bool, ImplementationError> {
    Ok(wallet.owns_outpoint(outpoint))
}

/// Every confirmed UTXO this receiver could contribute, as candidates for
/// `try_preserving_privacy` (§7).
///
/// Confirmed only: contributing an unconfirmed output makes the payjoin a child
/// of a transaction that may yet be replaced, which is a worse deal for both
/// parties than declining. `list_unspent` includes unconfirmed outputs, which
/// is why the old "pick one confirmed UTXO" comment was not true of the code
/// beneath it.
fn candidate_inputs(wallet: &OpenWallet) -> Result<Vec<InputPair>> {
    let mut candidates = Vec::new();

    for utxo in wallet.wallet.list_unspent() {
        if !utxo.chain_position.is_confirmed() {
            continue;
        }

        let txin = TxIn {
            previous_output: utxo.outpoint,
            ..Default::default()
        };
        let psbtin = psbt::Input {
            witness_utxo: Some(utxo.txout.clone()),
            ..Default::default()
        };

        match InputPair::new(txin, psbtin, None) {
            Ok(pair) => candidates.push(pair),
            // One malformed candidate is no reason to refuse the payjoin when
            // the wallet has others.
            Err(e) => tracing::debug!(outpoint = %utxo.outpoint, error = %e, "skipping candidate"),
        }
    }

    if candidates.is_empty() {
        return Err(CoreError::Payjoin(
            "no confirmed UTXO available to contribute".into(),
        ));
    }
    Ok(candidates)
}

fn open(ctx: &Context) -> Result<OpenWallet> {
    OpenWallet::load(&ctx.cfg.wallet_db(&ctx.user), ctx.cfg.network.network())
}

fn floor_rate(cfg: &AppConfig) -> Result<FeeRate> {
    Ok(crate::rpc::fees::FeePolicy::new(cfg).floor())
}

/// The most the receiver will pay, in effective fee rate, for the input it
/// contributes (§7).
///
/// Tracks the live network rate so the cap is neither too tight to accept a
/// legitimate payjoin — the builder's own default of `BROADCAST_MIN` is, and it
/// silently refused every sender that did not absorb our fee — nor loose enough
/// that a sender can push the receiver's money into miner fees. A fee-source
/// failure degrades to the clamp floor rather than failing the receive.
fn receiver_max_fee_rate(cfg: &AppConfig) -> FeeRate {
    let policy = crate::rpc::fees::FeePolicy::new(cfg);

    let fastest = policy
        .options()
        .ok()
        .and_then(|o| o.presets.iter().map(|(_, rate)| *rate).max())
        .map(|rate| rate.to_sat_per_vb_ceil())
        .unwrap_or(MIN_MAX_FEE_RATE);

    let capped = fastest
        .saturating_mul(MAX_FEE_RATE_MULTIPLIER)
        .clamp(MIN_MAX_FEE_RATE, MAX_MAX_FEE_RATE);

    FeeRate::from_sat_per_vb(capped).unwrap_or(FeeRate::BROADCAST_MIN)
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

/// Rebuild the state a stored session is in, for `/pj_sessions` (§8.2).
///
/// Takes the whole row rather than just the label, because `Completed` and
/// `FellBack` are only worth anything with the transaction that settled them —
/// this used to hand the front end an all-zeros txid for both.
pub fn state_from_row(row: &crate::payjoin::persist::SessionRow) -> PayjoinState {
    match row.state.as_str() {
        "ProposalReceived" => PayjoinState::ProposalReceived,
        "ProposalSent" => PayjoinState::ProposalSent,
        "Completed" => match row.txid {
            Some(txid) => PayjoinState::Completed { txid },
            None => PayjoinState::ProposalSent,
        },
        "FellBack" => match row.txid {
            Some(txid) => PayjoinState::FellBack { txid },
            None => PayjoinState::ProposalSent,
        },
        "Expired" => PayjoinState::Expired,
        "Cancelled" => PayjoinState::Cancelled,
        "Failed" => PayjoinState::Failed {
            reason: "the session ended in an error".into(),
        },
        // A label written by an older build must not trap `/pj_sessions`.
        _ => PayjoinState::Waiting,
    }
}

/// Whether a stored session still needs the seed before it can go on (§7).
///
/// Every step up to and including `finalize_proposal` either selects with the
/// wallet's keys or signs with them. From `Monitor` onwards the only question
/// left is which transaction settled the session, and that is a chain lookup —
/// so those sessions can resume on a restart without waiting for their owner
/// to come back and unlock.
///
/// A log that cannot be replayed is reported as needing the seed: the next
/// unlock will try again, which is a better failure than resuming a session
/// whose state we could not read.
pub fn needs_seed(store: &SessionStore, session: SessionId) -> bool {
    let persister = store.persister::<SessionEvent>(session);
    match replay_event_log(&persister) {
        Ok((state, _)) => !matches!(
            state,
            ReceiveSession::Monitor(_) | ReceiveSession::PendingFallback(_)
        ),
        Err(e) => {
            tracing::warn!(%session, error = %e, "could not replay a payjoin session");
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payjoin::persist::SessionRow;
    use crate::service::types::PayjoinRole;
    use std::str::FromStr;

    const TXID: &str = "6a0c1b8f0a2f1a4e5b3c7d9e8f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2";
    const OTHER_TXID: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    fn txid() -> Txid {
        Txid::from_str(TXID).expect("a valid txid")
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

    fn row(state: &str, txid: Option<Txid>) -> SessionRow {
        SessionRow {
            id: SessionId::new(),
            user: UserId::new(),
            role: PayjoinRole::Receiver,
            created_at: std::time::SystemTime::now(),
            expires_at: std::time::SystemTime::now() + SESSION_EXPIRY,
            closed: false,
            state: state.to_string(),
            txid,
            amount: Some(Amount::from_sat(120_000)),
        }
    }

    #[test]
    fn endpoints_follow_the_active_network() {
        let (directory, relay) = endpoints(&cfg());
        assert_eq!(directory, "https://payjo.in");
        assert_eq!(relay, "https://relay.invalid");
    }

    /// §7: the whole point of reading `SessionOutcome` instead of discarding
    /// it. A fallback is a payjoin that did **not** happen, and reporting it as
    /// `Completed` tells the user they got a privacy gain they did not get.
    #[test]
    fn a_broadcast_fallback_is_not_reported_as_a_completed_payjoin() {
        let fallback = Txid::from_str(OTHER_TXID).expect("a valid txid");
        let state = from_outcome(SessionOutcome::FallbackBroadcasted, Some(fallback), None);

        assert_eq!(state, PayjoinState::FellBack { txid: fallback });
        assert_ne!(state, PayjoinState::Completed { txid: fallback });
    }

    /// §7: a completed session carries the transaction that settled it. This
    /// used to be `Hash::all_zeros()` for every outcome alike, so the receiver
    /// could never say *which* transaction was the payjoin.
    #[test]
    fn a_successful_payjoin_carries_the_transaction_that_settled_it() {
        let state = from_outcome(SessionOutcome::Success(txid()), None, None);
        assert_eq!(state, PayjoinState::Completed { txid: txid() });
    }

    /// A sender with non-SegWit inputs changes the txid when it signs, so the
    /// proposal we posted can never be found by id. Saying "proposal sent"
    /// beats claiming a completion we cannot point at.
    #[test]
    fn an_untrackable_proposal_is_not_claimed_as_completed() {
        assert_eq!(
            from_outcome(SessionOutcome::PayjoinProposalSent, None, None),
            PayjoinState::ProposalSent
        );
    }

    /// §7: the reason a check refused is the whole diagnostic value, and
    /// `SessionOutcome::Aborted` does not carry it.
    #[test]
    fn an_aborted_session_keeps_the_reason_the_check_gave() {
        let state = from_outcome(
            SessionOutcome::Aborted,
            None,
            Some("apply_fee_range: FeeTooHigh".into()),
        );
        assert_eq!(
            state,
            PayjoinState::Failed {
                reason: "apply_fee_range: FeeTooHigh".into()
            }
        );

        // And it still says *something* when there was no reason to keep.
        assert!(matches!(
            from_outcome(SessionOutcome::Aborted, None, None),
            PayjoinState::Failed { .. }
        ));
    }

    /// §7: `ProposalSent` is where a session goes when we ran out of patience,
    /// not where it ends. Closing the row would mean a restart never came back
    /// to find out which transaction settled it.
    #[test]
    fn a_session_still_waiting_on_settlement_is_not_terminal() {
        assert!(!is_terminal(&PayjoinState::ProposalSent));
        assert!(!is_terminal(&PayjoinState::Waiting));
        assert!(!is_terminal(&PayjoinState::ProposalReceived));

        for terminal in [
            PayjoinState::Completed { txid: txid() },
            PayjoinState::FellBack { txid: txid() },
            PayjoinState::Expired,
            PayjoinState::Cancelled,
            PayjoinState::Failed {
                reason: "any".into(),
            },
        ] {
            assert!(is_terminal(&terminal), "{terminal:?} ends the session");
        }
    }

    /// §8.2: a stored row round-trips, txid included.
    #[test]
    fn a_stored_state_round_trips_with_its_transaction() {
        for state in [
            PayjoinState::Completed { txid: txid() },
            PayjoinState::FellBack { txid: txid() },
        ] {
            let stored = row(&label(&state), outcome_txid(&state));
            assert_eq!(state_from_row(&stored), state);
        }

        for state in [
            PayjoinState::Waiting,
            PayjoinState::ProposalReceived,
            PayjoinState::ProposalSent,
            PayjoinState::Expired,
            PayjoinState::Cancelled,
        ] {
            let stored = row(&label(&state), None);
            assert_eq!(state_from_row(&stored), state);
        }
    }

    /// A row from an older build has a `Completed` label and no txid. Inventing
    /// an all-zeros one is what this change exists to stop, so it reads as the
    /// last thing we can honestly claim instead.
    #[test]
    fn a_completed_row_with_no_txid_does_not_invent_one() {
        assert_eq!(
            state_from_row(&row("Completed", None)),
            PayjoinState::ProposalSent
        );
        assert_eq!(
            state_from_row(&row("FellBack", None)),
            PayjoinState::ProposalSent
        );
    }

    #[test]
    fn an_unknown_label_reads_as_waiting_rather_than_panicking() {
        // A label written by an older build must not trap /pj_sessions.
        assert_eq!(
            state_from_row(&row("something-new", None)),
            PayjoinState::Waiting
        );
    }

    /// §7: an empty or unreadable log must report *needing* the seed.
    ///
    /// `needs_seed` decides whether a restart resumes a session straight away
    /// or parks it for its owner's next unlock. Guessing "no seed needed" for
    /// a session whose state could not be read would resume it into the
    /// signing steps without one, so the conservative answer is the safe one.
    #[test]
    fn a_session_whose_log_cannot_be_read_is_treated_as_needing_the_seed() {
        let store = crate::payjoin::persist::SessionStore::in_memory().expect("opens");
        let id = SessionId::new();
        store
            .create(
                id,
                UserId::new(),
                PayjoinRole::Receiver,
                SESSION_EXPIRY,
                Some(50_000),
            )
            .expect("creates");

        // A row with no events yet: nothing to replay, so nothing is known.
        assert!(needs_seed(&store, id));
    }

    /// §7: the builder's default is `FeeRate::BROADCAST_MIN`, which refuses
    /// every sender that will not absorb our fee. Whatever the estimator says
    /// — including nothing at all — the cap has to stay inside a range that can
    /// accept a real payjoin without funding one.
    #[test]
    fn the_receiver_fee_cap_is_clamped_and_never_the_broadcast_minimum() {
        let cap = receiver_max_fee_rate(&cfg());

        assert!(
            cap > FeeRate::BROADCAST_MIN,
            "a cap at the broadcast minimum silently refuses every third-party sender"
        );
        assert!(cap >= FeeRate::from_sat_per_vb(MIN_MAX_FEE_RATE).expect("valid"));
        assert!(cap <= FeeRate::from_sat_per_vb(MAX_MAX_FEE_RATE).expect("valid"));
    }
}
