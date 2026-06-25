{
  _workspace ? import ./.,
  pkgs ? _workspace.pkgs,
  libraries ? _workspace.libraries,
  rust-toolchain ? _workspace.rust-toolchain,
}:
pkgs.mkShell {
  LD_LIBRARY_PATH = "${pkgs.lib.makeLibraryPath libraries}";
  packages = with pkgs; [
    niv
    cargo-generate
    mdbook
    mdbook-d2
    pkg-config
    rust-toolchain
    stgit
  ] ++ libraries;
}
