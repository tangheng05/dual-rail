use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Currency {
    Usd,
    Khr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Money {
    amount_minor: i64,
    currency: Currency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MoneyError {
    #[error("amount must be positive, got {0}")]
    NonPositive(i64),
    #[error("cannot combine {left:?} with {right:?}")]
    CurrencyMismatch { left: Currency, right: Currency },
    #[error("amount overflowed")]
    Overflow,
}

impl Money {
    pub fn new(amount_minor: i64, currency: Currency) -> Result<Self, MoneyError> {
        if amount_minor <= 0 {
            return Err(MoneyError::NonPositive(amount_minor));
        }
        Ok(Self {
            amount_minor,
            currency,
        })
    }

    pub fn amount_minor(self) -> i64 {
        self.amount_minor
    }

    pub fn currency(self) -> Currency {
        self.currency
    }

    pub fn checked_add(self, other: Self) -> Result<Self, MoneyError> {
        if self.currency != other.currency {
            return Err(MoneyError::CurrencyMismatch {
                left: self.currency,
                right: other.currency,
            });
        }
        let amount_minor = self
            .amount_minor
            .checked_add(other.amount_minor)
            .ok_or(MoneyError::Overflow)?;
        Ok(Self {
            amount_minor,
            currency: self.currency,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usd(amount_minor: i64) -> Money {
        Money::new(amount_minor, Currency::Usd).unwrap()
    }

    #[test]
    fn rejects_zero_and_negative_amounts() {
        assert_eq!(
            Money::new(0, Currency::Usd),
            Err(MoneyError::NonPositive(0))
        );
        assert_eq!(
            Money::new(-5, Currency::Khr),
            Err(MoneyError::NonPositive(-5))
        );
    }

    #[test]
    fn adds_same_currency() {
        assert_eq!(usd(250).checked_add(usd(750)), Ok(usd(1000)));
    }

    #[test]
    fn refuses_to_add_different_currencies() {
        let khr = Money::new(4000, Currency::Khr).unwrap();
        assert_eq!(
            usd(1).checked_add(khr),
            Err(MoneyError::CurrencyMismatch {
                left: Currency::Usd,
                right: Currency::Khr
            })
        );
    }

    #[test]
    fn reports_overflow_instead_of_wrapping() {
        assert_eq!(usd(i64::MAX).checked_add(usd(1)), Err(MoneyError::Overflow));
    }
}
