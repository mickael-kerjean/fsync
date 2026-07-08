# End-to-end tests for fsync-windows against a live Filestash server.
#
#

param(
    [string]$Filter = "*",
    [switch]$KeepGoing
)

$ErrorActionPreference = "Continue"
$ProgressPreference = "SilentlyContinue"

$Base = "http://192.168.68.105:8334"
$Root = "$env:USERPROFILE\Filestash"
$LogFile = "$env:LOCALAPPDATA\Filestash\fsync.log"
$E2E = "e2e"
$LocalE2E = Join-Path $Root "folderA\$E2E"
$SrvE2E = "/folderA/$E2E"

$script:Token = $null
$script:Results = @()
$script:Shell = New-Object -ComObject Shell.Application


function Enc([string]$s) { [uri]::EscapeDataString($s) }

function Srv-Auth {
    $jar = "$env:TEMP\fsync-e2e-cookies.txt"
    curl.exe -s -c $jar -X POST -H "X-Requested-With: SDKHttpRequest" `
        -d "user=test&password=test" "$Base/api/session/auth/?label=virtualfs" | Out-Null
    $line = Get-Content $jar | Select-String "`tauth`t"
    if (-not $line) { throw "server auth failed" }
    $script:Token = ($line -split "`t")[-1]
}

function Srv-Call([string]$Method, [string]$Url, [string]$BodyFile) {
    $args = @("-s", "-X", $Method,
        "-H", "X-Requested-With: SDKHttpRequest",
        "-H", "Authorization: Bearer $script:Token")
    if ($BodyFile) { $args += @("--data-binary", "@$BodyFile") }
    $args += "$Base$Url"
    (curl.exe @args) -join "`n"
}

function Srv-Save([string]$Path, [string]$Content) {
    $tmp = "$env:TEMP\fsync-e2e-body.bin"
    [IO.File]::WriteAllText($tmp, $Content, (New-Object Text.UTF8Encoding $false))
    $r = Srv-Call "POST" "/api/files/cat?path=$(Enc $Path)" $tmp
    if ($r -notmatch '"status": "ok"') { throw "save $Path`: $r" }
}

function Srv-SaveFile([string]$Path, [string]$LocalFile) {
    $r = Srv-Call "POST" "/api/files/cat?path=$(Enc $Path)" $LocalFile
    if ($r -notmatch '"status": "ok"') { throw "save $Path`: $r" }
}

function Srv-Cat([string]$Path) {
    Srv-Call "GET" "/api/files/cat?path=$(Enc $Path)" $null
}

function Srv-Mkdir([string]$Path) {
    $r = Srv-Call "POST" "/api/files/mkdir?path=$(Enc $Path)" $null
    if ($r -notmatch '"status": "ok"') { throw "mkdir $Path`: $r" }
}

function Srv-Rm([string]$Path) {
    Srv-Call "POST" "/api/files/rm?path=$(Enc $Path)" $null | Out-Null
}

function Srv-Ls([string]$Path) {
    $r = Srv-Call "GET" "/api/files/ls?path=$(Enc $Path)" $null
    if ($r -notmatch '"status": "ok"') { return @() }
    $parsed = $r | ConvertFrom-Json
    if ($null -eq $parsed.results) { return @() }
    @($parsed.results | ForEach-Object { $_.name })
}

function Srv-Has([string]$Dir, [string]$Name) { @(Srv-Ls $Dir) -contains $Name }


function LPath([string]$rel) { Join-Path $LocalE2E ($rel -replace '/', '\') }

function Wait-Until([scriptblock]$Cond, [int]$TimeoutSec = 25, [string]$What = "condition") {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt $TimeoutSec) {
        $ok = & {
            $ErrorActionPreference = 'Stop'
            try { & $Cond } catch { $false }
        }
        if ($ok) { return $true }
        Start-Sleep -Milliseconds 500
    }
    Write-Host "    timed out waiting for: $What" -ForegroundColor Yellow
    return $false
}

function Is-Placeholder([string]$abs) {
    try {
        $attrs = [int](Get-Item -Force -LiteralPath $abs -ErrorAction Stop).Attributes
        return ($attrs -band 0x400) -ne 0
    } catch { return $false }
}

function Is-Offline([string]$abs) {
    try {
        $attrs = [int](Get-Item -Force -LiteralPath $abs -ErrorAction Stop).Attributes
        return ($attrs -band 0x400000) -ne 0
    } catch { return $false }
}

# Pin/unpin the way Explorer's "Always keep on this device" / "Free up space" do:
# CfSetPinState via cldapi, NOT `attrib +p` (which only stamps the attribute and does
# not engage the cloud-filter, so the OS never hydrates).
Add-Type -Namespace Cf -Name Api -MemberDefinition @'
[DllImport("cldapi.dll", CharSet=CharSet.Unicode)] public static extern int CfOpenFileWithOplock(string path, uint flags, out IntPtr handle);
[DllImport("cldapi.dll")] public static extern int CfSetPinState(IntPtr handle, int state, uint flags, IntPtr overlapped);
[DllImport("cldapi.dll")] public static extern int CfCloseHandle(IntPtr handle);
'@
function Set-Pin([string]$abs, [int]$state) {
    $h = [IntPtr]::Zero
    $hr = [Cf.Api]::CfOpenFileWithOplock($abs, 0x2, [ref]$h)  # WRITE_ACCESS
    if ($hr -ne 0) { throw ("CfOpenFileWithOplock 0x{0:X}" -f $hr) }
    try {
        $hr = [Cf.Api]::CfSetPinState($h, $state, 0, [IntPtr]::Zero)
        if ($hr -ne 0) { throw ("CfSetPinState 0x{0:X}" -f $hr) }
    } finally { [Cf.Api]::CfCloseHandle($h) | Out-Null }
}
function Pin([string]$abs)   { Set-Pin $abs 1 }   # CF_PIN_STATE_PINNED
function Unpin([string]$abs) { Set-Pin $abs 2 }   # CF_PIN_STATE_UNPINNED


function Open-View([string]$abs) {
    $script:Shell.Open($abs)
    Start-Sleep -Milliseconds 1500
}

function Close-Views {
    foreach ($w in @($script:Shell.Windows())) {
        try {
            if ($w.LocationURL -like "*Filestash*") { $w.Quit() }
        } catch {}
    }
    Start-Sleep -Milliseconds 500
}

function Recycle([string]$abs) {
    Add-Type -AssemblyName Microsoft.VisualBasic
    $item = Get-Item -Force -LiteralPath $abs
    if ($item.PSIsContainer) {
        [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteDirectory(
            $abs, 'OnlyErrorDialogs', 'SendToRecycleBin')
    } else {
        [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile(
            $abs, 'OnlyErrorDialogs', 'SendToRecycleBin')
    }
}

function Restore-FromBin([string]$name) {
    $bin = $script:Shell.NameSpace(0xA)
    foreach ($item in @($bin.Items())) {
        if ($item.Name -eq $name) {
            foreach ($verb in @($item.Verbs())) {
                if ($verb.Name -replace '&', '' -match 'Restore|estaurer') {
                    $verb.DoIt()
                    return $true
                }
            }
        }
    }
    return $false
}

function Empty-BinOf([string]$name) {
    $bin = $script:Shell.NameSpace(0xA)
    foreach ($item in @($bin.Items())) {
        if ($item.Name -eq $name) {
            try { Remove-Item -Force -Recurse -LiteralPath $item.Path -ErrorAction SilentlyContinue } catch {}
        }
    }
}


function Log-Mark {
    if (Test-Path $LogFile) { (Get-Content $LogFile | Measure-Object -Line).Lines } else { 0 }
}

function Log-Since([int]$mark) {
    if (Test-Path $LogFile) { @(Get-Content $LogFile | Select-Object -Skip $mark) } else { @() }
}


function Test([string]$Name, [scriptblock]$Body) {
    if ($Name -notlike $Filter) { return }
    Write-Host "== $Name" -ForegroundColor Cyan
    $mark = Log-Mark
    $failure = $null
    try {
        $out = @(& $Body)
        $ok = $out.Count -gt 0 -and $out[-1] -eq $true
        if (-not $ok) { $failure = "assertion failed" }
    } catch {
        $failure = $_.Exception.Message
    }
    $errors = @(Log-Since $mark | Where-Object { $_ -match "level=ERROR|panic" })
    $alive = [bool](Get-Process fsync-windows -ErrorAction SilentlyContinue)
    if (-not $alive) { $failure = "CLIENT PROCESS DIED: $failure" }
    if ($errors.Count -gt 0 -and -not $failure) { $failure = "log errors: $($errors -join ' | ')" }
    if ($failure) {
        Write-Host "  FAIL: $failure" -ForegroundColor Red
        foreach ($e in $errors) { Write-Host "    $e" -ForegroundColor Red }
        $script:Results += [pscustomobject]@{ Name = $Name; Status = "FAIL"; Why = $failure }
        if (-not $KeepGoing -and -not $alive) { Summary; exit 1 }
    } else {
        Write-Host "  PASS" -ForegroundColor Green
        $script:Results += [pscustomobject]@{ Name = $Name; Status = "PASS"; Why = "" }
    }
}

function Summary {
    Write-Host ""
    Write-Host "======== summary ========"
    $script:Results | ForEach-Object {
        $color = "Green"
        if ($_.Status -eq "FAIL") { $color = "Red" }
        Write-Host ("{0,-42} {1}  {2}" -f $_.Name, $_.Status, $_.Why) -ForegroundColor $color
    }
    $fails = @($script:Results | Where-Object { $_.Status -eq "FAIL" }).Count
    Write-Host "$(@($script:Results).Count) tests, $fails failed"
}


Srv-Auth
if (-not (Get-Process fsync-windows -ErrorAction SilentlyContinue)) {
    throw "fsync-windows is not running"
}
Close-Views
Srv-Rm "$SrvE2E/"
if (Test-Path $LocalE2E) { Remove-Item -Recurse -Force $LocalE2E -ErrorAction SilentlyContinue }
Start-Sleep -Seconds 2
Srv-Mkdir "$SrvE2E/"
Open-View (Join-Path $Root "folderA")
if (-not (Wait-Until { Test-Path $LocalE2E } 30 "e2e dir to appear locally")) {
    throw "e2e dir never appeared locally: remote-to-local chain is broken, aborting"
}
Open-View $LocalE2E


Test "remote-create-file" {
    Srv-Save "$SrvE2E/r1.txt" "remote one"
    Wait-Until { Test-Path (LPath "r1.txt") } 25 "r1.txt locally"
}

Test "hydrate-on-read" {
    $content = Get-Content -Raw (LPath "r1.txt")
    $content -eq "remote one"
}

Test "remote-modify-updates-placeholder" {
    Srv-Save "$SrvE2E/r1.txt" "remote one but longer now"
    $ok = Wait-Until {
        (Get-Item -Force (LPath "r1.txt")).Length -eq 25
    } 25 "size to become 25"
    if (-not $ok) { return $false }
    (Get-Content -Raw (LPath "r1.txt")) -eq "remote one but longer now"
}

Test "remote-modify-dehydrated-placeholder" {
    Srv-Save "$SrvE2E/rd.txt" "v1"
    $ok = Wait-Until { Test-Path (LPath "rd.txt") } 25 "rd.txt locally"
    if (-not $ok) { return $false }
    Srv-Save "$SrvE2E/rd.txt" "v2 much longer content"
    $ok = Wait-Until {
        (Get-Item -Force (LPath "rd.txt")).Length -eq 22
    } 25 "dehydrated size to update"
    if (-not $ok) { return $false }
    $content = Get-Content -Raw (LPath "rd.txt")
    Srv-Rm "$SrvE2E/rd.txt"
    $content -eq "v2 much longer content"
}

Test "remote-delete-file" {
    Srv-Rm "$SrvE2E/r1.txt"
    Wait-Until { -not (Test-Path (LPath "r1.txt")) } 25 "r1.txt to vanish locally"
}

Test "remote-tree-populates-on-first-enumeration" {
    Srv-Mkdir "$SrvE2E/rtree/"
    Srv-Mkdir "$SrvE2E/rtree/deep/"
    Srv-Save "$SrvE2E/rtree/a.txt" "aaa"
    Srv-Save "$SrvE2E/rtree/deep/b.txt" "bbb"
    $ok = Wait-Until { Test-Path (LPath "rtree") } 25 "rtree locally"
    if (-not $ok) { return $false }
    $names = @(Get-ChildItem (LPath "rtree") | ForEach-Object Name)
    if ($names -notcontains "a.txt" -or $names -notcontains "deep") {
        Write-Host "    first enumeration gave: $($names -join ', ')" -ForegroundColor Yellow
        return $false
    }
    (Get-Content -Raw (LPath "rtree/deep/b.txt")) -eq "bbb"
}

Test "remote-delete-clean-dir" {
    Srv-Rm "$SrvE2E/rtree/"
    Wait-Until { -not (Test-Path (LPath "rtree")) } 30 "rtree to vanish locally"
}


Test "local-create-uploads" {
    Set-Content -LiteralPath (LPath "l1.txt") -Value "local one" -Encoding ascii -NoNewline
    Wait-Until { Srv-Has "$SrvE2E/" "l1.txt" } 25 "l1.txt on server"
}

Test "local-create-becomes-placeholder" {
    Wait-Until { Is-Placeholder (LPath "l1.txt") } 20 "l1.txt converted"
}

Test "local-edit-uploads" {
    Set-Content -LiteralPath (LPath "l1.txt") -Value "local one edited" -Encoding ascii -NoNewline
    Wait-Until { (Srv-Cat "$SrvE2E/l1.txt") -eq "local one edited" } 25 "server content updated"
}

Test "local-rename-propagates" {
    Rename-Item (LPath "l1.txt") "l1-renamed.txt"
    $ok = Wait-Until { Srv-Has "$SrvE2E/" "l1-renamed.txt" } 25 "renamed on server"
    if (-not $ok) { return $false }
    -not (Srv-Has "$SrvE2E/" "l1.txt")
}

Test "local-mkdir-and-move-in" {
    New-Item -ItemType Directory (LPath "lsub") | Out-Null
    $ok = Wait-Until { Srv-Has "$SrvE2E/" "lsub" } 25 "lsub on server"
    if (-not $ok) { return $false }
    Move-Item (LPath "l1-renamed.txt") (LPath "lsub\l1-renamed.txt")
    Wait-Until { Srv-Has "$SrvE2E/lsub/" "l1-renamed.txt" } 25 "file moved on server"
}

Test "local-delete-file-propagates" {
    Remove-Item (LPath "lsub\l1-renamed.txt")
    Wait-Until { -not (Srv-Has "$SrvE2E/lsub/" "l1-renamed.txt") } 25 "gone on server"
}

Test "local-delete-dir-propagates" {
    Remove-Item -Recurse (LPath "lsub")
    Wait-Until { -not (Srv-Has "$SrvE2E/" "lsub") } 25 "dir gone on server"
}


Test "recycle-hydrated-placeholder" {
    Srv-Save "$SrvE2E/rec1.txt" "to be recycled"
    $ok = Wait-Until { Test-Path (LPath "rec1.txt") } 25 "rec1.txt locally"
    if (-not $ok) { return $false }
    $null = Get-Content -Raw (LPath "rec1.txt")
    Recycle (LPath "rec1.txt")
    $ok = Wait-Until { -not (Srv-Has "$SrvE2E/" "rec1.txt") } 25 "gone on server"
    Empty-BinOf "rec1.txt"
    $ok -and -not (Test-Path (LPath "rec1.txt"))
}

Test "recycle-dehydrated-placeholder" {
    Srv-Save "$SrvE2E/rec2.txt" "never opened"
    $ok = Wait-Until { Test-Path (LPath "rec2.txt") } 25 "rec2.txt locally"
    if (-not $ok) { return $false }
    Recycle (LPath "rec2.txt")
    $ok = Wait-Until { -not (Srv-Has "$SrvE2E/" "rec2.txt") } 25 "gone on server"
    Empty-BinOf "rec2.txt"
    $ok -and -not (Test-Path (LPath "rec2.txt"))
}

Test "recycle-directory" {
    Srv-Mkdir "$SrvE2E/recdir/"
    Srv-Save "$SrvE2E/recdir/x.txt" "x"
    $ok = Wait-Until { Test-Path (LPath "recdir") } 25 "recdir locally"
    if (-not $ok) { return $false }
    $null = Get-ChildItem (LPath "recdir")
    Recycle (LPath "recdir")
    $ok = Wait-Until { -not (Srv-Has "$SrvE2E/" "recdir") } 25 "gone on server"
    Empty-BinOf "recdir"
    $ok -and -not (Test-Path (LPath "recdir"))
}


Test "pin-hydrates" {
    Srv-Save "$SrvE2E/pin1.txt" "pin me"
    $ok = Wait-Until { Test-Path (LPath "pin1.txt") } 25 "pin1.txt locally"
    if (-not $ok) { return $false }
    if (-not (Is-Offline (LPath "pin1.txt"))) { return $false }
    Pin (LPath "pin1.txt")
    Wait-Until { -not (Is-Offline (LPath "pin1.txt")) } 30 "bytes on disk"
}

Test "unpin-dehydrates" {
    Unpin (LPath "pin1.txt")
    $ok = Wait-Until { Is-Offline (LPath "pin1.txt") } 45 "bytes released"
    Srv-Rm "$SrvE2E/pin1.txt"
    $ok
}

Test "pinned-file-stays-pinned-across-refreshes" {
    Srv-Save "$SrvE2E/pinstay.txt" "pinned v1"
    $ok = Wait-Until { Test-Path (LPath "pinstay.txt") } 25 "pinstay.txt locally"
    if (-not $ok) { return $false }
    Pin (LPath "pinstay.txt")
    $ok = Wait-Until { -not (Is-Offline (LPath "pinstay.txt")) } 30 "hydrated"
    if (-not $ok) { return $false }
    Start-Sleep -Seconds 25
    $attrs = [int](Get-Item -Force (LPath "pinstay.txt")).Attributes
    (-not (Is-Offline (LPath "pinstay.txt"))) -and (($attrs -band 0x80000) -ne 0)
}

Test "pinned-file-tracks-remote-change" {
    Srv-Save "$SrvE2E/pinstay.txt" "pinned v2 now much longer"
    $ok = Wait-Until {
        (-not (Is-Offline (LPath "pinstay.txt"))) -and
        ((Get-Item -Force (LPath "pinstay.txt")).Length -eq 25)
    } 45 "new version hydrated"
    if (-not $ok) { return $false }
    $content = Get-Content -Raw (LPath "pinstay.txt")
    $attrs = [int](Get-Item -Force (LPath "pinstay.txt")).Attributes
    Srv-Rm "$SrvE2E/pinstay.txt"
    ($content -eq "pinned v2 now much longer") -and (($attrs -band 0x80000) -ne 0)
}


Test "conflict-keeps-both" {
    Set-Content -LiteralPath (LPath "c1.txt") -Value "base" -Encoding ascii -NoNewline
    $ok = Wait-Until { Srv-Has "$SrvE2E/" "c1.txt" } 25 "c1.txt on server"
    if (-not $ok) { return $false }
    $ok = Wait-Until { Is-Placeholder (LPath "c1.txt") } 20 "c1.txt converted"
    if (-not $ok) { return $false }
    Srv-Save "$SrvE2E/c1.txt" "server version"
    Set-Content -LiteralPath (LPath "c1.txt") -Value "local version" -Encoding ascii -NoNewline
    $ok = Wait-Until {
        @(Srv-Ls "$SrvE2E/") -match "c1 \(conflicted copy\)"
    } 30 "conflicted copy on server"
    if (-not $ok) { return $false }
    (Srv-Cat "$SrvE2E/c1 (conflicted copy).txt") -eq "local version" -and
        (Srv-Cat "$SrvE2E/c1.txt") -eq "server version"
}

Test "dirty-file-survives-remote-dir-delete" {
    Srv-Mkdir "$SrvE2E/ddir/"
    Srv-Save "$SrvE2E/ddir/keep.txt" "server"
    $ok = Wait-Until { Test-Path (LPath "ddir\keep.txt") } 25 "ddir/keep.txt locally"
    if (-not $ok) { return $false }
    Set-Content -LiteralPath (LPath "ddir\keep.txt") -Value "my precious edits" -Encoding ascii -NoNewline
    Srv-Rm "$SrvE2E/ddir/"
    Start-Sleep -Seconds 12
    $kept = (Test-Path (LPath "ddir\keep.txt")) -and
        (Get-Content -Raw (LPath "ddir\keep.txt")) -eq "my precious edits"
    $uploaded = Wait-Until {
        (Srv-Cat "$SrvE2E/ddir/keep.txt") -eq "my precious edits"
    } 40 "edits re-uploaded"
    Remove-Item -Recurse -Force (LPath "ddir") -ErrorAction SilentlyContinue
    $kept -and $uploaded
}

Test "unicode-names" {
    Srv-Save "$SrvE2E/café rémote.txt" "unicode remote"
    $ok = Wait-Until { Test-Path (LPath "café rémote.txt") } 25 "unicode file locally"
    if (-not $ok) { return $false }
    if ((Get-Content -Raw (LPath "café rémote.txt")) -ne "unicode remote") { return $false }
    Set-Content -LiteralPath (LPath "café löcal.txt") -Value "unicode local" -Encoding utf8 -NoNewline
    Wait-Until { Srv-Has "$SrvE2E/" "café löcal.txt" } 25 "unicode file on server"
}

Test "big-file-roundtrip" {
    $big = "$env:TEMP\fsync-e2e-big.bin"
    $bytes = New-Object byte[] (5MB)
    (New-Object Random 42).NextBytes($bytes)
    [IO.File]::WriteAllBytes($big, $bytes)
    $srcHash = (Get-FileHash $big -Algorithm SHA256).Hash
    Srv-SaveFile "$SrvE2E/big.bin" $big
    $ok = Wait-Until { Test-Path (LPath "big.bin") } 25 "big.bin locally"
    if (-not $ok) { return $false }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $gotHash = (Get-FileHash (LPath "big.bin") -Algorithm SHA256).Hash
    Write-Host "    hydrated 5MB in $([int]$sw.Elapsed.TotalMilliseconds)ms"
    $gotHash -eq $srcHash
}

Test "explorer-enumeration-is-fast" {
    Srv-Mkdir "$SrvE2E/many/"
    for ($i = 0; $i -lt 40; $i++) { Srv-Save "$SrvE2E/many/f$i.txt" "file $i" }
    $ok = Wait-Until { Test-Path (LPath "many") } 25 "many/ locally"
    if (-not $ok) { return $false }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $names = @(Get-ChildItem (LPath "many") | ForEach-Object Name)
    $first = $sw.Elapsed.TotalMilliseconds
    $sw.Restart()
    $null = @(Get-ChildItem (LPath "many"))
    $second = $sw.Elapsed.TotalMilliseconds
    Write-Host "    first (network) enumeration: $([int]$first)ms, second (local): $([int]$second)ms"
    ($names.Count -eq 40) -and ($first -lt 10000) -and ($second -lt 1000)
}


Close-Views
Srv-Rm "$SrvE2E/"
Summary
