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
  leaves = naersk.buildPackage {
    CARGO_BUILD_TARGET = target;
    src = gitignore.gitignoreSource ./.;
  };
in
{
  inherit pkgs libraries rust-toolchain leaves;

  default = leaves;

  perfShell = pkgs.mkShell {
    LD_LIBRARY_PATH = "${pkgs.lib.makeLibraryPath libraries}";
    packages = with pkgs; [
      niv
      cargo-generate
      mdbook
      mdbook-d2
      pkg-config
      rust-toolchain
      stgit
      perf
      cargo-flamegraph
      rust-addr2line
    ] ++ libraries;
  };
}
