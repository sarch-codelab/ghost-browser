//! Puente MCP (Model Context Protocol) sobre el API de control local de un
//! host Wisp/Ghost. Habla JSON-RPC 2.0 por stdio, de modo que cualquier
//! cliente MCP (Claude, Cursor, Devin, scripts propios) puede conducir el
//! navegador como herramienta nativa.
//!
//! Es agnóstico del host: apunta al API de control en `WISP_REMOTE_PORT` /
//! `GHOST_REMOTE_PORT` (por defecto `127.0.0.1:47820`), así el **mismo cliente
//! y la misma configuración** funcionan contra Ghost o contra
//! `wisp-agent serve`.

pub mod client;
pub mod control;

use serde_json::{Value, json};
use std::io::{BufRead, Write};

/// Versión del protocolo MCP anunciada si el cliente no propone otra.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Bucle stdio: lee JSON-RPC por línea y responde por stdout.
pub fn run_stdio() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(response) = handle_request(&request) {
            let _ = writeln!(stdout, "{response}");
            let _ = stdout.flush();
        }
    }
}

/// Responde una petición JSON-RPC 2.0. `None` para notificaciones.
pub fn handle_request(request: &Value) -> Option<Value> {
    let id = request.get("id").cloned()?;
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => {
            let version = request
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION);
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "wisp-mcp", "version": env!("CARGO_PKG_VERSION")}
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tool_definitions()}),
        "tools/call" => {
            let params = request.get("params").cloned().unwrap_or(json!({}));
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            call_tool(name, &args)
        }
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("método no soportado: {method}")}
            }));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn tool_result(value: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default()}],
        "isError": value.get("ok").is_some_and(|ok| ok == false),
    })
}

/// Ejecuta una herramienta del contrato contra el API de control.
pub fn call_tool(name: &str, args: &Value) -> Value {
    let result = match name {
        "ghost_health" => client::http("GET", "/health", None),
        "ghost_tabs" => client::http("GET", "/tabs", None),
        "ghost_open_tab" => {
            let url = args.get("url").and_then(Value::as_str).unwrap_or("");
            client::http("POST", "/tabs", Some(&json!({"url": url}).to_string()))
        }
        "ghost_navigate" => {
            let id = args.get("tab_id").and_then(Value::as_u64).unwrap_or(0);
            let url = args.get("url").and_then(Value::as_str).unwrap_or("");
            client::http(
                "POST",
                &format!("/tabs/{id}/navigate"),
                Some(&json!({"url": url}).to_string()),
            )
        }
        "ghost_tab_action" => {
            let id = args.get("tab_id").and_then(Value::as_u64).unwrap_or(0);
            let action = args.get("action").and_then(Value::as_str).unwrap_or("");
            if !matches!(
                action,
                "back" | "forward" | "reload" | "close" | "activate" | "text"
            ) {
                return tool_result(json!({"ok": false, "error": "acción desconocida"}));
            }
            client::http("POST", &format!("/tabs/{id}/{action}"), None)
        }
        "ghost_eval" => {
            let id = args.get("tab_id").and_then(Value::as_u64).unwrap_or(0);
            let js = args.get("js").and_then(Value::as_str).unwrap_or("");
            client::http(
                "POST",
                &format!("/tabs/{id}/eval"),
                Some(&json!({"js": js}).to_string()),
            )
        }
        "ghost_click" | "ghost_type" | "ghost_press" | "ghost_scroll" | "ghost_move"
        | "ghost_screenshot" | "ghost_cdp" => {
            let id = args.get("tab_id").and_then(Value::as_u64).unwrap_or(0);
            let endpoint = name.trim_start_matches("ghost_");
            let body = json!({
                "x": args.get("x"),
                "y": args.get("y"),
                "button": args.get("button"),
                "clicks": args.get("clicks"),
                "text": args.get("text"),
                "key": args.get("key"),
                "deltaX": args.get("delta_x"),
                "deltaY": args.get("delta_y"),
                "path": args.get("path"),
                "method": args.get("method"),
                "params": args.get("params"),
            });
            client::http(
                "POST",
                &format!("/tabs/{id}/{endpoint}"),
                Some(&body.to_string()),
            )
        }
        _ => {
            return tool_result(
                json!({"ok": false, "error": format!("herramienta desconocida: {name}")}),
            );
        }
    };
    match result {
        Ok(value) => tool_result(value),
        Err(error) => tool_result(json!({"ok": false, "error": error})),
    }
}

/// Definición de herramientas expuesta por `tools/list`.
pub fn tool_definitions() -> Value {
    json!([
        {
            "name": "ghost_health",
            "description": "Comprueba que el host está abierto y su API de control responde.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "ghost_tabs",
            "description": "Lista las pestañas del host: id, título, url, motor (WebView2/Wisp), activa, cargando, suspendida.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "ghost_open_tab",
            "description": "Abre una pestaña nueva y navega a la URL indicada.",
            "inputSchema": {
                "type": "object",
                "properties": {"url": {"type": "string", "description": "URL http(s)"}},
                "required": ["url"]
            }
        },
        {
            "name": "ghost_navigate",
            "description": "Navega una pestaña existente a una URL (o búsqueda).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "url": {"type": "string"}
                },
                "required": ["tab_id", "url"]
            }
        },
        {
            "name": "ghost_tab_action",
            "description": "Acción sobre una pestaña: back, forward, reload, close, activate (traer al frente) o text (contenido legible).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "action": {"type": "string", "enum": ["back", "forward", "reload", "close", "activate", "text"]}
                },
                "required": ["tab_id", "action"]
            }
        },
        {
            "name": "ghost_eval",
            "description": "Evalúa JavaScript en una pestaña y devuelve el resultado. En Ghost (WebView2) soporta promesas; en el motor Wisp usa un contexto Boa efímero sobre el HTML de la página (ve el DOM, pero no las variables que definió la página).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "js": {"type": "string"}
                },
                "required": ["tab_id", "js"]
            }
        },
        {
            "name": "ghost_click",
            "description": "Clic de ratón real (CDP Input.dispatchMouseEvent) en coordenadas CSS de la página. button: left|middle|right, clicks: 1-3. En el motor Wisp headless un clic solo resuelve enlaces: navega si hay uno bajo (x, y).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "x": {"type": "number"},
                    "y": {"type": "number"},
                    "button": {"type": "string", "enum": ["left", "middle", "right"]},
                    "clicks": {"type": "integer"}
                },
                "required": ["tab_id", "x", "y"]
            }
        },
        {
            "name": "ghost_type",
            "description": "Inserta texto en el elemento con foco de la página (CDP Input.insertText). Enfoca antes con ghost_click si hace falta. Solo en hosts con WebView2: el motor Wisp headless no tiene foco.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "text": {"type": "string"}
                },
                "required": ["tab_id", "text"]
            }
        },
        {
            "name": "ghost_press",
            "description": "Pulsa una tecla: enter, tab, escape/esc, backspace, delete, space, arrowleft/up/right/down (o left/up/right/down), home, end, pageup, pagedown, insert. Solo en hosts con WebView2.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "key": {"type": "string"}
                },
                "required": ["tab_id", "key"]
            }
        },
        {
            "name": "ghost_scroll",
            "description": "Rueda del ratón (CDP mouseWheel). deltaY positivo baja, negativo sube; deltaX para horizontal. x/y opcionales fijan la posición. Solo en hosts con WebView2: el motor Wisp headless maqueta el documento completo.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "delta_x": {"type": "number"},
                    "delta_y": {"type": "number"},
                    "x": {"type": "number"},
                    "y": {"type": "number"}
                },
                "required": ["tab_id", "delta_y"]
            }
        },
        {
            "name": "ghost_move",
            "description": "Mueve el cursor del ratón a coordenadas CSS de la página (sin clicar). En el motor Wisp headless informa del enlace bajo (x, y) en vez de mover un cursor real.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "x": {"type": "number"},
                    "y": {"type": "number"}
                },
                "required": ["tab_id", "x", "y"]
            }
        },
        {
            "name": "ghost_screenshot",
            "description": "Captura la pestaña como PNG (WebView2 CapturePreview) y devuelve la ruta del archivo guardado. Lee el PNG para ver la página.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "path": {"type": "string", "description": "Ruta opcional del PNG; por defecto %TEMP%/ghost_shots/"}
                },
                "required": ["tab_id"]
            }
        },
        {
            "name": "ghost_cdp",
            "description": "Llama cualquier método del Chrome DevTools Protocol (p. ej. Runtime.evaluate, DOM.getContentQuads, Page.captureScreenshot) y devuelve el resultado JSON.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "method": {"type": "string"},
                    "params": {"type": "object"}
                },
                "required": ["tab_id", "method"]
            }
        }
    ])
}
