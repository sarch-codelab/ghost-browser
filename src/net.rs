//! Detección de conectividad de bajo coste.
//!
//! No hace tráfico de red: pregunta al sistema operativo si hay una conexión
//! activa. Sirve para avisar en la barra de estado cuando el equipo se queda
//! sin red (Wi-Fi caído, cable desconectado, modo avión) en lugar de dejar al
//! usuario esperando una página que nunca va a cargar.

/// `true` si el sistema tiene una conexión de red activa.
#[cfg(windows)]
pub fn is_online() -> bool {
    #[link(name = "wininet")]
    unsafe extern "system" {
        fn InternetGetConnectedState(flags: *mut u32, reserved: u32) -> i32;
    }
    let mut flags = 0u32;
    unsafe { InternetGetConnectedState(&mut flags, 0) != 0 }
}

/// En el resto de plataformas no hay una consulta barata equivalente: se
/// asume que hay red y los avisos de carga lenta siguen funcionando.
#[cfg(not(windows))]
pub fn is_online() -> bool {
    true
}
