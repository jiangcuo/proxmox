//! Error types.

use std::error::Error as StdError;
use std::fmt;

/// Ticket parsing error.
#[derive(Clone, Copy, Debug)]
pub struct TicketError;

impl std::error::Error for TicketError {}

impl fmt::Display for TicketError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("invalid ticket")
    }
}

/// Error parsing an API response.
#[derive(Debug)]
pub enum ResponseError {
    /// An error happened when decoding the JSON response.
    Json(serde_json::Error),

    /// Some unexpected error occurred.
    Msg(&'static str),

    /// Failed to parse the ticket contained in the response.
    Ticket(TicketError),
}

impl StdError for ResponseError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Json(err) => Some(err),
            _ => None,
        }
    }
}

impl fmt::Display for ResponseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Json(err) => write!(f, "bad ticket response: {err}"),
            Self::Msg(err) => write!(f, "bad ticket response: {err}"),
            Self::Ticket(err) => write!(f, "failed to parse ticket in response: {err}"),
        }
    }
}

impl From<serde_json::Error> for ResponseError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

impl From<&'static str> for ResponseError {
    fn from(err: &'static str) -> Self {
        Self::Msg(err)
    }
}

impl From<TicketError> for ResponseError {
    fn from(err: TicketError) -> Self {
        Self::Ticket(err)
    }
}

/// Error creating a request for Two-Factor-Authentication.
#[derive(Debug)]
pub enum TfaError {
    /// The chosen TFA method is not available.
    Unavailable,

    /// A serialization error occurred.
    Json(serde_json::Error),
}

impl StdError for TfaError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Json(err) => Some(err),
            _ => None,
        }
    }
}

impl fmt::Display for TfaError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("the chosen TFA method is not available"),
            Self::Json(err) => write!(f, "a serialization error occurred: {err}"),
        }
    }
}

impl From<serde_json::Error> for TfaError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}
