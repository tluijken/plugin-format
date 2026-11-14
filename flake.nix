{
  description = "plugin-format: CLAP and VST3 export for plugins written in Rust";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      win = pkgs.pkgsCross.mingwW64;
      runtimeLibs = with pkgs; [ alsa-lib libGL libx11 libxcursor libxrandr libxi libxcb libxkbcommon ];
    in {
      devShells.${system} = {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc clippy rustfmt rust-analyzer pkg-config alsa-lib libx11 libxcb libGL ];
          shellHook = ''
            export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath runtimeLibs}
          '';
        };
        # Builds the plugin as a Windows DLL (`.clap`, `.vst3`) and runs the tests under Wine with MinGW.
        windows = win.mkShell {
          nativeBuildInputs = [ win.buildPackages.cargo win.buildPackages.rustc pkgs.wineWow64Packages.stable ];
          buildInputs = [ win.windows.pthreads ];
          CARGO_BUILD_TARGET = "x86_64-pc-windows-gnu";
          CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "${win.stdenv.cc.targetPrefix}cc";
          CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER = "wine";
          shellHook = ''
            if [ -z "''${WINEPREFIX:-}" ]; then
              root=$(git rev-parse --show-toplevel 2>/dev/null || echo "$PWD")
              export WINEPREFIX="$root/local/wineprefix"
            fi
            mkdir -p "$WINEPREFIX"
            export WINEDLLOVERRIDES="mscoree=;mshtml="
          '';
        };
      };
    };
}
