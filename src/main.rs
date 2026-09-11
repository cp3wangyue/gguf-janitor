//! GGUF Janitor CLI.
//!
//! `gguf-janitor` with no arguments starts the GUI. Subcommands:
//! scan / dedupe / info / fit / activate.

mod gui;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};

use gguf_janitor::actions;
use gguf_janitor::dedupe::{self, DedupeOptions, DupGroup};
use gguf_janitor::discover;
use gguf_janitor::fit;
use gguf_janitor::gguf;
use gguf_janitor::license::{self, LicenseState};
use gguf_janitor::{human_bytes, ScanResult};

#[derive(Parser)]
#[command(
    name = "gguf-janitor",
    version,
    about = "Find and safely reclaim disk space wasted by duplicate local LLM model files.",
    long_about = "GGUF Janitor scans Ollama, LM Studio, HuggingFace and custom folders for local \
                  model files, finds content-identical duplicates and reclaims space via NTFS \
                  hardlinks, the Recycle Bin, or verified archive moves. Without arguments it \
                  starts the graphical interface."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// Replace duplicates with NTFS hardlinks (same volume; tools keep working).
    Hardlink,
    /// Move duplicates to the Recycle Bin.
    Recycle,
    /// Move duplicates into an archive folder (hash-verified, undo manifest).
    Archive,
}

#[derive(Args)]
struct ScanArgs {
    /// Extra folders to scan (repeatable, comma-separated).
    #[arg(long = "paths", value_delimiter = ',')]
    paths: Vec<String>,
    /// Scan every fixed drive for loose .gguf files.
    #[arg(long)]
    all_drives: bool,
    /// Skip GGUF header parsing (faster; duplicates still found).
    #[arg(long)]
    no_parse: bool,
}

#[derive(Args)]
struct DedupeArgs {
    #[command(flatten)]
    scan: ScanArgs,
    /// What to do with duplicates.
    #[arg(long, value_enum, default_value = "hardlink")]
    mode: Mode,
    /// Destination folder for --mode archive.
    #[arg(long)]
    dest: Option<String>,
    /// Show what would happen, change nothing.
    #[arg(long)]
    dry_run: bool,
    /// Non-interactive: assume yes. Without it (and without --dry-run) you are
    /// asked to confirm before anything is modified.
    #[arg(long)]
    yes: bool,
    /// Minimum duplicate file size in MiB (default 8).
    #[arg(long, default_value_t = 8)]
    min_size_mb: u64,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Scan model stores and print a report.
    Scan {
        #[command(flatten)]
        args: ScanArgs,
        #[arg(long)]
        json: bool,
    },
    /// Find duplicate model files and reclaim space.
    Dedupe(DedupeArgs),
    /// Show GGUF metadata for one file.
    Info { file: PathBuf },
    /// Estimate whether the model fits this machine's RAM/VRAM.
    Fit {
        /// Model file(s); for split GGUFs pass the first shard.
        files: Vec<PathBuf>,
        /// Context size to estimate for.
        #[arg(long, default_value_t = 8192)]
        context: u64,
    },
    /// Move archived files back to their original locations.
    Undo {
        /// Folder containing gguf-janitor-archive.jsonl (the --dest used earlier).
        archive_folder: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    /// Activate a license key.
    Activate { key: String },
}

fn collect(args: &ScanArgs) -> Result<ScanResult> {
    let stores = discover::known_stores()?;
    let mut extra: Vec<PathBuf> = args.paths.iter().map(PathBuf::from).collect();
    if args.all_drives {
        for d in discover::fixed_drives() {
            if !stores.iter().any(|s| s.root.starts_with(&d)) {
                extra.push(d);
            }
        }
    }
    let parse = !args.no_parse;
    let mut progress = |p: &gguf_janitor::ScanProgress| {
        if !p.current.is_empty() {
            print!("\r  working… {}   ", human_bytes(p.bytes_done));
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    };
    let files = discover::collect_files(&stores, &extra, parse, &mut progress);
    println!();
    let summaries = discover::store_summaries(&stores, &files);
    let total_bytes = files.iter().map(|f| f.size).sum();
    Ok(ScanResult { stores: summaries, total_files: files.len() as u64, total_bytes, files })
}

fn group_paths(groups: &[DupGroup]) -> Vec<String> {
    let mut out = Vec::new();
    for g in groups {
        for m in g.members.iter().skip(1) {
            out.push(m.path.clone());
        }
    }
    out
}

/// License gate: a group is actionable in the free tier only if every file to
/// be touched is within the free per-file limit.
fn gated_groups(groups: Vec<DupGroup>, state: LicenseState, json: bool) -> Vec<DupGroup> {
    let (allowed, blocked): (Vec<_>, Vec<_>) = groups
        .into_iter()
        .partition(|g| g.members.iter().all(|m| license::action_allowed(state, m.size)));
    if !blocked.is_empty() && !json {
        let blocked_bytes: u64 = blocked.iter().map(|g| g.reclaimable).sum();
        eprintln!(
            "{} duplicate group(s) ({} reclaimable) exceed the free tier's {} per-file limit. \
             Activate a license to unlock: gguf-janitor activate <key>",
            blocked.len(),
            human_bytes(blocked_bytes),
            human_bytes(license::FREE_FILE_LIMIT)
        );
    }
    allowed
}

fn confirm(prompt: &str, assume_yes: bool) -> bool {
    if assume_yes {
        return true;
    }
    eprint!("{prompt} [y/N] ");
    use std::io::BufRead;
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn mode_label(m: Mode) -> &'static str {
    match m {
        Mode::Hardlink => "hardlink dedupe",
        Mode::Recycle => "Recycle Bin move",
        Mode::Archive => "archive move",
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let Some(cmd) = cli.command else {
        return gui::run();
    };
    match cmd {
        Command::Scan { args, json } => {
            let res = collect(&args)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else {
                println!("Model stores:");
                for s in &res.stores {
                    let status = if s.missing { "not found" } else { "ok" };
                    println!(
                        "  {:<12} {:>4} files  {:>10}  [{}]  {}",
                        s.kind.label(),
                        s.files,
                        human_bytes(s.bytes),
                        status,
                        s.root
                    );
                }
                println!("\nTotal: {} files, {}", res.total_files, human_bytes(res.total_bytes));
            }
        }
        Command::Dedupe(a) => {
            let res = collect(&a.scan)?;
            if res.files.is_empty() {
                bail!("no model files found — pass --paths or check your model stores");
            }
            let opts = DedupeOptions { min_file_size: a.min_size_mb * 1024 * 1024, ..Default::default() };
            let mut progress = |p: &gguf_janitor::ScanProgress| {
                print!("\r  hashing {} / {}   ", human_bytes(p.bytes_done), human_bytes(p.bytes_total));
                use std::io::Write;
                let _ = std::io::stdout().flush();
            };
            let groups_all = dedupe::find_duplicates(&res.files, &opts, &mut progress, &|| false)?;
            println!();
            let total_reclaim: u64 = groups_all.iter().map(|g| g.reclaimable).sum();
            if !a.json {
                for g in &groups_all {
                    println!(
                        "duplicate group — {} each, reclaimable {}",
                        human_bytes(g.size_each),
                        human_bytes(g.reclaimable)
                    );
                    for m in &g.members {
                        println!("    [{}] {}", m.store.label(), m.path);
                    }
                }
                println!("\n{} duplicate group(s), reclaimable: {}", groups_all.len(), human_bytes(total_reclaim));
            }
            if groups_all.is_empty() {
                if a.json {
                    println!("{}", serde_json::json!({"groups": [], "reclaimable_bytes": 0}));
                } else {
                    println!("No duplicates found. Nothing to do.");
                }
                return Ok(());
            }
            let state = license::current_state();
            let groups = gated_groups(groups_all, state, a.json);

            if a.dry_run {
                let reclaim: u64 = groups.iter().map(|g| g.reclaimable).sum();
                if a.json {
                    println!(
                        "{}",
                        serde_json::json!({"dry_run": true, "groups": groups, "reclaimable_bytes": reclaim})
                    );
                } else {
                    println!("Dry run — nothing was changed. Re-run without --dry-run (and optionally --yes) to apply.");
                }
                return Ok(());
            }
            if groups.is_empty() {
                bail!("no duplicate groups within license limits");
            }
            if !confirm(
                &format!("Apply {} to {} group(s) beyond the first copy of each?", mode_label(a.mode), groups.len()),
                a.yes,
            ) {
                println!("Aborted. Nothing was changed.");
                return Ok(());
            }
            let mut total_freed = 0u64;
            match a.mode {
                Mode::Hardlink => {
                    for g in &groups {
                        let r = actions::hardlink_group(g, false)?;
                        total_freed += r.bytes_freed;
                        for item in r.performed {
                            println!("  [{}] {}", if item.ok { "OK" } else { "SKIP" }, item.detail);
                        }
                    }
                }
                Mode::Recycle => {
                    let paths = group_paths(&groups);
                    let r = actions::recycle(&paths, false)?;
                    total_freed += r.bytes_freed;
                    for item in r.performed {
                        println!("  [{}] {} — {}", if item.ok { "OK" } else { "SKIP" }, item.path, item.detail);
                    }
                }
                Mode::Archive => {
                    let Some(dest) = a.dest.clone() else {
                        bail!("--mode archive requires --dest <folder>");
                    };
                    let paths = group_paths(&groups);
                    let r = actions::archive(&paths, PathBuf::from(&dest).as_path(), false)?;
                    total_freed += r.bytes_freed;
                    for item in r.performed {
                        println!("  [{}] {} — {}", if item.ok { "OK" } else { "SKIP" }, item.path, item.detail);
                    }
                }
            }
            println!("\nFreed: {}", human_bytes(total_freed));
            if a.json {
                println!("{}", serde_json::json!({"freed_bytes": total_freed, "groups": groups.len()}));
            }
        }
        Command::Info { file } => {
            let info = gguf::parse_header(&file).context("not a valid GGUF file")?;
            let s = info.summarize();
            println!("file:      {}", file.display());
            println!("size:      {}", human_bytes(file.metadata().map(|m| m.len()).unwrap_or(0)));
            println!("gguf ver:  v{}", info.version);
            println!("name:      {}", s.name.unwrap_or_else(|| "?".into()));
            println!("arch:      {}", s.architecture.unwrap_or_else(|| "?".into()));
            println!("quant:     {}", s.quant.unwrap_or_else(|| "?".into()));
            if let Some(p) = s.parameter_count {
                println!("params:    {:.2}B", p as f64 / 1e9);
            }
            println!("tensors:   {}", info.tensor_count);
            if let Some(c) = s.context_length {
                println!("ctx len:   {c}");
            }
        }
        Command::Fit { files, context } => {
            let dev = fit::probe_devices();
            println!(
                "machine:   RAM {}  VRAM {} ({})\n",
                human_bytes(dev.ram_total),
                if dev.vram_per_gpu.is_empty() {
                    "n/a".to_string()
                } else {
                    dev.vram_per_gpu.iter().map(|v| human_bytes(*v)).collect::<Vec<_>>().join(" + ")
                },
                dev.vram_source
            );
            for f in files {
                let info = gguf::parse_header(&f).context("not a valid GGUF file")?;
                let s = info.summarize();
                let weights = f.metadata().map(|m| m.len()).unwrap_or(0);
                let est = fit::estimate(weights, s.n_layers, s.n_embd, s.n_head, s.n_head_kv, context);
                let v = fit::verdict(&est, &dev);
                println!(
                    "{}: weights {} + kv {} + overhead {} = {} — {}",
                    f.display(),
                    human_bytes(est.weights_bytes),
                    human_bytes(est.kv_cache_bytes),
                    human_bytes(est.overhead_bytes),
                    human_bytes(est.total_estimate_bytes),
                    v.note
                );
            }
        }
        Command::Undo { archive_folder, dry_run } => {
            let r = actions::undo_archive(&archive_folder, dry_run)?;
            for item in &r.performed {
                println!("  [{}] {} — {}", if item.ok { "OK" } else { "SKIP" }, item.path, item.detail);
            }
            let n = r.performed.iter().filter(|i| i.ok).count();
            println!("
{} entrie(s) processed{}", n, if dry_run { " (dry run)" } else { "" });
        }
        Command::Activate { key } => match license::activate(&key) {
            Ok(LicenseState::Pro) => println!("License activated. Thank you for supporting GGUF Janitor!"),
            Ok(_) => println!("Key accepted but does not unlock Pro."),
            Err(e) => bail!("{e}"),
        },
    }
    Ok(())
}
