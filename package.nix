{
  pkgs,
  fenix,
  gitignore,
  naersk-src,
  target ? "x86_64-unknown-linux-musl",
  rust-toolchain ? with fenix; combine [
    stable.toolchain
    targets.${target}.stable.rust-std
  ],
}:
let
  libraries = with pkgs; [
    openssl
    stdenv.cc.cc.lib
  ];

  toolchain = rust-toolchain;
  naersk = pkgs.callPackage naersk-src {
    cargo = toolchain;
    rustc = toolchain;
  };
in
{
  inherit pkgs libraries rust-toolchain;

  leaves = naersk.buildPackage {
    CARGO_BUILD_TARGET = target;
    src = gitignore.gitignoreSource ./.;
  };
}
