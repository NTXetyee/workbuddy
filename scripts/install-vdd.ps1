<#
.SYNOPSIS
  Installs (or removes) the open-source Virtual Display Driver used by WorkBuddy.

.DESCRIPTION
  Pinned to VirtualDrivers/Virtual-Display-Driver release 25.7.23.
  Steps (install):
    1. Self-elevates (UAC prompt) if not already running as Administrator.
    2. Downloads the driver package zip from the official GitHub release, prints size and SHA256.
    3. Extracts it and verifies the Authenticode signature of every .sys/.dll/.cat (must be Valid).
    4. Downloads NefCon (nefarius/nefcon v1.14.0, the installer used by the upstream
       silent-install.ps1), verifies its signature, and installs MttVDD.inf on Root\MttVDD.
    5. Backs up any existing C:\VirtualDisplayDriver\vdd_settings.xml and copies scripts\vdd-settings.xml there.
    6. Disables/enables the virtual display device so the settings are re-read.
    7. Lists display adapters/monitors so the new 2266x1488 output can be identified.

  -Uninstall removes the device node and the driver package (pnputil) and leaves settings in place.

.NOTES
  Does not run anything from the WorkBuddy repo other than this script and the settings file.
  Downloads are kept in a fresh temp folder that is deleted at the end.
#>
[CmdletBinding()]
param(
    [switch]$Uninstall,
    [string]$DriverUrl = 'https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/download/25.7.23/VirtualDisplayDriver-x86.Driver.Only.zip',
    [string]$NefConUrl = 'https://github.com/nefarius/nefcon/releases/download/v1.14.0/nefcon_v1.14.0.zip',
    [string]$SettingsSource = (Join-Path $PSScriptRoot 'vdd-settings.xml'),
    [string]$SettingsDir = 'C:\VirtualDisplayDriver',
    [switch]$ElevatedRelaunch
)

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

function Write-Step([string]$msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

function Test-IsAdmin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    return (New-Object Security.Principal.WindowsPrincipal $id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

# ---------------------------------------------------------------- self-elevation
if (-not (Test-IsAdmin)) {
    Write-Step 'Not running as Administrator; relaunching elevated (UAC prompt).'
    $relaunchArgs = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-ElevatedRelaunch')
    if ($Uninstall) { $relaunchArgs += '-Uninstall' }
    try {
        Start-Process -FilePath 'powershell.exe' -ArgumentList $relaunchArgs -Verb RunAs | Out-Null
    } catch {
        Write-Error "Elevation was cancelled or failed: $($_.Exception.Message)"
        exit 1
    }
    exit 0
}

function Pause-IfRelaunched {
    if ($ElevatedRelaunch) {
        Write-Host ''
        Write-Host 'Press Enter to close this window.'
        [void](Read-Host)
    }
}

# ---------------------------------------------------------------- helpers
function Get-VddDevices {
    Get-PnpDevice -ErrorAction SilentlyContinue |
        Where-Object { $_.InstanceId -like 'ROOT\MttVDD*' -or $_.FriendlyName -eq 'Virtual Display Driver' -or $_.FriendlyName -eq 'IddSampleDriver Device HDR' }
}

function Assert-ValidSignature([string]$path) {
    $sig = Get-AuthenticodeSignature -LiteralPath $path
    if ($sig.Status -ne 'Valid') {
        throw "Authenticode signature NOT valid for '$path' (status: $($sig.Status)). Aborting."
    }
    Write-Host ("    signed by: {0}" -f $sig.SignerCertificate.Subject)
}

function Show-MonitorList {
    Write-Step 'Display adapters and monitors'
    Get-PnpDevice -Class Display, Monitor -ErrorAction SilentlyContinue |
        Select-Object Class, Status, FriendlyName, InstanceId | Format-Table -AutoSize | Out-String | Write-Host
    Add-Type -AssemblyName System.Windows.Forms
    Write-Step 'Screens as Windows sees them (index = order used by wb-host --list-outputs enumeration, verify there)'
    $i = 0
    foreach ($s in [System.Windows.Forms.Screen]::AllScreens) {
        Write-Host ("  [{0}] {1}  {2}x{3} at ({4},{5}) primary={6}" -f $i, $s.DeviceName, $s.Bounds.Width, $s.Bounds.Height, $s.Bounds.X, $s.Bounds.Y, $s.Primary)
        $i++
    }
}

# ---------------------------------------------------------------- uninstall
if ($Uninstall) {
    try {
        Write-Step 'Removing virtual display device node(s)'
        foreach ($d in Get-VddDevices) {
            Write-Host "    pnputil /remove-device $($d.InstanceId)"
            & pnputil.exe /remove-device $d.InstanceId | Out-Host
        }

        Write-Step 'Removing driver package (mttvdd.inf) from the driver store'
        $enum = & pnputil.exe /enum-drivers
        $published = $null
        foreach ($line in $enum) {
            if ($line -match '^\s*Published Name\s*:\s*(\S+)') { $published = $Matches[1] }
            elseif ($line -match '^\s*Original Name\s*:\s*mttvdd\.inf\s*$' -and $published) {
                Write-Host "    pnputil /delete-driver $published /uninstall /force"
                & pnputil.exe /delete-driver $published /uninstall /force | Out-Host
                $published = $null
            }
        }
        Write-Host "Uninstall finished. Settings at $SettingsDir were left in place."
        Show-MonitorList
    } catch {
        Write-Error $_
        Pause-IfRelaunched
        exit 1
    }
    Pause-IfRelaunched
    exit 0
}

# ---------------------------------------------------------------- install
$work = Join-Path $env:TEMP ('vdd-install-' + [guid]::NewGuid().ToString('N'))
try {
    New-Item -ItemType Directory -Path $work | Out-Null

    # 1. Driver package from the official release
    Write-Step "Downloading driver package: $DriverUrl"
    $zip = Join-Path $work 'vdd-driver.zip'
    Invoke-WebRequest -Uri $DriverUrl -OutFile $zip -UseBasicParsing
    $size = (Get-Item -LiteralPath $zip).Length
    $sha = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash
    Write-Host "    size:   $size bytes"
    Write-Host "    SHA256: $sha"

    $drvDir = Join-Path $work 'driver'
    Expand-Archive -LiteralPath $zip -DestinationPath $drvDir

    $inf = Get-ChildItem -LiteralPath $drvDir -Recurse -Filter 'MttVDD.inf' | Select-Object -First 1
    if (-not $inf) { throw 'MttVDD.inf not found in the downloaded package. Aborting.' }
    $infDir = $inf.DirectoryName

    # 2. Authenticode check of every signed binary/catalog in the package
    Write-Step 'Verifying Authenticode signatures of package files'
    $signed = Get-ChildItem -LiteralPath $drvDir -Recurse -File | Where-Object { $_.Extension -in '.sys', '.dll', '.cat' }
    if (-not $signed) { throw 'No .sys/.dll/.cat files found to verify. Aborting.' }
    foreach ($f in $signed) {
        Write-Host "  $($f.Name)"
        Assert-ValidSignature $f.FullName
    }

    # 3. NefCon (documented installer used by the upstream silent-install script)
    Write-Step "Downloading NefCon: $NefConUrl"
    $nefZip = Join-Path $work 'nefcon.zip'
    Invoke-WebRequest -Uri $NefConUrl -OutFile $nefZip -UseBasicParsing
    Write-Host ("    size:   {0} bytes" -f (Get-Item -LiteralPath $nefZip).Length)
    Write-Host ("    SHA256: {0}" -f (Get-FileHash -LiteralPath $nefZip -Algorithm SHA256).Hash)
    $nefDir = Join-Path $work 'nefcon'
    Expand-Archive -LiteralPath $nefZip -DestinationPath $nefDir
    $nefconw = Get-ChildItem -LiteralPath $nefDir -Recurse -Filter 'nefconw.exe' | Where-Object { $_.FullName -match '\\x64\\' } | Select-Object -First 1
    if (-not $nefconw) { throw 'x64\nefconw.exe not found in the NefCon package. Aborting.' }
    Assert-ValidSignature $nefconw.FullName

    # 4. Install the driver on the root-enumerated device
    Write-Step 'Installing driver (nefconw install MttVDD.inf Root\MttVDD)'
    Push-Location $infDir
    try {
        & $nefconw.FullName install (Join-Path $infDir 'MttVDD.inf') 'Root\MttVDD'
        $code = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    if ($code -ne 0) { throw "nefconw install failed with exit code $code." }
    Start-Sleep -Seconds 5

    # 5. Settings file: back up existing, copy ours
    Write-Step "Installing settings to $SettingsDir\vdd_settings.xml"
    [xml]$check = Get-Content -LiteralPath $SettingsSource -Raw -Encoding UTF8
    if ($check.vdd_settings.resolutions.resolution.Count -lt 1) { throw 'Settings file has no resolutions. Aborting.' }
    New-Item -ItemType Directory -Path $SettingsDir -Force | Out-Null
    $dest = Join-Path $SettingsDir 'vdd_settings.xml'
    if (Test-Path -LiteralPath $dest) {
        $bak = Join-Path $SettingsDir ('vdd_settings.xml.bak-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
        Copy-Item -LiteralPath $dest -Destination $bak
        Write-Host "    existing settings backed up to $bak"
    }
    Copy-Item -LiteralPath $SettingsSource -Destination $dest -Force

    # 6. Reload the device so the settings are re-read
    Write-Step 'Reloading the virtual display device'
    $devs = @(Get-VddDevices)
    if ($devs.Count -eq 0) {
        Write-Warning 'No VDD device node found after install. Check Device Manager > Display adapters.'
    } else {
        foreach ($d in $devs) {
            Disable-PnpDevice -InstanceId $d.InstanceId -Confirm:$false
            Start-Sleep -Seconds 2
            Enable-PnpDevice -InstanceId $d.InstanceId -Confirm:$false
        }
        Start-Sleep -Seconds 3
    }

    # 7. Show the result
    Show-MonitorList
    Write-Host ''
    Write-Host 'Done. Expect a new 2266x1488 display. Set Settings > System > Display: scale 200%, Extend these displays.'
} catch {
    Write-Error "Install failed: $($_.Exception.Message)"
    Pause-IfRelaunched
    exit 1
} finally {
    if (Test-Path -LiteralPath $work) { Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue }
}

Pause-IfRelaunched
exit 0
