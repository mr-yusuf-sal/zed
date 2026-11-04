# Starts the installed Zed after build.ps1 -Install or install-detached.ps1 replaced it.
param([Parameter(Mandatory)][string]$Target)

# Explorer runs Zed.exe for every file it opens, only to hand the path to the running Zed, so
# the "run as administrator" compatibility flag turns each double-click into a UAC prompt.
# Elevation comes from the "Zed" task below instead, and since the fork's instance mutex and
# pipe admit medium-integrity processes, those launches reach an elevated Zed without one.
$layersPath = 'Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers'
$layers = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($layersPath, $true)
if ($layers) {
    foreach ($name in $layers.GetValueNames() | Where-Object { $_ -eq $Target }) {
        $value = [string]$layers.GetValue($name)
        if ($value -notmatch '\bRUNASADMIN\b') { continue }
        $rest = ($value -replace '\bRUNASADMIN\b', '' -replace '\s+', ' ').Trim()
        if ($rest -eq '' -or $rest -eq '~') {
            $layers.DeleteValue($name)
        } else {
            $layers.SetValue($name, $rest)
        }
        Write-Host "Removed the run-as-administrator flag from $name"
    }
    $layers.Close()
}

function Start-Unelevated {
    # explorer.exe starts Zed with the desktop user's normal token even when this script is
    # elevated.
    $elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if ($elevated) { Start-Process explorer.exe -ArgumentList "`"$Target`"" } else { Start-Process $Target }
}

# The login task runs Zed elevated without a UAC prompt. Without it, Zed is started
# unelevated: an elevated Zed sees an empty Windows Credential Manager, so git fetch/pull/push
# fall through to Zed's username prompt.
$task = Get-ScheduledTask -TaskPath '\' -TaskName 'Zed' -ErrorAction SilentlyContinue |
    Where-Object { $_.Principal.RunLevel -eq 'Highest' }
if (-not $task) {
    Start-Unelevated
    return
}

Start-ScheduledTask -InputObject $task -ErrorAction Stop
# The task starts Zed hidden. A normal launch then hands off to it and brings up a window, but
# only once its pipe exists: before that, the launch would become the first instance itself
# and the task's Zed would exit as a second one.
$pipe = '\\.\pipe\Zed-Editor-Stable-Named-Pipe'
for ($i = 0; $i -lt 60 -and -not ([IO.Directory]::GetFiles('\\.\pipe\') -contains $pipe); $i++) {
    Start-Sleep -Milliseconds 500
}
if ([IO.Directory]::GetFiles('\\.\pipe\') -contains $pipe) {
    Start-Unelevated
} else {
    Write-Warning "The elevated Zed from the 'Zed' task did not come up within 30 s; run 'schtasks /run /tn Zed' to start it."
}
