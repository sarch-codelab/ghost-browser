//! Control profundo de WebView2 vía Chrome DevTools Protocol (CDP) y
//! captura de pantalla, usado por la API remota para que agentes de IA
//! puedan ver y manipular la página (clicks, tecleo, scroll, screenshots).
//!
//! Sólo existe en Windows (WebView2); en otras plataformas las funciones
//! devuelven error para que la API remota responda limpio.

use serde_json::{Value, json};
use std::sync::mpsc::Sender;

/// Lanza un método CDP sin esperar resultado (fire-and-forget). El handler
/// de finalización es un no-op: los eventos de `Input.*` se procesan en orden.
#[cfg(windows)]
pub fn fire(webview: &wry::WebView, method: &str, params: &str) -> Result<(), String> {
    imp::call_inner(webview, method, params, None)
}

/// Llama un método CDP y entrega el resultado JSON por `done` cuando el
/// runtime de WebView2 termina (canal diferido, igual que `remote_eval`).
#[cfg(windows)]
pub fn call(
    webview: &wry::WebView,
    method: &str,
    params: &str,
    done: Sender<Value>,
) -> Result<(), String> {
    imp::call_inner(webview, method, params, Some(done))
}

/// Captura un PNG del webview y lo guarda en `path`; el resultado
/// (`{"ok":true,"path":...,"bytes":N}` o error) llega por `done`.
#[cfg(windows)]
pub fn screenshot(
    webview: &wry::WebView,
    path: &std::path::Path,
    done: Sender<Value>,
) -> Result<(), String> {
    imp::capture(webview, path.to_path_buf(), done)
}

#[cfg(not(windows))]
pub fn fire(_webview: &wry::WebView, _method: &str, _params: &str) -> Result<(), String> {
    Err("CDP sólo está disponible en Windows/WebView2".into())
}

#[cfg(not(windows))]
pub fn call(
    _webview: &wry::WebView,
    _method: &str,
    _params: &str,
    _done: Sender<Value>,
) -> Result<(), String> {
    Err("CDP sólo está disponible en Windows/WebView2".into())
}

#[cfg(not(windows))]
pub fn screenshot(
    _webview: &wry::WebView,
    _path: &std::path::Path,
    _done: Sender<Value>,
) -> Result<(), String> {
    Err("la captura sólo está disponible en Windows/WebView2".into())
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::path::PathBuf;
    use webview2_com::Microsoft::Web::WebView2::Win32::*;
    use webview2_com::{
        CallDevToolsProtocolMethodCompletedHandler, CapturePreviewCompletedHandler,
    };
    use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
    use windows::Win32::System::Com::{IStream, STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET};
    use windows_core::HSTRING;
    use wry::WebViewExtWindows;

    pub fn call_inner(
        webview: &wry::WebView,
        method: &str,
        params: &str,
        done: Option<Sender<Value>>,
    ) -> Result<(), String> {
        let core = webview.webview();
        let method = HSTRING::from(method);
        let params = HSTRING::from(if params.trim().is_empty() {
            "{}"
        } else {
            params
        });
        let handler =
            CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |error, result| {
                if let Some(done) = done {
                    let value = match error {
                        Ok(()) => {
                            serde_json::from_str(&result).unwrap_or_else(|_| json!({"raw": result}))
                        }
                        Err(e) => json!({"ok": false, "error": format!("{e:?}")}),
                    };
                    let _ = done.send(json!({"ok": true, "result": value}));
                }
                Ok(())
            }));
        unsafe { core.CallDevToolsProtocolMethod(&method, &params, &handler) }
            .map_err(|e| e.to_string())
    }

    /// Lee todos los bytes de un IStream posicionado al inicio.
    unsafe fn read_stream(stream: &IStream) -> Result<Vec<u8>, String> {
        unsafe {
            let mut stat = STATSTG::default();
            stream
                .Stat(&mut stat, STATFLAG_NONAME)
                .map_err(|e| format!("stat del stream: {e}"))?;
            let size = stat.cbSize.min(64 * 1024 * 1024) as usize;
            stream
                .Seek(0, STREAM_SEEK_SET, None)
                .map_err(|e| format!("seek del stream: {e}"))?;
            let mut buf = vec![0u8; size.max(1)];
            let mut read = 0u32;
            stream
                .Read(
                    buf.as_mut_ptr() as *mut _,
                    buf.len() as u32,
                    Some(&mut read),
                )
                .ok()
                .map_err(|e| format!("read del stream: {e:?}"))?;
            buf.truncate(read as usize);
            Ok(buf)
        }
    }

    pub fn capture(
        webview: &wry::WebView,
        path: PathBuf,
        done: Sender<Value>,
    ) -> Result<(), String> {
        let core = webview.webview();
        let stream = unsafe { CreateStreamOnHGlobal(Default::default(), true) }
            .map_err(|e| format!("CreateStreamOnHGlobal: {e}"))?;
        let stream_for_handler = stream.clone();
        let handler = CapturePreviewCompletedHandler::create(Box::new(move |error| {
            let reply = match error {
                Ok(()) => unsafe { read_stream(&stream_for_handler) }
                    .and_then(|bytes| {
                        if let Some(parent) = path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        std::fs::write(&path, &bytes)
                            .map(|_| json!({"ok": true, "path": path.display().to_string(), "bytes": bytes.len()}))
                            .map_err(|e| format!("escribir {}: {e}", path.display()))
                    })
                    .unwrap_or_else(|e| json!({"ok": false, "error": e})),
                Err(e) => json!({"ok": false, "error": format!("CapturePreview: {e:?}")}),
            };
            let _ = done.send(reply);
            Ok(())
        }));
        unsafe {
            core.CapturePreview(
                COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
                &stream,
                &handler,
            )
        }
        .map_err(|e| e.to_string())
    }
}
