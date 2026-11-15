#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Map {
    Linear,
    /// Equal normalised steps are equal ratios. Needs `0 < min < max`; a range without that maps linearly
    /// instead, since a logarithm of it is not a number.
    Log,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Int {
        min: i32,
        max: i32,
    },
    /// An integer the host should offer as a list of its texts rather than as a number.
    List {
        min: i32,
        max: i32,
    },
    Bool,
    Float {
        min: f64,
        max: f64,
        map: Map,
    },
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
            Kind::Int { min, .. } | Kind::List { min, .. } => min as f64,
            Kind::Bool => 0.0,
            Kind::Float { min, .. } => min,
        }
    }

    pub fn max(&self) -> f64 {
        match self.kind {
            Kind::Int { max, .. } | Kind::List { max, .. } => max as f64,
            Kind::Bool => 1.0,
            Kind::Float { max, .. } => max,
        }
    }

    pub fn step_count(&self) -> i32 {
        match self.kind {
            Kind::Int { min, max } | Kind::List { min, max } => max - min,
            Kind::Bool => 1,
            Kind::Float { .. } => 0,
        }
    }

    /// Non-finite input becomes the default, so one bad host value cannot leave state that will not load.
    pub fn clamp(&self, plain: f64) -> f64 {
        let plain = if plain.is_finite() { plain } else { self.default };
        match self.kind {
            Kind::Float { .. } => plain.clamp(self.min(), self.max()),
            _ => plain.round().clamp(self.min(), self.max()),
        }
    }

    fn log_range(&self) -> Option<(f64, f64)> {
        match self.kind {
            Kind::Float { min, max, map: Map::Log } if min > 0.0 && max > min => Some((min, max)),
            _ => None,
        }
    }

    pub fn normalise(&self, plain: f64) -> f64 {
        let span = self.max() - self.min();
        if span <= 0.0 {
            return 0.0;
        }
        let plain = self.clamp(plain);
        match self.log_range() {
            Some((min, max)) => (plain / min).ln() / (max / min).ln(),
            None => (plain - self.min()) / span,
        }
    }

    pub fn plain(&self, normalised: f64) -> f64 {
        let normalised = normalised.clamp(0.0, 1.0);
        match self.log_range() {
            Some((min, max)) => self.clamp(min * (max / min).powf(normalised)),
            None => self.clamp(self.min() + normalised * (self.max() - self.min())),
        }
    }
}
