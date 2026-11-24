$ErrorActionPreference = 'Stop'
$workspace = Split-Path -Parent $PSScriptRoot
$executable = Join-Path $workspace 'dist\SlimStore.exe'
$version = [Diagnostics.FileVersionInfo]::GetVersionInfo($executable)
$cargo = Get-Content -LiteralPath (Join-Path $workspace 'Cargo.toml') -Raw
$match = [regex]::Match($cargo, '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"')
if (-not $match.Success) { throw 'Cargo.toml must specify a stable SlimStore version.' }
$tagVersion = $match.Groups[1].Value
$windowsVersion = "$tagVersion.0"
$fixedVersion = "$($version.FileMajorPart).$($version.FileMinorPart).$($version.FileBuildPart).$($version.FilePrivatePart)"
$manifest = [xml](Get-Content -LiteralPath (Join-Path $workspace 'assets\SlimStore.manifest') -Raw)
if ($version.ProductName -ne 'SlimStore' -or $version.FileVersion -ne $windowsVersion -or
    $version.ProductVersion -ne $windowsVersion -or $fixedVersion -ne $windowsVersion -or
    $manifest.assembly.assemblyIdentity.version -ne $windowsVersion) {
    throw 'Build SlimStore and check that the executable and manifest match Cargo.toml before packaging.'
}

$release = (New-Item -ItemType Directory -Force -Path (Join-Path $workspace 'release')).FullName
$source = Join-Path $release "SlimStore-$tagVersion-source.zip"
$temporary = Join-Path $release ('source-' + [Guid]::NewGuid().ToString('N') + '.zip')
$paths = @(
    '.cargo', '.github', '.gitignore', 'Cargo.toml', 'Cargo.lock', 'build.rs', 'SlimStore-icon.ico',
    'LICENSE', 'README.md', 'CHANGELOG.md', 'assets', 'catalog', 'scripts', 'src', 'ui',
    'docs/ARTWORK.md', 'docs/THIRD-PARTY.md', 'docs/TECHNICAL-DECISIONS.md', 'docs/RELEASE.md'
)
try {
    & tar.exe -a -cf $temporary -C $workspace @paths
    if ($LASTEXITCODE -ne 0) { throw 'Source archive creation failed.' }
    Move-Item -LiteralPath $temporary -Destination $source -Force
} finally {
    if (Test-Path -LiteralPath $temporary -PathType Leaf) { Remove-Item -LiteralPath $temporary }
}

$checksums = Join-Path $release 'SHA256SUMS.txt'
$lines = foreach ($path in @($executable, $source)) {
    $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $(Split-Path -Leaf $path)"
}
$lines | Set-Content -LiteralPath $checksums -Encoding ASCII
Get-Item -LiteralPath $executable,$source,$checksums | Select-Object FullName,Length
