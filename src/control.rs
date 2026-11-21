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
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, QueryServiceStatus, StartServiceW, SERVICE_STATUS,
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
        Op::RestartBluetooth { id, .. } => {
            let service = restart_service(BT_SERVICE);
            let (ok, msg) = pnputil(&["/restart-device", id]);
            match service {
                Ok(()) => (ok, if ok { "Bluetooth service and adapter restarted".to_owned() } else { msg }),
                Err(e) => (ok, if ok { format!("Adapter restarted (service: {e})") } else { msg }),
            }
        }
        Op::PowerSaving { id, allow, .. } => set_power_saving(id, *allow),
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

fn restart_service(name: &str) -> Result<(), String> {
    unsafe {
        let scm = OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err(format!("can't open service manager ({})", GetLastError()));
        }
        let svc = OpenServiceW(scm, wide(name).as_ptr(), SERVICE_STOP | SERVICE_START | SERVICE_QUERY_STATUS);
        if svc.is_null() {
            let e = GetLastError();
            CloseServiceHandle(scm);
            return Err(format!("can't open {name} ({e})"));
        }
        let mut status: SERVICE_STATUS = zeroed();
        let result = (|| {
            if ControlService(svc, SERVICE_CONTROL_STOP, &mut status) == 0 && GetLastError() != ERROR_SERVICE_NOT_ACTIVE {
                return Err(format!("couldn't stop ({})", GetLastError()));
            }
            let deadline = Instant::now() + Duration::from_secs(15);
            while QueryServiceStatus(svc, &mut status) != 0 && status.dwCurrentState != SERVICE_STOPPED {
                if Instant::now() > deadline {
                    return Err("didn't stop in time".to_owned());
                }
                std::thread::sleep(Duration::from_millis(150));
            }
            if StartServiceW(svc, 0, null()) == 0 && GetLastError() != ERROR_SERVICE_ALREADY_RUNNING {
                return Err(format!("couldn't start ({})", GetLastError()));
            }
            Ok(())
        })();
        CloseServiceHandle(svc);
        CloseServiceHandle(scm);
        result
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

/// The power-management checkbox is exposed through WMI (MSPower_DeviceEnable), which is
/// simplest to drive from PowerShell. Runs elevated, once, on explicit request.
fn set_power_saving(id: &str, allow: bool) -> (bool, String) {
    let script = format!(
        "$ErrorActionPreference='Stop'; $id='{}'; try {{ \
         $x = @(Get-CimInstance -Namespace root\\wmi -ClassName MSPower_DeviceEnable | \
               Where-Object {{ $_.InstanceName.StartsWith($id, [StringComparison]::OrdinalIgnoreCase) }}); \
         if ($x.Count -eq 0) {{ 'UNSUPPORTED'; exit 0 }}; \
         $was = $x[0].Enable; foreach ($i in $x) {{ Set-CimInstance -InputObject $i -Property @{{Enable=${}}} }}; \
         \"WAS:$was\" }} catch {{ 'ERR:' + $_.Exception.Message }}",
        id.replace('\'', "''"),
        if allow { "true" } else { "false" }
    );
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let output = Command::new(system32(r"WindowsPowerShell\v1.0\powershell.exe"))
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &base64(&utf16)])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match output {
        Err(e) => (false, e.to_string()),
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            if text.contains("UNSUPPORTED") {
                (false, "This adapter's driver doesn't offer a power-saving setting.".to_owned())
            } else if let Some(was) = text.lines().find_map(|l| l.trim().strip_prefix("WAS:")) {
                let was_on = was.eq_ignore_ascii_case("true");
                let state = if allow { "on" } else { "off" };
                if was_on == allow {
                    (true, format!("Power saving was already {state}"))
                } else {
                    (true, format!("Power saving is now {state}"))
                }
            } else if let Some(err) = text.lines().find_map(|l| l.trim().strip_prefix("ERR:")) {
                (false, err.trim().to_owned())
            } else {
                (false, "PowerShell didn't return a result.".to_owned())
            }
        }
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

fn parse_result(line: &str) -> Option<OpResult> {
    let p: Vec<&str> = line.split('\t').collect();
    (p.len() >= 3).then(|| OpResult { ok: p[0] == "1", label: p[1].to_owned(), msg: p[2].to_owned() })
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\r', '\n'], " ")
}
