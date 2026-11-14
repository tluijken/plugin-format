use nx_format::{Kind, ParamInfo};

#[test]
fn every_int_value_survives_normalising_and_back() {
    let p = ParamInfo { id: 0, name: "Pedal preset", kind: Kind::Int { min: 0, max: 126 }, default: 0.0, automatable: false };
    for v in 0..=126 { assert_eq!(p.plain(p.normalise(v as f64)), v as f64); }
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
