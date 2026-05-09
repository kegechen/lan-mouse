use std::{collections::HashMap, net::SocketAddr, time::Duration};

use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::Sender;
use tokio::task::JoinHandle;

use lan_mouse_ipc::ClientHandle;

use super::{capture_task::CaptureRequest, emulation_task::EmulationRequest, Server, State};

// 单次 ping round 等多久 client 回 alive。Mobile Hotspot / WiFi Direct
// 在系统弹窗、power state 变化时会有短暂转发停顿（Connected Standby +
// WLAN AutoConfig），原 500ms 太苛刻。
const MAX_RESPONSE_TIME: Duration = Duration::from_millis(2000);
// 连续多少次 ping miss 才判失联。单次抖动不立即 release pointer，避免漫游过程中
// 一个弹窗就把光标拽回 Windows。
const MAX_PING_MISSES: u32 = 3;

pub(crate) fn new(
    server: Server,
    sender_ch: Sender<(ProtoEvent, SocketAddr)>,
    emulate_notify: Sender<EmulationRequest>,
    capture_notify: Sender<CaptureRequest>,
) -> JoinHandle<()> {
    // timer task
    tokio::task::spawn_local(async move {
        tokio::select! {
            _ = server.notifies.cancel.cancelled() => {}
            _ = ping_task(&server, sender_ch, emulate_notify, capture_notify) => {}
        }
    })
}

async fn ping_task(
    server: &Server,
    sender_ch: Sender<(ProtoEvent, SocketAddr)>,
    emulate_notify: Sender<EmulationRequest>,
    capture_notify: Sender<CaptureRequest>,
) {
    // 跨 ping round 累积每个 client 连续 miss 次数；本轮响应到了即清零。
    let mut miss_counts: HashMap<ClientHandle, u32> = HashMap::new();
    loop {
        // wait for wake up signal
        server.ping_timer_notified().await;
        loop {
            let receiving = server.state.get() == State::Receiving;
            let (ping_clients, ping_addrs) = {
                let mut client_manager = server.client_manager.borrow_mut();

                let ping_clients: Vec<ClientHandle> = if receiving {
                    // if receiving we care about clients with pressed keys
                    client_manager
                        .get_client_states()
                        .filter(|(_, (_, s))| s.has_pressed_keys)
                        .map(|(h, _)| h)
                        .collect()
                } else {
                    // if sending we care about the active client
                    server.active_client.get().iter().cloned().collect()
                };

                // get relevant socket addrs for clients
                let ping_addrs: Vec<SocketAddr> = {
                    ping_clients
                        .iter()
                        .flat_map(|&h| client_manager.get(h))
                        .flat_map(|(c, s)| {
                            if s.alive && s.active_addr.is_some() {
                                vec![s.active_addr.unwrap()]
                            } else {
                                s.ips
                                    .iter()
                                    .cloned()
                                    .map(|ip| SocketAddr::new(ip, c.port))
                                    .collect()
                            }
                        })
                        .collect()
                };

                // reset alive
                for (_, (_, s)) in client_manager.get_client_states_mut() {
                    s.alive = false;
                }

                (ping_clients, ping_addrs)
            };

            if receiving && ping_clients.is_empty() {
                // receiving and no client has pressed keys
                // -> no need to keep pinging
                break;
            }

            // ping clients
            for addr in ping_addrs {
                if sender_ch.send((ProtoEvent::Ping, addr)).is_err() {
                    break;
                }
            }

            // give clients time to resond
            if receiving {
                log::trace!(
                    "waiting {MAX_RESPONSE_TIME:?} for response from client with pressed keys ..."
                );
            } else {
                log::trace!(
                    "state: {:?} => waiting {MAX_RESPONSE_TIME:?} for client to respond ...",
                    server.state.get()
                );
            }

            tokio::time::sleep(MAX_RESPONSE_TIME).await;

            // when anything is received from a client,
            // the alive flag gets set
            let unresponsive_clients: Vec<_> = {
                let client_manager = server.client_manager.borrow();
                ping_clients
                    .iter()
                    .filter_map(|&h| match client_manager.get(h) {
                        Some((_, s)) if !s.alive => Some(h),
                        _ => None,
                    })
                    .collect()
            };

            // 更新连续 miss 计数：本轮响应过的 client 清 0，未响应的 +1。
            for &h in &ping_clients {
                if unresponsive_clients.contains(&h) {
                    *miss_counts.entry(h).or_insert(0) += 1;
                } else {
                    miss_counts.remove(&h);
                }
            }

            // 只有连续 miss >= MAX_PING_MISSES 才真正判失联。
            let release_clients: Vec<ClientHandle> = unresponsive_clients
                .iter()
                .copied()
                .filter(|h| miss_counts.get(h).copied().unwrap_or(0) >= MAX_PING_MISSES)
                .collect();

            // we may not be receiving anymore but we should respond
            // to the original state and not the "new" one
            if receiving {
                for h in &release_clients {
                    log::warn!(
                        "device not responding ({MAX_PING_MISSES} consecutive misses), releasing keys!"
                    );
                    let _ = emulate_notify.send(EmulationRequest::ReleaseKeys(*h));
                    miss_counts.remove(h);
                }
            } else if !release_clients.is_empty() {
                log::warn!(
                    "client not responding ({MAX_PING_MISSES} consecutive misses), releasing pointer!"
                );
                server.state.replace(State::Receiving);
                let _ = capture_notify.send(CaptureRequest::Release);
                for h in &release_clients {
                    miss_counts.remove(h);
                }
            }
        }
    }
}
