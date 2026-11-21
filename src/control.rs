//! Switching adapters on/off with pnputil. When the app isn't elevated, it relaunches
//! itself once via UAC ("runas") with `--apply <ops> <out>` and reads the results back.

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::ptr::null_mut;
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, HWND};
use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use crate::devices;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const ERROR_CANCELLED: u32 = 1223;

#[derive(Clone)]
pub struct Op {
    pub enable: bool,
    pub id: String,
    pub label: String,
}

pub struct OpResult {
    pub op: Op,
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

/// Runs the operations, asking for admin rights once if needed. Blocks; call off the UI thread.
pub fn run(ops: &[Op], owner: isize) -> Result<Vec<OpResult>, String> {
    if ops.is_empty() {
        return Ok(Vec::new());
    }
    if is_elevated() {
        return Ok(ops.iter().cloned().map(run_one).collect());
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
    let body: String = ops
        .iter()
        .map(|o| format!("{}\t{}\t{}\n", if o.enable { "enable" } else { "disable" }, o.id, clean(&o.label)))
        .collect();
    std::fs::write(&ops_file, body).map_err(|e| e.to_string())?;

    let launched = elevate_and_wait(&ops_file, &out_file, owner);
    let _ = std::fs::remove_file(&ops_file);
    launched?;

    let text = std::fs::read_to_string(&out_file).map_err(|_| "The admin helper didn't report back.".to_owned())?;
    let _ = std::fs::remove_file(&out_file);
    Ok(text.lines().filter_map(parse_result).collect())
}

fn elevate_and_wait(ops_file: &PathBuf, out_file: &PathBuf, owner: isize) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_w = wide(&exe.to_string_lossy());
    let verb = wide("runas");
    let params = wide(&format!("--apply \"{}\" \"{}\"", ops_file.display(), out_file.display()));

    unsafe {
        CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as _);
        let mut info: SHELLEXECUTEINFOW = zeroed();
        info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.hwnd = owner as HWND;
        info.lpVerb = verb.as_ptr();
        info.lpFile = exe_w.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_HIDE as i32;
        if ShellExecuteExW(&mut info) == 0 {
            return Err(if GetLastError() == ERROR_CANCELLED {
                "Cancelled — admin permission is needed to switch adapters.".to_owned()
            } else {
                format!("Couldn't start the admin helper (error {}).", GetLastError())
            });
        }
        if !info.hProcess.is_null() {
            WaitForSingleObject(info.hProcess, INFINITE);
            CloseHandle(info.hProcess);
        }
    }
    Ok(())
}

/// Entry point of the elevated copy. Only ever touches Bluetooth radios, whatever the file says.
pub fn apply_from_file(ops_file: &str, out_file: &str) -> i32 {
    let Ok(text) = std::fs::read_to_string(ops_file) else { return 2 };
    let adapters: Vec<String> = devices::scan(false).adapters.iter().map(|a| a.node.id.to_ascii_uppercase()).collect();

    let mut out = String::new();
    let mut all_ok = true;
    for line in text.lines() {
        let mut parts = line.split('\t');
        let (Some(action), Some(id), Some(label)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let op = Op { enable: action == "enable", id: id.to_owned(), label: label.to_owned() };
        let result = if adapters.contains(&op.id.to_ascii_uppercase()) {
            run_one(op)
        } else {
            OpResult { op, ok: false, msg: "Not a Bluetooth adapter; skipped.".to_owned() }
        };
        all_ok &= result.ok;
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            result.ok as u8,
            if result.op.enable { "enable" } else { "disable" },
            result.op.id,
            clean(&result.op.label),
            clean(&result.msg)
        ));
    }
    if std::fs::write(out_file, out).is_err() {
        return 3;
    }
    if all_ok { 0 } else { 1 }
}

fn run_one(op: Op) -> OpResult {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
    let output = Command::new(format!(r"{root}\System32\pnputil.exe"))
        .arg(if op.enable { "/enable-device" } else { "/disable-device" })
        .arg(&op.id)
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    match output {
        Err(e) => OpResult { op, ok: false, msg: e.to_string() },
        Ok(out) => {
            let code = out.status.code().unwrap_or(-1);
            let text = String::from_utf8_lossy(&out.stdout);
            let last = text.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("").to_owned();
            match code {
                0 => OpResult { msg: if op.enable { "Turned on" } else { "Turned off" }.to_owned(), op, ok: true },
                3010 => OpResult { op, ok: true, msg: "Done, but Windows wants a restart to finish.".to_owned() },
                _ => OpResult {
                    op,
                    ok: false,
                    msg: if last.is_empty() { format!("pnputil exit code {code}") } else { last },
                },
            }
        }
    }
}

fn parse_result(line: &str) -> Option<OpResult> {
    let p: Vec<&str> = line.split('\t').collect();
    if p.len() < 5 {
        return None;
    }
    Some(OpResult {
        op: Op { enable: p[1] == "enable", id: p[2].to_owned(), label: p[3].to_owned() },
        ok: p[0] == "1",
        msg: p[4].to_owned(),
    })
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\r', '\n'], " ")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
