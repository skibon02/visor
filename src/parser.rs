use std::io::Read;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct DslHeader {
    pub version: u32,
    pub driver: String,
    pub device_mode: u32,
    pub total_samples: u64,
    pub total_probes: u32,
    pub total_blocks: u32,
    pub samplerate_hz: u64,
    pub trigger_time: u64,
    pub trigger_pos: u64,
    pub probes: Vec<(u32, String)>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionChannel {
    pub index: u32,
    pub name: String,
    pub enabled: bool,
    #[serde(rename = "type")]
    pub channel_type: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionData {
    #[serde(rename = "Sample rate")]
    pub sample_rate: Option<String>,
    #[serde(rename = "Sample count")]
    pub sample_count: Option<String>,
    pub channel: Option<Vec<SessionChannel>>,
}

#[derive(Debug, Clone)]
pub struct DslProject {
    pub header: DslHeader,
    pub channels: Vec<SessionChannel>,
    pub duration_secs: f64,
}

fn parse_header(text: &str) -> Result<DslHeader, String> {
    let mut version = 0u32;
    let mut driver = String::new();
    let mut device_mode = 0u32;
    let mut total_samples = 0u64;
    let mut total_probes = 0u32;
    let mut total_blocks = 0u32;
    let mut samplerate_hz = 0u64;
    let mut trigger_time = 0u64;
    let mut trigger_pos = 0u64;
    let mut probes: Vec<(u32, String)> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('[') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "version" => version = value.parse().unwrap_or(0),
            "driver" => driver = value.to_string(),
            "device mode" => device_mode = value.parse().unwrap_or(0),
            "total samples" => total_samples = value.parse().unwrap_or(0),
            "total probes" => total_probes = value.parse().unwrap_or(0),
            "total blocks" => total_blocks = value.parse().unwrap_or(0),
            "samplerate" => samplerate_hz = parse_samplerate(value),
            "trigger time" => trigger_time = value.parse().unwrap_or(0),
            "trigger pos" => trigger_pos = value.parse().unwrap_or(0),
            k if k.starts_with("probe") => {
                let idx_str = k.trim_start_matches("probe");
                if let Ok(idx) = idx_str.parse::<u32>() {
                    probes.push((idx, value.to_string()));
                }
            }
            _ => {}
        }
    }

    Ok(DslHeader {
        version,
        driver,
        device_mode,
        total_samples,
        total_probes,
        total_blocks,
        samplerate_hz,
        trigger_time,
        trigger_pos,
        probes,
    })
}

fn parse_samplerate(s: &str) -> u64 {
    let parts: Vec<&str> = s.splitn(2, ' ').collect();
    let value: f64 = parts[0].parse().unwrap_or(0.0);
    let multiplier = if parts.len() > 1 {
        match parts[1].to_uppercase().as_str() {
            "GHZ" => 1_000_000_000.0,
            "MHZ" => 1_000_000.0,
            "KHZ" => 1_000.0,
            _ => 1.0,
        }
    } else {
        1.0
    };
    (value * multiplier) as u64
}

pub fn format_samplerate(hz: u64) -> String {
    if hz >= 1_000_000_000 {
        format!("{:.3} GHz", hz as f64 / 1_000_000_000.0)
    } else if hz >= 1_000_000 {
        format!("{:.3} MHz", hz as f64 / 1_000_000.0)
    } else if hz >= 1_000 {
        format!("{:.3} kHz", hz as f64 / 1_000.0)
    } else {
        format!("{} Hz", hz)
    }
}

pub fn format_duration(secs: f64) -> String {
    if secs < 0.001 {
        format!("{:.3} µs", secs * 1_000_000.0)
    } else if secs < 1.0 {
        format!("{:.3} ms", secs * 1_000.0)
    } else {
        format!("{:.3} s", secs)
    }
}

pub fn parse_dsl_zip<R: Read + std::io::Seek>(reader: R) -> Result<DslProject, String> {
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| e.to_string())?;

    let header_text = {
        let mut f = archive.by_name("header").map_err(|_| "missing header".to_string())?;
        let mut s = String::new();
        f.read_to_string(&mut s).map_err(|e| e.to_string())?;
        s
    };

    let session_text = {
        let mut f = archive.by_name("session").map_err(|_| "missing session".to_string())?;
        let mut s = String::new();
        f.read_to_string(&mut s).map_err(|e| e.to_string())?;
        s
    };

    let header = parse_header(&header_text)?;
    let session: SessionData = serde_json::from_str(&session_text)
        .map_err(|e| format!("session JSON parse error: {e}"))?;

    let channels = session.channel.unwrap_or_default();

    let samplerate = if let Some(ref sr) = session.sample_rate {
        sr.parse::<u64>().unwrap_or(header.samplerate_hz)
    } else {
        header.samplerate_hz
    };

    // Sum actual data bytes from L-0 channel blocks to get real sample count.
    // Each byte holds 8 single-bit samples; all channels have identical block counts.
    let actual_samples = {
        let mut channel_bytes: u64 = 0;
        for i in 0..archive.len() {
            if let Ok(f) = archive.by_index(i) {
                let name = f.name().to_string();
                if name.starts_with("L-0/") && !name.ends_with('/') {
                    channel_bytes += f.size();
                }
            }
        }
        channel_bytes * 8
    };

    let total_samples = if actual_samples > 0 {
        actual_samples
    } else if let Some(ref sc) = session.sample_count {
        sc.parse::<u64>().unwrap_or(header.total_samples)
    } else {
        header.total_samples
    };

    let duration_secs = if samplerate > 0 {
        total_samples as f64 / samplerate as f64
    } else {
        0.0
    };

    Ok(DslProject {
        header,
        channels,
        duration_secs,
    })
}

pub fn parse_dsl_file(path: &std::path::Path) -> Result<DslProject, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    parse_dsl_zip(file)
}
