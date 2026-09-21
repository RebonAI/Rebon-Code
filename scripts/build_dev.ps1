# Build a side-by-side `rebon-dev.exe` for testing without disturbing
# a running `rebon.exe`.
#
# Why a separate binary?
#   A running `rebon.exe` holds an exclusive lock on the file on
#   Windows, so a fresh `cargo build` overwriting `target\debug\rebon.exe`
#   fails while the user has the main binary running. We sidestep this
#   by building into a parallel target directory (`target-dev\`) and
#   renaming the output to `rebon-dev.exe` so two binaries can coexist.
#
# Usage:
#   pwsh scripts\build_dev.ps1                       # debug build only
#   pwsh scripts\build_dev.ps1 -Release              # release build only
#   pwsh scripts\build_dev.ps1 -Run                  # build + launch TUI
#   pwsh scripts\build_dev.ps1 -Run --acp            # build + launch ACP server
#   pwsh scripts\build_dev.ps1 -Release -Run         # release build + launch
#   pwsh scripts\build_dev.ps1 -Beta beta.1          # tag version as 0.0.76-beta.1
#   pwsh scripts\build_dev.ps1 -Beta dev -Run        # tag + run; auto-suffix when omitted
#
# `-Beta <tag>` sets REBON_BETA_TAG so `crates/rebon-cli/build.rs`
# rewrites `CARGO_PKG_VERSION` to `<workspace-version>-<tag>` for
# this build only. Pass `-Beta auto` (or `-Beta` with no value) to
# get a timestamped tag like `dev-20260503T2114`.
#
# Any unrecognized args are forwarded to the launched binary.

[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$Run,
    [string]$Beta,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$ForwardedArgs = @()
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot
try {
    $profileName = if ($Release) { 'release' } else { 'debug' }
    $targetDir = Join-Path $repoRoot 'target-dev'

    $cargoArgs = @('build', '-p', 'rebon-cli', '--target-dir', $targetDir)
    if ($Release) { $cargoArgs += '--release' }

    # Resolve the beta tag and stamp it into the build environment.
    # The build.rs in `rebon-cli` reads REBON_BETA_TAG and rewrites
    # CARGO_PKG_VERSION when set.
    $effectiveBeta = $null
    if ($PSBoundParameters.ContainsKey('Beta')) {
        if ([string]::IsNullOrWhiteSpace($Beta) -or $Beta -ieq 'auto') {
            $effectiveBeta = "dev-$(Get-Date -Format 'yyyyMMddTHHmm')"
        } else {
            $effectiveBeta = $Beta
        }
    }

    if ($effectiveBeta) {
        $env:REBON_BETA_TAG = $effectiveBeta
        Write-Host "==> REBON_BETA_TAG=$effectiveBeta (version will render as <pkg>-$effectiveBeta)" -ForegroundColor Cyan
    } else {
        # Make sure a stale env value from the parent shell doesn't
        # silently leak into this build.
        Remove-Item Env:REBON_BETA_TAG -ErrorAction SilentlyContinue
    }

    Write-Host "==> cargo $($cargoArgs -join ' ')" -ForegroundColor Cyan
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Host "==> cargo build failed (exit $LASTEXITCODE)" -ForegroundColor Red
        exit $LASTEXITCODE
    }

    $built = Join-Path $targetDir (Join-Path $profileName 'rebon.exe')
    $dev   = Join-Path $targetDir (Join-Path $profileName 'rebon-dev.exe')

    if (-not (Test-Path -LiteralPath $built)) {
        Write-Host "==> expected build output not found: $built" -ForegroundColor Red
        exit 1
    }

    # If a previous `rebon-dev.exe` is still running, Windows holds
    # an exclusive lock on it. Rename the running file aside before
    # copying — the rename-then-replace trick works even on a locked
    # exe (the OS lets you rename in place).
    if (Test-Path -LiteralPath $dev) {
        $stash = "$dev.old-$(Get-Date -Format 'yyyyMMddHHmmss')"
        try {
            Move-Item -LiteralPath $dev -Destination $stash -Force
        } catch {
            Write-Host "==> could not displace existing rebon-dev.exe: $($_.Exception.Message)" -ForegroundColor Yellow
            Write-Host "    (a stale .old-* file may need manual cleanup later)" -ForegroundColor Yellow
        }
    }

    Copy-Item -LiteralPath $built -Destination $dev -Force
    Write-Host "==> built $dev" -ForegroundColor Green

    # Best-effort cleanup of stashed .old-* files that aren't locked.
    Get-ChildItem -LiteralPath (Split-Path $dev -Parent) -Filter 'rebon-dev.exe.old-*' -ErrorAction SilentlyContinue |
        ForEach-Object {
            try { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction Stop } catch { }
        }

    if ($Run) {
        Write-Host "==> launching $dev $($ForwardedArgs -join ' ')" -ForegroundColor Cyan
        & $dev @ForwardedArgs
        exit $LASTEXITCODE
    }

    Write-Host ""
    Write-Host "Run it:" -ForegroundColor Cyan
    Write-Host "  & '$dev'"
    if ($ForwardedArgs.Count -eq 0) {
        Write-Host "  & '$dev' --acp"
    }
}
finally {
    Pop-Location
}
