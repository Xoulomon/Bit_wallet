//! Payjoin sending (PLAN.md §7): BIP77 v2, plus BIP78 v1 for endpoints such as
//! BTCPay that never moved.
//!
//! The shape is deliberately the same as an ordinary `/send`: the Original PSBT
//! is built and signed exactly as it would be for a plain payment, and *that is
//! the fallback*. If the receiver never answers, or answers with something we
//! will not sign, we broadcast the original and tell the user it went as a
//! regular transaction. A payjoin that fails must never mean a payment that
//! failed.

use crate::{
    config::AppConfig,
    error::{CoreError, Result},
    onchain::OpenWallet,
    payjoin::persist::SessionStore,
    service::{
        events::CoreEvent,
        types::{PayjoinRole, PayjoinState, SessionId, UserId},
    },
};
use bdk_wallet::{
    bitcoin::{FeeRate, Psbt, Transaction, Txid},
    keys::bip39::Mnemonic,
};
use payjoin::{
    PjUri,
    send::v2::{SendSession, SenderBuilder, SessionEvent, replay_event_log},
};
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio::sync::broadcast;

/// How long to chase a payjoin before falling back (§7 step 4).
///
/// Short on purpose: the user is waiting, and the fallback is a perfectly good
/// payment. Privacy is worth two minutes of their time, not twenty.
pub const SEND_DEADLINE: Duration = Duration::from_secs(120);

const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// What a completed attempt produced.
pub struct Outcome {
    pub txid: Txid,
    /// False when the fallback was broadcast instead (§7 step 4).
    pub payjoin: bool,
}

/// Everything one send attempt needs.
pub struct Attempt {
    pub cfg: AppConfig,
    pub store: Arc<SessionStore>,
    pub events: broadcast::Sender<CoreEvent>,
    pub user: UserId,
    /// The signed Original PSBT — which is also the fallback.
    pub original: Psbt,
    pub uri: String,
    pub fee_rate: FeeRate,
    pub mnemonic: Mnemonic,
}

/// Try a payjoin, and fall back to the ordinary transaction if it does not
/// complete (§7).
pub async fn attempt(a: Attempt) -> Result<Outcome> {
    let session = SessionId::new();
    a.store
        .create(session, a.user, PayjoinRole::Sender, SEND_DEADLINE, None)?;

    let publish = |state: PayjoinState| {
        let _ = a.store.set_state(session, &super::receive::label(&state));
        let _ = a.events.send(CoreEvent::Payjoin {
            user: a.user,
            session,
            state,
        });
    };

    publish(PayjoinState::Waiting);

    let fallback = a
        .original
        .clone()
        .extract_tx()
        .map_err(|e| CoreError::Wallet(e.to_string()))?;

    match try_payjoin(&a, session).await {
        Ok(Some(proposal)) => {
            // The receiver's proposal, signed by us and broadcast.
            let txid = finish(&a, proposal).await?;
            publish(PayjoinState::Completed { txid });
            let _ = a.store.close(session);
            Ok(Outcome {
                txid,
                payjoin: true,
            })
        }
        Ok(None) | Err(_) => {
            // §7: broadcast the Original and say so. The user's payment goes
            // through either way — only the privacy gain is lost.
            let txid = broadcast(&a.cfg, &fallback).await?;
            publish(PayjoinState::FellBack { txid });
            let _ = a.store.close(session);
            Ok(Outcome {
                txid,
                payjoin: false,
            })
        }
    }
}

/// The v2 exchange, or the v1 POST for an endpoint that predates it (§7 step 2).
async fn try_payjoin(a: &Attempt, session: SessionId) -> Result<Option<Psbt>> {
    let uri = parse_uri(&a.uri, a.cfg.network.network())?;

    // A v1 endpoint — a BTCPay instance, say — has no directory, no OHTTP and
    // no session: it is one synchronous POST. Branching here rather than
    // inside the loop keeps the v2 state machine free of "except when v1".
    let v2_param = match uri.extras().pj_param() {
        payjoin::PjParam::V2(param) => param.clone(),
        // `PjParam` is non-exhaustive: a version we do not know is not a
        // version we should guess at, and the v1 POST is the safe attempt.
        _ => return v1_exchange(a, &uri).await,
    };

    let (_, relay) = super::receive::endpoints(&a.cfg);
    let persister = a.store.persister::<SessionEvent>(session);

    SenderBuilder::from_parts(a.original.clone(), &v2_param, uri.address(), uri.amount())
        .build_recommended(a.fee_rate)
        .map_err(|e| CoreError::Payjoin(e.to_string()))?
        .save(&persister)
        .map_err(|e| CoreError::Payjoin(e.to_string()))?;

    let deadline = std::time::Instant::now() + SEND_DEADLINE;

    loop {
        if std::time::Instant::now() > deadline {
            return Ok(None);
        }

        let (state, _history) =
            replay_event_log(&persister).map_err(|e| CoreError::Payjoin(e.to_string()))?;

        match state {
            SendSession::WithReplyKey(sender) => {
                let (request, response_ctx) = sender
                    .create_v2_post_request(relay.as_str())
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;
                let body = super::receive::post(&request).await?;

                sender
                    .process_response(&body, response_ctx)
                    .save(&persister)
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;

                let _ = a.events.send(CoreEvent::Payjoin {
                    user: a.user,
                    session,
                    state: PayjoinState::ProposalSent,
                });
            }

            SendSession::PollingForProposal(sender) => {
                let (request, response_ctx) = sender
                    .create_poll_request(relay.as_str())
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;
                let body = super::receive::post(&request).await?;

                use payjoin::persist::OptionalTransitionOutcome;
                let outcome = sender
                    .process_response(&body, response_ctx)
                    .save(&persister)
                    .map_err(|e| CoreError::Payjoin(e.to_string()))?;

                match outcome {
                    OptionalTransitionOutcome::Progress(proposal) => return Ok(Some(proposal)),
                    OptionalTransitionOutcome::Stasis(_) => {
                        tokio::time::sleep(POLL_INTERVAL).await;
                    }
                }
            }

            // The receiver declined or the session ended: the fallback carries
            // the payment.
            SendSession::PendingFallback(_) | SendSession::Closed(_) => return Ok(None),
        }
    }
}

/// BIP78 v1: one POST, one answer, no polling and nothing to persist (§7).
async fn v1_exchange(a: &Attempt, uri: &PjUri) -> Result<Option<Psbt>> {
    let sender = payjoin::send::v1::SenderBuilder::new(a.original.clone(), uri.clone())
        .build_recommended(a.fee_rate)
        .map_err(|e| CoreError::Payjoin(e.to_string()))?;

    let (request, context) = sender.create_v1_post_request();
    let body = match super::receive::post(&request).await {
        Ok(body) => body,
        // An unreachable v1 endpoint is exactly the fallback case.
        Err(e) => {
            tracing::warn!(error = %e, "v1 payjoin endpoint unreachable");
            return Ok(None);
        }
    };

    match context.process_response(&body) {
        Ok(proposal) => Ok(Some(proposal)),
        Err(e) => {
            tracing::warn!(error = %e, "v1 payjoin proposal rejected");
            Ok(None)
        }
    }
}

/// Sign our inputs in the receiver's proposal, finalise and broadcast (§7 step 3).
async fn finish(a: &Attempt, proposal: Psbt) -> Result<Txid> {
    let wallet = OpenWallet::load(&a.cfg.wallet_db(&a.user), a.cfg.network.network())?;

    let mut signed = proposal;
    wallet.sign(&mut signed, &a.mnemonic)?;

    let tx = signed
        .extract_tx()
        .map_err(|e| CoreError::Wallet(e.to_string()))?;

    broadcast(&a.cfg, &tx).await
}

async fn broadcast(cfg: &AppConfig, tx: &Transaction) -> Result<Txid> {
    let cfg = cfg.clone();
    let raw = bdk_wallet::bitcoin::consensus::encode::serialize_hex(tx);
    let txid = tx.compute_txid();

    tokio::task::spawn_blocking(move || -> Result<()> {
        use bitcoincore_rpc::RpcApi as _;
        let source = crate::rpc::ChainSource::connect(&cfg)?;
        source.client().send_raw_transaction(raw).map_err(|e| {
            match crate::rpc::map_rpc_error("sendrawtransaction")(e) {
                CoreError::Backend(crate::error::BackendError::Rpc { message, .. }) => {
                    CoreError::BroadcastRejected { reason: message }
                }
                other => other,
            }
        })?;
        Ok(())
    })
    .await
    .map_err(|e| CoreError::Wallet(e.to_string()))??;

    Ok(txid)
}

/// A BIP21 URI with a `pj=` parameter, as the payjoin crate wants it.
pub fn parse_uri(raw: &str, network: bdk_wallet::bitcoin::Network) -> Result<PjUri> {
    let uri = payjoin::Uri::from_str(raw)
        .map_err(|e| CoreError::Payjoin(format!("not a payjoin URI: {e}")))?
        .require_network(network)
        .map_err(|_| CoreError::InvalidPaymentTarget { network })?;

    uri.check_pj_supported()
        .map_err(|_| CoreError::Payjoin("that URI does not offer payjoin".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bdk_wallet::bitcoin::Network;

    const ADDRESS: &str = "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu";

    #[test]
    fn a_uri_without_pj_is_refused_as_a_payjoin_target() {
        // It is a perfectly good payment — it is just not a payjoin one, and
        // the caller has to take the ordinary path instead (§6 step 1).
        let plain = format!("bitcoin:{ADDRESS}?amount=0.001");
        assert!(matches!(
            parse_uri(&plain, Network::Bitcoin),
            Err(CoreError::Payjoin(_))
        ));
    }

    #[test]
    fn a_uri_for_the_wrong_network_is_refused() {
        let regtest = "bitcoin:bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080?amount=0.001";
        assert!(parse_uri(regtest, Network::Bitcoin).is_err());
    }

    #[test]
    fn nonsense_is_refused_rather_than_panicking() {
        for raw in ["", "hello", "bitcoin:", "http://example.com"] {
            assert!(parse_uri(raw, Network::Bitcoin).is_err(), "`{raw}` parsed");
        }
    }

    /// §7: two minutes. The user is waiting and the fallback is a perfectly
    /// good payment, so the deadline is short by design.
    #[test]
    fn the_send_deadline_is_short_enough_to_wait_for() {
        assert!(SEND_DEADLINE <= Duration::from_secs(300));
        assert!(SEND_DEADLINE >= Duration::from_secs(30));
    }
}
