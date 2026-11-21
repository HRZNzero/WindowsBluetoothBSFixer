# Windows Bluetooth BS Fixer

A tiny native Windows 11 app that lets you pick **which Bluetooth adapter Windows uses**.

Windows only runs one Bluetooth radio at a time and gives you no setting to choose it, so a
better USB dongle often gets ignored in favour of the onboard chip. This app lists every
adapter Windows knows about (plugged in or not), shows which paired devices belong to which
adapter, and with one click switches the others off so Windows has to use the one you picked.

## Features

- Lists all Bluetooth adapters with their state: *In use*, *Off*, *Not plugged in*, *On but unused*
- Paired devices grouped by the adapter they were paired with, with live connection status
- **Use this adapter**: enables your pick and disables the other present adapters (one UAC prompt)
- **Turn all adapters back on**: one-click undo
- Refreshes by itself when hardware is plugged or unplugged
- Warns when a proprietary 2.4 GHz receiver (Logitech LIGHTSPEED/Unifying/Bolt, etc.) is present.
  Those aren't Bluetooth and can't be "switched", but they share the 2.4 GHz band and are a
  common cause of Bluetooth lag. Separating them with USB extension cables usually helps.

## Things to know

- **Pairings are per adapter.** After switching, devices that were never paired with the new
  adapter need to be paired again (the app offers to open Bluetooth settings).
- If your keyboard is Bluetooth it will drop during the switch, so finish pairing with the mouse.
- Disabling survives reboots. If you unplug the dongle you picked, open the app and hit
  *Turn all adapters back on*, or the onboard adapter stays off.

## Build

Rust, Win32 API via `windows-sys`, no runtime dependencies. Release exe is ~460 KB.

```powershell
cargo build --release
```

The exe lands in `target\release\bt-switcher.exe`. Works with both the MSVC and GNU Rust toolchains.

## How it works

- Device discovery uses CfgMgr32 (`CM_Get_Device_ID_List`, `CM_Get_DevNode_Property`) directly,
  including non-present device nodes, and maps each paired device → Microsoft BT enumerator →
  radio through `DEVPKEY_Device_Parent`. A full scan takes ~30 ms and runs off the UI thread.
- Switching runs `pnputil /enable-device` / `/disable-device`. When the app isn't elevated it
  relaunches itself once via UAC with `--apply`, and the elevated copy only touches devices it
  verifies are Bluetooth adapters.
- `bt-switcher.exe --dump out.txt` writes what the scanner sees, for troubleshooting.
