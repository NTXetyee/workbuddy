<#
.SYNOPSIS
  Creates (or overwrites) WorkBuddy.lnk on the Desktop that starts the WorkBuddy iPad display host.

.DESCRIPTION
  The shortcut runs scripts\start-wb-host.ps1 through Windows PowerShell, using the repository
  root as the working directory.

  The target is conhost.exe, not powershell.exe directly. Windows 11 may host console apps in
  Windows Terminal, where the console is a pseudo-window that cannot be hidden. conhost.exe forces
  the classic console window, which the minimize-to-tray feature needs.
#>
[CmdletBinding()]
param()

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo      = Split-Path -Parent $scriptDir
$starter   = Join-Path $scriptDir 'start-wb-host.ps1'
$desktop   = [Environment]::GetFolderPath('Desktop')
$lnkPath   = Join-Path $desktop 'WorkBuddy.lnk'
$conhost   = Join-Path $env:SystemRoot 'System32\conhost.exe'

if (-not (Test-Path -LiteralPath $starter)) {
    throw "Launcher script not found: $starter"
}

# Pick a monitor-style icon that actually exists in this Windows install.
function Get-IconCount {
    param([string]$Path)
    Add-Type -Namespace WB -Name IconNative -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("shell32.dll", CharSet = System.Runtime.InteropServices.CharSet.Auto)]
public static extern uint ExtractIconEx(string lpszFile, int nIconIndex, System.IntPtr[] phiconLarge, System.IntPtr[] phiconSmall, uint nIcons);
'@ -ErrorAction SilentlyContinue
    return [WB.IconNative]::ExtractIconEx($Path, -1, $null, $null, 0)
}

$imageres = Join-Path $env:SystemRoot 'System32\imageres.dll'
$iconIndex = 109
if ((Test-Path -LiteralPath $imageres) -and ((Get-IconCount -Path $imageres) -gt $iconIndex)) {
    $iconLocation = "$imageres,$iconIndex"
} else {
    # Fallback: shell32 icon 15 (a computer/monitor-style icon on most Windows builds).
    $iconLocation = (Join-Path $env:SystemRoot 'System32\shell32.dll') + ',15'
}

$shell = New-Object -ComObject WScript.Shell
$sc = $shell.CreateShortcut($lnkPath)
$sc.TargetPath       = $conhost
$sc.Arguments        = "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$starter`""
$sc.WorkingDirectory = $repo
$sc.WindowStyle      = 1
$sc.Description      = 'Start the WorkBuddy iPad display host'
$sc.IconLocation     = $iconLocation
$sc.Save()

Write-Output "Created shortcut: $lnkPath"
Write-Output "  Target: $($sc.TargetPath)"
Write-Output "  Arguments: $($sc.Arguments)"
Write-Output "  Icon: $iconLocation"
