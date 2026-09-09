{
  description = "An oneko clone that chases the cursor across a Wayland desktop";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    systems.url = "github:nix-systems/default-linux";
  };

  outputs = {
    self,
    nixpkgs,
    systems,
  }: let
    eachSystem = nixpkgs.lib.genAttrs (import systems);
    pkgsFor = system: nixpkgs.legacyPackages.${system};
  in {
    packages = eachSystem (system: let
      pkgs = pkgsFor system;
    in rec {
      nekors = pkgs.callPackage ./nix/package.nix {};
      default = nekors;
    });

    # A plain module rather than one closing over `self`, so it can also be
    # imported straight from a checkout or a fetched tarball; it builds the
    # package from the sources beside it.
    homeModules = {
      nekors = import ./nix/home-module.nix;
      default = self.homeModules.nekors;
    };

    # The real development environment is devenv (see devenv.nix); this is the
    # plain-nix fallback so `nix develop` is not a dead end.
    devShells = eachSystem (system: let
      pkgs = pkgsFor system;
    in {
      default = pkgs.mkShell {
        inputsFrom = [self.packages.${system}.nekors];
        packages = with pkgs; [clippy rustfmt rust-analyzer];
        RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
      };
    });

    formatter = eachSystem (system: (pkgsFor system).alejandra);
  };
}
