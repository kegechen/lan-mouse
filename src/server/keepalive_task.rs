use std::{collections::HashMap, net::SocketAddr, time::Duration};

use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::Sender;
use tokio::task::JoinHandle;

use lan_mouse_ipc::ClientHandle;

use super::{capture_task::CaptureRequest, Server};

// 空闲保活（background keepalive）。
//
// 即使没有活动会话，也周期性地 ping 所有 active client，维护每个 client 的
// `connected` 状态。只有 active && connected 的 client 才会创建边缘 capture
// 屏障（layer-shell overlay），因此：
//   - 未连接的方向：鼠标撞到该屏幕边缘时不会抓住/定住指针，也没有动画；
//   - 一旦对端上线，keepalive 收到 Pong 后自动补上屏障，漫游立即可用；
//   - 对端掉线，去抖若干轮后自动拆除屏障。
//
// 本任务独立于 ping_task，不改动 ping_task 里针对 WiFi 抖动调过的会话释放逻辑。

// 单轮等待 Pong 的时间，也是保活轮的有效间隔。刻意与 ping_task 的
// MAX_RESPONSE_TIME 对齐（2s），使空闲失联判定容忍窗口 = ping_task 的会话释放
// 容忍窗口（都是 2s×3=6s），不会比会话逻辑更苛刻地误拆屏障；同时把空闲 ping
// 频率降到 2s 一次，减少无谓流量。
const KEEPALIVE_RESPONSE_TIME: Duration = Duration::from_millis(2000);
// 连续多少轮 miss 才判失联并拆除屏障。去抖，容忍 WiFi 瞬时丢包，
// 避免一次抖动就把在用的边缘屏障拆掉。2s×3 = 6s，与 ping_task 一致。
const KEEPALIVE_MAX_MISSES: u32 = 3;

pub(crate) fn new(
    server: Server,
    sender_ch: Sender<(ProtoEvent, SocketAddr)>,
    capture_notify: Sender<CaptureRequest>,
) -> JoinHandle<()> {
    tokio::task::spawn_local(async move {
        tokio::select! {
            _ = server.notifies.cancel.cancelled() => {}
            _ = keepalive_task(&server, sender_ch, capture_notify) => {}
        }
    })
}

async fn keepalive_task(
    server: &Server,
    sender_ch: Sender<(ProtoEvent, SocketAddr)>,
    capture_notify: Sender<CaptureRequest>,
) {
    // 跨轮累积每个 client 的连续 miss 次数。
    let mut miss_counts: HashMap<ClientHandle, u32> = HashMap::new();

    loop {
        // 正在发送的会话目标（Sending/AwaitAck 的 active client）由 ping_task
        // 用调过的 6s 容忍窗口管理其释放，keepalive 本轮跳过它：绝不在会话进行中
        // 拆它的屏障。否则 keepalive 会在 ping_task 之前（且用 Destroy 而非
        // Release，两个后端的 Destroy 都不恢复光标/grab）把在用会话打断。
        let skip = server.active_sending_client();

        // 收集所有（除在用会话目标外的）active client 及其待 ping 地址，
        // 并重置本轮 `responded`。
        let (clients, ping_addrs) = {
            let mut client_manager = server.client_manager.borrow_mut();

            let clients: Vec<ClientHandle> = client_manager
                .get_client_states()
                .filter(|(_, (_, s))| s.active)
                .map(|(h, _)| h)
                .filter(|h| Some(*h) != skip)
                .collect();

            // 清理已不在本轮管理集合内（被 deactivate/删除/正在会话）的陈旧 miss
            // 计数，避免 slab handle 复用时残留计数缩短新 client 的首次判失联。
            miss_counts.retain(|h, _| clients.contains(h));

            let mut ping_addrs: Vec<SocketAddr> = vec![];
            for &h in &clients {
                if let Some((c, s)) = client_manager.get(h) {
                    // 已知 active_addr 优先，否则 ping 所有配置的 ip。
                    if let Some(addr) = s.active_addr {
                        ping_addrs.push(addr);
                    } else {
                        for ip in &s.ips {
                            ping_addrs.push(SocketAddr::new(*ip, c.port));
                        }
                    }
                }
            }

            for &h in &clients {
                if let Some((_, s)) = client_manager.get_mut(h) {
                    s.responded = false;
                }
            }

            (clients, ping_addrs)
        };

        // 发送保活 ping。
        for addr in ping_addrs {
            if sender_ch.send((ProtoEvent::Ping, addr)).is_err() {
                return; // network task 已退出
            }
        }

        // 等待响应窗口（同时充当保活轮的节流间隔）。
        tokio::time::sleep(KEEPALIVE_RESPONSE_TIME).await;

        // 判定每个 client 的连通性变化，按需增删边缘屏障。
        for h in clients {
            let responded = server
                .client_manager
                .borrow()
                .get(h)
                .map(|(_, s)| s.responded)
                .unwrap_or(false);

            if responded {
                miss_counts.remove(&h);
                let became_connected = set_connected(server, h, true);
                if became_connected {
                    log::info!("client {h} reachable → creating edge barrier");
                    server.ensure_barrier(&capture_notify, h);
                }
            } else {
                let misses = miss_counts.entry(h).or_insert(0);
                *misses += 1;
                if *misses >= KEEPALIVE_MAX_MISSES {
                    let became_disconnected = set_connected(server, h, false);
                    if became_disconnected {
                        log::info!(
                            "client {h} unreachable ({KEEPALIVE_MAX_MISSES} keepalive misses) → removing edge barrier"
                        );
                        server.ensure_barrier(&capture_notify, h);
                    }
                    miss_counts.remove(&h);
                }
            }
        }
    }
}

/// Set `connected` for `handle`, returning `true` only when the value actually
/// changed (so the caller reconciles the barrier exactly on transitions).
fn set_connected(server: &Server, handle: ClientHandle, connected: bool) -> bool {
    let mut client_manager = server.client_manager.borrow_mut();
    match client_manager.get_mut(handle) {
        Some((_, s)) if s.connected != connected => {
            s.connected = connected;
            true
        }
        _ => false,
    }
}
