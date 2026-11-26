//! Contrato de cable del API de control local: lo comparten los hosts
//! (Ghost, `wisp-agent serve`) y el puente MCP. Un host implementa el
//! servidor sobre estas rutas y el puente las consume, así el mismo cliente
//! MCP funciona contra cualquiera de los dos.
//!
//!   GET  /health                 -> {"ok":true}
//!   GET  /tabs                   -> [{id,title,url,engine,active}]
//!   POST /tabs                   {"url": "..."}         -> nueva pestaña
//!   POST /tabs/{id}/navigate     {"url": "..."}
//!   POST /tabs/{id}/back|forward|reload|close|activate|text
//!   POST /tabs/{id}/eval         {"js": "..."}
//!   POST /tabs/{id}/click|type|press|scroll|move
//!   POST /tabs/{id}/cdp          {"method","params"}
//!   POST /tabs/{id}/screenshot   {"path"?}

use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;

/// Puerto por defecto del API de control.
pub const DEFAULT_PORT: u16 = 47820;

/// Puerto configurado: `WISP_REMOTE_PORT`, luego `GHOST_REMOTE_PORT`, luego
/// [`DEFAULT_PORT`]. El mismo host y el mismo cliente leen la misma clave.
pub fn configured_port() -> u16 {
    ["WISP_REMOTE_PORT", "GHOST_REMOTE_PORT"]
        .iter()
        .find_map(|key| std::env::var(key).ok()?.trim().parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

/// Petición ya parseada: método, ruta y cuerpo JSON.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Value,
}

/// Ruta del contrato. Los identificadores inválidos caen en [`Route::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Health,
    ListTabs,
    OpenTab,
    Navigate(u64),
    Back(u64),
    Forward(u64),
    Reload(u64),
    Close(u64),
    Activate(u64),
    Text(u64),
    Snapshot(u64),
    Eval(u64),
    Click(u64),
    Fill(u64),
    SelectOption(u64),
    Type(u64),
    Press(u64),
    Scroll(u64),
    Move(u64),
    WaitFor(u64),
    NetworkRequests(u64),
    ConsoleMessages(u64),
    Events(u64),
    Cdp(u64),
    Screenshot(u64),
    Pdf(u64),
    Unknown,
}

impl Request {
    /// Mapea método + ruta al contrato.
    pub fn route(&self) -> Route {
        let path = self.path.split('?').next().unwrap_or("/");
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        match (self.method.as_str(), segments.as_slice()) {
            ("GET", ["health"]) => Route::Health,
            ("GET", ["tabs"]) => Route::ListTabs,
            ("POST", ["tabs"]) => Route::OpenTab,
            ("GET", ["tabs", id, tail]) => {
                let Ok(id) = id.parse::<u64>() else {
                    return Route::Unknown;
                };
                match *tail {
                    "network" => Route::NetworkRequests(id),
                    "console" => Route::ConsoleMessages(id),
                    "events" => Route::Events(id),
                    _ => Route::Unknown,
                }
            }
            ("POST", ["tabs", id, tail]) => {
                let Ok(id) = id.parse::<u64>() else {
                    return Route::Unknown;
                };
                match *tail {
                    "navigate" => Route::Navigate(id),
                    "back" => Route::Back(id),
                    "forward" => Route::Forward(id),
                    "reload" => Route::Reload(id),
                    "close" => Route::Close(id),
                    "activate" => Route::Activate(id),
                    "text" => Route::Text(id),
                    "snapshot" => Route::Snapshot(id),
                    "eval" => Route::Eval(id),
                    "click" => Route::Click(id),
                    "fill" => Route::Fill(id),
                    "select_option" => Route::SelectOption(id),
                    "type" => Route::Type(id),
                    "press" => Route::Press(id),
                    "scroll" => Route::Scroll(id),
                    "move" => Route::Move(id),
                    "wait_for" => Route::WaitFor(id),
                    "network" => Route::NetworkRequests(id),
                    "console" => Route::ConsoleMessages(id),
                    "events" => Route::Events(id),
                    "cdp" => Route::Cdp(id),
                    "screenshot" => Route::Screenshot(id),
                    "pdf" => Route::Pdf(id),
                    _ => Route::Unknown,
                }
            }
            _ => Route::Unknown,
        }
    }
}

/// Lee una petición HTTP/1.1 completa (cabeceras + cuerpo por `Content-Length`).
pub fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
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

/// Escribe una respuesta JSON con `Connection: close`.
pub fn write_response(stream: &mut TcpStream, body: &Value) -> std::io::Result<()> {
    let text = serde_json::to_string(body).unwrap_or_else(|_| "{}".into());
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        text.len(),
        text
    )
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(method: &str, path: &str) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            body: json!({}),
        }
    }

    #[test]
    fn routes_match_the_contract() {
        assert_eq!(req("GET", "/health").route(), Route::Health);
        assert_eq!(req("GET", "/tabs").route(), Route::ListTabs);
        assert_eq!(req("POST", "/tabs").route(), Route::OpenTab);
        assert_eq!(req("POST", "/tabs/7/navigate").route(), Route::Navigate(7));
        assert_eq!(req("POST", "/tabs/7/back").route(), Route::Back(7));
        assert_eq!(req("POST", "/tabs/7/text?x=1").route(), Route::Text(7));
        assert_eq!(req("POST", "/tabs/7/cdp").route(), Route::Cdp(7));
        assert_eq!(
            req("POST", "/tabs/7/screenshot").route(),
            Route::Screenshot(7)
        );
    }

    #[test]
    fn unknown_routes_and_bad_ids_are_rejected() {
        assert_eq!(req("GET", "/nope").route(), Route::Unknown);
        assert_eq!(req("POST", "/tabs/abc/navigate").route(), Route::Unknown);
        assert_eq!(req("POST", "/tabs/7/nope").route(), Route::Unknown);
        assert_eq!(req("GET", "/tabs/7/text").route(), Route::Unknown);
    }
}
