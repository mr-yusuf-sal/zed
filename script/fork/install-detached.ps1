# Installs target\release\zed.exe after a delay, from a process that outlives Zed, for when the
# build ran in Zed's own terminal and stopping Zed would take build.ps1 -Install down with it.
# The log goes to %TEMP% so it never dirties this checkout.
$log = Join-Path $env:TEMP "zed-install-detached.log"
Start-Transcript -Path $log -Force | Out-Null
$dir = "$env:LOCALAPPDATA\Programs\Zed"
$target = Join-Path $dir "Zed.exe"
$official = Join-Path $dir "Zed.official.exe"
$previous = Join-Path $dir "Zed.previous.exe"
$built = Join-Path (Split-Path (Split-Path $PSScriptRoot)) "target\release\zed.exe"

Start-Sleep -Seconds 5

try {
    if (-not (Test-Path $official)) {
        Copy-Item $target $official -ErrorAction Stop
        Write-Host "Kept the official exe as $official"
    }
    # Windows lets a running exe be renamed but not overwritten, and keeps the image locked for
    # a moment after the process exits, so moving it aside avoids racing that lock.
    if (Test-Path $previous) {
        Remove-Item $previous -Force -ErrorAction SilentlyContinue
    }
    if (Test-Path $previous) {
        $previous = Join-Path $dir "Zed.previous-$(Get-Date -Format yyyyMMddHHmmss).exe"
    }
    Move-Item $target $previous -ErrorAction Stop
    Copy-Item $built $target -ErrorAction Stop
    Write-Host "Installed the fork to $target"
} catch {
    Write-Host "Install failed: $_"
}

# Agent hosts (`zed --agent-host`) run Claude Code turns, and terminal hosts
# (`zed --terminal-host`) run terminal shells, that must outlive Zed. They keep
# running from the exe moved aside above; the new Zed reconnects to them.
function Get-ZedUi {
    Get-CimInstance Win32_Process -Filter "Name = 'zed.exe'" |
        Where-Object { $_.CommandLine -notmatch '--(agent|terminal)-host' }
}
Get-ZedUi | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
for ($i = 0; $i -lt 30 -and (Get-ZedUi); $i++) { Start-Sleep -Milliseconds 500 }
# An elevated Zed sees an empty Windows Credential Manager, so git fetch/pull/push fall
# through to Zed's username prompt, and every host and restart it spawns stays elevated.
# explorer.exe starts it with the desktop user's normal token instead.
$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if ($elevated) { Start-Process explorer.exe -ArgumentList "`"$target`"" } else { Start-Process $target }
Stop-Transcript | Out-Null
