# import ./package.nix {}
let
  lock = builtins.fromJSON (builtins.readFile ./flake.lock);
  flake-compat = fetchTarball {
    url = "https://github.com/NixOS/flake-compat/archive/${lock.nodes.flake-compat.locked.rev}.tar.gz";
    sha256 = lock.nodes.flake-compat.locked.narHash;
  };
in
(import flake-compat { src = ./.; }).defaultNix.outputs.packages.${builtins.currentSystem}
