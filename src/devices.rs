//! Reads Bluetooth radios and paired devices straight from the PnP manager (CfgMgr32).
//! Windows keeps device nodes for unplugged hardware too, which is how we can show
//! pairings that belong to an adapter that isn't in use right now.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_PropertyW, CM_Get_Device_ID_ListW, CM_Get_Device_ID_List_SizeW, CM_Locate_DevNodeW,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;

const CR_SUCCESS: u32 = 0;
const CR_BUFFER_SMALL: u32 = 0x1A;
const CM_GETIDLIST_FILTER_ENUMERATOR: u32 = 0x1;
const CM_GETIDLIST_FILTER_CLASS: u32 = 0x200;
const CM_LOCATE_DEVNODE_PHANTOM: u32 = 1;

const DEVPROP_TYPE_BYTE: u32 = 0x03;
const DEVPROP_TYPE_UINT16: u32 = 0x05;
const DEVPROP_TYPE_UINT32: u32 = 0x07;
const DEVPROP_TYPE_UINT64: u32 = 0x09;
const DEVPROP_TYPE_FILETIME: u32 = 0x10;
const DEVPROP_TYPE_BOOLEAN: u32 = 0x11;
const DEVPROP_TYPE_STRING: u32 = 0x12;
const DEVPROP_TYPE_STRING_LIST: u32 = 0x2012;
const CONFIGFLAG_DISABLED: u32 = 0x1;
const CM_PROB_DISABLED: u32 = 22;

const BLUETOOTH_CLASS: &str = "{e0cbf06c-cd8b-4647-bb8a-263b43f0f974}";
/// Hands-Free Audio Gateway service: the "headset mic" profile that drops audio to mono.
const HFP_SERVICE: &str = "BTHENUM\\{0000111E-0000-1000-8000-00805F9B34FB}";

pub const ABS_VOLUME_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Bluetooth\Audio\AVRCP\CT";
pub const ABS_VOLUME_VALUE: &str = "DisableAbsoluteVolume";
/// Where KMDF drivers (BTHUSB and friends) keep Device Manager's
/// "Allow the computer to turn off this device to save power" checkbox.
pub const POWER_VALUE: &str = "IdleInWorkingState";

pub fn power_key(instance_id: &str) -> String {
    format!(r"SYSTEM\CurrentControlSet\Enum\{instance_id}\Device Parameters\WDF")
}

#[repr(C)]
struct DevPropKey {
    fmtid: GUID,
    pid: u32,
}

const fn key(guid: u128, pid: u32) -> DevPropKey {
    DevPropKey { fmtid: GUID::from_u128(guid), pid }
}

const DEVICE_DESC: DevPropKey = key(0xa45c254e_df1c_4efd_8020_67d146a850e0, 2);
const CONFIG_FLAGS: DevPropKey = key(0xa45c254e_df1c_4efd_8020_67d146a850e0, 12);
const MANUFACTURER: DevPropKey = key(0xa45c254e_df1c_4efd_8020_67d146a850e0, 13);
const FRIENDLY_NAME: DevPropKey = key(0xa45c254e_df1c_4efd_8020_67d146a850e0, 14);
const PROBLEM_CODE: DevPropKey = key(0x4340a6c5_93fa_4706_972c_7b648008a5a7, 3);
const PARENT: DevPropKey = key(0x4340a6c5_93fa_4706_972c_7b648008a5a7, 8);
const IS_PRESENT: DevPropKey = key(0x540b947e_8b40_45bc_a8a2_6a0b894cbda2, 5);
const DRIVER_DATE: DevPropKey = key(0xa8b865dd_2e3d_4094_ad97_e593a70c75d6, 2);
const DRIVER_VERSION: DevPropKey = key(0xa8b865dd_2e3d_4094_ad97_e593a70c75d6, 3);
const DRIVER_PROVIDER: DevPropKey = key(0xa8b865dd_2e3d_4094_ad97_e593a70c75d6, 9);
const CONTAINER_CATEGORY: DevPropKey = key(0x78c34fc8_104a_4aca_9ea4_524d52996e57, 90);
const BT_LAST_CONNECTED: DevPropKey = key(0x2bd67d8b_8beb_48d5_87e0_6cda3428040a, 11);
/// Undocumented but stable since Windows 8: "Bluetooth device is connected".
const BT_CONNECTED: DevPropKey = key(0x83da6326_97a6_4088_9453_a1923f573b29, 15);
/// Battery level Windows caches for devices that report it (LE battery service, HFP).
const BATTERY: DevPropKey = key(0x104ea319_6ee2_4701_bd47_8ddbf425bbe5, 2);
const RADIO_ADDRESS: DevPropKey = key(0xa92f26ca_eda7_4b1d_9db2_27b68aa5a2eb, 1);
const RADIO_LMP_VERSION: DevPropKey = key(0xa92f26ca_eda7_4b1d_9db2_27b68aa5a2eb, 4);

const ADAPTER_BUSES: [&str; 5] = ["USB", "PCI", "ACPI", "UART", "SDIO"];

pub struct Node {
    pub id: String,
    /// Upper-cased id, for case-insensitive matching (Windows mixes cases in parent ids).
    key: String,
    pub name: String,
    pub manufacturer: String,
    parent: Option<String>,
    pub present: bool,
    pub disabled: bool,
    connected: bool,
    inst: u32,
}

impl Node {
    fn enumerator(&self) -> &str {
        self.key.split('\\').next().unwrap_or("")
    }
}

/// The hands-free (headset mic) service entry of an audio device.
pub struct Hfp {
    pub id: String,
    pub disabled: bool,
}

pub struct Device {
    pub node: Node,
    pub category: Option<String>,
    pub battery: Option<u8>,
    pub last_connected: Option<u64>,
    pub hfp: Option<Hfp>,
    /// Sibling service entries (classic devices), removed together with the device.
    pub services: Vec<String>,
}

impl Device {
    pub fn connected(&self) -> bool {
        self.node.connected && self.node.present
    }

    pub fn is_le(&self) -> bool {
        self.node.enumerator() == "BTHLE"
    }

    pub fn address_raw(&self) -> Option<u64> {
        let part = self.node.key.split('\\').nth(1)?;
        let hex = part.strip_prefix("DEV_")?;
        (hex.len() == 12).then(|| u64::from_str_radix(hex, 16).ok()).flatten()
    }

    pub fn address(&self) -> String {
        self.address_raw().map(format_address).unwrap_or_default()
    }

    /// "Headphones", "Keyboard", ... from Windows' device-container category.
    pub fn kind(&self) -> String {
        let fallback = if self.is_le() { "Bluetooth LE device" } else { "Bluetooth device" };
        let Some(cat) = self.category.as_deref() else { return fallback.to_owned() };
        let known = match cat.to_ascii_lowercase().as_str() {
            "audio.headphone" => "Headphones",
            "audio.headset" => "Headset",
            "audio.speakers" => "Speaker",
            "audio.microphone" => "Microphone",
            "input.keyboard" => "Keyboard",
            "input.mouse" => "Mouse",
            "input.gaming" => "Game controller",
            "input.pen" => "Pen",
            "input.touchpad" => "Touchpad",
            "communication.phone" | "phone" => "Phone",
            "computer" | "computer.laptop" | "computer.desktop" => "Computer",
            "display.monitor" | "display" => "Display",
            "multimedia.tv" => "TV",
            "printfax.printer" => "Printer",
            "health.fitness" | "health" => "Fitness / health",
            "watch" => "Watch",
            _ => "",
        };
        if !known.is_empty() {
            return known.to_owned();
        }
        if self.hfp.is_some() {
            return "Headset".to_owned(); // vague category, but it has a hands-free (mic) profile
        }
        // Unknown category: use its last segment, e.g. "Other.Gadget" -> "Gadget".
        cat.rsplit('.').next().filter(|s| !s.is_empty()).map(str::to_owned).unwrap_or_else(|| fallback.to_owned())
    }
}

pub struct Adapter {
    pub node: Node,
    /// Windows has a running Bluetooth stack on this radio.
    pub active: bool,
    pub devices: Vec<Device>,
    pub lmp_version: Option<u8>,
    pub address: Option<u64>,
    pub driver_version: Option<String>,
    pub driver_provider: Option<String>,
    pub driver_date: Option<u64>,
    /// Windows may power the adapter down when idle. None = not set (driver default: on).
    pub power_saving: Option<bool>,
}

impl Adapter {
    pub fn power_saving_on(&self) -> bool {
        self.power_saving.unwrap_or(true)
    }

    pub fn state_text(&self) -> &'static str {
        match (self.node.present, self.node.disabled, self.active) {
            (false, true, _) => "Off · not plugged in",
            (false, false, _) => "Not plugged in",
            (true, true, _) => "Off",
            (true, false, true) => "In use",
            (true, false, false) => "On, but Windows isn't using it",
        }
    }

    pub fn version_text(&self) -> &'static str {
        self.lmp_version.map(bluetooth_version).unwrap_or("")
    }
}

#[derive(Default)]
pub struct Snapshot {
    pub adapters: Vec<Adapter>,
    /// Devices whose adapter Windows no longer knows about.
    pub orphans: Vec<Device>,
    pub absolute_volume_disabled: bool,
}

impl Snapshot {
    pub fn describe(&self) -> String {
        let mut s = String::new();
        let now = now_filetime();
        for a in &self.adapters {
            let _ = writeln!(
                s,
                "[{}] {} ({}) BT {} power-saving {} addr {} driver {} {}  {}",
                a.state_text(),
                a.node.name,
                a.node.manufacturer,
                a.version_text(),
                match a.power_saving {
                    Some(true) => "on",
                    Some(false) => "off",
                    None => "default",
                },
                a.address.map(format_address).unwrap_or_default(),
                a.driver_version.as_deref().unwrap_or("?"),
                a.driver_date.map(format_date).unwrap_or_default(),
                a.node.id
            );
            for d in &a.devices {
                let _ = writeln!(
                    s,
                    "    {:<28} {:<17} {:<12} batt {:<5} last {:<12} {}{}",
                    d.node.name,
                    d.address(),
                    d.kind(),
                    d.battery.map(|b| format!("{b}%")).unwrap_or_default(),
                    d.last_connected.map(|t| format_ago(t, now)).unwrap_or_default(),
                    if d.connected() { "connected" } else { "paired" },
                    match &d.hfp {
                        Some(h) if h.disabled => "  [headset mic off]",
                        Some(_) => "  [headset mic on]",
                        None => "",
                    }
                );
            }
        }
        for d in &self.orphans {
            let _ = writeln!(s, "  (unknown adapter) {} {}", d.node.name, d.node.id);
        }
        let _ = writeln!(s, "absolute volume: {}", if self.absolute_volume_disabled { "off" } else { "on" });
        s
    }
}

pub fn bluetooth_version(lmp: u8) -> &'static str {
    match lmp {
        0 => "1.0b",
        1 => "1.1",
        2 => "1.2",
        3 => "2.0",
        4 => "2.1",
        5 => "3.0",
        6 => "4.0",
        7 => "4.1",
        8 => "4.2",
        9 => "5.0",
        10 => "5.1",
        11 => "5.2",
        12 => "5.3",
        13 => "5.4",
        14 => "6.0",
        15 => "6.1",
        _ => "newer than 6.1",
    }
}

pub fn format_address(a: u64) -> String {
    let b = a.to_be_bytes();
    format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", b[2], b[3], b[4], b[5], b[6], b[7])
}

pub fn now_filetime() -> u64 {
    let mut ft = unsafe { std::mem::zeroed() };
    unsafe { GetSystemTimeAsFileTime(&mut ft) };
    (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64
}

/// FILETIME (100 ns ticks since 1601, UTC) -> "Feb 2, 2026".
pub fn format_date(ft: u64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    // Days since 1970-01-01, then Howard Hinnant's civil-from-days.
    let days = (ft / 864_000_000_000) as i64 - 134_774;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{} {}, {}", MONTHS[(m - 1) as usize], d, y)
}

pub fn format_ago(ft: u64, now: u64) -> String {
    let secs = now.saturating_sub(ft) / 10_000_000;
    match secs {
        0..=59 => "Just now".to_owned(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        86_400..=172_799 => "Yesterday".to_owned(),
        172_800..=2_591_999 => format!("{} days ago", secs / 86_400),
        _ => format_date(ft),
    }
}

/// Every Bluetooth-related device id (upper-case). The elevated helper only touches these.
pub fn all_bluetooth_ids() -> HashSet<String> {
    let mut ids: HashSet<String> = list_ids(Some(BLUETOOTH_CLASS), CM_GETIDLIST_FILTER_CLASS)
        .into_iter()
        .map(|s| s.to_ascii_uppercase())
        .collect();
    ids.extend(list_ids(Some("BTHENUM"), CM_GETIDLIST_FILTER_ENUMERATOR).into_iter().map(|s| s.to_ascii_uppercase()));
    ids
}

pub fn scan() -> Snapshot {
    // Bluetooth-class nodes, plus every BTHENUM node: the hands-free service entries are
    // class "System", and that's where Windows keeps classic headphones' battery level.
    let mut seen = HashSet::new();
    let nodes: Vec<Node> = list_ids(Some(BLUETOOTH_CLASS), CM_GETIDLIST_FILTER_CLASS)
        .into_iter()
        .chain(list_ids(Some("BTHENUM"), CM_GETIDLIST_FILTER_ENUMERATOR))
        .filter(|id| seen.insert(id.to_ascii_uppercase()))
        .filter_map(|id| load(&id))
        .collect();

    let radios_with_stack: HashSet<String> =
        nodes.iter().filter(|n| n.enumerator() == "BTH").filter_map(|n| n.parent.clone()).collect();

    let mut buf = PropBuf::new();
    let mut adapters: Vec<Adapter> = Vec::new();
    let mut adapter_index: HashMap<String, usize> = HashMap::new();
    let mut stacks: Vec<Node> = Vec::new();
    let mut devices: Vec<Node> = Vec::new();
    // (parent stack, MAC) -> (hfp id, disabled, battery)
    let mut hfp: HashMap<(String, String), (String, bool, Option<u8>)> = HashMap::new();
    // (parent stack, MAC) -> every BTHENUM service entry of that device
    let mut services: HashMap<(String, String), Vec<String>> = HashMap::new();

    for n in nodes {
        let e = n.enumerator();
        let second = n.key.split('\\').nth(1).unwrap_or("");
        if e == "BTH" {
            if n.parent.is_some() {
                stacks.push(n);
            }
        } else if (e == "BTHENUM" || e == "BTHLE") && second.starts_with("DEV_") {
            devices.push(n);
        } else if e == "BTHENUM" && second.starts_with('{') {
            // ...\B&2663C1A6&0&909C4AEE83A2_C00000000  -> MAC 909C4AEE83A2
            let mac = n.key.rsplit('&').next().and_then(|s| s.split('_').next()).unwrap_or("").to_owned();
            if let Some(parent) = n.parent.clone() {
                if n.key.starts_with(HFP_SERVICE) {
                    let battery = buf.int(n.inst, &BATTERY).map(|b| b as u8);
                    hfp.insert((parent.clone(), mac.clone()), (n.id.clone(), n.disabled, battery));
                }
                services.entry((parent, mac)).or_default().push(n.id.clone());
            }
        } else if !e.starts_with("BTH") && (ADAPTER_BUSES.contains(&e) || radios_with_stack.contains(&n.key)) {
            adapter_index.insert(n.key.clone(), adapters.len());
            adapters.push(Adapter {
                lmp_version: buf.int(n.inst, &RADIO_LMP_VERSION).map(|v| v as u8),
                address: buf.int(n.inst, &RADIO_ADDRESS),
                driver_version: buf.string(n.inst, &DRIVER_VERSION),
                driver_provider: buf.string(n.inst, &DRIVER_PROVIDER),
                driver_date: buf.filetime(n.inst, &DRIVER_DATE),
                power_saving: read_hklm_dword(&power_key(&n.id), POWER_VALUE).map(|v| v != 0),
                node: n,
                active: false,
                devices: Vec::new(),
            });
        }
    }

    let mut stack_to_adapter: HashMap<&str, usize> = HashMap::new();
    for s in &stacks {
        let Some(&ai) = s.parent.as_deref().and_then(|p| adapter_index.get(p)) else { continue };
        stack_to_adapter.insert(s.key.as_str(), ai);
        let a = &mut adapters[ai];
        if s.present && !s.disabled && a.node.present && !a.node.disabled {
            a.active = true;
        }
    }

    // LE keyboards/mice: their HID-over-GATT entries only exist while connected, which is a
    // more dependable "connected" signal than the Bluetooth flag for some devices.
    let le_hid_connected: HashSet<String> = list_ids(Some("HID"), CM_GETIDLIST_FILTER_ENUMERATOR)
        .into_iter()
        .filter(|id| id.to_ascii_uppercase().starts_with("HID\\{00001812-"))
        .filter(|id| is_present(id))
        .filter_map(|id| {
            // HID\{00001812-...}_DEV_VID&02046D_PID&B35B_REV&0013_E6233C39A28A&COL01\...
            let inner = id.split('\\').nth(1)?.to_ascii_uppercase();
            Some(inner.rsplit('_').next()?.split('&').next()?.to_owned())
        })
        .collect();

    let mut snap = Snapshot::default();
    for mut n in devices {
        let mac = n.key.split('\\').nth(1).and_then(|p| p.strip_prefix("DEV_")).unwrap_or("").to_owned();
        if n.enumerator() == "BTHLE" && le_hid_connected.contains(&mac) {
            n.connected = true;
        }
        let pair_key = n.parent.clone().map(|p| (p, mac));
        let service = pair_key.as_ref().and_then(|k| hfp.remove(k));
        let own_battery = buf.int(n.inst, &BATTERY).map(|b| b as u8);
        let device = Device {
            category: buf.first_string(n.inst, &CONTAINER_CATEGORY),
            battery: own_battery.or(service.as_ref().and_then(|s| s.2)),
            last_connected: buf.filetime(n.inst, &BT_LAST_CONNECTED),
            hfp: service.map(|(id, disabled, _)| Hfp { id, disabled }),
            services: pair_key.and_then(|k| services.remove(&k)).unwrap_or_default(),
            node: n,
        };
        match device.node.parent.as_deref().and_then(|p| stack_to_adapter.get(p)).copied() {
            Some(ai) => adapters[ai].devices.push(device),
            None => snap.orphans.push(device),
        }
    }

    for a in &mut adapters {
        a.devices.sort_by(|x, y| {
            y.connected()
                .cmp(&x.connected())
                .then_with(|| x.node.name.to_lowercase().cmp(&y.node.name.to_lowercase()))
        });
    }
    adapters.sort_by(|x, y| {
        y.active
            .cmp(&x.active)
            .then(y.node.present.cmp(&x.node.present))
            .then_with(|| x.node.name.cmp(&y.node.name))
    });
    snap.adapters = adapters;
    snap.absolute_volume_disabled = read_hklm_dword(ABS_VOLUME_KEY, ABS_VOLUME_VALUE).unwrap_or(0) != 0;
    snap
}

fn read_hklm_dword(subkey: &str, value: &str) -> Option<u32> {
    let (k, v) = (wide(subkey), wide(value));
    let mut data = 0u32;
    let mut size = 4u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut _,
            &mut size,
        )
    };
    (rc == 0).then_some(data)
}

fn load(id: &str) -> Option<Node> {
    let w = wide(id);
    let mut inst = 0u32;
    if unsafe { CM_Locate_DevNodeW(&mut inst, w.as_ptr(), CM_LOCATE_DEVNODE_PHANTOM) } != CR_SUCCESS {
        return None;
    }
    let mut buf = PropBuf::new();
    let problem = buf.int(inst, &PROBLEM_CODE);
    let flags = buf.int(inst, &CONFIG_FLAGS).unwrap_or(0) as u32;
    Some(Node {
        id: id.to_owned(),
        key: id.to_ascii_uppercase(),
        name: buf
            .string(inst, &FRIENDLY_NAME)
            .or_else(|| buf.string(inst, &DEVICE_DESC))
            .unwrap_or_else(|| id.to_owned()),
        manufacturer: buf.string(inst, &MANUFACTURER).unwrap_or_default(),
        parent: buf.string(inst, &PARENT).map(|p| p.to_ascii_uppercase()),
        present: buf.bool(inst, &IS_PRESENT).unwrap_or(false),
        disabled: problem == Some(CM_PROB_DISABLED as u64) || flags & CONFIGFLAG_DISABLED != 0,
        connected: buf.bool(inst, &BT_CONNECTED).unwrap_or(false),
        inst,
    })
}

fn is_present(id: &str) -> bool {
    let w = wide(id);
    let mut inst = 0u32;
    unsafe { CM_Locate_DevNodeW(&mut inst, w.as_ptr(), 0) == CR_SUCCESS }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn list_ids(filter: Option<&str>, flags: u32) -> Vec<String> {
    let filter_w = filter.map(wide);
    let filter_ptr = filter_w.as_ref().map_or(std::ptr::null(), |f| f.as_ptr());
    for _ in 0..5 {
        let mut len = 0u32;
        if unsafe { CM_Get_Device_ID_List_SizeW(&mut len, filter_ptr, flags) } != CR_SUCCESS {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        let cr = unsafe { CM_Get_Device_ID_ListW(filter_ptr, buf.as_mut_ptr(), len, flags) };
        if cr == CR_BUFFER_SMALL {
            continue; // a device showed up in between; try again
        }
        if cr != CR_SUCCESS {
            return Vec::new();
        }
        return buf.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect();
    }
    Vec::new()
}

/// Reusable property buffer: one 1 KB read covers almost every property,
/// so most lookups are a single call with no allocation.
struct PropBuf {
    data: Vec<u8>,
}

impl PropBuf {
    fn new() -> Self {
        Self { data: vec![0; 1024] }
    }

    fn read(&mut self, inst: u32, key: &DevPropKey) -> Option<(u32, &[u8])> {
        let mut ty = 0u32;
        let mut size = self.data.len() as u32;
        let key_ptr = key as *const DevPropKey as *const _;
        let mut cr = unsafe { CM_Get_DevNode_PropertyW(inst, key_ptr, &mut ty, self.data.as_mut_ptr(), &mut size, 0) };
        if cr == CR_BUFFER_SMALL {
            self.data.resize(size as usize, 0);
            cr = unsafe { CM_Get_DevNode_PropertyW(inst, key_ptr, &mut ty, self.data.as_mut_ptr(), &mut size, 0) };
        }
        (cr == CR_SUCCESS).then(|| (ty, &self.data[..size as usize]))
    }

    fn string(&mut self, inst: u32, key: &DevPropKey) -> Option<String> {
        let (ty, raw) = self.read(inst, key)?;
        (ty == DEVPROP_TYPE_STRING).then(|| utf16_first(raw)).flatten()
    }

    fn first_string(&mut self, inst: u32, key: &DevPropKey) -> Option<String> {
        let (ty, raw) = self.read(inst, key)?;
        (ty == DEVPROP_TYPE_STRING_LIST || ty == DEVPROP_TYPE_STRING).then(|| utf16_first(raw)).flatten()
    }

    /// Any unsigned integer property, widened to u64.
    fn int(&mut self, inst: u32, key: &DevPropKey) -> Option<u64> {
        let (ty, raw) = self.read(inst, key)?;
        let len = match ty {
            DEVPROP_TYPE_BYTE => 1,
            DEVPROP_TYPE_UINT16 => 2,
            DEVPROP_TYPE_UINT32 => 4,
            DEVPROP_TYPE_UINT64 => 8,
            _ => return None,
        };
        let mut bytes = [0u8; 8];
        bytes[..len].copy_from_slice(raw.get(..len)?);
        Some(u64::from_le_bytes(bytes))
    }

    fn filetime(&mut self, inst: u32, key: &DevPropKey) -> Option<u64> {
        let (ty, raw) = self.read(inst, key)?;
        if ty != DEVPROP_TYPE_FILETIME {
            return None;
        }
        let v = u64::from_le_bytes(raw.get(..8)?.try_into().ok()?);
        (v != 0).then_some(v)
    }

    fn bool(&mut self, inst: u32, key: &DevPropKey) -> Option<bool> {
        let (ty, raw) = self.read(inst, key)?;
        (ty == DEVPROP_TYPE_BOOLEAN).then(|| raw.first().map(|&b| b != 0)).flatten()
    }
}

/// First NUL-terminated UTF-16 string in a property buffer, trimmed.
fn utf16_first(raw: &[u8]) -> Option<String> {
    let units: Vec<u16> =
        raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
    let s = String::from_utf16_lossy(&units).trim().to_owned();
    (!s.is_empty()).then_some(s)
}
