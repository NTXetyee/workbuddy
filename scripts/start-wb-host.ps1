<#
.SYNOPSIS
  Starts the WorkBuddy iPad display host on the virtual 2266x1488 display.

.DESCRIPTION
  Intended to be launched from the WorkBuddy.lnk shortcut on the Desktop (see make-shortcut.ps1),
  but works from any current directory. Paths are resolved from this script's own location.
  Steps:
    1. Checks that host\target-all\release\wb-host.exe exists (tells the user how to build it if not).
    2. Refuses to start a second host if a wb-host process is already running.
    3. Runs "wb-host.exe --list-outputs" and picks the output whose SIZE is 2266x1488.
       If that size is not found, falls back to the highest-IDX output that is not the first row
       (the virtual monitor is usually not index 0) and warns about it.
    4. Starts "wb-host.exe --output <idx>" in this same console window and waits for it to end.

.PARAMETER DryRun
  Only prints the output index that would be used and exits without starting the host.
#>
[CmdletBinding()]
param(
    [switch]$DryRun
)

$targetSize = '2266x1488'
$scriptPath = $MyInvocation.MyCommand.Path
$scriptDir  = Split-Path -Parent $scriptPath
$repo       = Split-Path -Parent $scriptDir
$exe        = Join-Path $repo 'host\target-all\release\wb-host.exe'

function Wait-ForKey {
    if ($DryRun) { return }
    Write-Host ''
    Write-Host 'Press any key to close this window...'
    try {
        [void][System.Console]::ReadKey($true)
    } catch {
        Start-Sleep -Seconds 3
    }
}

function Exit-AfterKey {
    param([int]$Code = 1)
    Wait-ForKey
    exit $Code
}

# 1. Host program present?
if (-not (Test-Path -LiteralPath $exe)) {
    Write-Host 'The WorkBuddy host program was not found:' -ForegroundColor Red
    Write-Host "  $exe"
    Write-Host ''
    Write-Host 'Build it once: open a terminal in the host folder and run:'
    Write-Host '  cargo build --release -p wb-host --target-dir target-all'
    Write-Host 'Then start WorkBuddy again.'
    Exit-AfterKey -Code 1
}

# 2. Already running?
$running = @(Get-Process -Name 'wb-host' -ErrorAction SilentlyContinue)
if ($running.Count -gt 0 -and -not $DryRun) {
    Write-Host 'WorkBuddy host is already running' -ForegroundColor Yellow
    Write-Host 'Close the other WorkBuddy host window first if you want to restart it.'
    Exit-AfterKey -Code 0
}

# 3. Parse the output list.
$listLines = & $exe --list-outputs
$rows = @()
foreach ($line in $listLines) {
    if ($line -match '^\s*(\d+)\s+(.+?)\s+(\S*DISPLAY\d+)\s+(\d+x\d+)\s+(\S+)\s*$') {
        $rows += [pscustomobject]@{
            Idx      = [int]$Matches[1]
            Adapter  = $Matches[2].Trim()
            Output   = $Matches[3]
            Size     = $Matches[4]
            Attached = $Matches[5]
        }
    }
}

if ($rows.Count -eq 0) {
    Write-Host 'Could not read the display list from wb-host --list-outputs.' -ForegroundColor Red
    Exit-AfterKey -Code 1
}

$chosen = $rows | Where-Object { $_.Size -eq $targetSize } | Select-Object -First 1

if (-not $chosen) {
    $others = @($rows | Where-Object { $_.Idx -ne $rows[0].Idx })
    if ($others.Count -eq 0) {
        Write-Host 'Only one display output was found, so the WorkBuddy virtual display is missing.' -ForegroundColor Red
        Write-Host 'The virtual display driver is probably not installed or not attached.'
        Write-Host 'Run scripts\install-vdd.ps1 (as Administrator) to install it, then start WorkBuddy again.'
        Exit-AfterKey -Code 1
    }
    $chosen = $others | Sort-Object Idx -Descending | Select-Object -First 1
    Write-Host "WARNING: the virtual display was not found at $targetSize." -ForegroundColor Yellow
    Write-Host "Using the highest-numbered output instead (index $($chosen.Idx), $($chosen.Output), $($chosen.Size))." -ForegroundColor Yellow
    Write-Host ''
}

$idx = $chosen.Idx

# DryRun: report the choice and stop.
if ($DryRun) {
    Write-Output "DRYRUN: chosen output index = $idx ($($chosen.Output), $($chosen.Size), attached=$($chosen.Attached))"
    Write-Output "DRYRUN: host currently running = $($running.Count -gt 0)"
    exit 0
}

# 4. Start the host in this console window.
Write-Host 'WorkBuddy host. Open WorkBuddy on the iPad and keep it in the foreground. Close this window or press Ctrl+C to stop.'
Write-Host ''
Write-Host "Using display index $idx ($($chosen.Output), $($chosen.Size))."
Write-Host ''

& $exe --output $idx
$code = $LASTEXITCODE

Write-Host ''
Write-Host "WorkBuddy host has stopped. Exit code: $code"
Exit-AfterKey -Code $code
