//! Contrato de cable del API de control local: lo comparten los hosts
//! (Ghost, `wisp-agent serve`) y el puente MCP. Un host implementa el
//! servidor sobre estas rutas y el puente las consume, así el mismo cliente
//! MCP funciona contra cualquiera de los dos.
//!
//!   GET  /health                     -> {"ok":true}
//!   GET  /tabs[?engine=wisp]         -> {ok, tabs: [{id,title,url,engine,active}]}
//!   POST /tabs                       {"url": "..."}         -> nueva pestaña
//!   POST /tabs/{id|active}/navigate  {"url": "..."}
//!   POST /tabs/{id|active}/back|forward|reload|close|activate|text
//!   POST /tabs/{id|active}/eval      {"js": "..."}
//!   POST /tabs/{id|active}/click|type|press|scroll|move
//!   POST /tabs/{id|active}/cdp       {"method","params"}
//!   POST /tabs/{id|active}/screenshot {"path"?}

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

/// Resultado de leer una petición HTTP/1.1 local sin enmascarar JSON inválido.
#[derive(Debug, Clone)]
pub enum ReadRequest {
    Ok(Request),
    InvalidJson(String),
    Eof,
}

/// Destino de una ruta `/tabs/{target}/...`: un ID numérico explícito o `active`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabTarget {
    Id(u64),
    Active,
}

impl TabTarget {
    fn parse(segment: &str) -> Option<Self> {
        if segment.eq_ignore_ascii_case("active") {
            Some(Self::Active)
        } else {
            segment.parse::<u64>().ok().map(Self::Id)
        }
    }
}

/// Ruta del contrato. Los identificadores inválidos caen en [`Route::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Health,
    ListTabs,
    OpenTab,
    Navigate(TabTarget),
    Back(TabTarget),
    Forward(TabTarget),
    Reload(TabTarget),
    Close(TabTarget),
    Activate(TabTarget),
    Text(TabTarget),
    Snapshot(TabTarget),
    Eval(TabTarget),
    Click(TabTarget),
    Fill(TabTarget),
    SelectOption(TabTarget),
    Type(TabTarget),
    Press(TabTarget),
    Scroll(TabTarget),
    Move(TabTarget),
    WaitFor(TabTarget),
    NetworkRequests(TabTarget),
    ConsoleMessages(TabTarget),
    Events(TabTarget),
    Cdp(TabTarget),
    Screenshot(TabTarget),
    Pdf(TabTarget),
    Unknown,
}

impl Request {
    /// Extrae un parámetro de consulta (`?key=value`) de `self.path`.
    pub fn query_param(&self, key: &str) -> Option<&str> {
        let (_, query) = self.path.split_once('?')?;
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=')
                && k.eq_ignore_ascii_case(key)
            {
                return Some(v);
            }
        }
        None
    }

    /// Mapea método + ruta al contrato.
    pub fn route(&self) -> Route {
        let path = self.path.split('?').next().unwrap_or("/");
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        match (self.method.as_str(), segments.as_slice()) {
            ("GET", ["health"]) => Route::Health,
            ("GET", ["tabs"]) => Route::ListTabs,
            ("POST", ["tabs"]) => Route::OpenTab,
            ("GET", ["tabs", target, tail]) => {
                let Some(target) = TabTarget::parse(target) else {
                    return Route::Unknown;
                };
                match *tail {
                    "network" => Route::NetworkRequests(target),
                    "console" => Route::ConsoleMessages(target),
                    "events" => Route::Events(target),
                    _ => Route::Unknown,
                }
            }
            ("POST", ["tabs", target, tail]) => {
                let Some(target) = TabTarget::parse(target) else {
                    return Route::Unknown;
                };
                match *tail {
                    "navigate" => Route::Navigate(target),
                    "back" => Route::Back(target),
                    "forward" => Route::Forward(target),
                    "reload" => Route::Reload(target),
                    "close" => Route::Close(target),
                    "activate" => Route::Activate(target),
                    "text" => Route::Text(target),
                    "snapshot" => Route::Snapshot(target),
                    "eval" => Route::Eval(target),
                    "click" => Route::Click(target),
                    "fill" => Route::Fill(target),
                    "select_option" => Route::SelectOption(target),
                    "type" => Route::Type(target),
                    "press" => Route::Press(target),
                    "scroll" => Route::Scroll(target),
                    "move" => Route::Move(target),
                    "wait_for" => Route::WaitFor(target),
                    "network" => Route::NetworkRequests(target),
                    "console" => Route::ConsoleMessages(target),
                    "events" => Route::Events(target),
                    "cdp" => Route::Cdp(target),
                    "screenshot" => Route::Screenshot(target),
                    "pdf" => Route::Pdf(target),
                    _ => Route::Unknown,
                }
            }
            _ => Route::Unknown,
        }
    }
}

/// Lee una petición HTTP/1.1 completa distinguiendo cuerpo vacío de JSON malformado.
pub fn read_request_checked(stream: &mut TcpStream) -> ReadRequest {
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let Ok(n) = stream.read(&mut chunk) else {
            return ReadRequest::Eof;
        };
        if n == 0 {
            return ReadRequest::Eof;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 64 * 1024 {
            return ReadRequest::Eof;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let Some(request_line) = lines.next() else {
        return ReadRequest::Eof;
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return ReadRequest::Eof;
    };
    let method = method.to_string();
    let path = path.to_string();
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
        let Ok(n) = stream.read(&mut chunk) else {
            return ReadRequest::Eof;
        };
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    let body = if body.iter().all(u8::is_ascii_whitespace) {
        Value::Null
    } else {
        match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(error) => return ReadRequest::InvalidJson(format!("JSON inválido: {error}")),
        }
    };
    ReadRequest::Ok(Request { method, path, body })
}

/// Lee una petición HTTP/1.1 completa (cabeceras + cuerpo por `Content-Length`).
pub fn read_request(stream: &mut TcpStream) -> Option<Request> {
    match read_request_checked(stream) {
        ReadRequest::Ok(req) => Some(req),
        ReadRequest::InvalidJson(_) | ReadRequest::Eof => None,
    }
}

/// Escribe una respuesta JSON `200 OK` con `Connection: close`.
pub fn write_response(stream: &mut TcpStream, body: &Value) -> std::io::Result<()> {
    write_status_response(stream, 200, "OK", body)
}

/// Escribe una respuesta JSON con código de estado HTTP explícito.
pub fn write_status_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &Value,
) -> std::io::Result<()> {
    let text = serde_json::to_string(body).unwrap_or_else(|_| "{}".into());
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
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
    use std::net::TcpListener;

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
        assert_eq!(
            req("GET", "/tabs?engine=wisp").query_param("engine"),
            Some("wisp")
        );
        assert_eq!(req("POST", "/tabs").route(), Route::OpenTab);
        assert_eq!(
            req("POST", "/tabs/7/navigate").route(),
            Route::Navigate(TabTarget::Id(7))
        );
        assert_eq!(
            req("POST", "/tabs/active/eval").route(),
            Route::Eval(TabTarget::Active)
        );
        assert_eq!(
            req("POST", "/tabs/active/snapshot").route(),
            Route::Snapshot(TabTarget::Active)
        );
        assert_eq!(
            req("POST", "/tabs/7/back").route(),
            Route::Back(TabTarget::Id(7))
        );
        assert_eq!(
            req("POST", "/tabs/7/text?x=1").route(),
            Route::Text(TabTarget::Id(7))
        );
        assert_eq!(
            req("POST", "/tabs/7/cdp").route(),
            Route::Cdp(TabTarget::Id(7))
        );
        assert_eq!(
            req("POST", "/tabs/7/screenshot").route(),
            Route::Screenshot(TabTarget::Id(7))
        );
    }

    #[test]
    fn unknown_routes_and_bad_ids_are_rejected() {
        assert_eq!(req("GET", "/nope").route(), Route::Unknown);
        assert_eq!(req("POST", "/tabs/abc/navigate").route(), Route::Unknown);
        assert_eq!(req("POST", "/tabs/7/nope").route(), Route::Unknown);
        assert_eq!(req("GET", "/tabs/7/text").route(), Route::Unknown);
    }

    #[test]
    fn malformed_json_is_reported_not_masked_as_null() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let handle = std::thread::spawn(move || {
            let mut client = TcpStream::connect(addr).expect("connect");
            let bad = br#"{"js": "unterminated"#;
            let raw = format!(
                "POST /tabs/1/eval HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
                bad.len()
            );
            client.write_all(raw.as_bytes()).expect("write head");
            client.write_all(bad).expect("write body");
            let mut resp = String::new();
            client.read_to_string(&mut resp).expect("read resp");
            resp
        });

        let (mut server_stream, _) = listener.accept().expect("accept");
        match read_request_checked(&mut server_stream) {
            ReadRequest::InvalidJson(msg) => {
                assert!(msg.contains("JSON inválido"), "{msg}");
                write_status_response(
                    &mut server_stream,
                    400,
                    "Bad Request",
                    &json!({"ok": false, "error": msg}),
                )
                .expect("write 400");
            }
            other => panic!("expected InvalidJson, got {other:?}"),
        }

        drop(server_stream);
        let resp = handle.join().expect("join");
        assert!(resp.starts_with("HTTP/1.1 400 Bad Request"), "{resp}");
        assert!(resp.contains("JSON inválido"), "{resp}");
    }

    #[test]
    fn empty_body_remains_valid_null() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let handle = std::thread::spawn(move || {
            let mut client = TcpStream::connect(addr).expect("connect");
            client
                .write_all(b"POST /tabs/active/back HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .expect("write");
        });

        let (mut server_stream, _) = listener.accept().expect("accept");
        match read_request_checked(&mut server_stream) {
            ReadRequest::Ok(req) => {
                assert_eq!(req.route(), Route::Back(TabTarget::Active));
                assert_eq!(req.body, Value::Null);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        handle.join().expect("join");
    }
}
