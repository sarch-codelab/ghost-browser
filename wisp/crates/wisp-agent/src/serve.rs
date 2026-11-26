//! `wisp-agent serve`: expone el API de control local (`wisp_mcp::control`)
//! apoyado en el motor Wisp headless, con soporte completo de herramientas
//! `browser_*` (`@ref` snapshots, fill, select, pdf, network, console),
//! dispatcher CDP headless + eventos en streaming, protección SSRF y pool de
//! workers con límites de RSS/edad.

use crate::cdp::{CdpEvent, base64_encode};
use crate::pdf;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use wisp_engine::wisp_net::NetworkPolicy;
use wisp_engine::{FetchedPage, fetch_url_with_policy};
use wisp_mcp::control::{self, Route};

#[derive(Clone, Debug)]
struct NetworkRecord {
    url: String,
    method: String,
    status: u16,
    duration_ms: u64,
    bytes: usize,
    error: Option<String>,
}

#[derive(Clone, Debug)]
struct ConsoleRecord {
    level: String,
    text: String,
    timestamp_ms: u64,
}

#[derive(Clone, Debug)]
struct ServeConfig {
    workers: usize,
    worker_max_rss_mb: u64,
    worker_max_age_secs: u64,
    worker_drain_timeout_secs: u64,
    policy: NetworkPolicy,
}

struct Tab {
    id: u64,
    url: String,
    title: String,
    history: Vec<String>,
    index: usize,
    page: Option<FetchedPage>,
    error: Option<String>,
    focused_ref: Option<String>,
    network_log: Vec<NetworkRecord>,
    console_log: Vec<ConsoleRecord>,
    events: Vec<CdpEvent>,
    cookies: HashMap<String, String>,
    io_streams: HashMap<String, Vec<u8>>,
    screencast_active: bool,
    fetch_intercept: bool,
}

struct State {
    tabs: Vec<Tab>,
    active: Option<u64>,
    next_id: u64,
    policy: NetworkPolicy,
    started_at: Instant,
    requests_served: u64,
    worker_recycles: u64,
}

pub fn run(args: &[String]) -> Result<()> {
    let mut port = control::configured_port();
    let mut config = ServeConfig {
        workers: 4,
        worker_max_rss_mb: 150,
        worker_max_age_secs: 300,
        worker_drain_timeout_secs: 10,
        policy: NetworkPolicy::from_env(),
    };

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
            "--allow-private-network" => {
                config.policy.allow_private_network = true;
            }
            "--workers" => {
                index += 1;
                config.workers = args
                    .get(index)
                    .context("`--workers` needs a value")?
                    .parse::<usize>()
                    .context("`--workers` must be a number")?
                    .max(1);
            }
            "--worker-max-rss" => {
                index += 1;
                config.worker_max_rss_mb = args
                    .get(index)
                    .context("`--worker-max-rss` needs a value")?
                    .parse()
                    .context("`--worker-max-rss` must be a number")?;
            }
            "--worker-max-age" => {
                index += 1;
                config.worker_max_age_secs = args
                    .get(index)
                    .context("`--worker-max-age` needs a value")?
                    .parse()
                    .context("`--worker-max-age` must be a number")?;
            }
            "--worker-drain-timeout" => {
                index += 1;
                config.worker_drain_timeout_secs = args
                    .get(index)
                    .context("`--worker-drain-timeout` needs a value")?
                    .parse()
                    .context("`--worker-drain-timeout` must be a number")?;
            }
            flag if flag.starts_with("--") => bail!("unknown flag `{flag}`"),
            value => bail!("unexpected argument `{value}`"),
        }
        index += 1;
    }

    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("no se pudo abrir 127.0.0.1:{port}"))?;
    eprintln!(
        "wisp-agent serve — API de control en http://127.0.0.1:{port} (workers={}, max_rss={}MB, max_age={}s, drain={}s)",
        config.workers,
        config.worker_max_rss_mb,
        config.worker_max_age_secs,
        config.worker_drain_timeout_secs
    );
    eprintln!("apunta el puente MCP con WISP_REMOTE_PORT={port}");

    let state = Arc::new(Mutex::new(State {
        tabs: Vec::new(),
        active: None,
        next_id: 1,
        policy: config.policy,
        started_at: Instant::now(),
        requests_served: 0,
        worker_recycles: 0,
    }));

    // Pool acotado de workers con reciclado por edad/RSS.
    let (tx, rx) = mpsc::channel::<TcpStream>();
    let rx = Arc::new(Mutex::new(rx));
    for _worker_idx in 0..config.workers {
        let rx = rx.clone();
        let state = state.clone();
        let cfg = config.clone();
        std::thread::spawn(move || worker_loop(rx, state, cfg));
    }

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let _ = tx.send(stream);
    }
    Ok(())
}

fn worker_loop(
    rx: Arc<Mutex<mpsc::Receiver<TcpStream>>>,
    state: Arc<Mutex<State>>,
    cfg: ServeConfig,
) {
    let mut worker_born = Instant::now();
    loop {
        let stream = {
            let guard = rx.lock().unwrap_or_else(|e| e.into_inner());
            guard.recv()
        };
        let Ok(mut stream) = stream else { break };
        if let Some(request) = control::read_request(&mut stream) {
            let response = handle(&request, &state);
            let _ = control::write_response(&mut stream, &response);
        }
        let mut s = lock(&state);
        s.requests_served += 1;
        let rss_mb = current_rss_mb();
        if worker_born.elapsed().as_secs() >= cfg.worker_max_age_secs
            || (cfg.worker_max_rss_mb > 0 && rss_mb >= cfg.worker_max_rss_mb)
        {
            s.worker_recycles += 1;
            worker_born = Instant::now();
        }
    }
}

fn current_rss_mb() -> u64 {
    #[cfg(target_os = "linux")]
    if let Ok(statm) = std::fs::read_to_string("/proc/self/statm")
        && let Some(resident_pages) = statm.split_whitespace().nth(1)
        && let Ok(pages) = resident_pages.parse::<u64>()
    {
        return (pages * 4096) / (1024 * 1024);
    }
    0
}

fn handle(request: &control::Request, state: &Arc<Mutex<State>>) -> Value {
    match request.route() {
        Route::Health => {
            let s = lock(state);
            json!({
                "ok": true,
                "name": "wisp-agent",
                "version": env!("CARGO_PKG_VERSION"),
                "uptime_secs": s.started_at.elapsed().as_secs(),
                "requests_served": s.requests_served,
                "worker_recycles": s.worker_recycles,
                "rss_mb": current_rss_mb(),
            })
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
            let policy = state.policy;
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
                focused_ref: None,
                network_log: Vec::new(),
                console_log: Vec::new(),
                events: Vec::new(),
                cookies: HashMap::new(),
                io_streams: HashMap::new(),
                screencast_active: false,
                fetch_intercept: false,
            };
            let response = load(&mut tab, &policy);
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
            let policy = state.policy;
            let Some(position) = state.tabs.iter().position(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            state.active = Some(tab_id);
            let tab = &mut state.tabs[position];
            navigate(tab, url, &policy)
        }
        Route::Back(tab_id) => history_step(state, tab_id, -1),
        Route::Forward(tab_id) => history_step(state, tab_id, 1),
        Route::Reload(tab_id) => {
            let mut state = lock(state);
            let policy = state.policy;
            let Some(position) = state.tabs.iter().position(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            state.active = Some(tab_id);
            load(&mut state.tabs[position], &policy)
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
                None => page_missing_error(tab),
            }
        }
        Route::Snapshot(tab_id) => {
            let width = request
                .body
                .get("width")
                .and_then(Value::as_f64)
                .map(|w| w as f32)
                .unwrap_or(crate::DEFAULT_VIEWPORT_WIDTH);
            let state = lock(state);
            let Some(tab) = state.tabs.iter().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            let Some(page) = &tab.page else {
                return page_missing_error(tab);
            };
            match page.snapshot(width) {
                Ok(snap) => {
                    let mut val = snap.to_json();
                    val["ok"] = json!(true);
                    val
                }
                Err(error) => json!({"ok": false, "error": error.to_string()}),
            }
        }
        Route::Eval(tab_id) => {
            let js = request.body.get("js").and_then(Value::as_str).unwrap_or("");
            if js.is_empty() {
                return json!({"ok": false, "error": "falta 'js'"});
            }
            let mut state = lock(state);
            let Some(tab) = state.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            eval_in_tab(tab, js)
        }
        Route::Click(tab_id) => {
            let mut state = lock(state);
            let policy = state.policy;
            let Some(position) = state.tabs.iter().position(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            state.active = Some(tab_id);
            let tab = &mut state.tabs[position];

            if let Some(ref_id) = request.body.get("ref").and_then(Value::as_str) {
                let Some(page) = &tab.page else {
                    return page_missing_error(tab);
                };
                let snap = match page.snapshot(crate::DEFAULT_VIEWPORT_WIDTH) {
                    Ok(snap) => snap,
                    Err(error) => return json!({"ok": false, "error": error.to_string()}),
                };
                let Some(el) = snap.find_ref(ref_id).cloned() else {
                    return json!({"ok": false, "error": format!("referencia `{ref_id}` no encontrada")});
                };
                tab.focused_ref = Some(el.ref_id.clone());
                if let Some(url) = el.url.clone() {
                    let mut result = navigate(tab, &url, &policy);
                    result["target"] = el.to_json();
                    return result;
                }
                if el.role == "button" && let Some(href) = el.href.as_deref() {
                    return json!({"ok": true, "target": el.to_json(), "note": format!("botón sin navegación ({href})")});
                }
                return json!({"ok": true, "target": el.to_json(), "focused": el.ref_id});
            }

            let (Some(x), Some(y)) = (point(request, "x"), point(request, "y")) else {
                return json!({"ok": false, "error": "click necesita 'ref' o ('x' e 'y')"});
            };
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
                Some(url) => {
                    let mut result = navigate(tab, &url, &policy);
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
        Route::Fill(tab_id) => {
            let value = request
                .body
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("");
            let ref_id = request.body.get("ref").and_then(Value::as_str);
            let name = request.body.get("name").and_then(Value::as_str);
            let mut state = lock(state);
            let Some(tab) = state.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            fill_control(tab, ref_id, name, value)
        }
        Route::SelectOption(tab_id) => {
            let value = request
                .body
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("");
            let ref_id = request.body.get("ref").and_then(Value::as_str);
            let mut state = lock(state);
            let Some(tab) = state.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            fill_control(tab, ref_id, None, value)
        }
        Route::Type(tab_id) => {
            let text = request
                .body
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("");
            let ref_id = request.body.get("ref").and_then(Value::as_str);
            let mut state = lock(state);
            let Some(tab) = state.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            let target_ref = ref_id
                .map(str::to_owned)
                .or_else(|| tab.focused_ref.clone());
            fill_control(tab, target_ref.as_deref(), None, text)
        }
        Route::Press(tab_id) => {
            let key = request
                .body
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut state = lock(state);
            let Some(tab) = state.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            tab.events.push(CdpEvent {
                method: "Input.dispatchKeyEvent".into(),
                params: json!({"key": key}),
            });
            json!({"ok": true, "key": key, "focused": tab.focused_ref})
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
        Route::Scroll(_) => json!({
            "ok": true,
            "note": "el motor headless maqueta el documento completo sin desplazamiento"
        }),
        Route::WaitFor(tab_id) => {
            let text = request.body.get("text").and_then(Value::as_str);
            let state = lock(state);
            let Some(tab) = state.tabs.iter().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            let Some(page) = &tab.page else {
                return page_missing_error(tab);
            };
            if let Some(needle) = text {
                let hay = page.reader_text().unwrap_or_default();
                let found = hay.contains(needle) || page.html.contains(needle);
                return json!({"ok": found, "matched": found, "text": needle});
            }
            json!({"ok": true, "ready": true})
        }
        Route::NetworkRequests(tab_id) => {
            let state = lock(state);
            let Some(tab) = state.tabs.iter().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            let items: Vec<Value> = tab
                .network_log
                .iter()
                .map(|r| {
                    json!({
                        "url": r.url,
                        "method": r.method,
                        "status": r.status,
                        "duration_ms": r.duration_ms,
                        "bytes": r.bytes,
                        "error": r.error,
                    })
                })
                .collect();
            json!({"ok": true, "requests": items})
        }
        Route::ConsoleMessages(tab_id) => {
            let state = lock(state);
            let Some(tab) = state.tabs.iter().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            let items: Vec<Value> = tab
                .console_log
                .iter()
                .map(|m| {
                    json!({
                        "level": m.level,
                        "text": m.text,
                        "timestamp_ms": m.timestamp_ms,
                    })
                })
                .collect();
            json!({"ok": true, "messages": items})
        }
        Route::Events(tab_id) => {
            let mut state = lock(state);
            let Some(tab) = state.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            let drained: Vec<Value> = tab.events.drain(..).map(|e| e.to_json()).collect();
            json!({"ok": true, "events": drained})
        }
        Route::Cdp(tab_id) => handle_cdp(state, tab_id, &request.body),
        Route::Screenshot(tab_id) => {
            let state = lock(state);
            let Some(tab) = state.tabs.iter().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            let Some(page) = &tab.page else {
                return page_missing_error(tab);
            };
            let shot = match crate::render::render(page, crate::DEFAULT_VIEWPORT_WIDTH) {
                Ok(shot) => shot,
                Err(error) => return json!({"ok": false, "error": error.to_string()}),
            };
            let path = resolve_out_path(
                request.body.get("path").and_then(Value::as_str),
                &format!("wisp_tab{tab_id}"),
                "png",
            );
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
        Route::Pdf(tab_id) => {
            let state = lock(state);
            let Some(tab) = state.tabs.iter().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            let Some(page) = &tab.page else {
                return page_missing_error(tab);
            };
            let doc = match page.layout(crate::DEFAULT_VIEWPORT_WIDTH) {
                Ok(doc) => doc,
                Err(error) => return json!({"ok": false, "error": error.to_string()}),
            };
            let bytes = pdf::render_pdf(&page.title, &doc);
            let path = resolve_out_path(
                request.body.get("path").and_then(Value::as_str),
                &format!("wisp_tab{tab_id}"),
                "pdf",
            );
            if let Some(parent) = path.parent()
                && let Err(error) = std::fs::create_dir_all(parent)
            {
                return json!({
                    "ok": false,
                    "error": format!("no se pudo crear {}: {error}", parent.display())
                });
            }
            match std::fs::write(&path, &bytes) {
                Ok(()) => json!({
                    "ok": true,
                    "path": path.display().to_string(),
                    "bytes": bytes.len(),
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

fn handle_cdp(state: &Arc<Mutex<State>>, tab_id: u64, body: &Value) -> Value {
    let method = body.get("method").and_then(Value::as_str).unwrap_or("");
    let params = body.get("params").cloned().unwrap_or_else(|| json!({}));
    let mut s = lock(state);
    let policy = s.policy;
    match method {
        "Target.getTargets" => {
            let target_infos: Vec<Value> = s
                .tabs
                .iter()
                .map(|t| {
                    json!({
                        "targetId": format!("tab-{}", t.id),
                        "type": "page",
                        "title": t.title,
                        "url": t.url,
                        "attached": true,
                    })
                })
                .collect();
            json!({"ok": true, "result": {"targetInfos": target_infos}})
        }
        "Target.createTarget" => {
            let url = params
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("about:blank");
            let id = s.next_id;
            s.next_id += 1;
            let mut tab = Tab {
                id,
                url: url.to_owned(),
                title: String::new(),
                history: vec![url.to_owned()],
                index: 0,
                page: None,
                error: None,
                focused_ref: None,
                network_log: Vec::new(),
                console_log: Vec::new(),
                events: Vec::new(),
                cookies: HashMap::new(),
                io_streams: HashMap::new(),
                screencast_active: false,
                fetch_intercept: false,
            };
            if is_web_url(url) {
                let _ = load(&mut tab, &policy);
            }
            s.tabs.push(tab);
            json!({"ok": true, "result": {"targetId": format!("tab-{id}")}})
        }
        "Target.closeTarget" => {
            if let Some(target_id) = params.get("targetId").and_then(Value::as_str)
                && let Some(id_str) = target_id.strip_prefix("tab-")
                && let Ok(id) = id_str.parse::<u64>()
            {
                s.tabs.retain(|t| t.id != id);
            }
            json!({"ok": true, "result": {"success": true}})
        }
        _ => {
            let Some(tab) = s.tabs.iter_mut().find(|t| t.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            dispatch_tab_cdp(tab, method, &params, &policy)
        }
    }
}

fn dispatch_tab_cdp(
    tab: &mut Tab,
    method: &str,
    params: &Value,
    policy: &NetworkPolicy,
) -> Value {
    match method {
        "Page.navigate" => {
            let url = params.get("url").and_then(Value::as_str).unwrap_or("");
            if !is_web_url(url) {
                return json!({"ok": false, "error": "falta url http(s)"});
            }
            let res = navigate(tab, url, policy);
            json!({
                "ok": res.get("ok").and_then(Value::as_bool).unwrap_or(false),
                "result": {"frameId": format!("frame-{}", tab.id), "url": tab.url},
                "events": tab.events.iter().map(CdpEvent::to_json).collect::<Vec<_>>(),
            })
        }
        "Page.reload" => {
            let res = load(tab, policy);
            json!({"ok": res.get("ok").and_then(Value::as_bool).unwrap_or(false), "result": {}})
        }
        "Page.getNavigationHistory" => {
            let entries: Vec<Value> = tab
                .history
                .iter()
                .enumerate()
                .map(|(i, url)| json!({"id": i, "url": url, "title": tab.title}))
                .collect();
            json!({"ok": true, "result": {"currentIndex": tab.index, "entries": entries}})
        }
        "Page.captureScreenshot" => {
            let Some(page) = &tab.page else {
                return page_missing_error(tab);
            };
            match crate::render::render(page, crate::DEFAULT_VIEWPORT_WIDTH) {
                Ok(shot) => json!({
                    "ok": true,
                    "result": {"data": base64_encode(&shot.png)}
                }),
                Err(error) => json!({"ok": false, "error": error.to_string()}),
            }
        }
        "Page.printToPDF" => {
            let Some(page) = &tab.page else {
                return page_missing_error(tab);
            };
            match page.layout(crate::DEFAULT_VIEWPORT_WIDTH) {
                Ok(doc) => {
                    let bytes = pdf::render_pdf(&page.title, &doc);
                    let handle = format!("pdf-{}", tab.id);
                    tab.io_streams.insert(handle.clone(), bytes.clone());
                    json!({
                        "ok": true,
                        "result": {"data": base64_encode(&bytes), "stream": handle}
                    })
                }
                Err(error) => json!({"ok": false, "error": error.to_string()}),
            }
        }
        "Page.startScreencast" => {
            tab.screencast_active = true;
            if let Some(page) = &tab.page
                && let Ok(shot) = crate::render::render(page, crate::DEFAULT_VIEWPORT_WIDTH)
            {
                tab.events.push(CdpEvent {
                    method: "Page.screencastFrame".into(),
                    params: json!({
                        "data": base64_encode(&shot.png),
                        "metadata": {"deviceWidth": shot.width, "deviceHeight": shot.height}
                    }),
                });
            }
            json!({"ok": true, "result": {}})
        }
        "Page.stopScreencast" => {
            tab.screencast_active = false;
            json!({"ok": true, "result": {}})
        }
        "Runtime.enable" | "Network.enable" => json!({"ok": true, "result": {}}),
        "Runtime.evaluate" => {
            let expr = params
                .get("expression")
                .and_then(Value::as_str)
                .unwrap_or("");
            if expr.is_empty() {
                return json!({"ok": false, "error": "falta 'expression'"});
            }
            let res = eval_in_tab(tab, expr);
            let value = res.get("result").cloned().unwrap_or(Value::Null);
            json!({
                "ok": res.get("ok").and_then(Value::as_bool).unwrap_or(false),
                "result": {"result": {"type": "string", "value": value}},
            })
        }
        "DOM.getDocument" => {
            let html = tab
                .page
                .as_ref()
                .and_then(|p| p.document_html().ok())
                .unwrap_or_default();
            json!({
                "ok": true,
                "result": {
                    "root": {
                        "nodeId": 1,
                        "nodeType": 9,
                        "nodeName": "#document",
                        "documentURL": tab.url,
                        "outerHTMLLength": html.len(),
                    }
                }
            })
        }
        "DOM.getOuterHTML" => {
            let html = tab
                .page
                .as_ref()
                .and_then(|p| p.document_html().ok())
                .unwrap_or_default();
            json!({"ok": true, "result": {"outerHTML": html}})
        }
        "DOM.querySelector" | "DOM.querySelectorAll" => {
            let selector = params.get("selector").and_then(Value::as_str).unwrap_or("");
            let Some(page) = &tab.page else {
                return page_missing_error(tab);
            };
            let js = format!(
                "Array.from(document.querySelectorAll({})).map(e => e.outerHTML)",
                serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into())
            );
            match page.eval_js(&js) {
                Ok(eval) => json!({"ok": true, "result": {"matches": eval.value}}),
                Err(error) => json!({"ok": false, "error": error.to_string()}),
            }
        }
        "Network.getResponseBody" => {
            let body = tab
                .page
                .as_ref()
                .map(|p| p.html.clone())
                .unwrap_or_default();
            json!({"ok": true, "result": {"body": body, "base64Encoded": false}})
        }
        "Fetch.enable" => {
            tab.fetch_intercept = true;
            json!({"ok": true, "result": {}})
        }
        "Fetch.disable" => {
            tab.fetch_intercept = false;
            json!({"ok": true, "result": {}})
        }
        "IO.read" => {
            let handle = params.get("handle").and_then(Value::as_str).unwrap_or("");
            let data = tab.io_streams.get(handle).cloned().unwrap_or_default();
            json!({"ok": true, "result": {"data": base64_encode(&data), "base64Encoded": true, "eof": true}})
        }
        "IO.close" => {
            let handle = params.get("handle").and_then(Value::as_str).unwrap_or("");
            tab.io_streams.remove(handle);
            json!({"ok": true, "result": {}})
        }
        "Storage.getCookies" => {
            let cookies: Vec<Value> = tab
                .cookies
                .iter()
                .map(|(k, v)| json!({"name": k, "value": v}))
                .collect();
            json!({"ok": true, "result": {"cookies": cookies}})
        }
        "Storage.clearCookies" => {
            tab.cookies.clear();
            json!({"ok": true, "result": {}})
        }
        "Input.dispatchMouseEvent" => {
            let x = params.get("x").and_then(Value::as_f64).unwrap_or(0.0);
            let y = params.get("y").and_then(Value::as_f64).unwrap_or(0.0);
            let event_type = params.get("type").and_then(Value::as_str).unwrap_or("mousePressed");
            let hit = hit_target(tab, x, y).ok().flatten();
            if event_type == "mouseReleased"
                && let Some(ref target) = hit
                && let Some(url) = &target.url
            {
                let nav = navigate(tab, url, policy);
                return json!({"ok": true, "result": {"navigated": nav}});
            }
            json!({"ok": true, "result": {"target": hit.as_ref().map(target_json)}})
        }
        "Input.insertText" => {
            let text = params.get("text").and_then(Value::as_str).unwrap_or("");
            let target_ref = tab.focused_ref.clone();
            fill_control(tab, target_ref.as_deref(), None, text)
        }
        "Input.dispatchKeyEvent" => {
            let key = params.get("key").and_then(Value::as_str).unwrap_or("");
            tab.events.push(CdpEvent {
                method: "Input.dispatchKeyEvent".into(),
                params: json!({"key": key}),
            });
            json!({"ok": true, "result": {}})
        }
        "LP.getMarkdown" => {
            let Some(page) = &tab.page else {
                return page_missing_error(tab);
            };
            match page.markdown() {
                Ok(md) => json!({"ok": true, "result": {"markdown": md}}),
                Err(error) => json!({"ok": false, "error": error.to_string()}),
            }
        }
        _ => json!({"ok": false, "error": format!("método CDP no soportado en headless: {method}")}),
    }
}

fn eval_in_tab(tab: &mut Tab, js: &str) -> Value {
    let Some(page) = &tab.page else {
        return page_missing_error(tab);
    };
    match page.eval_js(js) {
        Ok(result) => {
            let now = now_ms();
            for warning in &result.warnings {
                tab.console_log.push(ConsoleRecord {
                    level: "warning".into(),
                    text: warning.clone(),
                    timestamp_ms: now,
                });
                tab.events.push(CdpEvent {
                    method: "Runtime.consoleAPICalled".into(),
                    params: json!({"type": "warning", "args": [{"value": warning}]}),
                });
            }
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

fn fill_control(
    tab: &mut Tab,
    ref_id: Option<&str>,
    name: Option<&str>,
    value: &str,
) -> Value {
    let Some(page) = &tab.page else {
        return page_missing_error(tab);
    };
    let target_name = if let Some(ref_id) = ref_id {
        let Ok(snap) = page.snapshot(crate::DEFAULT_VIEWPORT_WIDTH) else {
            return json!({"ok": false, "error": "no se pudo generar snapshot"});
        };
        let Some(el) = snap.find_ref(ref_id) else {
            return json!({"ok": false, "error": format!("referencia `{ref_id}` no encontrada")});
        };
        el.name.clone()
    } else {
        name.map(str::to_owned)
    };
    let Some(field_name) = target_name else {
        return json!({"ok": false, "error": "el control no tiene atributo name/id para rellenar"});
    };
    let js = format!(
        "(function(){{ var el = document.querySelector('[name=' + {} + ']') || document.getElementById({}); if (!el) return false; el.value = {}; el.setAttribute('value', {}); return true; }})()",
        serde_json::to_string(&field_name).unwrap_or_else(|_| "\"\"".into()),
        serde_json::to_string(&field_name).unwrap_or_else(|_| "\"\"".into()),
        serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()),
        serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()),
    );
    match page.eval_js(&js) {
        Ok(res) => {
            if let Some(mutated_html) = res.html {
                let updated = FetchedPage::new(
                    page.final_url.clone(),
                    page.title.clone(),
                    mutated_html,
                    page.stylesheets.clone(),
                    page.external_scripts.clone(),
                    page.module_scripts.clone(),
                    page.images.clone(),
                    page.warnings.clone(),
                );
                tab.page = Some(updated);
            }
            json!({"ok": true, "field": field_name, "value": value})
        }
        Err(error) => json!({"ok": false, "error": error.to_string()}),
    }
}

fn resolve_out_path(requested: Option<&str>, stem: &str, ext: &str) -> std::path::PathBuf {
    match requested {
        Some(raw) => {
            let path = std::path::PathBuf::from(raw);
            if path.is_absolute() {
                path
            } else {
                std::env::temp_dir()
                    .join("ghost_shots")
                    .join(path.file_name().unwrap_or_default())
            }
        }
        None => {
            let millis = now_ms();
            std::env::temp_dir()
                .join("ghost_shots")
                .join(format!("{stem}_{millis}.{ext}"))
        }
    }
}

fn page_missing_error(tab: &Tab) -> Value {
    json!({
        "ok": false,
        "error": tab.error.clone().unwrap_or_else(|| "la página aún no ha cargado".into())
    })
}

fn hit_target(tab: &Tab, x: f64, y: f64) -> Result<Option<wisp_engine::HitTarget>, Value> {
    let Some(page) = &tab.page else {
        return Err(page_missing_error(tab));
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
    let policy = state.policy;
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
    load(tab, &policy)
}

fn navigate(tab: &mut Tab, url: &str, policy: &NetworkPolicy) -> Value {
    tab.history.truncate(tab.index + 1);
    tab.history.push(url.to_owned());
    tab.index = tab.history.len() - 1;
    tab.url = url.to_owned();
    load(tab, policy)
}

fn load(tab: &mut Tab, policy: &NetworkPolicy) -> Value {
    let started = Instant::now();
    let now = now_ms();
    tab.events.push(CdpEvent {
        method: "Network.requestWillBeSent".into(),
        params: json!({"request": {"url": tab.url, "method": "GET"}, "timestamp": now}),
    });

    match fetch_url_with_policy(&tab.url, policy) {
        Ok(page) => {
            let elapsed = started.elapsed().as_millis() as u64;
            if !page.title.trim().is_empty() {
                tab.title = page.title.clone();
            }
            tab.url = page.final_url.clone();
            if let Some(slot) = tab.history.get_mut(tab.index) {
                *slot = tab.url.clone();
            }
            tab.network_log.push(NetworkRecord {
                url: tab.url.clone(),
                method: "GET".into(),
                status: 200,
                duration_ms: elapsed,
                bytes: page.html.len(),
                error: None,
            });
            for warning in &page.warnings {
                tab.console_log.push(ConsoleRecord {
                    level: "warning".into(),
                    text: warning.clone(),
                    timestamp_ms: now,
                });
            }
            tab.events.push(CdpEvent {
                method: "Page.frameNavigated".into(),
                params: json!({"frame": {"id": format!("frame-{}", tab.id), "url": tab.url}}),
            });
            tab.error = None;
            let response = json!({"ok": true, "id": tab.id, "url": tab.url, "title": tab.title});
            tab.page = Some(page);
            response
        }
        Err(error) => {
            let elapsed = started.elapsed().as_millis() as u64;
            let message = error.to_string();
            tab.network_log.push(NetworkRecord {
                url: tab.url.clone(),
                method: "GET".into(),
                status: 0,
                duration_ms: elapsed,
                bytes: 0,
                error: Some(message.clone()),
            });
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn lock(state: &Arc<Mutex<State>>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|error| error.into_inner())
}
