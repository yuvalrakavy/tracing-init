//! UDP multicast beacon listener for instant collector availability detection.
//!
//! Listens for `OTEL:ONLINE\n` and `OTEL:OFFLINE\n` messages on a multicast
//! group to immediately open or close the circuit breaker.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;

use super::circuit_breaker::CircuitState;

/// Start a background task that listens for beacon messages on the given
/// multicast group and port. Returns a `JoinHandle` that should be aborted
/// on shutdown.
pub fn start_beacon_listener(
    state: Arc<CircuitState>,
    group: &str,
    port: u16,
) -> tokio::task::JoinHandle<()> {
    let group_addr: Ipv4Addr = group
        .parse()
        .unwrap_or_else(|_| Ipv4Addr::new(239, 255, 77, 1));
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);

    tokio::spawn(async move {
        let socket = match setup_multicast_socket(bind_addr, group_addr) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("OTel beacon listener failed to start: {e}");
                return;
            }
        };

        let udp = match tokio::net::UdpSocket::from_std(socket) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("OTel beacon listener failed to convert socket: {e}");
                return;
            }
        };

        let mut buf = [0u8; 64];
        loop {
            match udp.recv_from(&mut buf).await {
                Ok((len, _addr)) => {
                    if let Ok(msg) = std::str::from_utf8(&buf[..len]) {
                        let msg = msg.trim();
                        match msg {
                            "OTEL:ONLINE" => state.force_close(),
                            "OTEL:OFFLINE" => state.force_open(),
                            _ => {} // Ignore unknown messages
                        }
                    }
                }
                Err(e) => {
                    eprintln!("OTel beacon listener recv error: {e}");
                    // Brief pause before retrying to avoid tight error loop
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    })
}

/// Set up a UDP socket joined to the multicast group using `socket2` for
/// platform-independent `SO_REUSEADDR` / `SO_REUSEPORT` support.
fn setup_multicast_socket(
    bind_addr: SocketAddrV4,
    group_addr: Ipv4Addr,
) -> Result<std::net::UdpSocket, Box<dyn std::error::Error>> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    // SO_REUSEPORT is available on Unix-like systems
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&socket2::SockAddr::from(bind_addr))?;
    socket.join_multicast_v4(&group_addr, &Ipv4Addr::UNSPECIFIED)?;

    Ok(socket.into())
}
