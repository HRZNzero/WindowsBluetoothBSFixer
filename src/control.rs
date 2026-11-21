//! Everything that changes the system. Operations that need admin rights are written to a temp
//! file and run by an elevated copy of this exe (`--apply <ops> <out>`) after one UAC prompt.
//! The elevated copy re-checks every target is a Bluetooth device before touching it.

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Devices::Bluetooth::{BluetoothRemoveDevice, BLUETOOTH_ADDRESS};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, HWND};
use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, REG_DWORD,
};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiDestroyDeviceInfoList, SetupDiGetDeviceInstallParamsW,
    SetupDiOpenDeviceInfoW, SetupDiSetClassInstallParamsW, SP_CLASSINSTALL_HEADER, SP_DEVINFO_DATA,
    SP_DEVINSTALL_PARAMS_W, SP_PROPCHANGE_PARAMS,
};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, ControlService, EnumDependentServicesW, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
    StartServiceW, ENUM_SERVICE_STATUSW, SC_HANDLE, SERVICE_STATUS,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use crate::devices::{self, wide};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const ERROR_CANCELLED: u32 = 1223;
const ERROR_SERVICE_ALREADY_RUNNING: u32 = 1056;
const ERROR_SERVICE_NOT_ACTIVE: u32 = 1062;
const SC_MANAGER_CONNECT: u32 = 0x1;
const SERVICE_QUERY_STATUS: u32 = 0x4;
const SERVICE_START: u32 = 0x10;
const SERVICE_STOP: u32 = 0x20;
const SERVICE_CONTROL_STOP: u32 = 1;
const SERVICE_STOPPED: u32 = 1;
const SERVICE_ENUMERATE_DEPENDENTS: u32 = 0x8;
const SERVICE_ACTIVE: u32 = 0x1;
const SERVICE_STATE_ALL: u32 = 0x3;
const DIF_PROPERTYCHANGE: u32 = 0x12;
const DICS_PROPCHANGE: u32 = 3;
const DICS_FLAG_CONFIGSPECIFIC: u32 = 2;
const DI_NEEDRESTART: u32 = 0x80;
const DI_NEEDREBOOT: u32 = 0x100;
const BT_SERVICE: &str = "bthserv";

#[derive(Clone)]
pub enum Op {
    Enable { id: String, label: String },
    Disable { id: String, label: String },
    /// pnputil /remove-device /subtree: drops the device entry and its child entries.
    Remove { id: String, label: String },
    /// Restarts the Bluetooth Support Service and the radio itself.
    RestartBluetooth { id: String, label: String },
    /// Device Manager's "Allow the computer to turn off this device to save power".
    PowerSaving { id: String, label: String, allow: bool },
    AbsoluteVolume { enabled: bool },
}

impl Op {
    pub fn label(&self) -> String {
        match self {
            Op::Enable { label, .. } => format!("Turn on {label}"),
            Op::Disable { label, .. } => format!("Turn off {label}"),
            Op::Remove { label, .. } => format!("Remove {label}"),
            Op::RestartBluetooth { label, .. } => format!("Restart Bluetooth on {label}"),
            Op::PowerSaving { label, allow, .. } => {
                format!("{} power saving on {label}", if *allow { "Allow" } else { "Stop" })
            }
            Op::AbsoluteVolume { enabled } => format!("Turn absolute volume {}", if *enabled { "on" } else { "off" }),
        }
    }

    fn device_id(&self) -> Option<&str> {
        match self {
            Op::Enable { id, .. }
            | Op::Disable { id, .. }
            | Op::Remove { id, .. }
            | Op::RestartBluetooth { id, .. }
            | Op::PowerSaving { id, .. } => Some(id),
            Op::AbsoluteVolume { .. } => None,
        }
    }

    fn to_line(&self) -> String {
        let c = clean;
        match self {
            Op::Enable { id, label } => format!("enable\t{id}\t{}", c(label)),
            Op::Disable { id, label } => format!("disable\t{id}\t{}", c(label)),
            Op::Remove { id, label } => format!("remove\t{id}\t{}", c(label)),
            Op::RestartBluetooth { id, label } => format!("restart\t{id}\t{}", c(label)),
            Op::PowerSaving { id, label, allow } => format!("power\t{id}\t{}\t{}", c(label), *allow as u8),
            Op::AbsoluteVolume { enabled } => format!("absvol\t-\t-\t{}", *enabled as u8),
        }
    }

    fn from_line(line: &str) -> Option<Op> {
        let p: Vec<&str> = line.split('\t').collect();
        let (id, label) = (p.get(1)?.to_string(), p.get(2)?.to_string());
        let flag = p.get(3).map(|s| *s == "1");
        Some(match *p.first()? {
            "enable" => Op::Enable { id, label },
            "disable" => Op::Disable { id, label },
            "remove" => Op::Remove { id, label },
            "restart" => Op::RestartBluetooth { id, label },
            "power" => Op::PowerSaving { id, label, allow: flag? },
            "absvol" => Op::AbsoluteVolume { enabled: flag? },
            _ => return None,
        })
    }
}

pub struct OpResult {
    pub label: String,
    pub ok: bool,
    pub msg: String,
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation: TOKEN_ELEVATION = zeroed();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut c_void,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// Unpairs through the Bluetooth API (no admin needed). Works for devices on the active radio.
pub fn unpair(address: u64) -> bool {
    let mut addr: BLUETOOTH_ADDRESS = unsafe { zeroed() };
    addr.Anonymous.ullLong = address;
    unsafe { BluetoothRemoveDevice(&addr) == 0 }
}

/// Runs the operations, asking for admin rights once if needed. Blocks; call off the UI thread.
pub fn run(ops: &[Op], owner: isize) -> Result<Vec<OpResult>, String> {
    if ops.is_empty() {
        return Ok(Vec::new());
    }
    if is_elevated() {
        return Ok(ops.iter().map(run_one).collect());
    }

    let dir = std::env::temp_dir().join("BluetoothSwitcher");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stamp = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    );
    let ops_file = dir.join(format!("ops-{stamp}.txt"));
    let out_file = dir.join(format!("result-{stamp}.txt"));
    let body: String = ops.iter().map(|o| o.to_line() + "\n").collect();
    std::fs::write(&ops_file, body).map_err(|e| e.to_string())?;

    let launched = elevate_and_wait(&ops_file, &out_file, owner);
    let _ = std::fs::remove_file(&ops_file);
    launched?;

    let text = std::fs::read_to_string(&out_file).map_err(|_| "The admin helper didn't report back.".to_owned())?;
    let _ = std::fs::remove_file(&out_file);
    Ok(text.lines().filter_map(parse_result).collect())
}

fn elevate_and_wait(ops_file: &Path, out_file: &Path, owner: isize) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_w = wide(&exe.to_string_lossy());
    let verb = wide("runas");
    let params = wide(&format!("--apply \"{}\" \"{}\"", ops_file.display(), out_file.display()));

    unsafe {
        CoInitializeEx(null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as _);
        let mut info: SHELLEXECUTEINFOW = zeroed();
        info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.hwnd = owner as HWND;
        info.lpVerb = verb.as_ptr();
        info.lpFile = exe_w.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_HIDE as i32;
        if ShellExecuteExW(&mut info) == 0 {
            let err = GetLastError();
            return Err(if err == ERROR_CANCELLED {
                "Cancelled \u{2014} that change needs admin permission.".to_owned()
            } else {
                format!("Couldn't start the admin helper (error {err}).")
            });
        }
        if !info.hProcess.is_null() {
            WaitForSingleObject(info.hProcess, INFINITE);
            CloseHandle(info.hProcess);
        }
    }
    Ok(())
}

/// Entry point of the elevated copy.
pub fn apply_from_file(ops_file: &str, out_file: &str) -> i32 {
    let Ok(text) = std::fs::read_to_string(ops_file) else { return 2 };
    let allowed = devices::all_bluetooth_ids();

    let mut out = String::new();
    let mut all_ok = true;
    for op in text.lines().filter_map(Op::from_line) {
        let permitted = op.device_id().is_none_or(|id| allowed.contains(&id.to_ascii_uppercase()));
        let r = if permitted {
            run_one(&op)
        } else {
            OpResult { label: op.label(), ok: false, msg: "Not a Bluetooth device; skipped.".to_owned() }
        };
        all_ok &= r.ok;
        out.push_str(&format!("{}\t{}\t{}\n", r.ok as u8, clean(&r.label), clean(&r.msg)));
    }
    if std::fs::write(out_file, out).is_err() {
        return 3;
    }
    if all_ok { 0 } else { 1 }
}

fn run_one(op: &Op) -> OpResult {
    let (ok, msg) = match op {
        Op::Enable { id, .. } => pnputil(&["/enable-device", id]),
        Op::Disable { id, .. } => pnputil(&["/disable-device", id]),
        Op::Remove { id, .. } => pnputil(&["/remove-device", id, "/subtree"]),
        Op::RestartBluetooth { id, .. } => restart_bluetooth(id),
        Op::PowerSaving { id, allow, .. } => {
            match set_hklm_dword(&devices::power_key(id), devices::POWER_VALUE, *allow as u32) {
                Err(e) => (false, e),
                // The driver reads the setting when it starts, so restart the adapter if it's plugged in.
                Ok(()) => match restart_device(id) {
                    Ok(false) => (true, "Saved, adapter restarted".to_owned()),
                    _ => (true, "Saved \u{2014} takes effect after Windows restarts".to_owned()),
                },
            }
        }
        Op::AbsoluteVolume { enabled } => match set_hklm_dword(
            devices::ABS_VOLUME_KEY,
            devices::ABS_VOLUME_VALUE,
            if *enabled { 0 } else { 1 },
        ) {
            Ok(()) => (true, "Saved".to_owned()),
            Err(e) => (false, e),
        },
    };
    OpResult { label: op.label(), ok, msg }
}

fn system32(exe: &str) -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
    format!(r"{root}\System32\{exe}")
}

fn pnputil(args: &[&str]) -> (bool, String) {
    let output = Command::new(system32("pnputil.exe")).args(args).creation_flags(CREATE_NO_WINDOW).output();
    match output {
        Err(e) => (false, e.to_string()),
        Ok(out) => {
            let code = out.status.code().unwrap_or(-1);
            let text = String::from_utf8_lossy(&out.stdout);
            let last = text.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("").to_owned();
            match code {
                0 => (true, "Done".to_owned()),
                3010 => (true, "Done \u{2014} Windows wants a restart to finish.".to_owned()),
                _ => (false, if last.is_empty() { format!("pnputil exit code {code}") } else { last }),
            }
        }
    }
}

/// Restarts a device the way Device Manager / devcon do (DIF_PROPERTYCHANGE), which works on
/// every Windows edition, unlike `pnputil /restart-device`. Ok(true) = Windows needs a reboot
/// to finish (something held the device open).
fn restart_device(id: &str) -> Result<bool, String> {
    unsafe {
        let set = SetupDiCreateDeviceInfoList(null(), null_mut());
        if set as isize == -1 {
            return Err(format!("SetupAPI error {}", GetLastError()));
        }
        let mut data: SP_DEVINFO_DATA = zeroed();
        data.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
        let result = (|| {
            if SetupDiOpenDeviceInfoW(set, wide(id).as_ptr(), null_mut(), 0, &mut data) == 0 {
                return Err(format!("device not found ({})", GetLastError()));
            }
            let mut params: SP_PROPCHANGE_PARAMS = zeroed();
            params.ClassInstallHeader.cbSize = size_of::<SP_CLASSINSTALL_HEADER>() as u32;
            params.ClassInstallHeader.InstallFunction = DIF_PROPERTYCHANGE;
            params.StateChange = DICS_PROPCHANGE;
            params.Scope = DICS_FLAG_CONFIGSPECIFIC;
            if SetupDiSetClassInstallParamsW(set, &data, &params.ClassInstallHeader, size_of::<SP_PROPCHANGE_PARAMS>() as u32)
                == 0
                || SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, set, &data) == 0
            {
                return Err(format!("restart refused ({})", GetLastError()));
            }
            let mut install: SP_DEVINSTALL_PARAMS_W = zeroed();
            install.cbSize = size_of::<SP_DEVINSTALL_PARAMS_W>() as u32;
            Ok(SetupDiGetDeviceInstallParamsW(set, &data, &mut install) != 0
                && install.Flags & (DI_NEEDREBOOT | DI_NEEDRESTART) != 0)
        })();
        SetupDiDestroyDeviceInfoList(set);
        result
    }
}

struct Scm(SC_HANDLE);

impl Drop for Scm {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseServiceHandle(self.0) };
        }
    }
}

fn open_service(scm: &Scm, name: &str, access: u32) -> Result<Scm, String> {
    let h = unsafe { OpenServiceW(scm.0, wide(name).as_ptr(), access) };
    if h.is_null() { Err(format!("can't open {name} ({})", unsafe { GetLastError() })) } else { Ok(Scm(h)) }
}

fn open_scm() -> Result<Scm, String> {
    let h = unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT) };
    if h.is_null() { Err(format!("can't open the service manager ({})", unsafe { GetLastError() })) } else { Ok(Scm(h)) }
}

/// Stop the Bluetooth Support Service (and the per-user BluetoothUserService that depends on
/// it), restart the radio while nothing holds it, then always start the services again.
fn restart_bluetooth(id: &str) -> (bool, String) {
    let scm = match open_scm() {
        Ok(s) => s,
        Err(e) => return (false, e),
    };
    let main = match open_service(
        &scm,
        BT_SERVICE,
        SERVICE_STOP | SERVICE_START | SERVICE_QUERY_STATUS | SERVICE_ENUMERATE_DEPENDENTS,
    ) {
        Ok(s) => s,
        Err(e) => return (false, e),
    };

    for dep in dependents(&main, SERVICE_ACTIVE) {
        if let Ok(d) = open_service(&scm, &dep, SERVICE_STOP | SERVICE_QUERY_STATUS) {
            let _ = stop_and_wait(&d);
        }
    }
    // bthserv can take a long time to stop; carry on either way, the start below recovers it.
    let _ = stop_and_wait(&main);
    let device = restart_device(id);

    // Bring everything back no matter what happened above. Dependents include stopped
    // per-user instances (BluetoothUserService_xxxxx), which also repairs a half-stopped stack.
    let start = start_service(&main, BT_SERVICE);
    for dep in dependents(&main, SERVICE_STATE_ALL) {
        if let Ok(d) = open_service(&scm, &dep, SERVICE_START) {
            let _ = start_service(&d, &dep); // the template "BluetoothUserService" can't start; fine
        }
    }

    match (start, device) {
        (Err(e), _) => (false, format!("The Bluetooth service didn't start again: {e}")),
        (Ok(()), Err(e)) => (false, format!("Service restarted, but the adapter couldn't be restarted: {e}")),
        (Ok(()), Ok(false)) => (true, "Bluetooth service and adapter restarted".to_owned()),
        (Ok(()), Ok(true)) => (
            true,
            "Bluetooth service restarted. Windows will finish resetting the adapter itself the next time the PC \
             restarts \u{2014} if problems continue, restart the PC."
                .to_owned(),
        ),
    }
}

fn start_service(svc: &Scm, name: &str) -> Result<(), String> {
    if unsafe { StartServiceW(svc.0, 0, null()) } == 0 {
        let e = unsafe { GetLastError() };
        if e != ERROR_SERVICE_ALREADY_RUNNING {
            return Err(format!("couldn't start {name} ({e})"));
        }
    }
    Ok(())
}

fn stop_and_wait(svc: &Scm) -> Result<(), String> {
    unsafe {
        let mut status: SERVICE_STATUS = zeroed();
        if ControlService(svc.0, SERVICE_CONTROL_STOP, &mut status) == 0 {
            let e = GetLastError();
            if e != ERROR_SERVICE_NOT_ACTIVE {
                return Err(format!("couldn't stop ({e})"));
            }
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while QueryServiceStatus(svc.0, &mut status) != 0 && status.dwCurrentState != SERVICE_STOPPED {
            if Instant::now() > deadline {
                return Err("didn't stop in time".to_owned());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }
}

fn dependents(svc: &Scm, state: u32) -> Vec<String> {
    unsafe {
        let mut needed = 0u32;
        let mut count = 0u32;
        EnumDependentServicesW(svc.0, state, null_mut(), 0, &mut needed, &mut count);
        if needed == 0 {
            return Vec::new();
        }
        // u64 storage keeps the ENUM_SERVICE_STATUSW array properly aligned.
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        let entries = buf.as_mut_ptr() as *mut ENUM_SERVICE_STATUSW;
        if EnumDependentServicesW(svc.0, state, entries, needed, &mut needed, &mut count) == 0 {
            return Vec::new();
        }
        (0..count as usize)
            .map(|i| {
                let p = (*entries.add(i)).lpServiceName;
                let len = (0..).take_while(|&j| *p.add(j) != 0).count();
                String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
            })
            .collect()
    }
}

fn set_hklm_dword(subkey: &str, value: &str, data: u32) -> Result<(), String> {
    unsafe {
        let mut hkey: HKEY = null_mut();
        let rc = RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            wide(subkey).as_ptr(),
            0,
            null(),
            0,
            KEY_SET_VALUE,
            null(),
            &mut hkey,
            null_mut(),
        );
        if rc == 5 {
            return Err("Access denied (needs admin rights)".to_owned());
        }
        if rc != 0 {
            return Err(format!("registry error {rc}"));
        }
        let rc = RegSetValueExW(hkey, wide(value).as_ptr(), 0, REG_DWORD, &data as *const u32 as *const u8, 4);
        RegCloseKey(hkey);
        if rc == 0 { Ok(()) } else { Err(format!("registry error {rc}")) }
    }
}

fn parse_result(line: &str) -> Option<OpResult> {
    let p: Vec<&str> = line.split('\t').collect();
    (p.len() >= 3).then(|| OpResult { ok: p[0] == "1", label: p[1].to_owned(), msg: p[2].to_owned() })
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\r', '\n'], " ")
}
