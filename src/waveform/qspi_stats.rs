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

pub struct PacketStats {
    pub packets: Vec<DecodedPacket>,
    pub groups: PacketGroups,
}

impl PacketStats {
    pub fn compute(
        store: &EdgeStore,
        qspi: &QspiConfig,
        transactions: &[Transaction],
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
            return Self { packets, groups };
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

        Self { packets, groups }
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

    let mut clk_val = (clk_first_value + lo as u8) & 1;
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
