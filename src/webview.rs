use crate::store::Settings;
use anyhow::Result;
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use wry::http::{Request, Response};
use wry::raw_window_handle::HasWindowHandle;
use wry::{NewWindowResponse, PageLoadEvent, Rect, WebView, WebViewBuilder};

const HOME_BACKGROUND: &[u8] = include_bytes!("../black.webp");
pub const RESIZE_GUTTER: f64 = 6.0;

/// Eventos que los webviews envían al hilo principal de la aplicación.
#[allow(dead_code)]
#[derive(Debug)]
pub enum TabEvent {
    PageStarted {
        tab: u64,
        url: String,
    },
    PageFinished {
        tab: u64,
        url: String,
    },
    TitleChanged {
        tab: u64,
        title: String,
    },
    Ipc {
        tab: u64,
        source_url: String,
        message: String,
    },
    DownloadStarted {
        tab: u64,
        url: String,
        path: PathBuf,
    },
    DownloadFinished {
        url: String,
        path: Option<PathBuf>,
        ok: bool,
    },
    WispPageLoaded {
        tab: u64,
        request_id: u64,
        result: Result<wisp_engine::FetchedPage, String>,
    },
    WispLayoutReady {
        tab: u64,
        request_id: u64,
        result: Result<wisp_engine::LayoutDocument, String>,
    },
}

/// Script base inyectado en cada documento: reenvía navegación SPA, aperturas
/// de ventanas, atajos de teclado del navegador y eventos de título. Las
/// funciones de privacidad consultan `__ghost_block_ads` y
/// `__ghost_freeze_media`, que se inicializan con `init_script` y se pueden
/// cambiar en caliente con `set_privacy_flags`.
pub const INIT_SCRIPT: &str = r#"
(function () {
    if (window.__ghost_init) return;
    window.__ghost_init = true;
    const post = (o) => { try { window.ipc.postMessage(JSON.stringify(o)); } catch (e) {} };

    // --- Bloqueo nativo de rastreadores y publicidad ---
    const BLOCKED_DOMAINS = [
        'doubleclick.net', 'google-analytics.com', 'googlesyndication.com',
        'googleadservices.com', 'adservice.google.com', 'adnxs.com',
        'scorecardresearch.com', 'facebook.net', 'clarity.ms', 'hotjar.com',
        'outbrain.com', 'taboola.com', 'criteo.com', 'amazon-adsystem.com',
        'adroll.com', 'quantserve.com'
    ];
    const isBlocked = (u) => {
        if (!u) return false;
        let host;
        try { host = new URL(String(u), location.href).hostname.toLowerCase(); }
        catch (_) { return false; }
        return BLOCKED_DOMAINS.some(d => d.endsWith('.')
            ? host.startsWith(d)
            : host === d || host.endsWith('.' + d));
    };

    const origFetch = window.fetch;
    if (origFetch) {
        window.fetch = function (resource, init) {
            const url = typeof resource === 'string' ? resource : (resource && resource.url);
            if (window.__ghost_block_ads && isBlocked(url)) {
                return Promise.reject(new Error('Blocked by Ghost native ad-blocker'));
            }
            return origFetch.apply(this, arguments);
        };
    }

    const origXhrOpen = XMLHttpRequest.prototype.open;
    XMLHttpRequest.prototype.open = function (method, url) {
        if (window.__ghost_block_ads && isBlocked(url)) {
            this.__ghost_blocked = true;
            return;
        }
        return origXhrOpen.apply(this, arguments);
    };
    const origXhrSend = XMLHttpRequest.prototype.send;
    XMLHttpRequest.prototype.send = function () {
        if (this.__ghost_blocked) return;
        return origXhrSend.apply(this, arguments);
    };

    // --- Congelar reproducción en pestañas en segundo plano (excepto YouTube Music) ---
    document.addEventListener('visibilitychange', () => {
        if (!window.__ghost_freeze_media) return;
        if (document.hidden) {
            const host = (location.hostname || '').toLowerCase();
            if (!host.includes('music.youtube.com')) {
                document.querySelectorAll('video, audio').forEach((el) => {
                    if (!el.paused) {
                        el.__ghost_paused = true;
                        el.pause();
                    }
                });
            }
        } else {
            document.querySelectorAll('video, audio').forEach((el) => {
                if (el.__ghost_paused) {
                    el.__ghost_paused = false;
                    el.play().catch(() => {});
                }
            });
        }
    });

    const nav = (navigation) => post({ type: 'nav', navigation });
    const origPush = history.pushState.bind(history);
    const origReplace = history.replaceState.bind(history);
    history.pushState = function () { origPush.apply(null, arguments); nav('push'); };
    history.replaceState = function () { origReplace.apply(null, arguments); nav('replace'); };
    window.addEventListener('popstate', () => nav('traverse'));
    window.addEventListener('hashchange', () => nav('traverse'));

    window.addEventListener('click', (e) => {
        const a = e.target && e.target.closest ? e.target.closest('a[target="_blank"]') : null;
        if (a && a.href) { e.preventDefault(); e.stopPropagation(); post({ type: 'open_url', url: a.href }); }
    }, true);

    const origOpen = window.open.bind(window);
    window.open = function (url) {
        if (url) { post({ type: 'open_url', url: String(url) }); return null; }
        return origOpen.apply(null, arguments);
    };

    window.addEventListener('keydown', (e) => {
        const k = (e.key || '').toLowerCase();
        const send = (a) => { e.preventDefault(); e.stopPropagation(); post({ type: 'shortcut', action: a }); };
        if (e.key === 'F5') { send('reload'); return; }
        if (e.key === 'F12') { send('devtools'); return; }
        if (e.altKey && !e.ctrlKey && !e.shiftKey && !e.metaKey) {
            if (k === 'arrowleft') { send('back'); return; }
            if (k === 'arrowright') { send('forward'); return; }
        }
        if (e.ctrlKey && !e.altKey && !e.metaKey) {
            if (!e.shiftKey) {
                if (k === 't') send('new_tab');
                else if (k === 'w') send('close_tab');
                else if (k === 'l') send('focus_url');
                else if (k === 'r') send('reload');
                else if (k === 'd') send('bookmark');
                else if (k === 'h') send('history');
                else if (k === 'b') send('bookmarks');
                else if (k === '=' || k === '+') send('zoom_in');
                else if (k === '-') send('zoom_out');
                else if (k === '0') send('zoom_reset');
                else if (k === 'p') send('print');
                else if (k === 'tab') send('next_tab');
            } else {
                if (k === 't') send('restore_last_closed');
                else if (k === 'tab') send('prev_tab');
                else if (k === 'b') send('bookmarks');
            }
        }
    }, true);
})();
"#;

/// Script inicial con el estado actual de los ajustes de privacidad.
pub fn init_script(block_ads: bool, freeze_media: bool) -> String {
    format!(
        "window.__ghost_block_ads = {block_ads};\n\
         window.__ghost_freeze_media = {freeze_media};\n\
         {INIT_SCRIPT}"
    )
}

/// Aplica los ajustes de privacidad a un webview ya cargado.
pub fn set_privacy_flags(webview: &WebView, block_ads: bool, freeze_media: bool) {
    let js = format!(
        "window.__ghost_block_ads = {block_ads}; \
         window.__ghost_freeze_media = {freeze_media};"
    );
    if let Err(err) = webview.evaluate_script(&js) {
        log::warn!("No se pudieron aplicar los ajustes de privacidad: {err}");
    }
}

/// ¿Es la página interna de nueva pestaña? WebView2 puede devolverla como
/// `http://ghost.newtab/` además de `ghost://newtab`, y no debe mostrarse en
/// la barra de direcciones ni guardarse en el historial.
pub fn is_newtab_url(url: &str) -> bool {
    let u = url.trim();
    if u.is_empty() {
        return false;
    }
    if u.eq_ignore_ascii_case("ghost://newtab") || u.eq_ignore_ascii_case("ghost://newtab/") {
        return true;
    }
    if let Ok(parsed) = url::Url::parse(u) {
        if parsed.scheme().eq_ignore_ascii_case("ghost")
            && parsed
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case("newtab"))
        {
            return true;
        }
        if parsed
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("ghost.newtab"))
        {
            return true;
        }
    }
    false
}

/// Convierte lo que escribe el usuario en una URL navegable.
pub fn normalize_url(input: &str, home_page: &str) -> String {
    let t = input.trim();
    if t.is_empty() {
        return home_page.to_string();
    }
    if is_newtab_url(t) {
        return "ghost://newtab".to_string();
    }
    let lower = t.to_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("about:")
        || lower.starts_with("data:")
        || lower.starts_with("file:")
        || lower.starts_with("ghost:")
        || lower.starts_with("javascript:")
    {
        return t.to_string();
    }
    // localhost o IP con puerto
    if (lower.starts_with("localhost") || lower.starts_with("127.")) && !t.contains(' ') {
        return format!("http://{t}");
    }
    // Parece un dominio: sin espacios y con punto, o con ":puerto"
    let has_dot = t.contains('.');
    let has_port = t.contains(':') && !t.contains('/');
    if !t.contains(' ') && (has_dot || has_port) {
        return format!("https://{t}");
    }
    // Búsqueda
    format!("https://duckduckgo.com/?q={}", percent_encode(t))
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Nombre de archivo razonable a partir de una URL de descarga.
fn download_filename(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').next().unwrap_or("download");
    let name = if name.is_empty() { "download" } else { name };
    // Evita nombres con caracteres problemáticos en Windows.
    name.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            _ => c,
        })
        .collect()
}

/// Pide a WebView2 que reduzca (o recupere) su consumo de memoria.
/// Se usa en las pestañas que no están a la vista.
pub fn set_low_memory(webview: &WebView, low: bool) {
    #[cfg(windows)]
    {
        use wry::WebViewExtWindows;
        let level = if low {
            wry::MemoryUsageLevel::Low
        } else {
            wry::MemoryUsageLevel::Normal
        };
        let _ = webview.set_memory_usage_level(level);
    }
    #[cfg(not(windows))]
    {
        let _ = (webview, low);
    }
}

/// Fija el tema del webview (afecta a `prefers-color-scheme` en las páginas).
pub fn set_dark_theme(webview: &WebView, dark: bool) {
    #[cfg(windows)]
    {
        use wry::WebViewExtWindows;
        let theme = if dark {
            wry::Theme::Dark
        } else {
            wry::Theme::Light
        };
        let _ = webview.set_theme(theme);
    }
    #[cfg(not(windows))]
    {
        let _ = (webview, dark);
    }
}

const AD_DOMAINS: &[&str] = &[
    "doubleclick.net",
    "google-analytics.com",
    "googlesyndication.com",
    "googleadservices.com",
    "adservice.google.com",
    "adnxs.com",
    "scorecardresearch.com",
    "facebook.net",
    "clarity.ms",
    "hotjar.com",
    "outbrain.com",
    "taboola.com",
    "criteo.com",
    "amazon-adsystem.com",
    "adroll.com",
    "quantserve.com",
];

pub fn is_ad_domain(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    AD_DOMAINS.iter().any(|domain| {
        if domain.ends_with('.') {
            host.starts_with(domain)
        } else {
            host == *domain || host.ends_with(&format!(".{domain}"))
        }
    })
}

fn available_download_path(dir: &std::path::Path, url: &str) -> Option<PathBuf> {
    let base = download_filename(url);
    let (stem, extension) = match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (base.as_str(), String::new()),
    };

    for suffix in 0u64.. {
        let name = if suffix == 0 {
            base.clone()
        } else {
            format!("{stem} ({suffix}){extension}")
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Crea un webview hijo con todos los manejadores del navegador.
#[allow(clippy::too_many_arguments)]
pub fn create_webview<W: HasWindowHandle>(
    window: &W,
    bounds: Rect,
    url: &str,
    tab_id: u64,
    visible: bool,
    settings: &Settings,
    block_ads: Arc<AtomicBool>,
    tx: Sender<TabEvent>,
    download_dir: Arc<RwLock<PathBuf>>,
) -> Result<WebView> {
    let dark = settings.dark_mode;
    let script = if std::env::var_os("GHOST_NO_INIT_SCRIPT").is_some() {
        String::new()
    } else {
        init_script(settings.block_ads, settings.freeze_background_media)
    };
    let tx_ipc = tx.clone();
    let tx_load = tx.clone();
    let tx_title = tx.clone();
    let tx_dl_start = tx.clone();
    let tx_dl_done = tx.clone();

    let webview = WebViewBuilder::new()
        .with_bounds(bounds)
        .with_url(url)
        .with_visible(visible)
        .with_background_color(if dark {
            (0, 0, 0, 255)
        } else {
            (255, 255, 255, 255)
        })
        .with_devtools(true)
        .with_clipboard(true)
        .with_hotkeys_zoom(true)
        .with_initialization_script(&script)
        .with_navigation_handler(move |url| {
            if block_ads.load(Ordering::Relaxed) && is_ad_domain(&url) {
                log::info!("Ghost bloqueó navegación a dominio publicitario/rastreador: {url}");
                return false;
            }
            true
        })
        .with_ipc_handler(move |req: Request<String>| {
            let _ = tx_ipc.send(TabEvent::Ipc {
                tab: tab_id,
                source_url: req.uri().to_string(),
                message: req.body().clone(),
            });
        })
        .with_on_page_load_handler(move |event, url| {
            let msg = match event {
                PageLoadEvent::Started => TabEvent::PageStarted { tab: tab_id, url },
                PageLoadEvent::Finished => TabEvent::PageFinished { tab: tab_id, url },
            };
            let _ = tx_load.send(msg);
        })
        .with_document_title_changed_handler(move |title| {
            let _ = tx_title.send(TabEvent::TitleChanged { tab: tab_id, title });
        })
        .with_download_started_handler(move |url, path| {
            let dir = download_dir.read().map(|d| d.clone()).unwrap_or_default();
            let Some(file) = available_download_path(&dir, &url) else {
                log::error!("No se encontró un nombre disponible para la descarga: {url}");
                return false;
            };
            *path = file.clone();
            let _ = tx_dl_start.send(TabEvent::DownloadStarted {
                tab: tab_id,
                url,
                path: file,
            });
            true
        })
        .with_download_completed_handler(move |url, path, ok| {
            let _ = tx_dl_done.send(TabEvent::DownloadFinished { url, path, ok });
        })
        .with_new_window_req_handler(|_url, _features| NewWindowResponse::Deny)
        .with_custom_protocol("ghost".to_string(), |_id, req| {
            if req.uri().path() == "/logo.png" {
                return Response::builder()
                    .header("Content-Type", "image/png")
                    .body(Cow::Borrowed(crate::logo::compact_icon_png()))
                    .expect("respuesta válida");
            }
            if req.uri().path() == "/black.webp" {
                return Response::builder()
                    .header("Content-Type", "image/webp")
                    .body(Cow::Borrowed(HOME_BACKGROUND))
                    .expect("respuesta válida");
            }
            Response::builder()
                .header("Content-Type", "text/html; charset=utf-8")
                .body(Cow::Owned(NEWTAB_HTML.as_bytes().to_vec()))
                .expect("respuesta válida")
        })
        .build_as_child(window)?;

    set_low_memory(&webview, !visible);
    set_dark_theme(&webview, dark);
    Ok(webview)
}

/// Página de nueva pestaña servida por el protocolo interno `ghost://`.
const NEWTAB_HTML: &str = r##"<!DOCTYPE html>
<html lang="es">
<head>
<meta charset="utf-8">
<meta name="color-scheme" content="dark light">
<title>Nueva pestaña</title>
<style>
  :root {
    --line: rgba(255,255,255,.12);
    --text: #F5F5F5;
    --muted: #858585;
    --surface: #0B0B0B;
    --surface-hi: #111111;
  }
  * { margin:0; padding:0; box-sizing:border-box; }
  html, body { height:100%; }
  body {
    font-family:'Segoe UI',system-ui,sans-serif;
    color:var(--text); background:#050505; overflow:auto;
    display:flex; align-items:center; justify-content:center;
  }
  body::before {
    content:""; position:fixed; inset:0; z-index:0; pointer-events:none;
    background:linear-gradient(rgba(0,0,0,.62),rgba(0,0,0,.62)),
               url('/black.webp') center / cover no-repeat;
  }
  main { position:relative; z-index:1; width:min(640px, 88vw); display:flex; flex-direction:column;
         align-items:center; gap:16px; }
  .logo { width:64px; height:64px; object-fit:contain; }
  h1 { color:var(--text); font-size:30px; font-weight:600;
       letter-spacing:-.7px; line-height:1.1; }
  form { width:100%; margin-top:10px; }
  .field { width:100%; display:flex; align-items:center; gap:14px; padding:0 18px; height:56px;
           border-radius:12px; background:var(--surface); border:1px solid var(--line);
           transition:border-color .16s, background .16s; }
  .field:focus-within { border-color:rgba(255,255,255,.38); background:var(--surface-hi); }
  .field svg { color:var(--muted); flex:none; }
  input { flex:1; min-width:0; background:none; border:none; outline:none; color:var(--text);
          font-family:inherit; font-size:15px; font-weight:400; }
  input::placeholder { color:var(--muted); }
  .links { width:100%; display:flex; align-items:center; justify-content:center;
           flex-wrap:wrap; gap:6px; margin-top:12px; }
  a.tile { min-height:40px; display:inline-flex; align-items:center; justify-content:center;
           padding:0 14px; border:1px solid transparent; border-radius:9px;
           color:var(--muted); text-decoration:none; font-size:13px; font-weight:500;
           transition:color .15s, background .15s, border-color .15s; }
  a.tile:hover { color:var(--text); background:var(--surface); border-color:var(--line); }
  a.tile:focus-visible { outline:2px solid currentColor; outline-offset:2px; }

  @media (prefers-color-scheme: light) {
    :root { --line:rgba(0,0,0,.12); --text:#171717; --muted:#686868;
            --surface:#FFFFFF; --surface-hi:#FFFFFF; }
    body { background:#FAFAFA; }
    body::before { display:none; }
    .logo { filter:invert(1); }
    .field:focus-within { border-color:rgba(0,0,0,.4); }
  }
</style>
</head>
<body>
  <main>
    <img class="logo" src="/logo.png" alt="Marca de Ghost">
    <h1>Ghost</h1>
    <form id="f" role="search">
      <label class="field">
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor"
             stroke-width="1.8" stroke-linecap="round" aria-hidden="true">
          <circle cx="11" cy="11" r="7"/><path d="M20 20l-3.6-3.6"/>
        </svg>
        <input id="q" type="text" aria-label="Buscar o escribir una dirección"
               placeholder="Buscar en la web o escribir una dirección"
               autocomplete="off" autocapitalize="off" spellcheck="false"
               enterkeyhint="go" autofocus>
      </label>
    </form>
    <nav class="links" aria-label="Accesos rápidos">
      <a class="tile" href="https://duckduckgo.com">DuckDuckGo</a>
      <a class="tile" href="https://github.com">GitHub</a>
      <a class="tile" href="https://es.wikipedia.org">Wikipedia</a>
      <a class="tile" href="https://docs.rs">docs.rs</a>
      <a class="tile" href="https://www.youtube.com">YouTube</a>
    </nav>
  </main>
<script>
  const form = document.getElementById('f');
  const input = document.getElementById('q');
  form.addEventListener('submit', function (e) {
    e.preventDefault();
    const q = input.value.trim();
    if (!q) return;
    let url;
    if (/^[a-z][a-z0-9+.-]*:/i.test(q)) url = q;
    else if (!/\s/.test(q) && (q.includes('.') || q.includes(':'))) url = 'https://' + q;
    else url = 'https://duckduckgo.com/?q=' + encodeURIComponent(q);
    window.ipc.postMessage(JSON.stringify({ type: 'navigate', url: url }));
  });
  input.addEventListener('keydown', function (e) {
    if (e.key === 'Enter' && !e.isComposing) {
      e.preventDefault();
      form.requestSubmit();
    }
  });
  input.focus();
</script>
</body>
</html>"##;

#[cfg(test)]
mod newtab_tests {
    use super::NEWTAB_HTML;

    #[test]
    fn inicio_envia_con_enter_sin_boton_visible() {
        assert!(NEWTAB_HTML.contains("form.requestSubmit()"));
        assert!(NEWTAB_HTML.contains("addEventListener('submit'"));
        assert!(!NEWTAB_HTML.contains("<button"));
    }

    #[test]
    fn fondo_black_webp_es_valido_y_se_sirve_en_inicio() {
        use super::HOME_BACKGROUND;

        assert_eq!(&HOME_BACKGROUND[..4], b"RIFF");
        assert_eq!(&HOME_BACKGROUND[8..12], b"WEBP");
        assert!(NEWTAB_HTML.contains("url('/black.webp')"));
    }

    #[test]
    fn test_ad_domain_blocker() {
        use super::is_ad_domain;
        assert!(is_ad_domain("https://adservice.google.com/ads?id=123"));
        assert!(is_ad_domain(
            "https://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js"
        ));
        assert!(is_ad_domain(
            "https://securepubads.g.doubleclick.net/gampad/ads"
        ));
        assert!(!is_ad_domain("https://www.rust-lang.org/"));
        assert!(!is_ad_domain("https://github.com/rust-lang/rust"));
        assert!(!is_ad_domain("https://music.youtube.com/"));
        assert!(!is_ad_domain("https://safe.example/?next=doubleclick.net"));
        assert!(!is_ad_domain("https://doubleclick.net.attacker.example/"));
    }

    #[test]
    fn descargas_usan_un_nombre_libre_sin_limite_arbitrario_de_intentos() {
        use super::available_download_path;
        use std::fs;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ghost-download-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        for suffix in 0..=500 {
            let name = if suffix == 0 {
                "report.zip".to_string()
            } else {
                format!("report ({suffix}).zip")
            };
            fs::write(dir.join(name), []).unwrap();
        }

        let next = available_download_path(&dir, "https://example.com/report.zip").unwrap();

        assert_eq!(next.file_name().unwrap(), "report (501).zip");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_content_bounds_physical_coordinates() {
        use super::content_bounds;
        use winit::dpi::PhysicalSize;

        // Escalado 1.0
        let b1 = content_bounds(PhysicalSize::new(1000, 700), 1.0, 82.0, 24.0, false);
        let pos1: (i32, i32) = b1.position.to_physical::<i32>(1.0).into();
        assert_eq!(pos1.1, 82);
        assert_eq!(pos1.0, 6);

        // Escalado 1.5 en pantalla High-DPI
        let b2 = content_bounds(PhysicalSize::new(1920, 1080), 1.5, 82.0, 24.0, false);
        let pos2: (i32, i32) = b2.position.to_physical::<i32>(1.5).into();
        assert_eq!(pos2.1, 123);
        assert_eq!(pos2.0, 9);

        // Maximizado: gutter debe ser 0
        let b3 = content_bounds(PhysicalSize::new(1920, 1080), 1.5, 82.0, 24.0, true);
        let pos3: (i32, i32) = b3.position.to_physical::<i32>(1.5).into();
        assert_eq!(pos3.0, 0);
        let size3: (u32, u32) = b3.size.to_physical::<u32>(1.5).into();
        assert_eq!(size3.0, 1920);
    }

    #[test]
    fn test_init_script_respeta_los_ajustes_de_privacidad() {
        use super::init_script;

        let on = init_script(true, true);
        assert!(on.contains("window.__ghost_block_ads = true"));
        assert!(on.contains("window.__ghost_freeze_media = true"));
        assert!(on.contains("window.__ghost_block_ads && isBlocked"));
        assert!(on.contains("!window.__ghost_freeze_media"));
        assert!(on.contains("new_tab"));

        let off = init_script(false, false);
        assert!(off.contains("window.__ghost_block_ads = false"));
        assert!(off.contains("window.__ghost_freeze_media = false"));
    }
}

/// Ámbito del contenido web dentro de la ventana, dejando libre la barra
/// superior (`top_points`) y la barra de estado (`bottom_points`), en píxeles físicos exactos.
pub fn content_bounds(
    window_size: winit::dpi::PhysicalSize<u32>,
    scale_factor: f64,
    top_points: f64,
    bottom_points: f64,
    is_maximized: bool,
) -> Rect {
    let gutter = if is_maximized { 0.0 } else { RESIZE_GUTTER };
    let gutter_px = (gutter * scale_factor).round() as i32;
    let top_px = (top_points * scale_factor).round() as i32;
    let bottom_px = (bottom_points * scale_factor).round() as i32;

    let w = (window_size.width as i32 - 2 * gutter_px).max(1);
    let h = (window_size.height as i32 - top_px - bottom_px).max(0);

    // Solo aplicar gutter horizontalmente, no verticalmente
    Rect {
        position: wry::dpi::PhysicalPosition::new(gutter_px, top_px).into(),
        size: wry::dpi::PhysicalSize::new(w as u32, h as u32).into(),
    }
}
