param(
    [switch]$SkipTests,
    [ValidatePattern('^[0-9a-fA-F]{40}$')][string]$SigningThumbprint,
    [uri]$TimestampUrl
)
$ErrorActionPreference = 'Stop'
$workspace = Split-Path -Parent $PSScriptRoot
$staged = $null
Push-Location $workspace
try {
    if ([bool]$SigningThumbprint -ne [bool]$TimestampUrl) { throw 'Supply both a code-signing certificate thumbprint and its RFC 3161 timestamp URL.' }
    if ($SigningThumbprint) {
        if (-not $TimestampUrl.IsAbsoluteUri -or $TimestampUrl.Scheme -notin @('http','https') -or $TimestampUrl.UserInfo) { throw 'Specify an absolute HTTP(S) timestamp URL without credentials.' }
        $certificate = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert | Where-Object { $_.Thumbprint -eq $SigningThumbprint -and $_.HasPrivateKey -and $_.NotBefore -le (Get-Date) -and $_.NotAfter -gt (Get-Date) }
        if (-not $certificate) { throw 'The requested valid code-signing certificate and its private key must be present in the current user certificate store.' }
        $signTool = (Get-Command 'signtool.exe' -CommandType Application -ErrorAction Stop).Source
    }
    $toolchain = Join-Path $workspace '.tools'
    if (-not (Test-Path -LiteralPath (Join-Path $toolchain 'cargo\bin\cargo.exe')) -and -not (Get-Command cargo -ErrorAction SilentlyContinue) -and $env:LOCALAPPDATA) {
        $toolchain = Join-Path $env:LOCALAPPDATA 'SlimStore-dev\toolchain'
    }
    if (Test-Path -LiteralPath (Join-Path $toolchain 'cargo\bin\cargo.exe')) {
        $env:CARGO_HOME = Join-Path $toolchain 'cargo'
        $env:RUSTUP_HOME = Join-Path $toolchain 'rustup'
        $env:Path = "$env:CARGO_HOME\bin;$env:Path"
    }
    & cargo fmt --check
    if ($LASTEXITCODE -ne 0) { throw 'Rust formatting failed' }
    if (-not $SkipTests) {
        & cargo test --locked
        if ($LASTEXITCODE -ne 0) { throw 'Automated tests failed' }
    }
    & cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'Release compilation failed' }
    $dist = Join-Path $workspace 'dist'
    New-Item -ItemType Directory -Force -Path $dist | Out-Null
    if ((Get-Item -LiteralPath $dist -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw 'The dist folder must be a regular directory.'
    }
    $extra = @(Get-ChildItem -LiteralPath $dist -Force | Where-Object {
        ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
        ($_.PSIsContainer -and $_.Name -ne 'SlimStore-data') -or
        (-not $_.PSIsContainer -and $_.Name -ne 'SlimStore.exe')
    })
    if ($extra.Count -gt 0) { throw 'dist can contain SlimStore.exe and the optional SlimStore-data folder. Move unexpected entries out before building.' }
    $targetDirectory = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $workspace 'target' }
    $built = Join-Path $targetDirectory 'x86_64-pc-windows-msvc\release\SlimStore.exe'
    if ($SigningThumbprint) {
        $staged = Join-Path ([IO.Path]::GetTempPath()) ('SlimStore-' + [Guid]::NewGuid().ToString('N') + '.exe')
        Copy-Item -LiteralPath $built -Destination $staged
        & $signTool sign /sha1 $SigningThumbprint /fd SHA256 /tr $TimestampUrl.AbsoluteUri /td SHA256 $staged
        if ($LASTEXITCODE -ne 0) { throw 'Signing or timestamping the release executable failed.' }
        & $signTool verify /pa /v $staged
        if ($LASTEXITCODE -ne 0) { throw 'The release signature failed Authenticode verification.' }
        $built = $staged
    }
    $destination = Join-Path $dist 'SlimStore.exe'
    for ($attempt = 0; $attempt -lt 5; $attempt++) {
        try {
            Copy-Item -LiteralPath $built -Destination $destination -Force
            break
        } catch {
            if ($attempt -eq 4) { throw }
            Start-Sleep -Milliseconds 500
        }
    }
    Get-Item -LiteralPath $destination | Select-Object FullName,Length,@{Name='Version';Expression={$_.VersionInfo.FileVersion}}
    Write-Output "Authenticode $((Get-AuthenticodeSignature -LiteralPath $destination).Status)"
    Write-Output "SHA256 $((Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash)"
    Write-Output 'Run scripts\verify.ps1 for independent portability and GUI checks.'
} finally {
    if ($staged -and (Test-Path -LiteralPath $staged)) { Remove-Item -LiteralPath $staged }
    Pop-Location
}
