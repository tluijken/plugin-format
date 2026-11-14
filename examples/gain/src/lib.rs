use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use plugin_format::{Block, GuiContext, Info, Kind, Map, ParamInfo, Plugin, Shared, egui, export_clap, export_vst3};

pub const GAIN: u32 = 0;
pub const FREQUENCY: u32 = 1;
pub const TEST_GESTURE: u32 = 2;

pub const INFOS: [ParamInfo; 3] = [
    ParamInfo { id: GAIN, name: "Gain", kind: Kind::Int { min: -60, max: 24 }, default: 0.0, automatable: true },
    ParamInfo { id: FREQUENCY, name: "Frequency", kind: Kind::Float { min: 20.0, max: 20000.0, map: Map::Log }, default: 1000.0, automatable: true },
    ParamInfo { id: TEST_GESTURE, name: "Test gesture", kind: Kind::Bool, default: 0.0, automatable: false },
];

pub struct GainShared {
    db: AtomicI32,
    frequency: AtomicU64,
    cx: Mutex<Option<GuiContext>>,
}

impl GainShared {
    /// What an editor does when a drag sets the gain to -6 dB, for the host tests to see without a window.
    fn gesture(&self) {
        let Some(cx) = self.cx.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return };
        cx.begin(GAIN);
        cx.set(GAIN, -6.0);
        cx.end(GAIN);
        cx.mark_dirty();
    }
}

impl Shared for GainShared {
    fn params(&self) -> &'static [ParamInfo] {
        &INFOS
    }

    fn param(&self, id: u32) -> f64 {
        match id {
            GAIN => self.db.load(Ordering::Relaxed) as f64,
            FREQUENCY => f64::from_bits(self.frequency.load(Ordering::Relaxed)),
            _ => 0.0,
        }
    }

    fn set_param(&self, id: u32, plain: f64) {
        match id {
            GAIN => self.db.store(INFOS[0].clamp(plain) as i32, Ordering::Relaxed),
            FREQUENCY => self.frequency.store(INFOS[1].clamp(plain).to_bits(), Ordering::Relaxed),
            TEST_GESTURE if plain >= 0.5 => self.gesture(),
            _ => {}
        }
    }

    fn param_text(&self, id: u32, plain: f64) -> String {
        if id == FREQUENCY { format!("{plain:.0} Hz") } else { format!("{plain:.0} dB") }
    }

    fn param_from_text(&self, id: u32, text: &str) -> Option<f64> {
        let unit = if id == FREQUENCY { "Hz" } else { "dB" };
        let value: f64 = text.trim().trim_end_matches(unit).trim().parse().ok()?;
        value.is_finite().then_some(value)
    }

    fn save(&self) -> Vec<u8> {
        format!("gain={};frequency={}", self.db.load(Ordering::Relaxed), self.param(FREQUENCY)).into_bytes()
    }

    fn load(&self, bytes: &[u8]) -> bool {
        let Some(text) = std::str::from_utf8(bytes).ok() else { return false };
        let Some((db, frequency)) = text.strip_prefix("gain=").and_then(|rest| rest.split_once(";frequency=")) else { return false };
        let (Ok(db), Ok(frequency)) = (db.parse::<i32>(), frequency.parse::<f64>()) else { return false };
        if !(-60..=24).contains(&db) || !(20.0..=20000.0).contains(&frequency) {
            return false;
        }
        self.db.store(db, Ordering::Relaxed);
        self.frequency.store(frequency.to_bits(), Ordering::Relaxed);
        true
    }

    fn editor(&self, _: GuiContext) -> Box<dyn FnMut(&mut egui::Ui) + Send> {
        Box::new(|root| {
            egui::CentralPanel::default().show(root, |ui| {
                ui.heading("Gain");
            });
        })
    }

    fn attach(&self, cx: GuiContext) {
        *self.cx.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx);
    }
}

pub struct Gain {
    shared: Arc<GainShared>,
    inputs: usize,
}

impl Plugin for Gain {
    const INFO: Info = Info {
        name: "Gain",
        vendor: "plugin-format",
        version: env!("CARGO_PKG_VERSION"),
        clap_id: "plugin-format.gain",
        description: "A gain in dB, the example plugin of plugin-format",
        clap_features: &[c"audio-effect", c"stereo", c"mono", c"utility"],
        vst3_class_id: *b"plugfmtGainExamp",
        vst3_subcategories: "Fx",
        sidechain: false,
    };
    const EDITOR_SIZE: (u32, u32) = (200, 100);

    type Shared = GainShared;

    fn new() -> (Self, Arc<GainShared>) {
        let shared = Arc::new(GainShared { db: AtomicI32::new(0), frequency: AtomicU64::new(1000f64.to_bits()), cx: Mutex::new(None) });
        (Gain { shared: shared.clone(), inputs: 1 }, shared)
    }

    fn activate(&mut self, _: f64, _: usize, inputs: usize) -> u32 {
        self.inputs = inputs;
        0
    }

    fn deactivate(&mut self) {}

    fn latency(&self) -> u32 {
        0
    }

    fn process(&mut self, block: Block<'_>) {
        let channels = block.channels;
        let factor = 10f32.powf(self.shared.db.load(Ordering::Relaxed) as f32 / 20.0);
        for channel in channels.iter_mut() {
            for sample in channel.iter_mut() {
                *sample *= factor;
            }
        }
        // The wrapper fills only output channel 0 from a mono input.
        if self.inputs == 1 && channels.len() == 2 {
            let (left, right) = channels.split_at_mut(1);
            right[0].copy_from_slice(left[0]);
        }
    }

    fn take_latency_changed(&mut self) -> bool {
        false
    }
}

export_clap!(Gain);
export_vst3!(Gain);
