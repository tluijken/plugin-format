use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use plugin_format::{Info, Kind, ParamInfo, Plugin, Shared, egui, export_clap, export_vst3};

pub const GAIN: u32 = 0;

pub const INFOS: [ParamInfo; 1] = [ParamInfo { id: GAIN, name: "Gain", kind: Kind::Int { min: -60, max: 24 }, default: 0.0, automatable: true }];

pub struct GainShared {
    db: AtomicI32,
}

impl Shared for GainShared {
    fn params(&self) -> &'static [ParamInfo] {
        &INFOS
    }

    fn param(&self, id: u32) -> f64 {
        if id == GAIN { self.db.load(Ordering::Relaxed) as f64 } else { 0.0 }
    }

    fn set_param(&self, id: u32, plain: f64) {
        if id == GAIN {
            self.db.store(INFOS[0].clamp(plain) as i32, Ordering::Relaxed);
        }
    }

    fn param_text(&self, _: u32, plain: f64) -> String {
        format!("{plain:.0} dB")
    }

    fn param_from_text(&self, _: u32, text: &str) -> Option<f64> {
        let value: f64 = text.trim().trim_end_matches("dB").trim().parse().ok()?;
        value.is_finite().then_some(value)
    }

    fn save(&self) -> Vec<u8> {
        format!("gain={}", self.db.load(Ordering::Relaxed)).into_bytes()
    }

    fn load(&self, bytes: &[u8]) -> bool {
        let Some(db) = std::str::from_utf8(bytes).ok().and_then(|text| text.strip_prefix("gain=")).and_then(|db| db.parse::<i32>().ok()) else { return false };
        if !(-60..=24).contains(&db) {
            return false;
        }
        self.db.store(db, Ordering::Relaxed);
        true
    }

    fn editor(&self) -> Box<dyn FnMut(&mut egui::Ui) + Send> {
        Box::new(|root| {
            egui::CentralPanel::default().show(root, |ui| {
                ui.heading("Gain");
            });
        })
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
    };
    const EDITOR_SIZE: (u32, u32) = (200, 100);

    type Shared = GainShared;

    fn new() -> (Self, Arc<GainShared>) {
        let shared = Arc::new(GainShared { db: AtomicI32::new(0) });
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

    fn process(&mut self, channels: &mut [&mut [f32]]) {
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
