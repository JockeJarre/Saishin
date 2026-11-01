<#
.SYNOPSIS
    Bump the version, commit it and tag it, so the release publishes itself.

.DESCRIPTION
    Cargo.toml is the only place a version is written: build.rs stamps it into
    the binary (saishin -V), CI names artifacts after it, and the release
    workflow refuses a tag that does not match it. This script keeps the tag
    and Cargo.toml in step in one command.

    After it runs, publish with:  git push main --follow-tags

.EXAMPLE
    pwsh scripts/release.ps1 1.1.0
    pwsh scripts/release.ps1 1.1.0-rc1
#>
param(
    [Parameter(Mandatory = true, Position = 0)]
    [string] $Version
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$manifest = Join-Path $root 'Cargo.toml'

# The first "version = " line is the package version in [package].
function Get-ManifestVersion {
    (Get-Content $manifest | Where-Object { $_ -match '^version = ' } | Select-Object -First 1) `
        -replace '.*"(.*)".*', '$1'
}

$current = Get-ManifestVersion

if ($Version -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z][0-9A-Za-z.-]*)?$') {
    throw "'$Version' is not a version. Use x.y.z, optionally with -prerelease (current: $current)."
}

$dirty = git status --porcelain --untracked-files=no
if ($dirty) {
    throw "The tree has uncommitted changes. Commit them first.`n$dirty"
}

Write-Host "Version: $current -> $Version"

# Only the package version line starts with "version = ".
(Get-Content $manifest) -replace '^version = .*$', "version = `"$Version`"" | Set-Content $manifest
if ((Get-ManifestVersion) -ne $Version) {
    throw 'Cargo.toml was not updated as expected.'
}

# Refresh the package's own entry in Cargo.lock, offline: dependencies stay pinned.
& cargo metadata --format-version 1 --offline | Out-Null

git add Cargo.toml Cargo.lock
git commit -m "Release v$Version"
git tag -a "v$Version" -m "saishin $Version"

Write-Host "Committed and tagged v$Version."
Write-Host 'Publish with: git push main --follow-tags'
