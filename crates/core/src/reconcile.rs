use crate::Money;
use crate::code::string_codes;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MismatchKind {
    MissingInLedger,
    LedgerWithoutPayment,
    UnbalancedEntry,
    MissingAtProvider,
    AmountMismatch,
    PaidAfterExpiry,
    StalePending,
    Unverified,
}

string_codes!(MismatchKind, "mismatch kind", {
    MismatchKind::MissingInLedger => "missing_in_ledger",
    MismatchKind::LedgerWithoutPayment => "ledger_without_payment",
    MismatchKind::UnbalancedEntry => "unbalanced_entry",
    MismatchKind::MissingAtProvider => "missing_at_provider",
    MismatchKind::AmountMismatch => "amount_mismatch",
    MismatchKind::PaidAfterExpiry => "paid_after_expiry",
    MismatchKind::StalePending => "stale_pending",
    MismatchKind::Unverified => "unverified",
});

/// What a provider says about one of our payments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderView {
    Paid {
        amount_minor: Option<i64>,
        currency: Option<String>,
    },
    NotPaid {
        status: String,
    },
    Missing,
    Unknown {
        reason: String,
    },
}

/// Compares a payment we recorded as succeeded with the provider's view.
pub fn check_succeeded(ours: Money, provider: &ProviderView) -> Option<(MismatchKind, String)> {
    match provider {
        ProviderView::Paid {
            amount_minor,
            currency,
        } => {
            let matches = *amount_minor == Some(ours.amount_minor())
                && currency.as_deref().is_some_and(|currency| {
                    currency.eq_ignore_ascii_case(ours.currency().as_str())
                });
            (!matches).then(|| {
                (
                    MismatchKind::AmountMismatch,
                    format!(
                        "provider reports {} {}, ledger has {} {}",
                        amount_minor.map_or("no amount".to_owned(), |amount| amount.to_string()),
                        currency.as_deref().unwrap_or("no currency"),
                        ours.amount_minor(),
                        ours.currency(),
                    ),
                )
            })
        }
        ProviderView::NotPaid { status } => Some((
            MismatchKind::MissingAtProvider,
            format!("provider status is {status}"),
        )),
        ProviderView::Missing => Some((
            MismatchKind::MissingAtProvider,
            "provider has no record of this payment".to_owned(),
        )),
        ProviderView::Unknown { reason } => Some((MismatchKind::Unverified, reason.clone())),
    }
}

/// Checks a payment we expired: the provider must not have been paid for it.
pub fn check_expired(provider: &ProviderView) -> Option<(MismatchKind, String)> {
    match provider {
        ProviderView::Paid { .. } => Some((
            MismatchKind::PaidAfterExpiry,
            "provider reports a payment for a QR we expired".to_owned(),
        )),
        ProviderView::Unknown { reason } => Some((MismatchKind::Unverified, reason.clone())),
        ProviderView::NotPaid { .. } | ProviderView::Missing => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Currency;

    fn usd(amount_minor: i64) -> Money {
        Money::new(amount_minor, Currency::Usd).unwrap()
    }

    fn paid(amount_minor: Option<i64>, currency: Option<&str>) -> ProviderView {
        ProviderView::Paid {
            amount_minor,
            currency: currency.map(str::to_owned),
        }
    }

    fn kind(result: Option<(MismatchKind, String)>) -> Option<MismatchKind> {
        result.map(|(kind, _)| kind)
    }

    #[test]
    fn matching_payment_has_no_mismatch() {
        assert_eq!(
            check_succeeded(usd(1000), &paid(Some(1000), Some("usd"))),
            None
        );
    }

    #[test]
    fn different_or_missing_amount_or_currency_is_an_amount_mismatch() {
        for view in [
            paid(Some(999), Some("USD")),
            paid(Some(1000), Some("KHR")),
            paid(None, Some("USD")),
            paid(Some(1000), None),
        ] {
            assert_eq!(
                kind(check_succeeded(usd(1000), &view)),
                Some(MismatchKind::AmountMismatch),
                "{view:?}"
            );
        }
    }

    #[test]
    fn unpaid_or_unknown_at_provider_is_missing_there() {
        let not_paid = ProviderView::NotPaid {
            status: "requires_payment_method".to_owned(),
        };
        assert_eq!(
            kind(check_succeeded(usd(1000), &not_paid)),
            Some(MismatchKind::MissingAtProvider)
        );
        assert_eq!(
            kind(check_succeeded(usd(1000), &ProviderView::Missing)),
            Some(MismatchKind::MissingAtProvider)
        );
    }

    #[test]
    fn provider_errors_are_unverified_not_mismatches() {
        let unknown = ProviderView::Unknown {
            reason: "http 403".to_owned(),
        };
        assert_eq!(
            kind(check_succeeded(usd(1000), &unknown)),
            Some(MismatchKind::Unverified)
        );
        assert_eq!(
            kind(check_expired(&unknown)),
            Some(MismatchKind::Unverified)
        );
    }

    #[test]
    fn expired_payment_that_was_paid_is_flagged() {
        assert_eq!(
            kind(check_expired(&paid(Some(1000), Some("USD")))),
            Some(MismatchKind::PaidAfterExpiry)
        );
        assert_eq!(check_expired(&ProviderView::Missing), None);
    }
}
