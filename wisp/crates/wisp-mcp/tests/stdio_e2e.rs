//! Prueba end-to-end del contrato de control a través del **puente MCP**.
//!
//! Levanta un `wisp-agent serve` real y le habla por el mismo canal que verá un
//! agente: JSON-RPC 2.0 por stdio contra el binario `wisp-mcp`. Recorre el ciclo
//! completo (`initialize`, `tools/list` y las herramientas del contrato) sobre
//! una página servida en local, sin tocar la red externa.
//!
//! Requiere los binarios `wisp-mcp` y `wisp-agent` compilados en el mismo
//! directorio de perfil, lo habitual al ejecutar `cargo test --workspace` (que
//! construye los binarios de todos los paquetes). Si falta `wisp-agent`, la
//! prueba se omite con un aviso en vez de fallar.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// Binario hermano en el directorio de perfil (`target/<perfil>/`).
fn sibling_binary(name: &str) -> Option<PathBuf> {
    let bridge = PathBuf::from(env!("CARGO_BIN_EXE_wisp-mcp"));
    let candidate = bridge
        .parent()?
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    candidate.exists().then_some(candidate)
}

/// Servidor HTTP mínimo: sirve la página de origen (con un enlace) y la de
/// destino, para poder comprobar una navegación sin salir a internet.
fn spawn_page_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind del servidor de prueba");
    let port = listener.local_addr().expect("addr local").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            let read = stream.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..read]);
            let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let body = if path.contains("next") {
                "<html><head><title>Destino</title></head><body><p>llegaste</p></body></html>"
            } else {
                "<html><head><title>Origen</title></head><body><a href=\"/next.html\">Siguiente</a></body></html>"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://127.0.0.1:{port}/index.html")
}

/// Puerto libre: se suelta enseguida para que lo tome `wisp-agent serve`.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr local").port();
    drop(listener);
    port
}

fn wait_for_port(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("`wisp-agent serve` no abrió el puerto {port}");
}

/// Proceso `wisp-agent serve`; se mata al soltarlo.
struct Host {
    child: Child,
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_host(binary: &Path, port: u16) -> Host {
    let child = Command::new(binary)
        .args(["serve", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("lanzar `wisp-agent serve`");
    wait_for_port(port);
    Host { child }
}

/// Puente MCP hablando JSON-RPC por stdio.
struct Bridge {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: u64,
}

impl Bridge {
    fn spawn(binary: &Path, port: u16) -> Self {
        let mut child = Command::new(binary)
            .env("WISP_REMOTE_PORT", port.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("lanzar `wisp-mcp`");
        let stdin = child.stdin.take().expect("stdin del puente");
        let reader = BufReader::new(child.stdout.take().expect("stdout del puente"));
        Self {
            child,
            stdin,
            reader,
            next_id: 1,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{request}").expect("escribir al puente");
        self.stdin.flush().expect("vaciar stdin");
        let mut line = String::new();
        let read = self.reader.read_line(&mut line).expect("leer del puente");
        assert!(
            read > 0,
            "el puente cerró su salida sin responder a `{method}`"
        );
        let value: Value = serde_json::from_str(line.trim()).expect("respuesta JSON-RPC válida");
        assert_eq!(value.get("id").and_then(Value::as_u64), Some(id));
        value.get("result").cloned().expect("campo `result`")
    }

    /// Llama una herramienta y devuelve el JSON que el puente puso como texto.
    fn tool(&mut self, name: &str, args: Value) -> Value {
        let result = self.request("tools/call", json!({"name": name, "arguments": args}));
        let text = result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("{}");
        serde_json::from_str(text).expect("el texto de la herramienta es JSON")
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_mcp_bridge_drives_wisp_agent_over_stdio() {
    let Some(host_binary) = sibling_binary("wisp-agent") else {
        eprintln!(
            "omitido: falta el binario `wisp-agent` en el directorio de perfil; \
             ejecuta `cargo test --workspace` para construir todos los binarios"
        );
        return;
    };
    let bridge_binary = PathBuf::from(env!("CARGO_BIN_EXE_wisp-mcp"));

    let page = spawn_page_server();
    let port = free_port();
    let _host = spawn_host(&host_binary, port);
    let mut bridge = Bridge::spawn(&bridge_binary, port);

    // `initialize` y `tools/list`: lo primero que pide cualquier agente.
    let init = bridge.request("initialize", json!({"protocolVersion": "2024-11-05"}));
    assert_eq!(
        init.pointer("/serverInfo/name").and_then(Value::as_str),
        Some("wisp-mcp")
    );
    let tools = bridge.request("tools/list", json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .expect("`tools` es una lista")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    for expected in [
        "ghost_health",
        "ghost_open_tab",
        "ghost_tabs",
        "ghost_navigate",
        "ghost_tab_action",
        "ghost_eval",
        "ghost_click",
        "ghost_move",
        "ghost_screenshot",
    ] {
        assert!(
            names.contains(&expected),
            "falta la herramienta `{expected}` en {names:?}"
        );
    }

    // El host responde y se identifica.
    let health = bridge.tool("ghost_health", json!({}));
    assert_eq!(health.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(
        health.get("name").and_then(Value::as_str),
        Some("wisp-agent")
    );

    // Abrir la página y comprobar que aparece en la lista de pestañas.
    let opened = bridge.tool("ghost_open_tab", json!({"url": page}));
    assert_eq!(
        opened.get("ok").and_then(Value::as_bool),
        Some(true),
        "{opened}"
    );
    let tab_id = opened
        .get("id")
        .and_then(Value::as_u64)
        .expect("id de pestaña");
    let tabs = bridge.tool("ghost_tabs", json!({}));
    let listed = tabs["tabs"].as_array().expect("`tabs` es una lista");
    assert_eq!(listed.len(), 1, "{tabs}");
    assert_eq!(
        listed[0].get("engine").and_then(Value::as_str),
        Some("Wisp")
    );
    assert_eq!(
        listed[0].get("url").and_then(Value::as_str),
        Some(page.as_str())
    );

    // Texto legible y evaluación de JS.
    let text = bridge.tool(
        "ghost_tab_action",
        json!({"tab_id": tab_id, "action": "text"}),
    );
    assert!(
        text.get("result")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("Siguiente"),
        "{text}"
    );
    let title = bridge.tool(
        "ghost_eval",
        json!({"tab_id": tab_id, "js": "document.title"}),
    );
    assert_eq!(
        title.get("result").and_then(Value::as_str),
        Some("Origen"),
        "{title}"
    );

    // `move` informa del enlace bajo el punto (sin cursor real).
    let moved = bridge.tool("ghost_move", json!({"tab_id": tab_id, "x": 20, "y": 15}));
    assert_eq!(
        moved.get("ok").and_then(Value::as_bool),
        Some(true),
        "{moved}"
    );

    // Captura: rasteriza el paint list a un PNG real y lo deja en disco.
    let shot_path = std::env::temp_dir().join("wisp_mcp_e2e.png");
    let shot = bridge.tool(
        "ghost_screenshot",
        json!({"tab_id": tab_id, "path": shot_path.to_string_lossy()}),
    );
    assert_eq!(
        shot.get("ok").and_then(Value::as_bool),
        Some(true),
        "{shot}"
    );
    assert_eq!(
        shot.get("width").and_then(Value::as_u64),
        Some(1280),
        "{shot}"
    );
    let png = std::fs::read(&shot_path).expect("leer el PNG capturado");
    assert_eq!(
        &png[..8],
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
        "la captura debe ser un PNG"
    );

    // `click` sobre el enlace navega. El hit-testing depende de la maquetación,
    // así que se prueban unas cuantas alturas hasta acertar el enlace.
    let mut navigated = String::new();
    for y in [5, 10, 15, 20, 25, 30] {
        let clicked = bridge.tool("ghost_click", json!({"tab_id": tab_id, "x": 20, "y": y}));
        assert_eq!(
            clicked.get("ok").and_then(Value::as_bool),
            Some(true),
            "{clicked}"
        );
        let url = clicked.get("url").and_then(Value::as_str).unwrap_or("");
        if url.contains("next.html") {
            navigated = url.to_owned();
            break;
        }
    }
    assert!(
        navigated.contains("next.html"),
        "el clic debería haber navegado al enlace bajo el punto"
    );

    // Tras navegar, la pestaña muestra la página de destino.
    let title_after = bridge.tool(
        "ghost_eval",
        json!({"tab_id": tab_id, "js": "document.title"}),
    );
    assert_eq!(
        title_after.get("result").and_then(Value::as_str),
        Some("Destino"),
        "{title_after}"
    );

    let _ = std::fs::remove_file(&shot_path);
}
