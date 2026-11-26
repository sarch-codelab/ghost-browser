//! API de control remoto de Ghost en localhost: permite que agentes de IA
//! (o cualquier herramienta) listen, naveguen, evalúen JS y lean texto de
//! las pestañas. Sin dependencias: HTTP/1.1 mínimo sobre TcpListener.
//!
//! El contrato de rutas vive en `wisp_mcp::control`, compartido con
//! `wisp-agent serve`: el mismo cliente MCP funciona contra ambos hosts.
//!
//! Puerto: `WISP_REMOTE_PORT` / `GHOST_REMOTE_PORT` (por defecto 47820).

use serde_json::{Value, json};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use wisp_mcp::control::{self, ReadRequest, Request, Route, TabTarget};

/// Comandos que el hilo HTTP encola para el hilo de UI (donde vive Browser).
pub enum RemoteCommand {
    /// Petición síncrona: el hilo de UI responde por `reply`.
    Sync {
        request: Request,
        reply: Sender<Value>,
    },
    /// Evaluación JS en un WebView2 o Wisp: la respuesta llega cuando la
    /// página devuelve el resultado por IPC (ver `remote_eval` en browser.rs).
    Eval {
        target: TabTarget,
        js: String,
        done: Sender<Value>,
    },
    /// Método crudo del Chrome DevTools Protocol: la respuesta llega por el
    /// handler `CallDevToolsProtocolMethodCompleted` de WebView2.
    Cdp {
        target: TabTarget,
        method: String,
        params: String,
        done: Sender<Value>,
    },
    /// Captura de pantalla PNG vía `CapturePreview`; el resultado (con la
    /// ruta del archivo) llega por el handler de finalización.
    Shot {
        target: TabTarget,
        path: Option<String>,
        done: Sender<Value>,
    },
}

/// Canal hacia el hilo de UI. `None` si el bind del puerto falló.
pub fn start() -> Option<Receiver<RemoteCommand>> {
    let port = control::configured_port();
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(error) => {
            log::warn!("API remota desactivada (puerto {port} ocupado?): {error}");
            return None;
        }
    };
    log::info!("API remota de Ghost en http://127.0.0.1:{port}");
    let (tx, rx) = channel::<RemoteCommand>();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let tx = tx.clone();
            thread::spawn(move || handle_connection(stream, tx));
        }
    });
    Some(rx)
}

fn handle_connection(mut stream: TcpStream, tx: Sender<RemoteCommand>) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(15)));
    match control::read_request_checked(&mut stream) {
        ReadRequest::Ok(request) => {
            let response = route(&request, &tx);
            let _ = control::write_response(&mut stream, &response);
        }
        ReadRequest::InvalidJson(message) => {
            let _ = control::write_status_response(
                &mut stream,
                400,
                "Bad Request",
                &json!({"ok": false, "error": message}),
            );
        }
        ReadRequest::Eof => {}
    }
}

fn route(request: &Request, tx: &Sender<RemoteCommand>) -> Value {
    match request.route() {
        // GET /health no necesita el hilo de UI.
        Route::Health => {
            json!({"ok": true, "name": "ghost", "version": env!("CARGO_PKG_VERSION")})
        }

        // Eval tiene su propio canal de respuesta (llega por IPC de la página).
        Route::Eval(target) => {
            let js = request.body.get("js").and_then(Value::as_str).unwrap_or("");
            if js.is_empty() {
                return json!({"ok": false, "error": "falta 'js'"});
            }
            let (done_tx, done_rx) = channel::<Value>();
            if tx
                .send(RemoteCommand::Eval {
                    target,
                    js: js.to_string(),
                    done: done_tx,
                })
                .is_err()
            {
                return json!({"ok": false, "error": "Ghost no está procesando comandos"});
            }
            done_rx
                .recv_timeout(std::time::Duration::from_secs(15))
                .unwrap_or_else(|_| json!({"ok": false, "error": "timeout esperando la página"}))
        }

        // CDP crudo: POST /tabs/{id|active}/cdp {"method": "...", "params": {...}}
        Route::Cdp(target) => {
            let method = request
                .body
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("");
            if method.is_empty() {
                return json!({"ok": false, "error": "falta 'method'"});
            }
            let params = match request.body.get("params") {
                Some(Value::String(s)) => s.clone(),
                Some(v) if !v.is_null() => v.to_string(),
                _ => "{}".to_string(),
            };
            let (done_tx, done_rx) = channel::<Value>();
            if tx
                .send(RemoteCommand::Cdp {
                    target,
                    method: method.to_string(),
                    params,
                    done: done_tx,
                })
                .is_err()
            {
                return json!({"ok": false, "error": "Ghost no está procesando comandos"});
            }
            done_rx
                .recv_timeout(std::time::Duration::from_secs(15))
                .unwrap_or_else(|_| json!({"ok": false, "error": "timeout esperando CDP"}))
        }

        // Screenshot: POST /tabs/{id|active}/screenshot {"path": "..."?}
        // Devuelve la ruta del PNG guardado (el contenido es binario, no JSON).
        Route::Screenshot(target) => {
            let path = request
                .body
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_string);
            let (done_tx, done_rx) = channel::<Value>();
            if tx
                .send(RemoteCommand::Shot {
                    target,
                    path,
                    done: done_tx,
                })
                .is_err()
            {
                return json!({"ok": false, "error": "Ghost no está procesando comandos"});
            }
            done_rx
                .recv_timeout(std::time::Duration::from_secs(20))
                .unwrap_or_else(|_| json!({"ok": false, "error": "timeout capturando pantalla"}))
        }

        // El resto lo resuelve el hilo de UI contra el modelo de pestañas.
        _ => {
            let (reply_tx, reply_rx) = channel::<Value>();
            if tx
                .send(RemoteCommand::Sync {
                    request: request.clone(),
                    reply: reply_tx,
                })
                .is_err()
            {
                return json!({"ok": false, "error": "Ghost no está procesando comandos"});
            }
            reply_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap_or_else(|_| json!({"ok": false, "error": "timeout"}))
        }
    }
}
