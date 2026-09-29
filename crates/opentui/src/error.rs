use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A native constructor returned an invalid handle. OpenTUI logs the
    /// reason through its log callback (e.g. zero dimensions, out of memory).
    CreateFailed(&'static str),
    /// A native call refused its arguments or failed.
    CallFailed(&'static str),
    /// OpenTUI objects are alive on another thread; see the crate docs.
    WrongThread,
    /// An image couldn't be decoded, for the reason given.
    Image(&'static str),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::CreateFailed(what) => write!(f, "OpenTUI failed to create {what}"),
            Error::CallFailed(what) => write!(f, "OpenTUI {what} failed"),
            Error::WrongThread => f.write_str("OpenTUI objects are in use on another thread"),
            Error::Image(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for Error {}
