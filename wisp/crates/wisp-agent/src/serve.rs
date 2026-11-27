//! `wisp-agent serve`: expone el API de control local (`wisp_mcp::control`)
//! apoyado en el motor Wisp headless, con soporte completo de herramientas
//! `browser_*` (`@ref` snapshots, fill, select, pdf, network, console),
//! dispatcher CDP headless + eventos en streaming, protección SSRF y pool de
//! workers con límites de RSS/edad.

use crate::cdp::{CdpEvent, base64_encode};
use crate::pdf;
use crate::ws::{self, WsMessage};
use anyhow::{Context, Result, bail};
use scraper::{ElementRef, Html, Selector};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::Read;
#[cfg(test)]
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use wisp_engine::{FetchOptions, FetchedPage, fetch_url_with_options};
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
    options: FetchOptions,
}

/// Viewport emulado por `Emulation.setDeviceMetricsOverride`.
#[derive(Clone, Debug)]
struct Viewport {
    width: u32,
    height: u32,
    screen_width: u32,
    screen_height: u32,
    device_scale_factor: f64,
    mobile: bool,
}

impl Default for Viewport {
    fn default() -> Self {
        let width = crate::DEFAULT_VIEWPORT_WIDTH.round() as u32;
        Self {
            width,
            height: 800,
            screen_width: width,
            screen_height: 800,
            device_scale_factor: 1.0,
            mobile: false,
        }
    }
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
    /// Viewport que pidió el cliente por CDP; manda en layout y capturas.
    viewport: Viewport,
    network_enabled: bool,
    log_enabled: bool,
    /// Identificador del contexto de ejecución del documento actual. Chrome
    /// crea uno nuevo por navegación y Playwright lo necesita para evaluar.
    execution_context: u64,
    /// Mundo aislado creado por el cliente (`Page.createIsolatedWorld`).
    world_name: Option<String>,
    /// Handles sintéticos para `Runtime.callFunctionOn` (el DOM es efímero,
    /// así que un `objectId` guarda el valor serializado, no una referencia).
    object_handles: HashMap<String, Value>,
    next_handle: u64,
    /// Contexto de navegador CDP al que pertenece la pestaña.
    context_id: String,
}

impl Tab {
    fn new(id: u64, url: &str) -> Self {
        Self {
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
            viewport: Viewport::default(),
            network_enabled: false,
            log_enabled: false,
            execution_context: 1,
            world_name: None,
            object_handles: HashMap::new(),
            next_handle: 1,
            context_id: "default".to_owned(),
        }
    }

    fn viewport_width(&self) -> f32 {
        self.viewport.width as f32
    }
}

struct State {
    tabs: Vec<Tab>,
    active: Option<u64>,
    next_id: u64,
    port: u16,
    options: FetchOptions,
    started_at: Instant,
    requests_served: u64,
    worker_recycles: u64,
    /// Eventos de nivel navegador (`Target.attachedToTarget`), que no
    /// pertenecen a ninguna pestaña concreta.
    events: Vec<CdpEvent>,
    /// Contextos CDP creados por el cliente con `Target.createBrowserContext`.
    browser_contexts: Vec<String>,
    /// Targets ya anunciados con `Target.attachedToTarget`. Playwright reenvía
    /// `Target.setAutoAttach` y falla con «Duplicate target» si se repiten.
    announced_targets: HashSet<u64>,
}

pub fn run(args: &[String]) -> Result<()> {
    let mut port = control::configured_port();
    let mut config = ServeConfig {
        workers: 4,
        worker_max_rss_mb: 150,
        worker_max_age_secs: 300,
        worker_drain_timeout_secs: 10,
        options: FetchOptions::from_env(),
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
                config.options.policy.allow_private_network = true;
            }
            "--stealth" => {
                config.options.policy.stealth = true;
            }
            "--block-trackers" => {
                config.options.policy.block_trackers = true;
            }
            "--proxy" => {
                index += 1;
                config.options.proxy_url =
                    Some(args.get(index).context("`--proxy` needs a URL")?.to_owned());
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
    let actual_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    eprintln!(
        "wisp-agent serve — API de control en http://127.0.0.1:{actual_port} (workers={}, max_rss={}MB, max_age={}s, drain={}s)",
        config.workers,
        config.worker_max_rss_mb,
        config.worker_max_age_secs,
        config.worker_drain_timeout_secs
    );
    eprintln!("CDP WebSocket en ws://127.0.0.1:{actual_port}/devtools/browser/wisp");
    eprintln!("apunta el puente MCP con WISP_REMOTE_PORT={actual_port}");

    let state = Arc::new(Mutex::new(State {
        tabs: Vec::new(),
        active: None,
        next_id: 1,
        port: actual_port,
        options: config.options.clone(),
        started_at: Instant::now(),
        requests_served: 0,
        worker_recycles: 0,
        events: Vec::new(),
        browser_contexts: Vec::new(),
        announced_targets: HashSet::new(),
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

enum IncomingHttp {
    WebSocket { path: String, key: String },
    Rest(control::Request),
    InvalidJson(String),
    Eof,
}

fn read_incoming_http(stream: &mut TcpStream) -> IncomingHttp {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 2048];
    let head_end;
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            head_end = pos + 4;
            break;
        }
        if buf.len() >= 65_536 {
            return IncomingHttp::InvalidJson("HTTP header too large".to_owned());
        }
        match stream.read(&mut tmp) {
            Ok(0) => return IncomingHttp::Eof,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => return IncomingHttp::Eof,
        }
    }

    let head = String::from_utf8_lossy(&buf[..head_end - 4]);
    let mut lines = head.split("\r\n");
    let Some(req_line) = lines.next() else {
        return IncomingHttp::Eof;
    };
    let mut parts = req_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_ascii_uppercase();
    let path = parts.next().unwrap_or("/").to_owned();

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }

    if headers
        .get("upgrade")
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        && let Some(key) = headers.get("sec-websocket-key").cloned()
    {
        return IncomingHttp::WebSocket { path, key };
    }

    let content_len = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
        .min(16 * 1024 * 1024);

    let mut body_bytes = buf[head_end..].to_vec();
    while body_bytes.len() < content_len {
        let need = (content_len - body_bytes.len()).min(tmp.len());
        match stream.read(&mut tmp[..need]) {
            Ok(0) | Err(_) => break,
            Ok(n) => body_bytes.extend_from_slice(&tmp[..n]),
        }
    }
    body_bytes.truncate(content_len);

    let body = if body_bytes.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice::<Value>(&body_bytes) {
            Ok(val) => val,
            Err(err) => return IncomingHttp::InvalidJson(format!("JSON inválido: {err}")),
        }
    };

    IncomingHttp::Rest(control::Request { method, path, body })
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
        match read_incoming_http(&mut stream) {
            IncomingHttp::WebSocket { path, key } => {
                if ws::write_handshake(&mut stream, &key).is_ok() {
                    let st = state.clone();
                    let tab_hint = parse_ws_tab_hint(&path);
                    std::thread::spawn(move || serve_ws_cdp(stream, st, tab_hint));
                }
                continue;
            }
            IncomingHttp::Rest(request) => {
                if let Some(discovery) = handle_cdp_discovery(&request, &state) {
                    let _ = control::write_response(&mut stream, &discovery);
                } else {
                    let response = handle(&request, &state);
                    let _ = control::write_response(&mut stream, &response);
                }
            }
            IncomingHttp::InvalidJson(message) => {
                let _ = control::write_status_response(
                    &mut stream,
                    400,
                    "Bad Request",
                    &json!({"ok": false, "error": message}),
                );
            }
            IncomingHttp::Eof => continue,
        }
        let mut s = lock(&state);
        s.requests_served += 1;
        let rss_mb = sample_rss_mb().unwrap_or(0);
        if worker_born.elapsed().as_secs() >= cfg.worker_max_age_secs
            || (cfg.worker_max_rss_mb > 0 && rss_mb >= cfg.worker_max_rss_mb)
        {
            s.worker_recycles += 1;
            worker_born = Instant::now();
        }
    }
}

#[cfg(target_os = "windows")]
#[repr(C)]
struct ProcessMemoryCountersEx {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_usage: usize,
}

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut core::ffi::c_void;
}

#[cfg(target_os = "windows")]
#[link(name = "psapi")]
unsafe extern "system" {
    fn GetProcessMemoryInfo(
        process: *mut core::ffi::c_void,
        ppsmem_counters: *mut ProcessMemoryCountersEx,
        cb: u32,
    ) -> i32;
}

fn sample_rss_mb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    if let Ok(statm) = std::fs::read_to_string("/proc/self/statm")
        && let Some(resident_pages) = statm.split_whitespace().nth(1)
        && let Ok(pages) = resident_pages.parse::<u64>()
    {
        return Some((pages * 4096) / (1024 * 1024));
    }
    #[cfg(target_os = "windows")]
    {
        // SAFETY: `GetCurrentProcess` returns the current process pseudo-handle
        // and `mem` has its exact byte size in `cb`.
        unsafe {
            let mut mem: ProcessMemoryCountersEx = std::mem::zeroed();
            mem.cb = std::mem::size_of::<ProcessMemoryCountersEx>() as u32;
            if GetProcessMemoryInfo(GetCurrentProcess(), &mut mem, mem.cb) != 0 {
                return Some((mem.working_set_size / (1024 * 1024)) as u64);
            }
        }
    }
    None
}

fn resolve_tab_id(state: &Arc<Mutex<State>>, target: control::TabTarget) -> Option<u64> {
    match target {
        control::TabTarget::Id(id) => Some(id),
        control::TabTarget::Active => lock(state).active,
    }
}

fn handle(request: &control::Request, state: &Arc<Mutex<State>>) -> Value {
    macro_rules! tab_id_or_err {
        ($target:expr) => {
            match resolve_tab_id(state, $target) {
                Some(id) => id,
                None => return json!({"ok": false, "error": "pestaña no encontrada"}),
            }
        };
    }

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
                "rss_mb": sample_rss_mb(),
            })
        }
        Route::ListTabs => {
            let state = lock(state);
            let engine_ok = request
                .query_param("engine")
                .is_none_or(|eng| eng.eq_ignore_ascii_case("wisp"));
            let tabs: Vec<Value> = if engine_ok {
                state
                    .tabs
                    .iter()
                    .map(|tab| tab_json(tab, state.active))
                    .collect()
            } else {
                Vec::new()
            };
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
            let options = state.options.clone();
            let id = state.next_id;
            state.next_id += 1;
            let mut tab = Tab::new(id, url);
            let response = load(&mut tab, &options);
            state.tabs.push(tab);
            state.active = Some(id);
            response
        }
        Route::Navigate(target) => {
            let tab_id = tab_id_or_err!(target);
            let Some(url) = request.body.get("url").and_then(Value::as_str) else {
                return json!({"ok": false, "error": "falta 'url'"});
            };
            if !is_web_url(url) {
                return json!({"ok": false, "error": "falta una 'url' http(s) válida"});
            }
            let wait_until = request
                .body
                .get("waitUntil")
                .and_then(Value::as_str)
                .unwrap_or("load");
            let mut state = lock(state);
            let options = state.options.clone();
            let Some(position) = state.tabs.iter().position(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            state.active = Some(tab_id);
            let tab = &mut state.tabs[position];
            let mut res = navigate(tab, url, &options);
            if wait_until == "networkidle0" {
                settle_network_idle(tab, &mut res);
            }
            res
        }
        Route::Back(target) => {
            let tab_id = tab_id_or_err!(target);
            history_step(state, tab_id, -1)
        }
        Route::Forward(target) => {
            let tab_id = tab_id_or_err!(target);
            history_step(state, tab_id, 1)
        }
        Route::Reload(target) => {
            let tab_id = tab_id_or_err!(target);
            let mut state = lock(state);
            let options = state.options.clone();
            let Some(position) = state.tabs.iter().position(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            state.active = Some(tab_id);
            load(&mut state.tabs[position], &options)
        }
        Route::Close(target) => {
            let tab_id = tab_id_or_err!(target);
            let mut state = lock(state);
            state.tabs.retain(|tab| tab.id != tab_id);
            if state.active == Some(tab_id) {
                state.active = state.tabs.last().map(|tab| tab.id);
            }
            json!({"ok": true})
        }
        Route::Activate(target) => {
            let tab_id = tab_id_or_err!(target);
            let mut state = lock(state);
            if state.tabs.iter().any(|tab| tab.id == tab_id) {
                state.active = Some(tab_id);
            }
            json!({"ok": true})
        }
        Route::Text(target) => {
            let tab_id = tab_id_or_err!(target);
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
        Route::Snapshot(target) => {
            let tab_id = tab_id_or_err!(target);
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
        Route::Eval(target) => {
            let tab_id = tab_id_or_err!(target);
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
        Route::Click(target) => {
            let tab_id = tab_id_or_err!(target);
            let mut state = lock(state);
            let options = state.options.clone();
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
                    let mut result = navigate(tab, &url, &options);
                    result["target"] = el.to_json();
                    return result;
                }
                if let Some(href) = el.href.as_deref()
                    && el.role == "button"
                {
                    let note = format!("botón sin navegación ({href})");
                    return json!({"ok": true, "target": el.to_json(), "note": note});
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
                    let mut result = navigate(tab, &url, &options);
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
        Route::Fill(target) => {
            let tab_id = tab_id_or_err!(target);
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
        Route::SelectOption(target) => {
            let tab_id = tab_id_or_err!(target);
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
        Route::Type(target) => {
            let tab_id = tab_id_or_err!(target);
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
        Route::Press(target) => {
            let tab_id = tab_id_or_err!(target);
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
        Route::Move(target) => {
            let tab_id = tab_id_or_err!(target);
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
        Route::WaitFor(target) => {
            let tab_id = tab_id_or_err!(target);
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
        Route::NetworkRequests(target) => {
            let tab_id = tab_id_or_err!(target);
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
        Route::ConsoleMessages(target) => {
            let tab_id = tab_id_or_err!(target);
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
        Route::Events(target) => {
            let tab_id = tab_id_or_err!(target);
            let mut state = lock(state);
            let Some(tab) = state.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            let drained: Vec<Value> = tab.events.drain(..).map(|e| e.to_json()).collect();
            json!({"ok": true, "events": drained})
        }
        Route::Cdp(target) => {
            let tab_id = tab_id_or_err!(target);
            handle_cdp(state, tab_id, &request.body)
        }
        Route::Screenshot(target) => {
            let tab_id = tab_id_or_err!(target);
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
        Route::Pdf(target) => {
            let tab_id = tab_id_or_err!(target);
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
    let options = s.options.clone();
    let ua = wisp_engine::wisp_net::StealthProfile::for_policy(&options.policy).user_agent;
    match method {
        "Browser.getVersion" => json!({
            "ok": true,
            "result": {
                "protocolVersion": "1.3",
                "product": format!("Wisp/{}", env!("CARGO_PKG_VERSION")),
                "revision": "@wisp",
                "userAgent": ua,
                "jsVersion": "wisp-vm/1.0",
            }
        }),
        "Target.setDiscoverTargets" => json!({"ok": true, "result": {}}),
        "Target.getBrowserContexts" => {
            json!({"ok": true, "result": {"browserContextIds": s.browser_contexts}})
        }
        "Target.disposeBrowserContext" => {
            if let Some(id) = params.get("browserContextId").and_then(Value::as_str) {
                s.browser_contexts.retain(|known| known != id);
                s.tabs.retain(|tab| tab.context_id != id);
            }
            json!({"ok": true, "result": {}})
        }
        // Playwright los emite al adjuntarse al navegador. No hay descargas
        // gestionadas ni permisos que conceder en el motor headless.
        "Browser.setDownloadBehavior"
        | "Browser.grantPermissions"
        | "Browser.resetPermissions"
        | "Browser.setWindowBounds"
        | "Browser.getWindowBounds"
        | "Browser.getBrowserCommandLine"
        | "Browser.crash"
        | "Browser.close" => json!({"ok": true, "result": {}}),
        "Target.createBrowserContext" => {
            // Ids únicos por contexto: un id fijo hace que Playwright reutilice
            // un contexto ya cerrado y pierda la página recién creada.
            let id = format!("ctx-{}", s.browser_contexts.len() + 1);
            s.browser_contexts.push(id.clone());
            json!({"ok": true, "result": {"browserContextId": id}})
        }
        "Target.activateTarget" => json!({"ok": true, "result": {}}),
        "Target.getTargetInfo" => {
            let target_id = params.get("targetId").and_then(Value::as_str).unwrap_or("");
            let found = s
                .tabs
                .iter()
                .find(|tab| target_id.is_empty() || format!("tab-{}", tab.id) == target_id);
            match found {
                Some(tab) => json!({
                    "ok": true,
                    "result": {"targetInfo": {
                        "targetId": format!("tab-{}", tab.id),
                        "type": "page",
                        "title": tab.title,
                        "url": tab.url,
                        "attached": true,
                        "browserContextId": tab.context_id,
                    }}
                }),
                // Sin pestañas, el objetivo del navegador sigue siendo válido.
                None if target_id.is_empty() => json!({
                    "ok": true,
                    "result": {"targetInfo": {
                        "targetId": "wisp-browser",
                        "type": "browser",
                        "title": "",
                        "url": "",
                        "attached": true,
                        "browserContextId": "default",
                    }}
                }),
                None => json!({"ok": false, "error": format!("target {target_id} no encontrado")}),
            }
        }
        "Target.detachFromTarget" => json!({"ok": true, "result": {}}),
        // Playwright conecta en modo «flatten» y espera un
        // `Target.attachedToTarget` por cada pestaña existente.
        "Target.setAutoAttach" => {
            let pending: Vec<(u64, String, String, String)> = s
                .tabs
                .iter()
                .filter(|t| !s.announced_targets.contains(&t.id))
                .map(|t| (t.id, t.title.clone(), t.url.clone(), t.context_id.clone()))
                .collect();
            for (id, title, url, context_id) in pending {
                s.announced_targets.insert(id);
                // El cliente nuevo no debe recibir notificaciones anteriores a
                // su adjunción: reproducirlas hace que Playwright cree frames
                // que luego descarta («Frame has been detached»).
                if let Some(tab) = s.tabs.iter_mut().find(|t| t.id == id) {
                    tab.events.clear();
                    tab.network_log.clear();
                }
                s.events.push(CdpEvent {
                    method: "Target.attachedToTarget".into(),
                    params: json!({
                        "sessionId": format!("sess-tab-{id}"),
                        "targetInfo": {
                            "targetId": format!("tab-{id}"),
                            "type": "page",
                            "title": title,
                            "url": url,
                            "attached": true,
                            "browserContextId": context_id,
                        },
                        "waitingForDebugger": false,
                    }),
                });
            }
            json!({"ok": true, "result": {}})
        }
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
                        "browserContextId": t.context_id,
                    })
                })
                .collect();
            json!({"ok": true, "result": {"targetInfos": target_infos}})
        }
        "Target.attachToTarget" => {
            let target_id = params
                .get("targetId")
                .and_then(Value::as_str)
                .unwrap_or("tab-1")
                .to_owned();
            let session_id = format!("sess-{target_id}");
            let pending = s.tabs.iter().find_map(|tab| {
                (format!("tab-{}", tab.id) == target_id && !s.announced_targets.contains(&tab.id))
                    .then(|| {
                        (
                            tab.id,
                            tab.title.clone(),
                            tab.url.clone(),
                            tab.context_id.clone(),
                        )
                    })
            });
            if let Some((id, title, url, context_id)) = pending {
                s.announced_targets.insert(id);
                if let Some(tab) = s.tabs.iter_mut().find(|t| t.id == id) {
                    tab.events.clear();
                }
                let event = CdpEvent {
                    method: "Target.attachedToTarget".into(),
                    params: json!({
                        "sessionId": session_id,
                        "targetInfo": {
                            "targetId": target_id,
                            "type": "page",
                            "title": title,
                            "url": url,
                            "attached": true,
                            "browserContextId": context_id,
                        },
                        "waitingForDebugger": false,
                    }),
                };
                s.events.push(event);
            }
            json!({"ok": true, "result": {"sessionId": session_id}})
        }
        "Target.createTarget" => {
            let url = params
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("about:blank");
            let id = s.next_id;
            s.next_id += 1;
            let context_id = params
                .get("browserContextId")
                .and_then(Value::as_str)
                .unwrap_or("default")
                .to_owned();
            let mut tab = Tab::new(id, url);
            tab.context_id = context_id;
            if is_web_url(url) {
                let _ = load(&mut tab, &options);
            }
            let target_id = format!("tab-{id}");
            let title = tab.title.clone();
            let current_url = tab.url.clone();
            let tab_context = tab.context_id.clone();
            s.active = Some(id);
            s.announced_targets.insert(id);
            s.tabs.push(tab);
            s.events.push(CdpEvent {
                method: "Target.attachedToTarget".into(),
                params: json!({
                    "sessionId": format!("sess-tab-{id}"),
                    "targetInfo": {
                        "targetId": target_id.clone(),
                        "type": "page",
                        "title": title,
                        "url": current_url,
                        "attached": true,
                        "browserContextId": tab_context,
                    },
                    "waitingForDebugger": false,
                }),
            });
            json!({"ok": true, "result": {"targetId": target_id}})
        }
        "Target.closeTarget" => {
            let target_id = params
                .get("targetId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_default();
            if let Some(id_str) = target_id.strip_prefix("tab-")
                && let Ok(id) = id_str.parse::<u64>()
            {
                s.tabs.retain(|t| t.id != id);
                s.announced_targets.remove(&id);
                if s.active == Some(id) {
                    s.active = s.tabs.last().map(|t| t.id);
                }
                // Chrome anuncia el desmontaje; Playwright espera este evento
                // para completar `page.close()`.
                s.events.push(CdpEvent {
                    method: "Target.detachedFromTarget".into(),
                    params: json!({
                        "sessionId": format!("sess-tab-{id}"),
                        "targetId": target_id,
                    }),
                });
            }
            json!({"ok": true, "result": {"success": true}})
        }
        _ => {
            let Some(tab) = s.tabs.iter_mut().find(|t| t.id == tab_id) else {
                return json!({"ok": false, "error": "pestaña no encontrada"});
            };
            dispatch_tab_cdp(tab, method, &params, &options)
        }
    }
}

fn dispatch_tab_cdp(tab: &mut Tab, method: &str, params: &Value, options: &FetchOptions) -> Value {
    match method {
        // Chrome publica el contexto del documento en cuanto se habilita Page;
        // sin él Playwright no puede evaluar nada en el frame (`title()`, `locator`).
        "Page.enable" => {
            emit_execution_context(tab);
            json!({"ok": true, "result": {}})
        }
        "DOM.enable"
        | "Page.setLifecycleEventsEnabled"
        | "Page.setBypassCSP"
        | "Page.setDownloadBehavior"
        | "Page.bringToFront"
        | "Runtime.runIfWaitingForDebugger"
        | "Network.setCacheDisabled"
        | "Network.setExtraHTTPHeaders"
        | "Network.setUserAgentOverride"
        | "Network.clearBrowserCache"
        | "Network.setBlockedURLs"
        | "Security.setIgnoreCertificateErrors"
        | "Emulation.setLocaleOverride"
        | "Emulation.setUserAgentOverride"
        | "Emulation.setFocusEmulationEnabled"
        | "Emulation.setEmulatedMedia"
        | "Emulation.setScrollbarsHidden"
        | "Emulation.setDefaultBackgroundColorOverride"
        | "Inspector.enable"
        | "Inspector.disable"
        | "Debugger.enable"
        | "Debugger.disable"
        | "Debugger.setAsyncCallStackDepth"
        | "Runtime.setAsyncCallStackDepth"
        | "Runtime.discardConsoleEntries"
        | "Page.setPrerenderAllowed"
        | "Target.setAutoAttach"
        | "Target.setDiscoverTargets" => json!({"ok": true, "result": {}}),
        // Playwright/Puppeteer inyectan su script de utilidades al conectar.
        // El motor no ejecuta ese bundle (usa sintaxis fuera del alcance del
        // VM acotado), así que se acepta la petición sin romper el handshake.
        "Page.addScriptToEvaluateOnNewDocument" => json!({
            "ok": true,
            "result": {"identifier": format!("wisp-script-{}", now_ms())}
        }),
        "Emulation.setDeviceMetricsOverride" => {
            let width = params.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
            let height = params.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
            let dsf = params
                .get("deviceScaleFactor")
                .and_then(Value::as_f64)
                .unwrap_or(1.0);
            let mobile = params
                .get("mobile")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if width > 0 {
                tab.viewport.width = width;
                tab.viewport.screen_width = width;
            }
            if height > 0 {
                tab.viewport.height = height;
                tab.viewport.screen_height = height;
            }
            if dsf > 0.0 {
                tab.viewport.device_scale_factor = dsf;
            }
            tab.viewport.mobile = mobile;
            json!({"ok": true, "result": {}})
        }
        "Emulation.clearDeviceMetricsOverride" => {
            tab.viewport = Viewport::default();
            json!({"ok": true, "result": {}})
        }
        "Page.getLayoutMetrics" => {
            let width = tab.viewport_width();
            let Some(page) = &tab.page else {
                return page_missing_error(tab);
            };
            let doc = match page.layout(width) {
                Ok(doc) => doc,
                Err(error) => return json!({"ok": false, "error": error.to_string()}),
            };
            let w = doc.viewport_width as f64;
            let h = doc.content_height.max(tab.viewport.height as f32) as f64;
            json!({
                "ok": true,
                "result": {
                    "layoutViewport": {"pageX": 0, "pageY": 0, "clientWidth": w, "clientHeight": f64::from(tab.viewport.height)},
                    "contentSize": {"x": 0, "y": 0, "width": w, "height": h},
                    "cssLayoutViewport": {"pageX": 0, "pageY": 0, "clientWidth": w, "clientHeight": f64::from(tab.viewport.height)},
                    "cssContentSize": {"x": 0, "y": 0, "width": w, "height": h},
                }
            })
        }
        "Page.getFrameTree" => json!({
            "ok": true,
            "result": {
                "frameTree": {
                    "frame": {
                        "id": frame_id(tab),
                        "loaderId": loader_id(tab),
                        "url": tab.url,
                        "mimeType": "text/html",
                        "securityOrigin": origin_of(&tab.url),
                    }
                }
            }
        }),
        "Page.navigate" => {
            let url = params.get("url").and_then(Value::as_str).unwrap_or("");
            if !is_web_url(url) {
                return json!({"ok": false, "error": "falta url http(s)"});
            }
            let wait_until = params
                .get("waitUntil")
                .and_then(Value::as_str)
                .unwrap_or("load");
            let mut res = navigate(tab, url, options);
            if wait_until == "networkidle0" {
                settle_network_idle(tab, &mut res);
            }
            // Chrome responde `Page.navigate` con éxito de protocolo y
            // `errorText` cuando la carga falla; un `ok:false` lo convertiría
            // Playwright en "CDP error" en vez de un error de navegación.
            let error_text = if res.get("ok").and_then(Value::as_bool) == Some(true) {
                String::new()
            } else {
                res.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("navegación fallida")
                    .to_owned()
            };
            json!({
                "ok": true,
                "result": {
                    "frameId": frame_id(tab),
                    "loaderId": loader_id(tab),
                    "url": tab.url,
                    "errorText": error_text,
                },
                "events": tab.events.iter().map(CdpEvent::to_json).collect::<Vec<_>>(),
            })
        }
        "Page.reload" => {
            let res = load(tab, options);
            json!({"ok": res.get("ok").and_then(Value::as_bool).unwrap_or(false), "result": {}})
        }
        "Page.createIsolatedWorld" => {
            let world = params
                .get("worldName")
                .and_then(Value::as_str)
                .unwrap_or("wisp-utility")
                .to_owned();
            tab.execution_context += 1;
            let context = tab.execution_context;
            tab.world_name = Some(world.clone());
            emit_context(tab, context, Some(world));
            json!({"ok": true, "result": {"executionContextId": context}})
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
            let width = tab.viewport_width();
            let Some(page) = &tab.page else {
                return page_missing_error(tab);
            };
            match crate::render::render(page, width) {
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
        "Runtime.enable" => json!({"ok": true, "result": {}}),
        "Runtime.getProperties" => json!({"ok": true, "result": {"result": []}}),
        "Runtime.releaseObject" => {
            if let Some(handle) = params.get("objectId").and_then(Value::as_str) {
                tab.object_handles.remove(handle);
            }
            json!({"ok": true, "result": {}})
        }
        "Runtime.releaseObjectGroup" => {
            tab.object_handles.clear();
            json!({"ok": true, "result": {}})
        }
        "Network.enable" => {
            tab.network_enabled = true;
            json!({"ok": true, "result": {}})
        }
        "Network.disable" => {
            tab.network_enabled = false;
            json!({"ok": true, "result": {}})
        }
        "Log.enable" => {
            tab.log_enabled = true;
            json!({"ok": true, "result": {}})
        }
        "Log.disable" => {
            tab.log_enabled = false;
            json!({"ok": true, "result": {}})
        }
        "Log.clear" => json!({"ok": true, "result": {}}),
        "Runtime.callFunctionOn" => {
            let Some(decl) = params.get("functionDeclaration").and_then(Value::as_str) else {
                return json!({"ok": false, "error": "falta 'functionDeclaration'"});
            };
            let mut args = Vec::new();
            for arg in params
                .get("arguments")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                if let Some(value) = arg.get("value") {
                    args.push(serde_json::to_string(value).unwrap_or_else(|_| "null".into()));
                } else if let Some(handle) = arg.get("objectId").and_then(Value::as_str) {
                    let stored = tab
                        .object_handles
                        .get(handle)
                        .cloned()
                        .unwrap_or(Value::Null);
                    args.push(serde_json::to_string(&stored).unwrap_or_else(|_| "null".into()));
                } else {
                    args.push("undefined".to_owned());
                }
            }
            let return_by_value = params
                .get("returnByValue")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let source = format!("({decl})({})", args.join(","));
            let res = eval_in_tab(tab, &source);
            let value = res.get("result").cloned().unwrap_or(Value::Null);
            let mut remote = json!({"type": cdp_type_of(&value), "value": value.clone()});
            if !return_by_value {
                let handle = format!("obj-{}", tab.next_handle);
                tab.next_handle += 1;
                tab.object_handles.insert(handle.clone(), value);
                remote["objectId"] = json!(handle);
            }
            json!({
                "ok": res.get("ok").and_then(Value::as_bool).unwrap_or(false),
                "result": {"result": remote},
                "error": res.get("error").cloned().unwrap_or(Value::Null),
            })
        }
        "Runtime.evaluate" => {
            let expr = params
                .get("expression")
                .and_then(Value::as_str)
                .unwrap_or("");
            if expr.is_empty() {
                return json!({"ok": false, "error": "falta 'expression'"});
            }
            let return_by_value = params
                .get("returnByValue")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let res = eval_in_tab(tab, expr);
            let value = res.get("result").cloned().unwrap_or(Value::Null);
            let mut remote = json!({"type": cdp_type_of(&value), "value": value.clone()});
            if !return_by_value {
                let handle = format!("obj-{}", tab.next_handle);
                tab.next_handle += 1;
                tab.object_handles.insert(handle.clone(), value);
                remote["objectId"] = json!(handle);
            }
            if res.get("ok").and_then(Value::as_bool) != Some(true) {
                return json!({
                    "ok": false,
                    "error": res
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("evaluación fallida"),
                });
            }
            json!({
                "ok": true,
                "result": {"result": remote},
            })
        }
        "DOM.getDocument" => {
            let depth = params.get("depth").and_then(Value::as_i64).unwrap_or(-1);
            match build_dom_root(tab, depth) {
                Ok(root) => json!({"ok": true, "result": {"root": root}}),
                Err(response) => response,
            }
        }
        "DOM.describeNode" => {
            let node_id = params.get("nodeId").and_then(Value::as_u64).unwrap_or(0);
            match build_dom_root(tab, -1) {
                Ok(root) => match find_dom_node(&root, node_id) {
                    Some(node) => json!({"ok": true, "result": {"node": node}}),
                    None => json!({"ok": false, "error": format!("nodo {node_id} no encontrado")}),
                },
                Err(response) => response,
            }
        }
        "DOM.resolveNode" => {
            let node_id = params.get("nodeId").and_then(Value::as_u64).unwrap_or(0);
            json!({
                "ok": true,
                "result": {"object": {
                    "type": "object",
                    "subtype": "node",
                    "objectId": format!("node-{node_id}"),
                    "description": format!("Wisp node {node_id}"),
                }}
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
        "DOM.querySelector" => {
            let selector = params.get("selector").and_then(Value::as_str).unwrap_or("");
            match dom_query_ids(tab, selector) {
                Ok(ids) => {
                    json!({"ok": true, "result": {"nodeId": ids.first().copied().unwrap_or(0)}})
                }
                Err(response) => response,
            }
        }
        "DOM.querySelectorAll" => {
            let selector = params.get("selector").and_then(Value::as_str).unwrap_or("");
            match dom_query_ids(tab, selector) {
                Ok(ids) => json!({"ok": true, "result": {"nodeIds": ids}}),
                Err(response) => response,
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
            let event_type = params
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("mousePressed");
            match event_type {
                "mouseMoved" => {
                    let hit = hit_target(tab, x, y).ok().flatten();
                    json!({"ok": true, "result": {"target": hit.as_ref().map(target_json)}})
                }
                // El foco se guarda como atributo del DOM porque el contexto de
                // JavaScript de Wisp es efímero entre evaluaciones.
                "mousePressed" => {
                    let focused = eval_and_commit(tab, &focus_at_point_js(x, y));
                    json!({
                        "ok": focused.get("ok").and_then(Value::as_bool).unwrap_or(false),
                        "result": {"focused": focused.get("value").cloned().unwrap_or(Value::Null)},
                        "error": focused.get("error").cloned().unwrap_or(Value::Null),
                    })
                }
                "mouseReleased" => {
                    let clicked = eval_and_commit(tab, &click_at_point_js(x, y));
                    let hit = hit_target(tab, x, y).ok().flatten();
                    if let Some(target) = hit
                        && let Some(url) = target.url.clone()
                    {
                        let nav = navigate(tab, &url, options);
                        let ok = nav.get("ok").and_then(Value::as_bool).unwrap_or(false);
                        return json!({
                            "ok": ok,
                            "result": {"navigated": true, "url": url},
                            "error": nav.get("error").cloned().unwrap_or(Value::Null),
                        });
                    }
                    json!({
                        "ok": true,
                        "result": {"clicked": clicked.get("value").cloned().unwrap_or(Value::Null)},
                    })
                }
                _ => json!({"ok": true, "result": {}}),
            }
        }
        "Input.insertText" => {
            let text = params.get("text").and_then(Value::as_str).unwrap_or("");
            input_insert_text(tab, text)
        }
        "Input.dispatchKeyEvent" => {
            let key = params.get("key").and_then(Value::as_str).unwrap_or("");
            let text = params.get("text").and_then(Value::as_str).unwrap_or("");
            input_key(tab, key, text)
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
        _ => {
            let error = format!("método CDP no soportado en headless: {method}");
            json!({"ok": false, "error": error})
        }
    }
}

fn js_quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn cdp_type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "object",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) | Value::Object(_) => "object",
    }
}

/// Envuelve el resultado interno (`{ok, value}`) en la forma de respuesta CDP.
fn shape_result(res: Value, result: Value) -> Value {
    if res.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        json!({"ok": true, "result": result})
    } else {
        json!({
            "ok": false,
            "error": res
                .get("error")
                .cloned()
                .unwrap_or_else(|| json!("error desconocido")),
        })
    }
}

/// Guarda el HTML mutado por un script como nueva versión viva de la página,
/// igual que hace `fill_control` después de escribir en un control.
fn commit_html(tab: &mut Tab, html: String) {
    let Some(page) = &tab.page else { return };
    let updated = FetchedPage::new(
        page.final_url.clone(),
        page.title.clone(),
        html,
        page.stylesheets.clone(),
        page.external_scripts.clone(),
        page.module_scripts.clone(),
        page.images.clone(),
        page.warnings.clone(),
    );
    tab.page = Some(updated);
}

/// Evalúa `js`, persiste las mutaciones del DOM y devuelve `{ok, value}`.
/// Las advertencias del motor se publican como `Log.entryAdded` cuando el
/// cliente activó `Log.enable`.
fn eval_and_commit(tab: &mut Tab, js: &str) -> Value {
    let Some(page) = &tab.page else {
        return page_missing_error(tab);
    };
    match page.eval_js(js) {
        Ok(result) => {
            let value = result.value.clone().unwrap_or(Value::Null);
            for warning in &result.warnings {
                tab.console_log.push(ConsoleRecord {
                    level: "warning".into(),
                    text: warning.clone(),
                    timestamp_ms: now_ms(),
                });
                if tab.log_enabled {
                    tab.events.push(CdpEvent {
                        method: "Log.entryAdded".into(),
                        params: json!({"entry": {
                            "source": "javascript",
                            "level": "warning",
                            "text": warning,
                            "timestamp": now_ms() as f64,
                        }}),
                    });
                }
            }
            if let Some(html) = result.html {
                commit_html(tab, html);
            }
            json!({"ok": true, "value": value})
        }
        Err(error) => json!({"ok": false, "error": error.to_string()}),
    }
}

// El foco no puede vivir en el runtime de JavaScript porque el contexto de Wisp
// es efímero entre evaluaciones: se marca el nodo con un atributo del DOM.
// El marcador es una clase porque el motor de selectores del DOM nativo solo
// entiende `tag`, `#id` y `.class` (no selectores de atributo).
const JS_FOCUS_AT: &str = r#"(function(){var el=document.elementFromPoint(X,Y);if(!el)return '';var old=document.querySelectorAll('.__wisp_focus');for(var i=0;i<old.length;i++){old[i].classList.remove('__wisp_focus');}el.classList.add('__wisp_focus');return (el.getAttribute('name')||el.getAttribute('id')||el.nodeName||'');})()"#;
const JS_CLICK_AT: &str = r#"(function(){var el=document.elementFromPoint(X,Y);if(!el)return false;el.click();return true;})()"#;
const JS_INSERT_TEXT: &str = r#"(function(t){var el=document.querySelector('.__wisp_focus');if(!el)return false;el.value=(el.value||'')+t;el.setAttribute('value',el.value);return true;})"#;
const JS_KEY_ENTER: &str = r#"(function(){var el=document.querySelector('.__wisp_focus');if(!el)return 'none';var f=el.form;if(f&&f.submit){f.submit();return 'submit';}el.click();return 'click';})()"#;
const JS_KEY_TAB: &str = r#"(function(){var els=document.querySelectorAll('a,button,input,select,textarea');if(!els.length)return false;var cur=document.querySelector('.__wisp_focus');var idx=-1;for(var i=0;i<els.length;i++){if(els[i]===cur){idx=i;}}var next=els[idx+1];if(!next){next=els[0];}if(cur){cur.classList.remove('__wisp_focus');}next.classList.add('__wisp_focus');return true;})()"#;
const JS_KEY_BACKSPACE: &str = r#"(function(){var el=document.querySelector('.__wisp_focus');if(!el)return false;var v=el.value||'';el.value=v.substring(0,v.length-1);el.setAttribute('value',el.value);return true;})()"#;
const JS_KEY_ESCAPE: &str = r#"(function(){var all=document.querySelectorAll('.__wisp_focus');for(var i=0;i<all.length;i++){all[i].classList.remove('__wisp_focus');}return true;})()"#;

fn focus_at_point_js(x: f64, y: f64) -> String {
    JS_FOCUS_AT
        .replace('X', &x.to_string())
        .replace('Y', &y.to_string())
}

fn click_at_point_js(x: f64, y: f64) -> String {
    JS_CLICK_AT
        .replace('X', &x.to_string())
        .replace('Y', &y.to_string())
}

fn input_insert_text(tab: &mut Tab, text: &str) -> Value {
    let source = format!("{JS_INSERT_TEXT}({})", js_quote(text));
    shape_result(eval_and_commit(tab, &source), json!({}))
}

/// Traduce `Input.dispatchKeyEvent` a acciones del DOM. Las teclas sin efecto
/// propio se tratan como texto (`text`), que es lo que envía Playwright al
/// escribir carácter a carácter.
fn input_key(tab: &mut Tab, key: &str, text: &str) -> Value {
    let normalized = key.to_ascii_lowercase();
    let handler = match normalized.as_str() {
        "enter" | "return" => Some(JS_KEY_ENTER),
        "tab" => Some(JS_KEY_TAB),
        "backspace" | "delete" => Some(JS_KEY_BACKSPACE),
        "escape" | "esc" => Some(JS_KEY_ESCAPE),
        _ => None,
    };
    if let Some(js) = handler {
        let res = eval_and_commit(tab, js);
        let applied = res.get("value").cloned().unwrap_or(Value::Null);
        return shape_result(res, json!({"applied": applied}));
    }
    if !text.is_empty() {
        return input_insert_text(tab, text);
    }
    json!({"ok": true, "result": {"applied": false}})
}

/// HTML vivo de la pestaña tal como lo ve el motor (ya con las mutaciones de
/// los scripts), o el cuerpo original si la etapa de scripts falló.
fn live_html(tab: &Tab) -> Option<String> {
    let page = tab.page.as_ref()?;
    Some(page.document_html().unwrap_or_else(|_| page.html.clone()))
}

/// Árbol DOM en la forma de `DOM.getDocument`: `nodeId` 1 es el documento y los
/// elementos se numeran en pre-orden desde 2, igual que `dom_query_ids`.
///
/// Se construye en Rust con el mismo parser del motor y no en JavaScript: el
/// VM acotado de Wisp no conserva contadores mutables entre llamadas anidadas,
/// así que un recorrido en JS devolvía `nodeId: null`.
fn build_dom_root(tab: &Tab, depth: i64) -> std::result::Result<Value, Value> {
    let Some(html) = live_html(tab) else {
        return Err(page_missing_error(tab));
    };
    let document = Html::parse_document(&html);
    let mut next = 1u64;
    let root = element_to_cdp(&document.root_element(), &mut next, depth);
    Ok(json!({
        "nodeId": 1,
        "backendNodeId": 1,
        "nodeType": 9,
        "nodeName": "#document",
        "localName": "",
        "nodeValue": "",
        "documentURL": tab.url,
        "childNodeCount": 1,
        "children": [root],
    }))
}

fn element_to_cdp(element: &ElementRef, next: &mut u64, depth: i64) -> Value {
    *next += 1;
    let id = *next;
    let attributes: Vec<Value> = element
        .value()
        .attrs()
        .flat_map(|(name, value)| [json!(name), json!(value)])
        .collect();
    let children: Vec<ElementRef> = element.children().filter_map(ElementRef::wrap).collect();
    let mut node = json!({
        "nodeId": id,
        "backendNodeId": id,
        "nodeType": 1,
        "nodeName": element.value().name().to_ascii_uppercase(),
        "localName": element.value().name(),
        "nodeValue": "",
        "attributes": attributes,
        "childNodeCount": children.len(),
    });
    if depth != 0 && !children.is_empty() {
        let child_depth = if depth < 0 { depth } else { depth - 1 };
        let kids: Vec<Value> = children
            .iter()
            .map(|child| element_to_cdp(child, next, child_depth))
            .collect();
        node["children"] = json!(kids);
    }
    node
}

fn dom_query_ids(tab: &Tab, selector: &str) -> std::result::Result<Vec<u64>, Value> {
    let Some(html) = live_html(tab) else {
        return Err(page_missing_error(tab));
    };
    let Ok(parsed) = Selector::parse(selector) else {
        return Ok(Vec::new());
    };
    let document = Html::parse_document(&html);
    let mut next = 1u64;
    let mut found = Vec::new();
    collect_selector_ids(&document.root_element(), &parsed, &mut next, &mut found);
    Ok(found)
}

fn collect_selector_ids(
    element: &ElementRef,
    selector: &Selector,
    next: &mut u64,
    found: &mut Vec<u64>,
) {
    *next += 1;
    let id = *next;
    if selector.matches(element) {
        found.push(id);
    }
    let children: Vec<ElementRef> = element.children().filter_map(ElementRef::wrap).collect();
    for child in &children {
        collect_selector_ids(child, selector, next, found);
    }
}

fn find_dom_node(node: &Value, node_id: u64) -> Option<&Value> {
    if node.get("nodeId").and_then(Value::as_u64) == Some(node_id) {
        return Some(node);
    }
    for child in node
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(found) = find_dom_node(child, node_id) {
            return Some(found);
        }
    }
    None
}

fn fill_by_name_js(name: &str, value: &str) -> String {
    let q_name = js_quote(name);
    let q_val = js_quote(value);
    let mut js = String::from("(function(){ var el = document.querySelector('[name=' + ");
    js.push_str(&q_name);
    js.push_str(" + ']') || document.getElementById(");
    js.push_str(&q_name);
    js.push_str("); if (!el) return false; el.value = ");
    js.push_str(&q_val);
    js.push_str("; el.setAttribute('value', ");
    js.push_str(&q_val);
    js.push_str("); return true; })()");
    js
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

fn fill_control(tab: &mut Tab, ref_id: Option<&str>, name: Option<&str>, value: &str) -> Value {
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
    let js = fill_by_name_js(&field_name, value);
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
    let options = state.options.clone();
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
    load(tab, &options)
}

fn navigate(tab: &mut Tab, url: &str, options: &FetchOptions) -> Value {
    tab.history.truncate(tab.index + 1);
    tab.history.push(url.to_owned());
    tab.index = tab.history.len() - 1;
    tab.url = url.to_owned();
    load(tab, options)
}

/// En Chrome el frame principal comparte identificador con el target de la
/// pestaña, y Playwright indexa sus sesiones por ese id.
fn frame_id(tab: &Tab) -> String {
    format!("tab-{}", tab.id)
}

fn loader_id(tab: &Tab) -> String {
    format!("loader-{}", tab.id)
}

fn origin_of(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{}", rest.split('/').next().unwrap_or("")),
        None => "null".to_owned(),
    }
}

fn emit_context(tab: &mut Tab, context: u64, world: Option<String>) {
    let frame = frame_id(tab);
    let origin = origin_of(&tab.url);
    // Playwright distingue el mundo principal por `isDefault` y el aislado por
    // `name`: sin este último no puede evaluar en el mundo de utilidades.
    let (name, is_default, kind) = match &world {
        Some(name) => (name.clone(), false, "isolated"),
        None => ("main".to_owned(), true, "default"),
    };
    tab.events.push(CdpEvent {
        method: "Runtime.executionContextCreated".into(),
        params: json!({
            "context": {
                "id": context,
                "origin": origin,
                "name": name,
                "uniqueId": format!("ctx-{context}"),
                "auxData": {"frameId": frame, "isDefault": is_default, "type": kind},
            }
        }),
    });
}

fn emit_execution_context(tab: &mut Tab) {
    let context = tab.execution_context;
    emit_context(tab, context, None);
}

fn load(tab: &mut Tab, options: &FetchOptions) -> Value {
    let started = Instant::now();
    let now = now_ms();
    tab.events.push(CdpEvent {
        method: "Network.requestWillBeSent".into(),
        params: json!({"request": {"url": tab.url, "method": "GET"}, "timestamp": now}),
    });

    match fetch_url_with_options(&tab.url, options) {
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
            let frame = frame_id(tab);
            let loader = loader_id(tab);
            let origin = origin_of(&tab.url);
            tab.events.push(CdpEvent {
                method: "Page.frameNavigated".into(),
                params: json!({
                    "frame": {
                        "id": frame,
                        "loaderId": loader,
                        "url": tab.url,
                        "mimeType": "text/html",
                        "securityOrigin": origin,
                    }
                }),
            });
            tab.execution_context += 1;
            emit_execution_context(tab);
            if let Some(world) = tab.world_name.clone() {
                tab.execution_context += 1;
                let context = tab.execution_context;
                emit_context(tab, context, Some(world));
            }
            let stamp = elapsed as f64 / 1000.0;
            for name in ["DOMContentLoaded", "load"] {
                tab.events.push(CdpEvent {
                    method: "Page.lifecycleEvent".into(),
                    params: json!({
                        "frameId": frame,
                        "loaderId": loader,
                        "name": name,
                        "timestamp": stamp,
                    }),
                });
            }
            tab.events.push(CdpEvent {
                method: "Page.loadEventFired".into(),
                params: json!({"timestamp": stamp}),
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

fn settle_network_idle(tab: &mut Tab, response: &mut Value) {
    if let Some(page) = &tab.page {
        // Ensure page script/DOM stage and layout have fully drained before returning.
        let _ = page.document_html();
        let _ = page.layout(crate::DEFAULT_VIEWPORT_WIDTH);
        tab.events.push(CdpEvent {
            method: "Page.lifecycleEvent".into(),
            params: json!({
                "frameId": frame_id(tab),
                "loaderId": loader_id(tab),
                "name": "networkIdle",
                "timestamp": now_ms() as f64 / 1000.0,
            }),
        });
        response["waitUntil"] = json!("networkidle0");
    }
}

fn handle_cdp_discovery(request: &control::Request, state: &Arc<Mutex<State>>) -> Option<Value> {
    let path = request.path.split('?').next().unwrap_or("/");
    match (request.method.as_str(), path) {
        ("GET", "/json/version") => {
            let s = lock(state);
            let profile = wisp_engine::wisp_net::StealthProfile::for_policy(&s.options.policy);
            let ua = profile.user_agent;
            Some(json!({
                "Browser": format!("Wisp/{}", env!("CARGO_PKG_VERSION")),
                "Protocol-Version": "1.3",
                "User-Agent": ua,
                "V8-Version": "wisp-vm/1.0",
                "WebKit-Version": "wisp-layout/0.1",
                "webSocketDebuggerUrl": format!("ws://127.0.0.1:{}/devtools/browser/wisp", s.port),
            }))
        }
        ("GET", "/json" | "/json/list") => {
            let mut s = lock(state);
            if s.tabs.is_empty() {
                let id = s.next_id;
                s.next_id += 1;
                s.tabs.push(Tab::new(id, "about:blank"));
                s.active = Some(id);
            }
            let port = s.port;
            let list: Vec<Value> = s
                .tabs
                .iter()
                .map(|t| {
                    let ws_url = format!("ws://127.0.0.1:{port}/devtools/page/wisp-{}", t.id);
                    let frontend = format!(
                        "/devtools/inspector.html?ws=127.0.0.1:{port}/devtools/page/wisp-{}",
                        t.id
                    );
                    json!({
                        "description": "",
                        "devtoolsFrontendUrl": frontend,
                        "id": format!("wisp-{}", t.id),
                        "title": t.title,
                        "type": "page",
                        "url": t.url,
                        "webSocketDebuggerUrl": ws_url,
                    })
                })
                .collect();
            Some(Value::Array(list))
        }
        ("GET" | "PUT", "/json/new") => {
            let url = request
                .path
                .split_once('?')
                .map(|(_, q)| q)
                .filter(|q| !q.is_empty())
                .map(|q| q.strip_prefix("url=").unwrap_or(q).to_owned())
                .unwrap_or_else(|| "about:blank".to_owned());
            let mut s = lock(state);
            let options = s.options.clone();
            let id = s.next_id;
            s.next_id += 1;
            let mut tab = Tab::new(id, &url);
            if is_web_url(&url) {
                let _ = load(&mut tab, &options);
            }
            let port = s.port;
            let desc = json!({
                "id": format!("wisp-{id}"),
                "title": tab.title,
                "type": "page",
                "url": tab.url,
                "webSocketDebuggerUrl": format!("ws://127.0.0.1:{port}/devtools/page/wisp-{id}"),
            });
            s.active = Some(id);
            s.tabs.push(tab);
            Some(desc)
        }
        _ if path.starts_with("/json/close/") => {
            if let Some(id_str) = path.strip_prefix("/json/close/wisp-")
                && let Ok(id) = id_str.parse::<u64>()
            {
                let mut s = lock(state);
                s.tabs.retain(|t| t.id != id);
                if s.active == Some(id) {
                    s.active = s.tabs.last().map(|t| t.id);
                }
            }
            Some(json!({"ok": true, "status": "Target is closing"}))
        }
        _ => None,
    }
}

fn parse_ws_tab_hint(path: &str) -> Option<u64> {
    let clean = path.split('?').next().unwrap_or(path);
    clean
        .rsplit('/')
        .next()
        .and_then(|seg| seg.strip_prefix("wisp-").or(Some(seg)))
        .and_then(|num| num.parse::<u64>().ok())
}

fn serve_ws_cdp(mut stream: TcpStream, state: Arc<Mutex<State>>, tab_hint: Option<u64>) {
    loop {
        match ws::read_ws_message(&mut stream) {
            Ok(Some(WsMessage::Ping(payload))) => {
                if ws::write_ws_pong(&mut stream, &payload).is_err() {
                    break;
                }
            }
            Ok(Some(WsMessage::Pong(_))) => {}
            Ok(Some(WsMessage::Close)) | Ok(None) | Err(_) => {
                let _ = ws::write_ws_close(&mut stream);
                break;
            }
            Ok(Some(WsMessage::Text(text))) => {
                let Ok(req) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let id = req.get("id").cloned().unwrap_or(Value::Null);
                let session_id = req.get("sessionId").cloned();
                let method = req
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

                // Los métodos de navegador no pertenecen a ninguna pestaña y
                // un navegador CDP recién conectado no tiene páginas: crear
                // una pestaña fantasma aquí rompe el contexto de Playwright
                // (ve una página que luego no puede reconstruir).
                let browser_level = method.starts_with("Browser.")
                    || method.starts_with("Target.")
                    || method.starts_with("SystemInfo.");
                let target_tab_id = {
                    let mut s = lock(&state);
                    let chosen = tab_hint
                        .or_else(|| {
                            session_id
                                .as_ref()
                                .and_then(Value::as_str)
                                .and_then(|sid| sid.strip_prefix("sess-tab-"))
                                .and_then(|num| num.parse::<u64>().ok())
                        })
                        .or(s.active);
                    match chosen {
                        Some(tid) if s.tabs.iter().any(|t| t.id == tid) => tid,
                        _ if browser_level => 0,
                        _ => {
                            let nid = s.next_id;
                            s.next_id += 1;
                            s.tabs.push(Tab::new(nid, "about:blank"));
                            s.active = Some(nid);
                            nid
                        }
                    }
                };

                // Traza opcional del protocolo para depurar clientes CDP
                // reales (Playwright, Puppeteer): WISP_CDP_TRACE=1.
                let traced = std::env::var_os("WISP_CDP_TRACE").is_some();
                if traced {
                    eprintln!(
                        "<- [{target_tab_id}] {method} {}{}",
                        params,
                        session_id
                            .as_ref()
                            .and_then(Value::as_str)
                            .map(|sid| format!(" (session {sid})"))
                            .unwrap_or_default()
                    );
                }

                let cdp_body = json!({"method": method, "params": params});
                let resp = handle_cdp(&state, target_tab_id, &cdp_body);
                if traced {
                    eprintln!("-> [{target_tab_id}] {method} {resp}");
                }

                let mut rpc = if resp.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                    json!({
                        "id": id,
                        "result": resp.get("result").cloned().unwrap_or_else(|| json!({})),
                    })
                } else {
                    let msg = resp
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("CDP error");
                    json!({
                        "id": id,
                        "error": {
                            "code": -32000,
                            "message": msg,
                        }
                    })
                };
                if let Some(ref sid) = session_id {
                    rpc["sessionId"] = sid.clone();
                }

                // Las notificaciones salen **antes** de la respuesta, como en
                // Chrome: Playwright hace `_crPages.get(targetId)._page` nada
                // más resolver `Target.createTarget`, así que el
                // `Target.attachedToTarget` de ese target tiene que haber
                // llegado ya.
                let pending_events: Vec<Value> = {
                    let mut s = lock(&state);
                    let mut events: Vec<Value> = s.events.drain(..).map(|e| e.to_json()).collect();
                    if let Some(tab) = s.tabs.iter_mut().find(|t| t.id == target_tab_id) {
                        events.extend(tab.events.drain(..).map(|e| e.to_json()));
                    }
                    events
                };
                for mut ev in pending_events {
                    // `Target.attachedToTarget` anuncia una sesión nueva y viaja
                    // a nivel navegador. Etiquetarlo con la sesión de la
                    // petición en curso hace que Playwright lo lea como una
                    // auto-adjunción de esa página y descarte el target.
                    let session_scoped =
                        ev.get("method").and_then(Value::as_str) != Some("Target.attachedToTarget");
                    if session_scoped && let Some(ref sid) = session_id {
                        ev["sessionId"] = sid.clone();
                    }
                    if traced {
                        eprintln!("=> [{target_tab_id}] evento {ev}");
                    }
                    if ws::write_ws_text(&mut stream, &ev.to_string()).is_err() {
                        return;
                    }
                }

                if ws::write_ws_text(&mut stream, &rpc.to_string()).is_err() {
                    break;
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state(port: u16) -> Arc<Mutex<State>> {
        Arc::new(Mutex::new(State {
            tabs: Vec::new(),
            active: None,
            next_id: 1,
            port,
            options: FetchOptions::default(),
            started_at: Instant::now(),
            requests_served: 0,
            worker_recycles: 0,
            events: Vec::new(),
            browser_contexts: Vec::new(),
            announced_targets: HashSet::new(),
        }))
    }

    #[test]
    fn cdp_discovery_endpoints_expose_websocket_urls() {
        let state = test_state(49123);
        let version_req = control::Request {
            method: "GET".into(),
            path: "/json/version".into(),
            body: Value::Null,
        };
        let version = handle_cdp_discovery(&version_req, &state).expect("version");
        assert_eq!(
            version["webSocketDebuggerUrl"],
            "ws://127.0.0.1:49123/devtools/browser/wisp"
        );

        let list_req = control::Request {
            method: "GET".into(),
            path: "/json/list".into(),
            body: Value::Null,
        };
        let list = handle_cdp_discovery(&list_req, &state).expect("list");
        let arr = list.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(
            arr[0]["webSocketDebuggerUrl"],
            "ws://127.0.0.1:49123/devtools/page/wisp-1"
        );
    }

    #[test]
    fn websocket_cdp_roundtrip_handles_browser_version_and_runtime_eval() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = test_state(port);
        {
            let mut s = lock(&state);
            s.tabs.push(Tab {
                id: 1,
                url: "https://example.com/".into(),
                title: "Test".into(),
                history: vec!["https://example.com/".into()],
                index: 0,
                page: Some(FetchedPage::new(
                    "https://example.com/".into(),
                    "Test".into(),
                    "<html><body><h1 id=\"h\">Hola</h1></body></html>".into(),
                    vec![],
                    HashMap::new(),
                    HashMap::new(),
                    HashMap::new(),
                    vec![],
                )),
                error: None,
                focused_ref: None,
                network_log: vec![],
                console_log: vec![],
                events: vec![],
                cookies: HashMap::new(),
                io_streams: HashMap::new(),
                screencast_active: false,
                fetch_intercept: false,
                viewport: Viewport::default(),
                network_enabled: false,
                log_enabled: false,
                execution_context: 1,
                world_name: None,
                object_handles: HashMap::new(),
                next_handle: 1,
                context_id: "default".to_owned(),
            });
            s.active = Some(1);
        }

        let st = state.clone();
        let srv = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            match read_incoming_http(&mut stream) {
                IncomingHttp::WebSocket { path, key } => {
                    ws::write_handshake(&mut stream, &key).unwrap();
                    serve_ws_cdp(stream, st, parse_ws_tab_hint(&path));
                }
                _ => panic!("expected websocket upgrade"),
            }
        });

        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let req_bytes = concat!(
            "GET /devtools/page/wisp-1 HTTP/1.1\r\n",
            "Host: 127.0.0.1\r\n",
            "Upgrade: websocket\r\n",
            "Connection: Upgrade\r\n",
            "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
            "Sec-WebSocket-Version: 13\r\n\r\n"
        )
        .as_bytes();
        client.write_all(req_bytes).unwrap();
        let mut resp_buf = [0u8; 512];
        let mut total = 0;
        while !resp_buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
            let n = client.read(&mut resp_buf[total..]).unwrap();
            total += n;
        }
        let head = String::from_utf8_lossy(&resp_buf[..total]);
        assert!(head.contains("101 Switching Protocols"));
        assert!(head.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));

        // Send masked Runtime.evaluate frame
        let msg = concat!(
            r#"{"id":7,"method":"Runtime.evaluate","#,
            r#""params":{"expression":"document.getElementById('h').textContent"}}"#
        )
        .as_bytes();
        let mask = [1u8, 2, 3, 4];
        let mut frame = vec![0x81, 0x80 | (msg.len() as u8)];
        frame.extend_from_slice(&mask);
        for (i, b) in msg.iter().enumerate() {
            frame.push(b ^ mask[i % 4]);
        }
        client.write_all(&frame).unwrap();

        let mut ws_hdr = [0u8; 2];
        client.read_exact(&mut ws_hdr).unwrap();
        let plen = (ws_hdr[1] & 0x7f) as usize;
        let mut pbuf = vec![0u8; plen];
        client.read_exact(&mut pbuf).unwrap();
        let reply: Value = serde_json::from_slice(&pbuf).unwrap();
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["result"]["result"]["value"], "Hola");

        // Close
        client.write_all(&[0x88, 0x80, 0, 0, 0, 0]).unwrap();
        srv.join().unwrap();
    }

    fn write_client_frame(client: &mut TcpStream, payload: &[u8]) {
        let mask = [7u8, 11, 13, 17];
        let mut frame = vec![0x81u8];
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        for (i, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[i % 4]);
        }
        client.write_all(&frame).unwrap();
    }

    fn read_server_frame(client: &mut TcpStream) -> Value {
        let mut head = [0u8; 2];
        client.read_exact(&mut head).unwrap();
        let len = match head[1] & 0x7f {
            126 => {
                let mut ext = [0u8; 2];
                client.read_exact(&mut ext).unwrap();
                u16::from_be_bytes(ext) as usize
            }
            127 => {
                let mut ext = [0u8; 8];
                client.read_exact(&mut ext).unwrap();
                u64::from_be_bytes(ext) as usize
            }
            n => n as usize,
        };
        let mut buf = vec![0u8; len];
        client.read_exact(&mut buf).unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    /// Envía una petición CDP y devuelve `(respuesta, eventos)`.
    fn cdp_call(
        client: &mut TcpStream,
        id: u64,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> (Value, Vec<Value>) {
        let mut request = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            request["sessionId"] = json!(session);
        }
        write_client_frame(client, request.to_string().as_bytes());
        let mut events = Vec::new();
        loop {
            let frame = read_server_frame(client);
            if frame.get("id").and_then(Value::as_u64) == Some(id) {
                return (frame, events);
            }
            events.push(frame);
        }
    }

    fn connect_cdp(
        listener: TcpListener,
        state: Arc<Mutex<State>>,
        path: &'static str,
    ) -> (TcpStream, std::thread::JoinHandle<()>) {
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            match read_incoming_http(&mut stream) {
                IncomingHttp::WebSocket { path, key } => {
                    ws::write_handshake(&mut stream, &key).unwrap();
                    serve_ws_cdp(stream, state, parse_ws_tab_hint(&path));
                }
                _ => panic!("expected websocket upgrade"),
            }
        });
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        client.write_all(request.as_bytes()).unwrap();
        let mut buffer = [0u8; 512];
        let mut total = 0;
        while !buffer[..total].windows(4).any(|w| w == b"\r\n\r\n") {
            let read = client.read(&mut buffer[total..]).unwrap();
            total += read;
        }
        let head = String::from_utf8_lossy(&buffer[..total]);
        assert!(head.contains("101 Switching Protocols"));
        (client, server)
    }

    fn seeded_state(port: u16) -> Arc<Mutex<State>> {
        let state = test_state(port);
        {
            let mut s = lock(&state);
            let mut tab = Tab::new(1, "https://example.com/");
            tab.title = "Test".into();
            tab.page = Some(FetchedPage::new(
                "https://example.com/".into(),
                "Test".into(),
                "<html><body><h1 id=\"h\">Hola</h1><input id=\"q\" value=\"\"><a href=\"https://example.com/next\">next</a></body></html>".into(),
                vec![],
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                vec![],
            ));
            s.tabs.push(tab);
            s.active = Some(1);
        }
        state
    }

    /// Reproduce la secuencia que Playwright emite al hacer `connectOverCDP`:
    /// descubrimiento, auto-attach en modo flatten, habilitación de dominios y
    /// después interacción por `Input`, `DOM` y `Network`.
    #[test]
    fn chromium_frame_lifecycle_uses_target_id_and_lifecycle_events() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = seeded_state(port);
        let (mut client, _server) = connect_cdp(listener, state, "/devtools/browser/wisp");
        let session = "sess-tab-1";

        let (reply, events) = cdp_call(
            &mut client,
            1,
            "Target.setAutoAttach",
            json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}),
            None,
        );
        assert!(reply.get("error").is_none(), "{reply}");
        let target = events
            .iter()
            .find(|event| event["method"] == "Target.attachedToTarget")
            .expect("Target.attachedToTarget")["params"]["targetInfo"]["targetId"]
            .as_str()
            .expect("targetId")
            .to_owned();

        let (reply, events) = cdp_call(&mut client, 2, "Page.enable", json!({}), Some(session));
        assert!(reply.get("error").is_none(), "{reply}");
        // Chrome publica el contexto del documento al habilitar Page; sin él
        // Playwright no puede evaluar en el frame.
        let context = events
            .iter()
            .find(|event| event["method"] == "Runtime.executionContextCreated")
            .expect("Runtime.executionContextCreated");
        assert_eq!(
            context["params"]["context"]["auxData"]["frameId"],
            target.as_str()
        );
        assert_eq!(context["params"]["context"]["auxData"]["isDefault"], true);

        let (reply, _) = cdp_call(
            &mut client,
            3,
            "Page.getFrameTree",
            json!({}),
            Some(session),
        );
        // En Chrome el frame principal comparte identificador con el target.
        assert_eq!(
            reply["result"]["frameTree"]["frame"]["id"],
            target.as_str(),
            "{reply}"
        );
        assert!(
            reply["result"]["frameTree"]["frame"]["loaderId"].is_string(),
            "{reply}"
        );

        let (reply, _) = cdp_call(
            &mut client,
            4,
            "Page.createIsolatedWorld",
            json!({"frameId": target, "worldName": "utility"}),
            Some(session),
        );
        assert!(
            reply["result"]["executionContextId"].as_u64().is_some(),
            "{reply}"
        );

        // Un fallo de carga es `errorText` dentro de una respuesta válida, no
        // un error de protocolo (que Playwright reportaría como CDP error).
        let (reply, events) = cdp_call(
            &mut client,
            5,
            "Page.navigate",
            json!({"url": "http://127.0.0.1:1/"}),
            Some(session),
        );
        assert!(reply.get("error").is_none(), "{reply}");
        assert_eq!(reply["result"]["frameId"], target.as_str(), "{reply}");
        assert!(
            reply["result"]["errorText"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "{reply}"
        );
        assert!(
            events
                .iter()
                .all(|event| event["method"] != "Page.lifecycleEvent"),
            "sin carga no debe anunciarse load"
        );
    }

    #[test]
    fn playwright_connect_over_cdp_handshake_serves_input_dom_and_network() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = seeded_state(port);
        let (mut client, server) = connect_cdp(listener, state, "/devtools/browser/wisp");
        let session = "sess-tab-1";

        let (reply, _) = cdp_call(
            &mut client,
            1,
            "Target.setDiscoverTargets",
            json!({"discover": true}),
            None,
        );
        assert!(reply.get("error").is_none(), "{reply}");

        let (reply, events) = cdp_call(
            &mut client,
            2,
            "Target.setAutoAttach",
            json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}),
            None,
        );
        assert!(reply.get("error").is_none(), "{reply}");
        let attached = events
            .iter()
            .find(|event| event["method"] == "Target.attachedToTarget")
            .expect("Target.attachedToTarget");
        assert_eq!(attached["params"]["sessionId"], session);
        assert_eq!(attached["params"]["targetInfo"]["type"], "page");

        for (id, method) in [
            (3, "Runtime.enable"),
            (4, "Page.enable"),
            (5, "Network.enable"),
            (6, "Log.enable"),
        ] {
            let (reply, _) = cdp_call(&mut client, id, method, json!({}), Some(session));
            assert!(reply.get("error").is_none(), "{method}: {reply}");
        }

        let (reply, _) = cdp_call(
            &mut client,
            7,
            "Emulation.setDeviceMetricsOverride",
            json!({"width": 640, "height": 480, "deviceScaleFactor": 1, "mobile": false}),
            Some(session),
        );
        assert!(reply.get("error").is_none(), "{reply}");
        let (reply, _) = cdp_call(
            &mut client,
            8,
            "Page.getLayoutMetrics",
            json!({}),
            Some(session),
        );
        assert_eq!(reply["result"]["contentSize"]["width"], 640.0);

        let (reply, _) = cdp_call(
            &mut client,
            9,
            "DOM.getDocument",
            json!({"depth": -1, "pierce": true}),
            Some(session),
        );
        let root = &reply["result"]["root"];
        assert_eq!(root["nodeId"], 1);
        assert_eq!(root["nodeType"], 9);
        assert_eq!(root["nodeName"], "#document");
        assert_eq!(root["children"][0]["nodeName"], "HTML", "{reply}");
        assert_eq!(root["children"][0]["nodeId"], 2, "{reply}");

        let (reply, _) = cdp_call(
            &mut client,
            10,
            "DOM.querySelector",
            json!({"nodeId": 1, "selector": "#h"}),
            Some(session),
        );
        let heading_id = reply["result"]["nodeId"].as_u64().expect("nodeId");
        assert!(heading_id > 1, "{reply}");
        let (reply, _) = cdp_call(
            &mut client,
            11,
            "DOM.describeNode",
            json!({"nodeId": heading_id}),
            Some(session),
        );
        assert_eq!(reply["result"]["node"]["nodeName"], "H1");

        let (reply, _) = cdp_call(
            &mut client,
            12,
            "Input.dispatchMouseEvent",
            json!({"type": "mousePressed", "x": 8.0, "y": 8.0, "button": "left", "clickCount": 1}),
            Some(session),
        );
        assert!(reply.get("error").is_none(), "{reply}");
        let (reply, _) = cdp_call(
            &mut client,
            13,
            "Input.insertText",
            json!({"text": "hola"}),
            Some(session),
        );
        assert!(reply.get("error").is_none(), "{reply}");
        let (reply, _) = cdp_call(
            &mut client,
            14,
            "Input.dispatchKeyEvent",
            json!({"type": "keyDown", "key": "Backspace"}),
            Some(session),
        );
        assert!(reply.get("error").is_none(), "{reply}");
        let (reply, _) = cdp_call(
            &mut client,
            15,
            "Input.dispatchKeyEvent",
            json!({"type": "char", "key": "a", "text": "a"}),
            Some(session),
        );
        assert!(reply.get("error").is_none(), "{reply}");

        let (reply, _) = cdp_call(
            &mut client,
            16,
            "Network.getResponseBody",
            json!({"requestId": "req-1"}),
            Some(session),
        );
        assert!(reply.get("error").is_none(), "{reply}");

        let (reply, _) = cdp_call(
            &mut client,
            17,
            "Page.captureScreenshot",
            json!({"format": "png"}),
            Some(session),
        );
        let data = reply["result"]["data"].as_str().expect("data");
        assert!(data.starts_with("iVBORw0KGgo"), "no es un PNG");

        client.write_all(&[0x88, 0x80, 0, 0, 0, 0]).unwrap();
        server.join().unwrap();
    }
}
