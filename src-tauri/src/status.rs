//! The printer's state as the app shows it, updated from MQTT reports (`device/<serial>/report`).
//!
//! A1/P1 printers send partial reports, so every report field is optional and only the fields a
//! message carries change the status. Fields are parsed leniently: numbers may arrive as strings,
//! and a field with an unexpected shape counts as missing instead of discarding the message.

use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DefaultOnError, DisplayFromStr, PickFirst, Same};

#[cfg(test)]
mod tests;

// ── App-facing state (mirrored in vite-env.d.ts) ─────────────────────────────

#[derive(Debug, Clone, Serialize, Default)]
pub struct PrinterStatus {
    /// The active nozzle.
    pub nozzle_temp: f64,
    pub nozzle_target: f64,
    /// Both nozzles of a dual-nozzle printer; empty on single-nozzle printers.
    pub nozzles: Vec<Nozzle>,
    pub bed_temp: f64,
    pub bed_target: f64,
    /// Only for printers with a chamber sensor.
    pub chamber: Option<Temperature>,
    pub progress: u8,
    pub remaining_mins: u32,
    pub layer_num: u32,
    pub total_layer_num: u32,
    pub stage: String,
    pub gcode_state: String,
    pub ams: Vec<AmsUnit>,
    /// Loaded external spools.
    pub external_spools: Vec<ExternalSpool>,
    pub chamber_light: bool,
    pub spd_lvl: u8,
    pub subtask_name: String,
    pub task_id: String,
    pub hms: Vec<String>,
    pub device_name: String,
    /// Global slot ID of the tray currently loaded in the nozzle.
    /// 255 = nothing loaded, 254 = external spool, 0-15 = AMS slots.
    pub tray_now: u8,
    pub camera: Camera,
    /// Per extruder, what it is fed from: `ams_index << 8 | tray_index`.
    #[serde(skip)]
    extruder_feeds: Vec<u16>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct Temperature {
    pub current: f64,
    pub target: f64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Left,
    Right,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Nozzle {
    pub side: Side,
    pub current: f64,
    pub target: f64,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AmsUnit {
    pub id: u8,
    pub humidity: u8,
    pub trays: Vec<AmsTray>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AmsTray {
    pub id: u8,
    pub tray_type: String,
    pub color: String,
    pub name: String,
    /// Default print temperature for this filament (°C). Sent in filament-change commands.
    pub tray_temp: u16,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ExternalSpool {
    /// `vt_tray` spools: `EXTERNAL_SPOOL_SLOT`; `vir_slot` spools: the holder's ID.
    pub slot: u8,
    /// Set on dual-nozzle printers, which have a holder per side.
    pub side: Option<Side>,
    /// Whether `load_filament` supports this spool.
    pub loadable: bool,
    /// Feeding a nozzle.
    pub loaded: bool,
    pub tray: AmsTray,
    #[serde(skip)]
    source: SpoolSource,
}

/// Where the printer reports the spool, which also decides how it reports it as loaded.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SpoolSource {
    /// `print.vt_tray`, loaded when `tray_now` is `EXTERNAL_SPOOL_SLOT`.
    VtTray,
    /// `print.vir_slot`, loaded when an extruder is fed from it.
    VirSlot,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Camera {
    /// No status report yet.
    #[default]
    Unknown,
    /// JPEG frames over TLS on port 6000.
    Jpeg,
    /// RTSPS stream, played by the Android app's live view.
    Rtsp { url: String },
}

/// `load_filament`'s slot ID for the A1/P1-style external spool.
pub const EXTERNAL_SPOOL_SLOT: u8 = 254;

/// The external spool holders of a dual-nozzle printer, by `vir_slot` ID.
const LEFT_HOLDER: u8 = 254;
const RIGHT_HOLDER: u8 = 255;

const DEFAULT_TRAY_TEMP: u16 = 220;

impl PrinterStatus {
    /// Applies one MQTT report. Fails only when the payload is not a JSON object.
    pub fn apply(&mut self, payload: &[u8]) -> Result<(), serde_json::Error> {
        let report: Report = serde_json::from_slice(payload)?;
        if let Some(print) = report.print {
            self.apply_print(print);
        }
        Ok(())
    }

    fn apply_print(&mut self, p: PrintReport) {
        set(&mut self.nozzle_temp, p.nozzle_temper);
        set(&mut self.nozzle_target, p.nozzle_target_temper);
        set(&mut self.bed_temp, p.bed_temper);
        set(&mut self.bed_target, p.bed_target_temper);
        set(&mut self.progress, p.mc_percent);
        set(&mut self.remaining_mins, p.mc_remaining_time);
        set(&mut self.layer_num, p.layer_num);
        set(&mut self.total_layer_num, p.total_layer_num);
        set(&mut self.spd_lvl, p.spd_lvl);
        set(&mut self.gcode_state, p.gcode_state);
        set(&mut self.subtask_name, p.subtask_name);
        set(&mut self.task_id, p.task_id);
        if let Some(stage) = p.stg_cur {
            self.stage = stage_name(stage).to_owned();
        }

        if let Some(light) = p
            .lights_report
            .unwrap_or_default()
            .into_iter()
            .find(|l| l.node.as_deref() == Some("chamber_light"))
        {
            self.chamber_light = light.mode.as_deref() == Some("on");
        }

        if let Some(device) = p.device {
            self.apply_device(device, p.bed_temper.is_none());
        }

        if let Some(ams) = p.ams {
            set(&mut self.tray_now, ams.tray_now);
            if let Some(units) = ams.ams {
                self.ams = units
                    .into_iter()
                    .filter_map(|u| {
                        Some(AmsUnit {
                            id: u.id?,
                            humidity: u.humidity.unwrap_or(0),
                            trays: u.tray.unwrap_or_default().into_iter().map(tray).collect(),
                        })
                    })
                    .collect();
            }
        }

        if let Some(slots) = p.vir_slot {
            let dual = self.nozzles.len() == 2;
            self.external_spools = slots
                .into_iter()
                .filter(is_loaded)
                .map(|t| {
                    let slot = t.id.unwrap_or(0);
                    let side = match slot {
                        LEFT_HOLDER if dual => Some(Side::Left),
                        RIGHT_HOLDER if dual => Some(Side::Right),
                        _ => None,
                    };
                    ExternalSpool {
                        slot,
                        side,
                        loadable: false,
                        loaded: false,
                        tray: tray(t),
                        source: SpoolSource::VirSlot,
                    }
                })
                .collect();
        } else if let Some(vt) = p.vt_tray {
            self.external_spools = Some(vt)
                .filter(is_loaded)
                .map(|t| ExternalSpool {
                    slot: EXTERNAL_SPOOL_SLOT,
                    side: None,
                    loadable: true,
                    loaded: false,
                    tray: tray(t),
                    source: SpoolSource::VtTray,
                })
                .into_iter()
                .collect();
        }

        for spool in &mut self.external_spools {
            spool.loaded = match spool.source {
                SpoolSource::VtTray => self.tray_now == EXTERNAL_SPOOL_SLOT,
                SpoolSource::VirSlot => self.extruder_feeds.contains(&(u16::from(spool.slot) << 8)),
            };
        }

        // Bambu sends incremental updates, so most messages omit `hms`; only a present
        // list replaces the errors shown.
        if let Some(hms) = p.hms {
            self.hms = hms
                .into_iter()
                .filter_map(|h| {
                    let s = format!("{:08X}{:08X}", h.attr?, h.code?);
                    Some(format!(
                        "{}-{}-{}-{}",
                        &s[0..4],
                        &s[4..8],
                        &s[8..12],
                        &s[12..16]
                    ))
                })
                .collect();
        }

        // A1/P1 report no RTSPS URL; RTSPS printers report "disable" while LAN liveview is off.
        let full_report = p.command.as_deref() == Some("push_status");
        match p.ipcam.and_then(|c| c.rtsp_url) {
            Some(url) if url.starts_with("rtsp") => self.camera = Camera::Rtsp { url },
            Some(_) => self.camera = Camera::Jpeg,
            None if full_report && self.camera == Camera::Unknown => self.camera = Camera::Jpeg,
            None => {}
        }
    }

    fn apply_device(&mut self, d: DeviceReport, bed_from_device: bool) {
        if let Some(extruder) = d.extruder {
            // Bits 4-7 of `state` name the active extruder; 15 while switching nozzles.
            let active = extruder.state.map(|s| (s >> 4) & 0xF);
            let info = extruder.info.unwrap_or_default();
            self.extruder_feeds = info.iter().filter_map(|e| e.snow).collect();
            if info.len() == 2 {
                let mut nozzles: Vec<Nozzle> = info
                    .iter()
                    .filter_map(|e| {
                        let side = match e.id? {
                            0 => Side::Right,
                            1 => Side::Left,
                            _ => return None,
                        };
                        let t = unpack(e.temp?);
                        Some(Nozzle {
                            side,
                            current: t.current,
                            target: t.target,
                            active: active == Some(u32::from(e.id?)),
                        })
                    })
                    .collect();
                nozzles.sort_by_key(|n| n.side != Side::Left);
                self.nozzles = nozzles;
            }
        }

        // A1/P1 report a constant `chamber_temper` without having a sensor, so only `ctc` counts.
        if let Some(temp) = d.ctc.and_then(|h| h.info).and_then(|i| i.temp) {
            self.chamber = Some(unpack(temp));
        }

        if bed_from_device {
            if let Some(temp) = d.bed.and_then(|h| h.info).and_then(|i| i.temp) {
                let t = unpack(temp);
                self.bed_temp = t.current;
                self.bed_target = t.target;
            }
        }
    }

    /// Print temperature of the filament in a global slot.
    pub fn tray_temperature(&self, slot: u8) -> Option<u16> {
        if let Some(spool) = self.external_spools.iter().find(|s| s.slot == slot) {
            return Some(spool.tray.tray_temp);
        }
        self.ams
            .iter()
            .find(|u| u.id == slot / 4)
            .and_then(|u| u.trays.iter().find(|t| t.id == slot % 4))
            .map(|t| t.tray_temp)
    }
}

fn set<T>(dst: &mut T, value: Option<T>) {
    if let Some(v) = value {
        *dst = v;
    }
}

/// Newer printers pack a heater's state into one number: current in the low 16 bits,
/// target in the high 16.
fn unpack(temp: u32) -> Temperature {
    Temperature {
        current: f64::from(temp & 0xFFFF),
        target: f64::from(temp >> 16),
    }
}

fn is_loaded(t: &TrayReport) -> bool {
    t.tray_type.as_deref().is_some_and(|s| !s.is_empty())
}

fn tray(t: TrayReport) -> AmsTray {
    // The colour is RRGGBBAA; the app uses RRGGBB.
    let mut color = t.tray_color.unwrap_or_else(|| "3F3F46".to_owned());
    color.truncate(6);
    AmsTray {
        id: t.id.unwrap_or(0),
        tray_type: t.tray_type.unwrap_or_default(),
        color,
        name: t.tray_sub_brands.unwrap_or_default(),
        tray_temp: t.tray_temp.unwrap_or(DEFAULT_TRAY_TEMP),
    }
}

fn stage_name(stg: u64) -> &'static str {
    match stg {
        0 => "Idle",
        1 => "Auto bed leveling",
        2 => "Heatbed preheating",
        4 => "Changing filament",
        7 => "Heating hotend",
        9 => "Scanning bed surface",
        10 => "Inspecting first layer",
        13 => "Homing toolhead",
        14 => "Cleaning nozzle",
        17 => "Printing",
        20 => "Paused by user",
        22 => "Filament unloading",
        24 => "Filament loading",
        _ => "Working",
    }
}

// ── Report as sent by the printer ─────────────────────────────────────────────

/// A number, also when sent as a string.
type Num = DefaultOnError<Option<PickFirst<(Same, DisplayFromStr)>>>;
type Any = DefaultOnError<Option<Same>>;

#[derive(Deserialize)]
struct Report {
    print: Option<PrintReport>,
}

#[serde_as]
#[derive(Deserialize, Default)]
#[serde(default)]
struct PrintReport {
    #[serde_as(as = "Any")]
    command: Option<String>,
    #[serde_as(as = "Num")]
    nozzle_temper: Option<f64>,
    #[serde_as(as = "Num")]
    nozzle_target_temper: Option<f64>,
    #[serde_as(as = "Num")]
    bed_temper: Option<f64>,
    #[serde_as(as = "Num")]
    bed_target_temper: Option<f64>,
    #[serde_as(as = "Num")]
    mc_percent: Option<u8>,
    #[serde_as(as = "Num")]
    mc_remaining_time: Option<u32>,
    #[serde_as(as = "Num")]
    layer_num: Option<u32>,
    #[serde_as(as = "Num")]
    total_layer_num: Option<u32>,
    #[serde_as(as = "Num")]
    spd_lvl: Option<u8>,
    #[serde_as(as = "Num")]
    stg_cur: Option<u64>,
    #[serde_as(as = "Any")]
    gcode_state: Option<String>,
    #[serde_as(as = "Any")]
    subtask_name: Option<String>,
    #[serde_as(as = "Any")]
    task_id: Option<String>,
    #[serde_as(as = "Any")]
    lights_report: Option<Vec<LightReport>>,
    #[serde_as(as = "Any")]
    ams: Option<AmsReport>,
    #[serde_as(as = "Any")]
    vt_tray: Option<TrayReport>,
    #[serde_as(as = "Any")]
    vir_slot: Option<Vec<TrayReport>>,
    #[serde_as(as = "Any")]
    hms: Option<Vec<HmsReport>>,
    #[serde_as(as = "Any")]
    ipcam: Option<IpcamReport>,
    #[serde_as(as = "Any")]
    device: Option<DeviceReport>,
}

#[serde_as]
#[derive(Deserialize, Default)]
#[serde(default)]
struct LightReport {
    #[serde_as(as = "Any")]
    node: Option<String>,
    #[serde_as(as = "Any")]
    mode: Option<String>,
}

#[serde_as]
#[derive(Deserialize, Default)]
#[serde(default)]
struct AmsReport {
    #[serde_as(as = "Num")]
    tray_now: Option<u8>,
    #[serde_as(as = "Any")]
    ams: Option<Vec<AmsUnitReport>>,
}

#[serde_as]
#[derive(Deserialize, Default)]
#[serde(default)]
struct AmsUnitReport {
    #[serde_as(as = "Num")]
    id: Option<u8>,
    #[serde_as(as = "Num")]
    humidity: Option<u8>,
    #[serde_as(as = "Any")]
    tray: Option<Vec<TrayReport>>,
}

#[serde_as]
#[derive(Deserialize, Default)]
#[serde(default)]
struct TrayReport {
    #[serde_as(as = "Num")]
    id: Option<u8>,
    #[serde_as(as = "Any")]
    tray_type: Option<String>,
    #[serde_as(as = "Any")]
    tray_color: Option<String>,
    #[serde_as(as = "Any")]
    tray_sub_brands: Option<String>,
    #[serde_as(as = "Num")]
    tray_temp: Option<u16>,
}

#[serde_as]
#[derive(Deserialize, Default)]
#[serde(default)]
struct HmsReport {
    #[serde_as(as = "Num")]
    attr: Option<u64>,
    #[serde_as(as = "Num")]
    code: Option<u64>,
}

#[serde_as]
#[derive(Deserialize, Default)]
#[serde(default)]
struct IpcamReport {
    #[serde_as(as = "Any")]
    rtsp_url: Option<String>,
}

#[serde_as]
#[derive(Deserialize, Default)]
#[serde(default)]
struct DeviceReport {
    #[serde_as(as = "Any")]
    extruder: Option<ExtruderReport>,
    #[serde_as(as = "Any")]
    bed: Option<HeaterReport>,
    #[serde_as(as = "Any")]
    ctc: Option<HeaterReport>,
}

#[serde_as]
#[derive(Deserialize, Default)]
#[serde(default)]
struct ExtruderReport {
    #[serde_as(as = "Num")]
    state: Option<u32>,
    #[serde_as(as = "Any")]
    info: Option<Vec<ExtruderInfo>>,
}

#[serde_as]
#[derive(Deserialize, Default)]
#[serde(default)]
struct ExtruderInfo {
    #[serde_as(as = "Num")]
    id: Option<u8>,
    #[serde_as(as = "Num")]
    temp: Option<u32>,
    #[serde_as(as = "Num")]
    snow: Option<u16>,
}

#[serde_as]
#[derive(Deserialize, Default)]
#[serde(default)]
struct HeaterReport {
    #[serde_as(as = "Any")]
    info: Option<HeaterInfo>,
}

#[serde_as]
#[derive(Deserialize, Default)]
#[serde(default)]
struct HeaterInfo {
    #[serde_as(as = "Num")]
    temp: Option<u32>,
}
