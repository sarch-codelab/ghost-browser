use anyhow::{Result, anyhow, ensure};
use boa_engine::builtins::promise::PromiseState;
use boa_engine::module::{ModuleLoader, ModuleRequest, Referrer};
use boa_engine::native_function::NativeFunction;
use boa_engine::{
    Context, JsNativeError, JsObject, JsResult, JsString, JsValue, Module, Source, js_string,
    property::Attribute,
};
use ego_tree::NodeRef;
use reqwest::blocking::Client;
use reqwest::header::{ACCESS_CONTROL_ALLOW_ORIGIN, CONTENT_TYPE};
use reqwest::redirect::Policy;
use scraper::node::Node;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

const MAX_INLINE_SCRIPT_BYTES: usize = 64 * 1024;
const MAX_TOTAL_SCRIPT_BYTES: usize = 256 * 1024;
const MAX_INLINE_SCRIPT_COUNT: usize = 32;
const MAX_EXTERNAL_SCRIPT_BYTES: usize = 512 * 1024;
const MAX_TOTAL_EXTERNAL_SCRIPT_BYTES: usize = 2 * 1024 * 1024;
const MAX_EXECUTED_SCRIPTS: usize = 48;
const MAX_MODULES: usize = 32;
const MAX_LOOP_ITERATIONS: u64 = 100_000;
const MAX_RECURSION_DEPTH: usize = 128;
const MAX_STACK_SIZE: usize = 4096;
const MAX_TREE_DEPTH: usize = 256;
/// Presupuesto total de tiempo de JS por página. Boa no permite interrumpir
/// un `eval` en curso, así que el límite se aplica entre scripts: una vez
/// agotado, los scripts restantes se omiten con un warning. El peor caso de
/// sobrepaso es un único script, ya acotado por los límites de iteraciones,
/// recursión y pila de `RuntimeLimits`. En debug Boa es mucho más lento, así
/// que el presupuesto se amplía para no cortar tests y desarrollo.
#[cfg(not(debug_assertions))]
const MAX_SCRIPT_EXECUTION_TIME: Duration = Duration::from_millis(120);
#[cfg(debug_assertions)]
const MAX_SCRIPT_EXECUTION_TIME: Duration = Duration::from_secs(3);
const MAX_SCRIPT_WARNINGS: usize = 8;
const SCRIPT_THREAD_STACK_BYTES: usize = 8 * 1024 * 1024;
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
    used: usize,
    /// Fecha límite compartida por todas las peticiones de la página.
    deadline: Option<Instant>,
    /// Motivos de bloqueo (CORS, límites, red) para avisar en la interfaz.
    warnings: Vec<String>,
}

thread_local! {
    static FETCH_STATE: RefCell<FetchState> = RefCell::new(FetchState::default());
}

const DOM_RUNTIME: &str = include_str!("dom.js");

#[derive(Default)]
pub(crate) struct ScriptResult {
    /// Serialized document produced by `dom.js` when page scripts changed the
    /// tree. `None` means the original Rust-side document is still accurate.
    pub(crate) html: Option<String>,
    pub(crate) warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InitialNode {
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    attributes: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    children: Vec<InitialNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DomSnapshot {
    #[serde(default)]
    html: Option<String>,
    #[serde(default)]
    mutated: bool,
    #[serde(default)]
    oversized: bool,
}

struct ModuleEntry {
    label: String,
    /// Resolved absolute URL, or a synthetic `#wisp-inline-module-N` URL for
    /// inline modules. Used both as the loader cache key and as `import.meta.url`.
    url: String,
    source: String,
}

pub(crate) fn execute_scripts(
    document: &Html,
    external_scripts: &HashMap<String, String>,
    module_scripts: &HashMap<String, String>,
    page_url: Option<&str>,
    viewport_width: f32,
) -> Result<ScriptResult> {
    let page_url_owned = page_url.map(str::to_owned);
    let script_selector =
        Selector::parse("script").map_err(|error| anyhow!("invalid script selector: {error}"))?;
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

    let initial = build_initial_document(document)?;
    let (html, script_warnings) = run_scripts_on_thread(
        initial,
        classic,
        modules,
        module_scripts,
        page_url_owned,
        viewport_width,
    )?;
    for warning in script_warnings {
        push_warning(&mut warnings, warning);
    }

    Ok(ScriptResult { html, warnings })
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

fn build_initial_document(document: &Html) -> Result<InitialNode> {
    let root = document
        .tree
        .root()
        .children()
        .find(|node| matches!(node.value(), Node::Element(element) if element.name() == "html"))
        .ok_or_else(|| anyhow!("the parsed document has no <html> element"))?;
    convert_initial_node(root, 0)?
        .ok_or_else(|| anyhow!("the parsed document has no serializable root"))
}

fn convert_initial_node(node: NodeRef<'_, Node>, depth: usize) -> Result<Option<InitialNode>> {
    ensure!(
        depth <= MAX_TREE_DEPTH,
        "HTML nesting exceeds Wisp's depth limit"
    );
    match node.value() {
        Node::Text(text) => Ok(Some(InitialNode {
            tag: None,
            text: Some(text.text.to_string()),
            attributes: HashMap::new(),
            children: Vec::new(),
        })),
        Node::Element(element) => {
            let attributes = element
                .attrs()
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
                .collect::<HashMap<_, _>>();
            let mut children = Vec::new();
            for child in node.children() {
                if let Some(child) = convert_initial_node(child, depth + 1)? {
                    children.push(child);
                }
            }
            Ok(Some(InitialNode {
                tag: Some(element.name().to_owned()),
                text: None,
                attributes,
                children,
            }))
        }
        Node::Document | Node::Fragment => {
            let mut children = Vec::new();
            for child in node.children() {
                if let Some(child) = convert_initial_node(child, depth + 1)? {
                    children.push(child);
                }
            }
            if children.len() == 1 {
                Ok(children.pop())
            } else {
                Ok(None)
            }
        }
        Node::Doctype(_) | Node::Comment(_) | Node::ProcessingInstruction(_) => Ok(None),
    }
}

fn page_url_json(url: Option<&str>) -> serde_json::Value {
    let Some(parsed) = url.and_then(|url| url::Url::parse(url).ok()) else {
        return serde_json::json!({
            "href": "about:blank",
            "origin": "null",
            "protocol": "about:",
            "host": "",
            "hostname": "",
            "port": "",
            "pathname": "",
            "search": "",
            "hash": ""
        });
    };

    let host = match parsed.host_str() {
        Some(host) => match parsed.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        },
        None => String::new(),
    };
    let origin = if host.is_empty() {
        "null".to_owned()
    } else {
        format!("{}://{host}", parsed.scheme())
    };

    serde_json::json!({
        "href": parsed.as_str(),
        "origin": origin,
        "protocol": format!("{}:", parsed.scheme()),
        "host": host,
        "hostname": parsed.host_str().unwrap_or_default(),
        "port": parsed.port().map(|port| port.to_string()).unwrap_or_default(),
        "pathname": parsed.path(),
        "search": parsed.query().map(|query| format!("?{query}")).unwrap_or_default(),
        "hash": parsed.fragment().map(|fragment| format!("#{fragment}")).unwrap_or_default()
    })
}

/// Resolves ES module specifiers against Wisp's pre-fetched module graph.
///
/// Boa's built-in loaders treat specifiers as filesystem paths, which does not
/// match the URLs browsers use. Wisp stores every module under its resolved
/// absolute URL, so this loader joins relative specifiers with the referrer URL
/// and looks the result up in that map.
struct WispModuleLoader {
    modules: RefCell<HashMap<String, Module>>,
    sources: HashMap<String, String>,
    page_url: Option<String>,
    module_urls: RefCell<HashMap<String, String>>,
}

impl WispModuleLoader {
    fn new(sources: &HashMap<String, String>, page_url: Option<&str>) -> Self {
        Self {
            modules: RefCell::new(HashMap::new()),
            sources: sources.clone(),
            page_url: page_url.map(str::to_owned),
            module_urls: RefCell::new(HashMap::new()),
        }
    }

    fn register(&self, url: &str, module: Module) {
        self.modules.borrow_mut().insert(url.to_owned(), module);
    }

    fn register_url(&self, path: &str, url: &str) {
        self.module_urls
            .borrow_mut()
            .insert(path.to_owned(), url.to_owned());
    }
}

impl ModuleLoader for WispModuleLoader {
    fn load_imported_module(
        self: Rc<Self>,
        referrer: Referrer,
        request: ModuleRequest,
        context: &RefCell<&mut Context>,
    ) -> impl std::future::Future<Output = boa_engine::JsResult<Module>> {
        let specifier = request.specifier().to_std_string_escaped();
        let referrer_url = referrer
            .path()
            .map(|path| path.to_string_lossy().into_owned());
        let result = (|| {
            let base = referrer_url
                .as_deref()
                .or(self.page_url.as_deref())
                .ok_or_else(|| {
                    JsNativeError::typ()
                        .with_message("Wisp cannot resolve a module without a page URL")
                })?;
            let base = base.split('#').next().unwrap_or(base);
            let url = resolve_module_url(base, &specifier).ok_or_else(|| {
                JsNativeError::typ().with_message(format!(
                    "Wisp cannot load the module `{specifier}`: only URL-like specifiers are supported"
                ))
            })?;
            if let Some(module) = self.modules.borrow().get(&url) {
                return Ok(module.clone());
            }
            let source = self.sources.get(&url).ok_or_else(|| {
                JsNativeError::typ().with_message(format!("Wisp did not fetch the module `{url}`"))
            })?;
            let path = PathBuf::from(&url);
            let mut borrow = context.borrow_mut();
            let context = &mut **borrow;
            let module = Module::parse(
                Source::from_bytes(source.as_str()).with_path(&path),
                None,
                context,
            )?;
            self.register_url(&path.to_string_lossy(), &url);
            self.modules.borrow_mut().insert(url, module.clone());
            Ok(module)
        })();
        std::future::ready(result)
    }

    fn init_import_meta(
        self: Rc<Self>,
        import_meta: &JsObject,
        module: &Module,
        context: &mut Context,
    ) {
        let path = module
            .path()
            .map(|path| path.to_string_lossy().into_owned());
        let url = if let Some(p) = path {
            if let Some(mapped_url) = self.module_urls.borrow().get(&p) {
                mapped_url.clone()
            } else {
                p
            }
        } else {
            self.page_url.clone().unwrap_or_default()
        };
        let _ = import_meta.create_data_property_or_throw(
            js_string!("url"),
            js_string!(url.as_str()),
            context,
        );
    }
}

fn resolve_module_url(base_url: &str, specifier: &str) -> Option<String> {
    if !(specifier.starts_with('/')
        || specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier.contains("://"))
    {
        return None;
    }
    let base = url::Url::parse(base_url).ok()?;
    let url = base.join(specifier).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(url.to_string())
}

fn run_scripts_on_thread(
    initial: InitialNode,
    scripts: Vec<(String, String)>,
    modules: Vec<ModuleEntry>,
    module_scripts: &HashMap<String, String>,
    page_url: Option<String>,
    viewport_width: f32,
) -> Result<(Option<String>, Vec<String>)> {
    // Boa can need more native stack than the Windows main thread provides.
    std::thread::scope(|scope| {
        let handle = match std::thread::Builder::new()
            .name("wisp-script".to_owned())
            .stack_size(SCRIPT_THREAD_STACK_BYTES)
            .spawn_scoped(scope, move || {
                evaluate_scripts(
                    initial,
                    scripts,
                    modules,
                    module_scripts,
                    page_url.as_deref(),
                    viewport_width,
                )
            }) {
            Ok(handle) => handle,
            Err(error) => {
                return Ok((
                    None,
                    vec![format!(
                        "No se pudo iniciar el motor de JavaScript de Wisp: {error}"
                    )],
                ));
            }
        };
        match handle.join() {
            Ok(result) => result,
            Err(_) => Ok((
                None,
                vec!["El motor de JavaScript de Wisp falló inesperadamente.".to_owned()],
            )),
        }
    })
}

fn evaluate_scripts(
    initial: InitialNode,
    scripts: Vec<(String, String)>,
    modules: Vec<ModuleEntry>,
    module_scripts: &HashMap<String, String>,
    page_url: Option<&str>,
    viewport_width: f32,
) -> Result<(Option<String>, Vec<String>)> {
    let mut warnings = Vec::new();
    let loader = Rc::new(WispModuleLoader::new(module_scripts, page_url));
    let mut context = Context::builder()
        .module_loader(loader.clone())
        .build()
        .map_err(|error| anyhow!("could not start Wisp's ES module loader: {error}"))?;
    FETCH_STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.page_url = page_url.and_then(|url| url::Url::parse(url).ok());
        state.used = 0;
        state.deadline = Some(Instant::now() + MAX_FETCH_TOTAL_TIME);
        state.warnings.clear();
    });
    if let Err(error) = context.register_global_callable(
        js_string!("__wispFetchSync"),
        2,
        NativeFunction::from_fn_ptr(wisp_fetch_sync),
    ) {
        push_warning(
            &mut warnings,
            format!("No se pudo exponer fetch() en Wisp: {error}"),
        );
    }

    let mut runtime_limits = context.runtime_limits();
    runtime_limits.set_loop_iteration_limit(MAX_LOOP_ITERATIONS);
    runtime_limits.set_recursion_limit(MAX_RECURSION_DEPTH);
    runtime_limits.set_stack_size_limit(MAX_STACK_SIZE);
    context.set_runtime_limits(runtime_limits);

    let initial = serde_json::to_value(&initial)?;
    let initial = JsValue::from_json(&initial, &mut context)
        .map_err(|error| anyhow!("could not initialize Wisp's JavaScript DOM: {error}"))?;
    register_global(
        &mut context,
        "__wispDocument",
        initial,
        &mut warnings,
        "No se pudo iniciar el DOM de JavaScript de Wisp",
    )?;

    let url = page_url_json(page_url);
    let url = JsValue::from_json(&url, &mut context)
        .map_err(|error| anyhow!("could not initialize Wisp's page URL: {error}"))?;
    register_global(
        &mut context,
        "__wispUrl",
        url,
        &mut warnings,
        "No se pudo inicializar la URL de la página",
    )?;

    let viewport = serde_json::json!({ "width": viewport_width });
    let viewport = JsValue::from_json(&viewport, &mut context)
        .map_err(|error| anyhow!("could not initialize Wisp's viewport: {error}"))?;
    register_global(
        &mut context,
        "__wispViewport",
        viewport,
        &mut warnings,
        "No se pudo inicializar el viewport de la página",
    )?;

    if let Err(error) = context.eval(Source::from_bytes(DOM_RUNTIME)) {
        push_warning(
            &mut warnings,
            format!(
                "No se pudo iniciar el DOM de JavaScript de Wisp: {}",
                truncate_error(&error.to_string())
            ),
        );
        return Ok((None, warnings));
    }

    // El presupuesto empieza tras inicializar el runtime DOM: ese eval es
    // infraestructura de Wisp, no script de la página.
    let deadline = Instant::now() + MAX_SCRIPT_EXECUTION_TIME;
    let mut time_budget_exhausted = false;

    for (label, source) in scripts {
        if Instant::now() >= deadline {
            if !time_budget_exhausted {
                time_budget_exhausted = true;
                push_warning(
                    &mut warnings,
                    format!(
                        "Se omitieron scripts por el límite de {} ms de JavaScript de Wisp.",
                        MAX_SCRIPT_EXECUTION_TIME.as_millis()
                    ),
                );
            }
            continue;
        }
        if let Err(error) = context.eval(Source::from_bytes(&source)) {
            push_warning(
                &mut warnings,
                format!("Falló {label}: {}", truncate_error(&error.to_string())),
            );
        }
        if let Err(error) = context.run_jobs() {
            push_warning(
                &mut warnings,
                format!(
                    "Falló una tarea asíncrona de {label}: {}",
                    truncate_error(&error.to_string())
                ),
            );
        }
    }

    for module in modules {
        if Instant::now() >= deadline {
            if !time_budget_exhausted {
                time_budget_exhausted = true;
                push_warning(
                    &mut warnings,
                    format!(
                        "Se omitieron módulos por el límite de {} ms de JavaScript de Wisp.",
                        MAX_SCRIPT_EXECUTION_TIME.as_millis()
                    ),
                );
            }
            continue;
        }
        let path = PathBuf::from(&module.url);
        let parsed = match Module::parse(
            Source::from_bytes(module.source.as_str()).with_path(&path),
            None,
            &mut context,
        ) {
            Ok(parsed) => parsed,
            Err(error) => {
                push_warning(
                    &mut warnings,
                    format!(
                        "No se pudo analizar {}: {}",
                        module.label,
                        truncate_error(&error.to_string())
                    ),
                );
                continue;
            }
        };
        loader.register_url(&path.to_string_lossy(), &module.url);
        loader.register(&module.url, parsed.clone());
        let promise = parsed.load_link_evaluate(&mut context);
        if let Err(error) = context.run_jobs() {
            push_warning(
                &mut warnings,
                format!(
                    "Falló una tarea asíncrona de {}: {}",
                    module.label,
                    truncate_error(&error.to_string())
                ),
            );
        }
        match promise.state() {
            PromiseState::Rejected(reason) => {
                let message = reason
                    .to_string(&mut context)
                    .map(|text| text.to_std_string_escaped())
                    .unwrap_or_else(|_| "error desconocido".to_owned());
                push_warning(
                    &mut warnings,
                    format!("Falló {}: {}", module.label, truncate_error(&message)),
                );
            }
            PromiseState::Pending => {
                push_warning(
                    &mut warnings,
                    format!("{} no terminó de evaluarse.", module.label),
                );
            }
            PromiseState::Fulfilled(_) => {}
        }
    }

    if let Err(error) = context.run_jobs() {
        push_warning(
            &mut warnings,
            format!(
                "Falló una tarea asíncrona tras evaluar módulos: {}",
                truncate_error(&error.to_string())
            ),
        );
    }

    if let Err(error) = context.eval(Source::from_bytes("__wispFinish()")) {
        push_warning(
            &mut warnings,
            format!(
                "Falló el ciclo de vida de la página: {}",
                truncate_error(&error.to_string())
            ),
        );
    }
    if let Err(error) = context.run_jobs() {
        push_warning(
            &mut warnings,
            format!(
                "Falló una tarea asíncrona del ciclo de vida: {}",
                truncate_error(&error.to_string())
            ),
        );
    }

    let fetch_warnings = FETCH_STATE.with(|state| std::mem::take(&mut state.borrow_mut().warnings));
    for warning in fetch_warnings {
        push_warning(&mut warnings, warning);
    }

    let snapshot = read_snapshot(&mut context, &mut warnings);
    for warning in read_runtime_warnings(&mut context, &mut warnings) {
        push_warning(&mut warnings, warning);
    }

    let html = match snapshot {
        Some(snapshot) if snapshot.oversized => {
            push_warning(
                &mut warnings,
                "El DOM generado por JavaScript supera el límite de tamaño de Wisp.".to_owned(),
            );
            None
        }
        Some(snapshot) if snapshot.mutated => snapshot.html,
        _ => None,
    };

    Ok((html, warnings))
}

/// Puente nativo de `fetch()`: hace la petición HTTP de forma síncrona (el
/// motor de scripts ya corre en su propio hilo) y devuelve un JSON con el
/// cuerpo y los metadatos, o `null` si falla o la bloquea CORS.
fn wisp_fetch_sync(_this: &JsValue, args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    let Some(request) = args.first().and_then(|value| value.as_string()) else {
        return Ok(JsValue::null());
    };
    let request = request.to_std_string_escaped();
    let method = args
        .get(1)
        .and_then(|value| value.as_string())
        .map(|value| value.to_std_string_escaped())
        .unwrap_or_else(|| "GET".to_owned());
    Ok(match fetch_json(&request, &method) {
        Ok(json) => JsValue::from(JsString::from(json)),
        Err(error) => {
            record_fetch_warning(&error);
            // La página recibe un objeto con `error` en vez de un `null` mudo:
            // `dom.js` lo convierte en un rechazo de `fetch()` con el motivo.
            JsValue::from(JsString::from(
                serde_json::json!({ "error": error }).to_string(),
            ))
        }
    })
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
    let (page_url, used, deadline) = FETCH_STATE.with(|state| {
        let state = state.borrow();
        (state.page_url.clone(), state.used, state.deadline)
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
    FETCH_STATE.with(|state| state.borrow_mut().used += 1);

    let host = target.host_str().unwrap_or_default().to_owned();
    let timeout = deadline
        .map(|deadline| FETCH_TIMEOUT.min(deadline.saturating_duration_since(Instant::now())))
        .unwrap_or(FETCH_TIMEOUT);
    if timeout.is_zero() {
        return Err(budget_exhausted());
    }
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(3).min(timeout))
        .timeout(timeout)
        .redirect(Policy::limited(5))
        .user_agent("GhostWisp/0.1")
        .build()
        .map_err(|error| format!("fetch(): no se pudo preparar la petición: {error}"))?;
    let request_builder = if method == "HEAD" {
        client.head(target.clone())
    } else {
        client.get(target.clone())
    };
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

fn register_global(
    context: &mut Context,
    name: &str,
    value: JsValue,
    warnings: &mut Vec<String>,
    failure: &str,
) -> Result<()> {
    if let Err(error) = context.register_global_property(js_string!(name), value, Attribute::all())
    {
        push_warning(warnings, format!("{failure}: {error}"));
    }
    Ok(())
}

fn read_snapshot(context: &mut Context, warnings: &mut Vec<String>) -> Option<DomSnapshot> {
    let value = match context.eval(Source::from_bytes("__wispSnapshot()")) {
        Ok(value) => value,
        Err(error) => {
            push_warning(
                warnings,
                format!(
                    "No se pudo leer el DOM de JavaScript de Wisp: {}",
                    truncate_error(&error.to_string())
                ),
            );
            return None;
        }
    };
    match value.to_json(context) {
        Ok(Some(value)) => match serde_json::from_value(value) {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                push_warning(
                    warnings,
                    format!(
                        "No se pudo interpretar el DOM de JavaScript de Wisp: {}",
                        truncate_error(&error.to_string())
                    ),
                );
                None
            }
        },
        Ok(None) => {
            push_warning(
                warnings,
                "El DOM de JavaScript de Wisp no devolvió ninguna instantánea.".to_owned(),
            );
            None
        }
        Err(error) => {
            push_warning(
                warnings,
                format!(
                    "No se pudo leer el DOM de JavaScript de Wisp: {}",
                    truncate_error(&error.to_string())
                ),
            );
            None
        }
    }
}

fn read_runtime_warnings(context: &mut Context, warnings: &mut Vec<String>) -> Vec<String> {
    let value = match context.eval(Source::from_bytes("__wispTakeWarnings()")) {
        Ok(value) => value,
        Err(error) => {
            push_warning(
                warnings,
                format!(
                    "No se pudieron leer los avisos de JavaScript de Wisp: {}",
                    truncate_error(&error.to_string())
                ),
            );
            return Vec::new();
        }
    };
    match value.to_json(context) {
        Ok(Some(value)) => serde_json::from_value::<Vec<String>>(value).unwrap_or_default(),
        Ok(None) => Vec::new(),
        Err(error) => {
            push_warning(
                warnings,
                format!(
                    "No se pudieron leer los avisos de JavaScript de Wisp: {}",
                    truncate_error(&error.to_string())
                ),
            );
            Vec::new()
        }
    }
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
    fn custom_elements_receive_connected_callback() {
        let html = r##"<my-card id="one" data-label="uno">inicio</my-card>
            <script>
                class MyCard extends HTMLElement {
                    connectedCallback() {
                        this.textContent = "tarjeta:" + (this.getAttribute("data-label") || "");
                    }
                }
                customElements.define("my-card", MyCard);
                const second = document.createElement("my-card");
                second.id = "two";
                second.setAttribute("data-label", "dos");
                document.body.appendChild(second);
            </script>"##;
        assert_eq!(snapshot_text(html, "#one"), "tarjeta:uno");
        assert_eq!(snapshot_text(html, "#two"), "tarjeta:dos");
    }

    #[test]
    fn custom_elements_report_invalid_names() {
        let html = r##"<p id="out">antes</p>
            <script>
                try {
                    customElements.define("singuion", class extends HTMLElement {});
                } catch (error) {
                    document.getElementById("out").textContent = "error";
                }
            </script>"##;
        assert_eq!(snapshot_text(html, "#out"), "error");
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
}
