{ pkgs, inputs, ... }:
pkgs.mkShell {
  packages = with pkgs; [
    cargo
    clippy
    rustc
    rustfmt
    rust-analyzer
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
