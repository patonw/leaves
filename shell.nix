{
  _workspace ? import ./.,
  pkgs ? _workspace.pkgs,
  libraries ? _workspace.libraries,
  rust-toolchain ? _workspace.rust-toolchain,
  leaves ? _workspace.leaves,
}:
pkgs.mkShell {
  LD_LIBRARY_PATH = "${pkgs.lib.makeLibraryPath libraries}";
  packages = with pkgs; [
    pkg-config
    rust-toolchain
    leaves
  ] ++ libraries;
}
