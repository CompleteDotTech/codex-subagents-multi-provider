# Numeric-only collector. Called by the bounded supervisor, never directly by CI.
param([int]$OwnedPid = 0, [double]$BuildStart = 0)
$ErrorActionPreference = 'Stop'
$result = [ordered]@{ unavailable = 0; truncated = $false; memory = $null; pagefiles = @(); configured_pagefiles = $null; volumes = @(); processes = @(); process_scope = 'live-owned-descendant-snapshot-only-detached-servers-unobserved'; vhd = $null }
function Number($Value, [long]$Scale = 1) { if ($null -eq $Value) { return $null }; return [long]$Value * $Scale }
function Query($Class, $Filter) {
    $options = @{ ClassName = $Class; OperationTimeoutSec = 8; ErrorAction = 'Stop' }
    if ($Filter) { $options.Filter = $Filter }
    Get-CimInstance @options
}
try {
    $m = Query Win32_PerfFormattedData_PerfOS_Memory $null
    foreach ($field in @('AvailableBytes','CommittedBytes','CommitLimit','PercentCommittedBytesInUse')) { if ($null -eq $m.$field) { throw 'memory unavailable' } }
    $result.memory = @{ available_bytes = [long]$m.AvailableBytes; committed_bytes = [long]$m.CommittedBytes; commit_limit_bytes = [long]$m.CommitLimit; percent_commit = [long]$m.PercentCommittedBytesInUse; commit_peak_bytes = $null }
} catch { $result.unavailable++ }
try {
    $rows = @(Query Win32_PageFileUsage $null)
    if ($rows.Count -gt 16) { $result.truncated = $true }
    $result.pagefiles = @($rows | Select-Object -First 16 | ForEach-Object { @{ volume = [IO.Path]::GetPathRoot($_.Name); allocated_bytes = (Number $_.AllocatedBaseSize 1MB); current_bytes = (Number $_.CurrentUsage 1MB); peak_bytes = (Number $_.PeakUsage 1MB) } })
} catch { $result.unavailable++ }
try {
    $settings = @(Query Win32_PageFileSetting $null)
    if ($settings.Count -gt 16) { $result.truncated = $true }
    $result.configured_pagefiles = @($settings | Select-Object -First 16 | ForEach-Object {
        @{ volume = [IO.Path]::GetPathRoot($_.Name); initial_bytes = $(if ($null -eq $_.InitialSize) { $null } else { [long]$_.InitialSize * 1MB }); maximum_bytes = $(if ($null -eq $_.MaximumSize) { $null } else { [long]$_.MaximumSize * 1MB }) }
    })
} catch { $result.unavailable++ }
$roots = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
[void]$roots.Add('C:')
foreach ($name in @('CI_BUILD_ROOT','BAZEL_OUTPUT_BASE','BAZEL_OUTPUT_USER_ROOT','BAZEL_REPOSITORY_CACHE','BAZEL_REPO_CONTENTS_CACHE','CARGO_TARGET_DIR','RUNNER_TEMP','TEMP','TMP','VOICE_WINDOWS_BAZEL_REPOSITORY')) {
    $path = [Environment]::GetEnvironmentVariable($name)
    if ($path -and $path -match '^[A-Za-z]:[\\/]') { [void]$roots.Add([IO.Path]::GetPathRoot($path).TrimEnd('\')) }
}
if ($roots.Count -gt 16) { $result.truncated = $true }
foreach ($root in @($roots | Sort-Object | Select-Object -First 16)) {
    try {
        if ($root -notmatch '^[A-Za-z]:$') { throw 'unsupported volume' }
        $d = Query Win32_LogicalDisk "DeviceID='$root'"
        if ($null -eq $d -or $null -eq $d.FreeSpace -or $null -eq $d.Size) { throw 'unavailable volume' }
        $result.volumes += @{ volume = $root; free_bytes = [long]$d.FreeSpace; total_bytes = [long]$d.Size }
    } catch { $result.unavailable++; $result.volumes += @{ volume = $root; free_bytes = $null; total_bytes = $null } }
}
try {
    if ($env:RUNNER_TEMP) {
        $vhd = Join-Path $env:RUNNER_TEMP 'codex-dev-drive.vhdx'
        if (Test-Path -LiteralPath $vhd -PathType Leaf) {
            $result.vhd = @{ file_length_bytes = (Get-Item -LiteralPath $vhd).Length; logical_size_bytes = 68719476736; logical_size_source = 'setup-dev-drive.ps1'; backing_volume = [IO.Path]::GetPathRoot($vhd); allocated_physical_bytes = $null }
        }
    }
} catch { $result.unavailable++ }
# Query only descendants of the explicitly owned build, never all host processes.
if ($OwnedPid -gt 0) {
    $queue = [Collections.Generic.Queue[int]]::new()
    $seen = [Collections.Generic.HashSet[int]]::new()
    try {
        $root = Query Win32_Process "ProcessId=$OwnedPid"
        $created = ([DateTimeOffset]$root.CreationDate).ToUnixTimeMilliseconds() / 1000.0
        if (-not $root -or [Math]::Abs($created - $BuildStart) -gt 2) { throw 'ownership unavailable' }
    } catch { $result.unavailable++; $result | ConvertTo-Json -Depth 5 -Compress; exit 0 }
    $creation = @{ $OwnedPid = $root.CreationDate }
    $queue.Enqueue($OwnedPid)
    while ($queue.Count -gt 0 -and $seen.Count -lt 128) {
        $parent = $queue.Dequeue()
        if (-not $seen.Add($parent)) { continue }
        try {
            $current = Query Win32_Process "ProcessId=$parent"
            if (-not $current -or $current.CreationDate -ne $creation[$parent]) { $result.unavailable++; continue }
            foreach ($p in @(Query Win32_Process "ParentProcessId=$parent")) {
                if ($seen.Count + $queue.Count -ge 128) { $result.truncated = $true; break }
                # A reused PID older than this build cannot be an owned child.
                if (([DateTimeOffset]$p.CreationDate).ToUnixTimeMilliseconds() / 1000.0 -lt $created) { $result.unavailable++; continue }
                $creation[[int]$p.ProcessId] = $p.CreationDate
                $queue.Enqueue([int]$p.ProcessId)
                if ($p.Name -match '^(rustc|link|cl|ninja|cmake|python|python3|bazel|bash)(\.exe)?$') {
                    $result.processes += @{ image = $p.Name.ToLowerInvariant(); working_bytes = (Number $p.WorkingSetSize); private_bytes = (Number $p.PrivatePageCount) }
                }
            }
        } catch { $result.unavailable++ }
    }
    if ($queue.Count) { $result.truncated = $true }
}
$result | ConvertTo-Json -Depth 5 -Compress
