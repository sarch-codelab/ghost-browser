#[test]
#[ignore]
fn live_arconet_smoke_test() {
    let url = std::env::var("WISP_LIVE_URL").unwrap_or_else(|_| "https://arconet.es/".to_owned());
    let started = std::time::Instant::now();
    let page = wisp_engine::fetch_url(&url).expect("fetch");
    eprintln!(
        "fetched html={} bytes, stylesheets={}, external_scripts={}, warnings={} in {:?}",
        page.html.len(),
        page.stylesheets.len(),
        page.external_scripts.len(),
        page.warnings.len(),
        started.elapsed()
    );
    let layout_started = std::time::Instant::now();
    let document = page.layout(1200.0).expect("layout");
    eprintln!(
        "layout commands={}, height={}, warnings={:?} in {:?}",
        document.commands.len(),
        document.content_height,
        document.warnings,
        layout_started.elapsed()
    );
    assert!(document.commands.len() > 1);
}
