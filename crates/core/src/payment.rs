use thiserror::Error;

use crate::code::string_codes;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaymentMethod {
    Card,
    Khqr,
}

string_codes!(PaymentMethod, "payment method", {
    PaymentMethod::Card => "card",
    PaymentMethod::Khqr => "khqr",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaymentStatus {
    Pending,
    Succeeded,
    Failed,
    Expired,
}

string_codes!(PaymentStatus, "payment status", {
    PaymentStatus::Pending => "pending",
    PaymentStatus::Succeeded => "succeeded",
    PaymentStatus::Failed => "failed",
    PaymentStatus::Expired => "expired",
});

/// The only statuses a payment can move to. `Pending` is not an outcome, so a
/// transition back to it cannot be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    Succeeded,
    Failed,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("payment is already {from:?} and cannot become {to:?}")]
pub struct InvalidTransition {
    pub from: PaymentStatus,
    pub to: Outcome,
}

impl From<Outcome> for PaymentStatus {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Succeeded => Self::Succeeded,
            Outcome::Failed => Self::Failed,
            Outcome::Expired => Self::Expired,
        }
    }
}

impl PaymentStatus {
    pub fn transition(self, to: Outcome) -> Result<Self, InvalidTransition> {
        match self {
            Self::Pending => Ok(to.into()),
            from => Err(InvalidTransition { from, to }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTCOMES: [Outcome; 3] = [Outcome::Succeeded, Outcome::Failed, Outcome::Expired];

    #[test]
    fn pending_moves_to_any_outcome() {
        for outcome in OUTCOMES {
            assert_eq!(
                PaymentStatus::Pending.transition(outcome),
                Ok(outcome.into())
            );
        }
    }

    #[test]
    fn codes_round_trip() {
        for status in [
            PaymentStatus::Pending,
            PaymentStatus::Succeeded,
            PaymentStatus::Failed,
            PaymentStatus::Expired,
        ] {
            assert_eq!(status.as_str().parse(), Ok(status));
        }
        for method in [PaymentMethod::Card, PaymentMethod::Khqr] {
            assert_eq!(method.as_str().parse(), Ok(method));
        }
        assert!("paid".parse::<PaymentStatus>().is_err());
    }

    #[test]
    fn terminal_statuses_never_change() {
        for from in OUTCOMES.map(PaymentStatus::from) {
            for to in OUTCOMES {
                assert_eq!(from.transition(to), Err(InvalidTransition { from, to }));
            }
        }
    }
}
