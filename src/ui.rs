//! Native Win32 window: two list views (adapters, paired devices grouped by adapter),
//! a row of buttons, and live refresh on WM_DEVICECHANGE. Scans and pnputil calls run on
//! worker threads and post results back, so the window never blocks.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem::{size_of, transmute, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, DrawTextW, GetDC, InvalidateRect, GetMonitorInfoW, GetSysColor, GetSysColorBrush,
    MonitorFromWindow, ReleaseDC, SelectObject, SetBkColor, SetTextColor, COLOR_GRAYTEXT, COLOR_WINDOW,
    COLOR_WINDOWTEXT, DT_CALCRECT, DT_NOPREFIX, DT_SINGLELINE, DT_WORDBREAK, HDC, HFONT, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::UI::Controls::{
    SetWindowTheme, INITCOMMONCONTROLSEX, LVCOLUMNW, LVGROUP, LVITEMW, NMHDR, NMLVCUSTOMDRAW,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForWindow, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext, SystemParametersInfoForDpi,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::Shell::{ExtractIconExW, ShellExecuteW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::control::{self, Op, OpResult};
use crate::devices::{self, Snapshot};

// ---- ids, messages ---------------------------------------------------------------------------

const ID_USE: i32 = 101;
const ID_ALL_ON: i32 = 102;
const ID_REFRESH: i32 = 103;
const ID_SETTINGS: i32 = 104;
const ID_LV_ADAPTERS: i32 = 201;
const ID_LV_DEVICES: i32 = 202;

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
const NM_DBLCLK: u32 = (-3i32) as u32;
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

const ORPHAN_GROUP: i32 = 9999;

const INTRO: &str = "Windows only runs one Bluetooth adapter at a time and gives you no way to choose. \
Select the adapter you want and click \u{201C}Use this adapter\u{201D} \u{2014} the others get switched off, \
so Windows has to use yours. Pairings belong to an adapter, so devices not listed under it need pairing again.";

static SCAN_RESULT: Mutex<Option<Snapshot>> = Mutex::new(None);
static ACTION_RESULT: Mutex<Option<Outcome>> = Mutex::new(None);
static APP: AtomicPtr<App> = AtomicPtr::new(null_mut());

struct Outcome {
    target: Option<String>,
    result: Result<Vec<OpResult>, String>,
}

fn app() -> Option<&'static App> {
    unsafe { APP.load(Ordering::Relaxed).as_ref() }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

// ---- app state --------------------------------------------------------------------------------

struct App {
    hwnd: HWND,
    intro: HWND,
    lbl_adapters: HWND,
    lv_adapters: HWND,
    btn_use: HWND,
    btn_all_on: HWND,
    btn_refresh: HWND,
    btn_settings: HWND,
    lbl_devices: HWND,
    lv_devices: HWND,
    tip: HWND,
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
        let class = wide("BtAdapterSwitcher");
        let mut wc: WNDCLASSEXW = zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.style = CS_HREDRAW | CS_VREDRAW;
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        wc.hbrBackground = GetSysColorBrush(COLOR_WINDOW);
        wc.lpszClassName = class.as_ptr();
        RegisterClassExW(&wc);

        let title = wide("Bluetooth Adapter Switcher");
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            900,
            680,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            return;
        }
        set_icon(hwnd);

        let static_style = SS_NOPREFIX;
        let button_style = WS_TABSTOP | BS_PUSHBUTTON as u32;
        let list_style = WS_TABSTOP | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS | LVS_NOSORTHEADER;

        let state = Box::new(App {
            hwnd,
            intro: child(hwnd, "STATIC", INTRO, static_style, 0, 0),
            lbl_adapters: child(hwnd, "STATIC", "Bluetooth adapters", static_style, 0, 0),
            lv_adapters: child(hwnd, "SysListView32", "", list_style, WS_EX_CLIENTEDGE, ID_LV_ADAPTERS),
            btn_use: child(hwnd, "BUTTON", "Use this adapter", button_style, 0, ID_USE),
            btn_all_on: child(hwnd, "BUTTON", "Turn all adapters back on", button_style, 0, ID_ALL_ON),
            btn_refresh: child(hwnd, "BUTTON", "Refresh", button_style, 0, ID_REFRESH),
            btn_settings: child(hwnd, "BUTTON", "Bluetooth settings\u{2026}", button_style, 0, ID_SETTINGS),
            lbl_devices: child(hwnd, "STATIC", "Paired devices, by adapter", static_style, 0, 0),
            lv_devices: child(hwnd, "SysListView32", "", list_style, WS_EX_CLIENTEDGE, ID_LV_DEVICES),
            tip: child(hwnd, "STATIC", "", static_style, 0, 0),
            status: child(hwnd, "STATIC", "Scanning\u{2026}", static_style | SS_ENDELLIPSIS, 0, 0),
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

        init_list(app.lv_adapters, &["Adapter", "Made by", "State", "Paired"], false);
        init_list(app.lv_devices, &["Device", "Address", "Type", "Status"], true);
        ShowWindow(app.tip, SW_HIDE);
        app.apply_fonts();
        app.update_buttons();
        app.place_window();
        ShowWindow(hwnd, SW_SHOWDEFAULT);
        app.start_scan();

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
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

unsafe fn child(parent: HWND, class: &str, text: &str, style: u32, ex: u32, id: i32) -> HWND {
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

fn join_names<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for n in names {
        if !seen.contains(&n) {
            seen.push(n);
        }
    }
    seen.join(", ")
}

impl App {
    fn scale(&self, v: i32) -> i32 {
        (v * self.dpi.get() as i32 + 48) / 96
    }

    fn children(&self) -> [HWND; 11] {
        [
            self.intro,
            self.lbl_adapters,
            self.lv_adapters,
            self.btn_use,
            self.btn_all_on,
            self.btn_refresh,
            self.btn_settings,
            self.lbl_devices,
            self.lv_devices,
            self.tip,
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
        let (w, h) = (self.scale(900), self.scale(680));
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

    unsafe fn measure(&self, ctl: HWND, width: i32, font: HFONT, single_line: bool) -> (i32, i32) {
        let len = GetWindowTextLengthW(ctl);
        let mut buf = vec![0u16; len as usize + 1];
        GetWindowTextW(ctl, buf.as_mut_ptr(), len + 1);
        let sample = wide("Ag");
        let (text, n) = if len == 0 { (sample.as_ptr(), 2) } else { (buf.as_ptr(), len) };
        let hdc = GetDC(ctl);
        let old = SelectObject(hdc, font);
        let mut rc = RECT { left: 0, top: 0, right: width, bottom: 0 };
        let flags = DT_CALCRECT | DT_NOPREFIX | if single_line { DT_SINGLELINE } else { DT_WORDBREAK };
        DrawTextW(hdc, text, n, &mut rc, flags);
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

        let mut y = m;
        let (_, intro_h) = self.measure(self.intro, w, font, false);
        mv(self.intro, m, y, w, intro_h);
        y += intro_h + s(14);

        let (_, head_h) = self.measure(self.lbl_adapters, w, bold, true);
        mv(self.lbl_adapters, m, y, w, head_h);
        y += head_h + s(6);

        // Bottom-up: status line, then the optional tip.
        let (_, line_h) = self.measure(self.status, w, font, true);
        let mut bottom = rc.bottom - m;
        bottom -= line_h;
        mv(self.status, m, bottom, w, line_h);
        bottom -= s(10);
        if IsWindowVisible(self.tip) != 0 {
            let (_, tip_h) = self.measure(self.tip, w, font, false);
            bottom -= tip_h;
            mv(self.tip, m, bottom, w, tip_h);
            bottom -= s(12);
        }

        let btn_h = s(32);
        let fixed = s(10) + btn_h + s(18) + head_h + s(6);
        let avail = (bottom - y - fixed).max(s(160));
        let lv1 = ((avail as f32 * 0.36) as i32).max(s(96));
        let lv2 = (avail - lv1).max(s(80));

        mv(self.lv_adapters, m, y, w, lv1);
        y += lv1 + s(10);

        let btn_w = |h: HWND| (self.measure(h, 1000, font, true).0 + s(32)).max(s(96));
        let mut x = m;
        for b in [self.btn_use, self.btn_all_on, self.btn_refresh] {
            let bw = btn_w(b);
            mv(b, x, y, bw, btn_h);
            x += bw + s(8);
        }
        let sw = btn_w(self.btn_settings);
        mv(self.btn_settings, (m + w - sw).max(x), y, sw, btn_h);
        y += btn_h + s(18);

        mv(self.lbl_devices, m, y, w, head_h);
        y += head_h + s(6);
        mv(self.lv_devices, m, y, w, lv2);

        let inner = w - GetSystemMetricsForDpi(SM_CXVSCROLL, self.dpi.get()) - s(6);
        self.set_columns(self.lv_adapters, inner, &[0.42, 0.25, 0.23, 0.10]);
        self.set_columns(self.lv_devices, inner, &[0.38, 0.22, 0.14, 0.26]);
    }

    unsafe fn set_columns(&self, lv: HWND, width: i32, shares: &[f32]) {
        for (i, share) in shares.iter().enumerate() {
            SendMessageW(lv, LVM_SETCOLUMNWIDTH, i, (width as f32 * share) as isize);
        }
    }

    fn set_status(&self, text: &str) {
        unsafe { SetWindowTextW(self.status, wide(text).as_ptr()) };
    }

    // ---- scanning -------------------------------------------------------------------------

    fn start_scan(&self) {
        if self.scanning.replace(true) {
            self.rescan.set(true);
            return;
        }
        let hwnd = self.hwnd as isize;
        std::thread::spawn(move || {
            let snap = devices::scan(true);
            *SCAN_RESULT.lock().unwrap() = Some(snap);
            unsafe { PostMessageW(hwnd as HWND, WM_APP_SCANNED, 0, 0) };
        });
    }

    unsafe fn on_scanned(&self) {
        let Some(fresh) = SCAN_RESULT.lock().unwrap().take() else { return };
        let selected_id = self.selected_adapter_id();
        match self.snap.try_borrow_mut() {
            Ok(mut snap) => *snap = fresh,
            Err(_) => {
                // Mid-paint; try again on the next message.
                *SCAN_RESULT.lock().unwrap() = Some(fresh);
                PostMessageW(self.hwnd, WM_APP_SCANNED, 0, 0);
                return;
            }
        }
        self.scanning.set(false);
        self.populate(selected_id.as_deref());
        if self.rescan.replace(false) {
            self.start_scan();
        }
    }

    unsafe fn populate(&self, selected_id: Option<&str>) {
        let snap = self.snap.borrow();

        SendMessageW(self.lv_adapters, WM_SETREDRAW, 0, 0);
        SendMessageW(self.lv_adapters, LVM_DELETEALLITEMS, 0, 0);
        for (i, a) in snap.adapters.iter().enumerate() {
            let count = a.devices.len().to_string();
            let cells = [a.node.name.as_str(), a.node.manufacturer.as_str(), a.state_text(), count.as_str()];
            add_row(self.lv_adapters, i as i32, &cells, i as isize, None);
        }
        let select = snap
            .adapters
            .iter()
            .position(|a| Some(a.node.id.as_str()) == selected_id)
            .or_else(|| snap.adapters.iter().position(|a| a.active));
        if let Some(row) = select {
            let mut st: LVITEMW = zeroed();
            st.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
            st.state = LVIS_SELECTED | LVIS_FOCUSED;
            SendMessageW(self.lv_adapters, LVM_SETITEMSTATE, row, &st as *const _ as isize);
        }
        SendMessageW(self.lv_adapters, WM_SETREDRAW, 1, 0);
        InvalidateRect(self.lv_adapters, null(), 1);

        SendMessageW(self.lv_devices, WM_SETREDRAW, 0, 0);
        SendMessageW(self.lv_devices, LVM_DELETEALLITEMS, 0, 0);
        SendMessageW(self.lv_devices, LVM_REMOVEALLGROUPS, 0, 0);
        let mut row = 0;
        for (ai, a) in snap.adapters.iter().enumerate() {
            if a.devices.is_empty() {
                continue;
            }
            add_group(self.lv_devices, ai as i32, &format!("{}  \u{00B7}  {}", a.node.name, a.state_text()));
            for (di, d) in a.devices.iter().enumerate() {
                let status = if d.connected() {
                    "Connected"
                } else if a.active {
                    "Paired \u{00B7} not connected"
                } else {
                    "Paired \u{00B7} adapter not in use"
                };
                let addr = d.address();
                let cells = [d.node.name.as_str(), addr.as_str(), d.kind(), status];
                add_row(self.lv_devices, row, &cells, ((ai << 16) | di) as isize, Some(ai as i32));
                row += 1;
            }
        }
        if !snap.orphans.is_empty() {
            add_group(self.lv_devices, ORPHAN_GROUP, "Unknown adapter");
            for (di, d) in snap.orphans.iter().enumerate() {
                let addr = d.address();
                let cells = [d.node.name.as_str(), addr.as_str(), d.kind(), "Paired"];
                add_row(self.lv_devices, row, &cells, ((0xFFFF << 16) | di) as isize, Some(ORPHAN_GROUP));
                row += 1;
            }
        }
        SendMessageW(self.lv_devices, WM_SETREDRAW, 1, 0);
        InvalidateRect(self.lv_devices, null(), 1);

        match &snap.receiver {
            Some(name) => {
                let tip = format!(
                    "Heads-up: your mouse uses a \u{201C}{name}\u{201D}. That isn't Bluetooth, so it can't be moved to another \
                     adapter \u{2014} but it shares the 2.4 GHz band and can drown Bluetooth out. If your keyboard or headset \
                     lag, put the receiver and the Bluetooth adapter on separate short USB extension cables, a hand-width or \
                     more apart, and away from USB 3 ports."
                );
                SetWindowTextW(self.tip, wide(&tip).as_ptr());
                ShowWindow(self.tip, SW_SHOWNA);
            }
            None => {
                ShowWindow(self.tip, SW_HIDE);
            }
        }

        let status = match snap.adapters.iter().find(|a| a.active) {
            Some(a) => {
                let connected = a.devices.iter().filter(|d| d.connected()).count();
                format!("Windows is using {} \u{00B7} {} connected device(s).", a.node.name, connected)
            }
            None if snap.adapters.is_empty() => "No Bluetooth adapters found.".to_owned(),
            None => "No Bluetooth adapter is in use right now.".to_owned(),
        };
        drop(snap);
        if !self.busy.get() {
            self.set_status(&status);
        }
        self.layout();
        self.update_buttons();
    }

    unsafe fn selected_row_param(&self) -> Option<usize> {
        let row = SendMessageW(self.lv_adapters, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
        if row < 0 {
            return None;
        }
        let mut item: LVITEMW = zeroed();
        item.mask = LVIF_PARAM;
        item.iItem = row as i32;
        SendMessageW(self.lv_adapters, LVM_GETITEMW, 0, &mut item as *mut _ as isize);
        Some(item.lParam as usize)
    }

    unsafe fn selected_adapter_id(&self) -> Option<String> {
        let i = self.selected_row_param()?;
        let snap = self.snap.try_borrow().ok()?;
        snap.adapters.get(i).map(|a| a.node.id.clone())
    }

    unsafe fn update_buttons(&self) {
        let busy = self.busy.get();
        let Ok(snap) = self.snap.try_borrow() else { return };
        let selected = self.selected_row_param().and_then(|i| snap.adapters.get(i).map(|a| (i, a)));
        let can_use = !busy
            && selected.is_some_and(|(i, a)| {
                let others_on = snap.adapters.iter().enumerate().any(|(j, o)| j != i && o.node.present && !o.node.disabled);
                !(a.active && !others_on)
            });
        let any_off = snap.adapters.iter().any(|a| a.node.disabled);
        EnableWindow(self.btn_use, can_use as BOOL);
        EnableWindow(self.btn_all_on, (!busy && any_off) as BOOL);
        EnableWindow(self.btn_refresh, (!busy) as BOOL);
    }

    // ---- actions ---------------------------------------------------------------------------

    unsafe fn message(&self, text: &str, flags: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
        MessageBoxW(self.hwnd, wide(text).as_ptr(), wide("Bluetooth Adapter Switcher").as_ptr(), flags)
    }

    unsafe fn on_use(&self) {
        if self.busy.get() {
            return;
        }
        let Some(index) = self.selected_row_param() else { return };

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
            let others: Vec<_> = snap
                .adapters
                .iter()
                .enumerate()
                .filter(|(j, o)| *j != index && o.node.present && !o.node.disabled)
                .map(|(_, o)| o)
                .collect();

            let mut ops = Vec::new();
            if target.node.disabled {
                ops.push(Op { enable: true, id: target.node.id.clone(), label: target.node.name.clone() });
            }
            for o in &others {
                ops.push(Op { enable: false, id: o.node.id.clone(), label: o.node.name.clone() });
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
            prompt.push_str(
                "\nIf your keyboard is Bluetooth it will drop too \u{2014} use the mouse to finish pairing in Bluetooth settings.",
            );
            (ops, prompt, target.node.name.clone())
        };

        if ops.is_empty() {
            self.set_status("Windows is already using that adapter.");
            return;
        }
        if self.message(&prompt, MB_YESNO | MB_ICONQUESTION) == IDYES {
            self.start_action(ops, Some(target_name));
        }
    }

    unsafe fn on_all_on(&self) {
        if self.busy.get() {
            return;
        }
        let ops: Vec<Op> = self
            .snap
            .borrow()
            .adapters
            .iter()
            .filter(|a| a.node.disabled)
            .map(|a| Op { enable: true, id: a.node.id.clone(), label: a.node.name.clone() })
            .collect();
        self.start_action(ops, None);
    }

    unsafe fn start_action(&self, ops: Vec<Op>, target: Option<String>) {
        if ops.is_empty() {
            return;
        }
        self.busy.set(true);
        self.update_buttons();
        self.set_status(if control::is_elevated() { "Applying\u{2026}" } else { "Waiting for the admin prompt\u{2026}" });
        let hwnd = self.hwnd as isize;
        std::thread::spawn(move || {
            let result = control::run(&ops, hwnd);
            *ACTION_RESULT.lock().unwrap() = Some(Outcome { target, result });
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
        let restart = results.iter().any(|r| r.msg.contains("restart"));

        if !failed.is_empty() || results.is_empty() {
            let mut text = String::from("Some changes didn't go through:\n\n");
            for r in &failed {
                text += &format!("\u{2022} {} {}: {}\n", if r.op.enable { "Turn on" } else { "Turn off" }, r.op.label, r.msg);
            }
            if results.is_empty() {
                text.push_str("\u{2022} Nothing was applied.\n");
            }
            self.set_status("Some changes failed.");
            self.message(&text, MB_OK | MB_ICONWARNING);
        } else if let Some(target) = outcome.target {
            self.set_status(&format!("Switched to {target}."));
            let note = if restart { "\n\nWindows says a restart is needed to finish." } else { "" };
            let text = format!(
                "Done \u{2014} Windows is now using \u{201C}{target}\u{201D}.{note}\n\nOpen Bluetooth settings to pair anything that isn't listed under it yet?"
            );
            if self.message(&text, MB_YESNO | MB_ICONINFORMATION) == IDYES {
                open_bluetooth_settings(self.hwnd);
            }
        } else {
            self.set_status("All adapters are back on. Windows picks which one it uses.");
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
                    if let Some(a) = snap.adapters.get(ai) {
                        if let Some(d) = a.devices.get(di) {
                            if d.connected() {
                                cd.clrText = green;
                            } else if !a.active {
                                cd.clrText = gray;
                            }
                        }
                    } else {
                        cd.clrText = gray;
                    }
                }
                SelectObject(cd.nmcd.hdc, font);
                CDRF_NEWFONT
            }
            _ => CDRF_DODEFAULT,
        }
    }
}

unsafe fn open_bluetooth_settings(hwnd: HWND) {
    ShellExecuteW(hwnd, wide("open").as_ptr(), wide("ms-settings:bluetooth").as_ptr(), null(), null(), SW_SHOWNORMAL);
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
            mmi.ptMinTrackSize.x = app.scale(640);
            mmi.ptMinTrackSize.y = app.scale(520);
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
            match (wparam & 0xFFFF) as i32 {
                ID_USE => app.on_use(),
                ID_ALL_ON => app.on_all_on(),
                ID_REFRESH => app.start_scan(),
                ID_SETTINGS => open_bluetooth_settings(hwnd),
                _ => {}
            }
            0
        }
        WM_NOTIFY => {
            let hdr = &*(lparam as *const NMHDR);
            if hdr.hwndFrom == app.lv_adapters {
                match hdr.code {
                    LVN_ITEMCHANGED => app.update_buttons(),
                    NM_DBLCLK => {
                        PostMessageW(hwnd, WM_COMMAND, ID_USE as usize, 0);
                    }
                    NM_CUSTOMDRAW => return app.custom_draw(lparam, true),
                    _ => {}
                }
            } else if hdr.hwndFrom == app.lv_devices && hdr.code == NM_CUSTOMDRAW {
                return app.custom_draw(lparam, false);
            }
            0
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            let hdc = wparam as HDC;
            let ctl = lparam as HWND;
            SetBkColor(hdc, GetSysColor(COLOR_WINDOW));
            let color = if ctl == app.status {
                GetSysColor(COLOR_GRAYTEXT)
            } else if ctl == app.tip {
                rgb(157, 93, 0)
            } else {
                GetSysColor(COLOR_WINDOWTEXT)
            };
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
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
