#![allow(unsafe_op_in_unsafe_fn)]

use std::{
    ffi::c_void,
    mem::size_of,
    path::PathBuf,
    ptr,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use chrono::{Datelike, Duration as ChronoDuration, Timelike};
use windows_sys::Win32::{
    Foundation::{
        BOOL, COLORREF, CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM,
        LRESULT, RECT, TRUE, WPARAM,
    },
    Graphics::{
        Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
        Gdi::{
            BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DEFAULT_CHARSET,
            DEFAULT_PITCH, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, EndPaint,
            FF_DONTCARE, FillRect, FillRgn, GdiFlush, HGDIOBJ, InvalidateRect, OUT_DEFAULT_PRECIS,
            PAINTSTRUCT, RestoreDC, SaveDC, SelectClipRgn, SelectObject, SetBkMode, SetTextColor,
            TRANSPARENT,
        },
    },
    System::{
        LibraryLoader::GetModuleHandleW,
        Threading::{
            CreateMutexW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
    },
    UI::{
        Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
        HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
            SetProcessDpiAwarenessContext,
        },
        Input::KeyboardAndMouse::ReleaseCapture,
        WindowsAndMessaging::{
            CHILDID_SELF, CREATESTRUCTW, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, CreateWindowExW,
            DefWindowProcW, DispatchMessageW, EVENT_OBJECT_LOCATIONCHANGE, EnumWindows,
            GWL_EXSTYLE, GWL_STYLE, GWLP_HWNDPARENT, GWLP_USERDATA, GetClientRect, GetMessageW,
            GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, HCURSOR, HTCAPTION,
            HWND_TOP, IDC_ARROW, IsIconic, IsWindowVisible, LWA_ALPHA, LoadCursorW, MA_NOACTIVATE,
            MSG, OBJID_WINDOW, PostQuitMessage, RegisterClassW, SW_HIDE, SWP_NOACTIVATE,
            SWP_SHOWWINDOW, SendMessageW, SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW,
            SetWindowPos, ShowWindow, TranslateMessage, WINEVENT_OUTOFCONTEXT,
            WINEVENT_SKIPOWNPROCESS, WM_DESTROY, WM_ERASEBKGND, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
            WM_MOUSEACTIVATE, WM_NCCREATE, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_PAINT,
            WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_CAPTION, WS_EX_LAYERED, WS_EX_NOACTIVATE,
            WS_EX_TOOLWINDOW, WS_POPUP,
        },
    },
};

use crate::{
    locale::AppLocale,
    model::{LimitWindow, UsageSnapshot},
};

const CLASS_NAME: &str = "WeeklyUsageBar.Overlay";
const WINDOW_NAME: &str = "Weekly Usage Bar";
const MUTEX_NAME: &str = "Local\\WeeklyUsageBar.4BC6AD61";
const TRACK_TIMER: usize = 1;
const TRACK_INTERVAL_MS: u32 = 1_500;
const LOCALE_CHECK_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
struct Palette {
    accent: COLORREF,
}

impl Palette {
    const fn blue() -> Self {
        Self {
            accent: rgb(95, 145, 255),
        }
    }

    const fn green() -> Self {
        Self {
            accent: rgb(64, 196, 126),
        }
    }

    const fn purple() -> Self {
        Self {
            accent: rgb(173, 117, 255),
        }
    }
}

struct AppState {
    overlay: HWND,
    target: HWND,
    snapshot: UsageSnapshot,
    palette_index: usize,
    locale: AppLocale,
    next_locale_check: Instant,
}

unsafe impl Send for AppState {}

static STATE: OnceLock<Mutex<AppState>> = OnceLock::new();
static PALETTES: [Palette; 3] = [Palette::blue(), Palette::green(), Palette::purple()];
static CODEX_ACTIVE: AtomicBool = AtomicBool::new(false);

struct LocationWinEventHook(HWINEVENTHOOK);

impl LocationWinEventHook {
    fn install() -> Option<Self> {
        let hook = unsafe {
            SetWinEventHook(
                EVENT_OBJECT_LOCATIONCHANGE,
                EVENT_OBJECT_LOCATIONCHANGE,
                ptr::null_mut(),
                Some(win_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        (!hook.is_null()).then_some(Self(hook))
    }
}

impl Drop for LocationWinEventHook {
    fn drop(&mut self) {
        unsafe {
            UnhookWinEvent(self.0);
        }
    }
}

pub fn run() -> Result<()> {
    let Some(_instance) = InstanceMutex::acquire()? else {
        return Ok(());
    };
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    if instance.is_null() {
        bail!(
            "GetModuleHandleW failed: {}",
            std::io::Error::last_os_error()
        );
    }

    let class_name = wide(CLASS_NAME);
    let cursor = unsafe { LoadCursorW(ptr::null_mut(), IDC_ARROW) };
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
        lpfnWndProc: Some(window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: ptr::null_mut(),
        hCursor: cursor as HCURSOR,
        hbrBackground: ptr::null_mut(),
        lpszMenuName: ptr::null(),
        lpszClassName: class_name.as_ptr(),
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        bail!("RegisterClassW failed: {}", std::io::Error::last_os_error());
    }

    let window_name = wide(WINDOW_NAME);
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            class_name.as_ptr(),
            window_name.as_ptr(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null_mut(),
        )
    };
    if hwnd.is_null() {
        bail!(
            "CreateWindowExW failed: {}",
            std::io::Error::last_os_error()
        );
    }
    unsafe {
        SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA);
    }
    let saved_settings = crate::settings::Settings::load();

    STATE
        .set(Mutex::new(AppState {
            overlay: hwnd,
            target: ptr::null_mut(),
            snapshot: UsageSnapshot::default(),
            palette_index: saved_settings.palette_index % PALETTES.len(),
            locale: AppLocale::detect(),
            next_locale_check: Instant::now() + LOCALE_CHECK_INTERVAL,
        }))
        .map_err(|_| anyhow::anyhow!("application state was already initialized"))?;

    if unsafe { SetTimer(hwnd, TRACK_TIMER, TRACK_INTERVAL_MS, None) } == 0 {
        bail!("SetTimer failed: {}", std::io::Error::last_os_error());
    }
    track_codex_window();
    let _location_hook = LocationWinEventHook::install();
    crate::codex::start_worker();

    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        let result = unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) };
        if result == -1 {
            return Err(std::io::Error::last_os_error()).context("GetMessageW failed");
        }
        if result == 0 {
            break;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}

struct InstanceMutex(HANDLE);

impl InstanceMutex {
    fn acquire() -> Result<Option<Self>> {
        let name = wide(MUTEX_NAME);
        let handle = unsafe { CreateMutexW(ptr::null(), 1, name.as_ptr()) };
        if handle.is_null() {
            bail!("CreateMutexW failed: {}", std::io::Error::last_os_error());
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(handle) };
            return Ok(None);
        }
        Ok(Some(Self(handle)))
    }
}

impl Drop for InstanceMutex {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let create = lparam as *const CREATESTRUCTW;
            if !create.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*create).lpCreateParams as isize);
            }
            TRUE as LRESULT
        }
        WM_TIMER if wparam == TRACK_TIMER => {
            track_codex_window();
            0
        }
        WM_PAINT => {
            paint(hwnd);
            0
        }
        WM_ERASEBKGND => 1,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_LBUTTONDOWN => {
            let x = (lparam as u16) as i32;
            let mut client = empty_rect();
            GetClientRect(hwnd, &mut client);
            let dpi = GetDpiForWindow(hwnd).max(96);
            let settings_width = (24 * dpi as i32 / 96).max(24);
            if x >= client.right - settings_width {
                cycle_palette(hwnd);
                return 0;
            }
            if let Some(state) = STATE.get().and_then(|state| state.lock().ok())
                && !state.target.is_null()
            {
                ReleaseCapture();
                SendMessageW(state.target, WM_NCLBUTTONDOWN, HTCAPTION as usize, 0);
            }
            0
        }
        WM_LBUTTONDBLCLK => {
            if let Some(state) = STATE.get().and_then(|state| state.lock().ok())
                && !state.target.is_null()
            {
                SendMessageW(state.target, WM_NCLBUTTONDBLCLK, HTCAPTION as usize, 0);
            }
            0
        }
        WM_RBUTTONUP => {
            cycle_palette(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    let target = STATE
        .get()
        .and_then(|state| state.lock().ok())
        .map(|state| state.target)
        .unwrap_or(ptr::null_mut());

    if should_sync_location_event(event, hwnd, target, id_object, id_child) {
        sync_overlay_to_target(hwnd);
    }
}

fn should_sync_location_event(
    event: u32,
    hwnd: HWND,
    target: HWND,
    id_object: i32,
    id_child: i32,
) -> bool {
    event == EVENT_OBJECT_LOCATIONCHANGE
        && !hwnd.is_null()
        && hwnd == target
        && id_object == OBJID_WINDOW
        && id_child == CHILDID_SELF as i32
}

fn track_codex_window() {
    let Some(state_lock) = STATE.get() else {
        return;
    };
    if let Ok(mut state) = state_lock.lock() {
        let now = Instant::now();
        if now >= state.next_locale_check {
            let locale = AppLocale::detect();
            if locale != state.locale {
                state.locale = locale;
                unsafe { InvalidateRect(state.overlay, ptr::null(), 0) };
            }
            state.next_locale_check = now + LOCALE_CHECK_INTERVAL;
        }
    }

    sync_overlay_to_target(find_codex_window());
}

fn sync_overlay_to_target(target: HWND) {
    let Some(state_lock) = STATE.get() else {
        return;
    };
    let mut state = match state_lock.lock() {
        Ok(state) => state,
        Err(_) => return,
    };

    if target.is_null() || unsafe { IsIconic(target) } != 0 {
        CODEX_ACTIVE.store(false, Ordering::Relaxed);
        state.target = target;
        unsafe { ShowWindow(state.overlay, SW_HIDE) };
        return;
    }

    let mut bounds = empty_rect();
    if unsafe { GetWindowRect(target, &mut bounds) } == 0 {
        CODEX_ACTIVE.store(false, Ordering::Relaxed);
        unsafe { ShowWindow(state.overlay, SW_HIDE) };
        return;
    }

    let dpi = unsafe { GetDpiForWindow(target) }.max(96);
    let scale: f32 = dpi as f32 / 96.0_f32;
    let total_width = bounds.right - bounds.left;
    let top_margin = (5.0_f32 * scale).round() as i32;
    let height = (30.0_f32 * scale).round() as i32;
    let preferred_width = if state.snapshot.weekly.is_some() {
        440.0_f32
    } else {
        220.0_f32
    };
    let Some((relative_left, width)) = overlay_layout(total_width, scale, preferred_width) else {
        CODEX_ACTIVE.store(true, Ordering::Relaxed);
        unsafe { ShowWindow(state.overlay, SW_HIDE) };
        return;
    };
    let left = bounds.left + relative_left;

    if state.target != target {
        unsafe {
            SetWindowLongPtrW(state.overlay, GWLP_HWNDPARENT, target as isize);
        }
    }
    state.target = target;
    CODEX_ACTIVE.store(true, Ordering::Relaxed);
    unsafe {
        SetWindowPos(
            state.overlay,
            HWND_TOP,
            left,
            bounds.top + top_margin,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

fn overlay_layout(total_width: i32, scale: f32, preferred_width: f32) -> Option<(i32, i32)> {
    let left_reserve = (220.0_f32 * scale).round() as i32;
    let right_reserve = (158.0_f32 * scale).round() as i32;
    let preferred_width = (preferred_width * scale).round() as i32;
    let available_width = total_width - left_reserve - right_reserve;
    let minimum_width = (170.0_f32 * scale).round() as i32;
    if available_width < minimum_width {
        return None;
    }
    let width = preferred_width.min(available_width);
    let left = total_width - right_reserve - width;
    Some((left, width))
}

fn find_codex_window() -> HWND {
    let mut result: HWND = ptr::null_mut();
    unsafe {
        EnumWindows(
            Some(enum_windows_callback),
            (&mut result as *mut HWND) as LPARAM,
        );
    }
    result
}

unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
        return TRUE;
    }
    let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
    let extended_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
    if !is_main_window_candidate(style, extended_style) {
        return TRUE;
    }
    let mut cloaked: u32 = 0;
    if DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED as u32,
        (&mut cloaked as *mut u32).cast(),
        size_of::<u32>() as u32,
    ) == 0
        && cloaked != 0
    {
        return TRUE;
    }

    let Some(path) = process_path_for_window(hwnd) else {
        return TRUE;
    };
    let normalized = path.to_string_lossy().to_ascii_lowercase();
    if normalized.contains("\\openai.codex_") && normalized.ends_with("\\app\\chatgpt.exe") {
        *(lparam as *mut HWND) = hwnd;
        return 0;
    }
    TRUE
}

fn is_main_window_candidate(style: u32, extended_style: u32) -> bool {
    style & WS_CAPTION != 0 && extended_style & WS_EX_TOOLWINDOW == 0
}

unsafe fn paint(hwnd: HWND) {
    let mut paint: PAINTSTRUCT = std::mem::zeroed();
    let dc = BeginPaint(hwnd, &mut paint);
    if dc.is_null() {
        return;
    }
    let mut client = empty_rect();
    GetClientRect(hwnd, &mut client);
    let dpi = GetDpiForWindow(hwnd).max(96);
    let scale: f32 = dpi as f32 / 96.0_f32;

    let (snapshot, accent, locale) = STATE
        .get()
        .and_then(|state| state.lock().ok())
        .map(|state| {
            (
                state.snapshot.clone(),
                PALETTES[state.palette_index].accent,
                state.locale,
            )
        })
        .unwrap_or((
            UsageSnapshot::default(),
            PALETTES[0].accent,
            AppLocale::English,
        ));

    let background = CreateSolidBrush(rgb(31, 31, 31));
    FillRect(dc, &client, background);
    DeleteObject(background as HGDIOBJ);

    let font_height = -((12.0_f32 * scale).round() as i32);
    let face = wide("Segoe UI Variable Display");
    let font = CreateFontW(
        font_height,
        0,
        0,
        0,
        600,
        0,
        0,
        0,
        DEFAULT_CHARSET.into(),
        OUT_DEFAULT_PRECIS.into(),
        0,
        0,
        (DEFAULT_PITCH | FF_DONTCARE).into(),
        face.as_ptr(),
    );
    let previous = SelectObject(dc, font as HGDIOBJ);
    SetBkMode(dc, TRANSPARENT as i32);

    let width = client.right - client.left;
    let settings_width = (24.0_f32 * scale).round() as i32;
    let content_width = (width - settings_width).max(1);
    let content = RECT {
        left: 0,
        top: 0,
        right: content_width,
        bottom: client.bottom,
    };
    let plan = crate::planner::current_view();

    match (&snapshot.weekly, plan.as_ref()) {
        (Some(weekly), Some(plan)) => {
            draw_weekly_daily_bars(dc, content, weekly, plan, accent, scale);
        }
        (Some(weekly), None) => {
            draw_single_quota_bar(dc, content, weekly.remaining_percent as f64, accent, scale);
        }
        (None, _) => {
            if let Some(primary) = snapshot.primary.as_ref() {
                draw_single_quota_bar(dc, content, primary.remaining_percent as f64, accent, scale);
            } else {
                let status = wide(
                    snapshot
                        .status
                        .map(|status| locale.status_text(status))
                        .unwrap_or_else(|| locale.status_text(crate::model::UsageStatus::Retrying)),
                );
                let mut status_rect = content;
                SetTextColor(dc, rgb(150, 150, 150));
                DrawTextW(
                    dc,
                    status.as_ptr(),
                    -1,
                    &mut status_rect,
                    DT_CENTER | DT_SINGLELINE | DT_VCENTER,
                );
            }
        }
    }

    let mut dot = RECT {
        left: client.right - settings_width,
        top: 0,
        right: client.right,
        bottom: client.bottom,
    };
    let dot_text = wide("...");
    SetTextColor(dc, accent);
    DrawTextW(
        dc,
        dot_text.as_ptr(),
        -1,
        &mut dot,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );

    SelectObject(dc, previous);
    DeleteObject(font as HGDIOBJ);
    GdiFlush();
    EndPaint(hwnd, &paint);
}

unsafe fn draw_weekly_daily_bars(
    dc: *mut c_void,
    rect: RECT,
    weekly: &LimitWindow,
    plan: &crate::planner::PlanView,
    accent: COLORREF,
    scale: f32,
) {
    let outer_padding = (6.0_f32 * scale).round() as i32;
    let date_width = (34.0_f32 * scale).round().max(30.0_f32) as i32;
    let time_width = (42.0_f32 * scale).round().max(36.0_f32) as i32;
    let date_gap = (4.0_f32 * scale).round().max(3.0_f32) as i32;
    let section_gap = (10.0_f32 * scale).round().max(7.0_f32) as i32;
    let left = rect.left + outer_padding;
    let right = rect.right - outer_padding;
    let bars_width =
        (right - left - date_width * 2 - time_width * 2 - date_gap * 4 - section_gap).max(2);
    let weekly_width = bars_width / 2;
    let daily_width = bars_width - weekly_width;

    let bar_height = (20.0_f32 * scale).round().max(16.0_f32) as i32;
    let bar_top = rect.top + ((rect.bottom - rect.top - bar_height) / 2).max(0);
    let bar_bottom = (bar_top + bar_height).min(rect.bottom);
    let (start_label, reset_label) = weekly_date_labels(weekly);
    let (daily_start_label, daily_end_label) = daily_time_labels(weekly, plan.active_slot);

    let mut start_rect = RECT {
        left,
        top: rect.top,
        right: left + date_width,
        bottom: rect.bottom,
    };
    draw_date_label(dc, &mut start_rect, &start_label);

    let weekly_rect = RECT {
        left: start_rect.right + date_gap,
        top: bar_top,
        right: start_rect.right + date_gap + weekly_width,
        bottom: bar_bottom,
    };
    draw_quota_bar(dc, weekly_rect, weekly.remaining_percent as f64, accent);

    let mut reset_rect = RECT {
        left: weekly_rect.right + date_gap,
        top: rect.top,
        right: weekly_rect.right + date_gap + date_width,
        bottom: rect.bottom,
    };
    draw_date_label(dc, &mut reset_rect, &reset_label);

    let mut daily_start_rect = RECT {
        left: reset_rect.right + section_gap,
        top: rect.top,
        right: reset_rect.right + section_gap + time_width,
        bottom: rect.bottom,
    };
    draw_date_label(dc, &mut daily_start_rect, &daily_start_label);

    let daily_rect = RECT {
        left: daily_start_rect.right + date_gap,
        top: bar_top,
        right: (daily_start_rect.right + date_gap + daily_width).min(right),
        bottom: bar_bottom,
    };
    draw_quota_bar(
        dc,
        daily_rect,
        daily_remaining_percent(plan.today_used, plan.today_budget),
        accent,
    );

    let mut daily_end_rect = RECT {
        left: daily_rect.right + date_gap,
        top: rect.top,
        right: (daily_rect.right + date_gap + time_width).min(right),
        bottom: rect.bottom,
    };
    draw_date_label(dc, &mut daily_end_rect, &daily_end_label);
}

unsafe fn draw_date_label(dc: *mut c_void, rect: &mut RECT, label: &str) {
    let label = wide(label);
    SetTextColor(dc, rgb(210, 210, 210));
    DrawTextW(
        dc,
        label.as_ptr(),
        -1,
        rect,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );
}

fn weekly_date_labels(weekly: &LimitWindow) -> (String, String) {
    let Some(reset) = weekly.resets_at.as_ref() else {
        return ("--".to_string(), "--".to_string());
    };
    let start = reset.clone() - ChronoDuration::minutes(weekly.duration_minutes as i64);
    (
        format!("{}/{}", start.month(), start.day()),
        format!("{}/{}", reset.month(), reset.day()),
    )
}

fn daily_time_labels(weekly: &LimitWindow, active_slot: usize) -> (String, String) {
    let Some(reset) = weekly.resets_at.as_ref() else {
        return ("R--:--".to_string(), "+--".to_string());
    };
    let cycle_start = reset.clone() - ChronoDuration::minutes(weekly.duration_minutes as i64);
    let slot_minutes = (weekly.duration_minutes as i64 / crate::planner::SLOT_COUNT as i64).max(1);
    let slot = active_slot.min(crate::planner::SLOT_COUNT - 1) as i64;
    let start = cycle_start + ChronoDuration::minutes(slot_minutes * slot);
    let span = if slot_minutes % 60 == 0 {
        format!("+{}h", slot_minutes / 60)
    } else {
        format!("+{}m", slot_minutes)
    };
    (format!("R{:02}:{:02}", start.hour(), start.minute()), span)
}

unsafe fn draw_single_quota_bar(
    dc: *mut c_void,
    rect: RECT,
    remaining_percent: f64,
    accent: COLORREF,
    scale: f32,
) {
    let padding = (6.0_f32 * scale).round() as i32;
    let bar_height = (20.0_f32 * scale).round().max(16.0_f32) as i32;
    let top = rect.top + ((rect.bottom - rect.top - bar_height) / 2).max(0);
    draw_quota_bar(
        dc,
        RECT {
            left: rect.left + padding,
            top,
            right: rect.right - padding,
            bottom: (top + bar_height).min(rect.bottom),
        },
        remaining_percent,
        accent,
    );
}

unsafe fn draw_quota_bar(dc: *mut c_void, rect: RECT, remaining_percent: f64, accent: COLORREF) {
    let percent = remaining_percent.clamp(0.0, 100.0);
    let width = (rect.right - rect.left).max(0);
    let height = (rect.bottom - rect.top).max(0);
    if width <= 0 || height <= 0 {
        return;
    }

    let outer_corner = glass_corner_diameter(height);
    fill_round_rect(dc, rect, outer_corner, rgb(112, 122, 142));

    let inner = RECT {
        left: rect.left + 1,
        top: rect.top + 1,
        right: rect.right - 1,
        bottom: rect.bottom - 1,
    };
    let inner_height = (inner.bottom - inner.top).max(1);
    let inner_corner = glass_corner_diameter(inner_height);
    fill_round_rect(dc, inner, inner_corner, rgb(29, 34, 42));

    let saved_dc = SaveDC(dc);
    let clip = CreateRoundRectRgn(
        inner.left,
        inner.top,
        inner.right + 1,
        inner.bottom + 1,
        inner_corner,
        inner_corner,
    );
    if !clip.is_null() {
        SelectClipRgn(dc, clip);

        // A stronger pane highlight gives the track a darker glass appearance without
        // requiring acrylic blur or composition effects.
        let upper_pane = RECT {
            left: inner.left,
            top: inner.top,
            right: inner.right,
            bottom: (inner.top + (inner_height / 2).max(3)).min(inner.bottom),
        };
        let upper_brush = CreateSolidBrush(rgb(45, 52, 64));
        FillRect(dc, &upper_pane, upper_brush);
        DeleteObject(upper_brush as HGDIOBJ);

        let glass_glint = RECT {
            left: inner.left + 3,
            top: inner.top + 1,
            right: inner.right - 3,
            bottom: (inner.top + 2).min(inner.bottom),
        };
        if glass_glint.right > glass_glint.left && glass_glint.bottom > glass_glint.top {
            let glass_brush = CreateSolidBrush(rgb(134, 145, 166));
            FillRect(dc, &glass_glint, glass_brush);
            DeleteObject(glass_brush as HGDIOBJ);
        }

        let glass_shadow = RECT {
            left: inner.left + 2,
            top: (inner.bottom - 3).max(inner.top),
            right: inner.right - 2,
            bottom: inner.bottom,
        };
        if glass_shadow.right > glass_shadow.left && glass_shadow.bottom > glass_shadow.top {
            let shadow_brush = CreateSolidBrush(rgb(19, 23, 29));
            FillRect(dc, &glass_shadow, shadow_brush);
            DeleteObject(shadow_brush as HGDIOBJ);
        }

        let inner_width = (inner.right - inner.left).max(0);
        let fill_width = quota_fill_width(inner_width, percent);
        if fill_width > 0 {
            let fill = RECT {
                left: inner.left,
                top: inner.top,
                right: (inner.left + fill_width).min(inner.right),
                bottom: inner.bottom,
            };
            let fill_brush = CreateSolidBrush(dim_color(accent, 92));
            FillRect(dc, &fill, fill_brush);
            DeleteObject(fill_brush as HGDIOBJ);

            let fill_glint = RECT {
                left: fill.left,
                top: fill.top,
                right: fill.right,
                bottom: (fill.top + 3).min(fill.bottom),
            };
            let glint_brush = CreateSolidBrush(lighten_color(accent, 34));
            FillRect(dc, &fill_glint, glint_brush);
            DeleteObject(glint_brush as HGDIOBJ);

            let fill_shadow = RECT {
                left: fill.left,
                top: (fill.bottom - 3).max(fill.top),
                right: fill.right,
                bottom: fill.bottom,
            };
            let fill_shadow_brush = CreateSolidBrush(dim_color(accent, 54));
            FillRect(dc, &fill_shadow, fill_shadow_brush);
            DeleteObject(fill_shadow_brush as HGDIOBJ);

            // Keep the remaining edge readable when only a few percent remain. The marker
            // sits on the true fill boundary instead of inflating the displayed quota.
            if percent < 12.0 && percent > 0.0 {
                let marker_x = fill.right.saturating_sub(1).max(inner.left);
                let marker = RECT {
                    left: marker_x,
                    top: inner.top + 2,
                    right: (marker_x + 2).min(inner.right),
                    bottom: inner.bottom - 2,
                };
                if marker.right > marker.left && marker.bottom > marker.top {
                    let marker_brush = CreateSolidBrush(lighten_color(accent, 58));
                    FillRect(dc, &marker, marker_brush);
                    DeleteObject(marker_brush as HGDIOBJ);
                }
            }
        }

        DeleteObject(clip as HGDIOBJ);
    }
    if saved_dc != 0 {
        RestoreDC(dc, saved_dc);
    }

    let label = wide(&format!("{:.0}%", percent));
    let mut shadow_rect = rect;
    shadow_rect.top += 1;
    shadow_rect.bottom += 1;
    SetTextColor(dc, rgb(9, 11, 15));
    DrawTextW(
        dc,
        label.as_ptr(),
        -1,
        &mut shadow_rect,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );

    let mut text_rect = rect;
    SetTextColor(dc, rgb(250, 252, 255));
    DrawTextW(
        dc,
        label.as_ptr(),
        -1,
        &mut text_rect,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );
    GdiFlush();
}

fn glass_corner_diameter(height: i32) -> i32 {
    ((height * 2) / 3).clamp(6, height.max(6))
}

fn quota_fill_width(width: i32, percent: f64) -> i32 {
    if width <= 0 || percent <= 0.0 {
        0
    } else {
        ((width as f64) * percent.clamp(0.0, 100.0) / 100.0).round() as i32
    }
}

unsafe fn fill_round_rect(dc: *mut c_void, rect: RECT, radius: i32, color: COLORREF) {
    if rect.right <= rect.left || rect.bottom <= rect.top {
        return;
    }
    let region = CreateRoundRectRgn(
        rect.left,
        rect.top,
        rect.right + 1,
        rect.bottom + 1,
        radius,
        radius,
    );
    if region.is_null() {
        return;
    }
    let brush = CreateSolidBrush(color);
    FillRgn(dc, region, brush);
    DeleteObject(brush as HGDIOBJ);
    DeleteObject(region as HGDIOBJ);
}

fn dim_color(color: COLORREF, percent: u8) -> COLORREF {
    let scale = percent as u32;
    let red = ((color & 0xff) * scale / 100) as u8;
    let green = (((color >> 8) & 0xff) * scale / 100) as u8;
    let blue = (((color >> 16) & 0xff) * scale / 100) as u8;
    rgb(red, green, blue)
}

fn lighten_color(color: COLORREF, amount: u8) -> COLORREF {
    let amount = amount as u32;
    let lift = |channel: u32| -> u8 { (channel + ((255 - channel) * amount / 100)).min(255) as u8 };
    rgb(
        lift(color & 0xff),
        lift((color >> 8) & 0xff),
        lift((color >> 16) & 0xff),
    )
}

fn daily_remaining_percent(today_used: f64, today_budget: f64) -> f64 {
    if today_budget <= f64::EPSILON {
        return if today_used <= f64::EPSILON {
            100.0
        } else {
            0.0
        };
    }
    ((today_budget - today_used) / today_budget * 100.0).clamp(0.0, 100.0)
}

const fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    red as u32 | ((green as u32) << 8) | ((blue as u32) << 16)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

const fn empty_rect() -> RECT {
    RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    }
}

pub(crate) fn codex_is_active() -> bool {
    CODEX_ACTIVE.load(Ordering::Relaxed)
}

pub(crate) fn update_snapshot(snapshot: UsageSnapshot) {
    crate::planner::update_from_snapshot(&snapshot);
    let Some(state) = STATE.get() else { return };
    let hwnd = match state.lock() {
        Ok(mut state) => {
            state.snapshot = snapshot;
            state.overlay
        }
        Err(_) => return,
    };
    unsafe {
        InvalidateRect(hwnd, ptr::null(), 0);
    }
}

fn cycle_palette(hwnd: HWND) {
    let Some(state) = STATE.get() else { return };
    let palette_index = match state.lock() {
        Ok(mut state) => {
            state.palette_index = (state.palette_index + 1) % PALETTES.len();
            state.palette_index
        }
        Err(_) => return,
    };
    crate::settings::Settings { palette_index }.save();
    unsafe {
        InvalidateRect(hwnd, ptr::null(), 0);
    }
}

pub(crate) fn codex_desktop_cli_source() -> Option<PathBuf> {
    let target = STATE.get()?.lock().ok()?.target;
    if target.is_null() {
        return None;
    }
    let app_executable = unsafe { process_path_for_window(target) }?;
    let app_directory = app_executable.parent()?;
    let source = app_directory.join("resources").join("codex.exe");
    source.is_file().then_some(source)
}

unsafe fn process_path_for_window(hwnd: HWND) -> Option<PathBuf> {
    let mut process_id = 0;
    GetWindowThreadProcessId(hwnd, &mut process_id);
    if process_id == 0 {
        return None;
    }
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
    if process.is_null() {
        return None;
    }
    let mut buffer = [0_u16; 1024];
    let mut length = buffer.len() as u32;
    let ok = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length);
    CloseHandle(process);
    (ok != 0).then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_layout_is_right_aligned_and_preserves_menu_space() {
        let (left, width) = overlay_layout(1_200, 1.0, 220.0).expect("layout");
        assert_eq!(width, 220);
        assert_eq!(left, 822);
        assert_eq!(1_200 - left - width, 158);
        assert!(left >= 220);
    }

    #[test]
    fn weekly_only_layout_has_room_for_the_plan_text() {
        let (left, width) = overlay_layout(1_200, 1.0, 300.0).expect("layout");
        assert_eq!(width, 300);
        assert_eq!(left, 742);
        assert_eq!(1_200 - left - width, 158);
    }

    #[test]
    fn compact_layout_scales_with_dpi() {
        let (left, width) = overlay_layout(1_800, 1.5, 220.0).expect("layout");
        assert_eq!(width, 330);
        assert_eq!(1_800 - left - width, 237);
    }

    #[test]
    fn compact_layout_hides_instead_of_covering_menus() {
        assert_eq!(overlay_layout(500, 1.0, 220.0), None);
    }

    #[test]
    fn codex_main_window_is_an_attachment_candidate() {
        assert!(is_main_window_candidate(WS_CAPTION, 0));
    }

    #[test]
    fn codex_pet_tool_window_is_not_an_attachment_candidate() {
        assert!(!is_main_window_candidate(WS_CAPTION, WS_EX_TOOLWINDOW));
        assert!(!is_main_window_candidate(WS_POPUP, WS_EX_TOOLWINDOW));
    }

    #[test]
    fn daily_remaining_uses_today_budget_as_one_hundred_percent() {
        assert!((daily_remaining_percent(2.5, 10.0) - 75.0).abs() < 0.001);
        assert_eq!(daily_remaining_percent(10.0, 10.0), 0.0);
        assert_eq!(daily_remaining_percent(12.0, 10.0), 0.0);
    }

    #[test]
    fn empty_daily_budget_is_full_until_usage_exists() {
        assert_eq!(daily_remaining_percent(0.0, 0.0), 100.0);
        assert_eq!(daily_remaining_percent(1.0, 0.0), 0.0);
    }

    #[test]
    fn location_hook_only_accepts_the_current_codex_window() {
        let target = 0x1234usize as HWND;
        let other = 0x5678usize as HWND;
        assert!(should_sync_location_event(
            EVENT_OBJECT_LOCATIONCHANGE,
            target,
            target,
            OBJID_WINDOW,
            CHILDID_SELF as i32,
        ));
        assert!(!should_sync_location_event(
            EVENT_OBJECT_LOCATIONCHANGE,
            other,
            target,
            OBJID_WINDOW,
            CHILDID_SELF as i32,
        ));
        assert!(!should_sync_location_event(
            EVENT_OBJECT_LOCATIONCHANGE,
            target,
            target,
            OBJID_WINDOW - 1,
            CHILDID_SELF as i32,
        ));
    }

    #[test]
    fn low_quota_keeps_true_fill_width_without_minimum_inflation() {
        assert_eq!(quota_fill_width(160, 1.0), 2);
        assert_eq!(quota_fill_width(160, 5.0), 8);
        assert_eq!(quota_fill_width(160, 0.0), 0);
        assert!(glass_corner_diameter(20) < 20);
    }

    #[test]
    fn glass_color_helpers_keep_the_accent_controlled() {
        assert_eq!(dim_color(rgb(100, 150, 200), 50), rgb(50, 75, 100));
        assert_eq!(lighten_color(rgb(0, 0, 0), 20), rgb(51, 51, 51));
    }

    #[test]
    fn weekly_dates_show_window_start_and_reset() {
        use chrono::{Local, TimeZone};

        let reset = Local
            .with_ymd_and_hms(2026, 9, 15, 6, 0, 0)
            .single()
            .expect("local date");
        let weekly = LimitWindow {
            duration_minutes: 10_080,
            remaining_percent: 63,
            resets_at: Some(reset),
        };
        assert_eq!(
            weekly_date_labels(&weekly),
            ("9/8".to_string(), "9/15".to_string())
        );
        assert_eq!(
            daily_time_labels(&weekly, 0),
            ("R06:00".to_string(), "+24h".to_string())
        );
        assert_eq!(
            daily_time_labels(&weekly, 3),
            ("R06:00".to_string(), "+24h".to_string())
        );
    }
}
