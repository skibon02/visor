mod edges;
mod loader;
mod viewport;
mod qspi_stats;

use std::collections::HashSet;
use std::sync::mpsc;

use eframe::egui::{self, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};

use edges::EdgeStore;
use loader::{LoadRequest, LoadResult, spawn_loader};
use viewport::{RenderLayout, ViewState, LABEL_WIDTH_PX, LANE_HEIGHT_PX};
use qspi_stats::PacketStats;

use crate::parser::DslProject;

const SIGNAL_COLOR: Color32 = Color32::from_rgb(100, 220, 100);
const SIGNAL_UNKNOWN_COLOR: Color32 = Color32::from_rgb(50, 50, 50);
const LANE_BG_COLOR: Color32 = Color32::from_rgb(20, 20, 20);
const LANE_BG_ALT_COLOR: Color32 = Color32::from_rgb(25, 25, 28);
const LABEL_COLOR: Color32 = Color32::from_rgb(180, 180, 180);
const DECODE_LANE_HEIGHT: f32 = 32.0;
const DECODE_BG_COLOR: Color32 = Color32::from_rgb(15, 15, 25);
const QSPI_LABEL_BG: Color32 = Color32::from_rgb(100, 40, 160);
const QSPI_LABEL_FG: Color32 = Color32::WHITE;
const QSPI_DOT_COLOR: Color32 = Color32::RED;
const TRANSACTION_LINE_COLOR: Color32 = Color32::from_rgba_premultiplied(80, 180, 255, 60);
const NAV_BTN_BG: Color32 = Color32::from_rgb(50, 50, 70);
const NAV_BTN_BG_HOT: Color32 = Color32::from_rgb(80, 80, 110);
const NAV_BTN_FG: Color32 = Color32::from_rgb(200, 200, 255);
const NAV_BTN_SIZE: f32 = 22.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QspiRole {
    Clk,
    Cs,
    D0,
    D1,
    D2,
    D3,
}

impl QspiRole {
    const ALL: [QspiRole; 6] = [
        QspiRole::Clk,
        QspiRole::Cs,
        QspiRole::D0,
        QspiRole::D1,
        QspiRole::D2,
        QspiRole::D3,
    ];

    fn label(self) -> &'static str {
        match self {
            QspiRole::Clk => "CLK",
            QspiRole::Cs  => "!CS",
            QspiRole::D0  => "D0",
            QspiRole::D1  => "D1",
            QspiRole::D2  => "D2",
            QspiRole::D3  => "D3",
        }
    }
}

pub struct QspiConfig {
    pub enabled: bool,
    pub channel_roles: Vec<QspiRole>,
}

impl QspiConfig {
    fn new(num_channels: usize) -> Self {
        let roles = [QspiRole::D0, QspiRole::D1, QspiRole::D2, QspiRole::D3, QspiRole::Clk, QspiRole::Cs];
        let channel_roles = (0..num_channels)
            .map(|i| roles[i % roles.len()])
            .collect();
        Self { enabled: false, channel_roles }
    }

    fn channel_for(&self, role: QspiRole) -> Option<usize> {
        self.channel_roles.iter().position(|&r| r == role)
    }
}

/// A QSPI transaction: CS active window with at least one CLK edge.
#[derive(Clone, Copy)]
pub struct Transaction {
    pub start: u64,
    pub end: u64,
}

pub struct WaveformState {
    pub view: ViewState,
    pub store: EdgeStore,
    pub channel_names: Vec<String>,
    pub qspi: QspiConfig,
    pub samplerate_hz: u64,
    transactions: Vec<Transaction>,
    transactions_config_gen: u64,
    qspi_config_gen: u64,
    /// How many CS blocks were ingested when transactions were last built.
    /// Rebuilt whenever more CS blocks arrive.
    cs_blocks_at_last_tx_build: u32,
    packet_stats: Option<PacketStats>,
    stats_config_gen: u64,
    exclude_timing_outliers: bool,
    loader_tx: mpsc::SyncSender<LoadRequest>,
    loader_res_rx: mpsc::Receiver<LoadResult>,
    requested: HashSet<(u32, u32)>,
    _loader_handle: std::thread::JoinHandle<()>,
}

impl WaveformState {
    pub fn from_project(
        path: std::path::PathBuf,
        project: &DslProject,
        ctx: egui::Context,
    ) -> Result<Self, String> {
        let num_channels = project.header.total_probes as usize;
        let blocks_per_channel = project.header.total_blocks;
        let total_samples = project.header.total_samples;

        let store = EdgeStore::open(path.clone(), num_channels as u32, blocks_per_channel, total_samples)?;
        let view = ViewState::new(total_samples);

        let channel_names: Vec<String> = if !project.channels.is_empty() {
            project.channels.iter().map(|c| c.name.clone()).collect()
        } else {
            project.header.probes.iter().map(|(_, n)| n.clone()).collect()
        };

        let qspi = QspiConfig::new(num_channels);
        let samplerate_hz = project.header.samplerate_hz;

        let name_to_index = store.clone_name_to_index();
        let (loader_tx, req_rx) = mpsc::sync_channel::<LoadRequest>(256);
        let (res_tx, loader_res_rx) = mpsc::channel::<LoadResult>();
        let handle = spawn_loader(path, name_to_index, req_rx, res_tx, ctx);

        Ok(Self {
            view,
            store,
            channel_names,
            qspi,
            samplerate_hz,
            transactions: Vec::new(),
            transactions_config_gen: 0,
            qspi_config_gen: 1,
            cs_blocks_at_last_tx_build: 0,
            packet_stats: None,
            stats_config_gen: 0,
            exclude_timing_outliers: false,
            loader_tx,
            loader_res_rx,
            requested: HashSet::new(),
            _loader_handle: handle,
        })
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        let num_channels = self.channel_names.len();
        if num_channels == 0 {
            return;
        }

        // ---- QSPI controls ----
        ui.horizontal(|ui| {
            let start_time = self.view.sample_offset as f64 / self.samplerate_hz as f64;
            ui.label(egui::RichText::new(format!("T+ {}", crate::parser::format_duration(start_time))).monospace().strong());
            ui.separator();

            let before = self.qspi.enabled;
            ui.checkbox(&mut self.qspi.enabled, "QSPI decode");
            if self.qspi.enabled != before {
                self.qspi_config_gen += 1;
            }
            if self.qspi.enabled {
                ui.separator();
                let n = self.channel_names.len();
                for ch in 0..n {
                    let ch_name = self.channel_names[ch].clone();
                    let current = self.qspi.channel_roles[ch];
                    ui.label(&ch_name);
                    egui::ComboBox::from_id_salt(format!("qspi_ch_{}", ch))
                        .selected_text(current.label())
                        .width(48.0)
                        .show_ui(ui, |ui| {
                            for &role in &QspiRole::ALL {
                                let selected = current == role;
                                if ui.selectable_label(selected, role.label()).clicked() && !selected {
                                    if let Some(other) = self.qspi.channel_roles.iter().position(|&r| r == role) {
                                        self.qspi.channel_roles[other] = current;
                                    }
                                    self.qspi.channel_roles[ch] = role;
                                    self.qspi_config_gen += 1;
                                }
                            }
                        });
                    ui.add_space(4.0);
                }
            }
        });

        // ---- Packet stats ----
        if self.qspi.enabled {
            let mut toggle_exclude_timing_outliers = false;
            egui::CollapsingHeader::new("Packet Stats")
                .default_open(true)
                .show(ui, |ui| {
                    match &self.packet_stats {
                        None if self.transactions.is_empty() => {
                            ui.label("No transactions found.");
                        }
                        None => {
                            ui.label("Loading…");
                            ui.ctx().request_repaint();
                        }
                        Some(stats) => {
                            // Summary line
                            let total = stats.packets.len();
                            let outlier_threshold = ((total as f32 * 0.06) as usize).max(1);
                            let outliers: usize = stats.groups.values()
                                .map(|v| v.len())
                                .filter(|&c| c < outlier_threshold)
                                .sum();
                            ui.horizontal(|ui| {
                                ui.label(format!("Packets: {}", total));
                                ui.separator();
                                ui.label(format!("Groups: {}", stats.groups.len()));
                                ui.separator();
                                let outlier_color = if outliers == 0 {
                                    Color32::from_rgb(80, 200, 80)
                                } else {
                                    Color32::from_rgb(220, 80, 80)
                                };
                                ui.colored_label(outlier_color, format!("Outliers (<6%): {}", outliers));
                            });

                            ui.add_space(4.0);

                            // Table: one row per (byte_count, crc32) group, sorted by count desc
                            let mut rows: Vec<_> = stats.groups.iter().collect();
                            rows.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

                            egui::Grid::new("packet_stats_grid")
                                .num_columns(5)
                                .spacing([16.0, 3.0])
                                .striped(true)
                                .show(ui, |ui| {
                                    ui.label(egui::RichText::new("Bytes").strong());
                                    ui.label(egui::RichText::new("CRC32").strong());
                                    ui.label(egui::RichText::new("Count").strong());
                                    ui.label(egui::RichText::new("% of total").strong());
                                    ui.end_row();

                                    for ((byte_count, crc), tx_indices) in &rows {
                                        let count = tx_indices.len();
                                        let pct = count as f32 / total as f32 * 100.0;
                                        let is_outlier = count < outlier_threshold;
                                        let row_color = if is_outlier {
                                            Color32::from_rgb(220, 80, 80)
                                        } else {
                                            Color32::from_rgb(180, 180, 180)
                                        };
                                        ui.colored_label(row_color, format!("{}", byte_count));
                                        ui.colored_label(row_color, format!("{:08X}", crc));
                                        ui.colored_label(row_color, format!("{}", count));
                                        ui.colored_label(row_color, format!("{:.2}%", pct));

                                        if is_outlier {
                                            if ui.button(format!("Jump ({})", count)).clicked() {
                                                let current_pos = self.view.sample_offset;
                                                // Find the next packet in this group starting after current view
                                                let next_tx_idx = tx_indices.iter()
                                                    .map(|&idx| &self.transactions[idx])
                                                    .find(|tx| tx.start > current_pos + (105.0 * self.view.samples_per_pixel) as u64)
                                                    .or_else(|| tx_indices.first().map(|&idx| &self.transactions[idx]))
                                                    .map(|tx| tx.start);

                                                if let Some(start) = next_tx_idx {
                                                    self.view.sample_offset = start.saturating_sub((100.0 * self.view.samples_per_pixel) as u64);
                                                }
                                            }
                                        } else {
                                            ui.label("");
                                        }
                                        ui.end_row();
                                    }
                                });

                            // ---- Timing distribution ----
                            if let Some(ref t) = stats.timing {
                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Inter-frame timing").strong());
                                    if !t.period_outliers.is_empty() {
                                        ui.separator();
                                        let mut checked = self.exclude_timing_outliers;
                                        if ui.checkbox(&mut checked, "Exclude period outliers from histogram").changed() {
                                            toggle_exclude_timing_outliers = true;
                                        }
                                    }
                                });

                                // Period outliers list
                                if !t.period_outliers.is_empty() {
                                    let outlier_count = t.period_outliers.len();
                                    let period_outliers: Vec<(usize, f64)> = t.period_outliers.iter()
                                        .map(|&(tx_a, _, v)| (tx_a, v))
                                        .collect();
                                    egui::CollapsingHeader::new(
                                        egui::RichText::new(format!("Period outliers (≥2× median): {}", outlier_count))
                                            .color(Color32::from_rgb(220, 140, 60))
                                    )
                                    .id_salt("period_outliers")
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        egui::ScrollArea::vertical()
                                            .max_height(120.0)
                                            .id_salt("period_outliers_scroll")
                                            .show(ui, |ui| {
                                                let mut jump_to: Option<u64> = None;
                                                for &(tx_a, interval_us) in &period_outliers {
                                                    ui.horizontal(|ui| {
                                                        ui.colored_label(
                                                            Color32::from_rgb(220, 140, 60),
                                                            format!("tx#{} → #{}: {:.1}µs", tx_a, tx_a + 1, interval_us),
                                                        );
                                                        if ui.small_button("Jump").clicked() {
                                                            if let Some(tx) = self.transactions.get(tx_a + 1) {
                                                                jump_to = Some(tx.start);
                                                            }
                                                        }
                                                    });
                                                }
                                                if let Some(start) = jump_to {
                                                    self.view.sample_offset = start.saturating_sub(
                                                        (100.0 * self.view.samples_per_pixel) as u64
                                                    );
                                                }
                                            });
                                    });
                                }

                                ui.horizontal(|ui| {
                                    if t.histogram_excludes_outliers {
                                        ui.colored_label(Color32::from_rgb(180, 140, 60), "(outliers excluded)");
                                        ui.separator();
                                    }
                                    ui.label(format!("min {:.1}µs", t.min_us));
                                    ui.separator();
                                    ui.label(format!("p50 {:.1}µs", t.p50_us));
                                    ui.separator();
                                    ui.label(format!("p95 {:.1}µs", t.p95_us));
                                    ui.separator();
                                    ui.label(format!("p99 {:.1}µs", t.p99_us));
                                    ui.separator();
                                    ui.label(format!("max {:.1}µs", t.max_us));
                                    ui.separator();
                                    ui.label(format!("mean {:.1}µs", t.mean_us));
                                });

                                // Histogram bar chart
                                let max_count = t.histogram.iter().map(|b| b.1).max().unwrap_or(1).max(1);
                                let bar_area = ui.available_width().min(600.0);
                                let bar_w = (bar_area / t.histogram.len() as f32).max(1.0);
                                let bar_h = 48.0;

                                let (resp, painter) = ui.allocate_painter(
                                    Vec2::new(bar_w * t.histogram.len() as f32, bar_h + 14.0),
                                    Sense::hover(),
                                );
                                let rect = resp.rect;

                                for (i, &(bucket_us, count)) in t.histogram.iter().enumerate() {
                                    let fill_frac = count as f32 / max_count as f32;
                                    let x = rect.left() + i as f32 * bar_w;
                                    let bar_top = rect.top() + bar_h * (1.0 - fill_frac);
                                    let bar_rect = Rect::from_min_max(
                                        Pos2::new(x, bar_top),
                                        Pos2::new(x + bar_w - 1.0, rect.top() + bar_h),
                                    );

                                    let color = if bucket_us >= t.p99_us {
                                        Color32::from_rgb(220, 80, 80)
                                    } else if bucket_us >= t.p95_us {
                                        Color32::from_rgb(220, 160, 60)
                                    } else {
                                        Color32::from_rgb(80, 140, 220)
                                    };
                                    painter.rect_filled(bar_rect, 0.0, color);

                                    if i == 0 || i == t.histogram.len() / 2 || i == t.histogram.len() - 1 {
                                        painter.text(
                                            Pos2::new(x + bar_w / 2.0, rect.top() + bar_h + 2.0),
                                            egui::Align2::CENTER_TOP,
                                            format!("{:.0}", bucket_us),
                                            FontId::proportional(9.0),
                                            Color32::from_rgb(140, 140, 140),
                                        );
                                    }
                                }

                                // p50/p95/p99 marker lines
                                for (pct_us, label, color) in [
                                    (t.p50_us, "p50", Color32::from_rgb(100, 200, 100)),
                                    (t.p95_us, "p95", Color32::from_rgb(220, 160, 60)),
                                    (t.p99_us, "p99", Color32::from_rgb(220, 80, 80)),
                                ] {
                                    let range = (t.max_us - t.min_us).max(1.0);
                                    let x = rect.left() + ((pct_us - t.min_us) / range) as f32 * bar_w * t.histogram.len() as f32;
                                    painter.line_segment(
                                        [Pos2::new(x, rect.top()), Pos2::new(x, rect.top() + bar_h)],
                                        Stroke::new(1.0, color),
                                    );
                                    painter.text(
                                        Pos2::new(x + 2.0, rect.top() + 1.0),
                                        egui::Align2::LEFT_TOP,
                                        label,
                                        FontId::proportional(8.0),
                                        color,
                                    );
                                }
                            }
                        }
                    }
                });
            if toggle_exclude_timing_outliers {
                self.exclude_timing_outliers = !self.exclude_timing_outliers;
                self.stats_config_gen = self.stats_config_gen.wrapping_sub(1);
            }
        }

        // ---- Waveform area ----
        let decode_height = if self.qspi.enabled { DECODE_LANE_HEIGHT } else { 0.0 };
        let waveform_lanes_height = LANE_HEIGHT_PX * num_channels as f32;
        let total_height = (decode_height + waveform_lanes_height).min(ui.available_height());

        let (resp, painter) = ui.allocate_painter(
            Vec2::new(ui.available_width(), total_height),
            Sense::click_and_drag(),
        );

        let rect = resp.rect;
        let waveform_width = (rect.width() - LABEL_WIDTH_PX).max(0.0);

        let hover_pos = resp.hover_pos();
        let (scroll_delta, ctrl_held) =
            ui.input(|inp| (inp.raw_scroll_delta, inp.modifiers.ctrl));

        if resp.hovered() {
            if ctrl_held && scroll_delta.y != 0.0 {
                let cursor_x = hover_pos
                    .map(|p| (p.x - rect.left() - LABEL_WIDTH_PX).max(0.0))
                    .unwrap_or(waveform_width / 2.0);
                let factor = if scroll_delta.y > 0.0 { 1.30f64 } else { 1.0 / 1.30 };
                self.view.zoom(factor, cursor_x, waveform_width);
            } else if !ctrl_held && scroll_delta.x != 0.0 {
                self.view.pan(-scroll_delta.x, waveform_width);
            } else if !ctrl_held && scroll_delta.y != 0.0 {
                self.view.pan(-scroll_delta.y, waveform_width);
            }
        }

        self.view.clamp(waveform_width);

        let layout = self.view.layout([rect.width(), rect.height()], num_channels);
        self.preload_viewport(&layout);

        // When QSPI is enabled, eagerly load all CS blocks so transactions are discovered
        // across the full recording, not just the visible viewport.
        if self.qspi.enabled {
            if let Some(cs_ch) = self.qspi.channel_for(QspiRole::Cs) {
                for blk in 0..self.store.blocks_per_channel {
                    self.request_block(cs_ch as u32, blk);
                }
            }
        }

        // Rebuild transaction list when config changes OR when new CS blocks have been loaded.
        if self.qspi.enabled {
            if let Some(cs_ch) = self.qspi.channel_for(QspiRole::Cs) {
                let cs_loaded: u32 = (0..self.store.blocks_per_channel)
                    .filter(|&b| self.store.is_block_ingested(cs_ch as u32, b))
                    .count() as u32;
                let config_changed = self.transactions_config_gen != self.qspi_config_gen;
                let more_data = cs_loaded > self.cs_blocks_at_last_tx_build;
                if (config_changed || more_data) && cs_loaded > 0 {
                    self.transactions = find_transactions(&self.store, &self.qspi);
                    self.transactions_config_gen = self.qspi_config_gen;
                    self.cs_blocks_at_last_tx_build = cs_loaded;
                    // Invalidate stats — transaction list changed
                    self.stats_config_gen = self.transactions_config_gen.wrapping_sub(1);
                    self.packet_stats = None;
                }
            }
        }

        // Trigger full recording load for stats — one block per frame to avoid stalling.
        if self.qspi.enabled
            && !self.transactions.is_empty()
            && self.stats_config_gen != self.transactions_config_gen
        {
            if let (Some(clk_ch), Some(cs_ch), Some(d0_ch), Some(d1_ch), Some(d2_ch), Some(d3_ch)) = (
                self.qspi.channel_for(QspiRole::Clk),
                self.qspi.channel_for(QspiRole::Cs),
                self.qspi.channel_for(QspiRole::D0),
                self.qspi.channel_for(QspiRole::D1),
                self.qspi.channel_for(QspiRole::D2),
                self.qspi.channel_for(QspiRole::D3),
            ) {
                let last_tx_end = self.transactions.last().map(|t| t.end).unwrap_or(0);
                let last_block = (last_tx_end / edges::SAMPLES_PER_BLOCK)
                    .min(self.store.blocks_per_channel as u64 - 1) as u32;
                let needed_chs = [clk_ch as u32, cs_ch as u32, d0_ch as u32, d1_ch as u32, d2_ch as u32, d3_ch as u32];
                let all_loaded = needed_chs.iter().all(|&ch|
                    (0..=last_block).all(|b| self.store.is_block_ingested(ch, b))
                );
                if all_loaded {
                    self.packet_stats = Some(PacketStats::compute(
                        &self.store,
                        &self.qspi,
                        &self.transactions,
                        self.samplerate_hz,
                        self.exclude_timing_outliers,
                    ));
                    self.stats_config_gen = self.transactions_config_gen;
                } else {
                    // Queue all missing blocks — loader thread processes them in background
                    for b in 0..=last_block {
                        for &ch in &needed_chs {
                            self.request_block(ch, b);
                        }
                    }
                    ui.ctx().request_repaint();
                }
            }
        }

        // Draw signal lanes
        for (ch_idx, name) in self.channel_names.clone().iter().enumerate() {
            let lane_top = rect.top() + decode_height + ch_idx as f32 * LANE_HEIGHT_PX;

            let bg = if ch_idx % 2 == 0 { LANE_BG_COLOR } else { LANE_BG_ALT_COLOR };
            painter.rect_filled(
                Rect::from_min_size(
                    Pos2::new(rect.left(), lane_top),
                    Vec2::new(rect.width(), LANE_HEIGHT_PX),
                ),
                0.0,
                bg,
            );

            painter.text(
                Pos2::new(rect.left() + LABEL_WIDTH_PX / 2.0, lane_top + LANE_HEIGHT_PX / 2.0),
                egui::Align2::CENTER_CENTER,
                name,
                FontId::proportional(11.0),
                LABEL_COLOR,
            );

            let wave_rect = Rect::from_min_size(
                Pos2::new(rect.left() + LABEL_WIDTH_PX, lane_top),
                Vec2::new(waveform_width, LANE_HEIGHT_PX),
            );

            draw_channel(&painter, wave_rect, ch_idx as u32, &layout, &self.store);
        }

        // Draw QSPI decode lane and transaction overlays
        if self.qspi.enabled {
            let decode_top = rect.top();
            let decode_rect = Rect::from_min_size(
                Pos2::new(rect.left(), decode_top),
                Vec2::new(rect.width(), DECODE_LANE_HEIGHT),
            );

            painter.rect_filled(decode_rect, 0.0, DECODE_BG_COLOR);
            painter.text(
                Pos2::new(rect.left() + LABEL_WIDTH_PX / 2.0, decode_top + DECODE_LANE_HEIGHT / 2.0),
                egui::Align2::CENTER_CENTER,
                "QSPI",
                FontId::proportional(10.0),
                LABEL_COLOR,
            );

            let wave_decode_rect = Rect::from_min_size(
                Pos2::new(rect.left() + LABEL_WIDTH_PX, decode_top),
                Vec2::new(waveform_width, DECODE_LANE_HEIGHT),
            );

            draw_qspi_decode(&painter, wave_decode_rect, &layout, &self.store, &self.qspi);

            // Draw transaction boundary lines spanning all lanes
            let full_h_rect = Rect::from_min_size(
                Pos2::new(rect.left() + LABEL_WIDTH_PX, rect.top()),
                Vec2::new(waveform_width, total_height),
            );
            draw_transaction_lines(&painter, full_h_rect, &layout, &self.transactions);
        }

        // Nav buttons — drawn last so they appear on top
        let mut jump_target: Option<u64> = None;
        if self.qspi.enabled && !self.transactions.is_empty() {
            let first_sample = layout.first_sample;
            let last_sample = first_sample + layout.viewport_samples;

            // Find the leftmost transaction that overlaps or is to the right of viewport start
            let first_visible = self.transactions.partition_point(|t| t.end < first_sample);
            // Find the rightmost transaction that starts before viewport end
            let last_visible = self.transactions.partition_point(|t| t.start <= last_sample);

            // Prev button: jump to transaction before the first visible one
            if first_visible > 0 {
                let btn_rect = Rect::from_min_size(
                    Pos2::new(rect.left() + LABEL_WIDTH_PX + 2.0, rect.top() + decode_height + 4.0),
                    Vec2::splat(NAV_BTN_SIZE),
                );
                let hovered = hover_pos.map(|p| btn_rect.contains(p)).unwrap_or(false);
                painter.rect_filled(btn_rect, 4.0, if hovered { NAV_BTN_BG_HOT } else { NAV_BTN_BG });
                painter.text(btn_rect.center(), egui::Align2::CENTER_CENTER, "◀", FontId::proportional(12.0), NAV_BTN_FG);
                if hovered && resp.clicked() {
                    jump_target = Some(self.transactions[first_visible - 1].start);
                }
            }

            // Next button: jump to transaction after the last visible one
            if last_visible < self.transactions.len() {
                let btn_rect = Rect::from_min_size(
                    Pos2::new(rect.right() - LABEL_WIDTH_PX - 2.0 - NAV_BTN_SIZE, rect.top() + decode_height + 4.0),
                    Vec2::splat(NAV_BTN_SIZE),
                );
                let hovered = hover_pos.map(|p| btn_rect.contains(p)).unwrap_or(false);
                painter.rect_filled(btn_rect, 4.0, if hovered { NAV_BTN_BG_HOT } else { NAV_BTN_BG });
                painter.text(btn_rect.center(), egui::Align2::CENTER_CENTER, "▶", FontId::proportional(12.0), NAV_BTN_FG);
                if hovered && resp.clicked() {
                    jump_target = Some(self.transactions[last_visible].start);
                }
            }
        }

        if let Some(target_start) = jump_target {
            // Pan so that target transaction starts at 15% from the left edge
            let offset_px = waveform_width * 0.15;
            let new_offset = (target_start as f64 - offset_px as f64 * self.view.samples_per_pixel)
                .max(0.0) as u64;
            self.view.sample_offset = new_offset;
            self.view.clamp(waveform_width);
            ui.ctx().request_repaint();
        }

        if self.has_missing_data(&layout) {
            ui.ctx().request_repaint();
        }
    }

    fn preload_viewport(&mut self, layout: &RenderLayout) {
        self.drain_loader_results();

        let start = layout.first_sample;
        let end = layout.first_sample + layout.viewport_samples;
        let first_block = (start / edges::SAMPLES_PER_BLOCK) as u32;
        let last_block = ((end.saturating_sub(1)) / edges::SAMPLES_PER_BLOCK)
            .min(self.store.blocks_per_channel as u64 - 1) as u32;

        for ch in 0..self.store.num_channels {
            self.request_block(ch, 0);
            for blk in first_block..=last_block {
                self.request_block(ch, blk);
            }
        }
    }

    fn request_block(&mut self, channel_idx: u32, block_idx: u32) {
        if self.store.is_block_ingested(channel_idx, block_idx) {
            return;
        }
        let key = (channel_idx, block_idx);
        if self.requested.contains(&key) {
            return;
        }
        self.requested.insert(key);
        if self.loader_tx.try_send(LoadRequest::LoadBlock { channel_idx, block_idx }).is_err() {
            // Channel full — remove from requested so it's retried next frame
            self.requested.remove(&key);
        }
    }

    fn drain_loader_results(&mut self) {
        while let Ok(result) = self.loader_res_rx.try_recv() {
            self.store.apply_loaded(result);
        }
    }

    fn has_missing_data(&self, layout: &RenderLayout) -> bool {
        let block_indices = EdgeStore::blocks_for_range(
            layout.first_sample,
            layout.first_sample + layout.viewport_samples,
        );
        for ch in 0..self.store.num_channels {
            for &bi in &block_indices {
                if bi < self.store.blocks_per_channel
                    && !self.store.is_block_ingested(ch, bi)
                {
                    return true;
                }
            }
        }
        false
    }
}

/// Find all QSPI transactions (CS active windows) across the entire recording.
fn find_transactions(store: &EdgeStore, qspi: &QspiConfig) -> Vec<Transaction> {
    let Some(cs_ch) = qspi.channel_for(QspiRole::Cs) else { return Vec::new() };
    let cs = store.channel(cs_ch as u32);

    let mut transactions = Vec::new();
    // CS active = value 0. Walk all CS transitions to find active windows.
    let mut val = cs.first_value;
    let mut active_start: Option<u64> = if val == 0 { Some(0) } else { None };

    for &t in cs.transitions {
        val ^= 1;
        if val == 0 {
            // CS just went active
            active_start = Some(t);
        } else {
            // CS just went inactive
            if let Some(start) = active_start.take() {
                transactions.push(Transaction { start, end: t });
            }
        }
    }
    // If CS is still active at end of recording
    if let Some(start) = active_start {
        transactions.push(Transaction { start, end: store.total_samples });
    }

    transactions
}

fn draw_transaction_lines(
    painter: &egui::Painter,
    rect: Rect,
    layout: &RenderLayout,
    transactions: &[Transaction],
) {
    if transactions.is_empty() {
        return;
    }
    let spp = layout.samples_per_pixel;
    let first_sample = layout.first_sample;
    let last_sample = first_sample + layout.viewport_samples;

    let sample_to_x = |s: u64| -> f32 {
        rect.left() + (s.saturating_sub(first_sample) as f64 / spp) as f32
    };

    let stroke = Stroke::new(1.0, TRANSACTION_LINE_COLOR);

    // Minimum pixel gap between drawn lines — skip when transactions are denser than this.
    const MIN_LINE_GAP_PX: f32 = 4.0;
    let mut last_x = f32::NEG_INFINITY;

    let first_idx = transactions.partition_point(|t| t.end < first_sample);
    for t in &transactions[first_idx..] {
        if t.start > last_sample {
            break;
        }
        let x_start = sample_to_x(t.start);
        let x_end = sample_to_x(t.end);
        if x_start >= rect.left() && x_start <= rect.right() && x_start >= last_x + MIN_LINE_GAP_PX {
            painter.line_segment([Pos2::new(x_start, rect.top()), Pos2::new(x_start, rect.bottom())], stroke);
            last_x = x_start;
        }
        if x_end >= rect.left() && x_end <= rect.right() && x_end >= last_x + MIN_LINE_GAP_PX {
            painter.line_segment([Pos2::new(x_end, rect.top()), Pos2::new(x_end, rect.bottom())], stroke);
            last_x = x_end;
        }
    }
}

fn draw_channel(
    painter: &egui::Painter,
    rect: Rect,
    channel_idx: u32,
    layout: &RenderLayout,
    store: &EdgeStore,
) {
    let spp = layout.samples_per_pixel;
    let first_sample = layout.first_sample;
    let last_sample = first_sample + layout.viewport_samples;

    let y_high = rect.top() + rect.height() * 0.15;
    let y_low = rect.bottom() - rect.height() * 0.15;
    let stroke = Stroke::new(1.5, SIGNAL_COLOR);
    let stroke_unk = Stroke::new(1.0, SIGNAL_UNKNOWN_COLOR);

    let first_block = (first_sample / edges::SAMPLES_PER_BLOCK) as u32;
    let last_block = (last_sample.saturating_sub(1) / edges::SAMPLES_PER_BLOCK)
        .min(store.blocks_per_channel as u64 - 1) as u32;

    for blk in first_block..=last_block {
        if !store.is_block_ingested(channel_idx, blk) {
            let block_start = blk as u64 * edges::SAMPLES_PER_BLOCK;
            let block_end = block_start + edges::SAMPLES_PER_BLOCK;
            let vis_start = block_start.max(first_sample);
            let vis_end = block_end.min(last_sample);
            let x0 = rect.left() + (vis_start.saturating_sub(first_sample) as f64 / spp) as f32;
            let x1 = rect.left() + (vis_end.saturating_sub(first_sample) as f64 / spp) as f32;
            painter.line_segment(
                [Pos2::new(x0, rect.center().y), Pos2::new(x1, rect.center().y)],
                stroke_unk,
            );
        }
    }

    let ch = store.channel(channel_idx);
    let transitions = ch.transitions_in_range(first_sample, last_sample);
    let n_before = ch.transitions.partition_point(|&t| t < first_sample);
    let start_val = (ch.first_value + n_before as u8) & 1;

    let sample_to_x = |s: u64| -> f32 {
        rect.left() + (s.saturating_sub(first_sample) as f64 / spp) as f32
    };
    let y_for = |v: u8| -> f32 { if v == 1 { y_high } else { y_low } };

    let mut cur_val = start_val;
    let mut cur_x = rect.left();
    let mut cur_y = y_for(cur_val);
    let end_x = sample_to_x(last_sample).min(rect.right());

    let mut i = 0;
    while i < transitions.len() {
        let edge_x = sample_to_x(transitions[i]).clamp(rect.left(), rect.right());

        if edge_x > cur_x {
            painter.line_segment([Pos2::new(cur_x, cur_y), Pos2::new(edge_x, cur_y)], stroke);
        }

        let mut count = 0usize;
        while i < transitions.len()
            && sample_to_x(transitions[i]).clamp(rect.left(), rect.right()) <= edge_x + 0.5
        {
            count += 1;
            i += 1;
        }

        let next_val = (cur_val + count as u8) & 1;
        let next_y = y_for(next_val);

        if cur_y != next_y {
            painter.line_segment([Pos2::new(edge_x, cur_y), Pos2::new(edge_x, next_y)], stroke);
        } else {
            painter.line_segment([Pos2::new(edge_x, y_high), Pos2::new(edge_x, y_low)], stroke);
        }

        cur_x = edge_x;
        cur_y = next_y;
        cur_val = next_val;
    }

    if end_x > cur_x {
        painter.line_segment([Pos2::new(cur_x, cur_y), Pos2::new(end_x, cur_y)], stroke);
    }
}

struct QspiWord {
    byte: u8,
    sample_a: u64,
    sample_b: u64,
}

fn decode_qspi(
    store: &EdgeStore,
    qspi: &QspiConfig,
    first_sample: u64,
    last_sample: u64,
) -> Vec<QspiWord> {
    let (Some(clk_ch), Some(cs_ch), Some(d0_ch), Some(d1_ch), Some(d2_ch), Some(d3_ch)) = (
        qspi.channel_for(QspiRole::Clk),
        qspi.channel_for(QspiRole::Cs),
        qspi.channel_for(QspiRole::D0),
        qspi.channel_for(QspiRole::D1),
        qspi.channel_for(QspiRole::D2),
        qspi.channel_for(QspiRole::D3),
    ) else {
        return Vec::new();
    };

    let clk = store.channel(clk_ch as u32);
    let cs  = store.channel(cs_ch as u32);

    let search_start = first_sample.saturating_sub(last_sample - first_sample);

    let clk_transitions = clk.transitions_in_range(search_start, last_sample);
    let transitions_before = clk.transitions.partition_point(|&t| t < search_start);
    let mut clk_val = (clk.first_value + transitions_before as u8) & 1;

    let mut rising_edges: Vec<u64> = Vec::new();
    for &t in clk_transitions {
        clk_val ^= 1;
        if clk_val == 1 {
            rising_edges.push(t);
        }
    }

    let mut words = Vec::new();
    let mut i = 0;

    while i < rising_edges.len() {
        if cs.value_at(rising_edges[i]) != 0 {
            i += 1;
            continue;
        }

        if i + 1 >= rising_edges.len() {
            break;
        }
        let edge_a = rising_edges[i];
        let edge_b = rising_edges[i + 1];

        if cs.value_at(edge_b) != 0 {
            i += 1;
            continue;
        }

        let d0a = store.channel(d0_ch as u32).value_at(edge_a);
        let d1a = store.channel(d1_ch as u32).value_at(edge_a);
        let d2a = store.channel(d2_ch as u32).value_at(edge_a);
        let d3a = store.channel(d3_ch as u32).value_at(edge_a);

        let d0b = store.channel(d0_ch as u32).value_at(edge_b);
        let d1b = store.channel(d1_ch as u32).value_at(edge_b);
        let d2b = store.channel(d2_ch as u32).value_at(edge_b);
        let d3b = store.channel(d3_ch as u32).value_at(edge_b);

        let high_nibble = (d3a << 3) | (d2a << 2) | (d1a << 1) | d0a;
        let low_nibble  = (d3b << 3) | (d2b << 2) | (d1b << 1) | d0b;
        let byte = (high_nibble << 4) | low_nibble;

        if edge_b >= first_sample {
            words.push(QspiWord { byte, sample_a: edge_a, sample_b: edge_b });
        }

        i += 2;
    }

    words
}

fn draw_qspi_decode(
    painter: &egui::Painter,
    rect: Rect,
    layout: &RenderLayout,
    store: &EdgeStore,
    qspi: &QspiConfig,
) {
    let spp = layout.samples_per_pixel;
    let first_sample = layout.first_sample;
    let last_sample = first_sample + layout.viewport_samples;

    let sample_to_x = |s: u64| -> f32 {
        rect.left() + (s.saturating_sub(first_sample) as f64 / spp) as f32
    };

    let words = decode_qspi(store, qspi, first_sample, last_sample);

    let dot_y = rect.bottom() - 3.0;
    let label_top = rect.top() + 2.0;
    let label_bot = rect.bottom() - 8.0;
    let label_h = label_bot - label_top;

    let mut next_x = rect.left();

    for word in &words {
        let x_a = sample_to_x(word.sample_a).clamp(rect.left(), rect.right());
        let x_b = sample_to_x(word.sample_b).clamp(rect.left(), rect.right());

        let label_x0 = x_a.min(x_b);
        let label_x1 = x_a.max(x_b);
        let label_width = (label_x1 - label_x0).max(20.0);

        if label_x0 < next_x {
            continue;
        }
        if label_x0 > rect.right() {
            break;
        }

        let label_rect = Rect::from_min_size(
            Pos2::new(label_x0, label_top),
            Vec2::new(label_width, label_h),
        );

        next_x = label_rect.right() + 1.0;

        painter.rect_filled(label_rect, 3.0, QSPI_LABEL_BG);
        painter.text(
            label_rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("{:02X}", word.byte),
            FontId::monospace(13.0),
            QSPI_LABEL_FG,
        );

        painter.circle_filled(Pos2::new(x_a, dot_y), 2.0, QSPI_DOT_COLOR);
        painter.circle_filled(Pos2::new(x_b, dot_y), 2.0, QSPI_DOT_COLOR);
    }
}
