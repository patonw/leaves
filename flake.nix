{
  description = "A very basic flake";

  inputs = {
    self.submodules = true;
    nixpkgs.url = "github:nixos/nixpkgs";
    flake-compat.url = "github:edolstra/flake-compat";
    flake-utils.url = "github:numtide/flake-utils";
    fenix_.url = "github:nix-community/fenix";
    gitignore-src.url = "github:hercules-ci/gitignore.nix";
    naersk-src = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, fenix_, gitignore-src, naersk-src,  flake-compat }: 
    flake-utils.lib.eachDefaultSystem (system:
      let
        # pkgs = nixpkgs.legacyPackages.${system};
        pkgs = (import nixpkgs) {
          inherit system;
          # overlays = [];
        };
        fenix = pkgs.callPackage fenix_ {};
        naersk = pkgs.callPackage naersk-src {};
        gitignore = pkgs.callPackage gitignore-src {};
        callPackage = pkgs.lib.callPackageWith {
          inherit pkgs fenix naersk naersk-src gitignore;
          inherit (gitignore) gitignoreSource;
        };
      in
      {
        packages = callPackage ./package.nix {};
      }
    );
}
