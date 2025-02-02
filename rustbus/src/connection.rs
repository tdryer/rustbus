//! Different connection types you will need to talk to the bus
//!
//! * ll_conn is the basic send and recive primitives used to build the other connection types
//! * dispatch_conn is meant for services that need to dispatch calls to different handlers
//! * rpc_conn is meant for clients that make calls to services on the bus

pub mod dispatch_conn;
pub mod ll_conn;
pub mod rpc_conn;

use std::path::PathBuf;
use std::time;

use thiserror::Error;

#[derive(Clone, Copy)]
pub enum Timeout {
    Infinite,
    Nonblock,
    Duration(time::Duration),
}

use nix::sys::socket::UnixAddr;

/// Errors that can occur when using the Conn/RpcConn
#[derive(Debug, Error)]
pub enum Error {
    #[error("An io error occured: {0}")]
    IoError(std::io::Error),
    #[error("A nix error occured: {0}")]
    NixError(nix::Error),
    #[error("An error occured while unmarshalling: {0}")]
    UnmarshalError(crate::wire::errors::UnmarshalError),
    #[error("An error occured while marshalling: {0}")]
    MarshalError(crate::wire::errors::MarshalError),
    #[error("Authentication failed")]
    AuthFailed,
    #[error("Negotiating unix fd usage failed")]
    UnixFdNegotiationFailed,
    #[error("The name is already taken")]
    NameTaken,
    #[error("The address type {0} is not yet supportd by this lib")]
    AddressTypeNotSupported(String),
    #[error("This path does not exist: {0}")]
    PathDoesNotExist(String),
    #[error("Address not found")]
    NoAddressFound,
    #[error("Unexpected message type received")]
    UnexpectedMessageTypeReceived,
    #[error("Timeout occured")]
    TimedOut,
    #[error("Connection has been closed by the other side")]
    ConnectionClosed,
}

impl std::convert::From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::IoError(e)
    }
}

impl std::convert::From<crate::wire::errors::UnmarshalError> for Error {
    fn from(e: crate::wire::errors::UnmarshalError) -> Error {
        Error::UnmarshalError(e)
    }
}

impl std::convert::From<nix::Error> for Error {
    fn from(e: nix::Error) -> Error {
        Error::NixError(e)
    }
}

impl std::convert::From<crate::wire::errors::MarshalError> for Error {
    fn from(e: crate::wire::errors::MarshalError) -> Error {
        Error::MarshalError(e)
    }
}

type Result<T> = std::result::Result<T, Error>;

fn parse_dbus_addr_str(addr: &str) -> Result<UnixAddr> {
    if addr.starts_with("unix:path=") {
        let ps = addr.trim_start_matches("unix:path=");
        let p = PathBuf::from(&ps);
        if p.exists() {
            Ok(UnixAddr::new(&p)?)
        } else {
            Err(Error::PathDoesNotExist(ps.to_owned()))
        }
    } else if addr.starts_with("unix:abstract=") {
        #[cfg(not(target_os = "linux"))]
        {
            Err(Error::AddressTypeNotSupported(addr.to_owned()))
        }
        #[cfg(target_os = "linux")]
        {
            let mut ps = addr.trim_start_matches("unix:abstract=").to_string();
            let end_path_offset = ps.find(',').unwrap_or(ps.len());
            let ps: String = ps.drain(..end_path_offset).collect();
            let path_buf = ps.as_bytes();
            Ok(UnixAddr::new_abstract(path_buf)?)
        }
    } else {
        Err(Error::AddressTypeNotSupported(addr.to_owned()))
    }
}

/// Convenience function that returns the UnixAddr of the session bus according to the env
/// var $DBUS_SESSION_BUS_ADDRESS.
pub fn get_session_bus_path() -> Result<UnixAddr> {
    if let Ok(envvar) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        parse_dbus_addr_str(&envvar)
    } else {
        Err(Error::NoAddressFound)
    }
}

/// Convenience function that returns a path to the system bus at /run/dbus/systemd_bus_socket
pub fn get_system_bus_path() -> Result<UnixAddr> {
    let ps = "/run/dbus/system_bus_socket";
    let p = PathBuf::from(&ps);
    if p.exists() {
        Ok(UnixAddr::new(&p)?)
    } else {
        Err(Error::PathDoesNotExist(ps.to_owned()))
    }
}

pub(crate) fn calc_timeout_left(start_time: &time::Instant, timeout: Timeout) -> Result<Timeout> {
    match timeout {
        Timeout::Duration(timeout) => {
            let elapsed = start_time.elapsed();
            if elapsed >= timeout {
                return Err(Error::TimedOut);
            }
            let time_left = timeout - elapsed;
            Ok(Timeout::Duration(time_left))
        }
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::socket::UnixAddr;

    #[cfg(target_os = "linux")]
    #[test]
    fn test_get_session_bus_path() {
        let path = "unix:path=/tmp/dbus-test-not-exist";
        let abstract_path = "unix:abstract=/tmp/dbus-test";
        let abstract_path_with_keys = "unix:abstract=/tmp/dbus-test,guid=aaaaaaaa,test=bbbbbbbb";

        let addr = parse_dbus_addr_str(path);
        assert!(addr.is_err());

        let addr = parse_dbus_addr_str(abstract_path).unwrap();
        assert_eq!(addr, UnixAddr::new_abstract(b"/tmp/dbus-test").unwrap());

        let addr = parse_dbus_addr_str(abstract_path_with_keys).unwrap();
        assert_eq!(addr, UnixAddr::new_abstract(b"/tmp/dbus-test").unwrap());
    }
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn test_get_session_bus_path() {
        let path = "unix:path=/tmp/dbus-test-not-exist";

        let addr = parse_dbus_addr_str(path);
        assert!(addr.is_err());
    }
}
