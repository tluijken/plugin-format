#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Int { min: i32, max: i32 },
    Bool,
}

#[derive(Clone, Copy, Debug)]
pub struct ParamInfo {
    pub id: u32,
    pub name: &'static str,
    pub kind: Kind,
    pub default: f64,
    pub automatable: bool,
}

impl ParamInfo {
    pub fn min(&self) -> f64 {
        match self.kind {
            Kind::Int { min, .. } => min as f64,
            Kind::Bool => 0.0,
        }
    }

    pub fn max(&self) -> f64 {
        match self.kind {
            Kind::Int { max, .. } => max as f64,
            Kind::Bool => 1.0,
        }
    }

    pub fn step_count(&self) -> i32 {
        match self.kind {
            Kind::Int { min, max } => max - min,
            Kind::Bool => 1,
        }
    }

    pub fn clamp(&self, plain: f64) -> f64 {
        plain.round().clamp(self.min(), self.max())
    }

    pub fn normalise(&self, plain: f64) -> f64 {
        let span = self.max() - self.min();
        if span <= 0.0 {
            return 0.0;
        }
        (self.clamp(plain) - self.min()) / span
    }

    pub fn plain(&self, normalised: f64) -> f64 {
        let normalised = normalised.clamp(0.0, 1.0);
        self.clamp(self.min() + normalised * (self.max() - self.min()))
    }
}
