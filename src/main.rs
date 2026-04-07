mod parser;
mod waveform;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::SystemTime;

use eframe::egui;
use log::{info, warn};
use notify::{EventKind, RecursiveMode, Watcher};
use parser::{DslProject, format_duration, format_samplerate, parse_dsl_file};
use waveform::WaveformState;

const PROJECTS_DIR: &str = "./projects";

fn main() -> eframe::Result {
    simple_logger::SimpleLogger::new()
        .with_level(log::LevelFilter::Warn)
        .with_module_level("visor", log::LevelFilter::Debug)
        .init()
        .ok();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Visor")
            .with_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native("Visor", options, Box::new(|_cc| Ok(Box::new(App::new()))))
}

#[derive(Debug, Clone)]
struct ProjectEntry {
    path: PathBuf,
    name: String,
    size: u64,
    modified: Option<SystemTime>,
}

impl ProjectEntry {
    fn from_path(path: PathBuf) -> Option<Self> {
        let name = path.file_name()?.to_string_lossy().to_string();
        let meta = std::fs::metadata(&path).ok()?;
        Some(Self {
            name,
            size: meta.len(),
            modified: meta.modified().ok(),
            path,
        })
    }
}

struct App {
    projects: Vec<ProjectEntry>,
    selected: Option<usize>,
    loaded: Option<Result<DslProject, String>>,
    waveform: Option<WaveformState>,
    renaming: Option<(usize, String)>,
    delete_confirm: Option<usize>,
    watcher: Option<Box<dyn notify::Watcher>>,
    fs_rx: Option<Receiver<()>>,
}

impl App {
    fn new() -> Self {
        let projects_dir = PathBuf::from(PROJECTS_DIR);
        std::fs::create_dir_all(&projects_dir).ok();

        let (tx, rx) = mpsc::channel::<()>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                match event.kind {
                    EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_) => {
                        tx.send(()).ok();
                    }
                    _ => {}
                }
            }
        })
        .ok();

        if let Some(ref mut w) = watcher {
            w.watch(&projects_dir, RecursiveMode::NonRecursive).ok();
        }

        let mut app = Self {
            projects: Vec::new(),
            selected: None,
            loaded: None,
            waveform: None,
            renaming: None,
            delete_confirm: None,
            watcher: watcher.map(|w| Box::new(w) as Box<dyn notify::Watcher>),
            fs_rx: Some(rx),
        };
        app.refresh_projects();
        app
    }

    fn refresh_projects(&mut self) {
        let dir = Path::new(PROJECTS_DIR);
        let mut entries: Vec<ProjectEntry> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "dsl")
                    .unwrap_or(false)
            })
            .filter_map(|e| ProjectEntry::from_path(e.path()))
            .collect();

        entries.sort_by(|a, b| {
            b.modified
                .unwrap_or(SystemTime::UNIX_EPOCH)
                .cmp(&a.modified.unwrap_or(SystemTime::UNIX_EPOCH))
        });

        if let Some(sel) = self.selected {
            if let Some(proj) = self.projects.get(sel) {
                let old_path = proj.path.clone();
                self.selected = entries.iter().position(|e| e.path == old_path);
            }
        }

        self.projects = entries;
    }

    fn load_selected(&mut self) {
        if let Some(idx) = self.selected {
            if let Some(entry) = self.projects.get(idx) {
                let path = entry.path.clone();
                info!("loading project: {}", path.display());
                self.loaded = Some(parse_dsl_file(&path));
                self.waveform = match &self.loaded {
                    Some(Ok(project)) => {
                        info!("parsed ok — {} channels, {} blocks, {:.3}s",
                            project.header.total_probes,
                            project.header.total_blocks,
                            project.duration_secs);
                        match WaveformState::from_project(path.clone(), project) {
                            Ok(wf) => Some(wf),
                            Err(e) => { warn!("waveform init failed: {e}"); None }
                        }
                    }
                    Some(Err(e)) => { warn!("parse error: {e}"); None }
                    _ => None,
                };
            }
        }
    }

    fn commit_rename(&mut self) {
        if let Some((idx, new_name)) = self.renaming.take() {
            if let Some(entry) = self.projects.get(idx) {
                let new_name = new_name.trim().to_string();
                if new_name.is_empty() {
                    return;
                }
                let new_name = if new_name.ends_with(".dsl") {
                    new_name
                } else {
                    format!("{new_name}.dsl")
                };
                let new_path = entry.path.parent().unwrap().join(&new_name);
                if new_path != entry.path {
                    std::fs::rename(&entry.path, &new_path).ok();
                    self.refresh_projects();
                    self.loaded = None;
                    self.waveform = None;
                }
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(rx) = &self.fs_rx {
            if rx.try_recv().is_ok() {
                // drain all pending events
                while rx.try_recv().is_ok() {}
                self.refresh_projects();
            }
        }

        egui::SidePanel::left("project_list")
            .resizable(true)
            .default_width(280.0)
            .show(ctx, |ui| {
                ui.heading("Projects");
                ui.separator();

                if self.projects.is_empty() {
                    ui.label("No .dsl files found.");
                    ui.label(format!("Drop files into: {PROJECTS_DIR}"));
                }

                let mut load_idx: Option<usize> = None;
                let mut start_rename: Option<usize> = None;
                let mut confirm_delete: Option<usize> = None;
                let mut do_commit_rename = false;
                let mut cancel_rename = false;

                let names: Vec<String> = self.projects.iter().map(|e| e.name.clone()).collect();
                let rename_state = self.renaming.as_ref().map(|(i, s)| (*i, s.clone()));

                for (i, name) in names.iter().enumerate() {
                    let is_selected = self.selected == Some(i);

                    ui.horizontal(|ui| {
                        if let Some((rename_idx, _)) = rename_state {
                            if rename_idx == i {
                                if let Some((_, ref mut buf)) = self.renaming {
                                    let resp = ui.add(
                                        egui::TextEdit::singleline(buf).desired_width(160.0),
                                    );
                                    resp.request_focus();
                                    if resp.lost_focus()
                                        || ui.input(|inp| inp.key_pressed(egui::Key::Enter))
                                    {
                                        do_commit_rename = true;
                                    } else if ui.input(|inp| inp.key_pressed(egui::Key::Escape)) {
                                        cancel_rename = true;
                                    }
                                }
                                return;
                            }
                        }

                        let label = egui::SelectableLabel::new(is_selected, name.as_str());
                        let resp = ui.add(label);
                        if resp.double_clicked() {
                            start_rename = Some(i);
                        } else if resp.clicked() {
                            load_idx = Some(i);
                        }

                        if ui.small_button("✏").on_hover_text("Rename").clicked() {
                            start_rename = Some(i);
                        }
                        if ui.small_button("🗑").on_hover_text("Delete").clicked() {
                            confirm_delete = Some(i);
                        }
                    });
                }

                if do_commit_rename {
                    self.commit_rename();
                }
                if cancel_rename {
                    self.renaming = None;
                }

                if let Some(i) = load_idx {
                    if self.selected != Some(i) {
                        self.selected = Some(i);
                        self.loaded = None;
                        self.load_selected();
                    }
                }

                if let Some(i) = start_rename {
                    let name = names[i].clone();
                    let stem = name.strip_suffix(".dsl").unwrap_or(&name).to_string();
                    self.renaming = Some((i, stem));
                }

                if let Some(i) = confirm_delete {
                    self.delete_confirm = Some(i);
                }
            });

        // Delete confirmation dialog
        if let Some(del_idx) = self.delete_confirm {
            if let Some(entry) = self.projects.get(del_idx) {
                let name = entry.name.clone();
                let path = entry.path.clone();
                let mut open = true;
                egui::Window::new("Confirm Delete")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .open(&mut open)
                    .show(ctx, |ui| {
                        ui.label(format!("Delete '{name}'?"));
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui.button("Delete").clicked() {
                                std::fs::remove_file(&path).ok();
                                if self.selected == Some(del_idx) {
                                    self.selected = None;
                                    self.loaded = None;
                                    self.waveform = None;
                                }
                                self.delete_confirm = None;
                                self.refresh_projects();
                            }
                            if ui.button("Cancel").clicked() {
                                self.delete_confirm = None;
                            }
                        });
                    });
                if !open {
                    self.delete_confirm = None;
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            match &self.loaded {
                None => {
                    ui.centered_and_justified(|ui| {
                        ui.label("Select a project from the list to view details.");
                    });
                    return;
                }
                Some(Err(e)) => {
                    ui.colored_label(egui::Color32::RED, format!("Error: {e}"));
                    return;
                }
                Some(Ok(project)) => {
                    egui::CollapsingHeader::new("Capture Info")
                        .default_open(false)
                        .show(ui, |ui| show_project_info(ui, project));
                }
            }

            ui.separator();

            if let Some(wf) = &mut self.waveform {
                wf.show(ui);
            }
        });
    }
}

fn show_project_info(ui: &mut egui::Ui, project: &DslProject) {
    let h = &project.header;

    ui.heading("Capture Info");
    ui.separator();

    egui::Grid::new("info_grid")
        .num_columns(2)
        .spacing([20.0, 6.0])
        .striped(true)
        .show(ui, |ui| {
            ui.label("Driver");
            ui.label(&h.driver);
            ui.end_row();

            ui.label("Sample Rate");
            ui.label(format_samplerate(h.samplerate_hz));
            ui.end_row();

            ui.label("Duration");
            ui.label(format_duration(project.duration_secs));
            ui.end_row();

            ui.label("Total Samples");
            ui.label(format!("{}", h.total_samples));
            ui.end_row();

            ui.label("Channels");
            ui.label(format!("{}", h.total_probes));
            ui.end_row();

            ui.label("Blocks");
            ui.label(format!("{}", h.total_blocks));
            ui.end_row();
        });

    ui.add_space(16.0);
    ui.heading("Channels");
    ui.separator();

    if project.channels.is_empty() {
        // Fall back to header probes list
        egui::Grid::new("probes_grid")
            .num_columns(2)
            .spacing([20.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Index").strong());
                ui.label(egui::RichText::new("Name").strong());
                ui.end_row();
                for (idx, name) in &h.probes {
                    ui.label(format!("{idx}"));
                    ui.label(name);
                    ui.end_row();
                }
            });
    } else {
        egui::Grid::new("channels_grid")
            .num_columns(3)
            .spacing([20.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Index").strong());
                ui.label(egui::RichText::new("Name").strong());
                ui.label(egui::RichText::new("Enabled").strong());
                ui.end_row();
                for ch in &project.channels {
                    ui.label(format!("{}", ch.index));
                    ui.label(&ch.name);
                    ui.label(if ch.enabled { "yes" } else { "no" });
                    ui.end_row();
                }
            });
    }
}
