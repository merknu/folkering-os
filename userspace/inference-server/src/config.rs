//! Sampling configuration loaded from disk control sector 258.
//! MCP tools write a "FCTL"-magic block here at runtime to tweak parameters.

use libfolk::sys::block::SECTOR_SIZE;

use crate::consts::{
    CONTROL_SECTOR, DEFAULT_DRIFT_THRESHOLD, DEFAULT_REP_PENALTY, DEFAULT_REP_WINDOW,
    DEFAULT_TEMPERATURE, DEFAULT_TOP_K, DEFAULT_TOP_P,
};

/// Sampling configuration — populated from control sector or defaults.
#[derive(Clone, Copy)]
pub struct SamplingConfig {
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: u32,
    pub rep_penalty: f32,
    pub rep_window: usize,
    pub dump_layer: usize,
    pub drift_threshold: f32,
    pub telemetry_mode: u32, // 0=off, 1=anomalies_only, 2=continuous
}

impl SamplingConfig {
    pub fn defaults() -> Self {
        Self {
            temperature: DEFAULT_TEMPERATURE,
            top_p: DEFAULT_TOP_P,
            top_k: DEFAULT_TOP_K,
            rep_penalty: DEFAULT_REP_PENALTY,
            rep_window: DEFAULT_REP_WINDOW,
            dump_layer: 0,
            drift_threshold: DEFAULT_DRIFT_THRESHOLD,
            telemetry_mode: 0,
        }
    }
}

/// Read control sector (258) and return config. Falls back to defaults if not present.
pub fn read_control_sector() -> SamplingConfig {
    let mut buf = [0u8; SECTOR_SIZE];
    let mut cfg = SamplingConfig::defaults();

    if libfolk::sys::block::read_sector(CONTROL_SECTOR, &mut buf).is_err() {
        return cfg;
    }

    // Check magic "FCTL"
    if &buf[0..4] != b"FCTL" {
        return cfg;
    }

    // Parse fields (all little-endian, 0 = use default)
    let dump_layer = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    if dump_layer <= 29 {
        cfg.dump_layer = dump_layer as usize;
    }

    let temp = f32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
    if temp > 0.0 {
        cfg.temperature = temp;
    }

    let top_p = f32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
    if top_p > 0.0 {
        cfg.top_p = top_p;
    }

    let top_k = u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]);
    cfg.top_k = top_k; // 0 = disabled is valid

    let rep_pen = f32::from_le_bytes([buf[28], buf[29], buf[30], buf[31]]);
    if rep_pen > 0.0 {
        cfg.rep_penalty = rep_pen;
    }

    let rep_win = u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]);
    if rep_win > 0 {
        cfg.rep_window = rep_win as usize;
    }

    let drift_thresh = f32::from_le_bytes([buf[40], buf[41], buf[42], buf[43]]);
    if drift_thresh > 0.0 {
        cfg.drift_threshold = drift_thresh;
    }

    let telem_mode = u32::from_le_bytes([buf[44], buf[45], buf[46], buf[47]]);
    cfg.telemetry_mode = telem_mode;

    cfg
}
