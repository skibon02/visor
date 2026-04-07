use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::PathBuf;

use log::{debug, info};

pub const SAMPLES_PER_BLOCK: u64 = 16_777_216; // 2MB × 8 bits

/// Per-channel sparse signal representation.
/// `transitions` contains absolute sample indices where the signal flips.
/// The signal value at any sample S = (first_value + partition_point(transitions, S+1)) & 1
pub struct SparseChannel {
    pub first_value: u8,
    pub transitions: Vec<u64>,
}

impl SparseChannel {
    fn new() -> Self {
        Self { first_value: 0, transitions: Vec::new() }
    }

    /// Signal value at absolute sample index `s`.
    pub fn value_at(&self, s: u64) -> u8 {
        let n = self.transitions.partition_point(|&t| t <= s);
        (self.first_value + n as u8) & 1
    }

    /// Transitions in [start, end) as a slice.
    pub fn transitions_in_range(&self, start: u64, end: u64) -> &[u64] {
        let lo = self.transitions.partition_point(|&t| t < start);
        let hi = self.transitions.partition_point(|&t| t < end);
        &self.transitions[lo..hi]
    }

    /// Number of transitions in [start, end).
    pub fn transition_count_in_range(&self, start: u64, end: u64) -> usize {
        let lo = self.transitions.partition_point(|&t| t < start);
        let hi = self.transitions.partition_point(|&t| t < end);
        hi - lo
    }
}

/// Per-block edge data before value-chaining is resolved.
struct BlockEdges {
    /// Signal value at bit offset 0 of this block (from the raw bytes).
    first_value: u8,
    /// Bit offsets within this block where signal flips.
    transitions: Vec<u32>,
}

/// Per-channel state tracking in-progress ingestion.
struct ChannelData {
    /// Raw per-block edges (bit offsets within block).
    block_edges: HashMap<u32, BlockEdges>,
    /// The block index up to which values have been resolved and merged into `resolved`.
    /// None = no blocks resolved yet.
    resolved_tail: Option<u32>,
    /// Value at the end of the last resolved block (used to chain into the next).
    tail_value: u8,
    /// Fully resolved merged channel (absolute sample indices).
    resolved: SparseChannel,
}

impl ChannelData {
    fn new() -> Self {
        Self {
            block_edges: HashMap::new(),
            resolved_tail: None,
            tail_value: 0,
            resolved: SparseChannel::new(),
        }
    }

    /// Ingest raw block data: extract transitions and store per-block.
    /// Then try to advance the resolved tail forward.
    fn ingest_raw(&mut self, block_idx: u32, raw: &[u8]) {
        let (first_value, transitions) = extract_block_transitions(raw);
        self.block_edges.insert(block_idx, BlockEdges { first_value, transitions });
        self.advance_resolved_tail(block_idx);
    }

    /// Walk forward from current resolved_tail through consecutive ingested blocks,
    /// merging their edges into `self.resolved` with correct absolute sample indices.
    fn advance_resolved_tail(&mut self, up_to: u32) {
        let start = match self.resolved_tail {
            None => 0,
            Some(t) => t + 1,
        };

        for blk in start..=up_to {
            let Some(edges) = self.block_edges.get(&blk) else { break };

            let block_base = blk as u64 * SAMPLES_PER_BLOCK;

            // For blocks after block 0: if this block's actual first bit differs from
            // what the previous block left us (tail_value), insert a synthetic transition
            // at the block boundary to correct the parity.
            // Block 0 is excluded because channel_first_values already records its true
            // first bit — inserting a transition at sample 0 would double-count it.
            if blk > 0 && edges.first_value != self.tail_value {
                self.resolved.transitions.push(block_base);
                self.tail_value ^= 1;
            }

            for &offset in &edges.transitions {
                self.resolved.transitions.push(block_base + offset as u64);
            }

            self.tail_value = (edges.first_value + edges.transitions.len() as u8) & 1;
            self.resolved_tail = Some(blk);
        }
    }
}

/// Extract transitions (bit offsets within the block) from raw bytes.
/// Returns (first_value, transitions) where first_value is the signal level at bit 0.
fn extract_block_transitions(raw: &[u8]) -> (u8, Vec<u32>) {
    if raw.is_empty() {
        return (0, Vec::new());
    }

    let first_value = raw[0] & 1;
    let mut transitions = Vec::new();
    let mut prev_bit = first_value;

    for (byte_idx, &byte) in raw.iter().enumerate() {
        if byte == 0x00 {
            if prev_bit != 0 {
                transitions.push(byte_idx as u32 * 8);
                prev_bit = 0;
            }
            continue;
        }
        if byte == 0xFF {
            if prev_bit != 1 {
                transitions.push(byte_idx as u32 * 8);
                prev_bit = 1;
            }
            continue;
        }
        for bit in 0..8u32 {
            let v = (byte >> bit) & 1;
            if v != prev_bit {
                transitions.push(byte_idx as u32 * 8 + bit);
                prev_bit = v;
            }
        }
    }

    (first_value, transitions)
}

pub struct EdgeStore {
    /// Per-channel ingestion state and resolved data.
    channels: Vec<ChannelData>,
    /// first_value for each channel (value at sample 0, before block 0 is resolved).
    channel_first_values: Vec<u8>,
    /// Which (channel, block) pairs have been ingested.
    ingested: HashSet<(u32, u32)>,
    /// ZIP handle.
    archive: zip::ZipArchive<BufReader<File>>,
    name_to_index: HashMap<String, usize>,
    pub num_channels: u32,
    pub blocks_per_channel: u32,
    pub total_samples: u64,
}

impl EdgeStore {
    pub fn open(
        path: PathBuf,
        num_channels: u32,
        blocks_per_channel: u32,
        total_samples: u64,
    ) -> Result<Self, String> {
        info!("opening edge store: {}", path.display());
        let file = File::open(&path).map_err(|e| e.to_string())?;
        let reader = BufReader::new(file);
        let mut archive = zip::ZipArchive::new(reader).map_err(|e| e.to_string())?;

        let mut name_to_index = HashMap::new();
        for i in 0..archive.len() {
            let entry = archive.by_index(i).map_err(|e| e.to_string())?;
            name_to_index.insert(entry.name().to_string(), i);
        }
        info!("zip index: {} entries", name_to_index.len());

        let channels = (0..num_channels).map(|_| ChannelData::new()).collect();
        let channel_first_values = vec![0u8; num_channels as usize];
        let ingested = HashSet::new();

        Ok(Self {
            channels,
            channel_first_values,
            ingested,
            archive,
            name_to_index,
            num_channels,
            blocks_per_channel,
            total_samples,
        })
    }

    pub fn ingest_block(&mut self, channel_idx: u32, block_idx: u32) {
        if self.ingested.contains(&(channel_idx, block_idx)) {
            return;
        }

        let name = format!("L-{}/{}", channel_idx, block_idx);
        let Some(&zip_idx) = self.name_to_index.get(&name) else {
            return;
        };

        debug!("ingesting block ch={} blk={}", channel_idx, block_idx);

        let raw = {
            let mut entry = match self.archive.by_index(zip_idx) {
                Ok(e) => e,
                Err(_) => return,
            };
            let mut buf = Vec::with_capacity(entry.size() as usize);
            if entry.read_to_end(&mut buf).is_err() { return; }
            buf
        };

        // Capture first_value of block 0 for channel
        if block_idx == 0 && !raw.is_empty() {
            self.channel_first_values[channel_idx as usize] = raw[0] & 1;
        }

        self.channels[channel_idx as usize].ingest_raw(block_idx, &raw);
        self.ingested.insert((channel_idx, block_idx));

        let ch = &self.channels[channel_idx as usize];
        debug!("  transitions so far: {}", ch.resolved.transitions.len());
    }

    /// Ensure all blocks covering sample range [start, end) are ingested for all channels.
    pub fn ensure_range(&mut self, start: u64, end: u64) {
        let first_block = (start / SAMPLES_PER_BLOCK) as u32;
        let last_block = ((end.saturating_sub(1)) / SAMPLES_PER_BLOCK)
            .min(self.blocks_per_channel as u64 - 1) as u32;

        for ch in 0..self.num_channels {
            // Block 0 must always be loaded first to establish the correct first_value.
            // Without it, value_at() and the rendering parity are wrong for every channel
            // where the true first bit is 1.
            self.ingest_block(ch, 0);

            for blk in first_block..=last_block {
                self.ingest_block(ch, blk);
            }
        }
    }

    pub fn is_block_ingested(&self, channel_idx: u32, block_idx: u32) -> bool {
        self.ingested.contains(&(channel_idx, block_idx))
    }

    /// Returns the resolved SparseChannel for a given channel.
    /// Note: `first_value` on the returned struct may be 0; use `value_at(0)` which
    /// accounts for the true first_value via `channel_first_values`.
    pub fn channel(&self, channel_idx: u32) -> ChannelView<'_> {
        let data = &self.channels[channel_idx as usize];
        ChannelView {
            first_value: self.channel_first_values[channel_idx as usize],
            transitions: &data.resolved.transitions,
        }
    }

    pub fn blocks_for_range(start: u64, end: u64) -> Vec<u32> {
        let first = (start / SAMPLES_PER_BLOCK) as u32;
        let last = (end.saturating_sub(1) / SAMPLES_PER_BLOCK) as u32;
        (first..=last).collect()
    }
}

/// Lightweight view into a channel's edge data with correct first_value.
pub struct ChannelView<'a> {
    pub first_value: u8,
    pub transitions: &'a [u64],
}

impl<'a> ChannelView<'a> {
    pub fn value_at(&self, s: u64) -> u8 {
        let n = self.transitions.partition_point(|&t| t <= s);
        (self.first_value + n as u8) & 1
    }

    pub fn transitions_in_range(&self, start: u64, end: u64) -> &[u64] {
        let lo = self.transitions.partition_point(|&t| t < start);
        let hi = self.transitions.partition_point(|&t| t < end);
        &self.transitions[lo..hi]
    }
}
