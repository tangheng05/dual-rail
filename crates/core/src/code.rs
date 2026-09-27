use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown {kind} {value:?}")]
pub struct ParseCodeError {
    pub kind: &'static str,
    pub value: String,
}

macro_rules! string_codes {
    ($type:ty, $kind:literal, { $($variant:path => $code:literal),+ $(,)? }) => {
        impl $type {
            pub fn as_str(self) -> &'static str {
                match self {
                    $($variant => $code),+
                }
            }
        }

        impl std::str::FromStr for $type {
            type Err = $crate::ParseCodeError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($code => Ok($variant),)+
                    _ => Err($crate::ParseCodeError { kind: $kind, value: value.to_owned() }),
                }
            }
        }

        impl std::fmt::Display for $type {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

pub(crate) use string_codes;
