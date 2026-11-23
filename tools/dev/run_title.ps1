# Stage a read-only run overlay for an Xbox Series X|S title and launch it
# under the emulator's debug tracer.
#
# Title-agnostic: no path below belongs to one machine. Point -GameDir at a dump
# and -EmuDir at a build output directory (target
elease); it hardlinks the dump
# into a mirror overlay, copies the emulator's DLLs next to the exe and runs it
# under mdbg with a time limit.
#
# The dump stays untouched: every game file is hardlinked into the overlay
# (same bytes, no copy, no extra disk), and only the emulator's own DLLs are
# copied in. A hardlink shares its bytes with the dump, so a write through the
# overlay would reach the dump: treat the overlay as read-only, and re-run this
# with a fresh -Overlay if the dump must stay provably pristine (compare the
# dump's file sizes and mtimes before and after).
param(
  [Parameter(Mandatory)][string]$GameDir,
  [Parameter(Mandatory)][string]$EmuDir,
  [string]$Overlay,
  [string]$Exe = 'game_scarlett_master.exe',
  [string]$Staged = '',
  [int]$TimeoutSec = 180
)

$ErrorActionPreference = 'Stop'
$GameDir = (Resolve-Path -LiteralPath $GameDir).Path
$EmuDir = (Resolve-Path -LiteralPath $EmuDir).Path
if (-not $Overlay) { $Overlay = Join-Path (Split-Path $GameDir -Parent) "run-overlay-$(Split-Path $GameDir -Leaf)" }
if (-not (Test-Path -LiteralPath $Overlay)) { New-Item -ItemType Directory -Path $Overlay | Out-Null }

# Directories that must never be linked into a run overlay: the emulator
# checkout lives inside the game folder and is not game data.
$Skip = @('pc-compat', '.freebuff', '.git')

function Link-Tree([string]$Src, [string]$Dst) {
  foreach ($f in Get-ChildItem -LiteralPath $Src -File -Force) {
    New-Item -ItemType HardLink -Path (Join-Path $Dst $f.Name) -Target $f.FullName | Out-Null
  }
  foreach ($d in Get-ChildItem -LiteralPath $Src -Directory -Force) {
    if ($Skip -contains $d.Name) { continue }
    $sub = Join-Path $Dst $d.Name
    New-Item -ItemType Directory -Path $sub | Out-Null
    Link-Tree $d.FullName $sub
  }
}

$target = Join-Path $Overlay $Exe
if (-not (Test-Path -LiteralPath $target)) { Link-Tree $GameDir $Overlay }
if (-not (Test-Path -LiteralPath $target)) { throw "the overlay has no $Exe" }

$StagedList = @($Staged -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ })

# The emulator's own DLLs go next to the exe, which is where Windows resolves
# the console library names from. Nothing else in the dump is touched.
$stagedOut = @()
foreach ($dll in $StagedList) {
  $src = Join-Path $EmuDir $dll
  if (-not (Test-Path -LiteralPath $src)) { throw "emulator DLL not found: $src" }
  Copy-Item -LiteralPath $src -Destination (Join-Path $Overlay $dll) -Force
  $stagedOut += $dll
}

# Media Foundation forwarder: the emulator's mfplat loads the PC's real one
# from mfplat_sys.dll next to itself (see docs/DEVELOPMENT_LOG.md).
$mfSys = Join-Path $Overlay 'mfplat_sys.dll'
if ((Test-Path (Join-Path $Overlay 'mfplat.dll')) -and -not (Test-Path $mfSys)) {
  Copy-Item -LiteralPath (Join-Path $env:WINDIR 'System32\mfplat.dll') -Destination $mfSys -Force
  $stagedOut += 'mfplat_sys.dll'
}

[pscustomobject]@{
  Overlay = $Overlay
  Exe = $target
  Staged = ($stagedOut -join ', ')
} | Format-List | Out-String | Write-Host

$mdbg = Join-Path $EmuDir 'mdbg.exe'
$log = Join-Path $Overlay 'run.log'
$psi = New-Object System.Diagnostics.ProcessStartInfo
if (Test-Path -LiteralPath $mdbg) {
  $psi.FileName = $mdbg
  $psi.Arguments = "`"$target`""
  $psi.WorkingDirectory = $Overlay
} else {
  $psi.FileName = $target
  $psi.WorkingDirectory = $Overlay
}
$psi.UseShellExecute = $false
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.CreateNoWindow = $true
$env:MDBG_LIMIT = "$TimeoutSec"

Write-Host "launch: $($psi.FileName) (cwd $Overlay, limit ${TimeoutSec}s)"
$p = [System.Diagnostics.Process]::Start($psi)
$out = $p.StandardOutput.ReadToEndAsync()
$err = $p.StandardError.ReadToEndAsync()
if (-not $p.WaitForExit($TimeoutSec * 1000)) {
  Write-Host "still running after ${TimeoutSec}s - stopping pid $($p.Id)"
  try { $p.Kill() } catch {}
  $p.WaitForExit(10000) | Out-Null
}
[IO.File]::WriteAllText($log, $out.Result + $err.Result)
Write-Host "exit code: $($p.ExitCode)"
Write-Host "log: $log ($((Get-Item $log).Length) bytes)"
Get-Content -LiteralPath $log -Tail 40
