use std::collections::HashMap;

use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::Sha256;
use thiserror::Error;

const TOLERANCE_SECS: i64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SignatureError {
    #[error("Stripe-Signature header is missing a timestamp or v1 signature")]
    Malformed,
    #[error("signature timestamp is outside the {TOLERANCE_SECS}s tolerance")]
    Expired,
    #[error("no v1 signature matches the payload")]
    Mismatch,
}

/// Stripe sends one `v1` entry per active signing secret while a secret is being
/// rolled, so every entry is checked rather than only the first or last.
pub fn verify_signature(
    payload: &[u8],
    header: &str,
    secret: &str,
    now_unix: i64,
) -> Result<(), SignatureError> {
    let mut timestamp = None;
    let mut signatures = Vec::new();
    for pair in header.split(',') {
        match pair.trim().split_once('=') {
            Some(("t", value)) => timestamp = value.parse::<i64>().ok(),
            Some(("v1", value)) => signatures.push(value),
            _ => {}
        }
    }

    let timestamp = timestamp.ok_or(SignatureError::Malformed)?;
    if signatures.is_empty() {
        return Err(SignatureError::Malformed);
    }
    if now_unix.abs_diff(timestamp) > TOLERANCE_SECS.unsigned_abs() {
        return Err(SignatureError::Expired);
    }

    let expected = signed_mac(payload, secret, timestamp);
    let matched = signatures
        .iter()
        .filter_map(|signature| hex::decode(signature).ok())
        .any(|signature| expected.clone().verify_slice(&signature).is_ok());
    if matched {
        Ok(())
    } else {
        Err(SignatureError::Mismatch)
    }
}

pub fn signature_header(payload: &[u8], secret: &str, timestamp: i64) -> String {
    let signature = signed_mac(payload, secret, timestamp)
        .finalize()
        .into_bytes();
    format!("t={timestamp},v1={}", hex::encode(signature))
}

fn signed_mac(payload: &[u8], secret: &str, timestamp: i64) -> Hmac<Sha256> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    mac
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: String,
    pub livemode: bool,
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    PaymentIntentSucceeded(PaymentIntent),
    PaymentIntentPaymentFailed(PaymentIntent),
    PaymentIntentCanceled(PaymentIntent),
    Ignored(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PaymentIntent {
    pub id: String,
    pub amount_received: i64,
    pub currency: String,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

/// Reads only the fields we act on, so a Stripe API version change elsewhere in
/// the payload cannot make a signed event unreadable.
pub fn parse_event(payload: &[u8]) -> Result<Event, serde_json::Error> {
    #[derive(Deserialize)]
    struct Envelope {
        id: String,
        #[serde(default)]
        livemode: bool,
        #[serde(rename = "type")]
        kind: String,
        data: Data,
    }

    #[derive(Deserialize)]
    struct Data {
        object: serde_json::Value,
    }

    let envelope: Envelope = serde_json::from_slice(payload)?;
    let intent = || serde_json::from_value::<PaymentIntent>(envelope.data.object);
    let kind = match envelope.kind.as_str() {
        "payment_intent.succeeded" => EventKind::PaymentIntentSucceeded(intent()?),
        "payment_intent.payment_failed" => EventKind::PaymentIntentPaymentFailed(intent()?),
        "payment_intent.canceled" => EventKind::PaymentIntentCanceled(intent()?),
        _ => EventKind::Ignored(envelope.kind),
    };
    Ok(Event {
        id: envelope.id,
        livemode: envelope.livemode,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "whsec_test";
    const NOW: i64 = 1_800_000_000;
    const PAYLOAD: &[u8] = br#"{"id":"evt_1"}"#;

    #[test]
    fn accepts_a_valid_signature() {
        let header = signature_header(PAYLOAD, SECRET, NOW);
        assert_eq!(verify_signature(PAYLOAD, &header, SECRET, NOW + 10), Ok(()));
    }

    #[test]
    fn accepts_any_matching_signature_during_secret_rotation() {
        let valid = signature_header(PAYLOAD, SECRET, NOW);
        let stale = signature_header(PAYLOAD, "whsec_old", NOW);
        let v1 = |header: &str| header.split_once(",v1=").unwrap().1.to_owned();
        let header = format!("t={NOW},v1={},v1={}", v1(&valid), v1(&stale));
        assert_eq!(verify_signature(PAYLOAD, &header, SECRET, NOW), Ok(()));
    }

    #[test]
    fn rejects_a_tampered_payload() {
        let header = signature_header(PAYLOAD, SECRET, NOW);
        assert_eq!(
            verify_signature(br#"{"id":"evt_2"}"#, &header, SECRET, NOW),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn rejects_the_wrong_secret() {
        let header = signature_header(PAYLOAD, "whsec_other", NOW);
        assert_eq!(
            verify_signature(PAYLOAD, &header, SECRET, NOW),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn rejects_replays_outside_the_tolerance() {
        let header = signature_header(PAYLOAD, SECRET, NOW);
        assert_eq!(
            verify_signature(PAYLOAD, &header, SECRET, NOW + TOLERANCE_SECS + 1),
            Err(SignatureError::Expired)
        );
    }

    #[test]
    fn rejects_extreme_timestamps_without_overflowing() {
        let header = format!("t={},v1=00", i64::MIN);
        assert_eq!(
            verify_signature(PAYLOAD, &header, SECRET, NOW),
            Err(SignatureError::Expired)
        );
    }

    #[test]
    fn rejects_malformed_headers() {
        for header in ["", "t=abc,v1=00", "v1=00", "t=1800000000"] {
            assert_eq!(
                verify_signature(PAYLOAD, header, SECRET, NOW),
                Err(SignatureError::Malformed),
                "{header}"
            );
        }
    }

    #[test]
    fn parses_payment_intent_events_and_ignores_others() {
        let succeeded = parse_event(
            br#"{"id":"evt_1","livemode":true,"type":"payment_intent.succeeded","api_version":"2099-01-01",
                "data":{"object":{"id":"pi_1","amount_received":1000,"currency":"usd",
                "metadata":{"payment_id":"abc"},"some_future_field":{"x":1}}}}"#,
        )
        .unwrap();
        assert_eq!(
            succeeded,
            Event {
                id: "evt_1".to_owned(),
                livemode: true,
                kind: EventKind::PaymentIntentSucceeded(PaymentIntent {
                    id: "pi_1".to_owned(),
                    amount_received: 1000,
                    currency: "usd".to_owned(),
                    metadata: HashMap::from([("payment_id".to_owned(), "abc".to_owned())]),
                }),
            }
        );

        let other = parse_event(br#"{"id":"evt_2","type":"charge.refunded","data":{"object":{}}}"#)
            .unwrap();
        assert_eq!(other.kind, EventKind::Ignored("charge.refunded".to_owned()));
    }
}
