use std::io::Write;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static START: OnceLock<Instant> = OnceLock::new();

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
