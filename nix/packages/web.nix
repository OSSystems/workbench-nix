# The web UI's server, workbench-web, and its site: the WebAssembly the
# browser hydrates the pages with, the stylesheet and xterm.js.
{
  pkgs,
  inputs ? null,
  vmkit ? inputs.vmkit,
  ...
}:

let
  inherit (pkgs) lib;
  src = lib.fileset.toSource {
    root = ../..;
    fileset = lib.fileset.unions [
      ../../Cargo.toml
      ../../Cargo.lock
      ../../src
      ../../model
      ../../web
    ];
  };
  common = {
    version = "0.2.0";
    inherit src;
    # The same vmkit as the workbench package: see package.nix.
    postPatch = ''
      cp -r --no-preserve=mode ${vmkit} vmkit
    '';
    cargoLock.lockFile = ../../Cargo.lock;
    doCheck = false;
    meta.license = lib.licenses.mit;
  };

  xterm = pkgs.fetchurl {
    url = "https://registry.npmjs.org/@xterm/xterm/-/xterm-5.5.0.tgz";
    hash = "sha256-vZVPpyGHIXAYjMXX6D6I2zyDyaGKTo0kwng9JkkfWdI=";
  };

  site = pkgs.rustPlatform.buildRustPackage (
    common
    // {
      pname = "workbench-web-site";
      nativeBuildInputs = [
        pkgs.lld
        pkgs.wasm-bindgen-cli_0_2_126
        pkgs.binaryen
      ];
      buildPhase = ''
        runHook preBuild
        cargo build --offline --frozen --profile wasm-release -p workbench-web --lib \
          --target wasm32-unknown-unknown --features hydrate
        runHook postBuild
      '';
      installPhase = ''
        runHook preInstall
        mkdir -p $out/pkg $out/assets/xterm
        wasm-bindgen --target web --no-typescript --out-dir $out/pkg --out-name workbench-web \
          target/wasm32-unknown-unknown/wasm-release/workbench_web.wasm
        wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int \
          -o $out/pkg/workbench-web_bg.wasm $out/pkg/workbench-web_bg.wasm
        cp web/assets/* $out/assets/
        tar -xzf ${xterm} package/lib/xterm.js package/css/xterm.css package/LICENSE
        cp package/lib/xterm.js package/css/xterm.css package/LICENSE $out/assets/xterm/
        runHook postInstall
      '';
      dontFixup = true;
    }
  );
in
pkgs.rustPlatform.buildRustPackage (
  common
  // {
    pname = "workbench-web";
    cargoBuildFlags = [
      "-p"
      "workbench-web"
      "--features"
      "ssr"
      "--bin"
      "workbench-web"
    ];
    passthru = { inherit site; };
    meta = common.meta // {
      description = "workbench's web UI, a Leptos app behind single sign-on";
      mainProgram = "workbench-web";
    };
  }
)
