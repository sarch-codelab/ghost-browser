use std::io::Write;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static START: OnceLock<Instant> = OnceLock::new();

/// Reporta el uso de memoria actual si está en Windows. Solo escribe si
/// GHOST_TIMING_LOG está definido.
pub fn report_memory_usage() {
    let Ok(_path) = std::env::var("GHOST_TIMING_LOG") else {
        return;
    };
    #[cfg(windows)]
    {
        use windows::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
        };
        use windows::Win32::System::Threading::GetCurrentProcess;
        let handle = unsafe { GetCurrentProcess() };
        let mut mem = PROCESS_MEMORY_COUNTERS_EX::default();
        mem.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if unsafe { GetProcessMemoryInfo(handle, &mut mem as *mut _ as *mut PROCESS_MEMORY_COUNTERS, cb) }.is_ok() {
            let ws_mb = mem.WorkingSetSize / 1024 / 1024;
            let pb_mb = mem.PrivateUsage / 1024 / 1024;
            log_line(&format!("memory\tWS={}MB\tPB={}MB", ws_mb, pb_mb));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
}

/// Marca el inicio del proceso. Solo escribe si GHOST_TIMING_LOG está definido.
pub fn start() {
    START.get_or_init(Instant::now);
}

fn log_line(line: &str) {
    let Ok(path) = std::env::var("GHOST_TIMING_LOG") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

/// Marca un hito con los milisegundos desde el inicio del proceso.
pub fn mark(label: &str) {
    let Some(start) = START.get() else {
        return;
    };
    log_line(&format!("{label}\t{}", start.elapsed().as_millis()));
}

/// Mide una restauración de pestaña suspendida de principio a fin.
pub fn mark_restore(tab: u64, elapsed: Duration) {
    if std::env::var_os("GHOST_TIMING_LOG").is_none() {
        return;
    }
    log_line(&format!("restore:{tab}\t{}", elapsed.as_millis()));
}

/// Mide una fase intermedia de la restauración (por ejemplo, la creación del
/// WebView2) para distinguirla de la carga real de la página.
pub fn mark_restore_phase(tab: u64, phase: &str, elapsed: Duration) {
    if std::env::var_os("GHOST_TIMING_LOG").is_none() {
        return;
    }
    log_line(&format!("restore:{tab}:{phase}\t{}", elapsed.as_millis()));
}
