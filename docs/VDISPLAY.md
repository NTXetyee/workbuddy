# Virtual display for WorkBuddy (T5)

WorkBuddy needs one extra Windows display at **2266x1488 @ 60 Hz** (iPad mini 6 native) for the
host to capture. We use the existing open-source IddCx driver
[VirtualDrivers/Virtual-Display-Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver)
(MIT). No custom driver is written.

Files:
- `scripts/install-vdd.ps1` installs, reloads, or removes the driver (`-Uninstall`).
- `scripts/vdd-settings.xml` is the settings file it copies to `C:\VirtualDisplayDriver\vdd_settings.xml`.

## What the script does

Pinned to release **25.7.23**.

1. Relaunches itself elevated (UAC prompt) if not already Administrator.
2. Downloads `VirtualDisplayDriver-x86.Driver.Only.zip` from the official release, prints its size and SHA256.
3. Extracts it and checks the Authenticode signature of every `.sys`, `.dll` and `.cat`. It aborts unless each one is `Valid`.
4. Downloads NefCon v1.14.0 (`nefarius/nefcon`), checks its signature, and runs `nefconw install MttVDD.inf Root\MttVDD`. This is the same method the upstream `silent-install.ps1` uses.
5. Backs up any existing `C:\VirtualDisplayDriver\vdd_settings.xml` to `vdd_settings.xml.bak-<timestamp>`, then copies ours in.
6. Disables and re-enables the virtual display device so the settings are read.
7. Lists display adapters, monitors and screens.

The temp folder is deleted at the end.

## How to run it

1. Open the `scripts` folder in Explorer.
2. Right-click `install-vdd.ps1` and choose **Run with PowerShell**, or run it from an elevated PowerShell.
3. Accept the UAC prompt. The elevated window stays open until you press Enter.

Do not run it as a non-admin account; it will only relaunch elevated.

## Confirm the monitor

1. Open **Settings > System > Display**.
2. A second display should appear with a resolution of 2266x1488 (or the closest mode it offers).
3. If it is missing, open **Device Manager > Display adapters** and look for "Virtual Display Driver".

## Scale and layout

- Set **Scale** on the new display to **200%** (recommended for this panel).
- Set **Multiple displays** to **Extend these displays**.
- Do not make the virtual display the primary display unless you want Windows windows to open on the iPad.

## Find the monitor index for `wb-host --list-outputs`

Run `wb-host --list-outputs` (once it exists) and pick the entry with 2266x1488 and the adapter named in
`vdd-settings.xml`. The script also prints the Windows screen list
(`System.Windows.Forms.Screen.AllScreens`). That list is in Windows' order, which may not match
`wb-host`'s enumeration. Verify with `wb-host` before relying on an index. Prefer `--output <name>`
over a fixed index.

## Settings in use

`scripts/vdd-settings.xml` uses these keys, taken from the upstream sample file
`Virtual Display Driver (HDR)/vdd_settings.xml`:

| Key | Value | Meaning |
|---|---|---|
| `monitors/count` | 1 | one virtual monitor |
| `gpu/friendlyname` | `NVIDIA GeForce RTX 5060 Laptop GPU` | adapter that hosts the virtual display |
| `resolutions/resolution` | 2266x1488@60 | native iPad mini 6 mode |
| `resolutions/resolution` | 1488x2266@60 | portrait, for later |
| `resolutions/resolution` | 1133x744@60, 1600x1050@60 | fallbacks |

The upstream sample also has a `<global><g_refresh_rate>` list (60 to 240). We left it out so the
driver does not advertise extra refresh rates.

Edit the XML, then rerun the script or disable and re-enable "Virtual Display Driver" in Device Manager.

## Uninstall

```powershell
powershell -ExecutionPolicy Bypass -File scripts\install-vdd.ps1 -Uninstall
```

This removes the device node (`pnputil /remove-device`) and the `mttvdd.inf` driver package
(`pnputil /delete-driver ... /uninstall /force`). It leaves `C:\VirtualDisplayDriver` in place.
It also accepts UAC.

## Caveats

- **GPU choice is unverified.** The sample sets `friendlyname` to a single adapter name. Upstream
  says `default` picks the adapter with the most VRAM. Upstream sources (search snippets, not the
  full pages) say that to pin a card you put its exact Device Manager name in `friendlyname`, and
  that identical GPUs need a bus-number suffix. We did not find a documented fallback list: if the
  named GPU is missing, behaviour is unknown and must be tested. The script does not check which
  adapter the monitor landed on. Check the `wb-host` log, or the adapter shown under Settings > Display > Advanced display.
- **Hybrid vs dGPU mode.** On this laptop (RTX 5060 + Radeon 780M) the NVIDIA GPU may be used only
  when the display is routed through it. If the virtual display shows up on the 780M, check the
  laptop's GPU mode (MUX / dGPU-only setting, if present) and the NVIDIA Control Panel. We did not verify
  how this virtual display routes on a hybrid laptop.
- **Driver updates.** Upstream advises uninstalling VDD before major GPU or chipset driver
  updates and reinstalling afterwards, because the virtual display can take display priority or cause a black screen on boot.
  Recovery: boot to Safe Mode and remove it in Device Manager.
- **HDR/SDR.** Upstream lists SDR at 8/10-bit and HDR at 10/12-bit, with HDR support on Windows 11
  23H2 and later. WorkBuddy's M1 is SDR (`SDR10bit` and `HDRPlus` are false). We did not verify the
  behaviour of this driver on Windows 11 26xxx builds: the upstream sources we could read do not
  mention build 26xxx.
- **Windows 10 issue.** The 25.7.23 release notes say a Windows 10 installation issue was fixed. We
  only target Windows 11.
- **Certificate.** We do not import certificates into the machine store. The upstream silent-install
  script does, from `mttvdd.cat`. The script relies on Authenticode `Valid` status. If `nefconw`
  fails with a trust error, that is the first thing to check.
- **Install method.** The project's documented install is the Virtual Driver Control (VDC) app
  (`VDD.Control.25.7.23.zip`, 67.9 MB, signed) or `winget install --id=VirtualDrivers.Virtual-Display-Driver -e`.
  We did not use either, because they are interactive and VDC is a large GUI download. The script uses
  the driver-only zip plus NefCon, as in upstream's `silent-install.ps1`.
- **Not run.** The script has been parse-checked only. It has not been executed, and the install
  itself has not been tested on this machine.

## Unverified items (stated, not guessed)

- The exact `nefconw` exit codes and whether `Root\MttVDD` is the right hardware ID for 25.7.23. We
  took both from upstream's `silent-install.ps1` (summarised by the fetch tool, not read verbatim).
- Whether the driver re-reads `vdd_settings.xml` only at device start. We assumed a disable/enable
  of the device is enough. Upstream does not document this.
- Whether the asset named `VirtualDisplayDriver-x86.Driver.Only.zip` contains x64 binaries. Upstream's
  silent-install script downloads that same asset for x64 Windows, so we use it, but we did not open the zip.
- The `friendlyname` behaviour when the named adapter is absent, and whether it accepts a
  bus-number suffix on a single GPU.
- The mintlify reference pages (`reference/vdd-settings-xml`, `configuration/resolutions`,
  `configuration/multi-gpu`, `troubleshooting/common-issues`) returned HTTP 404, 410, or 429 on
  fetch. Only search-result snippets from them were seen, so they are not used as a source for
  specific values.

## Sources

Read directly:
- https://github.com/VirtualDrivers/Virtual-Display-Driver (README: install, settings file path, GPU/driver-update notes, HDR/SDR table, MIT licence)
- https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/latest (tag 25.7.23, release notes)
- https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/expanded_assets/25.7.23 (asset names and sizes)
- https://api.github.com/repos/VirtualDrivers/Virtual-Display-Driver/contents/ (repo layout)
- https://api.github.com/repos/VirtualDrivers/Virtual-Display-Driver/contents/Virtual%20Display%20Driver%20(HDR) (file list)
- https://raw.githubusercontent.com/VirtualDrivers/Virtual-Display-Driver/master/Virtual%20Display%20Driver%20(HDR)/vdd_settings.xml (settings schema)
- https://raw.githubusercontent.com/VirtualDrivers/Virtual-Display-Driver/master/Community%20Scripts/silent-install.ps1 (install method; summarised)
- https://raw.githubusercontent.com/VirtualDrivers/Virtual-Display-Driver/master/Community%20Scripts/toggle-VDD.ps1 (device name and disable/enable)
- https://raw.githubusercontent.com/VirtualDrivers/Virtual-Display-Driver/master/Community%20Scripts/README.md

Search snippets only (page fetch failed):
- https://www.mintlify.com/VirtualDrivers/Virtual-Display-Driver/reference/vdd-settings-xml (gpu friendlyname default and bus-number note)
- https://www.mintlify.com/VirtualDrivers/Virtual-Display-Driver/configuration/multi-gpu (GPU selection, wrong-adapter symptom)
- https://github.com/VirtualDrivers/Virtual-Display-Driver/issues/200 (GPU partitioning issue; search snippet only)
