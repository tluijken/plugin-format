use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use plugin_format::{Block, Info, Kind, Map, ParamInfo, Plugin, Shared, egui, export_clap, export_vst3};

pub const POSITION: u32 = 0;

pub const INFOS: [ParamInfo; 1] = [ParamInfo {
    id: POSITION,
    name: "Position",
    kind: Kind::Float { min: -1.0, max: 1e15, map: Map::Linear },
    default: -1.0,
    automatable: false,
}];

/// The last block's position as an f64, or -1 when the host gave none.
pub struct SidechainShared {
    position: AtomicU64,
}

impl Shared for SidechainShared {
    fn params(&self) -> &'static [ParamInfo] {
        &INFOS
    }

    fn param(&self, id: u32) -> f64 {
        if id == POSITION { f64::from_bits(self.position.load(Ordering::Relaxed)) } else { 0.0 }
    }

    fn set_param(&self, _: u32, _: f64) {}

    fn param_text(&self, _: u32, plain: f64) -> String {
        format!("{plain:.0}")
    }

    fn param_from_text(&self, _: u32, text: &str) -> Option<f64> {
        text.trim().parse().ok()
    }

    fn save(&self) -> Vec<u8> {
        Vec::new()
    }

    fn load(&self, bytes: &[u8]) -> bool {
        bytes.is_empty()
    }

    fn editor(&self) -> Box<dyn FnMut(&mut egui::Ui) + Send> {
        Box::new(|root| {
            egui::CentralPanel::default().show(root, |ui| {
                ui.heading("Sidechain");
            });
        })
    }
}

pub struct Sidechain {
    shared: Arc<SidechainShared>,
}

impl Plugin for Sidechain {
    const INFO: Info = Info {
        name: "Sidechain",
        vendor: "plugin-format",
        version: env!("CARGO_PKG_VERSION"),
        clap_id: "plugin-format.sidechain",
        description: "Adds its sidechain to its input and reports the transport position, the example plugin for both",
        clap_features: &[c"audio-effect", c"stereo", c"utility"],
        vst3_class_id: *b"plugfmtSidechain",
        vst3_subcategories: "Fx",
        sidechain: true,
    };
    const EDITOR_SIZE: (u32, u32) = (200, 100);

    type Shared = SidechainShared;

    fn new() -> (Self, Arc<SidechainShared>) {
        let shared = Arc::new(SidechainShared { position: AtomicU64::new((-1f64).to_bits()) });
        (Sidechain { shared: shared.clone() }, shared)
    }

    fn activate(&mut self, _: f64, _: usize, _: usize) -> u32 {
        0
    }

    fn deactivate(&mut self) {}

    fn latency(&self) -> u32 {
        0
    }

    fn process(&mut self, block: Block<'_>) {
        if let Some(sidechain) = block.sidechain {
            for (channel, side) in block.channels.iter_mut().zip(sidechain) {
                for (sample, &add) in channel.iter_mut().zip(side) {
                    *sample += add;
                }
            }
        }
        let position = block.position.map_or(-1.0, |position| position as f64);
        self.shared.position.store(position.to_bits(), Ordering::Relaxed);
    }

    fn take_latency_changed(&mut self) -> bool {
        false
    }
}

export_clap!(Sidechain);
export_vst3!(Sidechain);
