use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use json_repair_rs::{RepairOptions, load_reader, repair_reader};

#[derive(Debug, Parser)]
#[command(version, about = "Repair malformed JSON from stdin or a file")]
struct Args {
    /// Input file. Reads stdin when omitted.
    input: Option<PathBuf>,

    /// Pretty-print the repaired JSON value with serde_json instead of Python-style spacing.
    #[arg(long)]
    object: bool,

    /// Skip the strict serde_json fast path and run the repair parser immediately.
    #[arg(long)]
    skip_json_loads: bool,

    /// Reject duplicate keys and multiple top-level values instead of repairing them.
    #[arg(long)]
    strict: bool,

    /// Preserve non-ASCII characters instead of escaping them.
    #[arg(long)]
    no_ensure_ascii: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let options = RepairOptions {
        skip_json_loads: args.skip_json_loads,
        strict: args.strict,
        ensure_ascii: !args.no_ensure_ascii,
    };

    if args.object {
        let value = with_input(args.input, |reader| load_reader(reader, options))?;
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        let repaired = with_input(args.input, |reader| repair_reader(reader, options))?;
        println!("{repaired}");
    }
    Ok(())
}

fn with_input<T>(
    input: Option<PathBuf>,
    f: impl FnOnce(Box<dyn Read>) -> Result<T, json_repair_rs::RepairError>,
) -> anyhow::Result<T> {
    let reader: Box<dyn Read> = if let Some(path) = input {
        Box::new(File::open(&path).with_context(|| format!("failed to open {}", path.display()))?)
    } else {
        Box::new(io::stdin())
    };
    f(reader).context("failed to repair JSON")
}
