# Benchmark de RAM: Ghost (Wisp / WebView2) vs Edge.
# Mide Working Set y Private Bytes (PageFileUsage) del arbol de procesos.
# Uso: powershell -File ram_bench.ps1 [-TabCounts 1,5,10] [-WaitSecs 35]
param(
    [int[]]$TabCounts = @(1, 5, 10, 20),
    [int]$WaitSecs = 35,
    [ValidateSet("debug", "release")]
    [string]$Profile = "release"
)

$ghost = "C:\Users\Sami\Downloads\oasis\target\$Profile\ghost.exe"
$edge  = "C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"
$store = "$env:LOCALAPPDATA\Ghost\ghost.json"
$backup = "$store.bench-bak"

$base = "https://es.wikipedia.org/wiki/"
$allPages = @(
    "Rust_(lenguaje_de_programaci%C3%B3n)",
    "Navegador_web",
    "Inteligencia_artificial",
    "JavaScript",
    "HTML",
    "Cascada_de_hojas_de_estilo",
    "Wikipedia:Portada",
    "Sistema_operativo",
    "Red_internet",
    "Algoritmo",
    "Python_(lenguaje_de_programaci%C3%B3n)",
    "Microsoft_Windows",
    "Linux",
    "Base_de_datos",
    "Red_neuronal",
    "Compilador",
    "Criptograf%C3%ADa",
    "Navegaci%C3%B3n_por_sat%C3%A9lite",
    "Teclado",
    "Internet"
) | ForEach-Object { $base + $_ }

if (Test-Path $store) { Copy-Item $store $backup -Force }

function Get-TreeStatsMB([int]$rootPid) {
    $procs = Get-CimInstance Win32_Process |
        Select-Object ProcessId, ParentProcessId, WorkingSetSize, PageFileUsage
    $byId = @{}
    foreach ($p in $procs) { $byId[[int]$p.ProcessId] = $p }
    $ws = 0; $pb = 0; $count = 0
    $queue = [System.Collections.Generic.Queue[int]]::new()
    $seen = @{}
    $queue.Enqueue($rootPid)
    while ($queue.Count -gt 0) {
        $id = $queue.Dequeue()
        if ($seen.ContainsKey($id) -or $id -eq 0) { continue }
        $seen[$id] = $true
        $proc = $byId[$id]
        if ($null -eq $proc) { continue }
        $count += 1
        $ws += $proc.WorkingSetSize
        $pb += $proc.PageFileUsage * 1KB
        foreach ($p in $procs) {
            if ([int]$p.ParentProcessId -eq $id) {
                $queue.Enqueue([int]$p.ProcessId)
            }
        }
    }
    return [pscustomobject]@{
        WS_MB = [math]::Round($ws / 1MB, 1)
        PB_MB = [math]::Round($pb / 1MB, 1)
        Procs = $count
    }
}

function Measure-Ghost([string]$label, [string[]]$extraArgs, [string[]]$urls) {
    $argList = @("--no-saver", "--no-session") + $extraArgs + $urls
    $proc = Start-Process -FilePath $ghost -ArgumentList $argList -PassThru
    Write-Host "[$label] ghost.exe PID $($proc.Id) con $($urls.Count) URLs; esperando $WaitSecs s..."
    Start-Sleep -Seconds $WaitSecs
    if ($proc.HasExited) {
        Write-Host "[$label] WARNING: ghost.exe salio antes de medir."
        return
    }
    $s = Get-TreeStatsMB $proc.Id
    Write-Host ("[{0}] WS={1} MB  PB={2} MB  procs={3}  (~{4} MB/pestana)" -f `
        $label, $s.WS_MB, $s.PB_MB, $s.Procs, `
        [math]::Round($s.WS_MB / ($urls.Count + 1), 1))
    & taskkill /PID $proc.Id /T /F | Out-Null
    Start-Sleep -Seconds 2
}

function Measure-Edge([string[]]$urls) {
    if (-not (Test-Path $edge)) {
        Write-Host "[Edge] msedge.exe no encontrado; se omite."
        return
    }
    $profile = "$env:TEMP\ghost-bench-edge-$PID"
    $argList = @("--user-data-dir=$profile", "--no-first-run", "--no-default-browser-check") + $urls
    $proc = Start-Process -FilePath $edge -ArgumentList $argList -PassThru
    Write-Host "[Edge] msedge.exe PID $($proc.Id) con $($urls.Count) URLs; esperando $WaitSecs s..."
    Start-Sleep -Seconds $WaitSecs
    if ($proc.HasExited) {
        Write-Host "[Edge] WARNING: msedge.exe salio antes de medir."
        return
    }
    $s = Get-TreeStatsMB $proc.Id
    Write-Host ("[Edge] WS={0} MB  PB={1} MB  procs={2}  (~{3} MB/pestana)" -f `
        $s.WS_MB, $s.PB_MB, $s.Procs, `
        [math]::Round($s.WS_MB / $urls.Count, 1))
    & taskkill /PID $proc.Id /T /F | Out-Null
    Start-Sleep -Seconds 2
}

Write-Host "=== Benchmark RAM (build $Profile) - Wikipedia ==="
foreach ($n in $TabCounts) {
    $urls = $allPages | Select-Object -First $n
    Write-Host ""
    Write-Host "----- $n pestanas -----"
    Measure-Ghost "Wisp     " @("--wisp") $urls
    Measure-Ghost "WebView2" @("--webview2") $urls
    Measure-Edge $urls
}

if (Test-Path $backup) {
    Copy-Item $backup $store -Force
    Remove-Item $backup -Force
    Write-Host "Perfil restaurado desde backup."
}
Write-Host "Listo."
