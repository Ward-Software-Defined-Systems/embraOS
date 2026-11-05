//! Whether WardSONDB is running: a copy of its files while it writes them
//! would be inconsistent, so a snapshot or a restore runs only while the
//! loopback port is closed — before the services start.

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub fn database_is_running() -> bool {
    listens(crate::paths::DB_ADDR)
}

pub fn listens(addr: &str) -> bool {
    let Ok(mut addrs) = addr.to_socket_addrs() else { return false };
    let Some(a) = addrs.next() else { return false };
    TcpStream::connect_timeout(&a, Duration::from_millis(200)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_port_reads_as_not_running_and_an_open_one_as_running() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(listens(&addr.to_string()));
        drop(listener);
        assert!(!listens(&addr.to_string()));
        assert!(!listens("not an address"));
    }
}
