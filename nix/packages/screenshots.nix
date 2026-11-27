# `nix run .#screenshots`: the web UI's screens across a front and two
# nodes, which checks.multi-node's browser takes with made-up people and
# workspaces, into docs/fleet (or the directory given), for docs/fleet.md.
{
  pkgs,
  flake,
  system,
  ...
}:

pkgs.writeShellApplication {
  name = "screenshots";
  runtimeInputs = [
    pkgs.coreutils
    pkgs.oxipng
  ];
  text = ''
    dest=''${1:-docs/fleet}
    install -d "$dest"
    install -m 0644 ${flake.checks.${system}.multi-node}/shots/*.png "$dest"/
    oxipng --quiet --opt 2 --strip safe "$dest"/*.png
    echo "The screens are in $dest."
  '';
}
