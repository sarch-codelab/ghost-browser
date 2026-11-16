use anyhow::{Context, Result, bail};
use std::env;
use std::fs;
use std::path::PathBuf;
use wisp_engine::{MAX_DOCUMENT_BYTES, layout_html};

fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let Some(input) = args.next() else {
        print_usage();
        bail!("an HTML file path is required");
    };
    if input.to_string_lossy() == "--help" {
        print_usage();
        return Ok(());
    }
    let input = PathBuf::from(input);
    let width = match args.next() {
        Some(value) => value
            .to_string_lossy()
            .parse::<f32>()
            .context("viewport width must be a number")?,
        None => 800.0,
    };
    if args.next().is_some() {
        print_usage();
        bail!("too many arguments");
    }

    let metadata =
        fs::metadata(&input).with_context(|| format!("could not inspect {}", input.display()))?;
    if metadata.len() > MAX_DOCUMENT_BYTES as u64 {
        bail!(
            "{} is larger than Wisp's {}-byte prototype limit",
            input.display(),
            MAX_DOCUMENT_BYTES
        );
    }
    let html = fs::read_to_string(&input)
        .with_context(|| format!("could not read HTML file {}", input.display()))?;
    let document = layout_html(&html, width)?;
    println!("{}", serde_json::to_string_pretty(&document)?);
    Ok(())
}

fn print_usage() {
    eprintln!("Wisp Engine prototype");
    eprintln!("Usage: wisp-engine <input.html> [viewport-width]");
}
