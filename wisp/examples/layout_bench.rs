use std::collections::HashMap;
use std::time::Instant;
use wisp_engine::FetchedPage;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "examples/big.html".to_owned());
    let width: f32 = std::env::args()
        .nth(2)
        .and_then(|w| w.parse().ok())
        .unwrap_or(1200.0);
    let html = std::fs::read_to_string(&path).unwrap();
    let page = FetchedPage::new(
        "https://bench.local/".to_owned(),
        "bench".to_owned(),
        html,
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        Vec::new(),
    );

    // First layout pays the script stage (Boa + inline scripts).
    let start = Instant::now();
    let first = page.layout(width).unwrap();
    let first_ms = start.elapsed().as_secs_f64() * 1000.0;

    // Cached layouts only redo block layout on prepared roots.
    let mut commands = first.commands.len();
    let start = Instant::now();
    for _ in 0..200 {
        let doc = page.layout(width).unwrap();
        commands = doc.commands.len();
    }
    let elapsed = start.elapsed();
    println!(
        "{path} w={width}: first layout: {first_ms:.1} ms (incluye JS) | 200 layouts cacheados en {:?} -> {:.2} ms/layout, {} comandos",
        elapsed,
        elapsed.as_secs_f64() * 1000.0 / 200.0,
        commands
    );
    std::thread::sleep(std::time::Duration::from_secs(10));
}
