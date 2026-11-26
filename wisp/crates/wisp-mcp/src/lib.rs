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

fn resolve_tab_id(args: &Value) -> Result<u64, String> {
    if let Some(id) = args.get("tab_id").and_then(Value::as_u64) {
        return Ok(id);
    }
    let tabs = client::http("GET", "/tabs", None)?;
    let list = tabs
        .get("tabs")
        .and_then(Value::as_array)
        .ok_or_else(|| "no hay pestañas abiertas".to_string())?;
    if let Some(active) = list
        .iter()
        .find(|t| t.get("active").and_then(Value::as_bool) == Some(true))
        .and_then(|t| t.get("id").and_then(Value::as_u64))
    {
        return Ok(active);
    }
    list.first()
        .and_then(|t| t.get("id").and_then(Value::as_u64))
        .ok_or_else(|| "no hay pestañas abiertas".to_string())
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
            let allowed = ["back", "forward", "reload", "close", "activate", "text"];
            if !allowed.contains(&action) {
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
                "ref": args.get("ref"),
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

        // ---- Herramientas unificadas `browser_*` (ítem 21) ----
        "browser_navigate" => {
            let url = args.get("url").and_then(Value::as_str).unwrap_or("");
            if let Some(id) = args.get("tab_id").and_then(Value::as_u64) {
                client::http(
                    "POST",
                    &format!("/tabs/{id}/navigate"),
                    Some(&json!({"url": url}).to_string()),
                )
            } else {
                match resolve_tab_id(args) {
                    Ok(id) => client::http(
                        "POST",
                        &format!("/tabs/{id}/navigate"),
                        Some(&json!({"url": url}).to_string()),
                    ),
                    Err(_) => client::http("POST", "/tabs", Some(&json!({"url": url}).to_string())),
                }
            }
        }
        "browser_snapshot" => match resolve_tab_id(args) {
            Ok(id) => client::http(
                "POST",
                &format!("/tabs/{id}/snapshot"),
                Some(&json!({"width": args.get("width")}).to_string()),
            ),
            Err(error) => Err(error),
        },
        "browser_screenshot" => match resolve_tab_id(args) {
            Ok(id) => client::http(
                "POST",
                &format!("/tabs/{id}/screenshot"),
                Some(&json!({"path": args.get("path")}).to_string()),
            ),
            Err(error) => Err(error),
        },
        "browser_pdf" => match resolve_tab_id(args) {
            Ok(id) => client::http(
                "POST",
                &format!("/tabs/{id}/pdf"),
                Some(&json!({"path": args.get("path")}).to_string()),
            ),
            Err(error) => Err(error),
        },
        "browser_click" => match resolve_tab_id(args) {
            Ok(id) => client::http(
                "POST",
                &format!("/tabs/{id}/click"),
                Some(
                    &json!({
                        "ref": args.get("ref"),
                        "x": args.get("x"),
                        "y": args.get("y"),
                        "button": args.get("button"),
                        "clicks": args.get("clicks"),
                    })
                    .to_string(),
                ),
            ),
            Err(error) => Err(error),
        },
        "browser_fill" => match resolve_tab_id(args) {
            Ok(id) => client::http(
                "POST",
                &format!("/tabs/{id}/fill"),
                Some(
                    &json!({
                        "ref": args.get("ref"),
                        "name": args.get("name"),
                        "value": args.get("value"),
                    })
                    .to_string(),
                ),
            ),
            Err(error) => Err(error),
        },
        "browser_type" => match resolve_tab_id(args) {
            Ok(id) => client::http(
                "POST",
                &format!("/tabs/{id}/type"),
                Some(
                    &json!({
                        "ref": args.get("ref"),
                        "text": args.get("text"),
                    })
                    .to_string(),
                ),
            ),
            Err(error) => Err(error),
        },
        "browser_press_key" => match resolve_tab_id(args) {
            Ok(id) => client::http(
                "POST",
                &format!("/tabs/{id}/press"),
                Some(&json!({"key": args.get("key")}).to_string()),
            ),
            Err(error) => Err(error),
        },
        "browser_select_option" => match resolve_tab_id(args) {
            Ok(id) => client::http(
                "POST",
                &format!("/tabs/{id}/select_option"),
                Some(
                    &json!({
                        "ref": args.get("ref"),
                        "value": args.get("value"),
                    })
                    .to_string(),
                ),
            ),
            Err(error) => Err(error),
        },
        "browser_evaluate" => match resolve_tab_id(args) {
            Ok(id) => {
                let js = args
                    .get("expression")
                    .or_else(|| args.get("js"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                client::http(
                    "POST",
                    &format!("/tabs/{id}/eval"),
                    Some(&json!({"js": js}).to_string()),
                )
            }
            Err(error) => Err(error),
        },
        "browser_wait_for" => match resolve_tab_id(args) {
            Ok(id) => client::http(
                "POST",
                &format!("/tabs/{id}/wait_for"),
                Some(
                    &json!({
                        "text": args.get("text"),
                        "selector": args.get("selector"),
                        "timeout_ms": args.get("timeout_ms"),
                    })
                    .to_string(),
                ),
            ),
            Err(error) => Err(error),
        },
        "browser_network_requests" => match resolve_tab_id(args) {
            Ok(id) => client::http("GET", &format!("/tabs/{id}/network"), None),
            Err(error) => Err(error),
        },
        "browser_console_messages" => match resolve_tab_id(args) {
            Ok(id) => client::http("GET", &format!("/tabs/{id}/console"), None),
            Err(error) => Err(error),
        },
        "browser_close" => match resolve_tab_id(args) {
            Ok(id) => client::http("POST", &format!("/tabs/{id}/close"), None),
            Err(error) => Err(error),
        },
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
            "name": "browser_navigate",
            "description": "Navega la pestaña activa (o abre una nueva) a una URL http(s).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "tab_id": {"type": "integer"}
                },
                "required": ["url"]
            }
        },
        {
            "name": "browser_snapshot",
            "description": "Devuelve un snapshot accesible con referencias estables (@e1, @e2...), texto accesible, rol, rectángulo y centralidad.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "width": {"type": "number"}
                }
            }
        },
        {
            "name": "browser_screenshot",
            "description": "Captura la pestaña como PNG y devuelve la ruta y dimensiones.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "path": {"type": "string"}
                }
            }
        },
        {
            "name": "browser_pdf",
            "description": "Exporta la pestaña actual a un archivo PDF y devuelve la ruta.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "path": {"type": "string"}
                }
            }
        },
        {
            "name": "browser_click",
            "description": "Hace clic por referencia estable (@e1, @e2...) o por coordenadas CSS (x, y).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "ref": {"type": "string"},
                    "x": {"type": "number"},
                    "y": {"type": "number"},
                    "button": {"type": "string", "enum": ["left", "middle", "right"]},
                    "clicks": {"type": "integer"}
                }
            }
        },
        {
            "name": "browser_fill",
            "description": "Rellena un campo de formulario identificado por @ref o nombre.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "ref": {"type": "string"},
                    "name": {"type": "string"},
                    "value": {"type": "string"}
                },
                "required": ["value"]
            }
        },
        {
            "name": "browser_type",
            "description": "Escribe texto en el elemento enfocado o indicado por @ref.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "ref": {"type": "string"},
                    "text": {"type": "string"}
                },
                "required": ["text"]
            }
        },
        {
            "name": "browser_press_key",
            "description": "Pulsa una tecla (Enter, Tab, Escape, etc.).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "key": {"type": "string"}
                },
                "required": ["key"]
            }
        },
        {
            "name": "browser_select_option",
            "description": "Selecciona una opción en un <select> identificado por @ref.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "ref": {"type": "string"},
                    "value": {"type": "string"}
                },
                "required": ["ref", "value"]
            }
        },
        {
            "name": "browser_evaluate",
            "description": "Evalúa JavaScript en la pestaña actual y devuelve el resultado.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "expression": {"type": "string"},
                    "js": {"type": "string"}
                }
            }
        },
        {
            "name": "browser_wait_for",
            "description": "Espera hasta que aparezca un texto o selector en la página o venza timeout_ms.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "text": {"type": "string"},
                    "selector": {"type": "string"},
                    "timeout_ms": {"type": "integer"}
                }
            }
        },
        {
            "name": "browser_network_requests",
            "description": "Lista las peticiones de red registradas para la pestaña con timings y estado.",
            "inputSchema": {
                "type": "object",
                "properties": {"tab_id": {"type": "integer"}}
            }
        },
        {
            "name": "browser_console_messages",
            "description": "Devuelve los mensajes y avisos de consola emitidos por la pestaña.",
            "inputSchema": {
                "type": "object",
                "properties": {"tab_id": {"type": "integer"}}
            }
        },
        {
            "name": "browser_close",
            "description": "Cierra la pestaña indicada o la pestaña activa.",
            "inputSchema": {
                "type": "object",
                "properties": {"tab_id": {"type": "integer"}}
            }
        },
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
            "description": "Evalúa JavaScript en una pestaña y devuelve el resultado.",
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
            "description": "Clic de ratón por @ref o coordenadas CSS (x, y).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": {"type": "integer"},
                    "ref": {"type": "string"},
                    "x": {"type": "number"},
                    "y": {"type": "number"},
                    "button": {"type": "string", "enum": ["left", "middle", "right"]},
                    "clicks": {"type": "integer"}
                },
                "required": ["tab_id"]
            }
        },
        {
            "name": "ghost_type",
            "description": "Inserta texto en el elemento con foco de la página.",
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
            "description": "Pulsa una tecla: enter, tab, escape/esc, backspace, delete, space, etc.",
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
            "description": "Rueda del ratón (CDP mouseWheel).",
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
            "description": "Mueve el cursor del ratón a coordenadas CSS de la página.",
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
            "description": "Captura la pestaña como PNG y devuelve la ruta del archivo guardado.",
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
            "description": "Llama cualquier método del Chrome DevTools Protocol y devuelve el resultado JSON.",
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
