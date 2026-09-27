use async_trait::async_trait;
use dual_rail_core::{Currency, Money};
use khqr_api::{BakongClient, TxStatus};
use khqr_core::Khqr;
use thiserror::Error;
use uuid::Uuid;

const BAKONG_BATCH_LIMIT: usize = 50;
const BILL_NUMBER_LEN: usize = 25;
const MAX_AMOUNT_TEXT_LEN: usize = 13;

#[derive(Debug, Clone)]
pub struct MerchantAccount {
    pub account_id: String,
    pub merchant_name: String,
    pub merchant_city: String,
    pub merchant_id: Option<String>,
    pub acquiring_bank: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedQr {
    pub payload: String,
    pub md5: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IssueError {
    #[error("KHR amounts must be whole riel, got {0} minor units")]
    FractionalRiel(i64),
    #[error("amount {0} is too large for a KHQR")]
    TooLarge(String),
    #[error("could not build KHQR: {0}")]
    Build(String),
    #[error("KHQR encodes {encoded:?} {currency} but the payment is {expected}")]
    AmountDrift {
        expected: String,
        encoded: Option<String>,
        currency: String,
    },
}

pub struct KhqrIssuer {
    account: MerchantAccount,
}

impl KhqrIssuer {
    pub fn new(account: MerchantAccount) -> Self {
        Self { account }
    }

    pub fn account_id(&self) -> &str {
        &self.account.account_id
    }

    pub fn issue(
        &self,
        payment_id: Uuid,
        amount: Money,
        created_at_ms: u64,
        expires_at_ms: u64,
    ) -> Result<IssuedQr, IssueError> {
        let (expected, value, currency) = khqr_amount(amount)?;
        let account = &self.account;
        let builder = match (&account.merchant_id, &account.acquiring_bank) {
            (Some(merchant_id), Some(bank)) => Khqr::merchant(&account.account_id)
                .merchant_id(merchant_id)
                .acquiring_bank(bank),
            _ => Khqr::individual(&account.account_id),
        };
        // The bill number makes every payload, and so every md5, unique per payment.
        let payload = builder
            .merchant_name(&account.merchant_name)
            .merchant_city(&account.merchant_city)
            .currency(currency)
            .amount(value)
            .bill_number(bill_number(payment_id))
            .created_at_ms(created_at_ms)
            .expires_at_ms(expires_at_ms)
            .build()
            .and_then(|khqr| khqr.to_qr_string())
            .map_err(|err| IssueError::Build(err.to_string()))?;

        // khqr-core formats a float and rounds; refuse any QR that would charge a
        // different amount than the one we record.
        let decoded =
            khqr_core::decode(&payload).map_err(|err| IssueError::Build(err.to_string()))?;
        if decoded.transaction_amount.as_deref() != Some(expected.as_str())
            || decoded.transaction_currency != currency.code()
        {
            return Err(IssueError::AmountDrift {
                expected,
                encoded: decoded.transaction_amount,
                currency: decoded.transaction_currency,
            });
        }

        Ok(IssuedQr {
            md5: khqr_core::md5(&payload),
            payload,
        })
    }
}

/// Base36 fits all 128 bits of the id into the 25 characters KHQR allows, so
/// distinct payments can never share a bill number.
fn bill_number(payment_id: Uuid) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut value = payment_id.as_u128();
    let mut out = [b'0'; BILL_NUMBER_LEN];
    for slot in out.iter_mut().rev() {
        *slot = DIGITS[(value % 36) as usize];
        value /= 36;
    }
    String::from_utf8(out.to_vec()).expect("base36 digits are ascii")
}

fn khqr_amount(amount: Money) -> Result<(String, f64, khqr_core::Currency), IssueError> {
    let minor = amount.amount_minor();
    let converted = match amount.currency() {
        Currency::Usd => Ok((
            format!("{}.{:02}", minor / 100, minor % 100),
            minor as f64 / 100.0,
            khqr_core::Currency::Usd,
        )),
        Currency::Khr if minor % 100 != 0 => Err(IssueError::FractionalRiel(minor)),
        Currency::Khr => {
            let riel = minor / 100;
            Ok((riel.to_string(), riel as f64, khqr_core::Currency::Khr))
        }
    }?;
    if converted.0.len() > MAX_AMOUNT_TEXT_LEN {
        return Err(IssueError::TooLarge(converted.0));
    }
    Ok(converted)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KhqrStatus {
    Unpaid,
    Paid(KhqrTransfer),
    Unreadable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KhqrTransfer {
    pub hash: String,
    pub amount_minor: Option<i64>,
    pub currency: Option<String>,
    pub to_account_id: Option<String>,
    pub paid_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Error)]
#[error("bakong check failed: {0}")]
pub struct VerifierError(pub String);

#[async_trait]
pub trait KhqrVerifier: Send + Sync {
    /// Answers in the same order as `md5s`.
    async fn check(&self, md5s: &[String]) -> Result<Vec<KhqrStatus>, VerifierError>;
}

/// Talks to Bakong directly, or through a relay in Cambodia when the client was
/// built with `BakongClient::with_base_url`.
pub struct BakongVerifier {
    client: BakongClient,
}

impl BakongVerifier {
    pub fn new(client: BakongClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl KhqrVerifier for BakongVerifier {
    async fn check(&self, md5s: &[String]) -> Result<Vec<KhqrStatus>, VerifierError> {
        let mut statuses = Vec::with_capacity(md5s.len());
        for chunk in md5s.chunks(BAKONG_BATCH_LIMIT) {
            let answers = self
                .client
                .check_transaction_by_md5_list(chunk)
                .await
                .map_err(|err| VerifierError(err.to_string()))?;
            statuses.extend(answers.into_iter().map(to_status));
        }
        Ok(statuses)
    }
}

fn to_status(status: TxStatus) -> KhqrStatus {
    match status {
        TxStatus::NotFound => KhqrStatus::Unpaid,
        TxStatus::StaticQr => {
            KhqrStatus::Unreadable("bakong treated a dynamic QR as static".to_owned())
        }
        TxStatus::Paid(transaction) => KhqrStatus::Paid(KhqrTransfer {
            hash: transaction.hash,
            amount_minor: transaction.amount.and_then(to_minor),
            currency: transaction.currency,
            to_account_id: transaction.to_account_id,
            // When the payer sent it; acknowledgement can lag past the expiry.
            paid_at_ms: transaction
                .created_date_ms
                .or(transaction.acknowledged_date_ms),
        }),
    }
}

fn to_minor(amount: f64) -> Option<i64> {
    let minor = (amount * 100.0).round();
    (minor.is_finite() && (1.0..1e15).contains(&minor)).then_some(minor as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREATED: u64 = 1_800_000_000_000;
    const EXPIRES: u64 = CREATED + 300_000;

    fn issuer() -> KhqrIssuer {
        KhqrIssuer::new(MerchantAccount {
            account_id: "dual_rail@devb".to_owned(),
            merchant_name: "Dual Rail".to_owned(),
            merchant_city: "Phnom Penh".to_owned(),
            merchant_id: None,
            acquiring_bank: None,
        })
    }

    fn issue(amount_minor: i64, currency: Currency) -> Result<IssuedQr, IssueError> {
        issuer().issue(
            Uuid::from_u128(1),
            Money::new(amount_minor, currency).unwrap(),
            CREATED,
            EXPIRES,
        )
    }

    fn encoded_amount(qr: &IssuedQr) -> (Option<String>, String) {
        let decoded = khqr_core::decode(&qr.payload).unwrap();
        (decoded.transaction_amount, decoded.transaction_currency)
    }

    #[test]
    fn encodes_dollars_with_cents() {
        for (minor, text) in [(1, "0.01"), (1000, "10.00"), (123_456, "1234.56")] {
            let qr = issue(minor, Currency::Usd).unwrap();
            assert_eq!(
                encoded_amount(&qr),
                (Some(text.to_owned()), "840".to_owned())
            );
            assert_eq!(qr.md5, khqr_core::md5(&qr.payload));
        }
    }

    #[test]
    fn encodes_whole_riel() {
        let qr = issue(500_000, Currency::Khr).unwrap();
        assert_eq!(
            encoded_amount(&qr),
            (Some("5000".to_owned()), "116".to_owned())
        );
    }

    #[test]
    fn refuses_fractional_riel_instead_of_rounding() {
        assert_eq!(
            issue(50_070, Currency::Khr),
            Err(IssueError::FractionalRiel(50_070))
        );
    }

    #[test]
    fn refuses_amounts_too_long_for_the_qr() {
        assert_eq!(
            issue(100_000_000_000_000, Currency::Usd),
            Err(IssueError::TooLarge("1000000000000.00".to_owned()))
        );
    }

    #[test]
    fn identical_payments_get_distinct_md5s() {
        let amount = Money::new(1000, Currency::Usd).unwrap();
        let first = issuer()
            .issue(Uuid::from_u128(1), amount, CREATED, EXPIRES)
            .unwrap();
        let second = issuer()
            .issue(Uuid::from_u128(2), amount, CREATED, EXPIRES)
            .unwrap();
        assert_ne!(first.md5, second.md5);
    }

    #[test]
    fn bill_numbers_are_unique_and_fit_the_field() {
        assert_eq!(bill_number(Uuid::from_u128(0)), "0".repeat(BILL_NUMBER_LEN));
        assert_eq!(bill_number(Uuid::max()), "f5lxx1zz5pnorynqglhzmsp33");
        assert_ne!(
            bill_number(Uuid::from_u128(1)),
            bill_number(Uuid::from_u128(2))
        );
    }

    #[test]
    fn carries_the_expiry() {
        let qr = issue(1000, Currency::Usd).unwrap();
        let decoded = khqr_core::decode(&qr.payload).unwrap();
        assert_eq!(decoded.expires_at_ms, Some(EXPIRES));
    }

    #[test]
    fn converts_bakong_amounts_to_minor_units() {
        assert_eq!(to_minor(10.1), Some(1010));
        assert_eq!(to_minor(0.29), Some(29));
        assert_eq!(to_minor(5000.0), Some(500_000));
        assert_eq!(to_minor(0.0), None);
        assert_eq!(to_minor(f64::NAN), None);
    }
}
