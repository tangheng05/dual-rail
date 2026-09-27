use std::collections::BTreeMap;

use thiserror::Error;

use crate::code::string_codes;
use crate::{Currency, Money, Provider};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Account {
    ClearingStripe,
    ClearingBakong,
    Revenue,
}

impl Account {
    pub fn code(self) -> &'static str {
        match self {
            Self::ClearingStripe => "clearing:stripe",
            Self::ClearingBakong => "clearing:bakong",
            Self::Revenue => "revenue",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Debit,
    Credit,
}

string_codes!(Direction, "ledger direction", {
    Direction::Debit => "debit",
    Direction::Credit => "credit",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerLine {
    pub account: Account,
    pub direction: Direction,
    pub amount: Money,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    lines: Vec<LedgerLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LedgerError {
    #[error("journal entry has no lines")]
    Empty,
    #[error("{currency:?} debits {debits} do not equal credits {credits}")]
    Unbalanced {
        currency: Currency,
        debits: i64,
        credits: i64,
    },
    #[error("journal entry total overflowed")]
    Overflow,
}

impl JournalEntry {
    pub fn new(lines: Vec<LedgerLine>) -> Result<Self, LedgerError> {
        if lines.is_empty() {
            return Err(LedgerError::Empty);
        }

        let mut totals: BTreeMap<Currency, (i64, i64)> = BTreeMap::new();
        for line in &lines {
            let (debits, credits) = totals.entry(line.amount.currency()).or_default();
            let side = match line.direction {
                Direction::Debit => debits,
                Direction::Credit => credits,
            };
            *side = side
                .checked_add(line.amount.amount_minor())
                .ok_or(LedgerError::Overflow)?;
        }

        for (currency, (debits, credits)) in totals {
            if debits != credits {
                return Err(LedgerError::Unbalanced {
                    currency,
                    debits,
                    credits,
                });
            }
        }

        Ok(Self { lines })
    }

    pub fn for_successful_payment(provider: Provider, amount: Money) -> Self {
        let clearing = match provider {
            Provider::Stripe => Account::ClearingStripe,
            Provider::Bakong => Account::ClearingBakong,
        };
        Self {
            lines: vec![
                LedgerLine {
                    account: clearing,
                    direction: Direction::Debit,
                    amount,
                },
                LedgerLine {
                    account: Account::Revenue,
                    direction: Direction::Credit,
                    amount,
                },
            ],
        }
    }

    pub fn lines(&self) -> &[LedgerLine] {
        &self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(
        account: Account,
        direction: Direction,
        amount_minor: i64,
        currency: Currency,
    ) -> LedgerLine {
        LedgerLine {
            account,
            direction,
            amount: Money::new(amount_minor, currency).unwrap(),
        }
    }

    #[test]
    fn successful_payment_entries_always_balance() {
        for provider in [Provider::Stripe, Provider::Bakong] {
            for currency in [Currency::Usd, Currency::Khr] {
                for amount_minor in [1, 1000, i64::MAX] {
                    let amount = Money::new(amount_minor, currency).unwrap();
                    let entry = JournalEntry::for_successful_payment(provider, amount);
                    assert_eq!(JournalEntry::new(entry.lines().to_vec()), Ok(entry));
                }
            }
        }
    }

    #[test]
    fn successful_payment_debits_the_provider_clearing_account() {
        let amount = Money::new(1000, Currency::Usd).unwrap();
        for (provider, clearing) in [
            (Provider::Stripe, Account::ClearingStripe),
            (Provider::Bakong, Account::ClearingBakong),
        ] {
            let entry = JournalEntry::for_successful_payment(provider, amount);
            assert_eq!(
                entry.lines(),
                [
                    LedgerLine {
                        account: clearing,
                        direction: Direction::Debit,
                        amount
                    },
                    LedgerLine {
                        account: Account::Revenue,
                        direction: Direction::Credit,
                        amount
                    },
                ]
            );
        }
    }

    #[test]
    fn rejects_empty_entry() {
        assert_eq!(JournalEntry::new(vec![]), Err(LedgerError::Empty));
    }

    #[test]
    fn rejects_unbalanced_entry() {
        let result = JournalEntry::new(vec![
            line(
                Account::ClearingStripe,
                Direction::Debit,
                1000,
                Currency::Usd,
            ),
            line(Account::Revenue, Direction::Credit, 999, Currency::Usd),
        ]);
        assert_eq!(
            result,
            Err(LedgerError::Unbalanced {
                currency: Currency::Usd,
                debits: 1000,
                credits: 999
            })
        );
    }

    #[test]
    fn balances_each_currency_separately() {
        let result = JournalEntry::new(vec![
            line(
                Account::ClearingStripe,
                Direction::Debit,
                1000,
                Currency::Usd,
            ),
            line(Account::Revenue, Direction::Credit, 1000, Currency::Khr),
        ]);
        assert!(matches!(result, Err(LedgerError::Unbalanced { .. })));
    }

    #[test]
    fn reports_overflow_instead_of_wrapping() {
        let result = JournalEntry::new(vec![
            line(
                Account::ClearingStripe,
                Direction::Debit,
                i64::MAX,
                Currency::Usd,
            ),
            line(Account::ClearingStripe, Direction::Debit, 1, Currency::Usd),
            line(Account::Revenue, Direction::Credit, i64::MAX, Currency::Usd),
        ]);
        assert_eq!(result, Err(LedgerError::Overflow));
    }
}
