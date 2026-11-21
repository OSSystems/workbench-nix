{ pkgs, inputs, ... }:
pkgs.mkShell {
  packages = with pkgs; [
    cargo
    clippy
    rustc
    rustfmt
    rust-analyzer
    # The tests sign SSH keys as the host does.
    openssh
    # The web UI's WebAssembly.
    lld
    wasm-bindgen-cli_0_2_126
  ];
  env.RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
  # The crate builds against the vmkit the flake locks, as the package does:
  # ./vmkit links to it. A link to a checkout of your own is left alone.
  shellHook = ''
    case "$(readlink vmkit 2>/dev/null || echo none)" in
      none | /nix/store/*) [ -d vmkit ] && [ ! -L vmkit ] || ln -sfn ${inputs.vmkit} vmkit ;;
    esac
  '';
}
