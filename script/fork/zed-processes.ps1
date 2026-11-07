# Helpers for the scripts that stop and restart Zed (build.ps1 -Install, install-detached.ps1).
# Dot-source it: . (Join-Path $PSScriptRoot "zed-processes.ps1")

# The Zed windows. Agent hosts (`zed --agent-host`) run Claude Code turns, and terminal hosts
# (`zed --terminal-host`) run terminal shells, that must outlive Zed, so they are left out.
function Get-ZedUi {
    Get-CimInstance Win32_Process -Filter "Name = 'zed.exe'" |
        Where-Object { $_.CommandLine -notmatch '--(agent|terminal)-host' }
}

Add-Type -Namespace ZedFork -Name Token -MemberDefinition @'
[DllImport("kernel32.dll", SetLastError = true)]
public static extern System.IntPtr OpenProcess(uint access, bool inherit, int processId);
[DllImport("advapi32.dll", SetLastError = true)]
public static extern bool OpenProcessToken(System.IntPtr process, uint access, out System.IntPtr token);
[DllImport("advapi32.dll", SetLastError = true)]
public static extern bool GetTokenInformation(System.IntPtr token, int infoClass, out int info, int length, out int returned);
[DllImport("kernel32.dll")]
public static extern bool CloseHandle(System.IntPtr handle);
'@ -ErrorAction SilentlyContinue

# Whether the process runs with an elevated token (TokenElevation).
function Test-ElevatedProcess([int]$ProcessId) {
    $process = [ZedFork.Token]::OpenProcess(0x1000, $false, $ProcessId) # QUERY_LIMITED_INFORMATION
    if ($process -eq [IntPtr]::Zero) { return $false }
    try {
        $token = [IntPtr]::Zero
        if (-not [ZedFork.Token]::OpenProcessToken($process, 0x8, [ref]$token)) { return $false } # TOKEN_QUERY
        try {
            $elevated = 0
            $returned = 0
            if (-not [ZedFork.Token]::GetTokenInformation($token, 20, [ref]$elevated, 4, [ref]$returned)) {
                return $false
            }
            return $elevated -ne 0
        } finally {
            [void][ZedFork.Token]::CloseHandle($token)
        }
    } finally {
        [void][ZedFork.Token]::CloseHandle($process)
    }
}

# Whether a Zed window runs elevated. The Zed started after an install keeps that elevation:
# agent hosts are separate for elevated and unelevated Zed, so a Zed of the other elevation
# would not find the Claude Code turns, subagents and background commands left running.
function Test-ZedUiElevated {
    @(Get-ZedUi | Where-Object { Test-ElevatedProcess $_.ProcessId }).Count -gt 0
}
