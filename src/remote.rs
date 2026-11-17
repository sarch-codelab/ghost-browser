//! API de control remoto de Ghost en localhost: permite que agentes de IA
//! (o cualquier herramienta) listen, naveguen, evalúen JS y lean texto de
//! las pestañas. Sin dependencias: HTTP/1.1 mínimo sobre TcpListener.
//!
//!   GET  /health                 -> {"ok":true}
//!   GET  /tabs                   -> [{id,title,url,engine,active}]
//!   POST /tabs                   {"url": "..."}         -> nueva pestaña
//!   POST /tabs/{id}/navigate     {"url": "..."}
//!   POST /tabs/{id}/back|forward|reload|close
//!   POST /tabs/{id}/eval         {"js": "..."}          -> resultado del JS
//!   POST /tabs/{id}/text                             -> texto legible
//!   POST /tabs/{id}/activate                          -> trae la pestaña al frente
//!   POST /tabs/{id}/click         {"x","y","button"?,"clicks"?}
//!   POST /tabs/{id}/type          {"text": "..."}       -> inserta texto
//!   POST /tabs/{id}/press         {"key": "Enter"}      -> tecla (enter/tab/esc/...)
//!   POST /tabs/{id}/scroll        {"deltaX","deltaY","x"?,"y"?}
//!   POST /tabs/{id}/move          {"x","y"}             -> mueve el ratón
//!   POST /tabs/{id}/cdp           {"method","params"}  -> Chrome DevTools Protocol
//!   POST /tabs/{id}/screenshot    {"path"?}             -> PNG en disco, devuelve ruta
//!
//! Puerto: 127.0.0.1:47820 (configurable con GHOST_REMOTE_PORT).

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

/// Comandos que el hilo HTTP encola para el hilo de UI (donde vive Browser).
pub enum RemoteCommand {
    /// Petición síncrona: el hilo de UI responde por `reply`.
    Sync {
        request: Request,
        reply: Sender<Value>,
    },
    /// Evaluación JS en un WebView2: la respuesta llega cuando la página
    /// devuelve el resultado por IPC (ver `remote_eval` en browser.rs).
    Eval {
        tab_id: u64,
        js: String,
        done: Sender<Value>,
    },
    /// Método crudo del Chrome DevTools Protocol: la respuesta llega por el
    /// handler `CallDevToolsProtocolMethodCompleted` de WebView2.
    Cdp {
        tab_id: u64,
        method: String,
        params: String,
        done: Sender<Value>,
    },
    /// Captura de pantalla PNG vía `CapturePreview`; el resultado (con la
    /// ruta del archivo) llega por el handler de finalización.
    Shot {
        tab_id: u64,
        path: Option<String>,
        done: Sender<Value>,
    },
}

/// Petición ya parseada: método, ruta y cuerpo JSON.
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Value,
}

/// Canal hacia el hilo de UI. `None` si el bind del puerto falló.
pub fn start() -> Option<Receiver<RemoteCommand>> {
    let port: u16 = std::env::var("GHOST_REMOTE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(47820);
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
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    let response = route(&request, &tx);
    let _ = write_response(&mut stream, &response);
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    // Headers + cuerpo: leemos hasta \r\n\r\n y luego el Content-Length.
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 64 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let content_length: usize = lines
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
    Some(Request { method, path, body })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn route(request: &Request, tx: &Sender<RemoteCommand>) -> Value {
    let path = request.path.split('?').next().unwrap_or("/");
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();

    // GET /health no necesita el hilo de UI.
    if request.method == "GET" && path == "/health" {
        return json!({"ok": true, "name": "ghost", "version": env!("CARGO_PKG_VERSION")});
    }

    // Eval tiene su propio canal de respuesta (llega por IPC de la página).
    if request.method == "POST"
        && segments.len() == 3
        && segments[0] == "tabs"
        && segments[2] == "eval"
        && let Ok(tab_id) = segments[1].parse::<u64>()
    {
        let js = request.body.get("js").and_then(Value::as_str).unwrap_or("");
        if js.is_empty() {
            return json!({"ok": false, "error": "falta 'js'"});
        }
        let (done_tx, done_rx) = channel::<Value>();
        if tx
            .send(RemoteCommand::Eval {
                tab_id,
                js: js.to_string(),
                done: done_tx,
            })
            .is_err()
        {
            return json!({"ok": false, "error": "Ghost no está procesando comandos"});
        }
        return done_rx
            .recv_timeout(std::time::Duration::from_secs(15))
            .unwrap_or_else(|_| json!({"ok": false, "error": "timeout esperando la página"}));
    }

    // CDP crudo: POST /tabs/{id}/cdp {"method": "...", "params": {...}}
    if request.method == "POST"
        && segments.len() == 3
        && segments[0] == "tabs"
        && segments[2] == "cdp"
        && let Ok(tab_id) = segments[1].parse::<u64>()
    {
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
                tab_id,
                method: method.to_string(),
                params,
                done: done_tx,
            })
            .is_err()
        {
            return json!({"ok": false, "error": "Ghost no está procesando comandos"});
        }
        return done_rx
            .recv_timeout(std::time::Duration::from_secs(15))
            .unwrap_or_else(|_| json!({"ok": false, "error": "timeout esperando CDP"}));
    }

    // Screenshot: POST /tabs/{id}/screenshot {"path": "..."?}
    // Devuelve la ruta del PNG guardado (el contenido es binario, no JSON).
    if request.method == "POST"
        && segments.len() == 3
        && segments[0] == "tabs"
        && segments[2] == "screenshot"
        && let Ok(tab_id) = segments[1].parse::<u64>()
    {
        let path = request
            .body
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string);
        let (done_tx, done_rx) = channel::<Value>();
        if tx
            .send(RemoteCommand::Shot {
                tab_id,
                path,
                done: done_tx,
            })
            .is_err()
        {
            return json!({"ok": false, "error": "Ghost no está procesando comandos"});
        }
        return done_rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap_or_else(|_| json!({"ok": false, "error": "timeout capturando pantalla"}));
    }

    let (reply_tx, reply_rx) = channel::<Value>();
    let command = RemoteCommand::Sync {
        request: Request {
            method: request.method.clone(),
            path: request.path.clone(),
            body: request.body.clone(),
        },
        reply: reply_tx,
    };
    if tx.send(command).is_err() {
        return json!({"ok": false, "error": "Ghost no está procesando comandos"});
    }
    reply_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap_or_else(|_| json!({"ok": false, "error": "timeout"}))
}

fn write_response(stream: &mut TcpStream, body: &Value) -> std::io::Result<()> {
    let text = serde_json::to_string(body).unwrap_or_else(|_| "{}".into());
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        text.len(),
        text
    )
}
