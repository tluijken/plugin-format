# plugin-format

CLAP and VST3 export for a plugin written once against a small trait, with an egui editor. Implement
`Plugin` (the audio half) and `Shared` (the half any thread may use: parameters, state, editor), then
export it:

```rust
plugin_format::export_clap!(MyPlugin);
plugin_format::export_vst3!(MyPlugin);
```

`examples/gain` is a complete plugin: one integer gain parameter in dB, mono or stereo in, stereo out. Its
library is what the host tests in `tests/clap_host.rs` and `tests/vst3_host.rs` load, the way a host does.

## Development

```sh
nix develop -c cargo test --workspace
nix develop .#windows -c cargo test --target x86_64-pc-windows-gnu   # MinGW cross build, tests under Wine
```

`scripts/bundle.sh <crate> <name> <bundle identifier> <output dir> [<version>]` builds a plugin in release and lays
it out as hosts look for it: `<name>.clap` and `<name>.vst3` for this system (on macOS universal,
signed ad hoc bundles), or for the Linux or Windows target in `CARGO_BUILD_TARGET`:

```sh
nix develop -c scripts/bundle.sh gain Gain com.example.gain bundles
nix develop .#windows -c scripts/bundle.sh gain Gain com.example.gain bundles
```

`scripts/validate.sh <bundle dir>` runs clap-validator and Steinberg's VST3 validator on every `.clap`
and `.vst3` in a directory. It builds both at the versions in `scripts/validators.env` on first use and
keeps them in `~/.cache/plugin-format/validators` (or `VALIDATORS_DIR`); CI runs it on the gain example.

```sh
nix develop -c scripts/validate.sh bundles
```

## Licence

MIT OR Apache-2.0, at your option. See `LICENSE-MIT` and `LICENSE-APACHE`.
