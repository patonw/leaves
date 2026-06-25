{
  pkgs,
  fenix,
  gitignore,
  naersk,
  rust-toolchain ? fenix.combine [
    fenix.stable.toolchain
    fenix.targets.wasm32-unknown-unknown.latest.rust-std
  ],
}:
let
  libraries = with pkgs; [
    openssl
    wasmtime
    stdenv.cc.cc.lib
  ];

  callPackage = pkgs.lib.callPackageWith {
    inherit pkgs fenix rust-toolchain naersk gitignore;
    inherit (gitignore) gitignoreSource;
  };
in
{
  inherit pkgs libraries rust-toolchain;

  # aerie = callPackage ./aerie {};
  # embasee = callPackage ./embasee {};
  # embcp-server = callPackage ./embcp-server {};
}

