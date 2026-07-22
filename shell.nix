{
  _workspace ? import ./.,
  pkgs ? _workspace.pkgs,
  libraries ? _workspace.libraries,
  rust-toolchain ? _workspace.rust-toolchain,
}:
pkgs.mkShell {
  LD_LIBRARY_PATH = "${pkgs.lib.makeLibraryPath libraries}";
  packages = with pkgs; [
    pkg-config
    rust-toolchain
  ] ++ libraries;
}
