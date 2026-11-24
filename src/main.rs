// En release no abre ventana de consola: es una app de escritorio.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser;
mod cdp;
mod logo;
mod perf;
mod remote;
mod store;
mod ui;
mod webview;

use anyhow::Result;
use browser::Browser;
use egui_wgpu::winit::Painter;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Instant;
use store::{BrowserEngine, Store};
use webview::{TabEvent, content_bounds, create_webview};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{CursorIcon, ResizeDirection, Theme, Window, WindowId};

/// Constantes para Strings frecuentes para evitar allocations repetidos.
const NEWTAB_URL: &str = "ghost://newtab";

fn apply_viewport_commands(
    event_loop: &ActiveEventLoop,
    window: &Window,
    output: egui::ViewportOutput,
) {
    for command in output.commands {
        match command {
            egui::ViewportCommand::Close => event_loop.exit(),
            egui::ViewportCommand::Minimized(minimized) => window.set_minimized(minimized),
            egui::ViewportCommand::Maximized(maximized) => window.set_maximized(maximized),
            egui::ViewportCommand::Fullscreen(fullscreen) => window
                .set_fullscreen(fullscreen.then_some(winit::window::Fullscreen::Borderless(None))),
            egui::ViewportCommand::Title(title) => window.set_title(&title),
            egui::ViewportCommand::StartDrag => {
                if let Err(err) = window.drag_window() {
                    log::warn!("No se pudo arrastrar la ventana: {err}");
                }
            }
            _ => {}
        }
    }
}

/// Tamaño del área cliente en píxeles físicos leído del SO. En Windows,
/// `window.inner_size()` de winit puede reportar un valor duplicado tras un
/// cambio de DPI entre monitores (el backend multiplica dos veces); la
/// geometría real del webview y de la superficie egui debe salir de aquí.
#[cfg(windows)]
fn client_size_physical(window: &Window) -> Option<winit::dpi::PhysicalSize<u32>> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::Win32(win) = handle.as_raw() else {
        return None;
    };
    let hwnd = win.hwnd.get();
    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetClientRect(hwnd: isize, rect: *mut Rect) -> i32;
        fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
    }
    let mut rect = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { GetClientRect(hwnd, &mut rect) } == 0 {
        return None;
    }
    let mut win = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { GetWindowRect(hwnd, &mut win) } != 0 {
        // Durante un cambio de DPI entre monitores Windows puede dejar el
        // client rect duplicado (mayor que la propia ventana). Es imposible
        // físicamente: si pasa, el client está corrupto y manda la ventana.
        let client_w = rect.right - rect.left;
        let client_h = rect.bottom - rect.top;
        let win_w = win.right - win.left;
        let win_h = win.bottom - win.top;
        if client_w > win_w || client_h > win_h {
            rect.right = win_w;
            rect.bottom = win_h;
        }
    }
    Some(winit::dpi::PhysicalSize::new(
        (rect.right - rect.left).max(0) as u32,
        (rect.bottom - rect.top).max(0) as u32,
    ))
}

#[cfg(not(windows))]
fn client_size_physical(window: &Window) -> Option<winit::dpi::PhysicalSize<u32>> {
    let _ = window;
    None
}

/// Área de trabajo del monitor bajo la ventana en píxeles físicos
/// (left, top, right, bottom). winit 0.30 no expone `work_area`, así que en
/// Windows se consulta con GetMonitorInfoW; en otras plataformas se usa el
/// tamaño completo del monitor como aproximación.
#[cfg(windows)]
fn monitor_work_area(window: &Window) -> Option<(i32, i32, i32, i32)> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::Win32(win) = handle.as_raw() else {
        return None;
    };
    let hwnd = win.hwnd.get();
    #[repr(C)]
    #[derive(Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct MonitorInfo {
        size: u32,
        monitor: Rect,
        work: Rect,
        flags: u32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MonitorFromWindow(hwnd: isize, flags: u32) -> isize;
        fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
    }
    let monitor = unsafe { MonitorFromWindow(hwnd, 2) };
    let mut info = MonitorInfo {
        size: size_of::<MonitorInfo>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }
    Some((
        info.work.left,
        info.work.top,
        info.work.right,
        info.work.bottom,
    ))
}

#[cfg(not(windows))]
fn monitor_work_area(window: &Window) -> Option<(i32, i32, i32, i32)> {
    let monitor = window.current_monitor()?;
    let pos = monitor.position();
    let size = monitor.size();
    Some((
        pos.x,
        pos.y,
        pos.x + size.width as i32,
        pos.y + size.height as i32,
    ))
}

fn resize_direction(
    size: winit::dpi::PhysicalSize<u32>,
    position: PhysicalPosition<f64>,
    scale_factor: f64,
) -> Option<ResizeDirection> {
    let edge = webview::RESIZE_GUTTER * scale_factor;
    let left = position.x < edge;
    let right = position.x >= size.width as f64 - edge;
    // La banda superior es más fina: el chrome empieza a 6 px y así no roba
    // el primer tramo de la fila de pestañas ni del botón «+».
    let top_edge = 2.0 * scale_factor;
    let top = position.y < top_edge;
    let bottom = position.y >= size.height as f64 - edge;

    match (top, bottom, left, right) {
        (true, _, true, _) => Some(ResizeDirection::NorthWest),
        (true, _, _, true) => Some(ResizeDirection::NorthEast),
        (_, true, true, _) => Some(ResizeDirection::SouthWest),
        (_, true, _, true) => Some(ResizeDirection::SouthEast),
        (true, _, _, _) => Some(ResizeDirection::North),
        (_, true, _, _) => Some(ResizeDirection::South),
        (_, _, true, _) => Some(ResizeDirection::West),
        (_, _, _, true) => Some(ResizeDirection::East),
        _ => None,
    }
}

fn resize_cursor(direction: ResizeDirection) -> CursorIcon {
    match direction {
        ResizeDirection::North | ResizeDirection::South => CursorIcon::NsResize,
        ResizeDirection::East | ResizeDirection::West => CursorIcon::EwResize,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::NeswResize,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::NwseResize,
    }
}

/// Opciones de línea de comandos: URLs a abrir y overrides de sesión
/// pensados para benchmarks (`--no-saver`, `--no-session`).
struct CliOptions {
    urls: Vec<String>,
    engine: Option<BrowserEngine>,
    no_saver: bool,
    no_session: bool,
}

fn parse_cli() -> CliOptions {
    let mut options = CliOptions {
        urls: Vec::new(),
        engine: None,
        no_saver: false,
        no_session: false,
    };
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--wisp" => options.engine = Some(BrowserEngine::Wisp),
            "--webview2" | "--webview" => options.engine = Some(BrowserEngine::WebView2),
            "--no-saver" => options.no_saver = true,
            "--no-session" => options.no_session = true,
            _ if arg.starts_with("http://") || arg.starts_with("https://") => {
                options.urls.push(arg);
            }
            _ => {}
        }
    }
    options
}

struct App {
    window: Option<Arc<Window>>,
    browser: Browser,
    tx: Sender<TabEvent>,
    egui_state: Option<egui_winit::State>,
    painter: Option<Painter>,
    modifiers: egui::Modifiers,
    /// Último panel abierto, para ocultar la webview mientras hay un overlay.
    panel_state: browser::Panel,
    /// Pestaña que tiene el foco del teclado (para no robarlo al redimensionar).
    focused_tab: Option<u64>,
    /// Tema aplicado a la ventana y a sus webviews.
    title_dark: bool,
    cursor_position: Option<PhysicalPosition<f64>>,
    /// Alturas medidas del chrome y de la barra de estado en el último frame.
    chrome_metrics: (f32, f32),
    /// El primer frame pinta el splash antes de crear ningún WebView2. Así no
    /// aparece la superficie blanca inicial de WebView2 al arrancar.
    startup_webviews_pending: bool,
    /// La ventana nace oculta: se muestra recién cuando el splash ya está
    /// pintado, para que Windows no enseñe un rectángulo blanco sin dueño.
    window_hidden: bool,
    /// El cursor actual pertenece a una zona de redimensionado y hay que
    /// devolverlo a la normalidad al salir, no dejarlo pegado en toda la UI.
    resize_cursor_active: bool,
    /// Canal de la API de control remoto (localhost). `None` si el puerto
    /// estaba ocupado al arrancar.
    remote_rx: Option<std::sync::mpsc::Receiver<remote::RemoteCommand>>,
    /// Verificación periódica de bounds: al mover la ventana entre monitores
    /// con DPI distinto, Windows entrega WM_SIZE/WM_DPICHANGED en ráfaga y
    /// el client rect puede quedar duplicado durante la transición sin que
    /// llegue ningún eventocorrector; se re-verifica cada ~500 ms.
    bounds_verify_at: Instant,
    /// Firma del último content_rect aplicado a las webviews.
    applied_content_sig: Option<String>,
    /// Último monitor conocido para detectar cambios de monitor.
    last_monitor: Option<winit::monitor::MonitorHandle>,
}

impl App {
    fn new(event_proxy: winit::event_loop::EventLoopProxy<()>) -> Self {
        let cli = parse_cli();
        let mut store = Store::load();
        if cli.no_session {
            store.session.clear();
        }
        if cli.no_session || cli.no_saver {
            // Las corridas de benchmark son efímeras: ni la sesión vaciada ni
            // los ajustes de memoria alterados deben llegar a ghost.json.
            store.make_ephemeral();
        }
        let (mut browser, tx) = Browser::with_store(store);
        browser.set_event_proxy(event_proxy);
        browser.cli_engine = cli.engine;
        if cli.no_saver {
            // Solo en memoria: el benchmark compara el coste por pestaña sin
            // la suspensión LRU. La sesión se restaura al cerrar Ghost.
            browser.store.settings.memory_saver = false;
            browser.store.settings.max_live_tabs = usize::MAX;
        }
        for url in cli.urls {
            let url = webview::normalize_url(
                &url,
                &browser.store.settings.home_page,
                &browser.store.settings.search_engine,
            );
            browser.pending_new_tabs.push_back(Some(url));
        }
        Self {
            window: None,
            browser,
            tx,
            egui_state: None,
            painter: None,
            modifiers: egui::Modifiers::default(),
            panel_state: browser::Panel::None,
            focused_tab: None,
            title_dark: true,
            cursor_position: None,
            chrome_metrics: (browser::UI_HEIGHT, browser::STATUS_HEIGHT),
            startup_webviews_pending: true,
            window_hidden: true,
            resize_cursor_active: false,
            remote_rx: remote::start(),
            bounds_verify_at: Instant::now(),
            applied_content_sig: None,
            last_monitor: None,
        }
    }

    fn content_rect(&self) -> wry::Rect {
        let Some(window) = &self.window else {
            return wry::Rect::default();
        };
        content_bounds(
            client_size_physical(window).unwrap_or_else(|| window.inner_size()),
            window.scale_factor(),
            self.browser.chrome_height as f64,
            self.browser.status_height as f64,
            window.is_maximized(),
        )
    }

    /// Crea los webviews pendientes (pestañas nuevas y pestañas suspendidas).
    fn materialize_webviews(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let rect = self.content_rect();

        let pending: Vec<Option<String>> = self.browser.drain_pending_tabs();
        for url in pending {
            let url = url.unwrap_or_else(|| self.browser.store.settings.home_page.clone());
            let id = self.browser.next_tab_id;
            self.browser.next_tab_id += 1;
            let mut engine = self
                .browser
                .cli_engine
                .unwrap_or(self.browser.store.settings.default_engine);
            if self.browser.cli_engine.is_none()
                && self.browser.store.settings.auto_engine
                && wisp_engine::url_suits_wisp(&url)
            {
                engine = BrowserEngine::Wisp;
            }
            // La nueva pestaña es nativa (egui): no se crea ni WebView2 ni
            // carga Wisp. Chromium queda perezoso hasta una URL real.
            if webview::is_newtab_url(&url) {
                self.browser.add_tab(browser::Tab {
                    id,
                    engine,
                    webview: None,
                    wisp_html: None,
                    wisp_layout: None,
                    wisp_layout_width: None,
                    wisp_error: None,
                    wisp_reader: None,
                    engine_manual: false,
                    title: String::new(),
                    url: NEWTAB_URL.to_string(),
                    loading: false,
                    zoom: self.browser.store.settings.default_zoom.clamp(0.25, 5.0),
                    navigation_history: vec![NEWTAB_URL.to_string()],
                    history_index: 0,
                    last_active: Instant::now(),
                    pending_url: None,
                    ephemeral: false,
                });
                continue;
            }
            if engine == BrowserEngine::Wisp {
                self.browser.add_tab(browser::Tab {
                    id,
                    engine,
                    webview: None,
                    wisp_html: None,
                    wisp_layout: None,
                    wisp_layout_width: None,
                    wisp_error: None,
                    wisp_reader: None,
                    engine_manual: false,
                    title: String::new(),
                    url: url.clone(),
                    loading: true,
                    zoom: self.browser.store.settings.default_zoom.clamp(0.25, 5.0),
                    navigation_history: vec![url.clone()],
                    history_index: 0,
                    last_active: Instant::now(),
                    pending_url: None,
                    ephemeral: false,
                });
                self.browser.start_wisp_load(id, url);
                continue;
            }
            match create_webview(
                window.as_ref(),
                rect,
                &url,
                id,
                !self.browser.splash,
                &self.browser.store.settings,
                self.browser.block_ads.clone(),
                self.tx.clone(),
                self.browser.download_dir.clone(),
            ) {
                Ok(webview) => self.browser.add_tab(browser::Tab {
                    id,
                    engine,
                    webview: Some(webview),
                    wisp_html: None,
                    wisp_layout: None,
                    wisp_layout_width: None,
                    wisp_error: None,
                    wisp_reader: None,
                    engine_manual: false,
                    title: String::new(),
                    url: url.clone(),
                    loading: true,
                    zoom: self.browser.store.settings.default_zoom.clamp(0.25, 5.0),
                    navigation_history: vec![url],
                    history_index: 0,
                    last_active: Instant::now(),
                    pending_url: None,
                    ephemeral: false,
                }),
                Err(e) => log::error!("No se pudo crear el webview: {e}"),
            }
        }

        for id in self.browser.drain_pending_restore() {
            let Some(index) = self.browser.tab_index_of(id) else {
                continue;
            };
            let active = self.browser.active;
            let visible = index == active && !self.browser.splash;
            let (url, target, zoom, engine) = {
                let Some(t) = self.browser.tabs.get(index) else {
                    continue;
                };
                (t.url.clone(), t.pending_url.clone(), t.zoom, t.engine)
            };
            let target = target.unwrap_or(url);
            // Defensa: una pestaña nueva nunca debe materializar un webview.
            if webview::is_newtab_url(&target) {
                if let Some(tab) = self.browser.tab_by_id_mut(id) {
                    tab.url = NEWTAB_URL.to_string();
                    tab.pending_url = None;
                    tab.loading = false;
                }
                self.browser.cancel_restore(id);
                continue;
            }
            if engine == BrowserEngine::Wisp {
                if let Some(tab) = self.browser.tab_by_id_mut(id) {
                    tab.pending_url = None;
                }
                self.browser.start_wisp_load(id, target);
                continue;
            }
            match create_webview(
                window.as_ref(),
                rect,
                &target,
                id,
                visible,
                &self.browser.store.settings,
                self.browser.block_ads.clone(),
                self.tx.clone(),
                self.browser.download_dir.clone(),
            ) {
                Ok(webview) => {
                    if let Some(elapsed) = self.browser.restore_elapsed(id) {
                        perf::mark_restore_phase(id, "webview_created", elapsed);
                    }
                    // El nivel de zoom se conserva al restaurar la pestaña.
                    let _ = webview.zoom(zoom);
                    if let Some(t) = self.browser.tab_by_id_mut(id) {
                        t.webview = Some(webview);
                        t.wisp_html = None;
                        t.wisp_layout = None;
                        t.wisp_layout_width = None;
                        t.wisp_error = None;
                        t.pending_url = None;
                        t.loading = true;
                        t.url = target;
                        t.last_active = Instant::now();
                    }
                    self.browser.needs_bounds_sync = true;
                }
                Err(e) => {
                    log::error!("No se pudo restaurar la pestaña {id}: {e}");
                    self.browser.fail_restore(id);
                }
            }
        }
    }

    /// Resincroniza posición/tamaño de todos los webviews, su visibilidad y el
    /// nivel de memoria (las pestañas de fondo pasan a consumo bajo).
    fn sync_webview_bounds(&mut self) {
        if !self.browser.needs_bounds_sync {
            return;
        }
        let rect = self.content_rect();
        self.applied_content_sig = Some(format!("{:?}", rect));
        let active = self.browser.active;
        let panel_open = self.browser.panel != browser::Panel::None;
        let active_id = self.browser.active_tab().map(|t| t.id);
        for (i, tab) in self.browser.tabs.iter().enumerate() {
            let Some(webview) = &tab.webview else {
                continue;
            };
            let _ = webview.set_bounds(rect);
            let _ = webview.set_visible(
                i == active
                    && !panel_open
                    && !self.browser.splash
                    && !self.browser.is_restoring(tab.id),
            );
            // La pestaña activa SIEMPRE queda en memoria normal: WebView2 en
            // nivel Low estrangula timers/rAF y páginas con lazy-load (YouTube,
            // SPAs) se quedan "cargando" para siempre. El ahorro real está en
            // las pestañas de fondo; la activa ya se autogestiona al ocultarse.
            webview::set_low_memory(webview, i != active);
        }
        // Al cambiar de pestaña el foco del teclado debe seguir a la nueva
        // (si no, se escribe en la pestaña que quedó oculta).
        if !panel_open {
            if let Some(id) = active_id
                && self.focused_tab != Some(id)
            {
                if let Some(wv) = self.browser.active_tab().and_then(|t| t.webview.as_ref()) {
                    let _ = wv.focus();
                }
                self.focused_tab = Some(id);
            }
        } else {
            self.focused_tab = None;
        }
        self.browser.needs_bounds_sync = false;
    }

    fn egui_ctx(&self) -> Option<&egui::Context> {
        self.egui_state.as_ref().map(|s| s.egui_ctx())
    }

    fn handle_shortcut(&mut self, key: &Key) {
        let key_name = match key {
            Key::Named(named) => named_key_name(named).to_string(),
            Key::Character(s) => s.to_lowercase(),
            _ => return,
        };
        if ui::key_is_f5(key) {
            if let Some(t) = self.browser.active_tab() {
                let id = t.id;
                self.browser.apply_shortcut(id, "reload");
            }
            return;
        }
        let (ctrl, alt, shift) = (
            self.modifiers.ctrl,
            self.modifiers.alt,
            self.modifiers.shift,
        );
        if let Some(action) = ui::shortcut_from_key(&key_name, ctrl, alt, shift) {
            let id = self.browser.active_tab().map(|t| t.id).unwrap_or(0);
            self.browser.apply_shortcut(id, action);
        }
    }
}

fn named_key_name(key: &NamedKey) -> &'static str {
    use NamedKey::*;
    match key {
        ArrowLeft => "ArrowLeft",
        ArrowRight => "ArrowRight",
        ArrowUp => "ArrowUp",
        ArrowDown => "ArrowDown",
        Tab => "Tab",
        Enter => "Enter",
        Escape => "Escape",
        F5 => "F5",
        F11 => "F11",
        F12 => "F12",
        _ => "",
    }
}

/// Devuelve el foco del teclado a la ventana (y no a la webview hija).
/// Hace falta al pulsar Ctrl+L o al hacer clic en el marco desde una página.
#[cfg(windows)]
fn focus_keyboard(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetFocus(hwnd: isize) -> isize;
    }

    if let Ok(handle) = window.window_handle()
        && let RawWindowHandle::Win32(win32) = handle.as_raw()
    {
        unsafe {
            SetFocus(win32.hwnd.get());
        }
    }
}

#[cfg(not(windows))]
fn focus_keyboard(_window: &Window) {}

/// Winit mantiene `WS_CAPTION` en las ventanas de nivel superior aunque no
/// tengan decoraciones, y DWM reserva la altura del título: el contenido de
/// egui/webview aparece desplazado ~29 px hacia abajo. Quitarlo y refrescar el
/// marco deja el cliente ocupando toda la ventana.
#[cfg(windows)]
fn remove_hidden_caption(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetWindowPos(
            hwnd: isize,
            after: isize,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
        fn GetWindowLongW(hwnd: isize, index: i32) -> i32;
        fn SetWindowLongW(hwnd: isize, index: i32, value: i32) -> i32;
    }

    const GWL_STYLE: i32 = -16;
    // WS_CAPTION = WS_BORDER | WS_DLGFRAME: sin quitarlo, DWM reserva la
    // altura del título aunque la ventana no dibuje decoración.
    const WS_CAPTION: u32 = 0x00C0_0000;

    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_FRAMECHANGED: u32 = 0x0020;

    if let Ok(handle) = window.window_handle()
        && let RawWindowHandle::Win32(win32) = handle.as_raw()
    {
        let hwnd = win32.hwnd.get();
        // Subclase propia: corrige el área cliente al maximizar para que
        // la ventana cubra el área de trabajo del monitor sin huecos.
        install_frame_subclass(hwnd);
        unsafe {
            let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
            if style & WS_CAPTION == 0 {
                return;
            }
            SetWindowLongW(hwnd, GWL_STYLE, (style & !WS_CAPTION) as i32);
            SetWindowPos(
                hwnd,
                0,
                0,
                0,
                0,
                0,
                SWP_NOSIZE | SWP_NOMOVE | SWP_NOZORDER | SWP_FRAMECHANGED,
            );
        }
    }
}

/// Subclase del HWND que ajusta `WM_NCCALCSIZE` y `WM_GETMINMAXINFO`.
///
/// Las ventanas sin marco de winit conservan `WS_THICKFRAME` (borde de
/// redimado invisible + sombra). Windows maximiza el *rectángulo de ventana*
/// incluyendo ese marco invisible, así que el área cliente queda desplazada
/// o con huecos — visible sobre todo con la barra de tareas auto-oculta o en
/// monitores secundarios. La subclase fuerza el cliente al `rcWork` del
/// monitor cuando está maximizada. Todo lo demás se reenvía intacto.
#[cfg(windows)]
fn install_frame_subclass(hwnd: isize) {
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

    static INSTALLED: AtomicBool = AtomicBool::new(false);
    static ORIGINAL_WNDPROC: AtomicIsize = AtomicIsize::new(0);

    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetWindowLongPtrW(hwnd: isize, index: isize, value: isize) -> isize;
        fn CallWindowProcW(
            prev: isize,
            hwnd: isize,
            msg: u32,
            wparam: usize,
            lparam: isize,
        ) -> isize;
        fn IsZoomed(hwnd: isize) -> i32;
        fn MonitorFromWindow(hwnd: isize, flags: u32) -> isize;
        fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
    }

    const GWLP_WNDPROC: isize = -4;
    const MONITOR_DEFAULTTONEAREST: u32 = 2;
    const WM_NCCALCSIZE: u32 = 0x0083;
    const WM_GETMINMAXINFO: u32 = 0x0024;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[repr(C)]
    struct NcCalcSizeParams {
        rects: [Rect; 3],
        pos: *mut Point,
    }
    #[repr(C)]
    struct MinMaxInfo {
        pt_reserved: Point,
        pt_max_size: Point,
        pt_max_position: Point,
        pt_min_track_size: Point,
        pt_max_track_size: Point,
        rc_max_position: Rect,
        rc_min_track_size: Rect,
    }
    #[repr(C)]
    #[derive(Default)]
    struct MonitorInfo {
        size: u32,
        monitor: Rect,
        work: Rect,
        flags: u32,
    }

    unsafe extern "system" fn frame_wndproc(
        hwnd: isize,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        unsafe {
            let original = ORIGINAL_WNDPROC.load(Ordering::SeqCst);
            match msg {
                WM_NCCALCSIZE if wparam != 0 => {
                    let result = CallWindowProcW(original, hwnd, msg, wparam, lparam);
                    if IsZoomed(hwnd) != 0 {
                        let mut info = MonitorInfo {
                            size: size_of::<MonitorInfo>() as u32,
                            ..Default::default()
                        };
                        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                        if GetMonitorInfoW(monitor, &mut info) != 0 {
                            let params = &mut *(lparam as *mut NcCalcSizeParams);
                            params.rects[0] = info.work;
                        }
                    }
                    result
                }
                WM_GETMINMAXINFO => {
                    // SIN gate de IsZoomed: este mensaje llega ANTES de
                    // maximizar, precisamente para definir el tamaño máximo.
                    // Sus campos ptMax* solo importan al hacer zoom, así que
                    // fijarlos siempre al área de trabajo es lo correcto.
                    let result = CallWindowProcW(original, hwnd, msg, wparam, lparam);
                    {
                        let mut info = MonitorInfo {
                            size: size_of::<MonitorInfo>() as u32,
                            ..Default::default()
                        };
                        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                        if GetMonitorInfoW(monitor, &mut info) != 0 {
                            let mmi = &mut *(lparam as *mut MinMaxInfo);
                            // ptMaxPosition es RELATIVO al origen del
                            // monitor (Windows suma monitor.left/top): si se
                            // pasa la coordenada absoluta, la ventana
                            // maximizada salta a monitor.left*2 — el bug de
                            // "Ghost desaparece al maximizar" en DISPLAY2.
                            mmi.pt_max_position = Point {
                                x: info.work.left - info.monitor.left,
                                y: info.work.top - info.monitor.top,
                            };
                            mmi.pt_max_size = Point {
                                x: info.work.right - info.work.left,
                                y: info.work.bottom - info.work.top,
                            };
                        }
                    }
                    result
                }
                _ => CallWindowProcW(original, hwnd, msg, wparam, lparam),
            }
        }
    }

    unsafe {
        let original = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, frame_wndproc as *const () as isize);
        if original != 0 {
            ORIGINAL_WNDPROC.store(original, Ordering::SeqCst);
        } else {
            // Fallo al subclassar: restaurar el puntero anterior (0) no es
            // posible de forma fiable, pero el flag evita reintentos.
            INSTALLED.store(false, Ordering::SeqCst);
        }
    }
}

#[cfg(not(windows))]
fn remove_hidden_caption(_window: &Window) {}

/// Da a la ventana el borde y las esquinas redondeadas de Windows 11.
#[cfg(windows)]
fn style_window_frame(window: &Window, dark: bool) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: isize,
            attribute: u32,
            value: *const core::ffi::c_void,
            size: u32,
        ) -> i32;
    }

    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    let hwnd = win32.hwnd.get();
    // DWMWA_USE_IMMERSIVE_DARK_MODE = 20
    let dark_mode: i32 = if dark { 1 } else { 0 };
    // DWMWA_WINDOW_CORNER_PREFERENCE = 33: redondeadas en ventana normal,
    // cuadradas al maximizar — forzar ROUND maximizada dibuja artefactos en
    // los bordes de la pantalla (el "marco raro" reportado por usuarios).
    let corner: i32 = if window.is_maximized() { 1 } else { 2 };
    // DWMWA_BORDER_COLOR = 34 (COLORREF 0x00BBGGRR)
    let border: u32 = if dark { 0x0000_0000 } else { 0x00FF_FFFF };
    unsafe {
        DwmSetWindowAttribute(hwnd, 20, &dark_mode as *const i32 as *const _, 4);
        DwmSetWindowAttribute(hwnd, 33, &corner as *const i32 as *const _, 4);
        DwmSetWindowAttribute(hwnd, 34, &border as *const u32 as *const _, 4);
    }
}

#[cfg(not(windows))]
fn style_window_frame(_window: &Window, _dark: bool) {}

/// Pinta el fondo de la clase con el color del tema. Evita el destello blanco
/// de Windows antes de que egui dibuje el primer frame del splash.
#[cfg(windows)]
fn set_window_background(window: &Window, dark: bool) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetClassLongPtrW(hwnd: isize, index: i32, value: isize) -> isize;
        fn CreateSolidBrush(color: u32) -> isize;
    }

    const GCLP_HBRBACKGROUND: i32 = -10;
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    let hwnd = win32.hwnd.get();
    let color = if dark { 0x000E_0E0E } else { 0x00F5_F5F5 };
    unsafe {
        let brush = CreateSolidBrush(color);
        SetClassLongPtrW(hwnd, GCLP_HBRBACKGROUND, brush);
    }
}

#[cfg(not(windows))]
fn set_window_background(_window: &Window, _dark: bool) {}

impl ApplicationHandler for App {
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let dark = self.browser.store.settings.dark_mode;
        let attrs = Window::default_attributes()
            .with_title("Ghost")
            .with_inner_size(LogicalSize::new(1200.0, 760.0))
            .with_min_inner_size(LogicalSize::new(320.0, 240.0))
            .with_decorations(false)
            .with_visible(false)
            .with_window_icon(logo::window_icon())
            .with_theme(Some(if dark { Theme::Dark } else { Theme::Light }));
        #[cfg(windows)]
        let attrs = {
            use winit::platform::windows::WindowAttributesExtWindows;
            // Deja que las webviews hijas se dibujen sobre la ventana sin
            // parpadeos al redimensionar. La sombra nativa necesita el marco
            // sin título que se quita en `remove_hidden_caption`.
            attrs
                .with_undecorated_shadow(true)
                .with_clip_children(false)
        };
        let window = Arc::new(event_loop.create_window(attrs).expect("crear ventana"));
        perf::mark("startup:window_created");
        self.title_dark = dark;

        // Ajusta el tamaño inicial al espacio real de la pantalla (respetando el
        // escalado DPI, para que nunca quede una ventana más grande que el monitor).
        if let Some(monitor) = window.current_monitor() {
            let scale = window.scale_factor().max(0.5);
            let size = monitor.size();
            let usable_w = (size.width as f64 / scale) * 0.88;
            let usable_h = (size.height as f64 / scale) * 0.88;
            let min_w = usable_w.clamp(1.0, 320.0);
            let min_h = usable_h.clamp(1.0, 240.0);
            window.set_min_inner_size(Some(LogicalSize::new(min_w, min_h)));
            let current = client_size_physical(&window).unwrap_or_else(|| window.inner_size());
            let cur_w = current.width as f64 / scale;
            let cur_h = current.height as f64 / scale;
            let want_w = cur_w.min(usable_w).max(min_w);
            let want_h = cur_h.min(usable_h).max(min_h);
            if (want_w - cur_w).abs() > 2.0 || (want_h - cur_h).abs() > 2.0 {
                let _ = window.request_inner_size(LogicalSize::new(want_w, want_h));
            }
        }

        // Centra la ventana en el monitor para que nunca quede parte de ella
        // detrás de la barra de tareas o fuera de la pantalla.
        if let Some(monitor) = window.current_monitor() {
            let monitor_pos = monitor.position();
            let monitor_size = monitor.size();
            let win_size = window.outer_size();
            let x =
                monitor_pos.x + ((monitor_size.width as i32 - win_size.width as i32) / 2).max(0);
            let y =
                monitor_pos.y + ((monitor_size.height as i32 - win_size.height as i32) / 2).max(0);
            window.set_outer_position(PhysicalPosition::new(x, y));
        }
        self.last_monitor = window.current_monitor();

        remove_hidden_caption(&window);
        style_window_frame(&window, dark);
        set_window_background(&window, dark);

        let ctx = egui::Context::default();
        register_wisp_fonts(&ctx);
        let egui_state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            None,
            window.theme(),
            None,
        );

        let painter = pollster::block_on(Painter::new(
            ctx,
            egui_wgpu::WgpuConfiguration::default(),
            false,
            egui_wgpu::RendererOptions::default(),
        ));
        let mut painter = painter;
        pollster::block_on(painter.set_window(egui::ViewportId::ROOT, Some(window.clone())))
            .expect("inicializar superficie wgpu");
        perf::mark("startup:surface_ready");

        self.window = Some(window);
        self.egui_state = Some(egui_state);
        self.painter = Some(painter);
        // El primer frame debe pintar el logo; los WebView2 se crean después.
        // Browser::new ya deja una pestaña de inicio en la lista.
        self.browser.begin_splash();
        self.startup_webviews_pending = true;
        if let Some(w) = &self.window {
            w.request_redraw();
        }
        // Pinta el primer frame (el splash) mientras la ventana sigue oculta.
        // Las ventanas ocultas no reciben RedrawRequested, así que no basta
        // con pedir un redraw: hay que renderizar una vez a mano y mostrarla
        // cuando la superficie ya tiene el logo.
        self.render(event_loop);
        perf::mark("startup:first_frame_hidden");
        // Con la ventana ya visible, pinta el splash una vez más antes de
        // crear los WebView2: esa creación puede tardar y bloquear el hilo.
        // Así el logo queda en pantalla durante la espera, no un fondo vacío.
        self.render(event_loop);
        perf::mark("startup:splash_visible");
        self.startup_webviews_pending = false;
        if self.browser.tabs.is_empty() {
            self.browser.request_new_tab();
        }
        self.materialize_webviews();
        perf::mark("startup:home_webview_created");
        self.sync_webview_bounds();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.clone() else {
            return;
        };

        // 1) Eventos de teclado propios antes de egui (atajos globales de la UI).
        match &event {
            WindowEvent::KeyboardInput {
                event: key_event, ..
            } if key_event.state == winit::event::ElementState::Pressed => {
                self.handle_shortcut(&key_event.logical_key);
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = egui::Modifiers {
                    alt: m.state().alt_key(),
                    ctrl: m.state().control_key(),
                    shift: m.state().shift_key(),
                    mac_cmd: false,
                    command: m.state().control_key(),
                };
            }
            _ => {}
        }

        // 2) Eventos hacia egui-winit.
        if let Some(state) = &mut self.egui_state {
            let r = state.on_window_event(&window, &event);
            if r.repaint {
                window.request_redraw();
            }
        }

        // 3) Eventos de ventana con efecto en wgpu/webviews.
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_position = Some(position);
                if !window.is_maximized() {
                    if let Some(direction) = resize_direction(
                        client_size_physical(&window).unwrap_or_else(|| window.inner_size()),
                        position,
                        window.scale_factor(),
                    ) {
                        window.set_cursor(resize_cursor(direction));
                        self.resize_cursor_active = true;
                    } else if self.resize_cursor_active {
                        // Al salir del borde se vuelve al cursor normal; si no,
                        // se queda en modo redimensionar sobre las pestañas.
                        window.set_cursor(CursorIcon::Default);
                        self.resize_cursor_active = false;
                    }
                } else if self.resize_cursor_active {
                    window.set_cursor(CursorIcon::Default);
                    self.resize_cursor_active = false;
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor_position = None;
                self.resize_cursor_active = false;
                window.set_cursor(CursorIcon::Default);
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if !window.is_maximized() => {
                if let Some(position) = self.cursor_position
                    && let Some(direction) = resize_direction(
                        client_size_physical(&window).unwrap_or_else(|| window.inner_size()),
                        position,
                        window.scale_factor(),
                    )
                {
                    let _ = window.drag_resize_window(direction);
                }
            }
            WindowEvent::Resized(reported) => {
                // winit puede duplicar el tamaño físico tras un cambio de
                // DPI entre monitores; el tamaño real siempre es GetClientRect.
                let size = client_size_physical(&window).unwrap_or(reported);
                if let Some(p) = &mut self.painter
                    && let (Some(w), Some(h)) = (
                        std::num::NonZeroU32::new(size.width),
                        std::num::NonZeroU32::new(size.height),
                    )
                {
                    p.on_window_resized(egui::ViewportId::ROOT, w, h);
                }
                // Al maximizar/restaurar winit vuelve a aplicar los estilos
                // nativos; hay que quitar otra vez el título oculto y
                // reevalorar las esquinas DWM (redondas ↔ cuadradas).
                remove_hidden_caption(&window);
                style_window_frame(&window, self.title_dark);
                self.browser.needs_bounds_sync = true;
                window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                // Al cruzar a otro monitor/DPI Windows puede restaurar el
                // marco nativo: hay que quitar el caption otra vez y
                // reevalorar esquinas/borde DWM, no solo en Resized.
                remove_hidden_caption(&window);
                style_window_frame(&window, self.title_dark);
                self.browser.needs_bounds_sync = true;
                window.request_redraw();
            }
            WindowEvent::Occluded(_) => {}
            WindowEvent::CloseRequested => {
                // Guarda las pestañas abiertas y el estado antes de salir.
                self.browser.capture_session();
                self.browser.store.flush();
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => self.render(event_loop),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Comandos de la API remota (localhost): navegación, eval, etc.
        if let Some(rx) = &self.remote_rx {
            while let Ok(command) = rx.try_recv() {
                self.browser.handle_remote_command(command);
            }
        }
        // Drena eventos de los webviews y materializa pestañas pendientes.
        let changed = self.browser.drain_events();
        let had_pending =
            !self.browser.pending_new_tabs.is_empty() || !self.browser.pending_restore.is_empty();
        if had_pending && !self.startup_webviews_pending {
            self.materialize_webviews();
        }
        let restores_timed_out = self.browser.tick_restores(Instant::now());
        // Verificación periódica de bounds: si el content_rect esperado ya
        // no coincide con el último aplicado (p. ej. el client rect se
        // asentó tarde tras un cambio de DPI), se fuerza un re-sync.
        if Instant::now() >= self.bounds_verify_at {
            self.bounds_verify_at = Instant::now() + std::time::Duration::from_millis(500);
            if let Some(window) = &self.window {
                let sig = format!("{:?}", self.content_rect());
                if self.applied_content_sig.as_deref() != Some(sig.as_str()) {
                    self.browser.needs_bounds_sync = true;
                }
                // Si la ventana quedó maximizada con el work area de OTRO
                // monitor (p. ej. se maximiza a mitad del cruce de DPI y
                // MonitorFromWindow aún reporta el monitor viejo), Windows
                // no siempre lo corrige solo: se re-maximiza aquí.
                // Se compara el rect EXTERIOR con umbral amplio: Windows da a
                // toda ventana redimensionable maximizada un overhang
                // invisible de ~8 px por lado (~16 px en tamaño), así que un
                // umbral de 8 px entraría en loop de re-maximize; la
                // corrupción real son cientos de px. No vale comparar el
                // client area: nuestro WM_NCCALCSIZE siempre lo fuerza al work
                // area y enmascararía el estado corrupto.
                if window.is_maximized()
                    && let (Some((wl, wt, wr, wb)), Ok(pos)) =
                        (monitor_work_area(window), window.outer_position())
                {
                    let size = window.outer_size();
                    let dx = (pos.x - wl).abs();
                    let dy = (pos.y - wt).abs();
                    let dw = (size.width as i32 - (wr - wl)).abs();
                    let dh = (size.height as i32 - (wb - wt)).abs();
                    if dx.max(dy).max(dw).max(dh) > 32 {
                        window.set_maximized(false);
                        window.set_maximized(true);
                    }
                }
            }
        }
        // Detectar cambio de monitor y recalcular tamaño/posición.
        if let Some(window) = &self.window
            && let Some(current_monitor) = window.current_monitor()
        {
            let monitor_changed = !self
                .last_monitor
                .as_ref()
                .map(|m| m.name() == current_monitor.name())
                .unwrap_or(false);
            if monitor_changed {
                self.last_monitor = Some(current_monitor.clone());
                let scale = window.scale_factor().max(0.5);
                let size = current_monitor.size();
                let usable_w = (size.width as f64 / scale) * 0.88;
                let usable_h = (size.height as f64 / scale) * 0.88;
                let min_w = usable_w.clamp(1.0, 320.0);
                let min_h = usable_h.clamp(1.0, 240.0);
                window.set_min_inner_size(Some(LogicalSize::new(min_w, min_h)));

                let current = client_size_physical(window).unwrap_or_else(|| window.inner_size());
                let cur_w = current.width as f64 / scale;
                let cur_h = current.height as f64 / scale;
                if cur_w > usable_w || cur_h > usable_h {
                    // `clamp` entra en pánico si el área usable es menor que el
                    // mínimo; aquí basta con no pasar del área usable.
                    let want_w = cur_w.min(usable_w).max(1.0);
                    let want_h = cur_h.min(usable_h).max(1.0);
                    let _ = window.request_inner_size(LogicalSize::new(want_w, want_h));
                }
                // No forzar re-centrado: el usuario puede haber movido la ventana a propósito.
                // Solo sincronizar bounds de las webviews.
                self.browser.needs_bounds_sync = true;
                window.request_redraw();
            }
        }
        // Sondas SPA: detectan páginas que quedaron en esqueleto y recargan.
        self.browser.tick_spa_probes(Instant::now());
        // Al terminar el splash hay que hacer visible la pestaña de inicio.
        let splash_done = self.browser.tick_splash(Instant::now());
        if !self.startup_webviews_pending {
            self.sync_webview_bounds();
        }
        self.browser.store.flush_if_due();

        // Tema claro/oscuro: actualiza la ventana y sus webviews.
        if let Some(window) = self.window.clone() {
            let dark = self.browser.store.settings.dark_mode;
            if dark != self.title_dark {
                self.title_dark = dark;
                window.set_theme(Some(if dark { Theme::Dark } else { Theme::Light }));
                style_window_frame(&window, dark);
                // Las páginas abiertas (incluida la de nueva pestaña) siguen el tema.
                for tab in &self.browser.tabs {
                    if let Some(wv) = &tab.webview {
                        webview::set_dark_theme(wv, dark);
                    }
                }
            }
        }

        // Ahorro de memoria: suspende pestañas inactivas.
        let suspended = self.browser.maintain(Instant::now());
        let dirty = self.browser.is_dirty();
        self.browser.clear_dirty();

        // Reportar uso de memoria periódicamente si está habilitado
        if suspended || dirty {
            perf::report_memory_usage();
        }

        if let Some(window) = &self.window {
            let egui_wants = self
                .egui_ctx()
                .map(|c| c.has_requested_repaint())
                .unwrap_or(false);
            if changed
                || suspended
                || egui_wants
                || dirty
                || had_pending
                || splash_done
                || restores_timed_out
            {
                window.request_redraw();
            }
        }

        // Duerme hasta la próxima suspensión (o hasta que haya eventos).
        // La verificación periódica de bounds también es un deadline.
        let verify_delay = self
            .bounds_verify_at
            .saturating_duration_since(Instant::now());
        match (self.browser.next_wakeup(), verify_delay) {
            (Some(a), b) => {
                event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + a.min(b)))
            }
            (None, b) => event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + b)),
        }
    }
}

impl App {
    fn render(&mut self, event_loop: &ActiveEventLoop) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(state) = &mut self.egui_state else {
            return;
        };
        let Some(painter) = &mut self.painter else {
            return;
        };

        let raw_input = state.take_egui_input(&window);
        let ctx = state.egui_ctx().clone();
        self.browser.maximized = window.is_maximized();
        ctx.begin_pass(raw_input);
        let close_requested = ui::draw(&ctx, &mut self.browser);
        // Clic en el marco (pestañas, barra, paneles) o Ctrl+L: el teclado debe
        // volver a la ventana; si no, se sigue escribiendo en la webview.
        let clicked_chrome = ctx.input(|i| i.pointer.any_pressed());
        if self.browser.focus_window_request || clicked_chrome {
            self.browser.focus_window_request = false;
            if clicked_chrome {
                self.focused_tab = None;
            }
            window.focus_window();
            focus_keyboard(&window);
        }
        if self.browser.panel != self.panel_state {
            self.panel_state = self.browser.panel;
            self.browser.needs_bounds_sync = true;
        }
        // Si el chrome cambió de alto (otro DPI, otro tamaño de ventana), la
        // webview se recoloca para seguir empezando justo debajo de la barra.
        let metrics = (self.browser.chrome_height, self.browser.status_height);
        if metrics != self.chrome_metrics {
            self.chrome_metrics = metrics;
            self.browser.needs_bounds_sync = true;
        }
        let mut full_output = ctx.end_pass();
        // Si el modo oscuro cambió durante el draw (desde el panel de ajustes),
        // forzar repaint inmediato para que la nueva pestaña y la UI reflejen el tema.
        if self.browser.dark_mode_changed {
            self.browser.dark_mode_changed = false;
            ctx.request_repaint();
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        if let Some(output) = full_output.viewport_output.remove(&egui::ViewportId::ROOT) {
            apply_viewport_commands(event_loop, &window, output);
        }
        state.handle_platform_output_with_event_loop(
            &window,
            event_loop,
            full_output.platform_output,
        );

        let ppp = full_output.pixels_per_point;
        let clipped = ctx.tessellate(full_output.shapes, ppp);
        let mut textures_delta = full_output.textures_delta;
        let clear = if self.browser.store.settings.dark_mode {
            [0.0, 0.0, 0.0, 1.0]
        } else {
            [1.0, 1.0, 1.0, 1.0]
        };
        painter.paint_and_update_textures(
            egui::ViewportId::ROOT,
            ppp,
            clear,
            &clipped,
            &mut textures_delta,
            Vec::new(),
            &window,
        );

        // El splash ya está pintado en la superficie: recién ahora se muestra
        // la ventana, sin pasar por el rectángulo blanco de inicialización.
        if self.window_hidden {
            self.window_hidden = false;
            self.browser.begin_splash();
            window.set_visible(true);
            perf::mark("startup:window_visible");
            window.focus_window();
            window.request_redraw();
        }

        if close_requested {
            self.browser.capture_session();
            self.browser.store.flush();
            event_loop.exit();
            return;
        }

        if ctx.has_requested_repaint() || self.browser.is_dirty() {
            window.request_redraw();
        }
    }
}

/// Registra la familia `wisp-bold` para el `font-weight` de Wisp con la misma
/// Ubuntu-Bold.ttf que Wisp usa para medir: misma fuente al medir y al pintar
/// = saltos de línea correctos en negritas.
fn register_wisp_fonts(ctx: &egui::Context) {
    let bytes: &'static [u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/wisp/assets/fonts/Ubuntu-Bold.ttf"
    ));
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "wisp-bold".to_string(),
        std::sync::Arc::new(egui::FontData::from_static(bytes)),
    );
    fonts
        .families
        .entry(egui::FontFamily::Name("wisp-bold".into()))
        .or_default()
        .push("wisp-bold".to_string());
    ctx.set_fonts(fonts);
}

fn main() -> Result<()> {
    env_logger::init();
    perf::start();
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new(event_loop.create_proxy());
    perf::mark("startup:app_new");
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod window_frame_tests {
    use super::*;

    #[test]
    fn resize_hit_test_respects_dpi_and_corner_edges() {
        let size = winit::dpi::PhysicalSize::new(1000, 800);
        assert!(matches!(
            resize_direction(size, PhysicalPosition::new(11.0, 300.0), 2.0),
            Some(ResizeDirection::West)
        ));
        assert!(matches!(
            resize_direction(size, PhysicalPosition::new(990.0, 3.0), 2.0),
            Some(ResizeDirection::NorthEast)
        ));
        // A 10 px del borde superior (zona de pestañas) no debe activarse el
        // redimensionado superior: solo el lateral derecho.
        assert!(matches!(
            resize_direction(size, PhysicalPosition::new(990.0, 10.0), 2.0),
            Some(ResizeDirection::East)
        ));
        assert_eq!(
            resize_direction(size, PhysicalPosition::new(500.0, 400.0), 2.0),
            None
        );
        assert_eq!(
            resize_direction(size, PhysicalPosition::new(500.0, 10.0), 2.0),
            None
        );
        assert!(matches!(
            resize_direction(size, PhysicalPosition::new(500.0, 3.0), 2.0),
            Some(ResizeDirection::North)
        ));
    }
}
