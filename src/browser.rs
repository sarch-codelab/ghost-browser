use crate::store::{BrowserEngine, ClosedTab, SessionTab, Settings, Store};
use crate::webview::{self, TabEvent};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};
use wry::WebView;

/// Máximo de texturas de imágenes Wisp en caché con LRU suave.
pub const MAX_WISP_TEXTURES: usize = 256;
/// Límites para HashMaps pendientes para evitar crecimiento indefinido.
const MAX_PENDING_WISP_REQUESTS: usize = 50;
const MAX_PENDING_WISP_LAYOUTS: usize = 50;
const MAX_RESTORE_STARTED: usize = 50;
const MAX_SPA_PROBE_AT: usize = 50;
const MAX_PENDING_HISTORY_INDEX: usize = 50;
const MAX_PENDING_REMOTE_EVALS: usize = 20;

/// Altura total de la franja superior: banda de redimensionado (6) + fila de pestañas/controles (32) + separación (4) + barra de direcciones (32).
pub const UI_HEIGHT: f32 = 80.0;
pub const CHROME_TOP_INSET: f32 = 6.0;
pub const STATUS_HEIGHT: f32 = 24.0;
/// Pestañas que se restauran al arrancar. Las suspendidas solo ocupan unos
/// pocos bytes de metadatos; el resto de la sesión se conserva en disco.
pub const MAX_RESTORED_TABS: usize = 200;
/// Máximo de entradas en el historial de navegación por pestaña para evitar
/// crecimiento indefinido de memoria en sesiones largas.
const MAX_HISTORY_PER_TAB: usize = 50;
const RESTORE_TIMEOUT: Duration = Duration::from_secs(15);
/// Espera antes de sondear si una navegación SPA terminó de renderizar.
const SPA_PROBE_DELAY: Duration = Duration::from_secs(6);
/// Mide texto visible y tamaño del DOM; responde por IPC como `spa_probe`.
const SPA_PROBE_JS: &str = "try{var b=document.body,t=b?b.innerText.length:0,h=document.documentElement?document.documentElement.innerHTML.length:0;window.ipc.postMessage(JSON.stringify({type:'spa_probe',len:t,html:h}))}catch(e){}";

/// Layout de Wisp en curso: ancho calculándose y último ancho pedido (por si
/// el usuario sigue redimensionando mientras el hilo trabaja).
struct PendingWispLayout {
    request_id: u64,
    width: f32,
    desired_width: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    None,
    Bookmarks,
    History,
    Settings,
    Reader,
}

pub struct Tab {
    pub id: u64,
    pub engine: BrowserEngine,
    /// `None` cuando la pestaña está suspendida para ahorrar memoria.
    pub webview: Option<WebView>,
    pub wisp_html: Option<wisp_engine::FetchedPage>,
    pub wisp_layout: Option<wisp_engine::LayoutDocument>,
    pub wisp_layout_width: Option<f32>,
    pub wisp_error: Option<String>,
    /// Texto extraído para el modo lectura (se calcula una vez por página).
    pub wisp_reader: Option<String>,
    /// `true` cuando el usuario eligió el motor a mano: desactiva el
    /// "smart engine" (`wisp_engine::url_suits_wisp`) en las próximas
    /// navegaciones de esta pestaña.
    pub engine_manual: bool,
    pub title: String,
    pub url: String,
    pub loading: bool,
    pub zoom: f64,
    pub navigation_history: Vec<String>,
    pub history_index: usize,
    /// Última vez que la pestaña estuvo activa (para suspenderla por LRU).
    pub last_active: Instant,
    /// URL a la que ir cuando se restaure una pestaña suspendida.
    pub pending_url: Option<String>,
    /// Pestaña de inicio creada al arrancar. No se guarda en la sesión para
    /// que cada apertura tenga una pestaña de inicio nueva.
    pub ephemeral: bool,
}

impl Tab {
    /// ¿Tiene un webview vivo (proceso de renderizado) en memoria?
    pub fn is_live(&self) -> bool {
        self.webview.is_some()
            || (self.engine == BrowserEngine::Wisp
                && (self.loading || self.wisp_html.is_some() || self.wisp_error.is_some()))
    }

    pub fn display_title(&self) -> String {
        if !self.title.is_empty() {
            self.title.to_string()
        } else {
            host_or_url(&self.url)
        }
    }

    pub fn display_url(&self) -> String {
        if self.url.is_empty() || webview::is_newtab_url(&self.url) {
            String::new()
        } else {
            self.url.to_string()
        }
    }

    /// ¿Es la página de nueva pestaña? Se pinta de forma nativa en egui:
    /// no tiene webview ni carga Wisp, así que no consume procesos.
    pub fn is_newtab(&self) -> bool {
        webview::is_newtab_url(&self.url)
    }
}

fn host_or_url(url: &str) -> String {
    if url.is_empty() || webview::is_newtab_url(url) {
        return "Nueva pestaña".to_string();
    }
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| url.to_string())
}

/// Normaliza la URL interna que WebView2 reporta para la página de inicio.
fn normalize_tab_url(url: &str) -> String {
    if webview::is_newtab_url(url) {
        "ghost://newtab".to_string()
    } else {
        url.to_string()
    }
}

fn same_origin(left: &str, right: &str) -> bool {
    if webview::is_newtab_url(left) || webview::is_newtab_url(right) {
        return webview::is_newtab_url(left) && webview::is_newtab_url(right);
    }
    match (url::Url::parse(left), url::Url::parse(right)) {
        (Ok(left), Ok(right)) => left.origin() == right.origin(),
        _ => false,
    }
}

fn is_web_url(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
}

/// Envuelve JS de un `eval` remoto para que el resultado vuelva por
/// `window.ipc` como mensaje `remote_eval` con el `eval_id` dado.
/// Soporta valores directos y promesas (`then`/`catch`).
/// Parámetros CDP de `Input.dispatchKeyEvent` para teclas comunes.
/// Devuelve (key, code, windowsVirtualKeyCode, text).
fn remote_key_params(key: &str) -> Option<(&'static str, &'static str, u32, &'static str)> {
    Some(match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => ("Enter", "Enter", 13, "\r"),
        "tab" => ("Tab", "Tab", 9, "\t"),
        "escape" | "esc" => ("Escape", "Escape", 27, ""),
        "backspace" => ("Backspace", "Backspace", 8, ""),
        "delete" | "del" => ("Delete", "Delete", 46, ""),
        " " | "space" | "spacebar" => (" ", "Space", 32, " "),
        "arrowleft" | "left" => ("ArrowLeft", "ArrowLeft", 37, ""),
        "arrowup" | "up" => ("ArrowUp", "ArrowUp", 38, ""),
        "arrowright" | "right" => ("ArrowRight", "ArrowRight", 39, ""),
        "arrowdown" | "down" => ("ArrowDown", "ArrowDown", 40, ""),
        "home" => ("Home", "Home", 36, ""),
        "end" => ("End", "End", 35, ""),
        "pageup" => ("PageUp", "PageUp", 33, ""),
        "pagedown" => ("PageDown", "PageDown", 34, ""),
        "insert" => ("Insert", "Insert", 45, ""),
        _ => return None,
    })
}

/// Ruta por defecto para screenshots remotos: `%TEMP%/ghost_shots/`.
fn default_shot_path(tab_id: u64) -> PathBuf {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    std::env::temp_dir()
        .join("ghost_shots")
        .join(format!("ghost_tab{tab_id}_{millis}.png"))
}

fn remote_eval_script(eval_id: u64, js: &str) -> String {
    let source = serde_json::to_string(js).unwrap_or_else(|_| "\"\"".into());
    format!(
        "(function(){{\
         var __send=function(o){{try{{window.ipc.postMessage(JSON.stringify(o))}}catch(e){{}}}};\
         var __src={source};\
         var __id={eval_id};\
         try{{\
           var __r=eval(__src);\
           if(__r&&typeof __r.then==='function'){{\
             __r.then(function(v){{__send({{type:'remote_eval',id:__id,ok:true,result:String(v)}})}})\
                .catch(function(e){{__send({{type:'remote_eval',id:__id,ok:false,error:String(e&&e.message||e)}})}});\
           }}else{{\
             __send({{type:'remote_eval',id:__id,ok:true,result:(typeof __r==='string'?__r:String(__r))}});\
           }}\
         }}catch(__e){{\
           __send({{type:'remote_eval',id:__id,ok:false,error:String(__e&&__e.message||__e)}});\
         }}\
         }})();"
    )
}

fn normalized_navigation_history(
    history: &[String],
    history_index: usize,
    current_url: &str,
) -> (Vec<String>, usize) {
    let current_url = normalize_tab_url(current_url);
    let mut history: Vec<String> = history
        .iter()
        .filter(|url| !url.trim().is_empty())
        .map(|url| normalize_tab_url(url))
        .collect();
    if history.is_empty() {
        history.push(current_url);
        return (history, 0);
    }

    let mut index = history_index.min(history.len() - 1);
    if history[index] != current_url {
        if let Some(existing) = history.iter().rposition(|url| url == &current_url) {
            index = existing;
        } else {
            history.truncate(index + 1);
            history.push(current_url);
            index = history.len() - 1;
        }
    }

    // Limitar el historial al cargar desde disco
    if history.len() > MAX_HISTORY_PER_TAB {
        let excess = history.len() - MAX_HISTORY_PER_TAB;
        history.drain(..excess);
        index = index.saturating_sub(excess);
    }

    (history, index)
}

fn push_navigation(history: &mut Vec<String>, index: &mut usize, url: String) {
    if history.get(*index) == Some(&url) {
        return;
    }
    history.truncate(index.saturating_add(1));
    history.push(url);
    *index = history.len() - 1;

    // Limitar el tamaño del historial para evitar crecimiento indefinido
    if history.len() > MAX_HISTORY_PER_TAB {
        let excess = history.len() - MAX_HISTORY_PER_TAB;
        history.drain(..excess);
        *index = index.saturating_sub(excess);
    }
}

pub struct Browser {
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub next_tab_id: u64,
    /// Pila de pestañas cerradas (la más reciente al final).
    pub closed_tabs: Vec<ClosedTab>,
    pub store: Store,
    /// Texto actual de la barra de direcciones.
    pub addr_text: String,
    /// Texto del buscador de la página de nueva pestaña nativa.
    pub newtab_query: String,
    /// La barra de direcciones tiene el foco (el usuario está escribiendo).
    pub addr_editing: bool,
    /// Solicitud de enfocar la barra (Ctrl+L).
    pub addr_focus_request: bool,
    /// Pide devolver el foco del teclado a la ventana (la webview lo tenía).
    pub focus_window_request: bool,
    /// Hay que seleccionar todo el texto de la barra de direcciones.
    pub addr_select_all: bool,
    pub panel: Panel,
    pub status: String,
    status_since: Instant,
    pub event_rx: Receiver<TabEvent>,
    event_tx: Sender<TabEvent>,
    event_proxy: Option<winit::event_loop::EventLoopProxy<()>>,
    pending_wisp_requests: HashMap<u64, u64>,
    next_wisp_request_id: u64,
    /// Layouts de Wisp calculándose en segundo plano, uno por pestaña.
    pending_wisp_layouts: HashMap<u64, PendingWispLayout>,
    next_wisp_layout_id: u64,
    /// Carpeta de descargas compartida con los handlers de los webviews.
    pub download_dir: Arc<RwLock<PathBuf>>,
    /// Nuevas pestañas pendientes de crear (requieren el handle de la ventana).
    pub pending_new_tabs: VecDeque<Option<String>>,
    /// Pestañas suspendidas que hay que volver a crear.
    pub pending_restore: VecDeque<u64>,
    /// Sesión restante que no se muestra por el límite de pestañas visibles.
    unloaded_session: Vec<SessionTab>,
    /// Inicio de una restauración, para medirla y aplicar su timeout.
    restore_started: HashMap<u64, Instant>,
    pending_history_index: HashMap<u64, usize>,
    /// Sonda de salud tras navegaciones SPA: si la página queda con DOM
    /// grande pero casi sin texto visible, se fuerza una recarga completa.
    spa_probe_at: HashMap<u64, Instant>,
    /// Pestañas cuyo webview se mantiene oculto mientras se restaura.
    restoring_tabs: HashSet<u64>,
    /// Indica que hay que resincronizar los límites de los webviews.
    pub needs_bounds_sync: bool,
    /// Altura real medida del chrome en el último frame (la webview empieza ahí,
    /// así que se adapta a cualquier DPI o tamaño de ventana).
    pub chrome_height: f32,
    /// Altura real medida de la barra de estado.
    pub status_height: f32,
    /// Estado real de la ventana (winit), para que el botón de maximizar
    /// muestre el icono correcto y pueda restaurar.
    pub maximized: bool,
    /// Bandera compartida con los manejadores de navegación de los webviews.
    pub block_ads: Arc<AtomicBool>,
    /// Splash de arranque: muestra el logo hasta que la pestaña de inicio
    /// esté lista, en lugar de enseñar una superficie blanca de WebView2.
    pub splash: bool,
    splash_page_loaded: bool,
    splash_until: Instant,
    /// Textura del logotipo para la interfaz.
    pub logo_tex: Option<egui::TextureHandle>,
    /// Caché de imágenes decodificadas de páginas Wisp, por URL resuelta.
    pub wisp_textures: HashMap<String, egui::TextureHandle>,
    /// Orden de uso reciente de texturas Wisp para LRU suave.
    pub wisp_texture_order: VecDeque<String>,
    /// Cuántas pestañas se han suspendido en esta sesión (dato informativo).
    pub suspended_this_session: usize,
    /// Motor forzado desde la línea de comandos (`--wisp`/`--webview2`), si lo hay.
    pub cli_engine: Option<BrowserEngine>,
    dirty: bool,
    /// Indica que el modo oscuro cambió y hay que actualizar el tema de la ventana.
    pub dark_mode_changed: bool,
    /// `eval` remotos pendientes de que la página devuelva el resultado por IPC.
    pending_remote_evals: HashMap<u64, Sender<serde_json::Value>>,
    next_remote_eval_id: u64,
}

impl Browser {
    /// Navegador con un Store ya cargado (perfil real o preparado por CLI).
    /// Separado de `from_store` para que `main` pueda ajustar sesión/ajustes
    /// antes de construir el navegador.
    pub fn with_store(store: Store) -> (Self, std::sync::mpsc::Sender<TabEvent>) {
        Self::from_store(store)
    }

    /// Navegador con un Store en memoria, para tests que no deben tocar el
    /// perfil real del usuario.
    #[cfg(test)]
    pub fn for_tests() -> (Self, std::sync::mpsc::Sender<TabEvent>) {
        Self::from_store(Store::default())
    }

    /// Construye el navegador con un Store ya cargado. Separado de `new` para
    /// que los tests puedan comprobar el arranque sin tocar el perfil real.
    fn from_store(store: Store) -> (Self, std::sync::mpsc::Sender<TabEvent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let event_tx = tx.clone();
        let download_dir = Arc::new(RwLock::new(default_download_dir(&store)));
        let unloaded_session = store
            .session
            .iter()
            .skip(MAX_RESTORED_TABS)
            .cloned()
            .collect();

        // La sesión anterior se conserva, pero no se carga al arrancar: siempre
        // abrimos una pestaña de inicio nueva y rápida. Las demás quedan
        // suspendidas (sin proceso) hasta que el usuario las elija.
        let now = Instant::now();
        let mut tabs: Vec<Tab> = Vec::with_capacity(store.session.len().min(MAX_RESTORED_TABS) + 1);
        let mut next_id = 1u64;
        for s in store.session.iter().take(MAX_RESTORED_TABS) {
            if s.url.trim().is_empty() || webview::is_newtab_url(&s.url) {
                continue;
            }
            let (navigation_history, history_index) =
                normalized_navigation_history(&s.navigation_history, s.history_index, &s.url);
            tabs.push(Tab {
                id: next_id,
                engine: s.engine,
                webview: None,
                wisp_html: None,
                wisp_layout: None,
                wisp_layout_width: None,
                wisp_error: None,
                wisp_reader: None,
                engine_manual: false,
                title: s.title.clone(),
                url: s.url.clone(),
                loading: false,
                zoom: if s.zoom.is_finite() {
                    s.zoom.clamp(0.25, 5.0)
                } else {
                    1.0
                },
                navigation_history,
                history_index,
                last_active: now,
                pending_url: None,
                ephemeral: false,
            });
            next_id += 1;
        }

        // Pestaña de inicio always-on. Es efímera: no se guarda en la sesión,
        // así que no se acumula una pestaña de inicio en cada arranque.
        let home_url = {
            let raw = store.settings.home_page.trim();
            if raw.is_empty() {
                "ghost://newtab".to_string()
            } else {
                normalize_tab_url(&webview::normalize_url(raw, "ghost://newtab"))
            }
        };
        let home_id = next_id;
        tabs.insert(
            0,
            Tab {
                id: home_id,
                engine: store.settings.default_engine,
                webview: None,
                wisp_html: None,
                wisp_layout: None,
                wisp_layout_width: None,
                wisp_error: None,
                wisp_reader: None,
                engine_manual: false,
                title: String::new(),
                url: home_url.clone(),
                // La nueva pestaña es nativa (egui): no hay nada que restaurar
                // ni proceso que crear para ella.
                loading: !webview::is_newtab_url(&home_url),
                zoom: store.settings.default_zoom.clamp(0.25, 5.0),
                navigation_history: vec![home_url.clone()],
                history_index: 0,
                last_active: now,
                pending_url: None,
                ephemeral: true,
            },
        );
        let active = 0;
        let next_tab_id = home_id + 1;
        let mut pending_restore = VecDeque::new();
        if !webview::is_newtab_url(&home_url) {
            pending_restore.push_back(home_id);
        }
        let splash_until = now + Duration::from_millis(450);

        let closed_tabs = store.closed_tabs.clone();
        let block_ads = Arc::new(AtomicBool::new(store.settings.block_ads));

        let browser = Self {
            tabs,
            active,
            next_tab_id,
            store,
            addr_text: String::new(),
            newtab_query: String::new(),
            addr_editing: false,
            addr_focus_request: false,
            focus_window_request: false,
            addr_select_all: false,
            panel: Panel::None,
            status: "Listo".to_string(),
            status_since: Instant::now(),
            event_rx: rx,
            event_tx,
            event_proxy: None,
            pending_wisp_requests: HashMap::new(),
            next_wisp_request_id: 0,
            pending_wisp_layouts: HashMap::new(),
            next_wisp_layout_id: 0,
            download_dir,
            pending_new_tabs: VecDeque::new(),
            pending_restore,
            unloaded_session,
            restore_started: HashMap::new(),
            pending_history_index: HashMap::new(),
            spa_probe_at: HashMap::new(),
            restoring_tabs: HashSet::new(),
            needs_bounds_sync: true,
            chrome_height: UI_HEIGHT,
            status_height: STATUS_HEIGHT,
            maximized: false,
            block_ads,
            splash: true,
            splash_page_loaded: false,
            splash_until,
            logo_tex: None,
            wisp_textures: HashMap::new(),
            wisp_texture_order: VecDeque::new(),
            suspended_this_session: 0,
            cli_engine: None,
            dirty: false,
            dark_mode_changed: false,
            closed_tabs,
            pending_remote_evals: HashMap::new(),
            next_remote_eval_id: 0,
        };
        (browser, tx)
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    pub fn set_event_proxy(&mut self, proxy: winit::event_loop::EventLoopProxy<()>) {
        self.event_proxy = Some(proxy);
    }

    /// Cambia el motor de la pestaña sin cargar nada: limpia el estado del
    /// motor anterior y los requests pendientes. La navegación resultante la
    /// decide el llamador ([`set_tab_engine`] recarga la URL actual; el smart
    /// engine deja que [`navigate_tab_by_id`] navegue a la URL pedida).
    fn reset_tab_for_engine(&mut self, tab_id: u64, engine: BrowserEngine) {
        if let Some(tab) = self.tab_by_id_mut(tab_id) {
            tab.engine = engine;
            tab.webview = None;
            tab.wisp_html = None;
            tab.wisp_layout = None;
            tab.wisp_layout_width = None;
            tab.wisp_error = None;
            tab.wisp_reader = None;
            tab.loading = true;
        }
        self.pending_wisp_requests.remove(&tab_id);
        self.pending_wisp_layouts.remove(&tab_id);
        self.pending_history_index.remove(&tab_id);
        self.pending_restore.retain(|pending| *pending != tab_id);
        self.cancel_restore(tab_id);
        // Limpiar texturas Wisp asociadas a esta pestaña si se descarga
        self.cleanup_wisp_textures_for_tab(tab_id);
    }

    /// Limpia texturas Wisp de una pestaña específica cuando se descarga o cambia de motor.
    fn cleanup_wisp_textures_for_tab(&mut self, tab_id: u64) {
        let urls_to_remove: Vec<String> = self
            .tab_by_id(tab_id)
            .and_then(|tab| tab.wisp_html.as_ref())
            .map(|html| html.images.keys().cloned().collect())
            .unwrap_or_default();

        for url in urls_to_remove {
            self.wisp_textures.remove(&url);
            self.wisp_texture_order.retain(|x| x != &url);
        }
    }

    /// Inserta en pending_history_index con límite de tamaño.
    fn set_pending_history_index(&mut self, tab_id: u64, index: usize) {
        if self.pending_history_index.len() >= MAX_PENDING_HISTORY_INDEX {
            log::warn!("Límite de índices de historial pendientes alcanzado, limpiando");
            self.pending_history_index.clear();
        }
        self.pending_history_index.insert(tab_id, index);
    }

    /// Cambio de motor iniciado por el usuario (botón, ajustes o atajo):
    /// recarga la URL actual y marca la pestaña como `engine_manual` para que
    /// el smart engine no la vuelva a tocar.
    pub fn set_tab_engine(&mut self, tab_id: u64, engine: BrowserEngine) {
        let Some(tab) = self.tab_by_id(tab_id) else {
            return;
        };
        if tab.engine == engine {
            return;
        }
        let url = tab.url.clone();
        if webview::is_newtab_url(&url) {
            // Solo cambia la etiqueta del motor: la página nativa no carga nada
            // hasta que el usuario navegue a una URL real.
            if let Some(tab) = self.tab_by_id_mut(tab_id) {
                tab.engine = engine;
                tab.engine_manual = true;
            }
            self.dirty = true;
            self.save_session_immediate();
            return;
        }
        self.reset_tab_for_engine(tab_id, engine);
        if let Some(tab) = self.tab_by_id_mut(tab_id) {
            tab.engine_manual = true;
        }
        match engine {
            BrowserEngine::WebView2 => self.queue_restore(tab_id),
            BrowserEngine::Wisp => self.start_wisp_load(tab_id, url),
        }
        self.needs_bounds_sync = true;
        self.dirty = true;
        self.save_session_immediate();
    }

    /// Conmutación automática a WebView2 cuando Wisp no puede cargar la
    /// página (fetch fallido o layout inválido): el usuario nunca ve una
    /// pantalla de error. No marca `engine_manual` — el smart engine puede
    /// volver a probar Wisp en la próxima navegación.
    fn fallback_to_webview(&mut self, tab_id: u64) {
        let Some(tab) = self.tab_by_id(tab_id) else {
            return;
        };
        if tab.engine != BrowserEngine::Wisp {
            return;
        }
        if webview::is_newtab_url(&tab.url) {
            return;
        }
        self.reset_tab_for_engine(tab_id, BrowserEngine::WebView2);
        self.queue_restore(tab_id);
        self.needs_bounds_sync = true;
        self.dirty = true;
        self.save_session_immediate();
    }

    /// Alterna el motor de la pestaña activa (botón de la barra, Ctrl+Shift+W).
    pub fn toggle_engine_active(&mut self) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let id = tab.id;
        let engine = match tab.engine {
            BrowserEngine::WebView2 => BrowserEngine::Wisp,
            BrowserEngine::Wisp => BrowserEngine::WebView2,
        };
        self.set_tab_engine(id, engine);
    }

    /// Texto de lectura de la pestaña activa. Solo existe para Wisp con la
    /// página ya preparada (el layout garantiza que `prepare_document` ya
    /// corrió, así que extraer el texto es barato).
    pub fn active_reader_text(&mut self) -> Option<String> {
        let id = self.active_tab()?.id;
        let tab = self.tab_by_id_mut(id)?;
        if tab.engine != BrowserEngine::Wisp || tab.wisp_layout.is_none() {
            return None;
        }
        if tab.wisp_reader.is_none() {
            tab.wisp_reader = tab.wisp_html.as_ref().and_then(|page| page.reader_text());
        }
        tab.wisp_reader.clone()
    }

    /// ¿La pestaña activa puede ofrecer modo lectura ahora mismo?
    pub fn active_can_read(&self) -> bool {
        self.active_tab().is_some_and(|tab| {
            tab.engine == BrowserEngine::Wisp
                && tab.wisp_html.is_some()
                && tab.wisp_layout.is_some()
        })
    }

    pub fn start_wisp_load(&mut self, tab_id: u64, url: String) {
        let Some(tab) = self.tab_by_id(tab_id) else {
            return;
        };
        if tab.engine != BrowserEngine::Wisp {
            return;
        }

        self.next_wisp_request_id = self.next_wisp_request_id.wrapping_add(1).max(1);
        let request_id = self.next_wisp_request_id;

        // Limitar tamaño del HashMap
        if self.pending_wisp_requests.len() >= MAX_PENDING_WISP_REQUESTS {
            log::warn!("Límite de requests Wisp pendientes alcanzado, limpiando");
            self.pending_wisp_requests.clear();
        }
        self.pending_wisp_requests.insert(tab_id, request_id);
        if let Some(tab) = self.tab_by_id_mut(tab_id) {
            tab.url = url.clone();
            tab.loading = true;
            tab.wisp_html = None;
            tab.wisp_layout = None;
            tab.wisp_layout_width = None;
            tab.wisp_error = None;
            tab.wisp_reader = None;
            if !webview::is_newtab_url(&url) {
                tab.ephemeral = false;
            }
        }
        self.set_status("Cargando con Wisp Beta…".to_string());

        if webview::is_newtab_url(&url) {
            let page = wisp_engine::FetchedPage::new(
                "ghost://newtab".to_string(),
                "Nueva pestaña · Wisp Beta".to_string(),
                r#"
                    <main style="padding:32px;color:#222">
                      <h1>Wisp Engine <span style="color:#b45309">Beta</span></h1>
                      <p>Motor experimental de Ghost. Escribe una dirección HTTP o HTTPS para probarlo.</p>
                      <p>Wisp ejecuta scripts inline/externos y módulos ES con un DOM limitado, y ya pinta imágenes.</p>
                    </main>
                "#
                .to_string(),
                Vec::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
                Vec::new(),
            );
            self.deliver_wisp_result(tab_id, request_id, Box::new(Ok(page)));
            return;
        }

        let event_tx = self.event_tx.clone();
        let event_proxy = self.event_proxy.clone();
        let request_url = url.clone();
        let spawn = thread::Builder::new()
            .name(format!("wisp-load-{tab_id}"))
            .spawn(move || {
                let result =
                    wisp_engine::fetch_url(&request_url).map_err(|error| format!("{error:#}"));
                let event = TabEvent::WispPageLoaded {
                    tab: tab_id,
                    request_id,
                    result: Box::new(result),
                };
                if event_tx.send(event).is_ok()
                    && let Some(proxy) = event_proxy
                {
                    let _ = proxy.send_event(());
                }
            });
        if let Err(error) = spawn {
            log::error!("No se pudo iniciar la carga de Wisp para la pestaña {tab_id}: {error}");
            self.deliver_wisp_result(
                tab_id,
                request_id,
                Box::new(Err(format!("No se pudo iniciar la carga: {error}"))),
            );
        }
    }

    fn deliver_wisp_result(
        &mut self,
        tab_id: u64,
        request_id: u64,
        result: Box<Result<wisp_engine::FetchedPage, String>>,
    ) {
        self.handle_event(TabEvent::WispPageLoaded {
            tab: tab_id,
            request_id,
            result,
        });
    }

    pub fn is_restoring(&self, tab_id: u64) -> bool {
        self.restoring_tabs.contains(&tab_id)
    }

    pub fn restore_elapsed(&self, tab_id: u64) -> Option<Duration> {
        self.restore_started
            .get(&tab_id)
            .map(|started| started.elapsed())
    }

    pub fn cancel_restore(&mut self, tab_id: u64) {
        self.restore_started.remove(&tab_id);
        if self.restoring_tabs.remove(&tab_id) {
            self.needs_bounds_sync = true;
        }
    }

    pub fn fail_restore(&mut self, tab_id: u64) {
        self.cancel_restore(tab_id);
        if let Some(tab) = self.tab_by_id_mut(tab_id) {
            tab.loading = false;
        }
        self.set_status("No se pudo restaurar la pestaña".to_string());
    }

    pub fn retry_restore(&mut self, tab_id: u64) -> bool {
        let Some(tab) = self.tab_by_id_mut(tab_id) else {
            return false;
        };
        if tab.is_live() {
            return false;
        }
        tab.loading = true;
        self.queue_restore(tab_id);
        self.set_status("Restaurando pestaña…".to_string());
        true
    }

    pub fn tick_restores(&mut self, now: Instant) -> bool {
        let expired: Vec<u64> = self
            .restore_started
            .iter()
            .filter(|(_, started)| now.saturating_duration_since(**started) >= RESTORE_TIMEOUT)
            .map(|(tab_id, _)| *tab_id)
            .collect();
        if expired.is_empty() {
            return false;
        }

        for tab_id in expired {
            self.restore_started.remove(&tab_id);
            self.restoring_tabs.remove(&tab_id);
            if let Some(tab) = self.tab_by_id_mut(tab_id) {
                tab.loading = false;
            }
        }
        self.needs_bounds_sync = true;
        if self.restore_started.is_empty() && self.status.starts_with("Restaurando") {
            self.set_status("Listo".to_string());
        }
        true
    }

    /// Inicia el splash cuando la ventana ya está lista para pintar. Si se
    /// iniciara en `Browser::new`, la inicialización de wgpu podría consumir el
    /// tiempo mínimo y el logo apenas se vería.
    pub fn begin_splash(&mut self) {
        self.splash = true;
        self.splash_page_loaded = false;
        self.splash_until = Instant::now() + Duration::from_millis(450);
    }

    /// Termina el splash cuando la pestaña de inicio ya cargó (con un mínimo
    /// visible para que no parpadee y un timeout de seguridad).
    pub fn tick_splash(&mut self, now: Instant) -> bool {
        if !self.splash {
            return false;
        }
        let grace_over = now >= self.splash_until;
        let timed_out = now >= self.splash_until + Duration::from_secs(4);
        // Si la pestaña de inicio es la nueva pestaña nativa no llega ningún
        // PageFinished: se considera cargada en cuanto pasa el tiempo mínimo.
        let home_ready = self.splash_page_loaded || self.active_tab().is_some_and(Tab::is_newtab);
        if (home_ready && grace_over) || timed_out {
            self.splash = false;
            self.needs_bounds_sync = true;
            crate::perf::mark("startup:splash_end");
            return true;
        }
        false
    }

    /// Dispara las sondas SPA vencidas: evalúa un probe en cada webview
    /// pendiente; la página responde por IPC (`spa_probe`) con sus métricas.
    pub fn tick_spa_probes(&mut self, now: Instant) {
        let due: Vec<u64> = self
            .spa_probe_at
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(id, _)| *id)
            .collect();
        for tab_id in due {
            self.spa_probe_at.remove(&tab_id);
            if let Some(webview) = self.tab_by_id(tab_id).and_then(|tab| tab.webview.as_ref()) {
                let _ = webview.evaluate_script(SPA_PROBE_JS);
            }
        }
    }

    pub fn tab_index_of(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    pub fn tab_by_id(&self, id: u64) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn tab_by_id_mut(&mut self, id: u64) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    pub fn add_tab(&mut self, tab: Tab) {
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.needs_bounds_sync = true;
        self.dirty = true;
        self.save_session_immediate();
    }

    /// Pestañas con proceso vivo.
    pub fn live_count(&self) -> usize {
        self.tabs.iter().filter(|t| t.webview.is_some()).count()
    }

    /// Pestañas suspendidas (sin proceso). La nueva pestaña nativa no tiene
    /// proceso por diseño, no cuenta como suspendida.
    pub fn suspended_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|t| {
                t.engine == BrowserEngine::WebView2 && t.webview.is_none() && !t.is_newtab()
            })
            .count()
    }

    /// Consumir eventos pendientes de los webviews. Devuelve true si algo cambió.
    pub fn drain_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(ev) = self.event_rx.try_recv() {
            changed = true;
            self.handle_event(ev);
        }
        if self.addr_text_needs_sync() {
            changed = true;
        }
        changed
    }

    fn handle_event(&mut self, ev: TabEvent) {
        match ev {
            TabEvent::PageStarted { tab, url } => {
                let url = normalize_tab_url(&url);
                if let Some(t) = self.tab_by_id_mut(tab) {
                    // Si se navega desde la pestaña de inicio, deja de ser
                    // efímera y pasa a guardarse con el resto de la sesión.
                    if t.ephemeral && !webview::is_newtab_url(&url) && t.url != url {
                        t.ephemeral = false;
                    }
                    t.loading = true;
                }
                self.record_navigation(tab, &url, "push");
            }
            TabEvent::PageFinished { tab, url } => {
                if self.active_tab().map(|t| t.id) == Some(tab) {
                    self.splash_page_loaded = true;
                    if self.splash {
                        crate::perf::mark("startup:home_page_loaded");
                    }
                }
                if let Some(started) = self.restore_started.remove(&tab) {
                    crate::perf::mark_restore(tab, started.elapsed());
                }
                if self.restoring_tabs.remove(&tab) {
                    self.needs_bounds_sync = true;
                }
                let url = normalize_tab_url(&url);
                if let Some(t) = self.tab_by_id_mut(tab) {
                    t.loading = false;
                    if t.title.is_empty() {
                        t.title = host_or_url(&url);
                    }
                }
                self.record_navigation(tab, &url, "replace");
                if let Some(t) = self.tab_by_id(tab) {
                    let title = t.title.clone();
                    if !webview::is_newtab_url(&url)
                        && !url.starts_with("ghost:")
                        && url != "about:blank"
                    {
                        self.store.add_history(url, title);
                    }
                }
                if self.status.starts_with("Restaurando") {
                    self.set_status("Listo".to_string());
                }
                self.sync_addr_text();
                self.dirty = true;
                self.save_session_immediate();
            }
            TabEvent::TitleChanged { tab, title } => {
                if let Some(t) = self.tab_by_id_mut(tab) {
                    t.title = title.clone();
                }
                let tab_url = self.tab_by_id(tab).map(|t| t.url.clone());
                if let (Some(url), Some(last)) = (tab_url, self.store.history.last_mut())
                    && last.url == url
                    && !title.is_empty()
                {
                    last.title = title.clone();
                }
                self.dirty = true;
                self.save_session_immediate();
            }
            TabEvent::Ipc {
                tab,
                source_url,
                message,
            } => self.handle_ipc(tab, &source_url, &message),
            TabEvent::DownloadStarted { url: _, path, .. } => {
                self.set_status(format!("Descargando → {}", path.display()));
            }
            TabEvent::DownloadFinished { path, ok, .. } => {
                self.set_status(match (path, ok) {
                    (Some(p), true) => format!("Descarga completada: {}", p.display()),
                    (Some(p), false) => format!("Descarga fallida: {}", p.display()),
                    (None, true) => "Descarga completada".to_string(),
                    (None, false) => "Descarga fallida".to_string(),
                });
            }
            TabEvent::WispPageLoaded {
                tab,
                request_id,
                result,
            } => self.handle_wisp_page_loaded(tab, request_id, result),
            TabEvent::WispLayoutReady {
                tab,
                request_id,
                result,
            } => self.handle_wisp_layout_ready(tab, request_id, result),
        }
    }

    /// Pide el layout de una pestaña Wisp en segundo plano. Si ya hay uno en
    /// curso solo actualiza el ancho deseado; el resultado se encadena solo.
    pub fn request_wisp_layout(&mut self, tab_id: u64, width: f32) {
        if let Some(pending) = self.pending_wisp_layouts.get_mut(&tab_id) {
            pending.desired_width = width;
            return;
        }
        let Some(page) = self.tab_by_id(tab_id).and_then(|tab| tab.wisp_html.clone()) else {
            return;
        };
        self.next_wisp_layout_id = self.next_wisp_layout_id.wrapping_add(1).max(1);
        let request_id = self.next_wisp_layout_id;

        // Limitar tamaño del HashMap
        if self.pending_wisp_layouts.len() >= MAX_PENDING_WISP_LAYOUTS {
            log::warn!("Límite de layouts Wisp pendientes alcanzado, limpiando");
            self.pending_wisp_layouts.clear();
        }
        self.pending_wisp_layouts.insert(
            tab_id,
            PendingWispLayout {
                request_id,
                width,
                desired_width: width,
            },
        );

        let event_tx = self.event_tx.clone();
        let event_proxy = self.event_proxy.clone();
        let spawn = thread::Builder::new()
            .name(format!("wisp-layout-{tab_id}"))
            .spawn(move || {
                let result = page.layout(width).map_err(|error| error.to_string());
                let event = TabEvent::WispLayoutReady {
                    tab: tab_id,
                    request_id,
                    result: Box::new(result),
                };
                if event_tx.send(event).is_ok()
                    && let Some(proxy) = event_proxy
                {
                    let _ = proxy.send_event(());
                }
            });
        if let Err(error) = spawn {
            self.pending_wisp_layouts.remove(&tab_id);
            log::error!("No se pudo iniciar el layout de Wisp para la pestaña {tab_id}: {error}");
        }
    }

    pub fn wisp_layout_pending(&self, tab_id: u64) -> bool {
        self.pending_wisp_layouts.contains_key(&tab_id)
    }

    #[allow(clippy::boxed_local)]
    fn handle_wisp_layout_ready(
        &mut self,
        tab_id: u64,
        request_id: u64,
        result: Box<Result<wisp_engine::LayoutDocument, String>>,
    ) {
        let Some(pending) = self.pending_wisp_layouts.remove(&tab_id) else {
            return;
        };
        if pending.request_id != request_id {
            return;
        }
        let Some(tab) = self.tab_by_id_mut(tab_id) else {
            return;
        };
        if tab.engine != BrowserEngine::Wisp || tab.wisp_html.is_none() {
            return;
        }
        let failed = result.is_err();
        match *result {
            Ok(layout) => {
                tab.wisp_layout = Some(layout);
                tab.wisp_layout_width = Some(pending.width);
                tab.wisp_error = None;
            }
            Err(error) => {
                tab.wisp_error = Some(error);
                tab.wisp_html = None;
                tab.wisp_reader = None;
                tab.loading = false;
            }
        }
        if failed && !self.tab_by_id(tab_id).is_some_and(|t| t.engine_manual) {
            self.fallback_to_webview(tab_id);
        }
        if (pending.desired_width - pending.width).abs() > 0.5 {
            self.request_wisp_layout(tab_id, pending.desired_width);
        }
    }

    fn handle_wisp_page_loaded(
        &mut self,
        tab_id: u64,
        request_id: u64,
        result: Box<Result<wisp_engine::FetchedPage, String>>,
    ) {
        if self.pending_wisp_requests.get(&tab_id) != Some(&request_id) {
            return;
        }
        self.pending_wisp_requests.remove(&tab_id);
        self.restore_started.remove(&tab_id);
        if self.restoring_tabs.remove(&tab_id) {
            self.needs_bounds_sync = true;
        }

        match *result {
            Ok(page) => {
                let url = normalize_tab_url(&page.final_url);
                let title = if page.title.trim().is_empty() {
                    host_or_url(&url)
                } else {
                    page.title.trim().to_string()
                };
                self.pending_wisp_layouts.remove(&tab_id);
                let Some(tab) = self.tab_by_id_mut(tab_id) else {
                    return;
                };
                if tab.engine != BrowserEngine::Wisp {
                    return;
                }
                tab.url = url.clone();
                tab.title = title.clone();
                tab.wisp_html = Some(page);
                tab.wisp_layout = None;
                tab.wisp_layout_width = None;
                tab.wisp_error = None;
                tab.wisp_reader = None;
                tab.loading = false;
                tab.pending_url = None;

                if self.active_tab().map(|tab| tab.id) == Some(tab_id) && self.splash {
                    self.splash_page_loaded = true;
                    crate::perf::mark("startup:home_page_loaded");
                }
                self.record_navigation(tab_id, &url, "push");
                if !webview::is_newtab_url(&url) && !url.starts_with("ghost:") {
                    self.store.add_history(url, title);
                }
                self.set_status("Listo · Wisp Beta".to_string());
            }
            Err(error) => {
                self.pending_history_index.remove(&tab_id);
                let mut manual = false;
                if let Some(tab) = self.tab_by_id_mut(tab_id) {
                    if tab.engine != BrowserEngine::Wisp {
                        return;
                    }
                    manual = tab.engine_manual;
                    tab.loading = false;
                    tab.wisp_error = Some(error.clone());
                    tab.wisp_html = None;
                    tab.wisp_layout = None;
                    tab.wisp_layout_width = None;
                }
                if manual {
                    self.set_status(format!("Wisp Beta: {error}"));
                } else {
                    self.fallback_to_webview(tab_id);
                    self.set_status(format!(
                        "Wisp no pudo cargar la página; usando WebView2 ({error})"
                    ));
                }
            }
        }
        self.sync_addr_text();
        self.dirty = true;
        self.save_session_immediate();
    }

    fn handle_ipc(&mut self, tab_id: u64, source_url: &str, message: &str) {
        let Some(tab) = self.tab_by_id(tab_id) else {
            return;
        };
        if !same_origin(source_url, &tab.url) {
            log::warn!("Se descartó un mensaje IPC de origen distinto para la pestaña {tab_id}");
            return;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(message) else {
            return;
        };
        let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match kind {
            "shortcut" => {
                if let Some(action) = v.get("action").and_then(|a| a.as_str())
                    && (webview::is_newtab_url(source_url)
                        || matches!(
                            action,
                            "back"
                                | "forward"
                                | "reload"
                                | "zoom_in"
                                | "zoom_out"
                                | "zoom_reset"
                                | "print"
                        ))
                {
                    self.apply_shortcut(tab_id, action);
                }
            }
            "open_url" | "navigate" => {
                if let Some(url) = v.get("url").and_then(|u| u.as_str())
                    && (webview::is_newtab_url(source_url) || is_web_url(url))
                {
                    self.navigate_tab_by_id(tab_id, url);
                }
            }
            "nav" => {
                let navigation = v
                    .get("navigation")
                    .and_then(|kind| kind.as_str())
                    .unwrap_or("replace");
                let url = normalize_tab_url(source_url);
                self.record_navigation(tab_id, &url, navigation);
                self.sync_addr_text();
                // NO save_session_immediate: las SPAs (YouTube, Gmail) disparan
                // pushState/replaceState constantemente y persistir el perfil
                // completo por cada evento satura el hilo de UI con I/O de
                // disco, congelando el webview. `dirty` basta: el flush
                // periódico y el cierre ya guardan la sesión.
                self.dirty = true;
                // Algunas SPAs se quedan con el esqueleto cargado y sin datos
                // (visto en YouTube). Se arma una sonda: si la página sigue
                // "vacía" en unos segundos, se fuerza una carga completa.
                if self.spa_probe_at.len() >= MAX_SPA_PROBE_AT {
                    log::warn!("Límite de sondas SPA alcanzado, limpiando");
                    self.spa_probe_at.clear();
                }
                self.spa_probe_at
                    .insert(tab_id, Instant::now() + SPA_PROBE_DELAY);
            }
            "spa_probe" => {
                let len = v.get("len").and_then(|n| n.as_u64()).unwrap_or(0);
                let html = v.get("html").and_then(|n| n.as_u64()).unwrap_or(0);
                // Esqueleto SPA atascado: DOM grande pero casi nada visible.
                // Las páginas de error/simple tienen HTML pequeño y no aplican.
                if len < 200
                    && html > 50_000
                    && let Some(webview) =
                        self.tab_by_id(tab_id).and_then(|tab| tab.webview.as_ref())
                {
                    log::info!("SPA atascada en pestaña {tab_id} ({len} chars): recargando");
                    let _ = webview.reload();
                    self.set_status("La página no terminó de cargar; recargando…".into());
                }
            }
            "remote_eval" => {
                if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
                    let ok = v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
                    let payload = if ok {
                        serde_json::json!({
                            "ok": true,
                            "result": v.get("result").cloned().unwrap_or(serde_json::Value::Null),
                        })
                    } else {
                        serde_json::json!({
                            "ok": false,
                            "error": v
                                .get("error")
                                .and_then(|e| e.as_str())
                                .unwrap_or("error en la página"),
                        })
                    };
                    self.complete_remote_eval(id, payload);
                }
            }
            _ => {}
        }
    }

    fn record_navigation(&mut self, tab_id: u64, url: &str, kind: &str) {
        let pending_index = self.pending_history_index.remove(&tab_id);
        let Some(tab) = self.tab_by_id_mut(tab_id) else {
            return;
        };
        let url = normalize_tab_url(url);
        if let Some(index) = pending_index {
            if let Some(entry) = tab.navigation_history.get_mut(index) {
                *entry = url.clone();
                tab.history_index = index;
            }
        } else {
            let current = tab.navigation_history.get(tab.history_index);
            if current != Some(&url) {
                match kind {
                    "replace" => {
                        if let Some(entry) = tab.navigation_history.get_mut(tab.history_index) {
                            *entry = url.clone();
                        } else {
                            tab.navigation_history.push(url.clone());
                            tab.history_index = tab.navigation_history.len() - 1;
                        }
                    }
                    "traverse" => {
                        let previous = tab.navigation_history[..tab.history_index]
                            .iter()
                            .rposition(|entry| entry == &url);
                        let next = tab.navigation_history[tab.history_index + 1..]
                            .iter()
                            .position(|entry| entry == &url)
                            .map(|offset| tab.history_index + 1 + offset);
                        if let Some(index) = previous.or(next) {
                            tab.history_index = index;
                        } else {
                            push_navigation(
                                &mut tab.navigation_history,
                                &mut tab.history_index,
                                url.clone(),
                            );
                        }
                    }
                    _ => push_navigation(
                        &mut tab.navigation_history,
                        &mut tab.history_index,
                        url.clone(),
                    ),
                }
            }
        }
        tab.url = url;
    }

    fn set_status(&mut self, msg: String) {
        self.status = msg;
        self.status_since = Instant::now();
    }

    pub fn status(&self) -> &str {
        if self.status_since.elapsed().as_secs() > 8 && self.status.starts_with("Descarg") {
            "Listo"
        } else {
            &self.status
        }
    }

    /// Actualiza el texto de la barra si el usuario no está escribiendo.
    fn sync_addr_text(&mut self) {
        if !self.addr_editing {
            self.addr_text = self
                .active_tab()
                .map(|t| t.display_url())
                .unwrap_or_default();
        }
    }

    fn addr_text_needs_sync(&self) -> bool {
        !self.addr_editing
            && self
                .active_tab()
                .map(|t| t.display_url() != self.addr_text)
                .unwrap_or(false)
    }

    /// Navega la pestaña activa con el texto de la barra de direcciones.
    pub fn navigate_active(&mut self, input: &str) {
        let Some(tab) = self.active_tab() else { return };
        let id = tab.id;
        self.navigate_tab_by_id(id, input);
    }

    pub fn navigate_tab_by_id(&mut self, tab_id: u64, input: &str) {
        // Navegación completa: cualquier sonda SPA pendiente queda obsoleta.
        self.spa_probe_at.remove(&tab_id);
        let (mut engine, engine_manual) = {
            let Some(t) = self.tab_by_id(tab_id) else {
                return;
            };
            (t.engine, t.engine_manual)
        };
        let url = normalize_tab_url(&webview::normalize_url(
            input,
            &self.store.settings.home_page,
        ));
        // La nueva pestaña es nativa: destruir cualquier webview/Wisp y no
        // crear procesos. WebView2 queda 100% perezoso hasta una URL real.
        if webview::is_newtab_url(&url) {
            self.navigate_to_newtab(tab_id);
            return;
        }
        // Smart engine: con `auto_engine` activo y sin elección manual del
        // usuario, la URL decide el motor — artículos/docs en Wisp (liviano),
        // webapps en WebView2 (compatible).
        if !engine_manual && self.store.settings.auto_engine && !webview::is_newtab_url(&url) {
            let suggested = if wisp_engine::url_suits_wisp(&url) {
                BrowserEngine::Wisp
            } else {
                BrowserEngine::WebView2
            };
            if suggested != engine {
                self.reset_tab_for_engine(tab_id, suggested);
                engine = suggested;
            }
        }
        if engine == BrowserEngine::Wisp {
            self.navigate_wisp_tab(tab_id, url);
            return;
        }
        if let Some(webview) = self.tab_by_id(tab_id).and_then(|tab| tab.webview.as_ref()) {
            if let Err(e) = webview.load_url(&url) {
                self.set_status(format!("No se pudo navegar: {e}"));
                return;
            }
            let _ = webview.focus();
            if let Some(t) = self.tab_by_id_mut(tab_id) {
                if !webview::is_newtab_url(&url) {
                    t.ephemeral = false;
                }
                t.url = url;
                t.loading = true;
            }
            self.dirty = true;
            self.save_session_immediate();
            return;
        }
        // Pestaña suspendida: se restaura en la URL pedida.
        if let Some(t) = self.tab_by_id_mut(tab_id) {
            if !webview::is_newtab_url(&url) {
                t.ephemeral = false;
            }
            t.pending_url = Some(url.clone());
            t.url = url;
            t.loading = true;
        }
        self.queue_restore(tab_id);
        self.dirty = true;
        self.save_session_immediate();
    }

    /// Deja la pestaña en la página de nueva pestaña nativa: sin webview, sin
    /// Wisp y sin requests pendientes. El historial la registra como cualquier
    /// navegación para que Atrás/Adelante funcionen.
    fn navigate_to_newtab(&mut self, tab_id: u64) {
        if let Some(t) = self.tab_by_id_mut(tab_id) {
            t.title.clear();
            t.loading = false;
            t.webview = None;
            t.wisp_html = None;
            t.wisp_layout = None;
            t.wisp_layout_width = None;
            t.wisp_error = None;
            t.wisp_reader = None;
            t.pending_url = None;
        }
        self.pending_wisp_requests.remove(&tab_id);
        self.pending_wisp_layouts.remove(&tab_id);
        self.pending_restore.retain(|pending| *pending != tab_id);
        self.cancel_restore(tab_id);
        self.record_navigation(tab_id, "ghost://newtab", "push");
        self.sync_addr_text();
        self.needs_bounds_sync = true;
        self.dirty = true;
        self.save_session_immediate();
    }

    fn navigate_wisp_tab(&mut self, tab_id: u64, url: String) {
        if let Some(tab) = self.tab_by_id_mut(tab_id)
            && !webview::is_newtab_url(&url)
        {
            tab.ephemeral = false;
        }
        self.start_wisp_load(tab_id, url);
        self.dirty = true;
        self.save_session_immediate();
    }

    fn queue_restore(&mut self, tab_id: u64) {
        // La nueva pestaña nativa no necesita proceso: restaurarla crearía un
        // WebView2 solo para mostrar la página de inicio.
        let target_is_newtab = self
            .tab_by_id(tab_id)
            .is_some_and(|t| webview::is_newtab_url(t.pending_url.as_deref().unwrap_or(&t.url)));
        if target_is_newtab {
            return;
        }
        if !self.pending_restore.contains(&tab_id) {
            // El arranque lo cubre el splash: no hace falta dejar la pestaña
            // oculta esperando a PageFinished.
            if !self.splash {
                self.restoring_tabs.insert(tab_id);

                // Limitar tamaño del HashMap
                if self.restore_started.len() >= MAX_RESTORE_STARTED {
                    log::warn!("Límite de restauraciones iniciadas alcanzado, limpiando");
                    self.restore_started.clear();
                }
                self.restore_started
                    .entry(tab_id)
                    .or_insert_with(Instant::now);
            }
            self.pending_restore.push_back(tab_id);
        }
    }

    /// Garantiza que la pestaña tenga webview (lo pide si está suspendida).
    /// La nueva pestaña nativa se considera viva: no necesita proceso.
    fn ensure_live(&mut self, tab_id: u64) -> bool {
        match self.tab_by_id(tab_id) {
            Some(t) if t.is_live() || t.is_newtab() => true,
            Some(_) => {
                if let Some(t) = self.tab_by_id_mut(tab_id) {
                    t.loading = true;
                }
                self.queue_restore(tab_id);
                false
            }
            None => false,
        }
    }

    pub fn go_back(&mut self) {
        self.navigate_history(-1);
    }

    pub fn go_forward(&mut self) {
        self.navigate_history(1);
    }

    fn navigate_history(&mut self, offset: isize) {
        let Some(tab_id) = self.active_tab().map(|tab| tab.id) else {
            return;
        };
        self.navigate_history_for_tab(tab_id, offset);
    }

    fn navigate_history_for_tab(&mut self, id: u64, offset: isize) {
        let Some(tab) = self.tab_by_id(id) else {
            return;
        };
        let next_index = tab.history_index as isize + offset;
        if next_index < 0 || next_index as usize >= tab.navigation_history.len() {
            if let Some(webview) = &tab.webview {
                let result = if offset < 0 {
                    webview.go_back()
                } else {
                    webview.go_forward()
                };
                if let Err(error) = result {
                    self.set_status(format!("No se pudo navegar el historial: {error}"));
                }
            }
            return;
        }

        let next_index = next_index as usize;
        let url = tab.navigation_history[next_index].clone();
        // Volver a la nueva pestaña no crea procesos: es nativa.
        if webview::is_newtab_url(&url) {
            self.set_pending_history_index(id, next_index);
            self.navigate_to_newtab(id);
            return;
        }
        if tab.engine == BrowserEngine::Wisp {
            self.set_pending_history_index(id, next_index);
            self.navigate_wisp_tab(id, url);
            return;
        }
        self.set_pending_history_index(id, next_index);
        if let Some(tab) = self.tab_by_id_mut(id) {
            tab.loading = true;
        }
        if self.tab_by_id(id).is_some_and(Tab::is_live) {
            let result = self
                .tab_by_id(id)
                .and_then(|tab| tab.webview.as_ref())
                .map(|webview| webview.load_url(&url));
            if let Some(Err(error)) = result {
                self.pending_history_index.remove(&id);
                if let Some(tab) = self.tab_by_id_mut(id) {
                    tab.loading = false;
                }
                self.set_status(format!("No se pudo navegar el historial: {error}"));
                return;
            }
        } else if let Some(tab) = self.tab_by_id_mut(id) {
            tab.pending_url = Some(url);
            self.queue_restore(id);
        }
        self.dirty = true;
        self.save_session_immediate();
    }

    pub fn reload(&mut self) {
        if let Some(t) = self.active_tab() {
            if t.is_newtab() {
                return;
            }
            if t.engine == BrowserEngine::Wisp {
                let id = t.id;
                let url = t.url.clone();
                self.start_wisp_load(id, url);
                return;
            }
            if let Some(webview) = &t.webview {
                let _ = webview.reload();
            }
        }
    }

    pub fn go_home(&mut self) {
        let home = self.store.settings.home_page.clone();
        self.navigate_active(&home);
    }

    pub fn request_new_tab(&mut self) {
        self.pending_new_tabs.push_back(None);
    }

    /// Solicita crear pestañas; `main` las materializa con el handle de ventana.
    pub fn drain_pending_tabs(&mut self) -> Vec<Option<String>> {
        self.pending_new_tabs.drain(..).collect()
    }

    /// Pestañas suspendidas que hay que recrear.
    pub fn drain_pending_restore(&mut self) -> Vec<u64> {
        self.pending_restore.drain(..).collect()
    }

    // ---- API de control remoto (localhost) ---------------------------------

    /// Punto de entrada para comandos que llegan del servidor HTTP local.
    /// Debe llamarse desde el hilo de UI (donde vive `Browser`).
    pub fn handle_remote_command(&mut self, command: crate::remote::RemoteCommand) {
        match command {
            crate::remote::RemoteCommand::Sync { request, reply } => {
                // `None` = la respuesta queda diferida: el resultado llegará
                // por IPC de la página (p. ej. innerText de WebView2).
                if let Some(value) = self.handle_remote(&request, &reply) {
                    let _ = reply.send(value);
                }
            }
            crate::remote::RemoteCommand::Eval { tab_id, js, done } => {
                self.start_remote_eval(tab_id, &js, done);
            }
            crate::remote::RemoteCommand::Cdp {
                tab_id,
                method,
                params,
                done,
            } => self.start_remote_cdp(tab_id, &method, &params, done),
            crate::remote::RemoteCommand::Shot { tab_id, path, done } => {
                self.start_remote_shot(tab_id, path, done)
            }
        }
    }

    /// Enruta una petición HTTP ya parseada. `None` difiere la respuesta:
    /// el sender de `reply` queda guardado en `pending_remote_evals` hasta
    /// que la página devuelva el resultado por IPC.
    fn handle_remote(
        &mut self,
        request: &crate::remote::Request,
        reply: &Sender<serde_json::Value>,
    ) -> Option<serde_json::Value> {
        use serde_json::json;
        let segments: Vec<&str> = request
            .path
            .split('?')
            .next()
            .unwrap_or("/")
            .trim_matches('/')
            .split('/')
            .collect();
        Some(match (request.method.as_str(), segments.as_slice()) {
            ("GET", ["tabs"]) => {
                let active_id = self.active_tab().map(|t| t.id);
                let tabs: Vec<serde_json::Value> = self
                    .tabs
                    .iter()
                    .map(|t| {
                        json!({
                            "id": t.id,
                            "title": t.title,
                            "url": t.url,
                            "engine": format!("{:?}", t.engine),
                            "active": Some(t.id) == active_id,
                            "loading": t.loading,
                            "suspended": !t.is_live() && !t.is_newtab(),
                        })
                    })
                    .collect();
                json!({"ok": true, "tabs": tabs})
            }
            ("POST", ["tabs"]) => {
                let url = request.body.get("url").and_then(|u| u.as_str());
                match url {
                    Some(url) if is_web_url(url) => {
                        self.pending_new_tabs.push_back(Some(url.to_string()));
                        json!({"ok": true})
                    }
                    _ => json!({"ok": false, "error": "falta una 'url' http(s) válida"}),
                }
            }
            ("POST", ["tabs", id, "navigate"]) => {
                let Ok(tab_id) = id.parse::<u64>() else {
                    return Some(json!({"ok": false, "error": "id de pestaña inválido"}));
                };
                let Some(url) = request.body.get("url").and_then(|u| u.as_str()) else {
                    return Some(json!({"ok": false, "error": "falta 'url'"}));
                };
                self.navigate_tab_by_id(tab_id, url);
                json!({"ok": true})
            }
            (
                "POST",
                [
                    "tabs",
                    id,
                    action @ ("back" | "forward" | "reload" | "close" | "activate" | "text"),
                ],
            ) => {
                let Ok(tab_id) = id.parse::<u64>() else {
                    return Some(json!({"ok": false, "error": "id de pestaña inválido"}));
                };
                match *action {
                    "back" => {
                        self.navigate_history_for_tab(tab_id, -1);
                        json!({"ok": true})
                    }
                    "forward" => {
                        self.navigate_history_for_tab(tab_id, 1);
                        json!({"ok": true})
                    }
                    "reload" => {
                        self.reload_tab_by_id(tab_id);
                        json!({"ok": true})
                    }
                    "close" => {
                        if let Some(index) = self.tab_index_of(tab_id) {
                            self.close_tab(index);
                        }
                        json!({"ok": true})
                    }
                    "activate" => {
                        if let Some(index) = self.tab_index_of(tab_id) {
                            self.active = index;
                            if let Some(t) = self.tabs.get_mut(index) {
                                t.last_active = Instant::now();
                            }
                            self.needs_bounds_sync = true;
                            self.dirty = true;
                        }
                        json!({"ok": true})
                    }
                    "text" => self.remote_tab_text(tab_id, reply.clone())?,
                    _ => unreachable!(),
                }
            }
            (
                "POST",
                [
                    "tabs",
                    id,
                    action @ ("click" | "type" | "press" | "scroll" | "move"),
                ],
            ) => {
                let Ok(tab_id) = id.parse::<u64>() else {
                    return Some(json!({"ok": false, "error": "id de pestaña inválido"}));
                };
                self.remote_input(tab_id, action, &request.body)
            }
            _ => json!({"ok": false, "error": "ruta desconocida"}),
        })
    }

    /// Recarga una pestaña concreta (WebView2 o Wisp), no solo la activa.
    fn reload_tab_by_id(&mut self, tab_id: u64) {
        let Some(tab) = self.tab_by_id(tab_id) else {
            return;
        };
        if tab.is_newtab() {
            return;
        }
        if tab.engine == BrowserEngine::Wisp {
            let url = tab.url.clone();
            self.start_wisp_load(tab_id, url);
        } else if let Some(webview) = &tab.webview {
            let _ = webview.reload();
        }
    }

    /// Texto legible de una pestaña: reader_text en Wisp, innerText vía eval
    /// en WebView2. En WebView2 devuelve `None` y deja `reply` pendiente en
    /// `pending_remote_evals` hasta que la página responda por IPC.
    fn remote_tab_text(
        &mut self,
        tab_id: u64,
        reply: Sender<serde_json::Value>,
    ) -> Option<serde_json::Value> {
        use serde_json::json;
        let Some(tab) = self.tab_by_id(tab_id) else {
            return Some(json!({"ok": false, "error": "pestaña no encontrada"}));
        };
        if tab.engine == BrowserEngine::Wisp {
            if let Some(page) = tab.wisp_html.as_ref() {
                return Some(json!({"ok": true, "result": page.reader_text().unwrap_or_default()}));
            }
            return Some(json!({"ok": false, "error": "la pestaña Wisp aún no tiene página"}));
        }
        if tab.webview.is_none() {
            return Some(
                json!({"ok": false, "error": "pestaña sin motor activo (suspendida o nueva)"}),
            );
        }
        let eval_id = self.next_remote_eval_id;
        self.next_remote_eval_id += 1;
        let js = "document.body ? document.body.innerText.slice(0, 200000) : ''".to_string();
        let wrapped = remote_eval_script(eval_id, &js);
        let delivered = self
            .tab_by_id(tab_id)
            .and_then(|t| t.webview.as_ref())
            .is_some_and(|webview| webview.evaluate_script(&wrapped).is_ok());
        if delivered {
            if self.pending_remote_evals.len() >= MAX_PENDING_REMOTE_EVALS {
                log::warn!("Límite de evals remotos pendientes alcanzado, limpiando");
                self.pending_remote_evals.clear();
            }
            self.pending_remote_evals.insert(eval_id, reply);
            None
        } else {
            Some(json!({"ok": false, "error": "no se pudo evaluar en la página"}))
        }
    }

    /// Evalúa JS en una pestaña WebView2 y devuelve el resultado por `done`.
    fn start_remote_eval(&mut self, tab_id: u64, js: &str, done: Sender<serde_json::Value>) {
        use serde_json::json;
        let Some(tab) = self.tab_by_id(tab_id) else {
            let _ = done.send(json!({"ok": false, "error": "pestaña no encontrada"}));
            return;
        };
        if tab.engine != BrowserEngine::WebView2 || tab.webview.is_none() {
            let _ = done.send(json!({
                "ok": false,
                "error": "eval solo está disponible en pestañas WebView2; usa /text para Wisp"
            }));
            return;
        }
        let eval_id = self.next_remote_eval_id;
        self.next_remote_eval_id += 1;
        let wrapped = remote_eval_script(eval_id, js);
        let delivered = self
            .tab_by_id(tab_id)
            .and_then(|t| t.webview.as_ref())
            .is_some_and(|webview| webview.evaluate_script(&wrapped).is_ok());
        if delivered {
            if self.pending_remote_evals.len() >= MAX_PENDING_REMOTE_EVALS {
                log::warn!("Límite de evals remotos pendientes alcanzado, limpiando");
                self.pending_remote_evals.clear();
            }
            self.pending_remote_evals.insert(eval_id, done);
        } else {
            let _ = done.send(json!({"ok": false, "error": "no se pudo evaluar en la página"}));
        }
    }

    /// Entrega el resultado de un eval remoto que la página devolvió por IPC.
    fn complete_remote_eval(&mut self, eval_id: u64, result: serde_json::Value) {
        if let Some(done) = self.pending_remote_evals.remove(&eval_id) {
            let _ = done.send(result);
        }
    }

    /// WebView2 vivo de una pestaña (no suspendida, no nueva, motor correcto).
    fn remote_webview(&self, tab_id: u64) -> Option<&wry::WebView> {
        let tab = self.tab_by_id(tab_id)?;
        if tab.engine != BrowserEngine::WebView2 {
            return None;
        }
        tab.webview.as_ref()
    }

    /// Eventos de entrada remotos (click/tecleo/scroll) vía CDP `Input.*`.
    /// Se disparan sin esperar resultado: la API responde `ok` al encolarlos
    /// y el agente verifica el efecto con `/screenshot` o `/eval`.
    fn remote_input(
        &self,
        tab_id: u64,
        action: &str,
        body: &serde_json::Value,
    ) -> serde_json::Value {
        use serde_json::json;
        let Some(webview) = self.remote_webview(tab_id) else {
            return json!({
                "ok": false,
                "error": "la entrada sólo funciona en pestañas WebView2 con página cargada"
            });
        };
        let num = |key: &str| body.get(key).and_then(|v| v.as_f64());
        let events: Vec<(String, String)> = match action {
            "click" => {
                let (Some(x), Some(y)) = (num("x"), num("y")) else {
                    return json!({"ok": false, "error": "click necesita 'x' e 'y'"});
                };
                let button = body
                    .get("button")
                    .and_then(|b| b.as_str())
                    .unwrap_or("left");
                if !matches!(button, "left" | "middle" | "right") {
                    return json!({"ok": false, "error": "button debe ser left|middle|right"});
                }
                let clicks = body
                    .get("clicks")
                    .and_then(|c| c.as_u64())
                    .unwrap_or(1)
                    .clamp(1, 3);
                let pressed = json!({
                    "type": "mousePressed", "x": x, "y": y,
                    "button": button, "clickCount": clicks,
                    "buttons": match button {
                        "left" => 1,
                        "right" => 2,
                        _ => 4,
                    }
                });
                let mut released = pressed.clone();
                released["type"] = json!("mouseReleased");
                released["buttons"] = json!(0);
                vec![
                    (
                        "Input.dispatchMouseEvent".into(),
                        json!({"type": "mouseMoved", "x": x, "y": y}).to_string(),
                    ),
                    ("Input.dispatchMouseEvent".into(), pressed.to_string()),
                    ("Input.dispatchMouseEvent".into(), released.to_string()),
                ]
            }
            "type" => {
                let Some(text) = body.get("text").and_then(|t| t.as_str()) else {
                    return json!({"ok": false, "error": "type necesita 'text'"});
                };
                if text.is_empty() {
                    return json!({"ok": false, "error": "'text' vacío"});
                }
                vec![("Input.insertText".into(), json!({"text": text}).to_string())]
            }
            "press" => {
                let Some(key) = body.get("key").and_then(|k| k.as_str()) else {
                    return json!({"ok": false, "error": "press necesita 'key'"});
                };
                let Some((name, code, vk, text)) = remote_key_params(key) else {
                    return json!({
                        "ok": false,
                        "error": "key no soportada; usa /cdp con Input.dispatchKeyEvent para casos complejos"
                    });
                };
                let mut down = json!({
                    "type": "keyDown", "key": name, "code": code,
                    "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk
                });
                if !text.is_empty() {
                    down["text"] = json!(text);
                }
                let up = json!({
                    "type": "keyUp", "key": name, "code": code,
                    "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk
                });
                vec![
                    ("Input.dispatchKeyEvent".into(), down.to_string()),
                    ("Input.dispatchKeyEvent".into(), up.to_string()),
                ]
            }
            "scroll" => {
                let dx = num("deltaX").unwrap_or(0.0);
                let dy = num("deltaY").unwrap_or(0.0);
                if dx == 0.0 && dy == 0.0 {
                    return json!({"ok": false, "error": "scroll necesita deltaX o deltaY"});
                }
                let mut wheel = json!({"type": "mouseWheel", "deltaX": dx, "deltaY": dy});
                if let Some(x) = num("x") {
                    wheel["x"] = json!(x);
                }
                if let Some(y) = num("y") {
                    wheel["y"] = json!(y);
                }
                vec![("Input.dispatchMouseEvent".into(), wheel.to_string())]
            }
            "move" => {
                let (Some(x), Some(y)) = (num("x"), num("y")) else {
                    return json!({"ok": false, "error": "move necesita 'x' e 'y'"});
                };
                vec![(
                    "Input.dispatchMouseEvent".into(),
                    json!({"type": "mouseMoved", "x": x, "y": y}).to_string(),
                )]
            }
            _ => return json!({"ok": false, "error": "acción de entrada desconocida"}),
        };
        for (method, params) in &events {
            if let Err(e) = crate::cdp::fire(webview, method, params) {
                return json!({"ok": false, "error": e});
            }
        }
        json!({"ok": true, "events": events.len()})
    }

    /// CDP crudo con resultado diferido (el handler de WebView2 responde).
    fn start_remote_cdp(
        &mut self,
        tab_id: u64,
        method: &str,
        params: &str,
        done: Sender<serde_json::Value>,
    ) {
        use serde_json::json;
        let Some(webview) = self.remote_webview(tab_id) else {
            let _ = done.send(
                json!({"ok": false, "error": "CDP sólo funciona en pestañas WebView2 con página cargada"}),
            );
            return;
        };
        let fallback = done.clone();
        if let Err(e) = crate::cdp::call(webview, method, params, done) {
            let _ = fallback.send(json!({"ok": false, "error": e}));
        }
    }

    /// Captura PNG de una pestaña WebView2; guarda el archivo y responde
    /// con su ruta (el binario no cabe en una respuesta JSON).
    fn start_remote_shot(
        &mut self,
        tab_id: u64,
        path: Option<String>,
        done: Sender<serde_json::Value>,
    ) {
        use serde_json::json;
        let Some(webview) = self.remote_webview(tab_id) else {
            let _ = done.send(
                json!({"ok": false, "error": "la captura sólo funciona en pestañas WebView2"}),
            );
            return;
        };
        let path = path
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| default_shot_path(tab_id));
        let path = if path.is_absolute() {
            path
        } else {
            std::env::temp_dir()
                .join("ghost_shots")
                .join(path.file_name().unwrap_or_default())
        };
        let fallback = done.clone();
        if let Err(e) = crate::cdp::screenshot(webview, &path, done) {
            let _ = fallback.send(json!({"ok": false, "error": e}));
        }
    }

    pub fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let t = self.tabs.remove(index);
        self.pending_history_index.remove(&t.id);
        self.spa_probe_at.remove(&t.id);
        self.pending_wisp_requests.remove(&t.id);
        self.pending_wisp_layouts.remove(&t.id);
        self.restore_started.remove(&t.id);
        self.restoring_tabs.remove(&t.id);
        self.pending_restore.retain(|tab_id| *tab_id != t.id);
        // Limpiar texturas Wisp de la pestaña cerrada
        self.cleanup_wisp_textures_for_tab(t.id);
        // Guardar en la pila de pestañas cerradas (para Ctrl+Shift+T).
        self.closed_tabs.push(ClosedTab {
            url: if t.url.trim().is_empty() {
                "ghost://newtab".to_string()
            } else {
                t.url.clone()
            },
            engine: t.engine,
            title: t.title.clone(),
            zoom: t.zoom,
            navigation_history: t.navigation_history.clone(),
            history_index: t.history_index,
        });
        // Mantener solo un historial razonable.
        if self.closed_tabs.len() > 30 {
            self.closed_tabs.remove(0);
        }
        self.store.closed_tabs = self.closed_tabs.clone();

        if self.tabs.is_empty() {
            // Si cerramos la última pestaña, creamos automáticamente una nueva pestaña
            // en blanco para que el navegador no se quede vacío ni se cierre accidentalmente.
            self.request_new_tab();
            self.active = 0;
            self.addr_text.clear();
            self.needs_bounds_sync = true;
            self.dirty = true;
            self.save_session_immediate();
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if self.active > index {
            self.active -= 1;
        }
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.last_active = Instant::now();
        }
        // Al cerrar la pestaña activa, la siguiente puede estar suspendida:
        // hay que recrearla para que el navegador no se quede en blanco.
        let active_id = self.tabs.get(self.active).map(|t| (t.id, t.is_live()));
        if let Some((id, false)) = active_id {
            if let Some(t) = self.tab_by_id_mut(id) {
                t.loading = true;
            }
            self.queue_restore(id);
            self.set_status("Restaurando pestaña…".to_string());
        }
        self.needs_bounds_sync = true;
        self.dirty = true;
        self.save_session_immediate();
        self.sync_addr_text();
    }

    /// Reabre la última pestaña cerrada (Ctrl+Shift+T), restaurando su URL y zoom.
    pub fn restore_last_closed(&mut self) -> bool {
        let Some(closed) = self.closed_tabs.pop() else {
            self.set_status("No hay pestañas cerradas recientemente".to_string());
            return false;
        };
        self.store.closed_tabs = self.closed_tabs.clone();
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let (navigation_history, history_index) = normalized_navigation_history(
            &closed.navigation_history,
            closed.history_index,
            &closed.url,
        );
        let tab = Tab {
            id,
            engine: closed.engine,
            webview: None,
            wisp_html: None,
            wisp_layout: None,
            wisp_layout_width: None,
            wisp_error: None,
            wisp_reader: None,
            engine_manual: false,
            title: closed.title,
            url: closed.url.clone(),
            loading: true,
            zoom: closed.zoom,
            navigation_history,
            history_index,
            last_active: Instant::now(),
            pending_url: None,
            ephemeral: false,
        };
        self.add_tab(tab);
        if webview::is_newtab_url(&closed.url) {
            // La nueva pestaña es nativa: no hay nada que cargar.
            if let Some(t) = self.tab_by_id_mut(id) {
                t.loading = false;
            }
        } else if self
            .tab_by_id(id)
            .is_some_and(|tab| tab.engine == BrowserEngine::Wisp)
        {
            self.start_wisp_load(id, closed.url);
        } else {
            self.queue_restore(id);
        }
        self.set_status("Pestaña restaurada".to_string());
        self.needs_bounds_sync = true;
        self.dirty = true;
        self.save_session_immediate();
        self.sync_addr_text();
        true
    }

    pub fn switch_tab(&mut self, index: usize) {
        if index >= self.tabs.len() || index == self.active {
            return;
        }
        self.active = index;
        let (id, live) = {
            let Some(t) = self.tabs.get(index) else {
                return;
            };
            // La nueva pestaña nativa no tiene proceso: cuenta como viva.
            (t.id, t.is_live() || t.is_newtab())
        };
        if let Some(t) = self.tabs.get_mut(index) {
            t.last_active = Instant::now();
        }
        if !live {
            if let Some(t) = self.tabs.get_mut(index) {
                t.loading = true;
            }
            self.queue_restore(id);
            self.set_status("Restaurando pestaña…".to_string());
        }
        self.needs_bounds_sync = true;
        self.dirty = true;
        self.save_session_immediate();
        self.sync_addr_text();
    }

    pub fn cycle_tab(&mut self, forward: bool) {
        if self.tabs.len() < 2 {
            return;
        }
        let next = if forward {
            (self.active + 1) % self.tabs.len()
        } else {
            (self.active + self.tabs.len() - 1) % self.tabs.len()
        };
        self.switch_tab(next);
    }

    pub fn toggle_bookmark_active(&mut self) {
        let Some(t) = self.active_tab() else { return };
        let url = t.url.clone();
        let title = t.display_title();
        if self.store.is_bookmarked(&url) {
            if let Some(i) = self.store.bookmarks.iter().position(|b| b.url == url) {
                self.store.remove_bookmark(i);
                self.set_status("Marcador eliminado".to_string());
            }
        } else {
            self.store.add_bookmark(title, url);
            self.set_status("Marcador añadido".to_string());
        }
    }

    /// Guarda las pestañas abiertas para restaurarlas en la próxima sesión.
    /// La pestaña de inicio efímera y las pestañas en blanco no se guardan:
    /// cada arranque crea una pestaña de inicio nueva.
    pub fn capture_session(&mut self) {
        let mut session = Vec::with_capacity(self.tabs.len());
        let mut active = 0;
        for (i, t) in self.tabs.iter().enumerate() {
            if t.ephemeral || t.url.trim().is_empty() || webview::is_newtab_url(&t.url) {
                continue;
            }
            if i == self.active {
                active = session.len();
            }
            session.push(SessionTab {
                url: t.url.clone(),
                engine: t.engine,
                title: t.title.clone(),
                zoom: t.zoom,
                navigation_history: t.navigation_history.clone(),
                history_index: t.history_index,
            });
        }
        session.extend(self.unloaded_session.iter().cloned());
        let active = if session.is_empty() {
            0
        } else {
            active.min(session.len() - 1)
        };
        self.store.set_session(session, active);
    }

    /// Guarda la sesión inmediatamente a disco tras cualquier cambio en las pestañas,
    /// para que un cierre forzado (cuelgue, corte de luz, fin de proceso) no las pierda.
    pub fn save_session_immediate(&mut self) {
        self.capture_session();
        self.store.save();
    }

    /// Añade la página actual a marcadores (sin borrarla si ya estaba).
    pub fn add_bookmark_current(&mut self) {
        let Some(t) = self.active_tab() else { return };
        let url = t.url.clone();
        if url.is_empty() || webview::is_newtab_url(&url) {
            self.set_status("La página de inicio no se guarda en marcadores".to_string());
            return;
        }
        if self.store.is_bookmarked(&url) {
            self.set_status("Ya está en marcadores".to_string());
            return;
        }
        let title = t.display_title();
        self.store.add_bookmark(title, url);
        self.set_status("Marcador añadido".to_string());
    }

    pub fn active_is_bookmarked(&self) -> bool {
        self.active_tab()
            .map(|t| self.store.is_bookmarked(&t.url))
            .unwrap_or(false)
    }

    pub fn apply_shortcut(&mut self, tab_id: u64, action: &str) {
        // Acciones que no dependen de una pestaña concreta.
        match action {
            "new_tab" => return self.request_new_tab(),
            "restore_last_closed" => {
                self.restore_last_closed();
                return;
            }
            "next_tab" => return self.cycle_tab(true),
            "prev_tab" => return self.cycle_tab(false),
            "focus_url" => {
                self.addr_focus_request = true;
                return;
            }
            "bookmarks" => {
                self.panel = if self.panel == Panel::Bookmarks {
                    Panel::None
                } else {
                    Panel::Bookmarks
                };
                return;
            }
            "history" => {
                self.panel = if self.panel == Panel::History {
                    Panel::None
                } else {
                    Panel::History
                };
                return;
            }
            "toggle_engine" => {
                self.toggle_engine_active();
                return;
            }
            "reader" => {
                if self.active_can_read() {
                    self.panel = if self.panel == Panel::Reader {
                        Panel::None
                    } else {
                        Panel::Reader
                    };
                }
                return;
            }
            _ => {}
        }
        let Some(index) = self.tab_index_of(tab_id) else {
            return;
        };
        if action == "close_tab" {
            self.close_tab(index);
            return;
        }
        if action == "bookmark" {
            if index == self.active {
                self.toggle_bookmark_active();
            }
            return;
        }
        if self
            .tab_by_id(tab_id)
            .is_some_and(|tab| tab.engine == BrowserEngine::Wisp)
        {
            match action {
                "back" => {
                    self.navigate_history_for_tab(tab_id, -1);
                    return;
                }
                "forward" => {
                    self.navigate_history_for_tab(tab_id, 1);
                    return;
                }
                "reload" => {
                    if let Some(tab) = self.tab_by_id(tab_id) {
                        let url = tab.url.clone();
                        self.start_wisp_load(tab_id, url);
                    }
                    return;
                }
                _ => {}
            }
        }
        // El resto necesita un webview vivo.
        let live = self.ensure_live(tab_id);
        match action {
            "back" | "forward" | "reload" => {
                // En la nueva pestaña nativa no hay webview: el historial se
                // recorre con la navegación propia de Ghost.
                if self.tab_by_id(tab_id).is_some_and(Tab::is_newtab) {
                    match action {
                        "back" => self.navigate_history_for_tab(tab_id, -1),
                        "forward" => self.navigate_history_for_tab(tab_id, 1),
                        _ => {}
                    }
                    return;
                }
                if !live {
                    return;
                }
                if let Some(t) = self.tabs.get_mut(index)
                    && let Some(webview) = &t.webview
                {
                    let _ = match action {
                        "back" => webview.go_back(),
                        "forward" => webview.go_forward(),
                        _ => webview.reload(),
                    };
                }
            }
            "zoom_in" | "zoom_out" | "zoom_reset" => {
                if let Some(t) = self.tabs.get_mut(index) {
                    t.zoom = match action {
                        "zoom_in" => (t.zoom + 0.1).min(5.0),
                        "zoom_out" => (t.zoom - 0.1).max(0.25),
                        _ => 1.0,
                    };
                    let zoom = t.zoom;
                    if let Some(webview) = &t.webview {
                        let _ = webview.zoom(zoom);
                    }
                }
                self.dirty = true;
                self.save_session_immediate();
            }
            "print" => {
                if !live {
                    return;
                }
                if let Some(t) = self.tabs.get_mut(index)
                    && let Some(webview) = &t.webview
                {
                    let _ = webview.print();
                }
            }
            "devtools" => {
                if !live {
                    return;
                }
                if let Some(t) = self.tabs.get_mut(index)
                    && let Some(webview) = &t.webview
                {
                    if webview.is_devtools_open() {
                        webview.close_devtools();
                    } else {
                        webview.open_devtools();
                    }
                }
            }
            _ => {}
        }
    }

    /// Suspende pestañas inactivas para liberar memoria. Devuelve true si
    /// cambió algo (hay que repintar).
    pub fn maintain(&mut self, now: Instant) -> bool {
        if !self.store.settings.memory_saver {
            return false;
        }
        let after = Duration::from_secs(self.store.settings.suspend_after_secs.max(20));
        let active = self.active;
        let mut changed = false;

        // 1) Suspender por inactividad.
        for i in 0..self.tabs.len() {
            if i == active || self.tabs[i].webview.is_none() {
                continue;
            }
            if now.saturating_duration_since(self.tabs[i].last_active) >= after {
                self.tabs[i].webview = None;
                self.suspended_this_session += 1;
                changed = true;
            }
        }

        // 2) Respetar el máximo de pestañas vivas (la más antigua se suspende).
        let max_live = self.store.settings.max_live_tabs.max(1);
        while self.live_count() > max_live {
            let victim = self
                .tabs
                .iter()
                .enumerate()
                .filter(|(i, t)| *i != active && t.webview.is_some())
                .min_by_key(|(_, t)| t.last_active)
                .map(|(i, _)| i);
            match victim {
                Some(i) => {
                    self.tabs[i].webview = None;
                    self.suspended_this_session += 1;
                    changed = true;
                }
                None => break,
            }
        }

        if changed {
            self.dirty = true;
        }
        changed
    }

    /// Tiempo hasta la próxima suspensión o timeout (para despertar el bucle).
    pub fn next_wakeup(&self) -> Option<Duration> {
        let now = Instant::now();
        let restore_delay = self
            .restore_started
            .values()
            .map(|started| RESTORE_TIMEOUT.saturating_sub(now.saturating_duration_since(*started)))
            .min();
        let other_delay = if self.splash {
            let end = if self.splash_page_loaded {
                self.splash_until
            } else {
                self.splash_until + Duration::from_secs(4)
            };
            Some(
                end.saturating_duration_since(now)
                    .max(Duration::from_millis(16)),
            )
        } else if self.store.settings.memory_saver {
            let after = Duration::from_secs(self.store.settings.suspend_after_secs.max(20));
            self.tabs
                .iter()
                .enumerate()
                .filter(|(i, t)| *i != self.active && t.webview.is_some())
                .map(|(_, t)| after.saturating_sub(t.last_active.elapsed()))
                .min()
                .map(|d| d.max(Duration::from_secs(1)))
        } else {
            None
        };
        restore_delay.into_iter().chain(other_delay).min()
    }

    pub fn set_settings(&mut self, settings: Settings) {
        let old = self.store.settings.clone();
        let block_ads = settings.block_ads;
        let freeze_media = settings.freeze_background_media;
        let dark_changed = old.dark_mode != settings.dark_mode;
        let privacy_changed =
            old.block_ads != block_ads || old.freeze_background_media != freeze_media;
        self.store.update_settings(settings);
        if privacy_changed {
            self.block_ads.store(block_ads, Ordering::Relaxed);
            for tab in &self.tabs {
                if let Some(webview) = &tab.webview {
                    webview::set_privacy_flags(webview, block_ads, freeze_media);
                }
            }
        }
        if dark_changed {
            self.dark_mode_changed = true;
        }
        let path = default_download_dir(&self.store);
        if let Ok(mut guard) = self.download_dir.write() {
            *guard = path;
        }
        self.dirty = true;
    }

    pub fn clear_browsing_data(&mut self) {
        let Some(t) = self.active_tab() else { return };
        let Some(webview) = &t.webview else {
            self.set_status("Abre una pestaña activa para borrar los datos".to_string());
            return;
        };
        match webview.clear_all_browsing_data() {
            Ok(()) => self.set_status("Datos de navegación borrados".to_string()),
            Err(e) => self.set_status(format!("Error al borrar datos: {e}")),
        }
    }

    pub fn can_go_back(&self) -> bool {
        self.active_tab().is_some_and(|tab| {
            tab.history_index > 0
                || tab
                    .webview
                    .as_ref()
                    .and_then(|webview| webview.can_go_back().ok())
                    .unwrap_or(false)
        })
    }

    pub fn can_go_forward(&self) -> bool {
        self.active_tab().is_some_and(|tab| {
            tab.history_index + 1 < tab.navigation_history.len()
                || tab
                    .webview
                    .as_ref()
                    .and_then(|webview| webview.can_go_forward().ok())
                    .unwrap_or(false)
        })
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
}

fn default_download_dir(store: &Store) -> PathBuf {
    store
        .settings
        .download_dir
        .as_ref()
        .map(PathBuf::from)
        .or_else(dirs::download_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La franja superior debe dejar libre la banda de redimensionado y, aun
    /// así, tener sitio para las dos filas (pestañas/controles y barra de
    /// direcciones) sin que el contenido se salga del panel.
    #[test]
    fn el_chrome_cabe_bajo_la_banda_de_redimensionado() {
        assert_eq!(CHROME_TOP_INSET, crate::webview::RESIZE_GUTTER as f32);
        let contenido = UI_HEIGHT - CHROME_TOP_INSET - 2.0;
        assert!(
            contenido >= 66.0,
            "el chrome necesita 66 puntos de contenido (32 + 2 + 32) y mide {contenido}"
        );
    }

    fn tab(id: u64, url: &str) -> Tab {
        Tab {
            id,
            engine: BrowserEngine::WebView2,
            webview: None,
            wisp_html: None,
            wisp_layout: None,
            wisp_layout_width: None,
            wisp_error: None,
            wisp_reader: None,
            engine_manual: false,
            title: url.to_string(),
            url: url.to_string(),
            loading: false,
            zoom: 1.0,
            navigation_history: vec![url.to_string()],
            history_index: 0,
            last_active: Instant::now(),
            pending_url: None,
            ephemeral: false,
        }
    }

    #[test]
    fn wisp_can_be_selected_for_one_tab_without_changing_the_default() {
        let (mut browser, _) = Browser::from_store(Store::default());
        let tab_id = browser.active_tab().unwrap().id;

        browser.set_tab_engine(tab_id, BrowserEngine::Wisp);

        let tab = browser.tab_by_id(tab_id).unwrap();
        assert_eq!(tab.engine, BrowserEngine::Wisp);
        // La pestaña está en la nueva pestaña nativa: el cambio de motor solo
        // cambia la etiqueta; la carga real ocurre al navegar a una URL.
        assert!(tab.is_newtab());
        assert!(tab.webview.is_none());
        assert!(tab.wisp_html.is_none());
        assert_eq!(
            browser.store.settings.default_engine,
            BrowserEngine::WebView2
        );
    }

    #[test]
    fn un_error_de_wisp_conmuta_a_webview2_sin_marcar_motor_manual() {
        let (mut browser, _tx) = Browser::from_store(Store::default());
        let tab_id = browser.active_tab().unwrap().id;
        {
            let t = browser.tab_by_id_mut(tab_id).unwrap();
            t.engine = BrowserEngine::Wisp;
            t.url = "https://example.com/".into();
        }
        browser.start_wisp_load(tab_id, "https://example.com/".into());
        let request_id = browser.pending_wisp_requests[&tab_id];

        browser.handle_wisp_page_loaded(tab_id, request_id, Box::new(Err("boom".into())));

        let t = browser.tab_by_id(tab_id).unwrap();
        assert_eq!(t.engine, BrowserEngine::WebView2);
        assert!(!t.engine_manual);
        assert!(browser.pending_restore.contains(&tab_id));
    }

    #[test]
    fn un_error_de_wisp_en_pestana_manual_no_conmuta() {
        let (mut browser, _tx) = Browser::from_store(Store::default());
        let tab_id = browser.active_tab().unwrap().id;
        {
            let t = browser.tab_by_id_mut(tab_id).unwrap();
            t.engine = BrowserEngine::Wisp;
            t.engine_manual = true;
            t.url = "https://example.com/".into();
        }
        browser.start_wisp_load(tab_id, "https://example.com/".into());
        let request_id = browser.pending_wisp_requests[&tab_id];

        browser.handle_wisp_page_loaded(tab_id, request_id, Box::new(Err("boom".into())));

        let t = browser.tab_by_id(tab_id).unwrap();
        assert_eq!(t.engine, BrowserEngine::Wisp);
        assert!(t.wisp_error.is_some());
    }

    #[test]
    fn navegar_a_inicio_nativo_limpia_el_motor_y_guarda_historial() {
        let (mut browser, _) = Browser::from_store(Store::default());
        let tab_id = browser.active_tab().unwrap().id;
        {
            let t = browser.tab_by_id_mut(tab_id).unwrap();
            t.url = "https://example.com/".into();
            t.loading = true;
            t.wisp_error = Some("fallo".into());
            t.pending_url = Some("https://pending.example/".into());
        }

        browser.navigate_active("ghost://newtab");

        let t = browser.tab_by_id(tab_id).unwrap();
        assert!(t.is_newtab());
        assert!(!t.loading);
        assert!(t.wisp_error.is_none());
        assert!(t.pending_url.is_none());
        assert!(t.webview.is_none());
        assert!(
            t.navigation_history
                .iter()
                .any(|url| url == "ghost://newtab")
        );
        // La nueva pestaña nativa no se cuenta como suspendida ni restaurable.
        assert_eq!(browser.suspended_count(), 0);
        assert!(!browser.pending_restore.contains(&tab_id));
    }

    #[test]
    fn new_home_tab_uses_the_saved_default_engine() {
        let mut store = Store::default();
        store.settings.default_engine = BrowserEngine::Wisp;

        let (browser, _) = Browser::from_store(store);

        assert_eq!(browser.active_tab().unwrap().engine, BrowserEngine::Wisp);
    }

    #[test]
    fn el_arranque_abre_inicio_y_deja_la_sesion_suspendida() {
        let mut store = Store::default();
        store.session = vec![
            SessionTab {
                url: "https://rust-lang.org/".into(),
                engine: BrowserEngine::WebView2,
                title: "Rust".into(),
                zoom: store.settings.default_zoom.clamp(0.25, 5.0),
                navigation_history: vec!["https://rust-lang.org/".into()],
                history_index: 0,
            },
            SessionTab {
                url: "https://gemini.google.com/app".into(),
                engine: BrowserEngine::WebView2,
                title: "Gemini".into(),
                zoom: store.settings.default_zoom.clamp(0.25, 5.0),
                navigation_history: vec!["https://gemini.google.com/app".into()],
                history_index: 0,
            },
        ];
        // Aunque la sesión anterior estuviera en Gemini, el arranque no la carga.
        store.session_active = 1;

        let (browser, _) = Browser::from_store(store);
        assert_eq!(browser.tabs.len(), 3);
        assert_eq!(browser.active, 0);
        assert!(browser.tabs[0].ephemeral);
        assert_eq!(browser.tabs[0].url, "ghost://newtab");
        assert!(browser.tabs[0].is_newtab());
        assert!(browser.tabs[1..].iter().all(|t| !t.is_live()));
        // La nueva pestaña es nativa: no se encola ninguna restauración y la
        // sesión anterior queda suspendida hasta que el usuario la elija.
        assert!(browser.pending_restore.is_empty());
        assert!(
            !browser.pending_restore.iter().any(|id| browser
                .tab_by_id(*id)
                .is_some_and(|t| t.url.contains("rust-lang"))),
            "la sesión anterior no debe cargarse sola al arrancar"
        );
    }

    #[test]
    fn cerrar_la_activa_restaura_la_siguiente_suspendida() {
        let (mut browser, _) = Browser::from_store(Store::default());
        browser.tabs.clear();
        browser.pending_restore.clear();
        browser.closed_tabs.clear();
        browser.tabs.push(tab(1, "https://a.example"));
        browser.tabs.push(tab(2, "https://b.example"));
        browser.tabs.push(tab(3, "https://c.example"));
        browser.active = 1;

        browser.close_tab(1);

        assert_eq!(browser.tabs.len(), 2);
        assert_eq!(browser.tabs[browser.active].id, 3);
        assert_eq!(browser.tabs[browser.active].url, "https://c.example");
        assert_eq!(browser.pending_restore.front().copied(), Some(3));
        assert!(browser.status().starts_with("Restaurando"));
    }

    #[test]
    fn los_mensajes_de_pagina_no_pueden_falsificar_la_url_ni_cerrar_la_pestana() {
        let (mut browser, _) = Browser::from_store(Store::default());
        browser.splash = false;
        browser.tabs.clear();
        browser.tabs.push(tab(1, "https://safe.example/page"));
        browser.active = 0;

        browser.handle_ipc(
            1,
            "https://safe.example/route",
            r#"{"type":"nav","url":"https://spoofed.example/","navigation":"replace"}"#,
        );
        assert_eq!(browser.tabs[0].url, "https://safe.example/route");

        browser.handle_ipc(
            1,
            "https://attacker.example/",
            r#"{"type":"shortcut","action":"close_tab"}"#,
        );
        assert_eq!(browser.tabs.len(), 1);
        assert_eq!(browser.tabs[0].url, "https://safe.example/route");
    }

    #[test]
    fn el_historial_de_navegacion_se_guarda_y_se_restauran_sus_indices() {
        let (mut browser, _) = Browser::from_store(Store::default());
        browser.tabs.clear();
        let mut current = tab(1, "https://c.example/");
        current.navigation_history = vec![
            "https://a.example/".into(),
            "https://b.example/".into(),
            "https://c.example/".into(),
        ];
        current.history_index = 2;
        browser.tabs.push(current);
        browser.active = 0;

        browser.capture_session();
        let saved = browser.store.session.first().unwrap();
        assert_eq!(saved.navigation_history.len(), 3);
        assert_eq!(saved.history_index, 2);

        let (restored, _) = Browser::from_store(browser.store);
        let restored_tab = restored
            .tabs
            .iter()
            .find(|tab| tab.url == "https://c.example/")
            .unwrap();
        assert_eq!(restored_tab.history_index, 2);
        assert_eq!(restored_tab.navigation_history[1], "https://b.example/");
    }

    #[test]
    fn retroceder_en_una_pestana_suspendida_usa_el_historial_guardado() {
        let (mut browser, _) = Browser::from_store(Store::default());
        browser.splash = false;
        browser.tabs.clear();
        let mut current = tab(1, "https://c.example/");
        current.navigation_history = vec![
            "https://a.example/".into(),
            "https://b.example/".into(),
            "https://c.example/".into(),
        ];
        current.history_index = 2;
        browser.tabs.push(current);
        browser.active = 0;

        browser.go_back();

        assert_eq!(browser.pending_history_index.get(&1), Some(&1));
        assert_eq!(
            browser.tab_by_id(1).unwrap().pending_url.as_deref(),
            Some("https://b.example/")
        );
        browser.record_navigation(1, "https://b.example/", "push");
        let tab = browser.tab_by_id(1).unwrap();
        assert_eq!(tab.history_index, 1);
        assert_eq!(tab.navigation_history.len(), 3);
        assert!(browser.can_go_forward());
    }

    #[test]
    fn guardar_la_sesion_preserva_pestanas_mas_alla_del_limite_visible() {
        let mut store = Store::default();
        store.session = (0..=MAX_RESTORED_TABS)
            .map(|index| SessionTab {
                url: format!("https://example{index}.com/"),
                engine: BrowserEngine::WebView2,
                title: format!("Pestaña {index}"),
                zoom: store.settings.default_zoom.clamp(0.25, 5.0),
                navigation_history: Vec::new(),
                history_index: 0,
            })
            .collect();
        let (mut browser, _) = Browser::from_store(store);
        assert_eq!(browser.unloaded_session.len(), 1);

        browser.capture_session();

        assert_eq!(browser.store.session.len(), MAX_RESTORED_TABS + 1);
        assert_eq!(
            browser.store.session.last().unwrap().url,
            format!("https://example{MAX_RESTORED_TABS}.com/")
        );
    }

    #[test]
    fn una_restauracion_atascada_libera_el_overlay_tras_el_timeout() {
        let (mut browser, _) = Browser::from_store(Store::default());
        let started = Instant::now();
        let tab_id = browser.tabs[0].id;
        browser.restoring_tabs.insert(tab_id);
        browser.restore_started.insert(tab_id, started);
        browser.status = "Restaurando pestaña…".to_string();

        assert!(!browser.tick_restores(started + RESTORE_TIMEOUT - Duration::from_millis(1)));
        assert!(browser.is_restoring(tab_id));
        assert!(browser.tick_restores(started + RESTORE_TIMEOUT));
        assert!(!browser.is_restoring(tab_id));
        assert!(!browser.tab_by_id(tab_id).unwrap().loading);
        assert!(browser.restore_elapsed(tab_id).is_none());
        assert!(browser.needs_bounds_sync);
        assert_eq!(browser.status(), "Listo");
    }

    #[test]
    fn cancelar_una_restauracion_limpia_su_timeout() {
        let (mut browser, _) = Browser::from_store(Store::default());
        let tab_id = browser.tabs[0].id;
        browser.restoring_tabs.insert(tab_id);
        browser.restore_started.insert(tab_id, Instant::now());

        browser.cancel_restore(tab_id);

        assert!(!browser.is_restoring(tab_id));
        assert!(browser.restore_elapsed(tab_id).is_none());
        assert!(!browser.tick_restores(Instant::now() + RESTORE_TIMEOUT));
    }

    #[test]
    fn un_fallo_de_creacion_permite_reintentar_la_restauracion() {
        let (mut browser, _) = Browser::from_store(Store::default());
        browser.splash = false;
        let tab_id = browser.tabs[0].id;
        // La pestaña de inicio es la nueva pestaña nativa (sin proceso que
        // restaurar): se le da una URL real para ejercitar la restauración.
        if let Some(t) = browser.tab_by_id_mut(tab_id) {
            t.url = "https://example.com/".into();
        }
        browser.queue_restore(tab_id);
        browser.pending_restore.clear();

        browser.fail_restore(tab_id);

        assert!(!browser.is_restoring(tab_id));
        assert!(!browser.tab_by_id(tab_id).unwrap().loading);
        assert_eq!(browser.status(), "No se pudo restaurar la pestaña");
        assert!(browser.retry_restore(tab_id));
        assert!(browser.is_restoring(tab_id));
        assert!(browser.tab_by_id(tab_id).unwrap().loading);
        assert_eq!(browser.pending_restore.front(), Some(&tab_id));
    }

    #[test]
    fn el_bucle_programa_un_despertar_para_el_timeout_de_restauracion() {
        let (mut browser, _) = Browser::from_store(Store::default());
        browser.splash = false;
        browser.store.settings.memory_saver = false;
        let tab_id = browser.tabs[0].id;
        browser.restore_started.insert(tab_id, Instant::now());

        assert!(
            browser
                .next_wakeup()
                .is_some_and(|delay| delay <= RESTORE_TIMEOUT)
        );
    }

    #[test]
    fn la_sesion_no_guarda_la_pestana_de_inicio_ni_las_blank() {
        let (mut browser, _) = Browser::from_store(Store::default());
        browser.tabs.clear();
        browser.pending_restore.clear();
        browser.closed_tabs.clear();
        browser.tabs.push(tab(1, "https://rust-lang.org/"));
        browser.tabs.push(Tab {
            ephemeral: true,
            ..tab(2, "ghost://newtab")
        });
        browser.active = 0;

        browser.capture_session();

        assert_eq!(browser.store.session.len(), 1);
        assert_eq!(browser.store.session[0].url, "https://rust-lang.org/");
    }

    #[test]
    fn test_close_and_restore_tab() {
        let (mut browser, _) = Browser::from_store(Store::default());
        browser.tabs.clear();
        browser.pending_restore.clear();
        browser.closed_tabs.clear();
        browser.active = 0;

        browser.tabs.push(Tab {
            id: 1,
            engine: BrowserEngine::WebView2,
            webview: None,
            wisp_html: None,
            wisp_layout: None,
            wisp_layout_width: None,
            wisp_error: None,
            wisp_reader: None,
            engine_manual: false,
            title: "Pestaña 1".into(),
            url: "https://example.com".into(),
            loading: false,
            zoom: 1.5,
            navigation_history: vec!["https://example.com".into()],
            history_index: 0,
            last_active: Instant::now(),
            pending_url: None,
            ephemeral: false,
        });
        browser.tabs.push(Tab {
            id: 2,
            engine: BrowserEngine::WebView2,
            webview: None,
            wisp_html: None,
            wisp_layout: None,
            wisp_layout_width: None,
            wisp_error: None,
            wisp_reader: None,
            engine_manual: false,
            title: "Pestaña 2".into(),
            url: "https://rust-lang.org".into(),
            loading: false,
            zoom: 1.2,
            navigation_history: vec!["https://example.net".into(), "https://rust-lang.org".into()],
            history_index: 1,
            last_active: Instant::now(),
            pending_url: None,
            ephemeral: false,
        });
        browser.active = 1;

        // Cerrar pestaña 1 (Pestaña 2)
        browser.close_tab(1);
        assert_eq!(browser.tabs.len(), 1);
        assert_eq!(browser.active, 0);
        // La pestaña que queda estaba suspendida: debe encolarse su restauración.
        assert_eq!(browser.pending_restore.front().copied(), Some(1));
        assert_eq!(browser.closed_tabs.len(), 1);
        assert_eq!(browser.closed_tabs[0].url, "https://rust-lang.org");
        assert_eq!(browser.closed_tabs[0].zoom, 1.2);

        // Reabrir con restore_last_closed (Ctrl+Shift+T)
        let ok = browser.restore_last_closed();
        assert!(ok);
        assert_eq!(browser.tabs.len(), 2);
        let restored = &browser.tabs[1];
        assert_eq!(restored.url, "https://rust-lang.org");
        assert_eq!(restored.title, "Pestaña 2");
        assert_eq!(restored.zoom, 1.2);
        assert_eq!(restored.navigation_history.len(), 2);
        assert_eq!(restored.history_index, 1);
        assert_eq!(browser.closed_tabs.len(), 0);
    }

    #[test]
    fn test_close_last_tab_does_not_crash_and_queues_new_tab() {
        let (mut browser, _) = Browser::from_store(Store::default());
        browser.tabs.clear();
        browser.pending_new_tabs.clear();
        browser.pending_restore.clear();
        browser.closed_tabs.clear();

        browser.tabs.push(Tab {
            id: 1,
            engine: BrowserEngine::WebView2,
            webview: None,
            wisp_html: None,
            wisp_layout: None,
            wisp_layout_width: None,
            wisp_error: None,
            wisp_reader: None,
            engine_manual: false,
            title: "Última".into(),
            url: "https://google.com".into(),
            loading: false,
            zoom: 1.1,
            navigation_history: vec!["https://google.com".into()],
            history_index: 0,
            last_active: Instant::now(),
            pending_url: None,
            ephemeral: false,
        });

        browser.close_tab(0);
        assert_eq!(browser.tabs.len(), 0);
        assert_eq!(browser.pending_new_tabs.len(), 1);
        assert!(browser.is_dirty());
        assert_eq!(browser.closed_tabs.len(), 1);
        assert_eq!(browser.closed_tabs[0].url, "https://google.com");
        assert_eq!(browser.closed_tabs[0].zoom, 1.1);

        // Funciona reabrir incluso si se había cerrado la última
        let ok = browser.restore_last_closed();
        assert!(ok);
        assert_eq!(browser.tabs.len(), 1);
        assert_eq!(browser.tabs[0].url, "https://google.com");
        assert_eq!(browser.tabs[0].zoom, 1.1);
    }

    #[test]
    fn remote_api_lists_tabs_and_rejects_bad_routes() {
        let (mut browser, _) = Browser::for_tests();

        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        browser.handle_remote_command(crate::remote::RemoteCommand::Sync {
            request: crate::remote::Request {
                method: "GET".into(),
                path: "/tabs".into(),
                body: serde_json::Value::Null,
            },
            reply: reply_tx,
        });
        let response = reply_rx.recv().unwrap();
        assert_eq!(response["ok"], true);
        assert!(!response["tabs"].as_array().unwrap().is_empty());

        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        browser.handle_remote_command(crate::remote::RemoteCommand::Sync {
            request: crate::remote::Request {
                method: "GET".into(),
                path: "/nope".into(),
                body: serde_json::Value::Null,
            },
            reply: reply_tx,
        });
        assert_eq!(reply_rx.recv().unwrap()["ok"], false);

        // Eval en una pestaña sin WebView2 vivo debe fallar con mensaje claro.
        let tab_id = browser.tabs[0].id;
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        browser.handle_remote_command(crate::remote::RemoteCommand::Eval {
            tab_id,
            js: "1+1".into(),
            done: done_tx,
        });
        let response = done_rx.recv().unwrap();
        assert_eq!(response["ok"], false);
    }
}
