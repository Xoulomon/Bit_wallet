//! Parsing what a user pasted into `/send` (PLAN.md §6 step 1, §7).
//!
//! A bare address, or a BIP21 URI that may carry an amount, a label, a `pj=`
//! payjoin endpoint and `pjos=0`. Written here rather than taken from a crate
//! because the result has to be *our* `PaymentTarget` either way, and because
//! §10 asks for the parsing to be tested directly.
//!
//! Address validation is network-checked: an address for the wrong chain is
//! refused here, before any coin selection happens.

use crate::{
    error::{CoreError, Result},
    service::types::PaymentTarget,
};
use bdk_wallet::bitcoin::{Address, Amount, Network, address::NetworkUnchecked};
use std::str::FromStr;

pub fn parse(input: &str, network: Network) -> Result<PaymentTarget> {
    let input = input.trim();
    let refuse = || CoreError::InvalidPaymentTarget { network };

    let target = if let Some(rest) = strip_scheme(input) {
        parse_bip21(rest, network)?
    } else {
        PaymentTarget {
            address: checked(input, network)?,
            amount: None,
            label: None,
            payjoin_endpoint: None,
            output_substitution_disabled: false,
        }
    };

    // `bitcoin:` with nothing after it parses as a URI but names no recipient.
    if target.address.clone().require_network(network).is_err() {
        return Err(refuse());
    }

    Ok(target)
}

/// The scheme is case-insensitive; QR encoders often uppercase the whole URI to
/// use the smaller alphanumeric mode.
fn strip_scheme(input: &str) -> Option<&str> {
    let (scheme, rest) = input.split_once(':')?;
    scheme.eq_ignore_ascii_case("bitcoin").then_some(rest)
}

fn parse_bip21(rest: &str, network: Network) -> Result<PaymentTarget> {
    let (address_part, query) = match rest.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (rest, None),
    };

    let address = checked(address_part, network)?;

    let mut amount = None;
    let mut label = None;
    let mut payjoin_endpoint = None;
    let mut output_substitution_disabled = false;

    for pair in query
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
    {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value);

        match key.to_ascii_lowercase().as_str() {
            "amount" => {
                amount = Some(
                    Amount::from_str_in(&value, bdk_wallet::bitcoin::Denomination::Bitcoin)
                        .map_err(|_| CoreError::InvalidPaymentTarget { network })?,
                );
            }
            "label" | "message" => {
                if label.is_none() && !value.is_empty() {
                    label = Some(value);
                }
            }
            "pj" => payjoin_endpoint = Some(value),
            "pjos" => output_substitution_disabled = value == "0",
            other if other.starts_with("req-") => {
                // BIP21: a `req-` parameter we do not understand must not be
                // ignored, because the receiver required it.
                return Err(CoreError::InvalidPaymentTarget { network });
            }
            _ => {}
        }
    }

    Ok(PaymentTarget {
        address,
        amount,
        label,
        payjoin_endpoint,
        output_substitution_disabled,
    })
}

fn checked(raw: &str, network: Network) -> Result<Address<NetworkUnchecked>> {
    let parsed =
        Address::from_str(raw.trim()).map_err(|_| CoreError::InvalidPaymentTarget { network })?;

    // Refuse here rather than at spend time: a testnet address that reached
    // coin selection would waste the user's attention at best.
    if parsed.clone().require_network(network).is_err() {
        return Err(CoreError::InvalidPaymentTarget { network });
    }
    Ok(parsed)
}

/// Minimal percent-decoding: `pj=` endpoints are URL-encoded, and a label may
/// carry spaces.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&input[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAINNET: &str = "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu";
    const REGTEST: &str = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";

    #[test]
    fn a_bare_address_parses() {
        let t = parse(MAINNET, Network::Bitcoin).expect("parses");
        assert_eq!(t.address.assume_checked().to_string(), MAINNET);
        assert!(t.amount.is_none());
        assert!(t.payjoin_endpoint.is_none());
    }

    #[test]
    fn surrounding_whitespace_is_forgiven() {
        assert!(parse(&format!("  {MAINNET}  "), Network::Bitcoin).is_ok());
    }

    /// §4: an address for another chain must never reach coin selection.
    #[test]
    fn an_address_for_the_wrong_network_is_refused() {
        assert!(matches!(
            parse(MAINNET, Network::Regtest),
            Err(CoreError::InvalidPaymentTarget { .. })
        ));
        assert!(matches!(
            parse(REGTEST, Network::Bitcoin),
            Err(CoreError::InvalidPaymentTarget { .. })
        ));
    }

    #[test]
    fn nonsense_is_refused_rather_than_guessed_at() {
        for input in ["", "hello", "bitcoin:", "bitcoin:not-an-address", "1234"] {
            assert!(
                parse(input, Network::Bitcoin).is_err(),
                "`{input}` should not parse"
            );
        }
    }

    #[test]
    fn a_bip21_uri_carries_its_amount_and_label() {
        let uri = format!("bitcoin:{MAINNET}?amount=0.0005&label=Coffee%20shop");
        let t = parse(&uri, Network::Bitcoin).expect("parses");
        assert_eq!(t.amount, Some(Amount::from_sat(50_000)));
        assert_eq!(t.label.as_deref(), Some("Coffee shop"));
    }

    /// §7: `pj=` is what routes a payment into the payjoin sender.
    #[test]
    fn a_payjoin_endpoint_is_extracted_and_decoded() {
        let uri = format!("bitcoin:{MAINNET}?amount=0.001&pj=https%3A%2F%2Fpayjo.in%2FABCDEF");
        let t = parse(&uri, Network::Bitcoin).expect("parses");
        assert_eq!(
            t.payjoin_endpoint.as_deref(),
            Some("https://payjo.in/ABCDEF")
        );
        assert!(!t.output_substitution_disabled);
    }

    #[test]
    fn pjos_zero_disables_output_substitution() {
        let uri = format!("bitcoin:{MAINNET}?pj=https%3A%2F%2Fpayjo.in%2FX&pjos=0");
        let t = parse(&uri, Network::Bitcoin).expect("parses");
        assert!(t.output_substitution_disabled);

        let uri = format!("bitcoin:{MAINNET}?pj=https%3A%2F%2Fpayjo.in%2FX&pjos=1");
        assert!(
            !parse(&uri, Network::Bitcoin)
                .expect("parses")
                .output_substitution_disabled
        );
    }

    #[test]
    fn the_scheme_is_case_insensitive_as_qr_encoders_assume() {
        let uri = format!("BITCOIN:{}", MAINNET.to_uppercase());
        // The address itself is bech32, which is case-insensitive too.
        assert!(parse(&uri, Network::Bitcoin).is_ok());
    }

    /// BIP21: a required parameter we do not understand must not be ignored.
    #[test]
    fn an_unknown_required_parameter_is_refused() {
        let uri = format!("bitcoin:{MAINNET}?req-something=1");
        assert!(matches!(
            parse(&uri, Network::Bitcoin),
            Err(CoreError::InvalidPaymentTarget { .. })
        ));
    }

    #[test]
    fn an_unknown_optional_parameter_is_ignored() {
        let uri = format!("bitcoin:{MAINNET}?amount=0.001&somethingelse=1");
        assert_eq!(
            parse(&uri, Network::Bitcoin).expect("parses").amount,
            Some(Amount::from_sat(100_000))
        );
    }

    #[test]
    fn a_malformed_amount_is_refused_rather_than_treated_as_zero() {
        let uri = format!("bitcoin:{MAINNET}?amount=lots");
        assert!(parse(&uri, Network::Bitcoin).is_err());
    }

    #[test]
    fn amounts_are_in_btc_as_bip21_requires() {
        // The classic mistake: reading 0.001 as sats.
        let uri = format!("bitcoin:{MAINNET}?amount=1");
        assert_eq!(
            parse(&uri, Network::Bitcoin).expect("parses").amount,
            Some(Amount::from_sat(100_000_000))
        );
    }
}
