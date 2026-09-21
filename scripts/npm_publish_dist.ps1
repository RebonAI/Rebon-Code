# Windows counterpart of npm_publish_dist.sh: publish every @rebon/cli tarball
# in dist/npm to the public npm registry, with a watchdog for npm publish's
# post-success lingering bug and for network stalls. Used by the
# publish-windows job in release.yml.
#
# Authentication is npm trusted publishing (OIDC). There is no token and no
# .npmrc: npm reads an `_authToken` as an instruction to authenticate as that
# token's owner, and having done so it will not attempt a trusted publish.
# The job supplies `permissions: id-token: write` instead, and npm exchanges
# that for a short-lived credential the registry checks against the trusted
# publisher configured on each package. See the .sh for the whole of it.
$ErrorActionPreference = 'Stop'

# A 36 MB payload over a slow residential uplink can legitimately take
# minutes; the old 180s watchdog killed a mid-flight win32 upload on v0.1.4.
# Keep the watchdog (npm's lingering keep-alive bug is real) but give the
# upload real room, and retry once. If npm times out after the registry
# accepted the upload, a registry lookup resolves the attempt as successful.
function Publish-One([string] $tarball) {
    try {
        Publish-Attempt $tarball
        return
    } catch {
        Write-Host "[retry] ${tarball}: first publish attempt failed; retrying once"
        Start-Sleep -Seconds 5
    }
    Publish-Attempt $tarball
}

function Get-PackageSpec([string] $tarball) {
    $json = (& tar -xOf $tarball package/package.json 2>$null | Out-String)
    if (-not $json.Trim()) {
        $json = (& tar -xOf $tarball package.json 2>$null | Out-String)
    }
    if (-not $json.Trim()) {
        throw "could not read package.json from $tarball"
    }

    $pkg = $json | ConvertFrom-Json
    if (-not $pkg.name -or -not $pkg.version) {
        throw "package.json in $tarball is missing name or version"
    }

    [PSCustomObject]@{
        Name = [string] $pkg.name
        Version = [string] $pkg.version
        Spec = "$($pkg.name)@$($pkg.version)"
    }
}

function Test-PublishedVersion([string] $tarball) {
    try {
        $pkg = Get-PackageSpec $tarball
        $version = (& npm view $pkg.Spec version --registry=https://registry.npmjs.org/ 2>$null | Out-String).Trim()
        if ($LASTEXITCODE -eq 0 -and $version -eq $pkg.Version) {
            Write-Host "skip: $($pkg.Spec) already published"
            return $true
        }
    } catch {
        Write-Host "[verify] ${tarball}: could not confirm registry state: $_"
    }
    return $false
}

function Publish-Attempt([string] $tarball) {
    # npm publish runs in a background job so the watchdog can bound it. The
    # `+ @rebon/...` success line is npm's authoritative "registry accepted"
    # marker; once we see it we stop waiting for the (possibly lingering)
    # process to exit on its own.
    $job = Start-Job -ScriptBlock {
        param($tb)
        npm publish $tb --tag latest --access public `
            --registry=https://registry.npmjs.org/ `
            --fetch-timeout=90000 --fetch-retries=2 2>&1
    } -ArgumentList $tarball

    $hardTimeout = 480
    # Wide enough to cover what still happens after the success line: a
    # trusted publish signs the provenance attestation and uploads it once the
    # tarball is accepted, so killing at the old five seconds could land
    # between the two and leave a published version with no attestation.
    $grace = 30
    $start = Get-Date
    $sawSuccessAt = $null
    while ($job.State -eq 'Running') {
        Start-Sleep -Seconds 1
        $log = (Receive-Job -Id $job.Id -Keep | Out-String)
        if (-not $sawSuccessAt -and $log -match '(?m)^\+ @rebon/') { $sawSuccessAt = Get-Date }
        if ($sawSuccessAt -and ((Get-Date) - $sawSuccessAt).TotalSeconds -ge $grace) {
            Write-Host "[watchdog] ${tarball}: success printed but npm did not exit; killing"
            break
        }
        if (((Get-Date) - $start).TotalSeconds -ge $hardTimeout) {
            Write-Host "[watchdog] ${tarball}: ${hardTimeout}s without success; killing"
            break
        }
    }

    $log = (Receive-Job -Id $job.Id -Keep | Out-String)
    Write-Host $log
    Stop-Job $job -ErrorAction SilentlyContinue
    Remove-Job $job -Force -ErrorAction SilentlyContinue

    if ($log -match '(?m)^\+ @rebon/') { return }
    if ($log -match 'cannot publish over') { Write-Host "skip: $tarball already published"; return }
    if (Test-PublishedVersion $tarball) { return }
    throw "publish failed for $tarball"
}

# No `npm whoami`: there is no user to be. A trusted publish authenticates per
# publish, so the first thing that proves the credential works is the publish.
#
# rebon-*.tgz covers @rebon/cli, the platform packages and the bare alias.
# dist/npm is wiped before every build, so the broad glob only sees this job's
# own output.
$tarballs = Get-ChildItem 'dist/npm/rebon-*.tgz' -File
if (-not $tarballs) { throw 'no rebon-*.tgz in dist/npm' }
foreach ($t in $tarballs) { Publish-One $t.FullName }
