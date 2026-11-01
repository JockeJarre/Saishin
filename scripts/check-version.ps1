# Checks that a Windows binary carries the version resource build.rs stamps, and
# fails otherwise. Used by the CI jobs that ship a binary. A missing version
# resource is invisible in a terminal and read everywhere else: the file's
# Properties window, Explorer's Version columns, an installer deciding which of
# two files is the newer one, and the "which build is this file" question asked
# months later with no terminal in sight.
#
#   ./scripts/check-version.ps1 dist/saishin.exe -Version 1.0.0 -Sha 16746ae
#
# The numbers have to be the version in Cargo.toml and the sha build.rs stamped
# from GITHUB_SHA, because a Properties window disagreeing with the -V line the
# same job printed is worse than one saying nothing.
#
# System.Diagnostics.FileVersionInfo, which is what Get-Item's .VersionInfo is,
# reads the version resource of the file and reports the four numbers as 0 with
# an empty string when the file has no such resource: exactly the answer this
# check is there to catch.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)][string] $Path,
    [Parameter(Mandatory = $true)][string] $Version,
    [Parameter(Mandatory = $true)][string] $Sha
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
    Write-Output "::error::No such file: $Path"
    exit 1
}

$info = (Get-Item -LiteralPath $Path).VersionInfo

if (-not $info.FileVersion -and -not $info.FileMajorPart -and -not $info.FileMinorPart) {
    Write-Output "::error::$Path carries no version resource"
    exit 1
}

# The numbers as the resource stores them: major and minor in the first pair,
# build and private in the second. A version with a suffix ("1.2.0-rc1") is cut
# the way build.rs cuts it, and the parts a short version leaves out are 0.
$parts = ($Version -split '[-+]')[0].Split('.')
$wanted = @(0, 0, 0, 0)
for ($i = 0; $i -lt 4 -and $i -lt $parts.Count; $i++) { $wanted[$i] = [int]$parts[$i] }
$found = @($info.FileMajorPart, $info.FileMinorPart, $info.FileBuildPart, $info.FilePrivatePart)

# All four places the version is written: the string the Properties window
# prints, the numbers an installer compares, the product string that carries the
# sha, and the description that names which of the two shipped files this is.
if ($info.FileVersion -ne $Version) {
    Write-Output "::error::$Path has FileVersion '$($info.FileVersion)', expected '$Version'"
    exit 1
}
if (($found -join '.') -ne ($wanted -join '.')) {
    Write-Output "::error::$Path is numbered $($found -join '.'), expected $($wanted -join '.')"
    exit 1
}
if (-not $info.ProductVersion -or -not $info.ProductVersion.StartsWith($Version) `
    -or -not $info.ProductVersion.Contains($Sha)) {
    Write-Output "::error::$Path has ProductVersion '$($info.ProductVersion)', expected '$Version' and the sha '$Sha' in it"
    exit 1
}
if (-not $info.FileDescription) {
    Write-Output "::error::$Path has no file description, so the Properties window would not say which file it is"
    exit 1
}

Write-Output "${Path}: version $($info.FileVersion) ($($found -join '.')), product '$($info.ProductVersion)', description '$($info.FileDescription)', original '$($info.OriginalFilename)'"
