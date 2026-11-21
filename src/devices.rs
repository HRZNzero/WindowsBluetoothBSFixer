//! Reads Bluetooth radios and paired devices straight from the PnP manager (CfgMgr32).
//! Windows keeps device nodes for unplugged hardware too, which is how we can show
//! pairings that belong to an adapter that isn't in use right now.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_PropertyW, CM_Get_Device_ID_ListW, CM_Get_Device_ID_List_SizeW, CM_Locate_DevNodeW,
};

const CR_SUCCESS: u32 = 0;
const CR_BUFFER_SMALL: u32 = 0x1A;
const CM_GETIDLIST_FILTER_PRESENT: u32 = 0x100;
const CM_GETIDLIST_FILTER_CLASS: u32 = 0x200;
const CM_LOCATE_DEVNODE_NORMAL: u32 = 0;
const CM_LOCATE_DEVNODE_PHANTOM: u32 = 1;

const DEVPROP_TYPE_UINT32: u32 = 0x07;
const DEVPROP_TYPE_BOOLEAN: u32 = 0x11;
const DEVPROP_TYPE_STRING: u32 = 0x12;
const CONFIGFLAG_DISABLED: u32 = 0x1;
const CM_PROB_DISABLED: u32 = 22;

const BLUETOOTH_CLASS: &str = "{e0cbf06c-cd8b-4647-bb8a-263b43f0f974}";

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
/// Undocumented but stable since Windows 8: "Bluetooth device is connected".
const BT_CONNECTED: DevPropKey = key(0x83da6326_97a6_4088_9453_a1923f573b29, 15);

const ADAPTER_BUSES: [&str; 5] = ["USB", "PCI", "ACPI", "UART", "SDIO"];
const RECEIVER_HINTS: [&str; 7] = [
    "LIGHTSPEED", "UNIFYING RECEIVER", "BOLT RECEIVER", "WIRELESS RECEIVER", "2.4G", "HYPERSPEED", "NANO RECEIVER",
];

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
}

impl Node {
    fn enumerator(&self) -> &str {
        self.key.split('\\').next().unwrap_or("")
    }
}

pub struct Device {
    pub node: Node,
}

impl Device {
    pub fn connected(&self) -> bool {
        self.node.connected && self.node.present
    }

    pub fn kind(&self) -> &'static str {
        if self.node.enumerator() == "BTHLE" { "Bluetooth LE" } else { "Bluetooth" }
    }

    /// BTHENUM\DEV_A8E6E89F86F4\...  ->  A8:E6:E8:9F:86:F4
    pub fn address(&self) -> String {
        let part = self.node.key.split('\\').nth(1).unwrap_or("");
        match part.strip_prefix("DEV_") {
            Some(hex) if hex.len() == 12 => {
                let mut s = String::with_capacity(17);
                for (i, c) in hex.chars().enumerate() {
                    if i > 0 && i % 2 == 0 {
                        s.push(':');
                    }
                    s.push(c);
                }
                s
            }
            _ => String::new(),
        }
    }
}

pub struct Adapter {
    pub node: Node,
    /// Windows has a running Bluetooth stack on this radio.
    pub active: bool,
    pub devices: Vec<Device>,
}

impl Adapter {
    pub fn state_text(&self) -> &'static str {
        match (self.node.present, self.node.disabled, self.active) {
            (false, true, _) => "Off · not plugged in",
            (false, false, _) => "Not plugged in",
            (true, true, _) => "Off",
            (true, false, true) => "In use",
            (true, false, false) => "On, but Windows isn't using it",
        }
    }
}

#[derive(Default)]
pub struct Snapshot {
    pub adapters: Vec<Adapter>,
    pub orphans: Vec<Device>,
    pub receiver: Option<String>,
}

impl Snapshot {
    pub fn describe(&self) -> String {
        let mut s = String::new();
        for a in &self.adapters {
            let _ = writeln!(s, "[{}] {} ({})  {}", a.state_text(), a.node.name, a.node.manufacturer, a.node.id);
            for d in &a.devices {
                let status = if d.connected() { "connected" } else { "paired" };
                let _ = writeln!(s, "    {:<34} {:<18} {:<13} {}", d.node.name, d.address(), d.kind(), status);
            }
        }
        for d in &self.orphans {
            let _ = writeln!(s, "  (unknown adapter) {} {}", d.node.name, d.node.id);
        }
        let _ = writeln!(s, "2.4 GHz receiver: {}", self.receiver.as_deref().unwrap_or("none"));
        s
    }
}

pub fn scan(include_receiver: bool) -> Snapshot {
    let nodes: Vec<Node> = list_ids(Some(BLUETOOTH_CLASS), CM_GETIDLIST_FILTER_CLASS)
        .iter()
        .filter_map(|id| load(id))
        .collect();

    // Microsoft's per-radio enumerators (BTH\MS_BTHBRB\..., BTH\MS_BTHLE\...) hang off the radio.
    let radios_with_stack: HashSet<String> = nodes
        .iter()
        .filter(|n| n.enumerator() == "BTH")
        .filter_map(|n| n.parent.clone())
        .collect();

    let mut adapters: Vec<Adapter> = Vec::new();
    let mut adapter_index: HashMap<String, usize> = HashMap::new();
    let mut stacks: Vec<Node> = Vec::new();
    let mut devices: Vec<Node> = Vec::new();

    for n in nodes {
        let e = n.enumerator();
        if e == "BTH" {
            if n.parent.is_some() {
                stacks.push(n);
            }
        } else if (e == "BTHENUM" || e == "BTHLE") && n.key.split('\\').nth(1).is_some_and(|p| p.starts_with("DEV_")) {
            devices.push(n);
        } else if !e.starts_with("BTH") && (ADAPTER_BUSES.contains(&e) || radios_with_stack.contains(&n.key)) {
            adapter_index.insert(n.key.clone(), adapters.len());
            adapters.push(Adapter { node: n, active: false, devices: Vec::new() });
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

    let mut snap = Snapshot::default();
    for n in devices {
        let target = n.parent.as_deref().and_then(|p| stack_to_adapter.get(p)).copied();
        match target {
            Some(ai) => adapters[ai].devices.push(Device { node: n }),
            None => snap.orphans.push(Device { node: n }),
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

    if include_receiver {
        snap.receiver = find_receiver();
    }
    snap
}

/// Looks for a proprietary 2.4 GHz dongle (Logitech LIGHTSPEED etc.) among present devices.
fn find_receiver() -> Option<String> {
    let mut buf = PropBuf::new();
    for id in list_ids(None, CM_GETIDLIST_FILTER_PRESENT) {
        let Some(inst) = locate(&id, CM_LOCATE_DEVNODE_NORMAL) else { continue };
        let name = buf.string(inst, &FRIENDLY_NAME).or_else(|| buf.string(inst, &DEVICE_DESC));
        if let Some(name) = name {
            let upper = name.to_uppercase();
            if RECEIVER_HINTS.iter().any(|h| upper.contains(h)) {
                return Some(name);
            }
        }
    }
    None
}

fn load(id: &str) -> Option<Node> {
    let inst = locate(id, CM_LOCATE_DEVNODE_PHANTOM)?;
    let mut buf = PropBuf::new();
    let problem = buf.u32(inst, &PROBLEM_CODE);
    let flags = buf.u32(inst, &CONFIG_FLAGS).unwrap_or(0);
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
        disabled: problem == Some(CM_PROB_DISABLED) || flags & CONFIGFLAG_DISABLED != 0,
        connected: buf.bool(inst, &BT_CONNECTED).unwrap_or(false),
    })
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn locate(id: &str, flags: u32) -> Option<u32> {
    let w = wide(id);
    let mut inst = 0u32;
    let cr = unsafe { CM_Locate_DevNodeW(&mut inst, w.as_ptr(), flags) };
    (cr == CR_SUCCESS).then_some(inst)
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
        return buf
            .split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect();
    }
    Vec::new()
}

/// Reusable property buffer: one stack-sized read covers almost every property,
/// so most lookups are a single syscall with no allocation.
struct PropBuf {
    data: Vec<u8>,
}

impl PropBuf {
    fn new() -> Self {
        Self { data: vec![0; 1024] }
    }

    fn read(&mut self, inst: u32, key: &DevPropKey, expected: u32) -> Option<&[u8]> {
        let mut ty = 0u32;
        let mut size = self.data.len() as u32;
        let key_ptr = key as *const DevPropKey as *const _;
        let mut cr = unsafe { CM_Get_DevNode_PropertyW(inst, key_ptr, &mut ty, self.data.as_mut_ptr(), &mut size, 0) };
        if cr == CR_BUFFER_SMALL {
            self.data.resize(size as usize, 0);
            cr = unsafe { CM_Get_DevNode_PropertyW(inst, key_ptr, &mut ty, self.data.as_mut_ptr(), &mut size, 0) };
        }
        (cr == CR_SUCCESS && ty == expected).then(|| &self.data[..size as usize])
    }

    fn string(&mut self, inst: u32, key: &DevPropKey) -> Option<String> {
        let raw = self.read(inst, key, DEVPROP_TYPE_STRING)?;
        let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
        let s = String::from_utf16_lossy(&units).trim().to_owned();
        (!s.is_empty()).then_some(s)
    }

    fn u32(&mut self, inst: u32, key: &DevPropKey) -> Option<u32> {
        let raw = self.read(inst, key, DEVPROP_TYPE_UINT32)?;
        raw.get(..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn bool(&mut self, inst: u32, key: &DevPropKey) -> Option<bool> {
        let raw = self.read(inst, key, DEVPROP_TYPE_BOOLEAN)?;
        raw.first().map(|&b| b != 0)
    }
}
