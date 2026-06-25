{
  pkgs,
  fenix,
  gitignore,
  naersk,
  rust-toolchain ? fenix.combine [
    fenix.stable.toolchain
  ],
}:
let
  libraries = with pkgs; [
    openssl
    stdenv.cc.cc.lib
  ];

  callPackage = pkgs.lib.callPackageWith {
    inherit pkgs fenix rust-toolchain naersk gitignore;
    inherit (gitignore) gitignoreSource;
  };
in
{
  inherit pkgs libraries rust-toolchain;
}

