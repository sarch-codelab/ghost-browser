//! Etapa de JavaScript de Wisp: ejecuta los scripts de una página en el
//! motor propio `wisp_js::vm` con DOM nativo en Rust, exponiendo el DOM a la
//! página y devolviendo el documento serializado cuando los scripts lo modifican.

use anyhow::Result;
use reqwest::blocking::Client;
use reqwest::header::{ACCESS_CONTROL_ALLOW_ORIGIN, CONTENT_TYPE};
use reqwest::redirect::Policy;
use scraper::{Html, Selector};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::time::{Duration, Instant};

/// Conformidad entre los descriptores WebIDL (`wisp-idl`) y el DOM nativo.
pub mod idl;
mod native_dom;
/// Motor JS propio de Wisp (lexer -> parser -> AST -> bytecode -> VM).
pub mod vm;

const MAX_INLINE_SCRIPT_BYTES: usize = 64 * 1024;
const MAX_TOTAL_SCRIPT_BYTES: usize = 256 * 1024;
const MAX_INLINE_SCRIPT_COUNT: usize = 32;
const MAX_EXTERNAL_SCRIPT_BYTES: usize = 512 * 1024;
const MAX_TOTAL_EXTERNAL_SCRIPT_BYTES: usize = 2 * 1024 * 1024;
const MAX_EXECUTED_SCRIPTS: usize = 48;
const MAX_MODULES: usize = 32;
const MAX_TREE_DEPTH: usize = 256;
const MAX_SCRIPT_WARNINGS: usize = 8;
/// `fetch()`: tamaño máximo de una respuesta, peticiones por página y tiempo
/// máximo. Son los mismos espíritu que el resto de límites de Wisp: la página
/// nunca puede bloquear ni llenar la memoria del navegador.
const MAX_FETCH_BYTES: usize = 512 * 1024;
const MAX_FETCHES_PER_PAGE: usize = 24;
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Presupuesto total de red que la página puede gastar en `fetch()`/XHR. Con
/// hasta 24 peticiones y 5 s cada una, una página lenta podía bloquear la
/// carga más de dos minutos; el tope global mantiene la página interactiva.
const MAX_FETCH_TOTAL_TIME: Duration = Duration::from_secs(6);

/// Estado del `fetch` de la página actual. El motor de scripts corre en su
/// propio hilo, así que un thread-local es exactamente el ámbito correcto.
#[derive(Default)]
struct FetchState {
    page_url: Option<url::Url>,
    policy: wisp_net::NetworkPolicy,
    used: usize,
    /// Fecha límite compartida por todas las peticiones de la página.
    deadline: Option<Instant>,
    /// Motivos de bloqueo (CORS, límites, red) para avisar en la interfaz.
    warnings: Vec<String>,
}

thread_local! {
    static FETCH_STATE: RefCell<FetchState> = RefCell::new(FetchState::default());
}

#[derive(Default)]
pub struct ScriptResult {
    /// Serialized document produced when page scripts changed the tree.
    /// `None` means the original Rust-side document is still accurate.
    pub html: Option<String>,
    pub warnings: Vec<String>,
}

/// Geometría computada para un elemento del DOM (por índice de elemento en preorden).
#[derive(Debug, Clone, Default)]
pub struct ElementGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub display: String,
    pub visibility: String,
    pub position: String,
    pub color: String,
    pub background_color: String,
    pub font_size: String,
}

/// Hook opcional para consultar geometría real de layout (p. ej. Taffy desde `wisp-engine`).
pub type GeometryHook<'a> = &'a mut dyn FnMut(&str, f32) -> HashMap<usize, ElementGeometry>;

struct ModuleEntry {
    label: String,
    /// Resolved absolute URL, or a synthetic `#wisp-inline-module-N` URL for
    /// inline modules. Used both as the loader cache key and as `import.meta.url`.
    url: String,
    source: String,
}

pub fn execute_scripts(
    document: &Html,
    external_scripts: &HashMap<String, String>,
    module_scripts: &HashMap<String, String>,
    page_url: Option<&str>,
    viewport_width: f32,
) -> Result<ScriptResult> {
    execute_scripts_with_policy(
        document,
        external_scripts,
        module_scripts,
        page_url,
        viewport_width,
        &wisp_net::NetworkPolicy::from_env(),
    )
}

pub fn execute_scripts_with_policy(
    document: &Html,
    external_scripts: &HashMap<String, String>,
    module_scripts: &HashMap<String, String>,
    page_url: Option<&str>,
    viewport_width: f32,
    policy: &wisp_net::NetworkPolicy,
) -> Result<ScriptResult> {
    execute_scripts_with_geometry(
        document,
        external_scripts,
        module_scripts,
        page_url,
        viewport_width,
        policy,
        None,
    )
}

pub fn execute_scripts_with_geometry(
    document: &Html,
    external_scripts: &HashMap<String, String>,
    module_scripts: &HashMap<String, String>,
    page_url: Option<&str>,
    viewport_width: f32,
    policy: &wisp_net::NetworkPolicy,
    geometry_hook: Option<GeometryHook<'_>>,
) -> Result<ScriptResult> {
    let page_url_owned = page_url.map(str::to_owned);
    let policy = *policy;
    let script_selector = Selector::parse("script")
        .map_err(|error| anyhow::anyhow!("invalid script selector: {error}"))?;
    let mut classic = Vec::new();
    let mut modules = Vec::new();
    let mut warnings = Vec::new();
    let mut inline_bytes = 0usize;
    let mut external_bytes = 0usize;
    let mut module_bytes = 0usize;
    let mut inline_scripts = 0usize;
    let mut inline_modules = 0usize;
    let mut missing_external = 0usize;
    let mut missing_modules = 0usize;

    for (index, script) in document.select(&script_selector).enumerate() {
        let script_type = script
            .attr("type")
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();

        if matches!(
            script_type.as_str(),
            "application/json" | "application/ld+json" | "importmap"
        ) {
            continue;
        }

        if script_type == "module" {
            if modules.len() >= MAX_MODULES {
                push_warning(
                    &mut warnings,
                    format!("Se alcanzó el límite de {MAX_MODULES} módulos ES."),
                );
                break;
            }
            if let Some(src) = script.attr("src") {
                let Some(url) = resolve_script_url(page_url, src) else {
                    missing_modules += 1;
                    continue;
                };
                let Some(source) = module_scripts.get(&url) else {
                    missing_modules += 1;
                    continue;
                };
                if source.len() > MAX_EXTERNAL_SCRIPT_BYTES {
                    push_warning(
                        &mut warnings,
                        format!(
                            "Se omitió el módulo externo {} por superar el límite de tamaño.",
                            index + 1
                        ),
                    );
                    continue;
                }
                if module_bytes.saturating_add(source.len()) > MAX_TOTAL_EXTERNAL_SCRIPT_BYTES {
                    push_warning(
                        &mut warnings,
                        "Se alcanzó el límite total de módulos ES de Wisp.".to_owned(),
                    );
                    break;
                }
                module_bytes += source.len();
                modules.push(ModuleEntry {
                    label: format!("el módulo externo {} ({url})", index + 1),
                    url,
                    source: source.clone(),
                });
            } else {
                let source = script.text().collect::<String>();
                if source.len() > MAX_INLINE_SCRIPT_BYTES {
                    push_warning(
                        &mut warnings,
                        format!(
                            "Se omitió el módulo inline {} por superar el límite de tamaño.",
                            index + 1
                        ),
                    );
                    continue;
                }
                if inline_bytes.saturating_add(source.len()) > MAX_TOTAL_SCRIPT_BYTES {
                    push_warning(
                        &mut warnings,
                        "Se alcanzó el límite total de JavaScript inline de Wisp.".to_owned(),
                    );
                    break;
                }
                inline_bytes += source.len();
                let url = if let Some(base) = page_url_owned.as_deref() {
                    format!("{}#wisp-inline-module-{}", base, inline_modules)
                } else {
                    format!("about:blank#wisp-inline-module-{}", inline_modules)
                };
                inline_modules += 1;
                modules.push(ModuleEntry {
                    label: format!("el módulo inline {}", index + 1),
                    url,
                    source,
                });
            }
            continue;
        }

        if !matches!(
            script_type.as_str(),
            "" | "text/javascript"
                | "application/javascript"
                | "text/ecmascript"
                | "application/ecmascript"
        ) {
            push_warning(
                &mut warnings,
                format!("Se omitió un script con tipo no compatible: {script_type}."),
            );
            continue;
        }

        if classic.len() >= MAX_EXECUTED_SCRIPTS {
            push_warning(
                &mut warnings,
                format!("Se alcanzó el límite de {MAX_EXECUTED_SCRIPTS} scripts ejecutados."),
            );
            break;
        }

        if let Some(src) = script.attr("src") {
            if script.attr("nomodule").is_some() {
                continue;
            }
            let Some(url) = resolve_script_url(page_url, src) else {
                missing_external += 1;
                continue;
            };
            let Some(source) = external_scripts.get(&url) else {
                missing_external += 1;
                continue;
            };
            if source.len() > MAX_EXTERNAL_SCRIPT_BYTES {
                push_warning(
                    &mut warnings,
                    format!(
                        "Se omitió el script externo {} por superar el límite de tamaño.",
                        index + 1
                    ),
                );
                continue;
            }
            if external_bytes.saturating_add(source.len()) > MAX_TOTAL_EXTERNAL_SCRIPT_BYTES {
                push_warning(
                    &mut warnings,
                    "Se alcanzó el límite total de JavaScript externo de Wisp.".to_owned(),
                );
                break;
            }
            external_bytes += source.len();
            classic.push((
                format!("el script externo {} ({url})", index + 1),
                source.clone(),
            ));
            continue;
        }

        if inline_scripts >= MAX_INLINE_SCRIPT_COUNT {
            push_warning(
                &mut warnings,
                format!("Se alcanzó el límite de {MAX_INLINE_SCRIPT_COUNT} scripts inline."),
            );
            break;
        }

        let source = script.text().collect::<String>();
        if source.len() > MAX_INLINE_SCRIPT_BYTES {
            push_warning(
                &mut warnings,
                format!(
                    "Se omitió el script inline {} por superar el límite de tamaño.",
                    index + 1
                ),
            );
            continue;
        }
        if inline_bytes.saturating_add(source.len()) > MAX_TOTAL_SCRIPT_BYTES {
            push_warning(
                &mut warnings,
                "Se alcanzó el límite total de JavaScript inline de Wisp.".to_owned(),
            );
            break;
        }
        inline_bytes += source.len();
        inline_scripts += 1;
        classic.push((format!("el script inline {}", index + 1), source));
    }

    if missing_external > 0 {
        push_warning(
            &mut warnings,
            if missing_external == 1 {
                "Wisp no pudo ejecutar 1 script externo.".to_owned()
            } else {
                format!("Wisp no pudo ejecutar {missing_external} scripts externos.")
            },
        );
    }
    if missing_modules > 0 {
        push_warning(
            &mut warnings,
            if missing_modules == 1 {
                "Wisp no pudo ejecutar 1 módulo ES.".to_owned()
            } else {
                format!("Wisp no pudo ejecutar {missing_modules} módulos ES.")
            },
        );
    }

    if classic.is_empty() && modules.is_empty() {
        return Ok(ScriptResult {
            html: None,
            warnings,
        });
    }

    let (html, script_warnings) = native_dom::run_scripts_native(
        document,
        classic,
        modules,
        module_scripts,
        page_url_owned,
        viewport_width,
        &policy,
        geometry_hook,
    );
    for warning in script_warnings {
        push_warning(&mut warnings, warning);
    }

    Ok(ScriptResult { html, warnings })
}

/// Resultado de evaluar una expresión contra el DOM de una página.
#[derive(Debug, Default)]
pub struct EvalResult {
    /// Valor devuelto por la expresión, serializado a JSON. `None` si la
    /// expresión no devolvió nada (p. ej. `undefined`) o su valor no se pudo
    /// serializar.
    pub value: Option<serde_json::Value>,
    /// Documento serializado si la expresión mutó el DOM; `None` si no cambió.
    pub html: Option<String>,
    pub warnings: Vec<String>,
}

/// Evalúa `source` contra el DOM de `html` y devuelve su valor.
pub fn evaluate_expression(
    html: &str,
    page_url: Option<&str>,
    viewport_width: f32,
    source: &str,
) -> Result<EvalResult> {
    evaluate_expression_with_policy(
        html,
        page_url,
        viewport_width,
        source,
        &wisp_net::NetworkPolicy::from_env(),
    )
}

pub fn evaluate_expression_with_policy(
    html: &str,
    page_url: Option<&str>,
    viewport_width: f32,
    source: &str,
    policy: &wisp_net::NetworkPolicy,
) -> Result<EvalResult> {
    evaluate_expression_with_geometry(html, page_url, viewport_width, source, policy, None)
}

pub fn evaluate_expression_with_geometry(
    html: &str,
    page_url: Option<&str>,
    viewport_width: f32,
    source: &str,
    policy: &wisp_net::NetworkPolicy,
    geometry_hook: Option<GeometryHook<'_>>,
) -> Result<EvalResult> {
    let document = Html::parse_document(html);
    native_dom::evaluate_expression_native(
        &document,
        page_url,
        viewport_width,
        source,
        *policy,
        geometry_hook,
    )
}

fn resolve_script_url(page_url: Option<&str>, src: &str) -> Option<String> {
    let base = url::Url::parse(page_url?).ok()?;
    let url = base.join(src).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(url.to_string())
}

/// Guarda el primer aviso de cada motivo (CORS, límites, red) para que Ghost
/// lo muestre en la página y en la barra de estado. Antes estos fallos se
/// descartaban en silencio.
fn record_fetch_warning(warning: &str) {
    FETCH_STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.warnings.len() < 4 && !state.warnings.iter().any(|seen| seen == warning) {
            state.warnings.push(warning.to_owned());
        }
    });
}

/// `fetch` acotado: sólo HTTP(S) sin credenciales, GET/HEAD, 512 KiB por
/// respuesta, 24 peticiones y 6 s de red por página. Las respuestas de otro
/// origen requieren `Access-Control-Allow-Origin`, igual que en un navegador.
/// Cada fallo devuelve el motivo en texto para poder avisar.
fn fetch_json(request: &str, method: &str) -> Result<String, String> {
    let (page_url, policy, used, deadline) = FETCH_STATE.with(|state| {
        let state = state.borrow();
        (
            state.page_url.clone(),
            state.policy,
            state.used,
            state.deadline,
        )
    });
    if used >= MAX_FETCHES_PER_PAGE {
        return Err(format!(
            "fetch(): límite de {MAX_FETCHES_PER_PAGE} peticiones por página alcanzado."
        ));
    }
    let budget_exhausted = || {
        format!(
            "fetch(): presupuesto de red de {} s por página agotado.",
            MAX_FETCH_TOTAL_TIME.as_secs()
        )
    };
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(budget_exhausted());
    }
    let method = method.trim().to_ascii_uppercase();
    if !matches!(method.as_str(), "GET" | "HEAD") {
        return Err(format!(
            "fetch(): el método {method} no está disponible en Wisp."
        ));
    }
    let target = match &page_url {
        Some(base) => base
            .join(request)
            .map_err(|_| format!("fetch(): URL no válida: {request}"))?,
        None => {
            url::Url::parse(request).map_err(|_| format!("fetch(): URL no válida: {request}"))?
        }
    };
    if !matches!(target.scheme(), "http" | "https")
        || !target.username().is_empty()
        || target.password().is_some()
    {
        return Err("fetch(): sólo se permiten URL HTTP(S) sin credenciales.".to_owned());
    }
    if policy.block_trackers && wisp_net::is_tracker_url(target.as_str()) {
        return Err(format!(
            "fetch(): bloqueado por --block-trackers: {}",
            target.host_str().unwrap_or_default()
        ));
    }
    wisp_net::validate_url(target.as_str(), &policy)
        .map_err(|error| format!("fetch(): {error}"))?;
    FETCH_STATE.with(|state| state.borrow_mut().used += 1);

    let host = target.host_str().unwrap_or_default().to_owned();
    let timeout = deadline
        .map(|deadline| FETCH_TIMEOUT.min(deadline.saturating_duration_since(Instant::now())))
        .unwrap_or(FETCH_TIMEOUT);
    if timeout.is_zero() {
        return Err(budget_exhausted());
    }
    let stealth_profile = wisp_net::StealthProfile::for_policy(&policy);
    let redirect_policy = policy;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(3).min(timeout))
        .timeout(timeout)
        .redirect(Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                attempt.stop()
            } else if wisp_net::validate_url(attempt.url().as_str(), &redirect_policy).is_err() {
                attempt.error("fetch(): redirección bloqueada por política SSRF")
            } else {
                attempt.follow()
            }
        }))
        .user_agent(stealth_profile.user_agent)
        .build()
        .map_err(|error| format!("fetch(): no se pudo preparar la petición: {error}"))?;
    let mut request_builder = if method == "HEAD" {
        client.head(target.clone())
    } else {
        client.get(target.clone())
    };
    if policy.stealth {
        let lang = stealth_profile.accept_language;
        request_builder = request_builder.header("Accept-Language", lang);
        if let Some(ch_ua) = stealth_profile.sec_ch_ua {
            request_builder = request_builder.header("Sec-CH-UA", ch_ua);
        }
        if let Some(ch_mob) = stealth_profile.sec_ch_ua_mobile {
            request_builder = request_builder.header("Sec-CH-UA-Mobile", ch_mob);
        }
        if let Some(ch_plat) = stealth_profile.sec_ch_ua_platform {
            request_builder = request_builder.header("Sec-CH-UA-Platform", ch_plat);
        }
    }
    let response = request_builder.send().map_err(|error| {
        if error.is_timeout() {
            format!(
                "fetch(): {host} no respondió en {} s.",
                timeout.as_secs().max(1)
            )
        } else {
            format!("fetch(): no se pudo conectar con {host}: {error}")
        }
    })?;
    if !same_origin(&page_url, response.url()) && !cors_allows(&response, &page_url) {
        return Err(format!(
            "fetch(): CORS bloqueó la respuesta de {host} (sin Access-Control-Allow-Origin)."
        ));
    }
    let status = response.status().as_u16();
    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    let mut body = Vec::new();
    response
        .take((MAX_FETCH_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|error| format!("fetch(): no se pudo leer la respuesta de {host}: {error}"))?;
    if body.len() > MAX_FETCH_BYTES {
        return Err(format!(
            "fetch(): la respuesta de {host} supera el límite de {} KiB.",
            MAX_FETCH_BYTES / 1024
        ));
    }
    Ok(serde_json::json!({
        "status": status,
        "ok": (200..300).contains(&status),
        "body": String::from_utf8_lossy(&body).into_owned(),
        "url": final_url,
        "type": content_type,
    })
    .to_string())
}

fn same_origin(page: &Option<url::Url>, target: &url::Url) -> bool {
    let Some(page) = page else {
        return false;
    };
    page.scheme() == target.scheme()
        && page.host_str() == target.host_str()
        && page.port_or_known_default() == target.port_or_known_default()
}

fn cors_allows(response: &reqwest::blocking::Response, page: &Option<url::Url>) -> bool {
    let Some(value) = response
        .headers()
        .get(ACCESS_CONTROL_ALLOW_ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let value = value.trim();
    if value == "*" {
        return true;
    }
    let Some(page) = page else {
        return false;
    };
    let host = page.host_str().unwrap_or_default();
    let origin = match page.port() {
        Some(port) => format!("{}://{host}:{port}", page.scheme()),
        None => format!("{}://{host}", page.scheme()),
    };
    value
        .split(',')
        .any(|allowed| allowed.trim().eq_ignore_ascii_case(&origin))
}

fn push_warning(warnings: &mut Vec<String>, warning: String) {
    if warnings.len() < MAX_SCRIPT_WARNINGS && !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

fn truncate_error(error: &str) -> String {
    let mut end = error.len().min(256);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(html: &str) -> ScriptResult {
        run_with_url(html, None)
    }

    fn run_with_url(html: &str, url: Option<&str>) -> ScriptResult {
        run_with_external(html, url, &[])
    }

    fn run_with_external(html: &str, url: Option<&str>, external: &[(&str, &str)]) -> ScriptResult {
        run_with_modules(html, url, external, &[])
    }

    fn run_with_modules(
        html: &str,
        url: Option<&str>,
        external: &[(&str, &str)],
        modules: &[(&str, &str)],
    ) -> ScriptResult {
        let document = Html::parse_document(html);
        let scripts = external
            .iter()
            .map(|(url, source)| ((*url).to_owned(), (*source).to_owned()))
            .collect::<HashMap<_, _>>();
        let module_scripts = modules
            .iter()
            .map(|(url, source)| ((*url).to_owned(), (*source).to_owned()))
            .collect::<HashMap<_, _>>();
        execute_scripts(&document, &scripts, &module_scripts, url, 300.0).unwrap()
    }

    fn snapshot(html: &str) -> String {
        run(html)
            .html
            .expect("the script should have mutated the DOM")
    }

    fn snapshot_text(html: &str, selector: &str) -> String {
        let snapshot = snapshot(html);
        result_text_html(&snapshot, selector)
    }

    fn result_text(result: &ScriptResult, selector: &str) -> String {
        let html = result
            .html
            .as_deref()
            .expect("the script should have mutated the DOM");
        result_text_html(html, selector)
    }

    fn result_text_html(html: &str, selector: &str) -> String {
        let document = Html::parse_document(html);
        document
            .select(&Selector::parse(selector).unwrap())
            .next()
            .map(|element| element.text().collect::<String>())
            .unwrap_or_default()
    }

    fn count(html: &str, selector: &str) -> usize {
        let snapshot = snapshot(html);
        let document = Html::parse_document(&snapshot);
        document.select(&Selector::parse(selector).unwrap()).count()
    }

    fn eval(html: &str, source: &str) -> EvalResult {
        evaluate_expression(html, Some("https://example.com/"), 300.0, source).unwrap()
    }

    #[test]
    fn eval_returns_the_expression_value() {
        let result = eval("<html><body><h1>Título</h1></body></html>", "1 + 1");
        assert_eq!(result.value, Some(serde_json::json!(2)));
        assert!(result.html.is_none(), "leer no debe serializar el DOM");
    }

    #[test]
    fn eval_reads_the_dom() {
        let result = eval(
            "<html><body><p id=\"x\">hola</p></body></html>",
            "document.getElementById('x').textContent",
        );
        assert_eq!(result.value, Some(serde_json::json!("hola")));
    }

    #[test]
    fn eval_can_mutate_the_dom() {
        let result = eval(
            "<html><body><p id=\"x\">hola</p></body></html>",
            "document.getElementById('x').textContent = 'adios'; 'hecho'",
        );
        assert_eq!(result.value, Some(serde_json::json!("hecho")));
        let html = result.html.expect("mutar debe serializar el DOM");
        assert!(html.contains("adios"), "{html}");
    }

    #[test]
    fn eval_reports_script_errors() {
        let error = evaluate_expression("<html></html>", None, 300.0, "throw new Error('boom')")
            .expect_err("un error de script debe propagarse");
        assert!(error.to_string().contains("falló la evaluación"), "{error}");
    }

    #[test]
    fn eval_returns_none_for_undefined() {
        let result = eval("<html><body></body></html>", "var sinValor;");
        assert_eq!(result.value, None);
    }

    #[test]
    fn external_scripts_without_sources_are_reported() {
        let result = run(r#"<script src="/app.js"></script><p>Static text</p>"#);
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains("script externo"));
        assert!(result.html.is_none());
    }

    #[test]
    fn multiple_external_scripts_are_reported_once() {
        let html = r#"<script src="a.js"></script><script src="b.js"></script><script src="c.js"></script>"#;
        let result = run(html);
        assert_eq!(
            result.warnings,
            ["Wisp no pudo ejecutar 3 scripts externos."]
        );
    }

    #[test]
    fn external_scripts_run_in_document_order_when_sources_are_available() {
        let html = r##"<p id="out">x</p>
            <script src="/first.js"></script>
            <script>document.getElementById("out").textContent += "-inline";</script>
            <script src="/second.js"></script>"##;
        let result = run_with_external(
            html,
            Some("https://example.com/page"),
            &[
                (
                    "https://example.com/first.js",
                    r#"document.getElementById("out").textContent = "external";"#,
                ),
                (
                    "https://example.com/second.js",
                    r#"document.getElementById("out").textContent += "-second";"#,
                ),
            ],
        );
        let document = Html::parse_document(result.html.as_deref().unwrap());
        let out = document
            .select(&Selector::parse("#out").unwrap())
            .next()
            .unwrap()
            .text()
            .collect::<String>();
        assert_eq!(out, "external-inline-second");
    }

    #[test]
    fn data_blocks_are_ignored_and_modules_execute() {
        let html = r#"<p id="out">before</p>
            <script type="module">document.getElementById("out").textContent = "module";</script>
            <script type="application/ld+json">{"a": 1}</script>"#;
        let result = run(html);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(snapshot_text(html, "#out"), "module");
    }

    #[test]
    fn inline_modules_can_import_external_dependencies() {
        let html = r#"<p id="out">before</p>
            <script type="module">
                import { label } from "./dep.js";
                document.getElementById("out").textContent = label;
            </script>"#;
        let result = run_with_modules(
            html,
            Some("https://example.com/page"),
            &[],
            &[(
                "https://example.com/dep.js",
                r#"export const label = "from-dep";"#,
            )],
        );
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result_text(&result, "#out"), "from-dep");
    }

    #[test]
    fn external_modules_run_and_can_import_relative_modules() {
        let html = r#"<p id="out">before</p>
            <script type="module" src="/app/main.js"></script>"#;
        let result = run_with_modules(
            html,
            Some("https://example.com/page"),
            &[],
            &[
                (
                    "https://example.com/app/main.js",
                    r#"import { value } from "./lib/util.js";
                       document.getElementById("out").textContent = value;"#,
                ),
                (
                    "https://example.com/app/lib/util.js",
                    r#"export const value = "nested";"#,
                ),
            ],
        );
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result_text(&result, "#out"), "nested");
    }

    #[test]
    fn import_meta_url_is_the_module_url() {
        let html = r#"<p id="out">before</p>
            <script type="module">
                document.getElementById("out").textContent = import.meta.url;
            </script>"#;
        let result = run_with_modules(html, Some("https://example.com/page"), &[], &[]);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(
            result_text(&result, "#out"),
            "https://example.com/page#wisp-inline-module-0"
        );
    }

    #[test]
    fn modules_run_after_classic_scripts() {
        let html = r#"<p id="out">before</p>
            <script>document.getElementById("out").textContent = "classic";</script>
            <script type="module">document.getElementById("out").textContent += "-module";</script>"#;
        let result = run(html);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(snapshot_text(html, "#out"), "classic-module");
    }

    #[test]
    fn missing_modules_warn_once() {
        let html = r#"<script type="module" src="/missing.js"></script>"#;
        let result = run_with_modules(html, Some("https://example.com/page"), &[], &[]);
        assert_eq!(result.warnings, ["Wisp no pudo ejecutar 1 módulo ES."]);
    }

    #[test]
    fn module_syntax_errors_are_reported_without_stopping_later_modules() {
        let html = r#"<p id="out">before</p>
            <script type="module">export const = ;</script>
            <script type="module">document.getElementById("out").textContent = "second";</script>"#;
        let result = run(html);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("No se pudo analizar el módulo inline 1")),
            "{:?}",
            result.warnings
        );
        assert_eq!(snapshot_text(html, "#out"), "second");
    }

    #[test]
    fn a_failing_script_does_not_prevent_later_scripts_from_running() {
        let html = r##"<p id="out">before</p>
            <script>const = ;</script>
            <script>document.getElementById("out").textContent = "after";</script>"##;
        let result = run(html);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("Falló el script inline 1"))
        );
        assert_eq!(snapshot_text(html, "#out"), "after");
    }

    #[test]
    fn scripts_cannot_replace_the_internal_dom() {
        let html = r#"<p>Still visible</p>
            <script>__wispDocument = { tag: "p", children: [{ text: "hacked" }] };</script>"#;
        let result = run(html);
        let document = Html::parse_document(result.html.as_deref().unwrap_or(html));
        let text = document
            .select(&Selector::parse("p").unwrap())
            .next()
            .unwrap()
            .text()
            .collect::<String>();
        assert_eq!(text, "Still visible");
    }

    #[test]
    fn an_empty_id_does_not_match_elements_without_an_id() {
        let html = r##"<p id="out">x</p>
            <script>document.getElementById("out").textContent =
                String(document.getElementById("") === null);</script>"##;
        assert_eq!(snapshot_text(html, "#out"), "true");
    }

    #[test]
    fn minimal_dom_collections_match_like_the_web_apis_they_mimic() {
        let html = r##"<p id="out">x</p><p class="item">a</p>
            <div><span class="item">b</span></div>
            <script>
                document.getElementById("out").textContent = [
                    document.getElementsByClassName("item").length,
                    document.getElementsByTagName("p").length,
                    document.getElementsByClassName("").length,
                    document.querySelectorAll("p.item").length
                ].join(",");
            </script>"##;
        assert_eq!(snapshot_text(html, "#out"), "2,2,0,1");
    }

    #[test]
    fn text_content_replaces_child_elements() {
        let html = r##"<div id="target"><span>child</span></div>
            <script>document.getElementById("target").textContent = "flat";</script>"##;
        assert_eq!(snapshot_text(html, "#target"), "flat");
    }

    #[test]
    fn scripts_can_create_append_and_remove_elements() {
        let html = r##"<ul id="list"><li>old</li></ul>
            <script>
                const list = document.getElementById("list");
                list.textContent = "";
                for (let index = 0; index < 3; index++) {
                    const item = document.createElement("li");
                    item.textContent = "Item " + index;
                    list.appendChild(item);
                }
                list.removeChild(list.firstChild);
            </script>"##;
        assert_eq!(count(html, "#list li"), 2);
        assert_eq!(snapshot_text(html, "#list li"), "Item 1");
    }

    #[test]
    fn attributes_classes_and_inline_styles_are_serialized() {
        let html = r##"<p id="target">x</p><script>
            const target = document.getElementById("target");
            target.setAttribute("data-role", "note");
            target.className = "card";
            target.classList.add("highlight");
            target.style.backgroundColor = "red";
            target.style.setProperty("padding", "4px");
        </script>"##;
        let snapshot = snapshot(html);
        assert!(snapshot.contains(r#"data-role="note""#));
        assert!(snapshot.contains(r#"class="card highlight""#));
        assert!(snapshot.contains("background-color: red"));
        assert!(snapshot.contains("padding: 4px"));
    }

    #[test]
    fn inner_html_parses_markup_and_escapes_text() {
        let html = r##"<div id="target"></div><script>
            document.getElementById("target").innerHTML =
                '<span class="badge">new</span><em>a &amp; b</em>';
        </script>"##;
        assert_eq!(count(html, "#target .badge"), 1);
        assert_eq!(snapshot_text(html, "#target em"), "a & b");
        assert!(snapshot(html).contains("&amp;"));
    }

    #[test]
    fn click_listeners_mutate_the_dom() {
        let html = r##"<button id="button">Open</button><p id="out">before</p>
            <script>
                document.getElementById("button").addEventListener("click", event => {
                    event.currentTarget.textContent = "Pressed";
                    document.getElementById("out").textContent = "after";
                });
                document.getElementById("button").click();
            </script>"##;
        assert_eq!(snapshot_text(html, "#button"), "Pressed");
        assert_eq!(snapshot_text(html, "#out"), "after");
    }

    #[test]
    fn timers_and_animation_frames_run_before_the_snapshot() {
        let html = r##"<p id="out">before</p><script>
            document.addEventListener("DOMContentLoaded", () => {
                document.getElementById("out").textContent = "ready";
            });
            window.addEventListener("load", () => {
                setTimeout(() => {
                    document.getElementById("out").textContent += " + timer";
                    requestAnimationFrame(() => {
                        document.getElementById("out").textContent += " + frame";
                    });
                }, 0);
            });
        </script>"##;
        assert_eq!(snapshot_text(html, "#out"), "ready + timer + frame");
    }

    #[test]
    fn location_storage_and_promises_are_available() {
        let html = r##"<p id="out">x</p><script>
            localStorage.setItem("key", "value");
            Promise.resolve().then(() => {
                document.getElementById("out").textContent =
                    location.pathname + "|" + localStorage.getItem("key");
            });
        </script>"##;
        let result = run_with_url(html, Some("https://example.com/docs/page?q=1"));
        let document = Html::parse_document(result.html.as_deref().unwrap());
        let out = document
            .select(&Selector::parse("#out").unwrap())
            .next()
            .unwrap()
            .text()
            .collect::<String>();
        assert_eq!(out, "/docs/page|value");
    }

    #[test]
    fn recursion_limit_reports_a_warning_instead_of_crashing() {
        let result =
            run("<script>function recurse(n) { return recurse(n + 1); } recurse(0);</script>");
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("Falló el script inline 1"))
        );
    }

    #[test]
    fn oversized_inline_scripts_are_skipped() {
        let mut html = String::from("<script>");
        html.push_str(&"a".repeat(MAX_INLINE_SCRIPT_BYTES + 1));
        html.push_str("</script>");
        let result = run(&html);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("límite de tamaño"))
        );
    }

    #[test]
    fn inline_script_count_is_capped() {
        let html = "<script></script>".repeat(MAX_INLINE_SCRIPT_COUNT + 1);
        let result = run(&html);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("límite de 32 scripts"))
        );
    }

    #[test]
    fn warning_output_is_bounded() {
        let html = (0..MAX_SCRIPT_WARNINGS + 4)
            .map(|index| format!(r#"<script type="text/x-wisp-{index}"></script>"#))
            .collect::<String>();
        let result = run(&html);
        assert_eq!(result.warnings.len(), MAX_SCRIPT_WARNINGS);
    }

    #[test]
    fn url_and_text_encoding_polyfills_work() {
        let html = r##"<p id="out">x</p><script>
            const url = new URL("../docs?q=1#top", "https://example.com/app/page");
            document.getElementById("out").textContent = [
                url.href,
                url.origin,
                url.pathname,
                url.search,
                url.hash,
                url.searchParams.get("q"),
                new TextDecoder().decode(new TextEncoder().encode("ñ"))
            ].join("|");
        </script>"##;
        assert_eq!(
            snapshot_text(html, "#out"),
            "https://example.com/docs?q=1#top|https://example.com|/docs|?q=1|#top|1|ñ"
        );
    }

    #[test]
    fn prepending_an_existing_child_does_not_throw() {
        let html = r##"<p id="first">first</p><p id="second">second</p>
            <script>
                const body = document.body;
                body.prepend(body.children[0]);
                body.insertAdjacentHTML("afterbegin", "<span>start</span>");
            </script>"##;
        assert_eq!(snapshot_text(html, "body > :first-child"), "start");
        assert_eq!(snapshot_text(html, "#first"), "first");
    }

    #[test]
    fn setting_document_title_updates_the_title_element() {
        let html = r##"<html><head><title>Old</title></head><body></body></html>
            <script>document.title = "New";</script>"##;
        assert_eq!(snapshot_text(html, "title"), "New");
    }

    #[test]
    fn unchanged_documents_do_not_need_a_snapshot() {
        let result = run(r#"<p>Static</p><script>const value = 1 + 1;</script>"#);
        assert!(result.html.is_none());
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn fetch_blocks_private_ips_under_strict_policy() {
        let eval = evaluate_expression_with_policy(
            "<p>ok</p>",
            Some("https://example.com/"),
            1280.0,
            "fetch('http://127.0.0.1:47820/health').catch(e => String(e))",
            &wisp_net::NetworkPolicy::strict(),
        )
        .expect("eval");
        assert!(
            eval.warnings
                .iter()
                .any(|w| w.contains("SSRF") || w.contains("127.0.0.1")),
            "warnings={:?}",
            eval.warnings
        );
    }
}
