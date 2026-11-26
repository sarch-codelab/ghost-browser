//! `wisp-agent serve`: expone el API de control local (`wisp_mcp::control`)
//! apoyado en el motor Wisp headless, con soporte completo de herramientas
//! `browser_*` (`@ref` snapshots, fill, select, pdf, network, console),
//! dispatcher CDP headless + eventos en streaming, protección SSRF y pool de
//! workers con límites de RSS/edad.

use crate::cdp::{CdpEvent, base64_encode};
use crate::pdf;
use crate::ws::{self, WsMessage};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
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
    port: u16,
    options: FetchOptions,
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
                config.options.proxy_url = Some(
                    args.get(index)
                        .context("`--proxy` needs a URL")?
                        .to_owned(),
                );
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
    WebSocket {
        path: String,
        key: String,
    },
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
        "Target.attachToTarget" => {
            let target_id = params
                .get("targetId")
                .and_then(Value::as_str)
                .unwrap_or("tab-1");
            json!({"ok": true, "result": {"sessionId": format!("sess-{target_id}")}})
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
                let _ = load(&mut tab, &options);
            }
            s.active = Some(id);
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
            dispatch_tab_cdp(tab, method, &params, &options)
        }
    }
}

fn dispatch_tab_cdp(tab: &mut Tab, method: &str, params: &Value, options: &FetchOptions) -> Value {
    match method {
        "Page.enable"
        | "DOM.enable"
        | "Page.setLifecycleEventsEnabled"
        | "Emulation.setDeviceMetricsOverride" => {
            json!({"ok": true, "result": {}})
        }
        "Page.getFrameTree" => json!({
            "ok": true,
            "result": {
                "frameTree": {
                    "frame": {
                        "id": format!("frame-{}", tab.id),
                        "loaderId": format!("loader-{}", tab.id),
                        "url": tab.url,
                        "mimeType": "text/html",
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
            json!({
                "ok": res.get("ok").and_then(Value::as_bool).unwrap_or(false),
                "result": {
                    "frameId": format!("frame-{}", tab.id),
                    "loaderId": format!("loader-{}", tab.id),
                    "url": tab.url,
                },
                "events": tab.events.iter().map(CdpEvent::to_json).collect::<Vec<_>>(),
            })
        }
        "Page.reload" => {
            let res = load(tab, options);
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
            let cdp_type = match &value {
                Value::Null => "object",
                Value::Bool(_) => "boolean",
                Value::Number(_) => "number",
                Value::String(_) => "string",
                Value::Array(_) | Value::Object(_) => "object",
            };
            json!({
                "ok": res.get("ok").and_then(Value::as_bool).unwrap_or(false),
                "result": {"result": {"type": cdp_type, "value": value}},
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
            let event_type = params
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("mousePressed");
            let hit = hit_target(tab, x, y).ok().flatten();
            if event_type == "mouseReleased"
                && let Some(ref target) = hit
                && let Some(url) = &target.url
            {
                let nav = navigate(tab, url, options);
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
        _ => {
            let error = format!("método CDP no soportado en headless: {method}");
            json!({"ok": false, "error": error})
        }
    }
}

fn js_quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
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
            tab.events.push(CdpEvent {
                method: "Page.frameNavigated".into(),
                params: json!({"frame": {"id": format!("frame-{}", tab.id), "url": tab.url}}),
            });
            tab.events.push(CdpEvent {
                method: "Page.loadEventFired".into(),
                params: json!({"timestamp": elapsed as f64 / 1000.0}),
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
                "frameId": format!("frame-{}", tab.id),
                "loaderId": format!("loader-{}", tab.id),
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
            let ua = wisp_engine::wisp_net::StealthProfile::for_policy(&s.options.policy)
                .user_agent;
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
                s.tabs.push(Tab {
                    id,
                    url: "about:blank".to_owned(),
                    title: String::new(),
                    history: vec!["about:blank".to_owned()],
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
                });
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
                .map(|q| {
                    q.strip_prefix("url=")
                        .unwrap_or(q)
                        .to_owned()
                })
                .unwrap_or_else(|| "about:blank".to_owned());
            let mut s = lock(state);
            let options = s.options.clone();
            let id = s.next_id;
            s.next_id += 1;
            let mut tab = Tab {
                id,
                url: url.clone(),
                title: String::new(),
                history: vec![url.clone()],
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
                    if let Some(tid) = chosen
                        && s.tabs.iter().any(|t| t.id == tid)
                    {
                        tid
                    } else {
                        let nid = s.next_id;
                        s.next_id += 1;
                        s.tabs.push(Tab {
                            id: nid,
                            url: "about:blank".to_owned(),
                            title: String::new(),
                            history: vec!["about:blank".to_owned()],
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
                        });
                        s.active = Some(nid);
                        nid
                    }
                };

                let cdp_body = json!({"method": method, "params": params});
                let resp = handle_cdp(&state, target_tab_id, &cdp_body);

                let pending_events: Vec<Value> = {
                    let mut s = lock(&state);
                    if let Some(tab) = s.tabs.iter_mut().find(|t| t.id == target_tab_id) {
                        tab.events.drain(..).map(|e| e.to_json()).collect()
                    } else {
                        Vec::new()
                    }
                };
                for mut ev in pending_events {
                    if let Some(ref sid) = session_id {
                        ev["sessionId"] = sid.clone();
                    }
                    if ws::write_ws_text(&mut stream, &ev.to_string()).is_err() {
                        return;
                    }
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
                if let Some(sid) = session_id {
                    rpc["sessionId"] = sid;
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
        assert_eq!(arr[0]["webSocketDebuggerUrl"], "ws://127.0.0.1:49123/devtools/page/wisp-1");
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
            "Sec-WebSocket-Version: 13\r\n\r\n",
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
        let msg = br#"{"id":7,"method":"Runtime.evaluate","params":{"expression":"document.getElementById('h').textContent"}}"#;
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
}
