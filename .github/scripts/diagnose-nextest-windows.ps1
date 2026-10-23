# Temporary same-runner comparison; remove after the Windows LEAK investigation.
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$env:CARGO_TERM_COLOR = 'never'
$env:NO_COLOR = '1'
$out = Join-Path $env:RUNNER_TEMP 'nextest-concurrency'
New-Item -ItemType Directory -Force $out | Out-Null
$metadata = Join-Path $out 'cargo-metadata.json'
$binaries = Join-Path $out 'binaries.json'

& cargo metadata --format-version 1 --no-deps --locked > $metadata
if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
& cargo nextest list --workspace --locked --list-type binaries-only --message-format json > $binaries
if ($LASTEXITCODE -ne 0) { throw 'test prebuild failed' }

$binaryPaths = (Get-Content $binaries -Raw | ConvertFrom-Json).'rust-binaries'.PSObject.Properties |
    ForEach-Object { $_.Value.'binary-path' }
$before = @($binaryPaths | Sort-Object | Get-FileHash -Algorithm SHA256)
$before | ConvertTo-Json | Set-Content (Join-Path $out 'binaries-before.json')
$runs = @()
$failed = $false
$index = 0

# No retries: every result contributes to the final exit status, including LEAK.
foreach ($threads in @(4, 8, 8, 4)) {
    $index++
    $label = "run$index-j$threads"
    $log = Join-Path $out "$label.log"
    $startId = "$label-start"
    $stopId = "$label-stop"
    Get-CimInstance Win32_Process |
        Select-Object ProcessId, ParentProcessId, Name, CreationDate |
        ConvertTo-Json | Set-Content (Join-Path $out "$label-processes-before.json")
    Register-CimIndicationEvent -Query 'SELECT * FROM Win32_ProcessStartTrace' -SourceIdentifier $startId | Out-Null
    Register-CimIndicationEvent -Query 'SELECT * FROM Win32_ProcessStopTrace' -SourceIdentifier $stopId | Out-Null
    try {
        Write-Host "::group::$label"
        $started = [DateTime]::UtcNow
        & cargo nextest run --cargo-metadata $metadata --binaries-metadata $binaries --test-threads $threads --no-fail-fast 2>&1 |
            Tee-Object -FilePath $log
        $code = $LASTEXITCODE
        $finished = [DateTime]::UtcNow
        $leaks = @(Select-String -Path $log -Pattern '^\s*LEAK(?:-FAIL)?\s+\[').Count
        $summary = @(Select-String -Path $log -Pattern '^\s*Summary\s+\[' | ForEach-Object { $_.Line })
        $runs += [pscustomobject]@{
            run = $index; threads = $threads; exit_code = $code; leaks = $leaks
            started = $started.ToString('o'); finished = $finished.ToString('o')
            elapsed_seconds = ($finished - $started).TotalSeconds; summary = $summary
        }
        if ($code -ne 0 -or $leaks -gt 0 -or $summary.Count -eq 0) { $failed = $true }
        Get-CimInstance Win32_Process |
            Select-Object ProcessId, ParentProcessId, Name, CreationDate |
            ConvertTo-Json | Set-Content (Join-Path $out "$label-processes-after.json")
        # Let process-event notifications drain after measurement has stopped.
        Start-Sleep -Seconds 2
    } finally {
        Unregister-Event -SourceIdentifier $startId -ErrorAction SilentlyContinue
        Unregister-Event -SourceIdentifier $stopId -ErrorAction SilentlyContinue
        $events = @(Get-Event -ErrorAction SilentlyContinue | Where-Object { $_.SourceIdentifier -in @($startId, $stopId) })
        @($events | ForEach-Object {
            $process = $_.SourceEventArgs.NewEvent
            [pscustomobject]@{
                kind = $_.SourceIdentifier; time = [string]$process.TIME_CREATED
                pid = $process.ProcessID; parent_pid = $process.ParentProcessID
                name = $process.ProcessName; exit_status = $process.ExitStatus
            }
        }) | ConvertTo-Json | Set-Content (Join-Path $out "$label-process-events.json")
        $runs | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $out 'results.json')
        # Queue entries may already be gone; cleanup must not discard recorded results.
        Remove-Event -SourceIdentifier $startId -ErrorAction SilentlyContinue
        Remove-Event -SourceIdentifier $stopId -ErrorAction SilentlyContinue
        Write-Host '::endgroup::'
    }
}

$after = @($binaryPaths | Sort-Object | Get-FileHash -Algorithm SHA256)
$after | ConvertTo-Json | Set-Content (Join-Path $out 'binaries-after.json')
if (Compare-Object $before $after -Property Path, Hash) { throw 'Test binaries changed during comparison' }
$runs | Format-Table run, threads, elapsed_seconds, leaks, exit_code
if ($failed) { exit 1 }
