use std::collections::BTreeMap;

use super::{QspiConfig, QspiRole, Transaction};
use crate::waveform::edges::EdgeStore;

/// CRC32 IEEE 802.3 used as a content fingerprint.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    crc ^ 0xFFFF_FFFF
}

pub struct DecodedPacket {
    pub bytes: Vec<u8>,
    pub crc: u32,
    /// Index into WaveformState::transactions
    pub tx_idx: usize,
}

/// (byte_count, crc) → list of transaction indices
pub type PacketGroups = BTreeMap<(usize, u32), Vec<usize>>;

pub struct TimingStats {
    /// All inter-frame intervals in microseconds, sorted ascending.
    pub intervals_us: Vec<f64>,
    /// Transaction index pairs whose interval is an outlier (≥ 2× median).
    /// Each entry: (tx_index_a, tx_index_b, interval_us) — the gap between tx_a and tx_a+1.
    pub period_outliers: Vec<(usize, usize, f64)>,
    pub min_us: f64,
    pub max_us: f64,
    pub mean_us: f64,
    pub p50_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
    /// Histogram built from non-outlier intervals when exclude_outliers=true, otherwise all.
    pub histogram: Vec<(f64, usize)>,
    pub histogram_excludes_outliers: bool,
}

impl TimingStats {
    pub fn compute(transactions: &[Transaction], samplerate_hz: u64, exclude_outliers: bool) -> Option<Self> {
        if transactions.len() < 2 || samplerate_hz == 0 {
            return None;
        }
        let us_per_sample = 1_000_000.0 / samplerate_hz as f64;

        // Collect raw intervals with their transaction indices.
        let raw: Vec<(usize, usize, f64)> = transactions
            .windows(2)
            .enumerate()
            .map(|(i, w)| (i, i + 1, (w[1].start.saturating_sub(w[0].start)) as f64 * us_per_sample))
            .collect();

        // Compute median from sorted values.
        let mut sorted: Vec<f64> = raw.iter().map(|&(_, _, v)| v).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = sorted.len();
        let median = sorted[n / 2];

        let period_outliers: Vec<(usize, usize, f64)> = raw.iter()
            .filter(|&&(_, _, v)| v >= median * 2.0)
            .copied()
            .collect();

        let intervals_for_stats: &[f64] = if exclude_outliers && !period_outliers.is_empty() {
            // Use sorted values up to the outlier threshold.
            let cutoff = median * 2.0;
            let hi = sorted.partition_point(|&v| v < cutoff);
            &sorted[..hi]
        } else {
            &sorted
        };

        if intervals_for_stats.is_empty() {
            return None;
        }

        let ns = intervals_for_stats.len();
        let min_us = intervals_for_stats[0];
        let max_us = intervals_for_stats[ns - 1];
        let mean_us = intervals_for_stats.iter().sum::<f64>() / ns as f64;
        let p50_us = intervals_for_stats[ns / 2];
        let p95_us = intervals_for_stats[(ns as f64 * 0.95) as usize];
        let p99_us = intervals_for_stats[(ns as f64 * 0.99) as usize];

        const BUCKETS: usize = 60;
        let range = (max_us - min_us).max(1.0);
        let bucket_width = range / BUCKETS as f64;
        let mut histogram = vec![(0.0f64, 0usize); BUCKETS];
        for (i, h) in histogram.iter_mut().enumerate() {
            h.0 = min_us + i as f64 * bucket_width;
        }
        for &v in intervals_for_stats {
            let idx = (((v - min_us) / bucket_width) as usize).min(BUCKETS - 1);
            histogram[idx].1 += 1;
        }

        Some(Self {
            intervals_us: sorted,
            period_outliers,
            min_us, max_us, mean_us, p50_us, p95_us, p99_us,
            histogram,
            histogram_excludes_outliers: exclude_outliers,
        })
    }
}

pub struct PacketStats {
    pub packets: Vec<DecodedPacket>,
    pub groups: PacketGroups,
    pub timing: Option<TimingStats>,
}

impl PacketStats {
    pub fn compute(
        store: &EdgeStore,
        qspi: &QspiConfig,
        transactions: &[Transaction],
        samplerate_hz: u64,
        exclude_timing_outliers: bool,
    ) -> Self {
        let mut packets = Vec::new();
        let mut groups: PacketGroups = BTreeMap::new();

        let (Some(clk_ch), Some(d0_ch), Some(d1_ch), Some(d2_ch), Some(d3_ch)) = (
            qspi.channel_for(QspiRole::Clk),
            qspi.channel_for(QspiRole::D0),
            qspi.channel_for(QspiRole::D1),
            qspi.channel_for(QspiRole::D2),
            qspi.channel_for(QspiRole::D3),
        ) else {
            return Self { packets, groups, timing: None };
        };

        let clk = store.channel(clk_ch as u32);
        let all_clk_transitions = clk.transitions;
        let clk_first_value = clk.first_value;

        for (tx_idx, tx) in transactions.iter().enumerate() {
            let bytes = decode_transaction_bytes(
                store,
                all_clk_transitions,
                clk_first_value,
                d0_ch as u32,
                d1_ch as u32,
                d2_ch as u32,
                d3_ch as u32,
                tx.start,
                tx.end,
            );
            let crc = crc32(&bytes);
            groups.entry((bytes.len(), crc)).or_default().push(tx_idx);
            packets.push(DecodedPacket { bytes, crc, tx_idx });
        }

        let timing = TimingStats::compute(transactions, samplerate_hz, exclude_timing_outliers);
        Self { packets, groups, timing }
    }
}

fn decode_transaction_bytes(
    store: &EdgeStore,
    all_clk_transitions: &[u64],
    clk_first_value: u8,
    d0_ch: u32,
    d1_ch: u32,
    d2_ch: u32,
    d3_ch: u32,
    tx_start: u64,
    tx_end: u64,
) -> Vec<u8> {
    let lo = all_clk_transitions.partition_point(|&t| t < tx_start);
    let hi = all_clk_transitions.partition_point(|&t| t < tx_end);
    let tx_transitions = &all_clk_transitions[lo..hi];

    let mut clk_val = ((clk_first_value as usize + lo) & 1) as u8;
    let mut rising_edges: Vec<u64> = Vec::new();
    for &t in tx_transitions {
        clk_val ^= 1;
        if clk_val == 1 {
            rising_edges.push(t);
        }
    }

    let d0 = store.channel(d0_ch);
    let d1 = store.channel(d1_ch);
    let d2 = store.channel(d2_ch);
    let d3 = store.channel(d3_ch);

    let mut bytes = Vec::new();
    let mut i = 0;
    while i + 1 < rising_edges.len() {
        let ea = rising_edges[i];
        let eb = rising_edges[i + 1];
        let high = (d3.value_at(ea) << 3) | (d2.value_at(ea) << 2) | (d1.value_at(ea) << 1) | d0.value_at(ea);
        let low  = (d3.value_at(eb) << 3) | (d2.value_at(eb) << 2) | (d1.value_at(eb) << 1) | d0.value_at(eb);
        bytes.push((high << 4) | low);
        i += 2;
    }

    bytes
}
