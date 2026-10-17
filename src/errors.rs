//! Errors an agent should handle differently from "something broke".

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Not logged in or the session expired: log in (`jet login`, or `jet login start` + `code`).
    AuthRequired,
    /// Ambiguous or missing input; the message lists the choices.
    NeedsInput,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::AuthRequired => "auth_required",
            Kind::NeedsInput => "needs_input",
        }
    }
    pub fn exit_code(self) -> i32 {
        match self {
            Kind::AuthRequired => 2,
            Kind::NeedsInput => 3,
        }
    }
}

#[derive(Debug)]
pub struct JetError {
    pub kind: Kind,
    pub message: String,
}

impl fmt::Display for JetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for JetError {}

pub fn auth_required(message: impl Into<String>) -> anyhow::Error {
    JetError {
        kind: Kind::AuthRequired,
        message: message.into(),
    }
    .into()
}

pub fn input_error(message: impl Into<String>) -> anyhow::Error {
    JetError {
        kind: Kind::NeedsInput,
        message: message.into(),
    }
    .into()
}

/// Like `bail!`, for input the caller has to supply or disambiguate.
macro_rules! needs_input {
    ($($t:tt)*) => {
        return Err($crate::errors::input_error(format!($($t)*)))
    };
}
pub(crate) use needs_input;
