use plugin_format::{Kind, Map, ParamInfo};

#[test]
fn every_int_value_survives_normalising_and_back() {
    let p = ParamInfo { id: 0, name: "Pedal preset", kind: Kind::Int { min: 0, max: 126 }, default: 0.0, automatable: false };
    for v in 0..=126 {
        assert_eq!(p.plain(p.normalise(v as f64)), v as f64);
    }
    assert_eq!(p.step_count(), 126);
}

#[test]
fn bools_normalise_to_the_ends() {
    let p = ParamInfo { id: 2, name: "Block 1 on", kind: Kind::Bool, default: 1.0, automatable: true };
    assert_eq!((p.normalise(0.0), p.normalise(1.0), p.step_count()), (0.0, 1.0, 1));
    assert_eq!(p.plain(0.49), 0.0);
    assert_eq!(p.plain(0.51), 1.0);
}

#[test]
fn out_of_range_values_are_clamped_and_rounded() {
    let p = ParamInfo { id: 1, name: "Snapshot", kind: Kind::Int { min: 1, max: 3 }, default: 1.0, automatable: true };
    assert_eq!((p.clamp(-4.0), p.clamp(9.0), p.clamp(2.4)), (1.0, 3.0, 2.0));
    assert_eq!((p.plain(-0.5), p.plain(1.5)), (1.0, 3.0));
}

#[test]
fn non_finite_values_fall_back_to_the_default() {
    let p = ParamInfo { id: 1, name: "Snapshot", kind: Kind::Int { min: 1, max: 3 }, default: 2.0, automatable: true };
    assert_eq!((p.clamp(f64::NAN), p.clamp(f64::INFINITY), p.clamp(f64::NEG_INFINITY)), (2.0, 2.0, 2.0));
    assert_eq!(p.plain(f64::NAN), 2.0);
}

#[test]
fn float_linear_round_trips() {
    let p = ParamInfo { id: 3, name: "Gain", kind: Kind::Float { min: -30.0, max: 30.0, map: Map::Linear }, default: 0.0, automatable: true };
    assert_eq!(p.plain(p.normalise(12.5)), 12.5);
    assert_eq!(p.normalise(0.0), 0.5);
    assert_eq!(p.step_count(), 0);
}

#[test]
fn float_log_round_trips() {
    let p = ParamInfo { id: 4, name: "Frequency", kind: Kind::Float { min: 20.0, max: 20000.0, map: Map::Log }, default: 1000.0, automatable: true };
    assert!((p.normalise(632.455_532_033_675_9) - 0.5).abs() < 1e-9);
    assert!((p.plain(p.normalise(1000.0)) - 1000.0).abs() < 1e-9);
    assert_eq!((p.normalise(20.0), p.normalise(20000.0)), (0.0, 1.0));
    assert_eq!(p.step_count(), 0);
}

#[test]
fn float_clamp_keeps_fractions_and_replaces_nan() {
    let p = ParamInfo { id: 3, name: "Gain", kind: Kind::Float { min: -30.0, max: 30.0, map: Map::Linear }, default: 2.5, automatable: true };
    assert_eq!(p.clamp(1.25), 1.25);
    assert_eq!(p.clamp(f64::NAN), 2.5);
    assert_eq!((p.clamp(99.0), p.clamp(-99.0)), (30.0, -30.0));
}

#[test]
fn a_log_range_that_does_not_start_above_zero_maps_linearly() {
    for (min, max) in [(0.0, 10.0), (-10.0, 10.0)] {
        let p = ParamInfo { id: 5, name: "Odd", kind: Kind::Float { min, max, map: Map::Log }, default: min, automatable: true };
        for v in [min, min + 0.25 * (max - min), min + 0.5 * (max - min), max] {
            let n = p.normalise(v);
            assert!(n.is_finite() && (0.0..=1.0).contains(&n), "{min}..{max}: {v} -> {n}");
            assert!((p.plain(n) - v).abs() < 1e-9, "{min}..{max}: {v}");
        }
        assert_eq!(p.normalise(min + 0.5 * (max - min)), 0.5);
        assert_eq!(p.plain(0.5), min + 0.5 * (max - min));
    }
}
