use local_channel::mpsc::{Receiver, Sender};
use std::{
    cell::Cell,
    collections::HashMap,
    io,
    net::SocketAddr,
    time::{SystemTime, UNIX_EPOCH},
};

use thiserror::Error;
use tokio::{net::UdpSocket, task::JoinHandle};

use super::Server;
use lan_mouse_proto::{
    decode_authenticated, encode_authenticated, ProtoEvent, ProtocolError, ReplayWindow,
    MAX_AUTH_DATAGRAM,
};

pub(crate) async fn new(
    server: Server,
    udp_recv_tx: Sender<Result<(ProtoEvent, SocketAddr), NetworkError>>,
    udp_send_rx: Receiver<(ProtoEvent, SocketAddr)>,
) -> io::Result<JoinHandle<()>> {
    // bind the udp socket
    let listen_addr = SocketAddr::new("0.0.0.0".parse().unwrap(), server.port.get());
    let mut socket = UdpSocket::bind(listen_addr).await?;

    // Clone the authentication key for the network task.
    let auth_key: Option<Vec<u8>> = server.config.authentication_key.clone();

    if auth_key.is_none() {
        log::error!(
            "SECURITY: authentication_key is not configured! \
             All remote UDP events will be REJECTED (fail-closed). \
             Set 'authentication_key' in config.toml or the LAN_MOUSE_AUTH_KEY \
             environment variable to enable peer authentication."
        );
    }

    Ok(tokio::task::spawn_local(async move {
        // Monotonically increasing counter for outgoing datagrams.
        //
        // Seed it with the current wall-clock time (µs since the UNIX epoch)
        // instead of 0. The counter must be strictly increasing across
        // *restarts*, not just within a single run: a peer's anti-replay window
        // persists while we are down, so a fresh 0-based counter would be
        // rejected as a replay ("too old", outside the window) until it climbed
        // back past the peer's stored high-water mark — which silently broke the
        // link after restarting only one side. Seeding from wall-clock µs makes
        // the new counter start above the previous run's highest (as long as the
        // clock advanced and we send < 1e6 datagrams/s), so a still-running peer
        // accepts us immediately. We still increment by 1 per datagram, keeping
        // the receiver window's 64-wide reordering tolerance intact.
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        let send_counter: Cell<u64> = Cell::new(seed);
        // Per-peer replay windows for incoming datagrams, keyed by source address.
        let mut replay_windows: HashMap<SocketAddr, ReplayWindow> = HashMap::new();

        let mut sender_rx = udp_send_rx;
        loop {
            let udp_receiver = udp_receiver(
                &socket,
                &udp_recv_tx,
                auth_key.as_deref(),
                &mut replay_windows,
            );
            let udp_sender = udp_sender(
                &socket,
                &mut sender_rx,
                auth_key.as_deref(),
                &send_counter,
            );
            tokio::select! {
                _ = udp_receiver => break, /* channel closed */
                _ = udp_sender => break, /* channel closed */
                _ = server.notifies.port_changed.notified() => update_port(&server, &mut socket).await,
                _ = server.cancelled() => break, /* cancellation requested */
            }
        }
    }))
}

async fn update_port(server: &Server, socket: &mut UdpSocket) {
    let new_port = server.port.get();
    let current_port = socket.local_addr().expect("socket not bound").port();

    // if port is the same, we dont need to change it
    if current_port == new_port {
        return;
    }

    // bind new socket
    let listen_addr = SocketAddr::new("0.0.0.0".parse().unwrap(), new_port);
    let new_socket = UdpSocket::bind(listen_addr).await;
    let err = match new_socket {
        Ok(new_socket) => {
            *socket = new_socket;
            None
        }
        Err(e) => Some(e.to_string()),
    };

    // notify frontend of the actual port
    let port = socket.local_addr().expect("socket not bound").port();
    server.notify_port_changed(port, err);
}

async fn udp_receiver(
    socket: &UdpSocket,
    receiver_tx: &Sender<Result<(ProtoEvent, SocketAddr), NetworkError>>,
    auth_key: Option<&[u8]>,
    replay_windows: &mut HashMap<SocketAddr, ReplayWindow>,
) {
    loop {
        let event = receive_event(socket, auth_key, replay_windows).await;
        // `None` means the packet was silently dropped (auth/replay failure).
        let Some(event) = event else {
            continue;
        };
        if receiver_tx.send(event).is_err() {
            // emulation task exited first (receiver dropped) → end cleanly,
            // the outer select! treats this return as a break condition.
            log::warn!("udp receiver channel closed → exiting network receiver");
            return;
        }
    }
}

async fn udp_sender(
    socket: &UdpSocket,
    rx: &mut Receiver<(ProtoEvent, SocketAddr)>,
    auth_key: Option<&[u8]>,
    send_counter: &Cell<u64>,
) {
    loop {
        let (event, addr) = match rx.recv().await {
            Some(v) => v,
            None => {
                // capture task exited first (sender dropped) → end cleanly,
                // the outer select! treats this return as a break condition.
                log::warn!("udp sender channel closed → exiting network sender");
                return;
            }
        };
        if let Err(e) = send_event(socket, event, addr, auth_key, send_counter) {
            log::warn!("udp send failed: {e}");
        };
    }
}

#[derive(Debug, Error)]
pub(crate) enum NetworkError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("network error: `{0}`")]
    Io(#[from] io::Error),
}

/// Receive, authenticate, and anti-replay-check a single datagram.
///
/// Returns `None` when the packet must be silently dropped (fail-closed,
/// auth failure, replay, or bad length).  The caller should `continue`.
async fn receive_event(
    socket: &UdpSocket,
    auth_key: Option<&[u8]>,
    replay_windows: &mut HashMap<SocketAddr, ReplayWindow>,
) -> Option<Result<(ProtoEvent, SocketAddr), NetworkError>> {
    let mut buf = [0u8; MAX_AUTH_DATAGRAM];
    let (len, src) = match socket.recv_from(&mut buf).await {
        Ok(v) => v,
        Err(e) => return Some(Err(NetworkError::Io(e))),
    };

    let Some(key) = auth_key else {
        // Fail-closed: no authentication key configured → drop ALL remote
        // packets.  The startup log::error already informed the operator.
        log::debug!("dropping packet from {src}: no authentication_key configured (fail-closed)");
        return None;
    };

    // Use the ACTUAL received length — not the full buffer.
    let datagram = &buf[..len];

    let (counter, event) = match decode_authenticated(datagram, key) {
        Ok(v) => v,
        Err(ProtocolError::AuthenticationFailed) => {
            log::debug!("dropping packet from {src}: authentication failed");
            return None;
        }
        Err(ProtocolError::BadLength(l)) => {
            log::debug!("dropping packet from {src}: bad datagram length {l}");
            return None;
        }
        Err(e) => return Some(Err(NetworkError::Protocol(e))),
    };

    // Anti-replay: per-source-address sliding window.
    let window = replay_windows.entry(src).or_insert_with(ReplayWindow::new);
    if !window.check_and_record(counter) {
        log::debug!("dropping replayed packet from {src} (counter={counter})");
        return None;
    }

    Some(Ok((event, src)))
}

fn send_event(
    sock: &UdpSocket,
    e: ProtoEvent,
    addr: SocketAddr,
    auth_key: Option<&[u8]>,
    send_counter: &Cell<u64>,
) -> Result<usize, NetworkError> {
    log::trace!("{:20} ------>->->-> {addr}", e.to_string());

    let Some(key) = auth_key else {
        // No authentication key — cannot send authenticated datagrams.
        // This matches the fail-closed receive side: if we can't authenticate
        // we should not send unauthenticated packets either.
        log::debug!("not sending to {addr}: no authentication_key configured");
        return Ok(0);
    };

    let counter = send_counter.get();
    send_counter.set(counter.wrapping_add(1));

    let (data, len) = encode_authenticated(&e, key, counter);
    // When udp blocks, we dont want to block the event loop.
    // Dropping events is better than potentially crashing the input capture.
    Ok(sock.try_send_to(&data[..len], addr)?)
}
