//! UDP link between server and client.
//!
//! The client says `hello` to the server every few seconds (the server address
//! comes from deskflow-core's log or `[kvm.effect].peer`); the server sends
//! enter/leave events to the clients it heard from recently, or broadcasts on
//! the LAN when it has not heard from anybody yet.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::proto::Msg;

/// A client counts as present this long after its last hello.
const PEER_TTL: Duration = Duration::from_secs(12);

pub struct Link {
    sock: UdpSocket,
    port: u16,
    /// Clients that said hello recently (server side).
    clients: Mutex<HashMap<SocketAddr, (String, Instant)>>,
}

fn resolve(host: &str, port: u16) -> Option<SocketAddr> {
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    (host, port).to_socket_addrs().ok()?.find(SocketAddr::is_ipv4)
}

impl Link {
    pub fn bind(port: u16) -> io::Result<Self> {
        Self::bind_on(Ipv4Addr::UNSPECIFIED, port)
    }

    fn bind_on(ip: Ipv4Addr, port: u16) -> io::Result<Self> {
        let sock = UdpSocket::bind((ip, port))?;
        sock.set_broadcast(true)?;
        sock.set_read_timeout(Some(Duration::from_millis(500)))?;
        Ok(Self { sock, port, clients: Mutex::new(HashMap::new()) })
    }

    fn send(&self, msg: &Msg, to: SocketAddr) {
        if let Err(e) = self.sock.send_to(msg.encode().as_bytes(), to) {
            log::debug!("effect: send to {to}: {e}");
        }
    }

    /// Client → server.
    pub fn hello(&self, server: &str, name: &str) {
        if let Some(addr) = resolve(server, self.port) {
            self.send(&Msg::Hello { name: name.to_string() }, addr);
        }
    }

    /// Server → clients (fresh hellos + `peer`; LAN broadcast if there is none).
    pub fn to_clients(&self, msg: &Msg, peer: &str) {
        let now = Instant::now();
        let mut targets: Vec<SocketAddr> = match self.clients.lock() {
            Ok(mut c) => {
                c.retain(|_, (_, seen)| now.duration_since(*seen) < PEER_TTL);
                c.keys().copied().collect()
            }
            Err(_) => Vec::new(),
        };
        if let Some(p) = resolve(peer, self.port) {
            if !targets.contains(&p) {
                targets.push(p);
            }
        }
        if targets.is_empty() {
            targets.push(SocketAddr::from((Ipv4Addr::BROADCAST, self.port)));
        }
        for t in targets {
            self.send(msg, t);
        }
    }

    /// Names of the clients that said hello recently.
    pub fn client_names(&self) -> Vec<String> {
        let now = Instant::now();
        self.clients
            .lock()
            .map(|c| {
                c.values()
                    .filter(|(_, seen)| now.duration_since(*seen) < PEER_TTL)
                    .map(|(n, _)| n.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Waits up to the read timeout for one message. Hellos are recorded here
    /// and also returned.
    pub fn recv(&self) -> Option<(Msg, SocketAddr)> {
        let mut buf = [0u8; 600];
        let (n, from) = self.sock.recv_from(&mut buf).ok()?;
        let msg = Msg::decode(&buf[..n])?;
        if let Msg::Hello { name } = &msg {
            if let Ok(mut c) = self.clients.lock() {
                c.insert(from, (name.clone(), Instant::now()));
            }
        }
        Some((msg, from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_registers_client_and_events_reach_it() {
        // loopback only, so the test never triggers a firewall prompt
        let server = Link::bind_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let server_port = server.sock.local_addr().unwrap().port();
        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        client
            .send_to(Msg::Hello { name: "mac".into() }.encode().as_bytes(), (Ipv4Addr::LOCALHOST, server_port))
            .unwrap();

        let (msg, _) = server.recv().expect("hello");
        assert_eq!(msg, Msg::Hello { name: "mac".into() });
        assert_eq!(server.client_names(), vec!["mac".to_string()]);

        let ev = Msg::Enter { x: 1, y: 2, name: "mac".into() };
        server.to_clients(&ev, "");
        let mut buf = [0u8; 600];
        let (n, _) = client.recv_from(&mut buf).unwrap();
        assert_eq!(Msg::decode(&buf[..n]), Some(ev));
    }
}
