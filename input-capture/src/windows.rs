use async_trait::async_trait;
use core::task::{Context, Poll};
use futures::Stream;
use once_cell::unsync::Lazy;

use std::collections::HashSet;
use std::ptr::{addr_of, addr_of_mut};

use futures::executor::block_on;
use std::default::Default;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Mutex};
use std::task::ready;
use std::{pin::Pin, thread};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, FALSE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BeginPath, BitBlt, CloseFigure, CreateCompatibleBitmap, CreateCompatibleDC,
    CreatePen, CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, EndPath, EnumDisplayDevicesW,
    EnumDisplaySettingsW, FillRect, InvalidateRect, LineTo, MoveToEx, Polygon, SelectClipPath,
    SelectObject, UpdateWindow, DEVMODEW, DISPLAY_DEVICEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP,
    ENUM_CURRENT_SETTINGS, HBRUSH, HGDIOBJ, HPEN, PAINTSTRUCT, PS_SOLID, RGN_COPY, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    VIRTUAL_KEY, VK_CAPITAL, VK_ESCAPE, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RCONTROL,
    VK_RMENU, VK_RSHIFT, VK_RWIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect,
    GetCursorPos, GetMessageW, KillTimer, LoadCursorW, PostThreadMessageW, RegisterClassW,
    SetCursorPos, SetLayeredWindowAttributes, SetTimer, SetWindowPos, SetWindowsHookExW, ShowCursor,
    ShowWindow, TranslateMessage, EDD_GET_DEVICE_INTERFACE_NAME, HHOOK, HMENU, HOOKPROC,
    HWND_TOPMOST, IDC_ARROW, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LWA_ALPHA, LWA_COLORKEY, MSG,
    MSLLHOOKSTRUCT, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WINDOW_STYLE, WM_DISPLAYCHANGE,
    WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WM_TIMER, WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW, WNDPROC,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use input_event::{
    scancode::{self, Linux},
    Event, KeyboardEvent, PointerEvent, BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT,
};

use super::{Capture, CaptureError, CaptureEvent, Position};

enum Request {
    Create(Position),
    Destroy(Position),
}

pub struct WindowsInputCapture {
    event_rx: Receiver<(Position, CaptureEvent)>,
    msg_thread: Option<std::thread::JoinHandle<()>>,
}

enum EventType {
    Request = 0,
    Release = 1,
    Exit = 2,
}

unsafe fn signal_message_thread(event_type: EventType) {
    if let Some(event_tid) = get_event_tid() {
        PostThreadMessageW(event_tid, WM_USER, WPARAM(event_type as usize), LPARAM(0)).unwrap();
    } else {
        panic!();
    }
}

#[async_trait]
impl Capture for WindowsInputCapture {
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        unsafe {
            {
                let mut requests = REQUEST_BUFFER.lock().unwrap();
                requests.push(Request::Create(pos));
            }
            signal_message_thread(EventType::Request);
        }
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        unsafe {
            {
                let mut requests = REQUEST_BUFFER.lock().unwrap();
                requests.push(Request::Destroy(pos));
            }
            signal_message_thread(EventType::Request);
        }
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        unsafe { signal_message_thread(EventType::Release) };
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

static mut REQUEST_BUFFER: Mutex<Vec<Request>> = Mutex::new(Vec::new());
static mut ACTIVE_CLIENT: Option<Position> = None;
static mut CLIENTS: Lazy<HashSet<Position>> = Lazy::new(HashSet::new);
static mut EVENT_TX: Option<Sender<(Position, CaptureEvent)>> = None;
static mut EVENT_THREAD_ID: AtomicU32 = AtomicU32::new(0);
unsafe fn set_event_tid(tid: u32) {
    EVENT_THREAD_ID.store(tid, Ordering::SeqCst);
}
unsafe fn get_event_tid() -> Option<u32> {
    match EVENT_THREAD_ID.load(Ordering::SeqCst) {
        0 => None,
        id => Some(id),
    }
}

static mut ENTRY_POINT: (i32, i32) = (0, 0);
// 系统光标每帧 motion 后会被 SetCursorPos 推回这个锚点（激活时 = 入口屏幕中心），
// 所以 to_mouse_event 看到的 pt - MOTION_ANCHOR 就是本帧 raw delta，不会因为
// Windows 把系统光标 clamp 在屏幕边界而卡住 — 这正是入口贴近主屏边缘时
// "远端某方向无法移动 / 自动反向跳" 的根因。
static mut MOTION_ANCHOR: (i32, i32) = (0, 0);
// 自注入 SetCursorPos 触发的 mouse hook fire 计数：mouse_proc 看到 >0 就 decrement 并 swallow,
// 不让 reset 自身的 hook fire 被当成用户输入而产生反向 motion。
static INJECTED_MOTION: AtomicU32 = AtomicU32::new(0);

unsafe fn warp_cursor_to(point: (i32, i32)) {
    INJECTED_MOTION.fetch_add(1, Ordering::SeqCst);
    let _ = SetCursorPos(point.0, point.1);
}

unsafe fn anchor_for_entry(entry: (i32, i32), displays: &[RECT]) -> (i32, i32) {
    if let Some(d) = displays.iter().find(|d| is_within_dp_region(entry, d)) {
        ((d.left + d.right) / 2, (d.top + d.bottom) / 2)
    } else {
        entry
    }
}

// === lan-mouse dwell-time patch (sticky corners) ===
// 鼠标必须在屏幕边界停留 N 毫秒才会越界，避免误触。
// 通过环境变量 LAN_MOUSE_DWELL_MS 配置（默认 0 = 关闭，恢复原行为）。
// 实现机制：第一次撞边界时设置 PENDING_BARRIER + Win32 SetTimer，鼠标
// 离开边界时取消 timer；timer fire 时（即使鼠标静止）window_proc 用
// GetCursorPos 复查光标仍贴边即激活。
const DWELL_TIMER_ID: usize = 0xD05E;
// 三元组：(barrier 方向, 入口点 clamp 到 display 内, dwell 起始时间)
// 起始时间用于 mouse-motion 路径激活（绕开 WM_TIMER 在 message queue 里被
// mouse 输入消息饿死的问题）。
static mut PENDING_BARRIER: Option<(Position, (i32, i32), std::time::Instant)> = None;
static mut MSG_HWND: HWND = HWND(std::ptr::null_mut());
// 最近一次从远端释放回本机的 (方向, 时刻)；release-cooldown 用它判断是否在冷却期。
// 与 ACTIVE_CLIENT 等一样只在 message_thread（LL hook 安装线程）访问，无需额外同步。
static mut LAST_RELEASE: Option<(Position, std::time::Instant)> = None;

fn dwell_ms() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CACHED: AtomicU64 = AtomicU64::new(u64::MAX);
    let v = CACHED.load(Ordering::Relaxed);
    if v != u64::MAX {
        return v;
    }
    let parsed = std::env::var("LAN_MOUSE_DWELL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    CACHED.store(parsed, Ordering::Relaxed);
    parsed
}

// === lan-mouse crossing-margin patch (借鉴 mykvm CROSSING_MARGIN) ===
// 进入远端前要求光标越过【源屏边缘】>= N px（"穿越确认余量"），过滤"贴边/手抖
// 擦边 1px 即误触发"。通过 LAN_MOUSE_ENTER_MARGIN 配置（默认 0 = 关闭）。
// 只作用于 entered_barrier 立即穿越路径；dwell 的 at_barrier 兜底不受影响。
// 注：一次真实越界 overshoot 恒 >= 1，所以 margin=0 时判定 >=0 永真 = 行为与今天一致。
fn enter_margin() -> i32 {
    use std::sync::atomic::{AtomicI32, Ordering};
    static CACHED: AtomicI32 = AtomicI32::new(i32::MIN);
    let v = CACHED.load(Ordering::Relaxed);
    if v != i32::MIN {
        return v;
    }
    let parsed = std::env::var("LAN_MOUSE_ENTER_MARGIN")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0)
        .max(0);
    CACHED.store(parsed, Ordering::Relaxed);
    parsed
}

// === lan-mouse release-cooldown patch (借鉴 mykvm hysteresis) ===
// 从远端切回本机后，N ms 内不再自动激活"刚释放的那一侧"，防止释放瞬间贴边
// 立刻又被吸回去来回横跳。通过 LAN_MOUSE_REARM_MS 配置（默认 0 = 关闭，永不阻塞）。
// 注：因 MOTION_ANCHOR 每帧把光标 warp 回入口屏中心，释放后光标通常落在屏幕中央，
// 该冷却在当前 Windows 实现里很少触发，属防御性补强（如 ping 超时/远端发起的释放）。
fn rearm_ms() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CACHED: AtomicU64 = AtomicU64::new(u64::MAX);
    let v = CACHED.load(Ordering::Relaxed);
    if v != u64::MAX {
        return v;
    }
    let parsed = std::env::var("LAN_MOUSE_REARM_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    CACHED.store(parsed, Ordering::Relaxed);
    parsed
}

/// returns the barrier (display edge) the point is touching, or None if interior
fn at_barrier(point: (i32, i32), displays: &[RECT]) -> Option<Position> {
    let display = displays.iter().find(|&d| is_within_dp_region(point, d))?;
    let (x, y) = point;
    if x <= display.left { return Some(Position::Left); }
    if x >= display.right - 1 { return Some(Position::Right); }
    if y <= display.top { return Some(Position::Top); }
    if y >= display.bottom - 1 { return Some(Position::Bottom); }
    None
}

// === Win11-style sticky-edge visual indicator ===
// 半透明小条贴在屏幕边缘，dwell 期间填充进度。
const INDICATOR_TIMER_ID: usize = 0xD06E;
const INDICATOR_REFRESH_MS: u32 = 16; // ~60fps
static mut INDICATOR_HWND: HWND = HWND(std::ptr::null_mut());

// 当前显示的箭头方向（paint 根据 pos 画对应 V 形）
static mut INDICATOR_POS: Position = Position::Left;
// 隐藏 Windows 系统光标 — 配对 ShowCursor 才能恢复（计数器）
static mut CURSOR_HIDDEN: bool = false;

unsafe fn hide_cursor() {
    if !CURSOR_HIDDEN {
        let _ = ShowCursor(false);
        CURSOR_HIDDEN = true;
    }
}

unsafe fn restore_cursor() {
    if CURSOR_HIDDEN {
        let _ = ShowCursor(true);
        CURSOR_HIDDEN = false;
    }
}

unsafe fn show_indicator(pos: Position, cursor: (i32, i32), displays: &[RECT]) {
    if INDICATOR_HWND.0.is_null() {
        return;
    }
    let display = match displays.iter().find(|&d| is_within_dp_region(cursor, d)) {
        Some(d) => *d,
        None => return,
    };
    // V 形箭头尺寸：水平方向（left/right）40×120，垂直方向（top/bottom）120×40
    let (x, y, w, h) = match pos {
        Position::Left => (display.left, cursor.1 - 60, 40, 120),
        Position::Right => (display.right - 40, cursor.1 - 60, 40, 120),
        Position::Top => (cursor.0 - 60, display.top, 120, 40),
        Position::Bottom => (cursor.0 - 60, display.bottom - 40, 120, 40),
    };
    INDICATOR_POS = pos;
    log::info!("show_indicator: pos={pos:?} rect=({x},{y},{w}x{h})");
    let _ = SetWindowPos(
        INDICATOR_HWND,
        HWND_TOPMOST,
        x,
        y,
        w,
        h,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    let _ = SetTimer(INDICATOR_HWND, INDICATOR_TIMER_ID, INDICATOR_REFRESH_MS, None);
    let _ = InvalidateRect(INDICATOR_HWND, None, FALSE);
    let _ = UpdateWindow(INDICATOR_HWND);
    // 隐藏 Windows 系统光标 — WC3 边缘滚屏的视觉感（demo 模式不影响光标，避免长期隐藏）
    if !demo_mode() {
        hide_cursor();
    }
}

fn demo_mode() -> bool {
    use std::sync::atomic::{AtomicI8, Ordering};
    static CACHED: AtomicI8 = AtomicI8::new(-1);
    let v = CACHED.load(Ordering::Relaxed);
    if v != -1 {
        return v == 1;
    }
    let parsed = std::env::var("LAN_MOUSE_INDICATOR_DEMO")
        .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    CACHED.store(if parsed { 1 } else { 0 }, Ordering::Relaxed);
    parsed
}

fn debug_motion() -> bool {
    use std::sync::atomic::{AtomicI8, Ordering};
    static CACHED: AtomicI8 = AtomicI8::new(-1);
    let v = CACHED.load(Ordering::Relaxed);
    if v != -1 {
        return v == 1;
    }
    let parsed = std::env::var("LAN_MOUSE_DEBUG_MOTION")
        .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    CACHED.store(if parsed { 1 } else { 0 }, Ordering::Relaxed);
    parsed
}

unsafe fn hide_indicator() {
    if INDICATOR_HWND.0.is_null() {
        return;
    }
    // 注：cursor 不在这里恢复 — 激活路径希望保持隐藏直到 Release，
    // 取消路径由 cancel_pending() 单独调 restore_cursor()。
    if demo_mode() {
        return; // demo 模式 indicator window 永久显示
    }
    let _ = KillTimer(INDICATOR_HWND, INDICATOR_TIMER_ID);
    let _ = ShowWindow(INDICATOR_HWND, SW_HIDE);
}

unsafe extern "system" fn indicator_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        x if x == WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let screen_dc = BeginPaint(hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            let _ = GetClientRect(hwnd, &mut rc);
            let w = rc.right - rc.left;
            let h = rc.bottom - rc.top;
            // double buffer：在内存 DC 完整画好再 BitBlt 到屏幕，消除闪烁
            let hdc = CreateCompatibleDC(screen_dc);
            let mem_bmp = CreateCompatibleBitmap(screen_dc, w, h);
            let old_mem_bmp = SelectObject(hdc, HGDIOBJ(mem_bmp.0));
            // erase 内存 DC 到 colorkey magenta（V 外像素将被 layered 透明化）
            let key_brush = CreateSolidBrush(COLORREF(0x00FF00FF));
            let _ = FillRect(hdc, &rc, key_brush);
            let _ = DeleteObject(HGDIOBJ(key_brush.0));

            // 进度 0.0..1.0
            let progress = if demo_mode() {
                static DEMO_START: std::sync::OnceLock<std::time::Instant> =
                    std::sync::OnceLock::new();
                let start = *DEMO_START.get_or_init(std::time::Instant::now);
                (start.elapsed().as_millis() as f64 / 1500.0) % 1.0
            } else if let Some((_, _, started)) = PENDING_BARRIER {
                let total = dwell_ms().max(1) as f64;
                (started.elapsed().as_millis() as f64 / total).clamp(0.0, 1.0)
            } else {
                0.0
            };

            let pos = INDICATOR_POS;

            // V 形箭头三个 vertex（尖端指向 pos 方向）
            let inset = 4i32;
            let pts = match pos {
                Position::Left => [
                    POINT { x: w - inset, y: inset },
                    POINT { x: inset, y: h / 2 },
                    POINT { x: w - inset, y: h - inset },
                ],
                Position::Right => [
                    POINT { x: inset, y: inset },
                    POINT { x: w - inset, y: h / 2 },
                    POINT { x: inset, y: h - inset },
                ],
                Position::Top => [
                    POINT { x: inset, y: h - inset },
                    POINT { x: w / 2, y: inset },
                    POINT { x: w - inset, y: h - inset },
                ],
                Position::Bottom => [
                    POINT { x: inset, y: inset },
                    POINT { x: w / 2, y: h - inset },
                    POINT { x: w - inset, y: inset },
                ],
            };

            // 颜色: 浅蓝轮廓 + 亮蓝进度（蓝色 = "前进/方向"感）
            let bg_color = COLORREF(0x00FFAA78); // BGR = 浅蓝 RGB(120,170,255)
            let fg_color = COLORREF(0x00FF6E28); // BGR = 亮蓝 RGB(40,110,255)

            // 1) 画整 V 形浅蓝（Polygon 自动填充闭合多边形）
            let bg_pen: HPEN = CreatePen(PS_SOLID, 2, bg_color);
            let bg_brush: HBRUSH = CreateSolidBrush(bg_color);
            let old_pen = SelectObject(hdc, HGDIOBJ(bg_pen.0));
            let old_brush = SelectObject(hdc, HGDIOBJ(bg_brush.0));
            let _ = Polygon(hdc, &pts);

            // 2) 设 clip path 到 V 形
            let _ = BeginPath(hdc);
            let _ = MoveToEx(hdc, pts[0].x, pts[0].y, None);
            let _ = LineTo(hdc, pts[1].x, pts[1].y);
            let _ = LineTo(hdc, pts[2].x, pts[2].y);
            let _ = CloseFigure(hdc);
            let _ = EndPath(hdc);
            let _ = SelectClipPath(hdc, RGN_COPY);

            // 3) clip 内画亮蓝进度矩形（从尖端方向 fill）
            let progress_rect = match pos {
                Position::Left => RECT {
                    left: 0, top: 0,
                    right: (w as f64 * progress) as i32, bottom: h,
                },
                Position::Right => RECT {
                    left: w - (w as f64 * progress) as i32, top: 0,
                    right: w, bottom: h,
                },
                Position::Top => RECT {
                    left: 0, top: 0,
                    right: w, bottom: (h as f64 * progress) as i32,
                },
                Position::Bottom => RECT {
                    left: 0, top: h - (h as f64 * progress) as i32,
                    right: w, bottom: h,
                },
            };
            let fg_brush: HBRUSH = CreateSolidBrush(fg_color);
            let _ = FillRect(hdc, &progress_rect, fg_brush);

            // 4) 清 clip — 设全 client 矩形为新 clip path
            let _ = BeginPath(hdc);
            let _ = MoveToEx(hdc, 0, 0, None);
            let _ = LineTo(hdc, w, 0);
            let _ = LineTo(hdc, w, h);
            let _ = LineTo(hdc, 0, h);
            let _ = CloseFigure(hdc);
            let _ = EndPath(hdc);
            let _ = SelectClipPath(hdc, RGN_COPY);

            SelectObject(hdc, old_pen);
            SelectObject(hdc, old_brush);
            let _ = DeleteObject(HGDIOBJ(bg_pen.0));
            let _ = DeleteObject(HGDIOBJ(bg_brush.0));
            let _ = DeleteObject(HGDIOBJ(fg_brush.0));
            // double buffer：原子地把内存 DC 拷到屏幕
            let _ = BitBlt(screen_dc, 0, 0, w, h, hdc, 0, 0, SRCCOPY);
            SelectObject(hdc, old_mem_bmp);
            let _ = DeleteObject(HGDIOBJ(mem_bmp.0));
            let _ = DeleteDC(hdc);
            let _ = EndPaint(hwnd, &ps);
            return LRESULT(0);
        }
        x if x == WM_TIMER => {
            if wparam.0 == INDICATOR_TIMER_ID {
                let _ = InvalidateRect(hwnd, None, FALSE);
                return LRESULT(0);
            }
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn cancel_pending() {
    if PENDING_BARRIER.take().is_some() {
        let _ = KillTimer(MSG_HWND, DWELL_TIMER_ID);
        hide_indicator();
        restore_cursor(); // 取消 dwell 时恢复光标（用户拉回鼠标）
    }
}

/// called from window_proc when WM_TIMER fires; cursor may be anywhere (incl. static)
unsafe fn try_activate_pending() {
    let Some((pos, entry, _)) = PENDING_BARRIER.take() else { return };
    let mut cursor = POINT { x: 0, y: 0 };
    if GetCursorPos(&mut cursor).is_err() {
        return;
    }
    if at_barrier((cursor.x, cursor.y), get_display_regions()) != Some(pos) {
        log::debug!("dwell timer fired but cursor no longer at {pos:?}");
        return;
    }
    if !CLIENTS.contains(&pos) {
        return;
    }
    if ACTIVE_CLIENT.is_some() {
        return;
    }
    ACTIVE_CLIENT.replace(pos);
    ENTRY_POINT = entry;
    MOTION_ANCHOR = anchor_for_entry(entry, get_display_regions());
    hide_indicator();
    log::info!(
        "BEGIN (dwell timer): pos={pos:?} entry=({},{}) anchor=({},{})",
        entry.0, entry.1, MOTION_ANCHOR.0, MOTION_ANCHOR.1
    );
    send_blocking(CaptureEvent::Begin);
    warp_cursor_to(MOTION_ANCHOR);
}

fn to_mouse_event(wparam: WPARAM, lparam: LPARAM) -> Option<PointerEvent> {
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    match wparam {
        WPARAM(p) if p == WM_LBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 1,
        }),
        WPARAM(p) if p == WM_MBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 1,
        }),
        WPARAM(p) if p == WM_RBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 1,
        }),
        WPARAM(p) if p == WM_LBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 0,
        }),
        WPARAM(p) if p == WM_RBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MOUSEMOVE as usize => unsafe {
            let (x, y) = (mouse_low_level.pt.x, mouse_low_level.pt.y);
            // dx/dy 是"本帧 raw delta"：MOTION_ANCHOR 由 mouse_proc 末尾每帧 SetCursorPos
            // 强行维护，所以 pt - MOTION_ANCHOR 就是本帧用户实际移动量，不受系统光标
            // 屏幕边界 clamp 影响。
            let (ex, ey) = MOTION_ANCHOR;
            let (dx, dy) = (x - ex, y - ey);

            // LAN_MOUSE_DEBUG_MOTION=1 时每个 motion 一行 debug，用于排查"远端瞬移 / 卡死"。
            if debug_motion() {
                let mut sys = POINT { x: 0, y: 0 };
                let _ = GetCursorPos(&mut sys);
                log::debug!(
                    "motion pt=({x},{y}) anchor=({ex},{ey}) delta=({dx},{dy}) sys_cursor=({},{})",
                    sys.x,
                    sys.y
                );
            }

            let (dx, dy) = (dx as f64, dy as f64);
            Some(PointerEvent::Motion { time: 0, dx, dy })
        },
        WPARAM(p) if p == WM_MOUSEWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 0,
            value: -(mouse_low_level.mouseData as i32 >> 16),
        }),
        WPARAM(p) if p == WM_XBUTTONDOWN as usize || p == WM_XBUTTONUP as usize => {
            let hb = mouse_low_level.mouseData >> 16;
            let button = match hb {
                1 => BTN_BACK,
                2 => BTN_FORWARD,
                _ => {
                    log::warn!("unknown mouse button");
                    return None;
                }
            };
            Some(PointerEvent::Button {
                time: 0,
                button,
                state: if p == WM_XBUTTONDOWN as usize { 1 } else { 0 },
            })
        }
        w => {
            log::warn!("unknown mouse event: {w:?}");
            None
        }
    }
}

/// VK 兜底：某些键盘驱动 / Win Mobile Hotspot / 远程会话场景下 KBDLLHOOKSTRUCT.scanCode
/// 给的是 0（实测 RightAlt 在 Kylin uinput 链路里 raw scanCode=0），scancode 翻译失败
/// release_bind 永远不命中。modifier 键的 vkCode 是稳定的，按 VK 直接映射 Linux scancode。
fn vk_to_linux(vk: u16) -> Option<Linux> {
    let v = VIRTUAL_KEY(vk);
    Some(if v == VK_LMENU { Linux::KeyLeftAlt }
    else if v == VK_RMENU { Linux::KeyRightalt }
    else if v == VK_LCONTROL { Linux::KeyLeftCtrl }
    else if v == VK_RCONTROL { Linux::KeyRightCtrl }
    else if v == VK_LSHIFT { Linux::KeyLeftShift }
    else if v == VK_RSHIFT { Linux::KeyRightShift }
    else if v == VK_LWIN { Linux::KeyLeftMeta }
    else if v == VK_RWIN { Linux::KeyRightmeta }
    else if v == VK_CAPITAL { Linux::KeyCapsLock }
    else if v == VK_ESCAPE { Linux::KeyEsc }
    else { return None })
}

unsafe fn to_key_event(wparam: WPARAM, lparam: LPARAM) -> Option<KeyboardEvent> {
    let kybrdllhookstruct: KBDLLHOOKSTRUCT = *(lparam.0 as *const KBDLLHOOKSTRUCT);
    // 先定 state；SYSKEYUP 原来误写成 state:1（Alt 键属 SYSKEY 路径，导致放手算成按下）
    let state: u8 = if wparam.0 == WM_KEYDOWN as usize || wparam.0 == WM_SYSKEYDOWN as usize {
        1
    } else if wparam.0 == WM_KEYUP as usize || wparam.0 == WM_SYSKEYUP as usize {
        0
    } else {
        return None;
    };

    let mut scan_code = kybrdllhookstruct.scanCode;
    log::trace!("scan_code: {scan_code} vk: {:#x} flags: {:?}", kybrdllhookstruct.vkCode, kybrdllhookstruct.flags);
    if kybrdllhookstruct.flags.contains(LLKHF_EXTENDED) {
        scan_code |= 0xE000;
    }

    // 主路径：raw scancode → Windows enum → Linux scancode
    let linux_scan_code = match scancode::Windows::try_from(scan_code) {
        Ok(win_sc) => {
            log::trace!("windows_scan: {win_sc:?}");
            match Linux::try_from(win_sc) {
                Ok(l) => Some(l),
                Err(_) => {
                    log::warn!("no Linux mapping for windows scancode {win_sc:?}");
                    None
                }
            }
        }
        Err(_) => None,
    };

    // 兜底：scancode 翻译失败 → 用 vkCode 找 modifier。命中也算合法事件，不再丢弃。
    let linux_scan_code = linux_scan_code.or_else(|| {
        let vk = kybrdllhookstruct.vkCode as u16;
        let fallback = vk_to_linux(vk);
        if let Some(l) = fallback {
            log::debug!("scancode {scan_code} 未映射，vkCode {vk:#x} 兜底 → {l:?}");
        } else {
            log::warn!("failed to translate to windows scancode: {scan_code} (vk={vk:#x})");
        }
        fallback
    })?;

    log::trace!("linux_scan: {linux_scan_code:?} state={state}");
    Some(KeyboardEvent::Key { time: 0, key: linux_scan_code as u32, state })
}

///
/// clamp point to display bounds
///
/// # Arguments
///
/// * `prev_point`: coordinates, the cursor was before entering, within bounds of a display
/// * `entry_point`: point to clamp
///
/// returns: (i32, i32), the corrected entry point
///
fn clamp_to_display_bounds(prev_point: (i32, i32), point: (i32, i32)) -> (i32, i32) {
    /* find display where movement came from */
    let display_regions = unsafe { get_display_regions() };
    let display = display_regions
        .iter()
        .find(|&d| is_within_dp_region(prev_point, d))
        .unwrap();

    /* clamp to bounds (inclusive) */
    let (x, y) = point;
    let (min_x, max_x) = (display.left, display.right - 1);
    let (min_y, max_y) = (display.top, display.bottom - 1);
    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}

unsafe fn send_blocking(event: CaptureEvent) {
    if let Some(active) = ACTIVE_CLIENT {
        block_on(async move {
            let _ = EVENT_TX.as_ref().unwrap().send((active, event)).await;
        });
    }
}

unsafe fn check_client_activation(wparam: WPARAM, lparam: LPARAM) -> bool {
    if wparam.0 != WM_MOUSEMOVE as usize {
        return ACTIVE_CLIENT.is_some();
    }
    let mouse_low_level: MSLLHOOKSTRUCT = *(lparam.0 as *const MSLLHOOKSTRUCT);
    static mut PREV_POS: Option<(i32, i32)> = None;
    let curr_pos = (mouse_low_level.pt.x, mouse_low_level.pt.y);
    let prev_pos = PREV_POS.unwrap_or(curr_pos);
    PREV_POS.replace(curr_pos);

    /* next event is the first actual event */
    let ret = ACTIVE_CLIENT.is_some();

    /* client already active, no need to check */
    if ACTIVE_CLIENT.is_some() {
        return ret;
    }

    let displays = get_display_regions();
    let dwell = dwell_ms();

    // sticky-corners pending state: 鼠标已撞到 barrier 但 dwell 还没到
    if let Some((pending_pos, entry, started)) = PENDING_BARRIER {
        let still_at = at_barrier(curr_pos, displays) == Some(pending_pos);
        if !still_at {
            log::debug!("dwell cancelled (cursor left {pending_pos:?})");
            cancel_pending();
            // 继续走下面的 entered_barrier 逻辑（鼠标可能撞到别的 barrier）
        } else {
            // 仍贴边 — 鼠标 motion 路径自己检查 dwell 是否已过
            // (因为 WM_TIMER 在 message queue 里优先级低于 mouse 输入，持续移动时 timer 被饿死)
            if started.elapsed() >= std::time::Duration::from_millis(dwell) {
                let _ = KillTimer(MSG_HWND, DWELL_TIMER_ID);
                PENDING_BARRIER = None;
                if CLIENTS.contains(&pending_pos) {
                    ACTIVE_CLIENT.replace(pending_pos);
                    ENTRY_POINT = entry;
                    MOTION_ANCHOR = anchor_for_entry(entry, displays);
                    hide_indicator();
                    log::info!(
                        "BEGIN (dwell motion): pos={pending_pos:?} entry=({},{}) anchor=({},{})",
                        entry.0, entry.1, MOTION_ANCHOR.0, MOTION_ANCHOR.1
                    );
                    send_blocking(CaptureEvent::Begin);
                    warp_cursor_to(MOTION_ANCHOR);
                }
                // 激活那一帧不转发 motion（curr_pos 在边角，pt - anchor 是巨大 offset）；
                // 下一次用户输入时 cursor 已被 warp 到 anchor，delta 才是真实 raw delta。
                return false;
            }
            // dwell 还没到，继续等
            return false;
        }
    }

    /* check if mouse crossed/touches a barrier
     * dwell == 0：保持原行为 entered_barrier 严格越界检测
     * dwell > 0：兜底 at_barrier(curr_pos) — 鼠标贴边即可触发，避免
     *           LowLevelMouseProc 在某些 Win 版本拿到 post-clamp 坐标导致
     *           "越界但 entered_barrier 不 fire" 的问题
     */
    // 穿越确认余量：只对 entered_barrier 立即穿越生效，要求越过源屏边缘 >= margin px。
    // margin=0（默认）时 overshoot>=1 恒满足 → 等价原行为；at_barrier 兜底不过滤。
    let margin = enter_margin();
    let crossed = entered_barrier(prev_pos, curr_pos, displays)
        .filter(|&p| crossing_overshoot(prev_pos, curr_pos, displays, p) >= margin);
    let pos = if dwell == 0 {
        crossed
    } else {
        crossed.or_else(|| at_barrier(curr_pos, displays))
    };
    let Some(pos) = pos else {
        return ret;
    };
    if !CLIENTS.contains(&pos) {
        return ret;
    }

    // 释放冷却（re-arm）：刚切回本机后 rearm ms 内不再激活"同一侧"，防止释放瞬间
    // 贴边立刻又被吸回去。rearm=0（默认）时整段跳过，永不阻塞。
    let rearm = rearm_ms();
    if rearm > 0 {
        if let Some((rpos, when)) = LAST_RELEASE {
            if rpos == pos && when.elapsed() < std::time::Duration::from_millis(rearm) {
                log::debug!("re-arm cooldown active for {pos:?}");
                return ret;
            }
        }
    }

    // 防御：at_barrier 兜底时 prev_pos 可能在屏幕外（pre-clamp 负坐标），
    // clamp_to_display_bounds 用 prev 找 display，找不到会 unwrap panic。
    let safe_prev = if displays.iter().any(|d| is_within_dp_region(prev_pos, d)) {
        prev_pos
    } else {
        curr_pos
    };
    let entry = clamp_to_display_bounds(safe_prev, curr_pos);

    if dwell == 0 {
        // 原行为：立即激活
        ACTIVE_CLIENT.replace(pos);
        ENTRY_POINT = entry;
        MOTION_ANCHOR = anchor_for_entry(entry, displays);
        log::info!(
            "BEGIN (immediate): pos={pos:?} prev={prev_pos:?} curr={curr_pos:?} entry=({},{}) anchor=({},{})",
            entry.0, entry.1, MOTION_ANCHOR.0, MOTION_ANCHOR.1
        );
        send_blocking(CaptureEvent::Begin);
        warp_cursor_to(MOTION_ANCHOR);
        return ret;
    }

    // dwell > 0: 设置 pending + 启动 SetTimer (兜底鼠标静止场景) + 显示 indicator
    PENDING_BARRIER = Some((pos, entry, std::time::Instant::now()));
    let _ = SetTimer(MSG_HWND, DWELL_TIMER_ID, dwell as u32, None);
    show_indicator(pos, curr_pos, displays);
    log::debug!("dwell pending @ {pos:?} for {dwell}ms");
    false
}

unsafe extern "system" fn mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // 自注入的 SetCursorPos 触发的 hook fire —— sentinel 配对吃掉，不当用户输入。
    if wparam.0 == WM_MOUSEMOVE as usize
        && INJECTED_MOTION.load(Ordering::SeqCst) > 0
    {
        INJECTED_MOTION.fetch_sub(1, Ordering::SeqCst);
        // 激活期间继续 swallow（不传给应用、不进 to_mouse_event）；
        // 未激活时让事件正常传给应用（理论上不会到这条路径，但保险起见）。
        if ACTIVE_CLIENT.is_some() {
            return LRESULT(1);
        }
        return CallNextHookEx(HHOOK::default(), ncode, wparam, lparam);
    }

    let active = check_client_activation(wparam, lparam);

    /* no client was active */
    if !active {
        return CallNextHookEx(HHOOK::default(), ncode, wparam, lparam);
    }

    /* get active client if any */
    let Some(pos) = ACTIVE_CLIENT else {
        return LRESULT(1);
    };

    /* convert to lan-mouse event */
    let Some(pointer_event) = to_mouse_event(wparam, lparam) else {
        return LRESULT(1);
    };
    let event = (pos, CaptureEvent::Input(Event::Pointer(pointer_event)));

    /* notify mainthread (drop events if sending too fast) */
    if let Err(e) = EVENT_TX.as_ref().unwrap().try_send(event) {
        log::warn!("dropped capture event (channel full / closed): {e}");
    }

    // motion 后把系统光标 warp 回 MOTION_ANCHOR：下一帧 hook 拿到的 pt - anchor 就是
    // 本帧 raw delta，避免 cursor 撞屏幕边后 pt 被 clamp 卡死。
    // sentinel 计数器保证这次 warp 触发的递归 hook fire 不被当用户输入。
    if wparam.0 == WM_MOUSEMOVE as usize {
        let mouse_low_level: MSLLHOOKSTRUCT = *(lparam.0 as *const MSLLHOOKSTRUCT);
        let pt = (mouse_low_level.pt.x, mouse_low_level.pt.y);
        if pt != MOTION_ANCHOR {
            warp_cursor_to(MOTION_ANCHOR);
        }
    }

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn kybrd_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    /* get active client if any */
    let Some(client) = ACTIVE_CLIENT else {
        return CallNextHookEx(HHOOK::default(), ncode, wparam, lparam);
    };

    /* convert to key event */
    let Some(key_event) = to_key_event(wparam, lparam) else {
        return LRESULT(1);
    };
    let event = (client, CaptureEvent::Input(Event::Keyboard(key_event)));

    if let Err(e) = EVENT_TX.as_ref().unwrap().try_send(event) {
        log::warn!("dropped key event (channel full / closed): {e}");
    }

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    uint: u32,
    wparam: WPARAM,
    _lparam: LPARAM,
) -> LRESULT {
    match uint {
        x if x == WM_DISPLAYCHANGE => {
            log::debug!("display resolution changed");
            DISPLAY_RESOLUTION_CHANGED = true;
        }
        x if x == WM_TIMER => {
            if wparam.0 == DWELL_TIMER_ID {
                let _ = KillTimer(hwnd, DWELL_TIMER_ID);
                try_activate_pending();
            }
        }
        _ => {}
    }
    LRESULT(1)
}

fn enumerate_displays() -> Vec<RECT> {
    unsafe {
        let mut display_rects = vec![];
        let mut devices = vec![];
        for i in 0.. {
            let mut device: DISPLAY_DEVICEW = std::mem::zeroed();
            device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            let ret = EnumDisplayDevicesW(None, i, &mut device, EDD_GET_DEVICE_INTERFACE_NAME);
            if ret == FALSE {
                break;
            }
            if device.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP != 0 {
                devices.push(device.DeviceName);
            }
        }
        for device in devices {
            let mut dev_mode: DEVMODEW = std::mem::zeroed();
            dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            let ret = EnumDisplaySettingsW(
                PCWSTR::from_raw(&device as *const _),
                ENUM_CURRENT_SETTINGS,
                &mut dev_mode,
            );
            if ret == FALSE {
                log::warn!("no display mode");
            }

            let pos = dev_mode.Anonymous1.Anonymous2.dmPosition;
            let (x, y) = (pos.x, pos.y);
            let (width, height) = (dev_mode.dmPelsWidth, dev_mode.dmPelsHeight);

            display_rects.push(RECT {
                left: x,
                right: x + width as i32,
                top: y,
                bottom: y + height as i32,
            });
        }
        display_rects
    }
}

static mut DISPLAY_RESOLUTION_CHANGED: bool = true;

unsafe fn get_display_regions() -> &'static Vec<RECT> {
    static mut DISPLAYS: Vec<RECT> = vec![];
    if DISPLAY_RESOLUTION_CHANGED {
        DISPLAYS = enumerate_displays();
        DISPLAY_RESOLUTION_CHANGED = false;
        log::debug!("displays: {DISPLAYS:?}");
    }
    &*addr_of!(DISPLAYS)
}

fn is_within_dp_region(point: (i32, i32), display: &RECT) -> bool {
    [
        Position::Left,
        Position::Right,
        Position::Top,
        Position::Bottom,
    ]
    .iter()
    .all(|&pos| is_within_dp_boundary(point, display, pos))
}
fn is_within_dp_boundary(point: (i32, i32), display: &RECT, pos: Position) -> bool {
    let (x, y) = point;
    match pos {
        Position::Left => display.left <= x,
        Position::Right => display.right > x,
        Position::Top => display.top <= y,
        Position::Bottom => display.bottom > y,
    }
}

/// returns whether the given position is within the display bounds with respect to the given
/// barrier position
///
/// # Arguments
///
/// * `x`:
/// * `y`:
/// * `displays`:
/// * `pos`:
///
/// returns: bool
///
fn in_bounds(point: (i32, i32), displays: &[RECT], pos: Position) -> bool {
    displays
        .iter()
        .any(|d| is_within_dp_boundary(point, d, pos))
}

fn in_display_region(point: (i32, i32), displays: &[RECT]) -> bool {
    displays.iter().any(|d| is_within_dp_region(point, d))
}

fn moved_across_boundary(
    prev_pos: (i32, i32),
    curr_pos: (i32, i32),
    displays: &[RECT],
    pos: Position,
) -> bool {
    /* was within bounds, but is not anymore */
    in_display_region(prev_pos, displays) && !in_bounds(curr_pos, displays, pos)
}

fn entered_barrier(
    prev_pos: (i32, i32),
    curr_pos: (i32, i32),
    displays: &[RECT],
) -> Option<Position> {
    [
        Position::Left,
        Position::Right,
        Position::Top,
        Position::Bottom,
    ]
    .into_iter()
    .find(|&pos| moved_across_boundary(prev_pos, curr_pos, displays, pos))
}

/// 光标在方向 `pos` 上越过【源屏边缘】的像素数（穿越确认余量用）。
/// 源屏 = 包含 `prev` 的 display。一次真实越界（entered_barrier 命中）该值恒 >= 1。
/// `prev` 不在任何 display 内（罕见，如 at_barrier 兜底的屏外坐标）时返回 i32::MAX —
/// 表示无法测量 → 不因余量阻塞（保持可穿越）。
fn crossing_overshoot(
    prev: (i32, i32),
    curr: (i32, i32),
    displays: &[RECT],
    pos: Position,
) -> i32 {
    let Some(d) = displays.iter().find(|d| is_within_dp_region(prev, d)) else {
        return i32::MAX;
    };
    match pos {
        Position::Left => d.left - curr.0,
        Position::Right => curr.0 - (d.right - 1),
        Position::Top => d.top - curr.1,
        Position::Bottom => curr.1 - (d.bottom - 1),
    }
}

fn get_msg() -> Option<MSG> {
    unsafe {
        let mut msg = std::mem::zeroed();
        let ret = GetMessageW(addr_of_mut!(msg), HWND::default(), 0, 0);
        match ret.0 {
            0 => None,
            x if x > 0 => Some(msg),
            _ => panic!("error in GetMessageW"),
        }
    }
}

static WINDOW_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

fn message_thread(ready_tx: mpsc::Sender<()>) {
    unsafe {
        set_event_tid(GetCurrentThreadId());
        ready_tx.send(()).expect("channel closed");
        let mouse_proc: HOOKPROC = Some(mouse_proc);
        let kybrd_proc: HOOKPROC = Some(kybrd_proc);
        let window_proc: WNDPROC = Some(window_proc);

        /* register hooks */
        let _ = SetWindowsHookExW(WH_MOUSE_LL, mouse_proc, HINSTANCE::default(), 0).unwrap();
        let _ = SetWindowsHookExW(WH_KEYBOARD_LL, kybrd_proc, HINSTANCE::default(), 0).unwrap();

        let instance = GetModuleHandleW(None).unwrap();
        let window_class: WNDCLASSW = WNDCLASSW {
            lpfnWndProc: window_proc,
            hInstance: instance.into(),
            lpszClassName: w!("lan-mouse-message-window-class"),
            ..Default::default()
        };

        if WINDOW_CLASS_REGISTERED
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            /* register window class if not yet done so */
            let ret = RegisterClassW(&window_class);
            if ret == 0 {
                panic!("RegisterClassW");
            }
        }

        /* window is used to receive WM_DISPLAYCHANGE / WM_TIMER messages */
        let hwnd = CreateWindowExW(
            Default::default(),
            w!("lan-mouse-message-window-class"),
            w!("lan-mouse-msg-window"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            HWND::default(),
            HMENU::default(),
            instance,
            None,
        )
        .expect("CreateWindowExW");
        MSG_HWND = hwnd;

        /* register & create the indicator window
         * demo 模式：纯 popup + class hbrBackground 红色，肉眼必能看到（layered window paint 有时不渲染）
         * 正常模式：LAYERED + TRANSPARENT 半透明 + 不点击穿透 */
        let demo = demo_mode();
        // class 背景 brush:
        //   - demo: 纯红（不透明，便于截图调试）
        //   - 正常: magenta（与 LWA_COLORKEY 配合 V 外完全透明）
        let class_brush: HBRUSH = if demo {
            HBRUSH(CreateSolidBrush(COLORREF(0x000000FF)).0)
        } else {
            HBRUSH(CreateSolidBrush(COLORREF(0x00FF00FF)).0)
        };
        let arrow_cursor = LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default();
        let indicator_class: WNDCLASSW = WNDCLASSW {
            lpfnWndProc: Some(indicator_proc),
            hInstance: instance.into(),
            lpszClassName: w!("lan-mouse-indicator-class"),
            hbrBackground: class_brush,
            hCursor: arrow_cursor,
            ..Default::default()
        };
        let _ = RegisterClassW(&indicator_class);
        let ex_style = if demo {
            // demo 模式：保留 TRANSPARENT（鼠标点击穿透 + 避免 hCursor 未设导致的 busy 转圈）
            WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW
        } else {
            WS_EX_LAYERED
                | WS_EX_TRANSPARENT
                | WS_EX_TOPMOST
                | WS_EX_NOACTIVATE
                | WS_EX_TOOLWINDOW
        };
        let indicator_hwnd = CreateWindowExW(
            ex_style,
            w!("lan-mouse-indicator-class"),
            w!("lan-mouse-indicator"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            HWND::default(),
            HMENU::default(),
            instance,
            None,
        )
        .expect("CreateWindowExW indicator");
        if !demo {
            // colorkey magenta + alpha 200: V 外像素完全透明，V 内 ~78% 不透明
            let _ = SetLayeredWindowAttributes(
                indicator_hwnd,
                COLORREF(0x00FF00FF),
                200,
                LWA_COLORKEY | LWA_ALPHA,
            );
        }
        INDICATOR_HWND = indicator_hwnd;

        // demo 模式：在 virtual desktop 最左侧屏的中央显示 indicator
        if demo_mode() {
            let displays = get_display_regions();
            if let Some(d) = displays.iter().min_by_key(|d| d.left) {
                let cy = (d.top + d.bottom) / 2;
                let cx_logical = d.left; // 鼠标进入位置取 leftmost display 的左边
                show_indicator(Position::Left, (cx_logical, cy), displays);
                log::info!(
                    "indicator demo mode: leftmost display rect=({},{},{},{}) center=({},{})",
                    d.left, d.top, d.right, d.bottom, (d.left + d.right) / 2, cy
                );
            }
        }

        /* run message loop */
        loop {
            // mouse / keybrd proc do not actually return a message
            let Some(msg) = get_msg() else {
                break;
            };
            if msg.hwnd.0.is_null() {
                /* messages sent via PostThreadMessage */
                match msg.wParam.0 {
                    x if x == EventType::Exit as usize => break,
                    x if x == EventType::Release as usize => {
                        let was = ACTIVE_CLIENT.take();
                        log::info!("RELEASE: was_active={was:?}");
                        // 记录释放方向+时刻，供 release-cooldown 判定（防止贴边瞬间重入同一侧）
                        if let Some(p) = was {
                            LAST_RELEASE = Some((p, std::time::Instant::now()));
                        }
                        restore_cursor(); // 释放回 Windows 时恢复光标
                    }
                    x if x == EventType::Request as usize => {
                        let requests = {
                            let mut res = vec![];
                            let mut requests = REQUEST_BUFFER.lock().unwrap();
                            for request in requests.drain(..) {
                                res.push(request);
                            }
                            res
                        };

                        for request in requests {
                            update_clients(request)
                        }
                    }
                    _ => {}
                }
            } else {
                /* other messages for window_procs */
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

fn update_clients(request: Request) {
    match request {
        Request::Create(pos) => {
            unsafe { CLIENTS.insert(pos) };
        }
        Request::Destroy(pos) => unsafe {
            if let Some(active_pos) = ACTIVE_CLIENT {
                if pos == active_pos {
                    let _ = ACTIVE_CLIENT.take();
                }
            }
            CLIENTS.remove(&pos);
        },
    }
}

impl WindowsInputCapture {
    pub(crate) fn new() -> Self {
        unsafe {
            let (tx, rx) = channel(10);
            EVENT_TX.replace(tx);
            let (ready_tx, ready_rx) = mpsc::channel();
            let msg_thread = Some(thread::spawn(|| message_thread(ready_tx)));
            /* wait for thread to set its id */
            ready_rx.recv().expect("channel closed");
            Self {
                msg_thread,
                event_rx: rx,
            }
        }
    }
}

impl Stream for WindowsInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match ready!(self.event_rx.poll_recv(cx)) {
            None => Poll::Ready(None),
            Some(e) => Poll::Ready(Some(Ok(e))),
        }
    }
}

impl Drop for WindowsInputCapture {
    fn drop(&mut self) {
        unsafe { signal_message_thread(EventType::Exit) };
        let _ = self.msg_thread.take().unwrap().join();
    }
}
