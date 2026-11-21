//! Native Win32 window. Two list views (adapters; paired devices grouped by adapter), a button
//! row under each, a menu bar with the same commands, and right-click menus on both lists.
//! Scans and system changes run on worker threads and post results back, so the UI never blocks.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem::{size_of, transmute, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, DrawTextW, GetDC, GetMonitorInfoW, GetSysColor, GetSysColorBrush,
    InvalidateRect, MonitorFromWindow, ReleaseDC, SelectObject, SetBkColor, SetTextColor, COLOR_GRAYTEXT,
    COLOR_WINDOW, COLOR_WINDOWTEXT, DT_CALCRECT, DT_NOPREFIX, DT_SINGLELINE, DT_WORDBREAK, HDC, HFONT, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::UI::Controls::{
    SetWindowTheme, INITCOMMONCONTROLSEX, LVCOLUMNW, LVGROUP, LVITEMW, NMHDR, NMLVCUSTOMDRAW, NMLVKEYDOWN,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForWindow, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext, SystemParametersInfoForDpi,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, GetFocus};
use windows_sys::Win32::UI::Shell::{ExtractIconExW, ShellExecuteW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::control::{self, Op, OpResult};
use crate::devices::{self, format_address, format_ago, format_date, now_filetime, wide, Adapter, Device, Snapshot};

// ---- command ids ------------------------------------------------------------------------------

const ID_USE: u16 = 101;
const ID_RESTART: u16 = 102;
const ID_POWER: u16 = 103;
const ID_ALL_ON: u16 = 105;
const ID_FORGET_ADAPTER: u16 = 106;
const ID_ADAPTER_DETAILS: u16 = 107;
const ID_FORGET_DEVICE: u16 = 201;
const ID_MIC: u16 = 202;
const ID_CLEANUP: u16 = 203;
const ID_DEVICE_DETAILS: u16 = 204;
const ID_ABSVOL: u16 = 301;
const ID_SOUND: u16 = 302;
const ID_REFRESH: u16 = 401;
const ID_SETTINGS: u16 = 402;
const ID_DEVMGR: u16 = 403;
const ID_TROUBLESHOOT: u16 = 404;
const ID_TIPS: u16 = 501;
const ID_ABOUT: u16 = 502;
const ID_LV_ADAPTERS: u16 = 901;
const ID_LV_DEVICES: u16 = 902;

const WM_APP_SCANNED: u32 = WM_APP + 1;
const WM_APP_DONE: u32 = WM_APP + 2;
const TIMER_RESCAN: usize = 1;

// ---- common-controls constants (kept local so they're exact) ---------------------------------

const LVM_SETCOLUMNWIDTH: u32 = 0x101E;
const LVM_DELETEALLITEMS: u32 = 0x1009;
const LVM_GETNEXTITEM: u32 = 0x100C;
const LVM_SETITEMSTATE: u32 = 0x102B;
const LVM_SETEXTENDEDLISTVIEWSTYLE: u32 = 0x1036;
const LVM_GETITEMW: u32 = 0x104B;
const LVM_INSERTITEMW: u32 = 0x104D;
const LVM_INSERTCOLUMNW: u32 = 0x1061;
const LVM_SETITEMTEXTW: u32 = 0x1074;
const LVM_INSERTGROUP: u32 = 0x1091;
const LVM_ENABLEGROUPVIEW: u32 = 0x109D;
const LVM_REMOVEALLGROUPS: u32 = 0x10A0;

const LVS_REPORT: u32 = 0x1;
const LVS_SINGLESEL: u32 = 0x4;
const LVS_SHOWSELALWAYS: u32 = 0x8;
const LVS_NOSORTHEADER: u32 = 0x8000;
const LVS_EX_FULLROWSELECT: u32 = 0x20;
const LVS_EX_DOUBLEBUFFER: u32 = 0x10000;

const LVIF_TEXT: u32 = 0x1;
const LVIF_PARAM: u32 = 0x4;
const LVIF_GROUPID: u32 = 0x100;
const LVIS_FOCUSED: u32 = 0x1;
const LVIS_SELECTED: u32 = 0x2;
const LVNI_SELECTED: u32 = 0x2;
const LVCF_WIDTH: u32 = 0x2;
const LVCF_TEXT: u32 = 0x4;
const LVCF_SUBITEM: u32 = 0x8;
const LVGF_HEADER: u32 = 0x1;
const LVGF_GROUPID: u32 = 0x10;

const LVN_ITEMCHANGED: u32 = (-101i32) as u32;
const LVN_KEYDOWN: u32 = (-155i32) as u32;
const NM_DBLCLK: u32 = (-3i32) as u32;
const NM_RETURN: u32 = (-4i32) as u32;
const NM_CUSTOMDRAW: u32 = (-12i32) as u32;
const CDDS_PREPAINT: u32 = 0x1;
const CDDS_ITEMPREPAINT: u32 = 0x10001;
const CDRF_DODEFAULT: LRESULT = 0x0;
const CDRF_NEWFONT: LRESULT = 0x2;
const CDRF_NOTIFYITEMDRAW: LRESULT = 0x20;

const SS_NOPREFIX: u32 = 0x80;
const SS_ENDELLIPSIS: u32 = 0x4000;
const ICC_LISTVIEW_CLASSES: u32 = 0x1;
const ICC_STANDARD_CLASSES: u32 = 0x4000;
const WM_DPICHANGED_: u32 = 0x02E0;
const VK_DELETE_: u16 = 0x2E;
const VK_F5_: u16 = 0x74;

const ORPHAN: usize = 0xFFFF;
const ORPHAN_GROUP: i32 = 9999;
const APP_NAME: &str = "Windows Bluetooth BS Fixer";

const INTRO: &str = "See every Bluetooth adapter and device Windows knows about, choose which adapter \
Windows uses, and fix common Bluetooth problems. Windows only runs one adapter at a time \u{2014} select the one \
you want and click \u{201C}Use this adapter\u{201D}. More fixes are in the menus and on right-click.";

const TIPS: &str = "Common Windows Bluetooth problems and what to try\n\n\
\u{2022} Bluetooth stopped working, the on/off toggle vanished, or nothing connects\n    \u{2192} Restart Bluetooth.\n\n\
\u{2022} Devices disconnect after a while or after the PC wakes from sleep\n    \u{2192} Adapter \u{25B8} Power saving: turn it off.\n\n\
\u{2022} Headphones sound muffled or mono during calls, games or whenever an app opens the mic\n    \u{2192} Select them and turn off the headset mic. They stay in full-quality stereo (use another mic).\n\n\
\u{2022} Volume is far too loud/quiet, or Windows and headphone volume fight each other\n    \u{2192} Audio \u{25B8} turn Absolute volume off.\n\n\
\u{2022} A device won't remove in Settings, or shows up twice\n    \u{2192} Select it and Forget device, or Device \u{25B8} Clean up old devices.\n\n\
\u{2022} A USB Bluetooth dongle is ignored\n    \u{2192} Windows sticks with the built-in adapter. Select the dongle and click Use this adapter.\n\n\
\u{2022} Lag, stutter or dropouts with several devices\n    \u{2192} Bluetooth shares the 2.4 GHz band with Wi-Fi, wireless mouse/keyboard dongles and noisy USB 3 ports. \
Move dongles away from the Bluetooth adapter (a short USB extension cable helps a lot) and prefer 5 GHz Wi-Fi.\n\n\
\u{2022} Still unreliable\n    \u{2192} Install the adapter driver from your PC, motherboard or adapter maker (Adapter \u{25B8} Details shows the version you have).";

static SCAN_RESULT: Mutex<Option<Snapshot>> = Mutex::new(None);
static ACTION_RESULT: Mutex<Option<Outcome>> = Mutex::new(None);
static APP: AtomicPtr<App> = AtomicPtr::new(null_mut());

enum Followup {
    /// Offer to open Bluetooth settings after switching.
    Switched(String),
    /// Show this text plus any informative per-step messages.
    Message(String),
}

struct Outcome {
    followup: Followup,
    result: Result<Vec<OpResult>, String>,
}

fn app() -> Option<&'static App> {
    unsafe { APP.load(Ordering::Relaxed).as_ref() }
}

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

fn join_names<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for n in names {
        if !seen.contains(&n) {
            seen.push(n);
        }
    }
    seen.join(", ")
}

/// Selected device: (adapter index or ORPHAN, device index).
type DevSel = (usize, usize);

fn device_at(snap: &Snapshot, (ai, di): DevSel) -> Option<&Device> {
    if ai == ORPHAN { snap.orphans.get(di) } else { snap.adapters.get(ai)?.devices.get(di) }
}

fn cleanup_candidates(snap: &Snapshot) -> Vec<&Device> {
    snap.orphans
        .iter()
        .chain(snap.adapters.iter().filter(|a| !a.node.present).flat_map(|a| a.devices.iter()))
        .collect()
}

fn active_adapter(snap: &Snapshot) -> Option<&Adapter> {
    snap.adapters.iter().find(|a| a.active)
}

fn remove_ops(d: &Device) -> Vec<Op> {
    let mut ops: Vec<Op> =
        d.services.iter().map(|id| Op::Remove { id: id.clone(), label: format!("{} (service)", d.node.name) }).collect();
    ops.push(Op::Remove { id: d.node.id.clone(), label: d.node.name.clone() });
    ops
}

// ---- app state --------------------------------------------------------------------------------

struct App {
    hwnd: HWND,
    menu: HMENU,
    intro: HWND,
    lbl_adapters: HWND,
    lv_adapters: HWND,
    btn_use: HWND,
    btn_restart: HWND,
    btn_adapter_details: HWND,
    btn_settings: HWND,
    lbl_devices: HWND,
    lv_devices: HWND,
    btn_forget: HWND,
    btn_mic: HWND,
    btn_cleanup: HWND,
    btn_device_details: HWND,
    status: HWND,

    dpi: Cell<u32>,
    font: Cell<HFONT>,
    bold: Cell<HFONT>,
    snap: RefCell<Snapshot>,
    busy: Cell<bool>,
    scanning: Cell<bool>,
    rescan: Cell<bool>,
}

pub fn run() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); // manifest does this too
        init_common_controls();

        let hinst = GetModuleHandleW(null());
        let class = wide("WindowsBluetoothBSFixer");
        let mut wc: WNDCLASSEXW = zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.style = CS_HREDRAW | CS_VREDRAW;
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        wc.hbrBackground = GetSysColorBrush(COLOR_WINDOW);
        wc.lpszClassName = class.as_ptr();
        RegisterClassExW(&wc);

        let menu = build_menu();
        let title = wide(APP_NAME);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            960,
            760,
            null_mut(),
            menu,
            hinst,
            null(),
        );
        if hwnd.is_null() {
            return;
        }
        set_icon(hwnd);

        let st = SS_NOPREFIX;
        let bt = WS_TABSTOP | BS_PUSHBUTTON as u32;
        let lv = WS_TABSTOP | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS | LVS_NOSORTHEADER;
        let ellipsis = "\u{2026}";

        let state = Box::new(App {
            hwnd,
            menu,
            intro: child(hwnd, "STATIC", INTRO, st, 0, 0),
            lbl_adapters: child(hwnd, "STATIC", "Bluetooth adapters", st, 0, 0),
            lv_adapters: child(hwnd, "SysListView32", "", lv, WS_EX_CLIENTEDGE, ID_LV_ADAPTERS),
            btn_use: child(hwnd, "BUTTON", "Use this adapter", bt, 0, ID_USE),
            btn_restart: child(hwnd, "BUTTON", "Restart Bluetooth", bt, 0, ID_RESTART),
            btn_adapter_details: child(hwnd, "BUTTON", &format!("Details{ellipsis}"), bt, 0, ID_ADAPTER_DETAILS),
            btn_settings: child(hwnd, "BUTTON", &format!("Bluetooth settings{ellipsis}"), bt, 0, ID_SETTINGS),
            lbl_devices: child(hwnd, "STATIC", "Paired devices, by adapter", st, 0, 0),
            lv_devices: child(hwnd, "SysListView32", "", lv, WS_EX_CLIENTEDGE, ID_LV_DEVICES),
            btn_forget: child(hwnd, "BUTTON", "Forget device", bt, 0, ID_FORGET_DEVICE),
            btn_mic: child(hwnd, "BUTTON", "Turn off headset mic", bt, 0, ID_MIC),
            btn_cleanup: child(hwnd, "BUTTON", &format!("Clean up old devices{ellipsis}"), bt, 0, ID_CLEANUP),
            btn_device_details: child(hwnd, "BUTTON", &format!("Details{ellipsis}"), bt, 0, ID_DEVICE_DETAILS),
            status: child(hwnd, "STATIC", &format!("Scanning{ellipsis}"), st | SS_ENDELLIPSIS, 0, 0),
            dpi: Cell::new(GetDpiForWindow(hwnd).max(96)),
            font: Cell::new(null_mut()),
            bold: Cell::new(null_mut()),
            snap: RefCell::new(Snapshot::default()),
            busy: Cell::new(false),
            scanning: Cell::new(false),
            rescan: Cell::new(false),
        });
        APP.store(Box::into_raw(state), Ordering::Release);
        let app = app().unwrap();

        init_list(app.lv_adapters, &["Adapter", "Made by", "Version", "State", "Paired"], false);
        init_list(app.lv_devices, &["Device", "Type", "Battery", "Status", "Last connected"], true);
        app.apply_fonts();
        app.update_buttons();
        app.place_window();
        ShowWindow(hwnd, SW_SHOWDEFAULT);
        app.start_scan();

        let accels = [ACCEL { fVirt: FVIRTKEY as u8, key: VK_F5_, cmd: ID_REFRESH }];
        let haccel = CreateAcceleratorTableW(accels.as_ptr(), accels.len() as i32);

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if TranslateAcceleratorW(hwnd, haccel, &msg) != 0 {
                continue;
            }
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

/// Registers the ComCtl32 v6 list-view classes (v6 is selected by the embedded manifest).
unsafe fn init_common_controls() {
    let lib = LoadLibraryW(wide("comctl32.dll").as_ptr());
    if let Some(proc_) = GetProcAddress(lib, b"InitCommonControlsEx\0".as_ptr()) {
        let init: unsafe extern "system" fn(*const INITCOMMONCONTROLSEX) -> BOOL = transmute(proc_);
        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES,
        };
        init(&icc);
    }
}

unsafe fn build_menu() -> HMENU {
    let item = |m: HMENU, id: u16, text: &str| {
        AppendMenuW(m, MF_STRING, id as usize, wide(text).as_ptr());
    };
    let sep = |m: HMENU| {
        AppendMenuW(m, MF_SEPARATOR, 0, null());
    };
    let bar = CreateMenu();

    let m = CreatePopupMenu();
    item(m, ID_USE, "&Use this adapter\tEnter");
    item(m, ID_RESTART, "&Restart Bluetooth");
    sep(m);
    item(m, ID_POWER, "Power &saving (turn off to fix dropouts)");
    sep(m);
    item(m, ID_ALL_ON, "Turn all adapters &back on");
    item(m, ID_FORGET_ADAPTER, "&Forget this adapter");
    sep(m);
    item(m, ID_ADAPTER_DETAILS, "&Details\u{2026}");
    AppendMenuW(bar, MF_POPUP, m as usize, wide("&Adapter").as_ptr());

    let m = CreatePopupMenu();
    item(m, ID_FORGET_DEVICE, "&Forget device\tDel");
    item(m, ID_MIC, "Headset &mic (hands-free)");
    sep(m);
    item(m, ID_CLEANUP, "&Clean up old devices\u{2026}");
    sep(m);
    item(m, ID_DEVICE_DETAILS, "&Details\u{2026}\tEnter");
    AppendMenuW(bar, MF_POPUP, m as usize, wide("&Device").as_ptr());

    let m = CreatePopupMenu();
    item(m, ID_ABSVOL, "&Absolute volume");
    sep(m);
    item(m, ID_SOUND, "&Sound settings");
    AppendMenuW(bar, MF_POPUP, m as usize, wide("A&udio").as_ptr());

    let m = CreatePopupMenu();
    item(m, ID_REFRESH, "&Refresh\tF5");
    sep(m);
    item(m, ID_SETTINGS, "&Bluetooth settings");
    item(m, ID_DEVMGR, "&Device Manager");
    item(m, ID_TROUBLESHOOT, "&Troubleshooters");
    AppendMenuW(bar, MF_POPUP, m as usize, wide("&Tools").as_ptr());

    let m = CreatePopupMenu();
    item(m, ID_TIPS, "&Troubleshooting tips");
    item(m, ID_ABOUT, "&About");
    AppendMenuW(bar, MF_POPUP, m as usize, wide("&Help").as_ptr());
    bar
}

unsafe fn set_icon(hwnd: HWND) {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
    let path = wide(&format!(r"{root}\System32\fsquirt.exe")); // Bluetooth File Transfer: has the BT icon
    let mut large: HICON = null_mut();
    let mut small: HICON = null_mut();
    if ExtractIconExW(path.as_ptr(), 0, &mut large, &mut small, 1) > 0 {
        SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, large as isize);
        SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, small as isize);
    }
}

unsafe fn child(parent: HWND, class: &str, text: &str, style: u32, ex: u32, id: u16) -> HWND {
    let c = wide(class);
    let t = wide(text);
    CreateWindowExW(
        ex,
        c.as_ptr(),
        t.as_ptr(),
        WS_CHILD | WS_VISIBLE | style,
        0,
        0,
        0,
        0,
        parent,
        id as isize as HMENU,
        GetModuleHandleW(null()),
        null(),
    )
}

unsafe fn init_list(lv: HWND, columns: &[&str], groups: bool) {
    SetWindowTheme(lv, wide("Explorer").as_ptr(), null());
    let ex = (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as usize;
    SendMessageW(lv, LVM_SETEXTENDEDLISTVIEWSTYLE, ex, ex as isize);
    for (i, name) in columns.iter().enumerate() {
        let mut text = wide(name);
        let mut col: LVCOLUMNW = zeroed();
        col.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM;
        col.cx = 100;
        col.pszText = text.as_mut_ptr();
        col.iSubItem = i as i32;
        SendMessageW(lv, LVM_INSERTCOLUMNW, i, &col as *const _ as isize);
    }
    if groups {
        SendMessageW(lv, LVM_ENABLEGROUPVIEW, 1, 0);
    }
}

unsafe fn add_row(lv: HWND, index: i32, cells: &[&str], param: isize, group: Option<i32>) {
    let mut text = wide(cells[0]);
    let mut item: LVITEMW = zeroed();
    item.mask = LVIF_TEXT | LVIF_PARAM | if group.is_some() { LVIF_GROUPID } else { 0 };
    item.iItem = index;
    item.pszText = text.as_mut_ptr();
    item.lParam = param;
    if let Some(g) = group {
        item.iGroupId = g as _;
    }
    let row = SendMessageW(lv, LVM_INSERTITEMW, 0, &item as *const _ as isize);
    if row < 0 {
        return;
    }
    for (sub, cell) in cells.iter().enumerate().skip(1) {
        let mut text = wide(cell);
        let mut si: LVITEMW = zeroed();
        si.iSubItem = sub as i32;
        si.pszText = text.as_mut_ptr();
        SendMessageW(lv, LVM_SETITEMTEXTW, row as usize, &si as *const _ as isize);
    }
}

unsafe fn add_group(lv: HWND, id: i32, header: &str) {
    let mut text = wide(header);
    let mut g: LVGROUP = zeroed();
    g.cbSize = size_of::<LVGROUP>() as u32;
    g.mask = LVGF_HEADER | LVGF_GROUPID;
    g.pszHeader = text.as_mut_ptr();
    g.iGroupId = id;
    SendMessageW(lv, LVM_INSERTGROUP, usize::MAX, &g as *const _ as isize);
}

unsafe fn select_row(lv: HWND, row: usize) {
    let mut st: LVITEMW = zeroed();
    st.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
    st.state = LVIS_SELECTED | LVIS_FOCUSED;
    SendMessageW(lv, LVM_SETITEMSTATE, row, &st as *const _ as isize);
}

unsafe fn selected_param(lv: HWND) -> Option<usize> {
    let row = SendMessageW(lv, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
    if row < 0 {
        return None;
    }
    let mut item: LVITEMW = zeroed();
    item.mask = LVIF_PARAM;
    item.iItem = row as i32;
    SendMessageW(lv, LVM_GETITEMW, 0, &mut item as *mut _ as isize);
    Some(item.lParam as usize)
}

unsafe fn shell_open(hwnd: HWND, target: &str) {
    ShellExecuteW(hwnd, wide("open").as_ptr(), wide(target).as_ptr(), null(), null(), SW_SHOWNORMAL);
}

impl App {
    fn scale(&self, v: i32) -> i32 {
        (v * self.dpi.get() as i32 + 48) / 96
    }

    fn children(&self) -> [HWND; 14] {
        [
            self.intro,
            self.lbl_adapters,
            self.lv_adapters,
            self.btn_use,
            self.btn_restart,
            self.btn_adapter_details,
            self.btn_settings,
            self.lbl_devices,
            self.lv_devices,
            self.btn_forget,
            self.btn_mic,
            self.btn_cleanup,
            self.btn_device_details,
            self.status,
        ]
    }

    unsafe fn apply_fonts(&self) {
        let dpi = self.dpi.get();
        let mut ncm: NONCLIENTMETRICSW = zeroed();
        ncm.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
        SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS, ncm.cbSize, &mut ncm as *mut _ as *mut c_void, 0, dpi);
        let mut lf = ncm.lfMessageFont;
        lf.lfHeight = -((10 * dpi as i32 + 36) / 72); // 10 pt
        let font = CreateFontIndirectW(&lf);
        lf.lfWeight = 600;
        let bold = CreateFontIndirectW(&lf);

        for h in self.children() {
            let f = if h == self.lbl_adapters || h == self.lbl_devices { bold } else { font };
            SendMessageW(h, WM_SETFONT, f as usize, 1);
        }
        let (old_font, old_bold) = (self.font.replace(font), self.bold.replace(bold));
        if !old_font.is_null() {
            DeleteObject(old_font);
        }
        if !old_bold.is_null() {
            DeleteObject(old_bold);
        }
    }

    unsafe fn place_window(&self) {
        let (w, h) = (self.scale(960), self.scale(760));
        let mon = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        let (mut x, mut y) = (CW_USEDEFAULT, CW_USEDEFAULT);
        if GetMonitorInfoW(mon, &mut mi) != 0 {
            let wa = mi.rcWork;
            x = wa.left + ((wa.right - wa.left) - w).max(0) / 2;
            y = wa.top + ((wa.bottom - wa.top) - h).max(0) / 2;
        }
        SetWindowPos(self.hwnd, null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
    }

    unsafe fn measure_text(&self, ctl: HWND, text: Option<&str>, width: i32, font: HFONT, single: bool) -> (i32, i32) {
        let buf: Vec<u16> = match text {
            Some(t) => t.encode_utf16().collect(),
            None => {
                let len = GetWindowTextLengthW(ctl);
                let mut b = vec![0u16; len as usize + 1];
                GetWindowTextW(ctl, b.as_mut_ptr(), len + 1);
                b.truncate(len as usize);
                if b.is_empty() { "Ag".encode_utf16().collect() } else { b }
            }
        };
        let hdc = GetDC(ctl);
        let old = SelectObject(hdc, font);
        let mut rc = RECT { left: 0, top: 0, right: width, bottom: 0 };
        let flags = DT_CALCRECT | DT_NOPREFIX | if single { DT_SINGLELINE } else { DT_WORDBREAK };
        DrawTextW(hdc, buf.as_ptr(), buf.len() as i32, &mut rc, flags);
        SelectObject(hdc, old);
        ReleaseDC(ctl, hdc);
        (rc.right - rc.left, rc.bottom - rc.top)
    }

    unsafe fn layout(&self) {
        let mut rc: RECT = zeroed();
        GetClientRect(self.hwnd, &mut rc);
        let s = |v| self.scale(v);
        let (font, bold) = (self.font.get(), self.bold.get());
        let m = s(16);
        let w = (rc.right - 2 * m).max(s(200));
        let mv = |h: HWND, x, y, cw, ch| {
            MoveWindow(h, x, y, cw, ch, 1);
        };
        let btn_h = s(32);
        let btn_w = |h: HWND, widest: Option<&str>| (self.measure_text(h, widest, 2000, font, true).0 + s(32)).max(s(96));
        let row = |y: i32, left: &[(HWND, Option<&str>)], right: HWND| {
            let mut x = m;
            for (b, widest) in left {
                let bw = btn_w(*b, *widest);
                mv(*b, x, y, bw, btn_h);
                x += bw + s(8);
            }
            let rw = btn_w(right, None);
            mv(right, (m + w - rw).max(x), y, rw, btn_h);
        };

        let mut y = m - s(4);
        let (_, intro_h) = self.measure_text(self.intro, None, w, font, false);
        mv(self.intro, m, y, w, intro_h);
        y += intro_h + s(12);

        let (_, head_h) = self.measure_text(self.lbl_adapters, None, w, bold, true);
        let (_, line_h) = self.measure_text(self.status, Some("Ag"), w, font, true);
        let bottom = rc.bottom - m - line_h;
        mv(self.status, m, bottom, w, line_h);

        // Two header+list+buttons blocks share what's left; devices get the larger part.
        let block_fixed = head_h + s(6) + s(10) + btn_h + s(16);
        let avail = (bottom - s(8) - y - 2 * block_fixed).max(s(200));
        let lv1 = ((avail as f32 * 0.34) as i32).max(s(100));
        let lv2 = (avail - lv1).max(s(100));

        mv(self.lbl_adapters, m, y, w, head_h);
        y += head_h + s(6);
        mv(self.lv_adapters, m, y, w, lv1);
        y += lv1 + s(10);
        row(y, &[(self.btn_use, None), (self.btn_restart, None), (self.btn_adapter_details, None)], self.btn_settings);
        y += btn_h + s(16);

        mv(self.lbl_devices, m, y, w, head_h);
        y += head_h + s(6);
        mv(self.lv_devices, m, y, w, lv2);
        y += lv2 + s(10);
        row(
            y,
            &[(self.btn_forget, None), (self.btn_mic, Some("Turn off headset mic")), (self.btn_cleanup, None)],
            self.btn_device_details,
        );

        let inner = w - GetSystemMetricsForDpi(SM_CXVSCROLL, self.dpi.get()) - s(6);
        Self::set_columns(self.lv_adapters, inner, &[0.36, 0.22, 0.09, 0.23, 0.10]);
        Self::set_columns(self.lv_devices, inner, &[0.32, 0.15, 0.09, 0.26, 0.18]);
    }

    unsafe fn set_columns(lv: HWND, width: i32, shares: &[f32]) {
        for (i, share) in shares.iter().enumerate() {
            SendMessageW(lv, LVM_SETCOLUMNWIDTH, i, (width as f32 * share) as isize);
        }
    }

    fn set_status(&self, text: &str) {
        unsafe { SetWindowTextW(self.status, wide(text).as_ptr()) };
    }

    unsafe fn message(&self, text: &str, flags: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
        MessageBoxW(self.hwnd, wide(text).as_ptr(), wide(APP_NAME).as_ptr(), flags)
    }

    unsafe fn confirm(&self, text: &str) -> bool {
        self.message(text, MB_YESNO | MB_ICONQUESTION) == IDYES
    }

    // ---- scanning -------------------------------------------------------------------------

    fn start_scan(&self) {
        if self.scanning.replace(true) {
            self.rescan.set(true);
            return;
        }
        let hwnd = self.hwnd as isize;
        std::thread::spawn(move || {
            let snap = devices::scan();
            *SCAN_RESULT.lock().unwrap() = Some(snap);
            unsafe { PostMessageW(hwnd as HWND, WM_APP_SCANNED, 0, 0) };
        });
    }

    unsafe fn on_scanned(&self) {
        let Some(fresh) = SCAN_RESULT.lock().unwrap().take() else { return };
        let (sel_adapter, sel_device) = {
            let Ok(old) = self.snap.try_borrow() else {
                *SCAN_RESULT.lock().unwrap() = Some(fresh);
                PostMessageW(self.hwnd, WM_APP_SCANNED, 0, 0);
                return;
            };
            (
                self.selected_adapter().and_then(|i| old.adapters.get(i)).map(|a| a.node.id.clone()),
                self.selected_device().and_then(|s| device_at(&old, s)).map(|d| d.node.id.clone()),
            )
        };
        *self.snap.borrow_mut() = fresh;
        self.scanning.set(false);
        self.populate(sel_adapter.as_deref(), sel_device.as_deref());
        if self.rescan.replace(false) {
            self.start_scan();
        }
    }

    unsafe fn populate(&self, sel_adapter: Option<&str>, sel_device: Option<&str>) {
        let snap = self.snap.borrow();
        let now = now_filetime();

        SendMessageW(self.lv_adapters, WM_SETREDRAW, 0, 0);
        SendMessageW(self.lv_adapters, LVM_DELETEALLITEMS, 0, 0);
        for (i, a) in snap.adapters.iter().enumerate() {
            let count = a.devices.len().to_string();
            let cells = [a.node.name.as_str(), a.node.manufacturer.as_str(), a.version_text(), a.state_text(), &count];
            add_row(self.lv_adapters, i as i32, &cells, i as isize, None);
        }
        let select = snap
            .adapters
            .iter()
            .position(|a| Some(a.node.id.as_str()) == sel_adapter)
            .or_else(|| snap.adapters.iter().position(|a| a.active));
        if let Some(row) = select {
            select_row(self.lv_adapters, row);
        }
        SendMessageW(self.lv_adapters, WM_SETREDRAW, 1, 0);
        InvalidateRect(self.lv_adapters, null(), 1);

        SendMessageW(self.lv_devices, WM_SETREDRAW, 0, 0);
        SendMessageW(self.lv_devices, LVM_DELETEALLITEMS, 0, 0);
        SendMessageW(self.lv_devices, LVM_REMOVEALLGROUPS, 0, 0);
        let mut row = 0usize;
        let mut select_dev = None;
        let mut add_device = |d: &Device, ai: usize, di: usize, adapter: Option<&Adapter>, group: i32| {
            let mut status = if d.connected() {
                "Connected".to_owned()
            } else if adapter.is_some_and(|a| a.active) {
                "Paired \u{00B7} not connected".to_owned()
            } else {
                "Paired \u{00B7} adapter not in use".to_owned()
            };
            if d.hfp.as_ref().is_some_and(|h| h.disabled) {
                status.push_str(" \u{00B7} mic off");
            }
            let battery = d.battery.map(|b| format!("{b}%")).unwrap_or_default();
            let last = if d.connected() {
                "Now".to_owned()
            } else {
                d.last_connected.map(|t| format_ago(t, now)).unwrap_or_default()
            };
            let kind = d.kind();
            let cells = [d.node.name.as_str(), &kind, &battery, &status, &last];
            add_row(self.lv_devices, row as i32, &cells, ((ai << 16) | di) as isize, Some(group));
            if Some(d.node.id.as_str()) == sel_device {
                select_dev = Some(row);
            }
            row += 1;
        };
        for (ai, a) in snap.adapters.iter().enumerate() {
            if a.devices.is_empty() {
                continue;
            }
            add_group(self.lv_devices, ai as i32, &format!("{}  \u{00B7}  {}", a.node.name, a.state_text()));
            for (di, d) in a.devices.iter().enumerate() {
                add_device(d, ai, di, Some(a), ai as i32);
            }
        }
        if !snap.orphans.is_empty() {
            add_group(self.lv_devices, ORPHAN_GROUP, "Adapter no longer present");
            for (di, d) in snap.orphans.iter().enumerate() {
                add_device(d, ORPHAN, di, None, ORPHAN_GROUP);
            }
        }
        if let Some(r) = select_dev {
            select_row(self.lv_devices, r);
        }
        SendMessageW(self.lv_devices, WM_SETREDRAW, 1, 0);
        InvalidateRect(self.lv_devices, null(), 1);

        let status = match active_adapter(&snap) {
            Some(a) => {
                let connected = a.devices.iter().filter(|d| d.connected()).count();
                let ver = a.version_text();
                let ver = if ver.is_empty() { String::new() } else { format!(" (Bluetooth {ver})") };
                format!("Windows is using {}{ver} \u{00B7} {connected} device(s) connected.", a.node.name)
            }
            None if snap.adapters.is_empty() => "No Bluetooth adapters found.".to_owned(),
            None => "No Bluetooth adapter is in use right now.".to_owned(),
        };
        drop(snap);
        if !self.busy.get() {
            self.set_status(&status);
        }
        self.update_buttons();
    }

    unsafe fn selected_adapter(&self) -> Option<usize> {
        selected_param(self.lv_adapters)
    }

    unsafe fn selected_device(&self) -> Option<DevSel> {
        selected_param(self.lv_devices).map(|p| (p >> 16, p & 0xFFFF))
    }

    // ---- command availability (shared by buttons and menus) --------------------------------

    unsafe fn enabled(&self, id: u16) -> bool {
        if self.busy.get() && !matches!(id, ID_SETTINGS | ID_DEVMGR | ID_TROUBLESHOOT | ID_SOUND | ID_TIPS | ID_ABOUT) {
            return false;
        }
        let Ok(snap) = self.snap.try_borrow() else { return false };
        let adapter = self.selected_adapter().and_then(|i| snap.adapters.get(i).map(|a| (i, a)));
        let device = self.selected_device().and_then(|s| device_at(&snap, s));
        match id {
            ID_USE => adapter.is_some_and(|(i, a)| {
                let others_on =
                    snap.adapters.iter().enumerate().any(|(j, o)| j != i && o.node.present && !o.node.disabled);
                !(a.active && !others_on)
            }),
            ID_RESTART => active_adapter(&snap).is_some(),
            ID_POWER => adapter.is_some(),
            ID_ALL_ON => snap.adapters.iter().any(|a| a.node.disabled),
            ID_FORGET_ADAPTER => adapter.is_some_and(|(_, a)| !a.node.present),
            ID_ADAPTER_DETAILS => adapter.is_some(),
            ID_FORGET_DEVICE | ID_DEVICE_DETAILS => device.is_some(),
            ID_MIC => device.is_some_and(|d| d.hfp.is_some()),
            ID_CLEANUP => !cleanup_candidates(&snap).is_empty(),
            _ => true,
        }
    }

    unsafe fn checked(&self, id: u16) -> bool {
        let Ok(snap) = self.snap.try_borrow() else { return false };
        match id {
            ID_ABSVOL => !snap.absolute_volume_disabled,
            ID_POWER => self.selected_adapter().and_then(|i| snap.adapters.get(i)).is_some_and(|a| a.power_saving_on()),
            ID_MIC => self
                .selected_device()
                .and_then(|s| device_at(&snap, s))
                .and_then(|d| d.hfp.as_ref())
                .is_some_and(|h| !h.disabled),
            _ => false,
        }
    }

    unsafe fn update_buttons(&self) {
        for (h, id) in [
            (self.btn_use, ID_USE),
            (self.btn_restart, ID_RESTART),
            (self.btn_adapter_details, ID_ADAPTER_DETAILS),
            (self.btn_forget, ID_FORGET_DEVICE),
            (self.btn_mic, ID_MIC),
            (self.btn_cleanup, ID_CLEANUP),
            (self.btn_device_details, ID_DEVICE_DETAILS),
        ] {
            EnableWindow(h, self.enabled(id) as BOOL);
        }
        let mic_on = self.checked(ID_MIC);
        let has_mic = self.enabled(ID_MIC);
        let text = if has_mic && !mic_on { "Turn on headset mic" } else { "Turn off headset mic" };
        SetWindowTextW(self.btn_mic, wide(text).as_ptr());
    }

    unsafe fn init_menu(&self, menu: HMENU) {
        for i in 0..GetMenuItemCount(menu).max(0) {
            let id = GetMenuItemID(menu, i);
            if id == u32::MAX || id == 0 {
                continue;
            }
            let id = id as u16;
            EnableMenuItem(menu, id as u32, MF_BYCOMMAND | if self.enabled(id) { MF_ENABLED } else { MF_GRAYED });
            CheckMenuItem(menu, id as u32, MF_BYCOMMAND | if self.checked(id) { MF_CHECKED } else { MF_UNCHECKED });
        }
    }

    unsafe fn context_menu(&self, ctl: HWND, lparam: LPARAM) {
        let index = if ctl == self.lv_adapters {
            0
        } else if ctl == self.lv_devices {
            1
        } else {
            return;
        };
        let (mut x, mut y) = ((lparam & 0xFFFF) as i16 as i32, ((lparam >> 16) & 0xFFFF) as i16 as i32);
        if lparam == -1 {
            // Keyboard (Shift+F10 / menu key): open at the list's top-left.
            let mut rc: RECT = zeroed();
            GetWindowRect(ctl, &mut rc);
            (x, y) = (rc.left + self.scale(24), rc.top + self.scale(32));
        }
        let sub = GetSubMenu(self.menu, index);
        TrackPopupMenu(sub, TPM_RIGHTBUTTON, x, y, 0, self.hwnd, null());
    }

    // ---- commands --------------------------------------------------------------------------

    unsafe fn command(&self, id: u16) {
        if !self.enabled(id) {
            return;
        }
        match id {
            ID_USE => self.cmd_use(),
            ID_RESTART => self.cmd_restart(),
            ID_POWER => self.cmd_power(),
            ID_ALL_ON => self.cmd_all_on(),
            ID_FORGET_ADAPTER => self.cmd_forget_adapter(),
            ID_ADAPTER_DETAILS => self.cmd_adapter_details(),
            ID_FORGET_DEVICE => self.cmd_forget_device(),
            ID_MIC => self.cmd_mic(),
            ID_CLEANUP => self.cmd_cleanup(),
            ID_DEVICE_DETAILS => self.cmd_device_details(),
            ID_ABSVOL => self.cmd_absolute_volume(),
            ID_SOUND => shell_open(self.hwnd, "ms-settings:sound"),
            ID_REFRESH => self.start_scan(),
            ID_SETTINGS => shell_open(self.hwnd, "ms-settings:bluetooth"),
            ID_DEVMGR => shell_open(self.hwnd, "devmgmt.msc"),
            ID_TROUBLESHOOT => shell_open(self.hwnd, "ms-settings:troubleshoot"),
            ID_TIPS => {
                self.message(TIPS, MB_OK | MB_ICONINFORMATION);
            }
            ID_ABOUT => {
                let text = format!(
                    "{APP_NAME} {}\n\nPick which Bluetooth adapter Windows uses and fix common Windows Bluetooth problems.\n\n{}",
                    env!("CARGO_PKG_VERSION"),
                    env!("CARGO_PKG_REPOSITORY")
                );
                self.message(&text, MB_OK | MB_ICONINFORMATION);
            }
            _ => {}
        }
    }

    unsafe fn cmd_use(&self) {
        let Some(index) = self.selected_adapter() else { return };
        let (ops, prompt, target_name) = {
            let snap = self.snap.borrow();
            let Some(target) = snap.adapters.get(index) else { return };
            if !target.node.present {
                drop(snap);
                self.message(
                    "That adapter isn't plugged in right now.\n\nPlug it in, give Windows a few seconds to notice it, then try again.",
                    MB_OK | MB_ICONINFORMATION,
                );
                return;
            }
            let others: Vec<&Adapter> = snap
                .adapters
                .iter()
                .enumerate()
                .filter(|(j, o)| *j != index && o.node.present && !o.node.disabled)
                .map(|(_, o)| o)
                .collect();

            let mut ops = Vec::new();
            if target.node.disabled {
                ops.push(Op::Enable { id: target.node.id.clone(), label: target.node.name.clone() });
            }
            for o in &others {
                ops.push(Op::Disable { id: o.node.id.clone(), label: o.node.name.clone() });
            }

            let mut prompt = format!("Switch Bluetooth to \u{201C}{}\u{201D}?\n\n", target.node.name);
            if others.is_empty() {
                prompt.push_str("It will be turned on.\n");
            } else {
                prompt += &format!("This turns off: {}\n", join_names(others.iter().map(|o| o.node.name.as_str())));
            }
            let dropping = join_names(
                others.iter().flat_map(|o| o.devices.iter()).filter(|d| d.connected()).map(|d| d.node.name.as_str()),
            );
            if !dropping.is_empty() {
                prompt += &format!("\nThese will disconnect now: {dropping}\n");
            }
            let known = join_names(target.devices.iter().map(|d| d.node.name.as_str()));
            if known.is_empty() {
                prompt.push_str("\nNothing is paired with it yet, so you'll pair your devices again afterwards.\n");
            } else {
                prompt += &format!("\nAlready paired with it (should reconnect by themselves): {known}\n");
            }
            prompt.push_str("\nIf your keyboard or mouse is Bluetooth it will drop too \u{2014} keep a wired one handy to finish pairing.");
            (ops, prompt, target.node.name.clone())
        };
        if ops.is_empty() {
            self.set_status("Windows is already using that adapter.");
            return;
        }
        if self.confirm(&prompt) {
            self.start_ops(ops, Followup::Switched(target_name));
        }
    }

    unsafe fn cmd_restart(&self) {
        let op = {
            let snap = self.snap.borrow();
            let Some(a) = active_adapter(&snap) else { return };
            Op::RestartBluetooth { id: a.node.id.clone(), label: a.node.name.clone() }
        };
        if self.confirm(
            "Restart Bluetooth?\n\nThis restarts the Bluetooth service and the adapter \u{2014} the usual fix when Bluetooth \
             stops working, the toggle disappears or devices refuse to connect. Everything disconnects and reconnects; \
             it can take up to a minute.",
        ) {
            self.start_ops(vec![op], Followup::Message("Restart done. Devices reconnect in a few seconds.".to_owned()));
        }
    }

    unsafe fn cmd_power(&self) {
        let Some(i) = self.selected_adapter() else { return };
        let (op, allow, present) = {
            let snap = self.snap.borrow();
            let Some(a) = snap.adapters.get(i) else { return };
            let allow = !a.power_saving_on();
            (Op::PowerSaving { id: a.node.id.clone(), label: a.node.name.clone(), allow }, allow, a.node.present)
        };
        let mut text = if allow {
            "Turn power saving back on?\n\nWindows may then switch the adapter off when it's idle (the Windows default)."
                .to_owned()
        } else {
            "Turn power saving off for this adapter?\n\nThis stops Windows from switching the adapter off to save power \u{2014} \
             the classic fix for devices that drop out after a while or don't come back after sleep. It costs a tiny bit \
             of battery on laptops."
                .to_owned()
        };
        if present {
            text.push_str("\n\nThe adapter restarts so this takes effect; devices reconnect in a few seconds.");
        }
        if self.confirm(&text) {
            let done = format!("Power saving is now {} for this adapter.", if allow { "on" } else { "off" });
            self.start_ops(vec![op], Followup::Message(done));
        }
    }

    unsafe fn cmd_all_on(&self) {
        let ops: Vec<Op> = self
            .snap
            .borrow()
            .adapters
            .iter()
            .filter(|a| a.node.disabled)
            .map(|a| Op::Enable { id: a.node.id.clone(), label: a.node.name.clone() })
            .collect();
        self.start_ops(ops, Followup::Message("All adapters are back on. Windows picks which one it uses.".to_owned()));
    }

    unsafe fn cmd_forget_adapter(&self) {
        let Some(i) = self.selected_adapter() else { return };
        let (op, prompt) = {
            let snap = self.snap.borrow();
            let Some(a) = snap.adapters.get(i) else { return };
            (
                Op::Remove { id: a.node.id.clone(), label: a.node.name.clone() },
                format!(
                    "Forget \u{201C}{}\u{201D}?\n\nThis removes the adapter and the {} device entries Windows remembers for it. \
                     If you plug it in again, Windows sets it up from scratch.",
                    a.node.name,
                    a.devices.len()
                ),
            )
        };
        if self.confirm(&prompt) {
            self.start_ops(vec![op], Followup::Message("Adapter forgotten.".to_owned()));
        }
    }

    unsafe fn cmd_forget_device(&self) {
        let Some(sel) = self.selected_device() else { return };
        let (name, address, on_active, ops) = {
            let snap = self.snap.borrow();
            let Some(d) = device_at(&snap, sel) else { return };
            let on_active = sel.0 != ORPHAN && snap.adapters.get(sel.0).is_some_and(|a| a.active);
            (d.node.name.clone(), d.address_raw(), on_active, remove_ops(d))
        };
        if !self.confirm(&format!(
            "Forget \u{201C}{name}\u{201D}?\n\nYou'll have to pair it again to use it. Tip: if it's stuck, put the device \
             itself in pairing mode before pairing again."
        )) {
            return;
        }
        let followup = Followup::Message(format!("{name} has been removed."));
        // Proper unpair first (no admin needed); fall back to removing the device entries.
        self.start_task(followup, move |owner| match address {
            Some(addr) if on_active && control::unpair(addr) => {
                Ok(vec![OpResult { label: format!("Unpair {name}"), ok: true, msg: "Done".to_owned() }])
            }
            _ => control::run(&ops, owner),
        });
    }

    unsafe fn cmd_mic(&self) {
        let Some(sel) = self.selected_device() else { return };
        let (op, prompt, done) = {
            let snap = self.snap.borrow();
            let Some(d) = device_at(&snap, sel) else { return };
            let Some(h) = &d.hfp else { return };
            if h.disabled {
                (
                    Op::Enable { id: h.id.clone(), label: format!("{} headset mic", d.node.name) },
                    format!(
                        "Turn the headset mic back on for \u{201C}{}\u{201D}?\n\nYou can use its microphone again, but audio \
                         drops to low-quality mono whenever an app opens the mic.",
                        d.node.name
                    ),
                    format!("Headset mic is on again for {}. Reconnect it if it doesn't show up as a mic.", d.node.name),
                )
            } else {
                (
                    Op::Disable { id: h.id.clone(), label: format!("{} headset mic", d.node.name) },
                    format!(
                        "Turn off the headset mic for \u{201C}{}\u{201D}?\n\nWindows switches Bluetooth headphones to a \
                         low-quality mono \u{201C}hands-free\u{201D} mode whenever an app (Discord, Teams, games\u{2026}) \
                         opens their mic. With the headset mic off they always stay in full-quality stereo \u{2014} use \
                         another microphone instead.",
                        d.node.name
                    ),
                    format!(
                        "Headset mic is off for {}. If audio still sounds bad, turn the headphones off and on once.",
                        d.node.name
                    ),
                )
            }
        };
        if self.confirm(&prompt) {
            self.start_ops(vec![op], Followup::Message(done));
        }
    }

    unsafe fn cmd_cleanup(&self) {
        let (ops, prompt, count) = {
            let snap = self.snap.borrow();
            let list = cleanup_candidates(&snap);
            let names = join_names(list.iter().map(|d| d.node.name.as_str()));
            let ops: Vec<Op> = list.iter().flat_map(|d| remove_ops(d)).collect();
            (
                ops,
                format!(
                    "Remove {} leftover device entr{} that belong to adapters that are no longer plugged in?\n\n{names}\n\n\
                     This clears duplicates and \u{201C}ghost\u{201D} devices. Nothing on your current adapter is touched. \
                     If you plug an old adapter back in, Windows may bring its devices back.",
                    list.len(),
                    if list.len() == 1 { "y" } else { "ies" }
                ),
                list.len(),
            )
        };
        if self.confirm(&prompt) {
            self.start_ops(ops, Followup::Message(format!("Cleaned up {count} old device entries.")));
        }
    }

    unsafe fn cmd_absolute_volume(&self) {
        let (enable, restart) = {
            let snap = self.snap.borrow();
            let restart = active_adapter(&snap)
                .map(|a| Op::RestartBluetooth { id: a.node.id.clone(), label: a.node.name.clone() });
            (snap.absolute_volume_disabled, restart)
        };
        let prompt = if enable {
            "Turn absolute volume back on?\n\nWindows and your headphones will share one volume level (the Windows default)."
        } else {
            "Turn absolute volume off?\n\nWindows and your headphones will keep separate volume levels. This fixes volume \
             that's far too loud or quiet, jumps around, or can't be controlled from the headphones."
        };
        let note = if restart.is_some() { "\n\nBluetooth restarts so this takes effect right away." } else { "" };
        if !self.confirm(&format!("{prompt}{note}")) {
            return;
        }
        let mut ops = vec![Op::AbsoluteVolume { enabled: enable }];
        ops.extend(restart);
        let done = format!(
            "Absolute volume is now {}. If a device still behaves the old way, restart the PC once.",
            if enable { "on" } else { "off" }
        );
        self.start_ops(ops, Followup::Message(done));
    }

    unsafe fn cmd_adapter_details(&self) {
        let Some(i) = self.selected_adapter() else { return };
        let text = {
            let snap = self.snap.borrow();
            let Some(a) = snap.adapters.get(i) else { return };
            let version = match a.lmp_version {
                Some(v) => format!("{} (LMP {v})", devices::bluetooth_version(v)),
                None => "unknown".to_owned(),
            };
            let driver_date = a.driver_date.map(format_date).unwrap_or_else(|| "unknown".to_owned());
            format!(
                "{}\n\nState:\t\t{}\nPower saving:\t{}\nMade by:\t\t{}\nBluetooth version:\t{}\nAddress:\t\t{}\n\
                 Driver:\t\t{} {}\nDriver date:\t{}\nPaired devices:\t{}\n\nDevice ID:\n{}\n\nTip: Ctrl+C copies this text.",
                a.node.name,
                a.state_text(),
                match a.power_saving {
                    Some(true) => "on",
                    Some(false) => "off",
                    None => "on (Windows default)",
                },
                if a.node.manufacturer.is_empty() { "unknown" } else { &a.node.manufacturer },
                version,
                a.address.map(format_address).unwrap_or_else(|| "unknown".to_owned()),
                a.driver_provider.as_deref().unwrap_or(""),
                a.driver_version.as_deref().unwrap_or("unknown"),
                driver_date,
                a.devices.len(),
                a.node.id
            )
        };
        self.message(&text, MB_OK | MB_ICONINFORMATION);
    }

    unsafe fn cmd_device_details(&self) {
        let Some(sel) = self.selected_device() else { return };
        let text = {
            let snap = self.snap.borrow();
            let Some(d) = device_at(&snap, sel) else { return };
            let adapter = snap.adapters.get(sel.0).map(|a| a.node.name.as_str()).unwrap_or("no longer present");
            let now = now_filetime();
            let last = match (d.connected(), d.last_connected) {
                (true, _) => "connected now".to_owned(),
                (false, Some(t)) => format!("{} ({})", format_ago(t, now), format_date(t)),
                (false, None) => "unknown".to_owned(),
            };
            let mic = match &d.hfp {
                Some(h) if h.disabled => "off (always full-quality stereo)",
                Some(_) => "on",
                None => "not available",
            };
            format!(
                "{}\n\nType:\t\t{}\nConnection:\t{}\nAddress:\t\t{}\nBattery:\t\t{}\nLast connected:\t{}\nHeadset mic:\t{}\n\
                 Paired with:\t{}\n\nDevice ID:\n{}\n\nTip: Ctrl+C copies this text.",
                d.node.name,
                d.kind(),
                if d.is_le() { "Bluetooth Low Energy" } else { "Bluetooth Classic" },
                d.address(),
                d.battery.map(|b| format!("{b}%")).unwrap_or_else(|| "not reported".to_owned()),
                last,
                mic,
                adapter,
                d.node.id
            )
        };
        self.message(&text, MB_OK | MB_ICONINFORMATION);
    }

    // ---- background work --------------------------------------------------------------------

    unsafe fn start_ops(&self, ops: Vec<Op>, followup: Followup) {
        if ops.is_empty() {
            return;
        }
        self.start_task(followup, move |owner| control::run(&ops, owner));
    }

    unsafe fn start_task<F>(&self, followup: Followup, work: F)
    where
        F: FnOnce(isize) -> Result<Vec<OpResult>, String> + Send + 'static,
    {
        self.busy.set(true);
        self.update_buttons();
        self.set_status(if control::is_elevated() { "Working\u{2026}" } else { "Working\u{2026} (approve the admin prompt if asked)" });
        let hwnd = self.hwnd as isize;
        std::thread::spawn(move || {
            let result = work(hwnd);
            *ACTION_RESULT.lock().unwrap() = Some(Outcome { followup, result });
            unsafe { PostMessageW(hwnd as HWND, WM_APP_DONE, 0, 0) };
        });
    }

    unsafe fn on_done(&self) {
        let Some(outcome) = ACTION_RESULT.lock().unwrap().take() else { return };
        self.busy.set(false);
        self.update_buttons();
        SetTimer(self.hwnd, TIMER_RESCAN, 1200, None);

        let results = match outcome.result {
            Err(e) => {
                self.set_status(&e);
                return;
            }
            Ok(r) => r,
        };
        let failed: Vec<&OpResult> = results.iter().filter(|r| !r.ok).collect();
        if !failed.is_empty() || results.is_empty() {
            let mut text = String::from("Some changes didn't go through:\n\n");
            for r in &failed {
                text += &format!("\u{2022} {}: {}\n", r.label, r.msg);
            }
            if results.is_empty() {
                text.push_str("\u{2022} Nothing was applied.\n");
            }
            self.set_status("Some changes failed.");
            self.message(&text, MB_OK | MB_ICONWARNING);
            return;
        }

        let notes: Vec<&str> =
            results.iter().map(|r| r.msg.as_str()).filter(|m| !matches!(*m, "Done" | "Saved")).collect();
        let notes = if notes.is_empty() { String::new() } else { format!("\n\n{}", notes.join("\n")) };
        match outcome.followup {
            Followup::Switched(target) => {
                self.set_status(&format!("Switched to {target}."));
                let text = format!(
                    "Done \u{2014} Windows is now using \u{201C}{target}\u{201D}.{notes}\n\nOpen Bluetooth settings to pair anything that isn't listed under it yet?"
                );
                if self.message(&text, MB_YESNO | MB_ICONINFORMATION) == IDYES {
                    shell_open(self.hwnd, "ms-settings:bluetooth");
                }
            }
            Followup::Message(text) => {
                self.set_status(text.lines().next().unwrap_or(""));
                self.message(&format!("{text}{notes}"), MB_OK | MB_ICONINFORMATION);
            }
        }
    }

    unsafe fn custom_draw(&self, lparam: LPARAM, adapters: bool) -> LRESULT {
        let cd = &mut *(lparam as *mut NMLVCUSTOMDRAW);
        match cd.nmcd.dwDrawStage {
            CDDS_PREPAINT => CDRF_NOTIFYITEMDRAW,
            CDDS_ITEMPREPAINT => {
                let Ok(snap) = self.snap.try_borrow() else { return CDRF_DODEFAULT };
                let param = cd.nmcd.lItemlParam as usize;
                let green = rgb(16, 124, 16);
                let gray = GetSysColor(COLOR_GRAYTEXT);
                let mut font = self.font.get();
                if adapters {
                    if let Some(a) = snap.adapters.get(param) {
                        if a.active {
                            cd.clrText = green;
                            font = self.bold.get();
                        } else if !a.node.present || a.node.disabled {
                            cd.clrText = gray;
                        }
                    }
                } else {
                    let (ai, di) = (param >> 16, param & 0xFFFF);
                    match snap.adapters.get(ai) {
                        Some(a) => {
                            if let Some(d) = a.devices.get(di) {
                                if d.connected() {
                                    cd.clrText = green;
                                } else if !a.active {
                                    cd.clrText = gray;
                                }
                            }
                        }
                        None => cd.clrText = gray,
                    }
                }
                SelectObject(cd.nmcd.hdc, font);
                CDRF_NEWFONT
            }
            _ => CDRF_DODEFAULT,
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app() else { return DefWindowProcW(hwnd, msg, wparam, lparam) };
    match msg {
        WM_SIZE => {
            app.layout();
            0
        }
        WM_GETMINMAXINFO => {
            let mmi = &mut *(lparam as *mut MINMAXINFO);
            mmi.ptMinTrackSize = POINT { x: app.scale(760), y: app.scale(600) };
            0
        }
        WM_DPICHANGED_ => {
            app.dpi.set(((wparam >> 16) & 0xFFFF) as u32);
            app.apply_fonts();
            let r = &*(lparam as *const RECT);
            SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            app.layout();
            0
        }
        WM_COMMAND => {
            app.command((wparam & 0xFFFF) as u16);
            0
        }
        WM_INITMENUPOPUP => {
            app.init_menu(wparam as HMENU);
            0
        }
        WM_CONTEXTMENU => {
            app.context_menu(wparam as HWND, lparam);
            0
        }
        WM_NOTIFY => {
            let hdr = &*(lparam as *const NMHDR);
            let from_adapters = hdr.hwndFrom == app.lv_adapters;
            let from_devices = hdr.hwndFrom == app.lv_devices;
            if !from_adapters && !from_devices {
                return 0;
            }
            match hdr.code {
                NM_CUSTOMDRAW => return app.custom_draw(lparam, from_adapters),
                LVN_ITEMCHANGED => app.update_buttons(),
                NM_DBLCLK | NM_RETURN => {
                    let id = if from_adapters { ID_USE } else { ID_DEVICE_DETAILS };
                    PostMessageW(hwnd, WM_COMMAND, id as usize, 0);
                }
                LVN_KEYDOWN => {
                    let key = { (*(lparam as *const NMLVKEYDOWN)).wVKey };
                    if key == VK_DELETE_ && from_devices {
                        PostMessageW(hwnd, WM_COMMAND, ID_FORGET_DEVICE as usize, 0);
                    }
                }
                _ => {}
            }
            0
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            let hdc = wparam as HDC;
            let ctl = lparam as HWND;
            SetBkColor(hdc, GetSysColor(COLOR_WINDOW));
            let color = if ctl == app.status { GetSysColor(COLOR_GRAYTEXT) } else { GetSysColor(COLOR_WINDOWTEXT) };
            SetTextColor(hdc, color);
            GetSysColorBrush(COLOR_WINDOW) as LRESULT
        }
        WM_DEVICECHANGE => {
            // Hardware changes come in bursts; debounce into one rescan.
            SetTimer(hwnd, TIMER_RESCAN, 700, None);
            1
        }
        WM_TIMER if wparam == TIMER_RESCAN => {
            KillTimer(hwnd, TIMER_RESCAN);
            app.start_scan();
            0
        }
        WM_APP_SCANNED => {
            app.on_scanned();
            0
        }
        WM_APP_DONE => {
            app.on_done();
            0
        }
        WM_ACTIVATE => {
            // Keep keyboard focus sensible after dialogs.
            if GetFocus().is_null() {
                windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(app.lv_adapters);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
