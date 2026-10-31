{
  description = "clipt: fast, lightweight screenshot tool for Wayland and X11";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in {
      packages = forAll (pkgs: rec {
        clipt = pkgs.rustPlatform.buildRustPackage {
          pname = "clipt";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.pkg-config pkgs.makeWrapper ];
          buildInputs = [ pkgs.libxkbcommon ];
          # The tests load a system font through fontconfig.
          nativeCheckInputs = [ pkgs.fontconfig pkgs.dejavu_fonts ];
          preCheck = ''
            export FONTCONFIG_FILE=${pkgs.makeFontsConf { fontDirectories = [ pkgs.dejavu_fonts ]; }}
            export HOME=$TMPDIR
          '';
          postInstall = ''
            install -Dm644 data/clipt.desktop -t $out/share/applications
            install -Dm644 data/clipt.svg -t $out/share/icons/hicolor/scalable/apps
            # fc-match finds the UI font; make sure it's on PATH.
            wrapProgram $out/bin/clipt --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.fontconfig ]}
          '';
          meta = {
            description = "Fast, lightweight screenshot tool for Wayland and X11";
            homepage = "https://github.com/jasondyck/clipt";
            license = with pkgs.lib.licenses; [ mit asl20 ];
            mainProgram = "clipt";
            platforms = pkgs.lib.platforms.linux;
          };
        };
        default = clipt;
      });
      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.system}.clipt ];
          packages = [ pkgs.cargo pkgs.rustc pkgs.clippy pkgs.rustfmt ];
        };
      });
    };
}
