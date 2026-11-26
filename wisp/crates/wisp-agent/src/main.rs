//! `wisp-agent`: binario headless del motor Wisp.
//!
//! Enlaza únicamente `wisp-engine` (DOM, CSS, layout, paint, JS y red) y no
//! depende de la capa de UI del navegador (`egui`, `wgpu`, `wry`,
//! `webview2-com`). Es el punto de entrada que usarán los agentes y el
//! harness de benchmarks: `fetch` descarga y procesa una URL y vuelca el
//! resultado por stdout, sin ventana ni WebView.

use anyhow::{Context, Result, bail};
use std::env;
use std::path::PathBuf;
use wisp_engine::{FetchOptions, LayoutDocument, PaintCommand, fetch_url_with_options};

mod cdp;
mod pdf;
mod png;
mod render;
mod serve;
mod ws;

pub(crate) const DEFAULT_VIEWPORT_WIDTH: f32 = 1280.0;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("--help") | Some("-h") => {
            print_usage();
            Ok(())
        }
        Some("fetch") => fetch(&args[1..]),
        Some("screenshot") => screenshot(&args[1..]),
        Some("serve") => serve::run(&args[1..]),
        Some(command) => {
            print_usage();
            bail!("unknown command `{command}`")
        }
    }
}

fn fetch(args: &[String]) -> Result<()> {
    let mut url: Option<String> = None;
    let mut dump = String::from("text");
    let mut width = DEFAULT_VIEWPORT_WIDTH;
    let mut options = FetchOptions::from_env();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--dump" => {
                index += 1;
                dump = args
                    .get(index)
                    .context("`--dump` needs a value")?
                    .to_ascii_lowercase();
            }
            "--width" => {
                index += 1;
                width = args
                    .get(index)
                    .context("`--width` needs a value")?
                    .parse()
                    .context("`--width` must be a number")?;
            }
            "--allow-private-network" => {
                options.policy.allow_private_network = true;
            }
            "--stealth" => {
                options.policy.stealth = true;
            }
            "--block-trackers" => {
                options.policy.block_trackers = true;
            }
            "--proxy" => {
                index += 1;
                options.proxy_url = Some(
                    args.get(index)
                        .context("`--proxy` needs a URL")?
                        .to_owned(),
                );
            }
            flag if flag.starts_with("--") => bail!("unknown flag `{flag}`"),
            value => {
                ensure_single_url(&mut url, value)?;
            }
        }
        index += 1;
    }

    let url = url.context("a URL is required, e.g. `wisp-agent fetch https://example.com`")?;
    let page = fetch_url_with_options(&url, &options)?;
    for warning in &page.warnings {
        eprintln!("warning: {warning}");
    }

    match dump.as_str() {
        "original" => print!("{}", page.html),
        "html" => print!("{}", page.document_html()?),
        "markdown" => print!("{}", page.markdown()?),
        "text" | "links" | "assets" => {
            let document = page.layout(width)?;
            for warning in &document.warnings {
                eprintln!("warning: {warning}");
            }
            match dump.as_str() {
                "links" => print_links(&document),
                "assets" => print_assets(&page),
                _ => print!("{}", rendered_text(&document)),
            }
        }
        other => {
            bail!("unknown `--dump` mode `{other}` (original|html|text|links|markdown|assets)")
        }
    }
    Ok(())
}

fn ensure_single_url(url: &mut Option<String>, value: &str) -> Result<()> {
    if url.is_some() {
        bail!("unexpected extra argument `{value}`");
    }
    *url = Some(value.to_string());
    Ok(())
}

/// Descarga `url`, la maqueta y rasteriza el paint list a un PNG.
fn screenshot(args: &[String]) -> Result<()> {
    let mut url: Option<String> = None;
    let mut width = DEFAULT_VIEWPORT_WIDTH;
    let mut out: Option<PathBuf> = None;
    let mut options = FetchOptions::from_env();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--width" => {
                index += 1;
                width = args
                    .get(index)
                    .context("`--width` needs a value")?
                    .parse()
                    .context("`--width` must be a number")?;
            }
            "--out" => {
                index += 1;
                out = Some(PathBuf::from(
                    args.get(index).context("`--out` needs a value")?,
                ));
            }
            "--allow-private-network" => {
                options.policy.allow_private_network = true;
            }
            "--stealth" => {
                options.policy.stealth = true;
            }
            "--block-trackers" => {
                options.policy.block_trackers = true;
            }
            "--proxy" => {
                index += 1;
                options.proxy_url = Some(
                    args.get(index)
                        .context("`--proxy` needs a URL")?
                        .to_owned(),
                );
            }
            flag if flag.starts_with("--") => bail!("unknown flag `{flag}`"),
            value => ensure_single_url(&mut url, value)?,
        }
        index += 1;
    }

    let url = url.context("a URL is required, e.g. `wisp-agent screenshot https://example.com`")?;
    let page = fetch_url_with_options(&url, &options)?;
    for warning in &page.warnings {
        eprintln!("warning: {warning}");
    }
    let shot = render::render(&page, width)?;
    let path = out.unwrap_or_else(|| shot_path("wisp_capture"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("no se pudo crear {}", parent.display()))?;
    }
    std::fs::write(&path, &shot.png)
        .with_context(|| format!("no se pudo escribir {}", path.display()))?;
    // La ruta va por stdout (fácil de encadenar); el detalle, por stderr.
    println!("{}", path.display());
    eprintln!("{}×{} — {} bytes", shot.width, shot.height, shot.png.len());
    Ok(())
}

/// Ruta de captura por defecto: `%TEMP%/ghost_shots/{stem}_{millis}.png`,
/// igual que la convención que usa Ghost.
pub(crate) fn shot_path(stem: &str) -> PathBuf {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    std::env::temp_dir()
        .join("ghost_shots")
        .join(format!("{stem}_{millis}.png"))
}

/// Texto visible en el orden del paint list: es lo que el motor renderiza,
/// no una heurística sobre el marcado.
fn rendered_text(document: &LayoutDocument) -> String {
    let mut out = String::new();
    for command in &document.commands {
        if let PaintCommand::Text { content, .. } = command {
            out.push_str(content);
        }
    }
    out
}

fn print_links(document: &LayoutDocument) {
    let mut seen = Vec::new();
    for command in &document.commands {
        let href = match command {
            PaintCommand::Text { href, .. } | PaintCommand::Image { href, .. } => href.as_deref(),
            _ => None,
        };
        if let Some(href) = href
            && !seen.iter().any(|existing| existing == href)
        {
            seen.push(href.to_string());
        }
    }
    for href in seen {
        println!("{href}");
    }
}

fn print_assets(page: &wisp_engine::FetchedPage) {
    for key in page.images.keys() {
        println!("image\t{key}");
    }
    for key in page.external_scripts.keys() {
        println!("script\t{key}");
    }
    for key in page.module_scripts.keys() {
        println!("module\t{key}");
    }
}

fn print_usage() {
    eprintln!("wisp-agent — headless Wisp engine");
    eprintln!(
        "Usage: wisp-agent fetch <url> [--dump original|html|text|links|markdown|assets] \
         [--width <px>] [--allow-private-network] [--stealth] [--block-trackers] [--proxy <url>]"
    );
    eprintln!(
        "       wisp-agent screenshot <url> [--width <px>] [--out <png>] \
         [--allow-private-network] [--stealth] [--block-trackers] [--proxy <url>]"
    );
    eprintln!(
        "       wisp-agent serve [--port <n>] [--allow-private-network] [--stealth] \
         [--block-trackers] [--proxy <url>] [--workers <n>] [--worker-max-rss <mb>] \
         [--worker-max-age <s>] [--worker-drain-timeout <s>]"
    );
}
