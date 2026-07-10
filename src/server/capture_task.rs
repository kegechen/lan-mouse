use futures::StreamExt;
use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::{Receiver, Sender};
use std::{collections::HashSet, net::SocketAddr};

use tokio::{process::Command, task::JoinHandle};

use input_capture::{
    self, CaptureError, CaptureEvent, CaptureHandle, InputCapture, InputCaptureError, Position,
};

use crate::server::State;
use lan_mouse_ipc::{ClientHandle, Status};

use super::Server;

#[derive(Clone, Copy, Debug)]
pub(crate) enum CaptureRequest {
    /// capture must release the mouse
    Release,
    /// add a capture client
    Create(CaptureHandle, Position),
    /// destory a capture client
    Destroy(CaptureHandle),
}

pub(crate) fn new(
    server: Server,
    capture_rx: Receiver<CaptureRequest>,
    udp_send: Sender<(ProtoEvent, SocketAddr)>,
) -> JoinHandle<()> {
    let backend = server.config.capture_backend.map(|b| b.into());
    tokio::task::spawn_local(capture_task(server, backend, udp_send, capture_rx))
}

async fn capture_task(
    server: Server,
    backend: Option<input_capture::Backend>,
    sender_tx: Sender<(ProtoEvent, SocketAddr)>,
    mut notify_rx: Receiver<CaptureRequest>,
) {
    loop {
        if let Err(e) = do_capture(backend, &server, &sender_tx, &mut notify_rx).await {
            log::warn!("input capture exited: {e}");
        }
        server.set_capture_status(Status::Disabled);
        if server.is_cancelled() {
            break;
        }

        // allow cancellation
        loop {
            tokio::select! {
                _ = notify_rx.recv() => continue, /* need to ignore requests here! */
                _ = server.capture_enabled() => break,
                _ = server.cancelled() => return,
            }
        }
    }
}

async fn do_capture(
    backend: Option<input_capture::Backend>,
    server: &Server,
    sender_tx: &Sender<(ProtoEvent, SocketAddr)>,
    notify_rx: &mut Receiver<CaptureRequest>,
) -> Result<(), InputCaptureError> {
    /* allow cancelling capture request */
    let mut capture = tokio::select! {
        r = InputCapture::new(backend) => r?,
        _ = server.cancelled() => return Ok(()),
    };

    server.set_capture_status(Status::Enabled);

    // Tracks which capture handles currently have an edge barrier, so that
    // Create/Destroy requests are idempotent (InputCapture::create panics on a
    // duplicate handle and destroy expects an existing one). Reset on every
    // (re)start of capture, matching the fresh InputCapture instance.
    let mut created: HashSet<CaptureHandle> = HashSet::new();

    // Only create barriers for clients that are both active and connected;
    // the keepalive task adds the rest once their peers respond.
    let initial: Vec<(CaptureHandle, Position)> = {
        let client_manager = server.client_manager.borrow();
        client_manager
            .get_client_states()
            .filter(|(_, (_, s))| s.active && s.connected)
            .map(|(h, (c, _))| (h, to_capture_pos(c.pos)))
            .collect()
    };
    for (handle, pos) in initial {
        capture.create(handle, pos).await?;
        created.insert(handle);
    }

    loop {
        tokio::select! {
            event = capture.next() => match event {
                Some(event) => {
                    if !handle_capture_event(server, &mut capture, sender_tx, event?).await? {
                        break;
                    }
                }
                None => return Ok(()),
            },
            e = notify_rx.recv() => {
                log::debug!("input capture notify rx: {e:?}");
                match e {
                    Some(e) => match e {
                        CaptureRequest::Release => {
                            log::info!("CaptureRequest::Release → capture.release()");
                            capture.release().await?;
                            server.state.replace(State::Receiving);
                        }
                        CaptureRequest::Create(h, p) => {
                            // de-dup: only create if we don't already have it
                            if created.insert(h) {
                                capture.create(h, p).await?;
                            }
                        }
                        CaptureRequest::Destroy(h) => {
                            // de-dup: only destroy a barrier that actually exists
                            if created.remove(&h) {
                                capture.destroy(h).await?;
                            }
                        }
                    },
                    None => break,
                }
            }
            _ = server.cancelled() => break,
        }
    }
    capture.terminate().await?;
    Ok(())
}

async fn handle_capture_event(
    server: &Server,
    capture: &mut InputCapture,
    sender_tx: &Sender<(ProtoEvent, SocketAddr)>,
    event: (CaptureHandle, CaptureEvent),
) -> Result<bool, CaptureError> {
    let (handle, event) = event;
    log::trace!("({handle}) {event:?}");

    // capture started
    if event == CaptureEvent::Begin {
        // Safety net: never lock the pointer for a client that is not
        // currently connected. The barrier should already be absent for
        // offline clients, but this guards the race where the pointer reaches
        // the edge in the instant before the keepalive task tears it down.
        if !server.is_connected(handle) {
            log::info!("({handle}) capture begin but client not connected → releasing immediately");
            capture.release().await?;
            return Ok(true);
        }
        // wait for remote to acknowlegde enter
        server.set_state(State::AwaitAck);
        server.set_active(Some(handle));
        // restart ping timer to release capture if unreachable
        server.restart_ping_timer();
        // spawn enter hook cmd
        spawn_hook_command(server, handle);
    }

    // release capture if emulation set state to Receiveing
    if server.get_state() == State::Receiving {
        log::info!("state==Receiving on capture event {event:?} → capture.release() (someone flipped state)");
        capture.release().await?;
        return Ok(true);
    }

    // check release bind
    if capture.keys_pressed(&server.release_bind) {
        log::info!("release_bind pressed → capture.release()");
        capture.release().await?;
        server.set_state(State::Receiving);
    }

    if let Some(addr) = server.active_addr(handle) {
        let event = match server.get_state() {
            State::Sending => match event {
                CaptureEvent::Begin => ProtoEvent::Enter(0),
                CaptureEvent::Input(e) => ProtoEvent::Input(e),
            },
            /* send additional enter events until acknowleged */
            State::AwaitAck => ProtoEvent::Enter(0),
            /* released capture */
            State::Receiving => ProtoEvent::Leave(0),
        };
        if sender_tx.send((event, addr)).is_err() {
            // network task exited first (udp_send_rx dropped); release the
            // pointer if we just acquired it, then signal a clean loop exit
            // so the normal teardown path (capture.terminate()) still runs.
            log::warn!("sender channel closed → releasing capture and exiting capture loop");
            capture.release().await?;
            return Ok(false);
        }
    };

    Ok(true)
}

fn spawn_hook_command(server: &Server, handle: ClientHandle) {
    let Some(cmd) = server
        .client_manager
        .borrow()
        .get(handle)
        .and_then(|(c, _)| c.cmd.clone())
    else {
        return;
    };
    tokio::task::spawn_local(async move {
        log::info!("spawning command!");
        let mut child = match Command::new("sh").arg("-c").arg(cmd.as_str()).spawn() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("could not execute cmd: {e}");
                return;
            }
        };
        match child.wait().await {
            Ok(s) => {
                if s.success() {
                    log::info!("{cmd} exited successfully");
                } else {
                    log::warn!("{cmd} exited with {s}");
                }
            }
            Err(e) => log::warn!("{cmd}: {e}"),
        }
    });
}

fn to_capture_pos(pos: lan_mouse_ipc::Position) -> input_capture::Position {
    match pos {
        lan_mouse_ipc::Position::Left => input_capture::Position::Left,
        lan_mouse_ipc::Position::Right => input_capture::Position::Right,
        lan_mouse_ipc::Position::Top => input_capture::Position::Top,
        lan_mouse_ipc::Position::Bottom => input_capture::Position::Bottom,
    }
}
