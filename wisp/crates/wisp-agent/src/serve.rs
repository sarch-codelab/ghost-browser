//! `wisp-agent serve`: expone el mismo API de control local que Ghost
//! (contrato [`wisp_mcp::control`]) apoyado en el motor Wisp, para que un
//! cliente MCP conduzca el motor headless igual que conduce Ghost.
//!
//! Cubre lo que el motor ya sabe hacer hoy (salud, pestañas, abrir, navegar,
//! historial, recargar, cerrar, activar, texto legible, evaluación de JS,
//! entrada por hit-testing y captura). La evaluación levanta un contexto Boa
//! efímero sobre el HTML de la página, así que ve el DOM pero no las variables
//! que definió la página. La captura rasteriza el paint list a un PNG, que es
//! lo que deja al agente ver la página y elegir coordenadas para `click`. Las
//! operaciones que dependen de foco, eventos o CDP nativo devuelven un error
//! limpio en vez de fallar en silencio.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, MutexGuard};
use wisp_engine::{FetchedPage, fetch_url};
use wisp_mcp::control::{self, Route};

struct Tab {
    id: u64,
    url: String,
    title: String,
    history: Vec<String>,
    index: usize,
    page: Option<FetchedPage>,
    error: Option<String>,
}

#[derive(Default)]
struct State {
    tabs: Vec<Tab>,
    active: Option<u64>,
    next_id: u64,
}

pub fn run(args: &[String]) -> Result<()> {
    let mut port = control::configured_port();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--port" => {
                index += 1;
                port = args
                    .get(index)
                    .context("`--port` needs a value")?
                    .parse()
                    .context("`--port` must be a number")?;
            }
            flag if flag.starts_with("--") => bail!("unknown flag `{flag}`"),
            value => bail!("unexpected argument `{value}`"),
        }
        index += 1;
    }

    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("no se pudo abrir 127.0.0.1:{port}"))?;
    eprintln!("wisp-agent serve — API de control en http://127.0.0.1:{port}");
    eprintln!("apunta el puente MCP con WISP_REMOTE_PORT={port}");

    let state = Arc::new(Mutex::new(State {
        next_id: 1,
        ..Default::default()
    }));

    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let state = state.clone();
        std::thread::spawn(move || {
            let Some(request) = control::read_request(&mut stream) else {
                return;
            };
            let response = handle(&request, &state);
            let _ = control::write_response(&mut stream, &response);
        });
    }
    Ok(())
}

fn handle(request: &control::Request, state: &Arc<Mutex<State>>) -> Value {
    match request.route() {
        Route::Health => {
            json!({"ok": true, "name": "wisp-agent", "version": env!("CARGO_PKG_VERSION")})
        }
        Route::ListTabs => {
            let state = lock(state);
            let tabs: Vec<Value> = state
                .tabs
                .iter()
                .map(|tab| tab_json(tab, state.active))
                .collect();
            json!({"ok": true, "tabs": tabs})
        }
        Route::OpenTab => {
            let Some(url) = request.body.get("url").and_then(Value::as_str) else {
                return json!({"ok": false, "error": "falta 'url'"});
            };
            if !is_web_url(url) {
                return json!({"ok": false, "error": "falta una 'url' http(s) válida"});
            }
            let mut state = lock(state);
            let id = state.next_id;
            state.next_id += 1;
            let mut tab = Tab {
                id,
                url: url.to_string(),
                title: String::new(),
                history: vec![url.to_string()],
                index: 0,
                page: None,
                error: None,
            };
            let response = load(&mut tab);
            state.tabs.push(tab);
            state.active = Some(id);
            response
        }
        Route::Navigate(tab_id) => {
            let Some(url) = request.body.get("url").and_then(Value::as_str) else {
                return json!({"ok": false, "error": "falta 'url'"});
            };
            if !is_web_url(url) {
                return json!({"ok": false, "error": "falta una 'url' http(s) válida"});
            }
            let mut state = lock(state);
            let Some(position) = state.tabs.iter().position(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            state.active = Some(tab_id);
            let tab = &mut state.tabs[position];
            navigate(tab, url)
        }
        Route::Back(tab_id) => history_step(state, tab_id, -1),
        Route::Forward(tab_id) => history_step(state, tab_id, 1),
        Route::Reload(tab_id) => {
            let mut state = lock(state);
            let Some(position) = state.tabs.iter().position(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            state.active = Some(tab_id);
            load(&mut state.tabs[position])
        }
        Route::Close(tab_id) => {
            let mut state = lock(state);
            state.tabs.retain(|tab| tab.id != tab_id);
            if state.active == Some(tab_id) {
                state.active = state.tabs.last().map(|tab| tab.id);
            }
            json!({"ok": true})
        }
        Route::Activate(tab_id) => {
            let mut state = lock(state);
            if state.tabs.iter().any(|tab| tab.id == tab_id) {
                state.active = Some(tab_id);
            }
            json!({"ok": true})
        }
        Route::Text(tab_id) => {
            let state = lock(state);
            let Some(tab) = state.tabs.iter().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            match &tab.page {
                Some(page) => {
                    json!({"ok": true, "result": page.reader_text().unwrap_or_default()})
                }
                None => json!({
                    "ok": false,
                    "error": tab.error.clone().unwrap_or_else(|| "la página aún no ha cargado".into())
                }),
            }
        }
        Route::Eval(tab_id) => {
            let js = request.body.get("js").and_then(Value::as_str).unwrap_or("");
            if js.is_empty() {
                return json!({"ok": false, "error": "falta 'js'"});
            }
            let state = lock(state);
            let Some(tab) = state.tabs.iter().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            let Some(page) = &tab.page else {
                return json!({
                    "ok": false,
                    "error": tab.error.clone().unwrap_or_else(|| "la página aún no ha cargado".into())
                });
            };
            match page.eval_js(js) {
                Ok(result) => {
                    // El contexto de JS es efímero: `result.html` es la única
                    // forma de observar una mutación del DOM hecha por `eval`.
                    let mut response = json!({"ok": true, "result": result.value});
                    if let Some(html) = result.html {
                        response["html"] = json!(html);
                    }
                    if !result.warnings.is_empty() {
                        response["warnings"] = json!(result.warnings);
                    }
                    response
                }
                Err(error) => json!({"ok": false, "error": error.to_string()}),
            }
        }
        Route::Click(tab_id) => {
            let (Some(x), Some(y)) = (point(request, "x"), point(request, "y")) else {
                return json!({"ok": false, "error": "click necesita 'x' e 'y'"});
            };
            let mut state = lock(state);
            let Some(position) = state.tabs.iter().position(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            state.active = Some(tab_id);
            let tab = &mut state.tabs[position];
            let target = match hit_target(tab, x, y) {
                Ok(target) => target,
                Err(response) => return response,
            };
            let Some(target) = target else {
                return json!({
                    "ok": true,
                    "target": Value::Null,
                    "note": "no hay ningún enlace en esas coordenadas"
                });
            };
            let response = target_json(&target);
            match target.url.clone() {
                // Un clic sobre un enlace navega, igual que en la GUI: el motor
                // headless no tiene contexto de eventos donde despachar el clic.
                Some(url) => {
                    let mut result = navigate(tab, &url);
                    result["target"] = response;
                    result
                }
                None => json!({
                    "ok": true,
                    "target": response,
                    "note": "el enlace no es navegable (no es http/https)"
                }),
            }
        }
        Route::Move(tab_id) => {
            let (Some(x), Some(y)) = (point(request, "x"), point(request, "y")) else {
                return json!({"ok": false, "error": "move necesita 'x' e 'y'"});
            };
            let state = lock(state);
            let Some(tab) = state.tabs.iter().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            match hit_target(tab, x, y) {
                Ok(target) => json!({"ok": true, "target": target.as_ref().map(target_json)}),
                Err(response) => response,
            }
        }
        Route::Type(_) | Route::Press(_) => json!({
            "ok": false,
            "error": "teclear o pulsar teclas necesita foco y un modelo de eventos, que el motor headless aún no tiene"
        }),
        Route::Scroll(_) => json!({
            "ok": false,
            "error": "el motor headless no tiene desplazamiento: maqueta el documento completo de una vez"
        }),
        Route::Cdp(_) => json!({
            "ok": false,
            "error": "CDP nativo necesita un WebView2, que wisp-agent no tiene"
        }),
        Route::Screenshot(tab_id) => {
            let state = lock(state);
            let Some(tab) = state.tabs.iter().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            let Some(page) = &tab.page else {
                return json!({
                    "ok": false,
                    "error": tab.error.clone().unwrap_or_else(|| "la página aún no ha cargado".into())
                });
            };
            let shot = match crate::render::render(page, crate::DEFAULT_VIEWPORT_WIDTH) {
                Ok(shot) => shot,
                Err(error) => return json!({"ok": false, "error": error.to_string()}),
            };
            // La ruta pedida se resuelve como en Ghost: absoluta tal cual, y
            // relativa bajo `%TEMP%/ghost_shots/`.
            let path = match request.body.get("path").and_then(Value::as_str) {
                Some(requested) => {
                    let path = std::path::PathBuf::from(requested);
                    if path.is_absolute() {
                        path
                    } else {
                        std::env::temp_dir()
                            .join("ghost_shots")
                            .join(path.file_name().unwrap_or_default())
                    }
                }
                None => crate::shot_path(&format!("wisp_tab{tab_id}")),
            };
            if let Some(parent) = path.parent()
                && let Err(error) = std::fs::create_dir_all(parent)
            {
                return json!({
                    "ok": false,
                    "error": format!("no se pudo crear {}: {error}", parent.display())
                });
            }
            match std::fs::write(&path, &shot.png) {
                Ok(()) => json!({
                    "ok": true,
                    "path": path.display().to_string(),
                    "width": shot.width,
                    "height": shot.height,
                    "bytes": shot.png.len(),
                }),
                Err(error) => json!({
                    "ok": false,
                    "error": format!("no se pudo escribir {}: {error}", path.display())
                }),
            }
        }
        Route::Unknown => json!({"ok": false, "error": "ruta desconocida"}),
    }
}

/// Resuelve el enlace bajo (`x`, `y`) o devuelve ya el error a enviar al cliente.
fn hit_target(tab: &Tab, x: f64, y: f64) -> Result<Option<wisp_engine::HitTarget>, Value> {
    let Some(page) = &tab.page else {
        return Err(json!({
            "ok": false,
            "error": tab.error.clone().unwrap_or_else(|| "la página aún no ha cargado".into())
        }));
    };
    page.hit_test(x as f32, y as f32)
        .map_err(|error| json!({"ok": false, "error": error.to_string()}))
}

fn target_json(target: &wisp_engine::HitTarget) -> Value {
    json!({
        "href": target.href,
        "url": target.url,
        "text": target.text,
    })
}

fn point(request: &control::Request, key: &str) -> Option<f64> {
    request.body.get(key).and_then(Value::as_f64)
}

fn history_step(state: &Arc<Mutex<State>>, tab_id: u64, step: i64) -> Value {
    let mut state = lock(state);
    let Some(position) = state.tabs.iter().position(|tab| tab.id == tab_id) else {
        return json!({"ok": false, "error": "pestaña no encontrada"});
    };
    let next = state.tabs[position].index as i64 + step;
    if next < 0 || next as usize >= state.tabs[position].history.len() {
        return json!({"ok": true, "url": state.tabs[position].url.clone()});
    }
    state.active = Some(tab_id);
    let tab = &mut state.tabs[position];
    tab.index = next as usize;
    tab.url = tab.history[tab.index].clone();
    load(tab)
}

/// Navega la pestaña a `url` como una entrada nueva de historial (corta el
/// futuro y descarta lo que había delante, como haría el navegador).
fn navigate(tab: &mut Tab, url: &str) -> Value {
    tab.history.truncate(tab.index + 1);
    tab.history.push(url.to_owned());
    tab.index = tab.history.len() - 1;
    tab.url = url.to_owned();
    load(tab)
}

/// Descarga la URL de la pestaña y actualiza página, título y error.
fn load(tab: &mut Tab) -> Value {
    match fetch_url(&tab.url) {
        Ok(page) => {
            if !page.title.trim().is_empty() {
                tab.title = page.title.clone();
            }
            tab.url = page.final_url.clone();
            if let Some(slot) = tab.history.get_mut(tab.index) {
                *slot = tab.url.clone();
            }
            tab.error = None;
            let response = json!({"ok": true, "id": tab.id, "url": tab.url, "title": tab.title});
            tab.page = Some(page);
            response
        }
        Err(error) => {
            let message = error.to_string();
            tab.error = Some(message.clone());
            tab.page = None;
            json!({"ok": false, "error": message})
        }
    }
}

fn tab_json(tab: &Tab, active: Option<u64>) -> Value {
    json!({
        "id": tab.id,
        "title": tab.title,
        "url": tab.url,
        "engine": "Wisp",
        "active": active == Some(tab.id),
        "loading": false,
        "suspended": false,
    })
}

fn is_web_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn lock(state: &Arc<Mutex<State>>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|error| error.into_inner())
}
