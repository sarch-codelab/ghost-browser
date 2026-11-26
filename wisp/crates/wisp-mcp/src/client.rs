//! Cliente del API de control local. Una petición por conexión,
//! `Connection: close`, sin dependencias HTTP externas.

use crate::control;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Host del API: `127.0.0.1:<puerto configurado>`.
pub fn api_base() -> String {
    format!("127.0.0.1:{}", control::configured_port())
}

/// Ejecuta una petición contra el API de control y devuelve el JSON.
pub fn http(method: &str, path: &str, body: Option<&str>) -> Result<Value, String> {
    let base = api_base();
    let mut stream = TcpStream::connect(&base).map_err(|e| format!("el host no responde: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let body = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {base}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.flush())
        .map_err(|e| e.to_string())?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&response);
    let json_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    serde_json::from_str(&text[json_start..]).map_err(|e| format!("respuesta inválida: {e}"))
}
