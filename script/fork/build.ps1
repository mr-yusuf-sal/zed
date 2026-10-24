<#
.SYNOPSIS
    Builds the personal Zed fork (mr-yusuf-sal/zed, branch "custom") and installs it
    over the official Zed.exe.

.DESCRIPTION
    The "custom" branch is a small stack of commits on top of an upstream stable tag
    (e.g. v1.24.4). Building from a stable tag keeps the release channel "stable", so
    the fork uses the same database, settings and workspace history as official Zed.

    -Update rebases the stack onto the newest upstream stable tag and pushes it to the
    fork. -Install keeps the official exe once as Zed.official.exe, moves the running exe
    aside, copies the build over Zed.exe and restarts Zed; agent hosts are left running so
    Claude Code turns survive, and the cli (bin\zed) is left as is since it just launches it.

    The build sets ZED_UPDATE_EXPLANATION, which compiles auto-update out of the binary.
    Without it the official updater would overwrite the fork on its next check. It is a
    build-time switch on purpose: "auto_update": false in settings.json would also stop
    official Zed from updating on the other machine that shares Backups\zed.

    The fork updates itself instead (crates/fork_sync): it asks zed.incapon.com for the
    newest build, which is the GitHub release on mr-yusuf-sal/zed that -Publish creates.
    -Publish zips the exe, uploads it as release custom-<commit sha> and keeps the newest
    three. The commit has to be pushed and the tree clean, since the release claims the
    exe is that commit; every fork running another commit installs it within the hour.

    -Sweep deletes build output nothing has used for -SweepDays days (default 7): the copies
    left behind by every rebase, toolchain update and profile or feature change, which cargo
    never removes on its own. Needs cargo-sweep (cargo install cargo-sweep).

    Every build then caps target\ at -SweepMaxSizeGB (default 50, 0 turns it off), deleting
    the least recently used build output first.

    The script lives in the checkout it builds (script\fork), so it is versioned with the
    stack and rebased along with it. -Update rewrites this file mid-run, which is safe:
    PowerShell has parsed the whole script before the first line runs.

.EXAMPLE
    .\build.ps1 -Install            # build the current branch and install it
    .\build.ps1 -Update -Install    # move to the latest stable tag, build, install
    .\build.ps1 -Install -Publish   # build, install here, ship it to every other machine
    .\build.ps1 -Update -Install -Sweep   # also drop what the old base left in target\
#>
[CmdletBinding()]
param(
    [string]$Repo = (Split-Path (Split-Path $PSScriptRoot)),
    [string]$Branch = "custom",
    [string]$InstallDir = "$env:LOCALAPPDATA\Programs\Zed",
    [switch]$Update,
    [switch]$Install,
    [switch]$Publish,
    [switch]$Sweep,
    [int]$SweepDays = 7,
    [int]$SweepMaxSizeGB = 50,
    [string]$GitHubRepo = "mr-yusuf-sal/zed",
    [int]$KeepReleases = 3
)

# No global "Stop": cargo and git log progress to stderr, and Windows PowerShell 5.1 turns
# those lines into terminating errors whenever the output is redirected. Native commands are
# checked through $LASTEXITCODE; cmdlets that must not fail get -ErrorAction Stop.

# winget installs rustup/cmake into PATH entries a running shell hasn't picked up yet.
$env:Path = [Environment]::GetEnvironmentVariable("Path", "Machine") + ";" +
            [Environment]::GetEnvironmentVariable("Path", "User")

function Invoke-Git {
    git -C $Repo @args
    if ($LASTEXITCODE -ne 0) { throw "git $($args -join ' ') failed" }
}

# Deletes target\*\incremental crate directories, oldest first: every one untouched for $Days
# days, then more until the rest fits in $BudgetBytes. cargo-sweep skips incremental\, which is
# not named by fingerprint hash, and rustc never deletes the directory of a crate hash it no
# longer builds. rustc rewrites a crate's directory on every compile, so deleting one only
# costs that crate its cache the next time it changes.
function Remove-StaleIncremental([int]$Days, [long]$BudgetBytes = [long]::MaxValue) {
    $cutoff = (Get-Date).AddDays(-$Days)
    $dirs = Get-ChildItem (Join-Path $Repo "target\*\incremental\*") -Directory -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime |
        ForEach-Object {
            $size = (Get-ChildItem $_.FullName -Recurse -File -Force -ErrorAction SilentlyContinue |
                Measure-Object Length -Sum).Sum
            [pscustomobject]@{ Dir = $_; Size = [long]$size }
        }
    $total = [long]($dirs | Measure-Object Size -Sum).Sum
    foreach ($d in $dirs) {
        if ($d.Dir.LastWriteTime -ge $cutoff -and $total -le $BudgetBytes) { break }
        Remove-Item $d.Dir.FullName -Recurse -Force -ErrorAction SilentlyContinue
        $total -= $d.Size
    }
}

if (-not (Test-Path $Repo)) {
    throw "Zed checkout not found at $Repo. Clone it with: git clone --filter=blob:none https://github.com/mr-yusuf-sal/zed `"$Repo`""
}

if ($Sweep -and -not (Get-Command cargo-sweep -ErrorAction SilentlyContinue)) {
    throw "-Sweep needs cargo-sweep. Install it with: cargo install cargo-sweep"
}

if ($Update) {
    Invoke-Git fetch upstream --tags --force
    Invoke-Git switch $Branch

    $stableTags = git -C $Repo tag --list "v*" |
        Where-Object { $_ -match '^v\d+\.\d+\.\d+$' } |
        Sort-Object { [version]$_.Substring(1) }
    $latest = $stableTags | Select-Object -Last 1
    $current = git -C $Repo describe --tags --abbrev=0 --match "v*" --exclude "*-pre" HEAD

    if ($latest -eq $current) {
        Write-Host "Already on the latest stable tag ($current)."
    } else {
        Write-Host "Rebasing $Branch from $current onto $latest..."
        git -C $Repo rebase --onto $latest $current $Branch
        if ($LASTEXITCODE -ne 0) {
            throw "Rebase hit conflicts. Resolve them in $Repo (git rebase --continue), then rerun without -Update."
        }
        Invoke-Git push --force-with-lease origin $Branch
    }
}

# Upstream's release profile (thin LTO, codegen-units = 1) ends in a long single-threaded
# LTO-then-link tail. These overrides live here rather than in the repo's Cargo.toml so the
# branch stays free of conflict surface when rebasing onto new stable tags.
$env:CARGO_PROFILE_RELEASE_LTO = "false"
$env:CARGO_PROFILE_RELEASE_DEBUG = "false"
$env:CARGO_PROFILE_RELEASE_CODEGEN_UNITS = "16"
$env:CARGO_PROFILE_RELEASE_INCREMENTAL = "true"

$env:ZED_UPDATE_EXPLANATION ="This is a custom build of the mr-yusuf-sal/zed fork. Update it with script\fork\build.ps1 -Update -Install in the fork's checkout."

if ($Publish) {
    # Checked before the build so a long build does not end in a refused publish.
    if (git -C $Repo status --porcelain) {
        throw "-Publish needs a clean tree: the release says the exe is HEAD, and a dirty build is not."
    }
    Invoke-Git fetch origin $Branch
    git -C $Repo merge-base --is-ancestor HEAD "origin/$Branch"
    if ($LASTEXITCODE -ne 0) { throw "-Publish needs HEAD pushed to origin/$Branch first: the release tag points at it." }
}

Push-Location $Repo
try {
    cargo build --release -p zed
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
} finally {
    Pop-Location
}

$built = Join-Path $Repo "target\release\zed.exe"
Write-Host "Built: $built ($(git -C $Repo describe --tags HEAD))"

if ($Sweep) {
    # cargo-sweep goes by the last access time of each .fingerprint entry. --time rather than
    # --stamp/--file: NTFS updates access times at most once an hour, so a build soon after the
    # previous one would look like it used nothing and lose its whole cache. Days also keep the
    # artifacts agents' `cargo check/test --release` build without this script's profile env.
    cargo sweep --time $SweepDays $Repo
    if ($LASTEXITCODE -ne 0) { Write-Warning "cargo sweep failed" }
    Remove-StaleIncremental $SweepDays
}

if ($SweepMaxSizeGB -gt 0) {
    if (Get-Command cargo-sweep -ErrorAction SilentlyContinue) {
        # cargo-sweep --maxsize measures all of target\ but only deletes outside incremental\,
        # so with incremental\ near the cap it would delete the build just made to fit.
        # Holding incremental\ to half the cap leaves the other half for the rest.
        Remove-StaleIncremental $SweepDays ($SweepMaxSizeGB * 1GB / 2)
        cargo sweep --maxsize "$($SweepMaxSizeGB)GB" $Repo
        if ($LASTEXITCODE -ne 0) { Write-Warning "cargo sweep failed" }
    } else {
        Write-Warning "target\ was not capped at $SweepMaxSizeGB GB: cargo-sweep is not installed (cargo install cargo-sweep)."
    }
}

if ($Publish) {
    $sha = (git -C $Repo rev-parse HEAD).Trim()
    $tag = "custom-$sha"
    $title = "$(git -C $Repo describe --tags HEAD) (fork)"
    $baseTag = git -C $Repo describe --tags --abbrev=0 --match "v*" --exclude "*-pre" HEAD
    $notes = git -C $Repo log --format="- %s" "$baseTag..HEAD" | Out-String

    $staging = Join-Path $env:TEMP "zed-fork-publish"
    Remove-Item $staging -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory $staging -ErrorAction Stop | Out-Null
    $zip = Join-Path $staging "zed-windows-x86_64.zip"
    # Windows' bundled bsdtar writes a deflate zip, which crates/fork_sync unpacks.
    tar.exe -a -c -f $zip -C (Split-Path $built) zed.exe
    if ($LASTEXITCODE -ne 0) { throw "zipping $built failed" }

    gh release view $tag --repo $GitHubRepo *> $null
    if ($LASTEXITCODE -eq 0) {
        gh release upload $tag $zip --repo $GitHubRepo --clobber
    } else {
        gh release create $tag $zip --repo $GitHubRepo --target $sha --title $title --notes $notes --latest
    }
    if ($LASTEXITCODE -ne 0) { throw "publishing $tag to $GitHubRepo failed" }
    Write-Host "Published $tag; forks on other commits install it within the hour."

    $old = gh release list --repo $GitHubRepo --limit 100 --json tagName,createdAt |
        ConvertFrom-Json |
        Where-Object { $_.tagName -like "custom-*" } |
        Sort-Object createdAt -Descending |
        Select-Object -Skip $KeepReleases
    foreach ($release in $old) {
        gh release delete $release.tagName --repo $GitHubRepo --cleanup-tag --yes
        if ($LASTEXITCODE -ne 0) { Write-Warning "Could not delete the old release $($release.tagName)" }
    }
    Remove-Item $staging -Recurse -Force -ErrorAction SilentlyContinue
}

# After publishing: stopping Zed can take this script down with it when it runs in Zed's
# terminal, which kills its processes on close.
if ($Install) {
    $target = Join-Path $InstallDir "Zed.exe"
    $official = Join-Path $InstallDir "Zed.official.exe"
    $previous = Join-Path $InstallDir "Zed.previous.exe"
    if (-not (Test-Path $target)) { throw "Official Zed install not found at $InstallDir" }

    if (-not (Test-Path $official)) {
        Copy-Item $target $official -ErrorAction Stop
        Write-Host "Kept the official exe as $official"
    }
    # Windows lets a running exe be renamed but not overwritten, so the running one is moved
    # aside instead of stopping every process that runs it.
    if (Test-Path $previous) {
        Remove-Item $previous -Force -ErrorAction SilentlyContinue
    }
    if (Test-Path $previous) {
        $previous = Join-Path $InstallDir "Zed.previous-$(Get-Date -Format yyyyMMddHHmmss).exe"
    }
    Move-Item $target $previous -ErrorAction Stop
    Copy-Item $built $target -ErrorAction Stop
    Write-Host "Installed the fork to $target"

    # Agent hosts (`zed --agent-host`) run Claude Code turns, and terminal hosts
    # (`zed --terminal-host`) run terminal shells, that must outlive Zed. They keep
    # running from the exe moved aside above, and the new Zed reconnects to them.
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
}
