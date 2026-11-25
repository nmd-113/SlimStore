param(
    [string]$Executable = (Join-Path (Split-Path -Parent $PSScriptRoot) 'dist\SlimStore.exe'),
    [string]$ReportDirectory = (Join-Path (Split-Path -Parent $PSScriptRoot) 'verification')
)
$ErrorActionPreference = 'Stop'
$verification = (New-Item -ItemType Directory -Force -Path $ReportDirectory).FullName
$exe = (Resolve-Path -LiteralPath $Executable).Path
$clock = [Diagnostics.Stopwatch]::StartNew()
$p = Start-Process -FilePath $exe -WindowStyle Hidden -PassThru -RedirectStandardError (Join-Path $verification 'benchmark-stderr.txt')
$null = $p.Handle
try {
    do {
        Start-Sleep -Milliseconds 25
        $p.Refresh()
        if ($p.HasExited) { throw 'SlimStore exited before showing a window' }
    } while ($p.MainWindowTitle -notlike 'SlimStore*' -and $clock.ElapsedMilliseconds -lt 15000)
    if ($p.MainWindowTitle -notlike 'SlimStore*') { throw 'No titled SlimStore window appeared' }
    $startup = $clock.Elapsed.TotalMilliseconds
    Start-Sleep -Seconds 3
    $p.Refresh()
    $before = $p.TotalProcessorTime.TotalMilliseconds
    Start-Sleep -Seconds 5
    $p.Refresh()
    $cpu = $p.TotalProcessorTime.TotalMilliseconds - $before
    $result = [ordered]@{
        StartupToWindowMilliseconds = [Math]::Round($startup,1)
        IdleCpuMillisecondsOverFiveSeconds = [Math]::Round($cpu,2)
        IdleCpuPercentOfOneCore = [Math]::Round($cpu / 50,3)
        WorkingSetMiB = [Math]::Round($p.WorkingSet64 / 1MB,2)
        PrivateMemoryMiB = [Math]::Round($p.PrivateMemorySize64 / 1MB,2)
        ExecutableMiB = [Math]::Round((Get-Item -LiteralPath $exe).Length / 1MB,2)
        SHA256 = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash
        WindowTitle = $p.MainWindowTitle
        Note = 'Single local sample, not a cross-machine performance guarantee; OS file cache and antivirus affect startup.'
    }
    $modules = @($p.Modules | Select-Object ModuleName,FileName)
    $modules | ConvertTo-Json | Set-Content -Encoding utf8 -LiteralPath (Join-Path $verification 'loaded-modules.json')
    $result | ConvertTo-Json | Set-Content -Encoding utf8 -LiteralPath (Join-Path $verification 'benchmark.json')
    & (Join-Path $PSScriptRoot 'capture.ps1') -ProcessId $p.Id -Destination (Join-Path $verification 'benchmark-home.png')
    [PSCustomObject]$result
} finally {
    if (-not $p.HasExited) { $p.CloseMainWindow() | Out-Null; if (-not $p.WaitForExit(15000)) { throw 'SlimStore did not close normally' } }
}
