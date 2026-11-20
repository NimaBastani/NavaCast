use std::fmt;
use std::io;

#[derive(Debug)]
pub enum Error {
    NotFound,
    Unauthorized,
    Busy,
    Limit,
    Bitrate,
    BadType,
    TooLarge,
    Invalid(String),
    Forbidden(String),
    Io(io::Error),
    Sql(String),
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound => write!(f, "not found"),
            Error::Unauthorized => write!(f, "unauthorized"),
            Error::Busy => write!(f, "mount busy"),
            Error::Limit => write!(f, "limit reached"),
            Error::Bitrate => write!(f, "bitrate limit"),
            Error::BadType => write!(f, "unsupported media type"),
            Error::TooLarge => write!(f, "too large"),
            Error::Invalid(s) | Error::Forbidden(s) => write!(f, "{s}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Sql(e) => write!(f, "{e}"),
            Error::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Error::Io(value)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(value: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(e, _) = &value {
            if e.code == rusqlite::ErrorCode::ConstraintViolation {
                return Error::Invalid("already exists".into());
            }
        }
        Error::Sql(value.to_string())
    }
}

impl PartialEq for Error {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Error::NotFound, Error::NotFound)
            | (Error::Unauthorized, Error::Unauthorized)
            | (Error::Busy, Error::Busy)
            | (Error::Limit, Error::Limit)
            | (Error::Bitrate, Error::Bitrate)
            | (Error::BadType, Error::BadType)
            | (Error::TooLarge, Error::TooLarge) => true,
            (Error::Invalid(a), Error::Invalid(b))
            | (Error::Forbidden(a), Error::Forbidden(b))
            | (Error::Sql(a), Error::Sql(b))
            | (Error::Other(a), Error::Other(b)) => a == b,
            _ => false,
        }
    }
}

pub fn code_of(err: &Error) -> (&'static str, u16) {
    match err {
        Error::NotFound => ("not_found", 404),
        Error::Unauthorized => ("unauthorized", 401),
        Error::Busy => ("busy", 409),
        Error::Limit => ("limit", 429),
        Error::Bitrate => ("bitrate", 409),
        Error::BadType => ("bad_type", 415),
        Error::TooLarge => ("too_large", 413),
        Error::Invalid(_) => ("invalid", 400),
        Error::Forbidden(_) => ("forbidden", 403),
        Error::Io(_) | Error::Sql(_) | Error::Other(_) => ("internal", 500),
    }
}
