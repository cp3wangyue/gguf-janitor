//! GGUF Janitor graphical interface (egui).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use eframe::egui;

use gguf_janitor::actions;
use gguf_janitor::dedupe::{self, DedupeOptions, DupGroup};
use gguf_janitor::discover::{self, StoreDir};
use gguf_janitor::fit;
use gguf_janitor::license::{self, LicenseState};
use gguf_janitor::{human_bytes, ModelFile, ScanProgress, ScanResult};

#[derive(Default)]
enum Job {
    #[default]
    Idle,
    Running {
        stop: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<ScanOutcome>>,
    },
}

#[derive(Clone)]
enum JobKind {
    Scan,
    DedupeHardlink(Vec<usize>),
    Recycle(Vec<String>),
    Archive(Vec<String>, PathBuf),
}

#[derive(Clone, Default)]
enum ScanOutcome {
    #[default]
    Pending,
    Done {
        scan: ScanResult,
        groups: Vec<DupGroup>,
    },
    Failed(String),
    ActionDone(Vec<String>),
}

struct Shared {
    progress: Option<ScanProgress>,
    outcome: ScanOutcome,
}

struct App {
    shared: Arc<Mutex<Shared>>,
    job: Job,
    job_kind: Arc<Mutex<Option<JobKind>>>,
    extra_paths: String,
    all_drives: bool,
    parse_headers: bool,
    tab: Tab,
    log: Vec<String>,
    confirm: Option<JobKind>,
    show_about: bool,
    license_state: LicenseState,
    show_license: bool,
    license_input: String,
    license_msg: Option<String>,
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Duplicates,
    AllModels,
    Log,
}

impl Default for App {
    fn default() -> Self {
        App {
            shared: Arc::new(Mutex::new(Shared { progress: None, outcome: ScanOutcome::Pending })),
            job: Job::Idle,
            job_kind: Arc::new(Mutex::new(None)),
            extra_paths: String::new(),
            all_drives: false,
            parse_headers: true,
            tab: Tab::Duplicates,
            log: vec!["Welcome to GGUF Janitor. Press Scan to inventory your local models.".into()],
            confirm: None,
            show_about: false,
            license_state: license::current_state(),
            show_license: false,
            license_input: String::new(),
            license_msg: None,
        }
    }
}

impl App {
    fn log(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
        if self.log.len() > 2000 {
            self.log.remove(0);
        }
    }

    fn stores(&self) -> Vec<StoreDir> {
        let mut stores = discover::known_stores().unwrap_or_default();
        if self.all_drives {
            for d in discover::fixed_drives() {
                if !stores.iter().any(|s| s.root.starts_with(&d)) {
                    stores.push(StoreDir { kind: gguf_janitor::StoreKind::Loose, root: d });
                }
            }
        }
        stores
    }

    fn extra_dirs(&self) -> Vec<PathBuf> {
        self.extra_paths
            .split([';', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect()
    }

    fn start_scan(&mut self) {
        if matches!(self.job, Job::Running { .. }) {
            return;
        }
        let stores = self.stores();
        let extra = self.extra_dirs();
        let parse = self.parse_headers;
        let shared = self.shared.clone();
        let job_kind = self.job_kind.clone();
        let stop = Arc::new(AtomicBool::new(false));
        self.shared.lock().unwrap().outcome = ScanOutcome::Pending;
        *job_kind.lock().unwrap() = Some(JobKind::Scan);

        let stop_t = stop.clone();
        let handle = thread::spawn(move || {
            let setp = |p: ScanProgress| {
                if stop_t.load(Ordering::Relaxed) {
                    return;
                }
                shared.lock().unwrap().progress = Some(p);
            };
            let files = {
                let mut cb = |p: &ScanProgress| setp(p.clone());
                discover::collect_files(&stores, &extra, parse, &mut cb)
            };
            if stop_t.load(Ordering::Relaxed) {
                return ScanOutcome::Failed("cancelled".into());
            }
            let summaries = discover::store_summaries(&stores, &files);
            let total_bytes = files.iter().map(|f| f.size).sum();
            let scan = ScanResult { stores: summaries, total_files: files.len() as u64, total_bytes, files };

            let opts = DedupeOptions::default();
            let shared2 = shared.clone();
            let mut cb = |p: &ScanProgress| {
                shared2.lock().unwrap().progress = Some(p.clone());
            };
            match dedupe::find_duplicates(&scan.files, &opts, &mut cb, &|| stop_t.load(Ordering::Relaxed)) {
                Ok(groups) => ScanOutcome::Done { scan, groups },
                Err(e) => ScanOutcome::Failed(e.to_string()),
            }
        });
        self.job = Job::Running { stop, handle: Some(handle) };
    }

    /// Run a reclaiming action on the current duplicate set.
    fn start_action(&mut self, kind: JobKind, label: &str) {
        if matches!(self.job, Job::Running { .. }) {
            return;
        }
        let snapshot = {
            let s = self.shared.lock().unwrap();
            match &s.outcome {
                ScanOutcome::Done { scan, .. } => Some(scan.files.clone()),
                _ => None,
            }
        };
        let Some(files) = snapshot else {
            self.log("No scan results. Scan first.");
            return;
        };
        let job_kind = self.job_kind.clone();
        let stop = Arc::new(AtomicBool::new(false));
        *job_kind.lock().unwrap() = Some(kind);

        let state = self.license_state;
        self.log(format!("{label}…"));
        let handle = thread::spawn(move || {
            // Re-run duplicate detection on the stored files to act on fresh data.
            let opts = DedupeOptions::default();
            let mut cb = |_p: &ScanProgress| {};
            let Ok(dups) = dedupe::find_duplicates(&files, &opts, &mut cb, &|| false) else {
                return ScanOutcome::Failed("re-detection failed".into());
            };
            let mut freed = 0u64;
            let mut lines: Vec<String> = Vec::new();
            match job_kind.lock().unwrap().clone().unwrap() {
                JobKind::DedupeHardlink(idxs) => {
                    for &gi in &idxs {
                        if let Some(g) = dups.get(gi) {
                            if !g.members.iter().all(|m| license::action_allowed(state, m.size)) {
                                lines.push(format!("SKIP (license): {}…", g.members[0].path));
                                continue;
                            }
                            if let Ok(r) = actions::hardlink_group(g, false) {
                                freed += r.bytes_freed;
                                for it in r.performed {
                                    lines.push(format!("{}: {}", if it.ok { "OK" } else { "SKIP" }, it.detail));
                                }
                            }
                        }
                    }
                }
                JobKind::Recycle(paths) => {
                    let allowed: Vec<String> = paths
                        .into_iter()
                        .filter(|p| {
                            let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                            let ok = license::action_allowed(state, size);
                            if !ok {
                                lines.push(format!("SKIP (license): {p}"));
                            }
                            ok
                        })
                        .collect();
                    if let Ok(r) = actions::recycle(&allowed, false) {
                        freed += r.bytes_freed;
                        for it in r.performed {
                            lines.push(format!("{}: {} — {}", if it.ok { "OK" } else { "SKIP" }, it.path, it.detail));
                        }
                    }
                }
                JobKind::Archive(paths, dest) => {
                    let allowed: Vec<String> = paths
                        .into_iter()
                        .filter(|p| {
                            let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                            let ok = license::action_allowed(state, size);
                            if !ok {
                                lines.push(format!("SKIP (license): {p}"));
                            }
                            ok
                        })
                        .collect();
                    if let Ok(r) = actions::archive(&allowed, &dest, false) {
                        freed += r.bytes_freed;
                        for it in r.performed {
                            lines.push(format!("{}: {} — {}", if it.ok { "OK" } else { "SKIP" }, it.path, it.detail));
                        }
                    }
                }
                JobKind::Scan => {}
            }
            lines.push(format!("Done. Bytes freed: {freed} ({}).", human_bytes(freed)));
            ScanOutcome::ActionDone(lines)
        });
        self.job = Job::Running { stop, handle: Some(handle) };
    }

    fn poll_job(&mut self) {
        let done = match &self.job {
            Job::Running { handle, .. } => handle.as_ref().map(|h| h.is_finished()).unwrap_or(false),
            Job::Idle => false,
        };
        if !done {
            return;
        }
        let Job::Running { handle, .. } = std::mem::take(&mut self.job) else { return };
        if let Some(h) = handle {
            match h.join() {
                Ok(ScanOutcome::Done { scan, groups }) => {
                    let reclaim: u64 = groups.iter().map(|g| g.reclaimable).sum();
                    self.log(format!(
                        "Scan complete: {} files ({}) across stores; {} duplicate groups, {} reclaimable.",
                        scan.total_files,
                        human_bytes(scan.total_bytes),
                        groups.len(),
                        human_bytes(reclaim)
                    ));
                    self.shared.lock().unwrap().outcome = ScanOutcome::Done { scan, groups };
                    self.tab = Tab::Duplicates;
                }
                Ok(ScanOutcome::ActionDone(lines)) => {
                    for l in lines {
                        self.log(l);
                    }
                }
                Ok(ScanOutcome::Failed(msg)) => {
                    if msg != "cancelled" {
                        self.log(format!("Error: {msg}"));
                    }
                }
                Ok(ScanOutcome::Pending) => {}
                Err(_) => self.log("Worker thread crashed.".to_string()),
            }
        }
        *self.job_kind.lock().unwrap() = None;
        self.shared.lock().unwrap().progress = None;
        self.license_state = license::current_state();
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_job();
        let running = matches!(self.job, Job::Running { .. });

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("🧹 GGUF Janitor");
                ui.separator();
                ui.separator();
                if ui.add_enabled(!running, egui::Button::new("🔍 Scan")).clicked() {
                    self.start_scan();
                }
                if running && ui.button("Cancel").clicked() {
                    if let Job::Running { stop, .. } = &self.job {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                if ui.add_enabled(!running, egui::Button::new("♻ Re-claim space")).clicked() {
                    let n = match &self.shared.lock().unwrap().outcome {
                        ScanOutcome::Done { groups, .. } => groups.len(),
                        _ => 0,
                    };
                    if n > 0 {
                        self.confirm = Some(JobKind::DedupeHardlink((0..n).collect()));
                    } else {
                        self.log("No duplicate groups. Scan first.");
                    }
                }
                ui.separator();
                let (state_label, tip) = match self.license_state {
                    LicenseState::Pro => ("Pro ✓", "Thank you! All features unlocked."),
                    LicenseState::Free => (
                        "Free",
                        "Free tier: dedupe files up to 1 GiB. gguf-janitor activate <key> unlocks unlimited.",
                    ),
                };
                if ui
                    .add(egui::Button::new(egui::RichText::new(format!("License: {state_label}")).weak()))
                    .clicked()
                {
                    self.show_license = true;
                }
                ui.label(egui::RichText::new("").weak()).on_hover_text(tip);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("?").clicked() {
                        self.show_about = !self.show_about;
                    }
                });
            });
            if let Some(p) = &self.shared.lock().unwrap().progress {
                ui.horizontal(|ui| {
                    let frac = if p.bytes_total > 0 {
                        (p.bytes_done as f32 / p.bytes_total as f32).clamp(0.0, 1.0)
                    } else if p.files_total > 0 {
                        (p.files_done as f32 / p.files_total as f32).clamp(0.0, 1.0)
                    } else {
                        0.2
                    };
                    ui.add(egui::ProgressBar::new(frac).show_percentage());
                    ui.weak(shorten(&p.current, 80));
                });
            }
            ui.add_space(4.0);
        });

        egui::SidePanel::left("settings").show(ctx, |ui| {
            ui.heading("Settings");
            ui.add_space(4.0);
            ui.label("Extra folders (one per line):");
            let resp = ui.add(
                egui::TextEdit::multiline(&mut self.extra_paths)
                    .desired_rows(3)
                    .hint_text("D:\\models\nE:\\hf-mirror"),
            );
            if resp.changed() && self.extra_paths.contains('/') {
                self.extra_paths = self.extra_paths.replace('/', "\\");
            }
            ui.checkbox(&mut self.all_drives, "Scan all fixed drives (slow)");
            ui.checkbox(&mut self.parse_headers, "Parse GGUF headers (info + fit)");
            ui.separator();
            if let ScanOutcome::Done { scan, groups } = &self.shared.lock().unwrap().outcome {
                ui.heading("Found");
                egui::Grid::new("stores").num_columns(3).spacing([8.0, 2.0]).show(ui, |ui| {
                    for s in &scan.stores {
                        if s.missing {
                            continue;
                        }
                        ui.label(s.kind.label());
                        ui.label(format!("{} files", s.files));
                        ui.label(human_bytes(s.bytes));
                        ui.end_row();
                    }
                });
                ui.separator();
                ui.label(format!("Total: {} files — {}", scan.total_files, human_bytes(scan.total_bytes)));
                let reclaim: u64 = groups.iter().map(|g| g.reclaimable).sum();
                ui.label(
                    egui::RichText::new(format!(
                        "Duplicates: {} groups — reclaimable {}",
                        groups.len(),
                        human_bytes(reclaim)
                    ))
                    .strong(),
                );
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Duplicates, "Duplicates");
                ui.selectable_value(&mut self.tab, Tab::AllModels, "All models");
                ui.selectable_value(&mut self.tab, Tab::Log, "Log");
            });
            ui.separator();
            match self.tab {
                Tab::Duplicates => self.show_duplicates(ctx, ui, running),
                Tab::AllModels => self.show_models(ui),
                Tab::Log => self.show_log(ui),
            }
        });

        if self.show_about {
            egui::Window::new("About GGUF Janitor")
                .open(&mut self.show_about)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label("GGUF Janitor v0.1.0");
                    ui.separator();
                    ui.label("Finds duplicate local LLM model files across Ollama, LM Studio,");
                    ui.label("HuggingFace and custom folders, then reclaims space with NTFS");
                    ui.label("hardlinks, the Recycle Bin, or verified archive moves.");
                    ui.add_space(4.0);
                    ui.label("Duplicates are hash-verified (XXH3 + head/tail byte check)");
                    ui.label("before anything is touched. Actions never silently delete:");
                    ui.label("hardlinks keep every tool working; deletes use the Recycle Bin.");
                });
        }

        if self.show_license {
            egui::Window::new("License")
                .open(&mut self.show_license)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    match self.license_state {
                        LicenseState::Pro => ui.label("Pro — all features unlocked. Thank you!"),
                        LicenseState::Free => ui.label(
                            "Free tier: dedupe/archive files up to 1 GiB each.

                             Enter your license key to unlock unlimited sizes.",
                        ),
                    };
                    ui.add_space(6.0);
                    ui.add(egui::TextEdit::singleline(&mut self.license_input).hint_text("GJ-XXXX-XXXX-XXXX-XXXX"));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!self.license_input.trim().is_empty(), egui::Button::new("Activate")).clicked() {
                            match license::activate(self.license_input.trim()) {
                                Ok(LicenseState::Pro) => {
                                    self.license_state = LicenseState::Pro;
                                    self.license_msg = Some("Activated. Thank you for supporting GGUF Janitor!".into());
                                }
                                Ok(_) => self.license_msg = Some("Key accepted but does not unlock Pro.".into()),
                                Err(e) => self.license_msg = Some(format!("Invalid key: {e}")),
                            }
                        }
                    });
                    if let Some(m) = &self.license_msg {
                        ui.label(m);
                    }
                    ui.add_space(4.0);
                    ui.weak("Keys are validated offline; buy once at the link in docs/BUYING.md.");
                });
        }

        if let Some(kind) = self.confirm.clone() {
            let (title, body) = match &kind {
                JobKind::DedupeHardlink(idxs) => {
                    let n = idxs.len();
                    (
                        "Hardlink duplicates?",
                        format!(
                            "Replace each duplicate with an NTFS hardlink to one shared copy.\n\
                             Every tool keeps seeing its own file; disk usage drops.\n\n\
                             {n} group(s) selected. Files are re-verified before replacement.\n\
                             Locked (in-use) files are skipped automatically."
                        ),
                    )
                }
                _ => ("Confirm action", "Apply the selected action?".into()),
            };
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(body);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(egui::RichText::new("Apply").strong()).clicked() {
                            self.confirm = None;
                            self.start_action(kind, "Applying");
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm = None;
                        }
                    });
                });
        }

        ctx.request_repaint_after(std::time::Duration::from_millis(150));
    }
}

impl App {
    fn show_duplicates(&mut self, _ctx: &egui::Context, ui: &mut egui::Ui, running: bool) {
        let outcome = self.shared.lock().unwrap().outcome.clone();
        match outcome {
            ScanOutcome::Pending | ScanOutcome::Failed(_) if !running => {
                ui.vertical_centered(|ui| {
                    ui.add_space(60.0);
                    ui.heading("No scan yet");
                    ui.label("Press Scan to inventory Ollama, LM Studio, HuggingFace");
                    ui.label("and custom folders, then find duplicate model files.");
                });
            }
            ScanOutcome::Done { groups, .. } => {
                if groups.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(60.0);
                        ui.heading("No duplicates found 🎉");
                    });
                    return;
                }
                ui.horizontal(|ui| {
                    let total: u64 = groups.iter().map(|g| g.reclaimable).sum();
                    ui.label(
                        egui::RichText::new(format!(
                            "{} groups — {} reclaimable",
                            groups.len(),
                            human_bytes(total)
                        ))
                        .strong(),
                    );
                });
                ui.add_space(4.0);
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    for (gi, g) in groups.iter().enumerate() {
                        let allowed = g.members.iter().all(|m| license::action_allowed(self.license_state, m.size));
                        egui::CollapsingHeader::new(format!(
                            "{} × {} — saves {} {}",
                            g.members.len(),
                            human_bytes(g.size_each),
                            human_bytes(g.reclaimable),
                            if allowed { "" } else { " (Pro)" }
                        ))
                        .default_open(gi < 3)
                        .show(ui, |ui| {
                            for m in &g.members {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(format!("[{}]", m.store.label())).weak());
                                    let name = if let Some(tag) = &m.ollama_tag {
                                        format!("{tag} ({})", shorten(&m.path, 70))
                                    } else {
                                        shorten(&m.path, 90)
                                    };
                                    ui.label(name).on_hover_text(&m.path);
                                });
                            }
                            ui.add_space(2.0);
                            ui.horizontal(|ui| {
                                if ui.add_enabled(!running && allowed, egui::Button::new("Hardlink duplicates")).clicked() {
                                    self.confirm = Some(JobKind::DedupeHardlink(vec![gi]));
                                }
                                if ui.add_enabled(!running && allowed, egui::Button::new("Recycle duplicates")).clicked() {
                                    let paths = group_paths_for(&groups, &[gi]);
                                    self.confirm = Some(JobKind::Recycle(paths));
                                }
                                if ui.add_enabled(!running && allowed, egui::Button::new("Archive duplicates…")).clicked() {
                                    if let Some(dest) = rfd::FileDialog::new().set_title("Choose archive destination").pick_folder() {
                                        let paths = group_paths_for(&groups, &[gi]);
                                        self.confirm = Some(JobKind::Archive(paths, dest));
                                    }
                                }
                            });
                        });
                    }
                });
            }
            _ => {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    ui.heading("Working…");
                });
            }
        }
    }

    fn show_models(&mut self, ui: &mut egui::Ui) {
        let outcome = self.shared.lock().unwrap().outcome.clone();
        let ScanOutcome::Done { scan, .. } = outcome else {
            ui.weak("Scan first.");
            return;
        };
        let dev = fit::probe_devices();
        ui.label(format!(
            "Machine: RAM {} — VRAM {} ({})",
            human_bytes(dev.ram_total),
            if dev.vram_per_gpu.is_empty() { "n/a".into() } else { dev.vram_per_gpu.iter().map(|v| human_bytes(*v)).collect::<Vec<_>>().join(" + ") },
            dev.vram_source
        ));
        ui.add_space(4.0);
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let files = &scan.files;
            let cap = 500.min(files.len());
            for f in files.iter().take(cap) {
                egui::CollapsingHeader::new(format!(
                    "{} — {} [{}]",
                    display_name(f),
                    human_bytes(f.size),
                    f.store.label()
                ))
                .show(ui, |ui| {
                    ui.label(shorten(&f.path, 110)).on_hover_text(&f.path);
                    if let Some(g) = &f.gguf {
                        egui::Grid::new(format!("g{}", f.path)).num_columns(2).show(ui, |ui| {
                            ui.label("model"); ui.label(g.name.clone().unwrap_or_else(|| "?".into())); ui.end_row();
                            ui.label("arch"); ui.label(g.architecture.clone().unwrap_or_else(|| "?".into())); ui.end_row();
                            ui.label("quant"); ui.label(g.quant.clone().unwrap_or_else(|| "?".into())); ui.end_row();
                            if let Some(p) = g.parameter_count {
                                ui.label("params"); ui.label(format!("{:.2}B", p as f64 / 1e9)); ui.end_row();
                            }
                            if let Some(c) = g.context_length {
                                ui.label("train ctx"); ui.label(c.to_string()); ui.end_row();
                            }
                        });
                        let ctx_len = g.context_length.unwrap_or(8192);
                        let est = fit::estimate(f.size, g.n_layers, g.n_embd, g.n_head, g.n_head_kv, ctx_len);
                        let v = fit::verdict(&est, &dev);
                        ui.label(format!(
                            "Est. run footprint @ {} ctx: {} ({} RAM / VRAM fit: {})",
                            ctx_len, human_bytes(est.total_estimate_bytes), human_bytes(dev.ram_total), v.note
                        ));
                    } else {
                        ui.weak("GGUF header not parsed.");
                    }
                });
            }
            if files.len() > cap {
                ui.weak(format!("… and {} more files (use the CLI report for the full list)", files.len() - cap));
            }
        });
    }

    fn show_log(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink(false).stick_to_bottom(true).show(ui, |ui| {
            for line in &self.log {
                ui.label(line);
            }
        });
    }
}

fn display_name(f: &ModelFile) -> String {
    if let Some(tag) = &f.ollama_tag {
        return tag.clone();
    }
    if let Some(g) = &f.gguf {
        if let Some(n) = &g.name {
            return n.clone();
        }
    }
    std::path::Path::new(&f.path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| f.path.clone())
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let keep = max / 2;
        format!("{}…{}", &s[..keep], &s[s.len() - keep..])
    }
}

fn group_paths_for(groups: &[DupGroup], group_idxs: &[usize]) -> Vec<String> {
    let mut out = Vec::new();
    for &gi in group_idxs {
        if let Some(g) = groups.get(gi) {
            for m in g.members.iter().skip(1) {
                out.push(m.path.clone());
            }
        }
    }
    out
}

pub fn run() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1150.0, 780.0])
            .with_icon(load_icon()),
        ..Default::default()
    };
    eframe::run_native("GGUF Janitor", options, Box::new(|_cc| Ok(Box::new(App::default()))))
        .map_err(|e| anyhow::anyhow!("gui error: {e}"))
}

fn load_icon() -> egui::IconData {
    // Minimal 32x32 broom-ish icon drawn programmatically (no asset files).
    let (w, h) = (32u32, 32u32);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let dx = x as f32 - 16.0;
            let dy = y as f32 - 16.0;
            let in_handle = dy < -4.0 && (dx / 2.2).powi(2) + ((dy + 12.0) / 12.0).powi(2) < 1.0;
            let in_bristles = dy >= -4.0 && dy < 14.0 && dx.abs() < (6.0 + (dy + 4.0) * 0.8);
            if in_handle {
                rgba[i] = 200;
                rgba[i + 1] = 140;
                rgba[i + 2] = 60;
                rgba[i + 3] = 255;
            } else if in_bristles {
                rgba[i] = 70;
                rgba[i + 1] = 160;
                rgba[i + 2] = 220;
                rgba[i + 3] = 255;
            }
        }
    }
    egui::IconData { width: w, height: h, rgba }
}
