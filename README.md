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

## Licence

MIT OR Apache-2.0, at your option. See `LICENSE-MIT` and `LICENSE-APACHE`.
