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
use chrono::{Datelike, Local};
use windows_sys::Win32::{
    Foundation::{
        BOOL, COLORREF, CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM,
        LRESULT, RECT, TRUE, WPARAM,
    },
    Graphics::{
        Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
        Gdi::{
            BeginPaint, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_PITCH, DT_CENTER,
            DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, EndPaint, FF_DONTCARE, FW_NORMAL,
            FillRect, GdiFlush, HGDIOBJ, InvalidateRect, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
            SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
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
        HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
            SetProcessDpiAwarenessContext,
        },
        Input::KeyboardAndMouse::ReleaseCapture,
        WindowsAndMessaging::{
            CREATESTRUCTW, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW,
            DispatchMessageW, EnumWindows, GWL_EXSTYLE, GWL_STYLE, GWLP_HWNDPARENT, GWLP_USERDATA,
            GetClientRect, GetMessageW, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId,
            HCURSOR, HTCAPTION, HWND_TOP, IDC_ARROW, IsIconic, IsWindowVisible, LWA_ALPHA,
            LoadCursorW, MA_NOACTIVATE, MSG, PostQuitMessage, RegisterClassW, SW_HIDE,
            SWP_NOACTIVATE, SWP_SHOWWINDOW, SendMessageW, SetLayeredWindowAttributes, SetTimer,
            SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, WM_DESTROY,
            WM_ERASEBKGND, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_MOUSEACTIVATE, WM_NCCREATE,
            WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_PAINT, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
            WS_CAPTION, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
        },
    },
};

use crate::{locale::AppLocale, model::UsageSnapshot, planner::PlanView};

const CLASS_NAME: &str = "WeeklyUsageBar.Overlay";
const WINDOW_NAME: &str = "Weekly Usage Bar";
const MUTEX_NAME: &str = "Local\\WeeklyUsageBar.4BC6AD61";
const TRACK_TIMER: usize = 1;
const TRACK_INTERVAL_MS: u32 = 250;
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

fn track_codex_window() {
    let Some(state_lock) = STATE.get() else {
        return;
    };
    let mut state = match state_lock.lock() {
        Ok(state) => state,
        Err(_) => return,
    };
    let now = Instant::now();
    if now >= state.next_locale_check {
        let locale = AppLocale::detect();
        if locale != state.locale {
            state.locale = locale;
            unsafe { InvalidateRect(state.overlay, ptr::null(), 0) };
        }
        state.next_locale_check = now + LOCALE_CHECK_INTERVAL;
    }
    let target = find_codex_window();
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
        300.0_f32
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
        InvalidateRect(state.overlay, ptr::null(), 0);
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

struct UsageBarLayout {
    weekly: RECT,
    date: RECT,
    daily: RECT,
}

fn usage_bar_layout(width: i32, height: i32, scale: f32) -> Option<UsageBarLayout> {
    let padding = (4.0_f32 * scale).round().max(3.0_f32) as i32;
    let major_gap = (7.0_f32 * scale).round().max(5.0_f32) as i32;
    let minor_gap = (4.0_f32 * scale).round().max(3.0_f32) as i32;
    let date_width = (38.0_f32 * scale).round().max(34.0_f32) as i32;
    let inner_width = width - padding * 2;
    let bars_width = inner_width - major_gap - date_width - minor_gap;
    let minimum_bars_width = (110.0_f32 * scale).round() as i32;
    if bars_width < minimum_bars_width || height <= padding * 2 {
        return None;
    }

    let weekly_width = (bars_width * 42 / 100).max((48.0_f32 * scale).round() as i32);
    if weekly_width >= bars_width {
        return None;
    }
    let daily_width = bars_width - weekly_width;
    let desired_bar_height = (20.0_f32 * scale).round() as i32;
    let bar_height = desired_bar_height.min(height - padding * 2).max(1);
    let bar_top = (height - bar_height) / 2;
    let bar_bottom = bar_top + bar_height;

    let weekly = RECT {
        left: padding,
        top: bar_top,
        right: padding + weekly_width,
        bottom: bar_bottom,
    };
    let date = RECT {
        left: weekly.right + major_gap,
        top: 0,
        right: weekly.right + major_gap + date_width,
        bottom: height,
    };
    let daily = RECT {
        left: date.right + minor_gap,
        top: bar_top,
        right: date.right + minor_gap + daily_width,
        bottom: bar_bottom,
    };

    Some(UsageBarLayout {
        weekly,
        date,
        daily,
    })
}

fn daily_remaining_percent(plan: &PlanView) -> u8 {
    if plan.today_budget <= f64::EPSILON {
        return 0;
    }
    let remaining = (plan.today_budget - plan.today_used).max(0.0);
    ((remaining / plan.today_budget) * 100.0)
        .clamp(0.0, 100.0)
        .round() as u8
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
    let face = wide("Segoe UI");
    let font = CreateFontW(
        font_height,
        0,
        0,
        0,
        FW_NORMAL as i32,
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

    let plan = crate::planner::current_view();
    if let Some(weekly) = snapshot.weekly.as_ref() {
        if let Some(layout) = usage_bar_layout(client.right, client.bottom, scale) {
            draw_percent_bar(dc, layout.weekly, Some(weekly.remaining_percent), accent);

            let today = Local::now();
            let date_text = wide(&format!("{}/{}", today.month(), today.day()));
            let mut date_rect = layout.date;
            SetTextColor(dc, rgb(190, 190, 190));
            DrawTextW(
                dc,
                date_text.as_ptr(),
                -1,
                &mut date_rect,
                DT_CENTER | DT_SINGLELINE | DT_VCENTER,
            );

            let daily_percent = plan.as_ref().map(daily_remaining_percent);
            draw_percent_bar(dc, layout.daily, daily_percent, accent);
        }
    } else {
        let status = wide(
            snapshot
                .status
                .map(|status| locale.status_text(status))
                .unwrap_or_else(|| locale.status_text(crate::model::UsageStatus::Retrying)),
        );
        let mut status_rect = client;
        SetTextColor(dc, rgb(150, 150, 150));
        DrawTextW(
            dc,
            status.as_ptr(),
            -1,
            &mut status_rect,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }

    SelectObject(dc, previous);
    DeleteObject(font as HGDIOBJ);
    GdiFlush();
    EndPaint(hwnd, &paint);
}

unsafe fn draw_percent_bar(
    dc: *mut c_void,
    rect: RECT,
    remaining_percent: Option<u8>,
    accent: COLORREF,
) {
    let track = CreateSolidBrush(rgb(61, 61, 61));
    FillRect(dc, &rect, track);
    DeleteObject(track as HGDIOBJ);

    if let Some(percent) = remaining_percent {
        let width = (rect.right - rect.left).max(0);
        let fill_width = width * percent.clamp(0, 100) as i32 / 100;
        if fill_width > 0 {
            let fill = RECT {
                left: rect.left,
                top: rect.top,
                right: rect.left + fill_width,
                bottom: rect.bottom,
            };
            let brush = CreateSolidBrush(accent);
            FillRect(dc, &fill, brush);
            DeleteObject(brush as HGDIOBJ);
        }
    }

    let label = remaining_percent
        .map(|percent| format!("{}%", percent))
        .unwrap_or_else(|| "--".to_string());
    let label = wide(&label);
    let mut text_rect = rect;
    SetTextColor(dc, rgb(238, 238, 238));
    DrawTextW(
        dc,
        label.as_ptr(),
        -1,
        &mut text_rect,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );
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
    fn weekly_layout_prefers_300_pixels() {
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
    fn side_by_side_usage_bars_fit_compact_width() {
        let layout = usage_bar_layout(220, 30, 1.0).expect("bar layout");
        assert!(layout.weekly.right > layout.weekly.left);
        assert!(layout.date.left > layout.weekly.right);
        assert!(layout.daily.left > layout.date.left);
        assert!(layout.daily.right <= 220);
    }

    #[test]
    fn daily_remaining_is_normalized_to_daily_budget() {
        let plan = PlanView {
            active_slot: 3,
            fill_ratios: [None; 7],
            reset_markers: [false; 7],
            today_budget: 16.0,
            today_used: 4.0,
        };
        assert_eq!(daily_remaining_percent(&plan), 75);
    }

    #[test]
    fn daily_remaining_clamps_overspend_to_zero() {
        let plan = PlanView {
            active_slot: 3,
            fill_ratios: [None; 7],
            reset_markers: [false; 7],
            today_budget: 16.0,
            today_used: 20.0,
        };
        assert_eq!(daily_remaining_percent(&plan), 0);
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
}
