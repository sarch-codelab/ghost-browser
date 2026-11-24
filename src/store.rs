use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserEngine {
    #[default]
    WebView2,
    Wisp,
}

impl BrowserEngine {
    pub fn label(self) -> &'static str {
        match self {
            Self::WebView2 => "WebView2",
            Self::Wisp => "Wisp (Beta)",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bookmark {
    pub title: String,
    pub url: String,
}

/// Pestaña guardada entre sesiones (se restaura al abrir Ghost).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTab {
    pub url: String,
    #[serde(default)]
    pub engine: BrowserEngine,
    #[serde(default)]
    pub title: String,
    #[serde(default = "default_zoom")]
    pub zoom: f64,
    #[serde(default)]
    pub navigation_history: Vec<String>,
    #[serde(default)]
    pub history_index: usize,
}

/// Pestaña cerrada recientemente (se guarda entre sesiones para Ctrl+Shift+T).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosedTab {
    pub url: String,
    #[serde(default)]
    pub engine: BrowserEngine,
    #[serde(default)]
    pub title: String,
    #[serde(default = "default_zoom")]
    pub zoom: f64,
    #[serde(default)]
    pub navigation_history: Vec<String>,
    #[serde(default)]
    pub history_index: usize,
}

fn default_zoom() -> f64 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub url: String,
    pub title: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub dark_mode: bool,
    pub default_engine: BrowserEngine,
    pub home_page: String,
    pub download_dir: Option<String>,
    /// Suspende (descarta) las pestañas inactivas para liberar RAM.
    pub memory_saver: bool,
    /// Segundos de inactividad antes de suspender una pestaña.
    pub suspend_after_secs: u64,
    /// Máximo de pestañas con webview vivo a la vez.
    pub max_live_tabs: usize,
    /// Bloquea anuncios y rastreadores conocidos antes de cargarlos.
    pub block_ads: bool,
    /// Pausa la reproducción de medios en pestañas de fondo (excepto YouTube Music).
    pub freeze_background_media: bool,
    /// Elige el motor por URL: artículos/docs/wiki abren en Wisp (liviano),
    /// el resto en WebView2. Solo aplica a pestañas donde el usuario no haya
    /// elegido motor a mano.
    pub auto_engine: bool,
    /// Nivel de zoom por defecto para nuevas pestañas (0.25 a 5.0).
    pub default_zoom: f64,
    /// Bloquear pop-ups no solicitados.
    pub block_popups: bool,
    /// Preguntar siempre dónde guardar descargas.
    pub ask_download_location: bool,
    /// Auto-abrir archivos descargados.
    pub auto_open_downloads: bool,
    /// Modo lectura: tamaño de fuente (8 a 32).
    pub reader_font_size: u8,
    /// Modo lectura: tipo de fuente (sans, serif, mono).
    pub reader_font_family: String,
    /// Modo lectura: color de texto (hex RGB).
    pub reader_text_color: String,
    /// Modo lectura: color de fondo (hex RGB).
    pub reader_bg_color: String,
    /// Nombre del perfil actual.
    pub profile_name: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dark_mode: true,
            default_engine: BrowserEngine::WebView2,
            home_page: "ghost://newtab".to_string(),
            download_dir: None,
            memory_saver: true,
            suspend_after_secs: 60,
            max_live_tabs: 3,
            block_ads: true,
            freeze_background_media: true,
            auto_engine: false,
            default_zoom: 1.0,
            block_popups: true,
            ask_download_location: false,
            auto_open_downloads: false,
            reader_font_size: 16,
            reader_font_family: "sans".to_string(),
            reader_text_color: "#F5F5F5".to_string(),
            reader_bg_color: "#0B0B0B".to_string(),
            profile_name: "Default".to_string(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub bookmarks: Vec<Bookmark>,
    pub history: Vec<HistoryEntry>,
    pub settings: Settings,
    /// Versión del formato de ajustes; sirve para migrar perfiles antiguos.
    #[serde(default)]
    pub settings_version: u32,
    /// Pestañas abiertas al cerrar la aplicación.
    #[serde(default)]
    pub session: Vec<SessionTab>,
    /// Índice de la pestaña que estaba activa.
    #[serde(default)]
    pub session_active: usize,
    /// Historial de pestañas cerradas para Ctrl+Shift+T.
    #[serde(default)]
    pub closed_tabs: Vec<ClosedTab>,
    /// Hay cambios sin escribir a disco.
    #[serde(skip)]
    pending_save: bool,
    /// Momento del último cambio (para agrupar escrituras).
    #[serde(skip)]
    last_change: Option<std::time::Instant>,
    /// Los tests usan un Store en memoria: nunca deben escribir el perfil real.
    #[serde(skip)]
    persist: bool,
}

impl Store {
    fn path() -> PathBuf {
        let dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Ghost");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("ghost.json")
    }

    /// Algunos editores (y PowerShell) guardan el JSON con BOM UTF-8. Sin
    /// quitarlo, serde falla y el perfil se reinterpreta como vacío.
    fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text.trim_start_matches('\u{feff}'))
    }

    pub fn load() -> Self {
        let path = Self::path();
        let (mut store, persist) = match std::fs::read_to_string(&path) {
            Ok(text) => match Self::parse(&text) {
                Ok(store) => (store, true),
                Err(e) => {
                    log::warn!("No se pudo leer {}: {e}", path.display());
                    let backup = path.with_extension("json.bak");
                    let persist = match std::fs::write(&backup, &text) {
                        Ok(()) => true,
                        Err(backup_error) => {
                            log::error!(
                                "No se pudo respaldar {} en {}: {backup_error}",
                                path.display(),
                                backup.display()
                            );
                            false
                        }
                    };
                    (Self::default(), persist)
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Self::default(), true),
            Err(error) => {
                log::error!("No se pudo leer {}: {error}", path.display());
                (Self::default(), false)
            }
        };
        store.persist = persist;
        store.migrate();
        store
    }

    /// Migra perfiles creados antes del perfil agresivo de memoria. Solo toca
    /// los valores que todavía eran los antiguos predeterminados (5 vivas y
    /// 180 s), para no pisar una configuración elegida por el usuario.
    fn migrate(&mut self) {
        if self.settings_version >= 1 {
            return;
        }
        if self.settings.max_live_tabs == 5 && self.settings.suspend_after_secs == 180 {
            self.settings.max_live_tabs = 3;
            self.settings.suspend_after_secs = 60;
        }
        self.settings_version = 1;
        self.save();
    }

    /// Modo efímero (benchmarks con `--no-session`/`--no-saver`): la sesión
    /// cargada se usa en memoria pero `save()` no escribe nunca en disco.
    pub fn make_ephemeral(&mut self) {
        self.persist = false;
    }

    pub fn save(&mut self) {
        self.pending_save = false;
        if !self.persist {
            return;
        }
        let path = Self::path();
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    log::warn!("No se pudo guardar {}: {e}", path.display());
                }
            }
            Err(e) => log::warn!("Error serializando estado: {e}"),
        }
    }

    /// Marca el estado como modificado; la escritura se agrupa en `flush_if_due`.
    fn mark_dirty(&mut self) {
        self.pending_save = true;
        self.last_change = Some(std::time::Instant::now());
    }

    /// Escribe a disco cuando han pasado unos cientos de ms sin cambios, para no
    /// escribir el JSON en cada frame mientras se arrastra un ajuste.
    pub fn flush_if_due(&mut self) {
        if !self.pending_save {
            return;
        }
        let due = self
            .last_change
            .map(|t| t.elapsed().as_millis() >= 600)
            .unwrap_or(true);
        if due {
            self.pending_save = false;
            self.save();
        }
    }

    /// Escribe ahora si quedó algo pendiente (al cerrar la ventana).
    pub fn flush(&mut self) {
        if self.pending_save {
            self.pending_save = false;
            self.save();
        }
    }

    pub fn add_bookmark(&mut self, title: String, url: String) {
        if self.bookmarks.iter().any(|b| b.url == url) {
            return;
        }
        self.bookmarks.push(Bookmark { title, url });
        if self.bookmarks.len() > 200 {
            self.bookmarks.remove(0);
        }
        self.mark_dirty();
    }

    pub fn remove_bookmark(&mut self, index: usize) {
        if index < self.bookmarks.len() {
            self.bookmarks.remove(index);
            self.mark_dirty();
        }
    }

    pub fn is_bookmarked(&self, url: &str) -> bool {
        self.bookmarks.iter().any(|b| b.url == url)
    }

    pub fn add_history(&mut self, url: String, title: String) {
        // Evita duplicar la misma URL consecutiva.
        if let Some(last) = self.history.last()
            && last.url == url
        {
            if last.title.is_empty() && !title.is_empty() {
                let mut last = last.clone();
                last.title = title;
                last.timestamp = now_secs();
                if let Some(slot) = self.history.last_mut() {
                    *slot = last;
                }
                self.mark_dirty();
            }
            return;
        }
        self.history.push(HistoryEntry {
            url,
            title,
            timestamp: now_secs(),
        });
        if self.history.len() > 1000 {
            let excess = self.history.len() - 1000;
            self.history.drain(..excess);
        }
        self.mark_dirty();
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
        self.mark_dirty();
    }

    pub fn recent_history(&self, limit: usize) -> Vec<&HistoryEntry> {
        self.history.iter().rev().take(limit).collect()
    }

    pub fn set_session(&mut self, session: Vec<SessionTab>, active: usize) {
        self.session = session;
        self.session_active = active;
        self.mark_dirty();
    }

    pub fn update_settings(&mut self, settings: Settings) {
        self.settings = settings;
        self.mark_dirty();
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_acepta_un_bom_utf8() {
        let json = serde_json::to_string(&Store::default()).unwrap();
        let store =
            Store::parse(&format!("\u{feff}{json}")).expect("el BOM no debe invalidar el perfil");
        assert_eq!(store.settings.home_page, "ghost://newtab");
        assert_eq!(store.settings.dark_mode, true);
    }

    #[test]
    fn sesiones_antiguas_cargan_sin_historial_de_navegacion() {
        let store = Store::parse(
            r#"{"bookmarks":[],"history":[],"settings":{},"session":[{"url":"https://example.com","title":"Example","zoom":1.0}]}"#,
        )
        .unwrap();
        let tab = &store.session[0];
        assert!(tab.navigation_history.is_empty());
        assert_eq!(tab.history_index, 0);
        assert_eq!(tab.engine, BrowserEngine::WebView2);
        assert_eq!(store.settings.default_engine, BrowserEngine::WebView2);
    }
}
