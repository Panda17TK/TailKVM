use serde::Serialize;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{LPARAM, POINT, RECT, TRUE};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, HDC, HMONITOR, MONITORINFO,
    MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

const MONITORINFOF_PRIMARY_VALUE: u32 = 0x00000001;

/// Make this process Per-Monitor-V2 DPI aware so that `GetCursorPos`,
/// `SetCursorPos`, monitor rectangles (`EnumDisplayMonitors`) and `SendInput`
/// all operate in the same **physical-pixel virtual-desktop** coordinate space.
///
/// Without this, secondary monitors with a different DPI return *virtualized*
/// coordinates, which silently breaks edge detection, screen transitions, and
/// absolute cursor positioning. Idempotent: if awareness is already set (e.g.
/// by the embedded application manifest) the call is a no-op. Call once at
/// startup, before any cursor/monitor query.
pub fn ensure_per_monitor_dpi_aware() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MonitorTopology {
    pub virtual_screen: RectI32,
    pub monitors: Vec<MonitorInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MonitorInfo {
    pub id: String,
    pub name: String,
    pub rect_physical_px: RectI32,
    pub work_area_physical_px: RectI32,
    pub dpi_x: u32,
    pub dpi_y: u32,
    pub scale_factor: f64,
    pub is_primary: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RectI32 {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub width: i32,
    pub height: i32,
}

impl RectI32 {
    fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
            width: right - left,
            height: bottom - top,
        }
    }

    fn from_rect(rect: RECT) -> Self {
        Self::new(rect.left, rect.top, rect.right, rect.bottom)
    }
}

pub fn get_monitor_topology() -> Result<MonitorTopology, String> {
    let virtual_left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let virtual_top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let virtual_width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let virtual_height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };

    let virtual_screen = RectI32::new(
        virtual_left,
        virtual_top,
        virtual_left + virtual_width,
        virtual_top + virtual_height,
    );

    let mut monitors: Vec<MonitorInfo> = Vec::new();

    let ok = unsafe {
        EnumDisplayMonitors(
            null_mut(),
            null(),
            Some(enum_monitor_proc),
            &mut monitors as *mut Vec<MonitorInfo> as LPARAM,
        )
    };

    if ok == 0 {
        return Err("EnumDisplayMonitors failed".to_string());
    }

    if monitors.is_empty() {
        return Err("No monitors detected".to_string());
    }

    monitors.sort_by(|a, b| {
        b.is_primary
            .cmp(&a.is_primary)
            .then_with(|| a.rect_physical_px.left.cmp(&b.rect_physical_px.left))
            .then_with(|| a.rect_physical_px.top.cmp(&b.rect_physical_px.top))
    });

    for (index, monitor) in monitors.iter_mut().enumerate() {
        monitor.id = format!("monitor-{}", index + 1);
    }

    Ok(MonitorTopology {
        virtual_screen,
        monitors,
    })
}

unsafe extern "system" fn enum_monitor_proc(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> i32 {
    let monitors = &mut *(lparam as *mut Vec<MonitorInfo>);

    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            rcMonitor: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            rcWork: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            dwFlags: 0,
        },
        szDevice: [0; 32],
    };

    let info_ptr = &mut info as *mut MONITORINFOEXW as *mut MONITORINFO;

    if GetMonitorInfoW(hmonitor, info_ptr) == 0 {
        return TRUE;
    }

    let name = utf16_array_to_string(&info.szDevice);

    let mut dpi_x: u32 = 96;
    let mut dpi_y: u32 = 96;

    let dpi_result = GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);

    if dpi_result < 0 {
        dpi_x = 96;
        dpi_y = 96;
    }

    let scale_factor = dpi_x as f64 / 96.0;
    let is_primary = (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY_VALUE) != 0;

    monitors.push(MonitorInfo {
        id: String::new(),
        name,
        rect_physical_px: RectI32::from_rect(info.monitorInfo.rcMonitor),
        work_area_physical_px: RectI32::from_rect(info.monitorInfo.rcWork),
        dpi_x,
        dpi_y,
        scale_factor,
        is_primary,
    });

    TRUE
}

fn utf16_array_to_string(value: &[u16]) -> String {
    let len = value.iter().position(|&ch| ch == 0).unwrap_or(value.len());
    String::from_utf16_lossy(&value[..len])
}

/// Physical-pixel rect `(left, top, right, bottom)` of the monitor that contains
/// (or is nearest to) the point `(x, y)` in virtual-desktop coordinates.
///
/// Used by seamless KVM so the switch edge is the edge of the monitor the cursor
/// is actually on — reachable on every monitor — instead of the full
/// virtual-screen edge, which is unreachable on shorter monitors in a mixed
/// multi-monitor layout. Falls back to the full virtual screen if the query
/// fails.
pub fn monitor_rect_at_point(x: i32, y: i32) -> (i32, i32, i32, i32) {
    unsafe {
        let hmonitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        if !hmonitor.is_null() {
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(hmonitor, &mut info) != 0 {
                let r = info.rcMonitor;
                return (r.left, r.top, r.right, r.bottom);
            }
        }
    }

    // Fallback: full virtual screen.
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    (left, top, left + width, top + height)
}
