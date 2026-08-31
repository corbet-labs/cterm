{
  description = "cterm development environment";

  # Nixpkgs 26.11 dropped x86_64-darwin. Keep the supported 26.05 line while
  # cterm still ships universal Intel/Apple Silicon macOS builds.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [
              cargo
              clippy
              pkg-config
              protobuf
              rust-analyzer
              rustc
              rustfmt
            ];

            buildInputs = nixpkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux (with pkgs; [
              cairo
              gtk4
              libadwaita
              pango
              wayland
            ]);

            PROTOC = nixpkgs.lib.getExe pkgs.protobuf;
          };
        });
    };
}
