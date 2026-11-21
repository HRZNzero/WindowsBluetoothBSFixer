# Windows Bluetooth BS Fixer

A small, fast, native Windows 10/11 app for the Bluetooth problems Windows doesn't give you a
button for: pick which Bluetooth adapter Windows uses, restart a stuck Bluetooth stack, stop
headphones dropping to mono call quality, clear out ghost devices, and more.

One ~500 KB exe, no installer, no runtime. Written in Rust on the raw Win32 API.

## What it does

**Adapters**

- **Use this adapter.** Windows runs only one Bluetooth adapter at a time and has no setting to
  choose it, so a better USB dongle often gets ignored in favour of the built-in chip. Pick the
  adapter you want and the others get switched off, so Windows has to use it. *Turn all adapters
  back on* undoes it.
- **Restart Bluetooth.** Restarts the Bluetooth Support Service (and the per-user Bluetooth
  service that depends on it) and asks Windows to restart the adapter. This is the usual fix
  when Bluetooth stops working, the on/off toggle disappears, or devices refuse to connect. If
  something holds the adapter open, Windows finishes the adapter reset at the next PC restart,
  and the app says so.
- **Power saving** on/off. Same as Device Manager's *Allow the computer to turn off this device
  to save power*, the classic fix for devices that drop out or don't come back after sleep. The
  current state is shown in the menu and in Details.
- **Details.** Bluetooth version (5.0, 5.2, …), adapter address, driver version and date.
- **Forget this adapter.** Removes an unplugged adapter and everything Windows remembers for it.

**Devices** (grouped by the adapter they're paired with)

- Type (headphones, keyboard, mouse, …), **battery level** where the device reports it,
  connection status and **last connected** time.
- **Forget device.** Unpairs properly, and falls back to removing the device entries when a
  device is stuck and won't remove in Settings.
- **Turn off headset mic.** Windows switches Bluetooth headphones to low-quality mono
  "hands-free" mode whenever an app opens their mic. Turning the headset mic off keeps them in
  full-quality stereo (use another mic). Can be turned back on any time.
- **Clean up old devices.** Removes leftover and duplicate "ghost" entries that belong to adapters
  that are no longer plugged in.

**Audio**

- **Absolute volume** on/off. Turn it off when the volume is far too loud or quiet, jumps
  around, or Windows and the headphones fight over it.

**Plus** live refresh when hardware is plugged or unplugged, shortcuts to Bluetooth settings,
Sound settings, Device Manager and the Windows troubleshooters, and a *Troubleshooting tips*
page (Help menu). Most commands are also on right-click.

## Things to know

- **Pairings belong to an adapter.** After switching adapters, devices that were never paired
  with the new one need pairing again. The app offers to open Bluetooth settings.
- If your keyboard or mouse is Bluetooth it drops during a switch or restart. Keep a wired one
  handy, just in case.
- Switching adapters survives reboots. If you unplug the dongle you picked, use *Turn all
  adapters back on*, or the built-in adapter stays off.
- Changes need admin rights; the app asks once per action via the normal Windows UAC prompt.

## Build

```powershell
cargo build --release
```

The exe lands in `target\release\bluetooth-bs-fixer.exe`. It builds with both the MSVC and GNU Rust
toolchains, with no resource compiler needed (the manifest is embedded by `build.rs`).

## How it works

- Discovery reads the PnP device tree directly through CfgMgr32 (`CM_Get_Device_ID_List`,
  `CM_Get_DevNode_Property`), including entries for unplugged hardware, and maps every paired
  device to its radio via `DEVPKEY_Device_Parent`. A full scan takes ~40 ms and runs off the UI thread.
- Battery, device type, last-connected time and the radio's LMP version come from the device
  properties Windows already caches. No pairing or radio traffic is involved.
- Changes use `pnputil` (enable/disable/remove), SetupAPI `DIF_PROPERTYCHANGE` (adapter restart,
  which works on every Windows edition, unlike `pnputil /restart-device`), the Service Control Manager, and the
  registry: `DisableAbsoluteVolume`, and the driver framework's `Device Parameters\WDF\IdleInWorkingState`
  for power saving. Unpairing uses `BluetoothRemoveDevice`.
- When not already elevated, the app relaunches itself once via UAC with `--apply`. The
  elevated copy only touches devices it verifies are Bluetooth devices.
- `bluetooth-bs-fixer.exe --dump out.txt` writes what the scanner sees, for troubleshooting or bug reports.

## Tested

Tested on Windows 11 (build 26200) with a MediaTek RZ616 adapter and a Realtek USB dongle:
scanning, switching UI, headset-mic toggle, absolute volume, power saving, Restart Bluetooth,
Forget device (a leftover entry on an unplugged adapter) and the elevated helper's safety checks.
Not yet tested on real devices: *Clean up old devices* and *Forget this adapter* (they remove many
entries at once), unpairing a device on the active adapter, and *Use this adapter* with two
adapters plugged in.
