use axum::extract::{Request, State};
use axum::http::{HeaderMap, header};
use axum::middleware::Next;
use axum::response::Response;
use dual_rail_store::api_keys;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::AppState;
use crate::error::ApiError;

const KEY_PREFIX: &str = "drk_";
const DISPLAY_PREFIX_LEN: usize = 12;
const CLIENT_TOKEN_HEADER: &str = "x-client-token";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApiKey {
    /// Shown once; only its hash is stored.
    pub key: String,
    pub prefix: String,
    pub hash: String,
}

pub fn generate_api_key() -> NewApiKey {
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).expect("the operating system's random source is available");
    let key = format!("{KEY_PREFIX}{}", hex::encode(secret));
    NewApiKey {
        prefix: key[..DISPLAY_PREFIX_LEN].to_owned(),
        hash: hash_api_key(&key),
        key,
    }
}

/// Keys carry 256 random bits, so a fast hash is enough: nothing about the key can
/// be guessed from it, unlike a password.
pub fn hash_api_key(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

/// The token a merchant hands to the customer's browser: it can read one payment
/// and nothing else. Derived rather than stored, so every replay returns the same one.
pub fn client_token(secret: &[u8], payment_id: Uuid) -> String {
    hex::encode(client_token_mac(secret, payment_id).finalize().into_bytes())
}

pub async fn require_api_key(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    authenticate(&state, request.headers()).await?;
    Ok(next.run(request).await)
}

/// A merchant key reads any payment; a client token reads only the payment it
/// was issued for.
pub async fn authorize_read(
    state: &AppState,
    headers: &HeaderMap,
    payment_id: Uuid,
) -> Result<(), ApiError> {
    match headers
        .get(CLIENT_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
    {
        Some(token) if verify_client_token(&state.client_token_secret, payment_id, token) => Ok(()),
        Some(_) => Err(ApiError::Unauthorized),
        None => authenticate(state, headers).await,
    }
}

async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let key = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|key| key.starts_with(KEY_PREFIX))
        .ok_or(ApiError::Unauthorized)?;
    let key_id = api_keys::find_active(&state.pool, &hash_api_key(key))
        .await?
        .ok_or(ApiError::Unauthorized)?;
    api_keys::touch(&state.pool, key_id).await?;
    Ok(())
}

fn verify_client_token(secret: &[u8], payment_id: Uuid, token: &str) -> bool {
    hex::decode(token).is_ok_and(|token| {
        client_token_mac(secret, payment_id)
            .verify_slice(&token)
            .is_ok()
    })
}

fn client_token_mac(secret: &[u8], payment_id: Uuid) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(b"dual-rail client token:");
    mac.update(payment_id.as_bytes());
    mac
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"a-test-secret-that-is-long-enough!!";

    #[test]
    fn generated_keys_are_unique_and_only_their_hash_is_kept() {
        let first = generate_api_key();
        let second = generate_api_key();

        assert_ne!(first.key, second.key);
        assert!(first.key.starts_with("drk_") && first.key.len() == 68);
        assert_eq!(first.prefix, &first.key[..12]);
        assert_eq!(first.hash, hash_api_key(&first.key));
        assert!(!first.hash.contains(&first.key[4..]));
    }

    #[test]
    fn a_client_token_opens_only_its_own_payment() {
        let payment = Uuid::from_u128(1);
        let token = client_token(SECRET, payment);

        assert!(verify_client_token(SECRET, payment, &token));
        assert!(!verify_client_token(SECRET, Uuid::from_u128(2), &token));
        assert!(!verify_client_token(
            b"another-secret-entirely-different!!",
            payment,
            &token
        ));
        assert!(!verify_client_token(SECRET, payment, "not hex"));
        assert!(!verify_client_token(SECRET, payment, ""));
    }
}
