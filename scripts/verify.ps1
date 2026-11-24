param(
    [string]$Executable = (Join-Path (Split-Path -Parent $PSScriptRoot) 'dist\SlimStore.exe'),
    [switch]$Live,
    [string]$ReportDirectory = (Join-Path (Split-Path -Parent $PSScriptRoot) 'verification')
)
$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path -LiteralPath $Executable).Path
$verification = (New-Item -ItemType Directory -Force -Path $ReportDirectory).FullName
$runRoot = Join-Path $verification ('portable-' + [Guid]::NewGuid().ToString('N'))
$first = Join-Path $runRoot 'First folder'
$moved = Join-Path $runRoot 'Moved folder'
New-Item -ItemType Directory -Path $first,$moved | Out-Null
$copy = Join-Path $first 'SlimStore.exe'
$destination = Join-Path $moved 'SlimStore.exe'
Copy-Item -LiteralPath $exe -Destination $copy
$checks = [System.Collections.Generic.List[string]]::new()
function Check([bool]$ok,[string]$message) { if (-not $ok) { throw $message }; $checks.Add("PASS $message"); Write-Output "PASS $message" }
function SingleFile([string]$dir) { $files = @(Get-ChildItem -LiteralPath $dir -Force); Check ($files.Count -eq 1 -and $files[0].Name -eq 'SlimStore.exe') "Single executable and no generated SlimStore files in $dir" }
function EarlyClose([string]$path) {
    Remove-Item Env:\SLIMSTORE_GUI_SMOKE,Env:\SLIMSTORE_SMOKE_REPORT,Env:\SLINT_SCALE_FACTOR -ErrorAction SilentlyContinue
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $process = Start-Process -FilePath $path -WorkingDirectory $env:SystemRoot -WindowStyle Hidden -PassThru
    $null = $process.Handle
    try {
        do {
            Start-Sleep -Milliseconds 10
            $process.Refresh()
        } while (-not $process.HasExited -and $process.MainWindowTitle -notlike 'SlimStore*' -and $clock.ElapsedMilliseconds -lt 10000)
        Check (-not $process.HasExited -and $process.MainWindowTitle -like 'SlimStore*') 'Titled SlimStore window appeared before early close'
        Check (([SlimStoreResourceCheck]::PostMessage($process.MainWindowHandle,0x0010,[IntPtr]::Zero,[IntPtr]::Zero)) -and $process.WaitForExit(15000)) 'SlimStore exits when closed as soon as its window appears'
        Check ($process.ExitCode -eq 0) 'Early close exits successfully'
    } finally {
        if (-not $process.HasExited) { $process.Kill(); $process.WaitForExit() }
    }
}
function Smoke([string]$path,[string]$mode,[string]$label,[string]$scale = '1') {
    $env:SLIMSTORE_GUI_SMOKE = $mode
    $env:SLIMSTORE_SMOKE_REPORT = Join-Path $verification ("smoke-$label.txt")
    $env:SLINT_SCALE_FACTOR = $scale
    if (Test-Path -LiteralPath $env:SLIMSTORE_SMOKE_REPORT) { Remove-Item -LiteralPath $env:SLIMSTORE_SMOKE_REPORT }
    $process = Start-Process -FilePath $path -WorkingDirectory $env:SystemRoot -WindowStyle Hidden -PassThru -RedirectStandardError (Join-Path $verification "stderr-$label.txt")
    $null = $process.Handle
    if (-not $process.WaitForExit(100000)) {
        try {
            if (-not $process.HasExited) { $process.Kill(); $null = $process.WaitForExit(5000) }
        } finally {
            throw "GUI smoke test timed out: $label. See stderr-$label.txt and smoke-$label.txt."
        }
    }
    $process.WaitForExit()
    Check ($process.ExitCode -eq 0) "GUI process exited successfully: $label"
    $report = Get-Content -LiteralPath $env:SLIMSTORE_SMOKE_REPORT -Raw
    Check ($report.Contains('PASS GUI SMOKE COMPLETE') -and -not $report.Contains('FAIL')) "GUI assertions passed: $label"
    Check ([string]::IsNullOrWhiteSpace((Get-Content -LiteralPath (Join-Path $verification "stderr-$label.txt") -Raw))) "No runtime warnings or errors: $label"
}
try {
    $bytes = [IO.File]::ReadAllBytes($exe)
    $pe = [BitConverter]::ToInt32($bytes,0x3c)
    Check ([BitConverter]::ToUInt16($bytes,$pe+4) -eq 0x8664) 'PE architecture is x64'
    Check ([BitConverter]::ToUInt16($bytes,$pe+24+68) -eq 2) 'PE subsystem is Windows GUI (no console)'
    $version = [Diagnostics.FileVersionInfo]::GetVersionInfo($exe)
    $manifest = [xml](Get-Content -LiteralPath (Join-Path $PSScriptRoot '..\assets\SlimStore.manifest') -Raw)
    $expected = $manifest.assembly.assemblyIdentity.version
    $fixedVersion = "$($version.FileMajorPart).$($version.FileMinorPart).$($version.FileBuildPart).$($version.FilePrivatePart)"
    Check ($version.ProductName -eq 'SlimStore' -and $version.FileVersion -eq $expected -and $version.ProductVersion -eq $expected -and $fixedVersion -eq $expected) 'Embedded executable product and version resources'
    Add-Type @'
using System;
using System.Runtime.InteropServices;
public class SlimStoreResourceCheck {
 [DllImport("shell32.dll",CharSet=CharSet.Unicode)] public static extern uint ExtractIconEx(string file,int index,IntPtr large,IntPtr small,uint count);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] public static extern bool PostMessage(IntPtr window,uint message,IntPtr wParam,IntPtr lParam);
}
'@
    Check ([SlimStoreResourceCheck]::ExtractIconEx($exe,-1,[IntPtr]::Zero,[IntPtr]::Zero,0) -gt 0) 'Embedded Windows executable icon'
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    $dumpbin = @(& $vswhere -latest -products '*' -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe') | Select-Object -First 1
    if (-not $dumpbin) { throw 'dumpbin.exe is required to verify DLL imports. Run from a Visual Studio C++ build environment.' }
    $imports = & $dumpbin /nologo /dependents $exe | Out-String
    [IO.File]::WriteAllText((Join-Path $verification 'imports.txt'),$imports)
    Check (-not [regex]::IsMatch($imports,'(?i)(vcruntime\d*|msvcp\d+|ucrtbased|slint|Qt\d*|WebView2Loader|libgcc|libstdc\+\+).*\.dll')) 'No Visual C++ redistributable, Slint, Qt, browser, or GCC runtime DLL imports'
    $dlls = @([regex]::Matches($imports,'(?im)^\s+([a-z0-9._-]+\.dll)\s*$') | ForEach-Object {$_.Groups[1].Value})
    Check ($dlls.Count -gt 0) 'Runtime imports were inspected'
    foreach ($dll in $dlls) { Check ((Test-Path -LiteralPath (Join-Path $env:SystemRoot "System32\$dll")) -or $dll.StartsWith('api-ms-')) "Normal Windows system import: $dll" }
    SingleFile $first
    Smoke $copy 'basic' 'clean-folder'
    EarlyClose $copy
    SingleFile $first
    $originalAcl = Get-Acl -LiteralPath $first
    $readOnlyAcl = Get-Acl -LiteralPath $first
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User
    $deny = [Security.AccessControl.FileSystemAccessRule]::new($sid,[Security.AccessControl.FileSystemRights]::Write,[Security.AccessControl.InheritanceFlags]'ContainerInherit,ObjectInherit',[Security.AccessControl.PropagationFlags]::None,[Security.AccessControl.AccessControlType]::Deny)
    try {
        $readOnlyAcl.AddAccessRule($deny)
        Set-Acl -LiteralPath $first -AclObject $readOnlyAcl
        $blocked = $false
        try { [IO.File]::WriteAllText((Join-Path $first 'write-probe.txt'),'probe') } catch [System.UnauthorizedAccessException] { $blocked = $true }
        Check $blocked 'Test folder actually denies file writes'
        Smoke $copy 'basic' 'read-only-folder'
        SingleFile $first
    } finally { Set-Acl -LiteralPath $first -AclObject $originalAcl }
    # Both resolved paths are inside this explicitly created verification run; move only this file.
    Move-Item -LiteralPath $copy -Destination $destination
    SingleFile $moved
    Smoke $destination 'basic' 'moved-high-dpi' '1.5'
    SingleFile $moved
    $env:SLIMSTORE_TEST_MISSING_WINGET = '1'
    Smoke $destination 'missing' 'missing-winget'
    Remove-Item Env:\SLIMSTORE_TEST_MISSING_WINGET
    SingleFile $moved
    Smoke $destination 'geometry' 'responsive-sizes'
    Smoke $destination 'dpi' 'dpi-100' '1'
    Smoke $destination 'dpi' 'dpi-125' '1.25'
    Smoke $destination 'dpi' 'dpi-150' '1.5'
    Smoke $destination 'dpi' 'dpi-200' '2'
    if ($Live) {
        Smoke $destination 'live' 'live-winget'
        Smoke $destination 'scoop-search' 'scoop-fallback'
        $env:SLIMSTORE_TEST_MISSING_WINGET = '1'
        Smoke $destination 'scoop-no-winget' 'scoop-without-winget'
        Remove-Item Env:\SLIMSTORE_TEST_MISSING_WINGET
        SingleFile $moved
    }
    $report = (@("SlimStore independent release verification", "Executable: $exe", "SHA256: $((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash)", "Authenticode: $((Get-AuthenticodeSignature -LiteralPath $exe).Status)", "Bytes: $($bytes.Length)", "") + $checks.ToArray()) -join [Environment]::NewLine
    [IO.File]::WriteAllText((Join-Path $verification 'release-report.txt'),$report)
    Write-Output "Report: $(Join-Path $verification 'release-report.txt'). System-changing tests and other OS checks are separate."
} finally {
    Remove-Item Env:\SLIMSTORE_GUI_SMOKE,Env:\SLIMSTORE_SMOKE_REPORT,Env:\SLIMSTORE_TEST_MISSING_WINGET,Env:\SLINT_SCALE_FACTOR -ErrorAction SilentlyContinue
    # Remove only the exact copied executable and empty folders created for this run.
    foreach ($file in @($copy,$destination)) {
        if (Test-Path -LiteralPath $file -PathType Leaf) { Remove-Item -LiteralPath $file }
    }
    foreach ($dir in @($first,$moved,$runRoot)) {
        if ((Test-Path -LiteralPath $dir -PathType Container) -and -not (Get-ChildItem -LiteralPath $dir -Force | Select-Object -First 1)) {
            Remove-Item -LiteralPath $dir
        }
    }
}
