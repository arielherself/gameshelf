{
  lib,
  rustPlatform,
  makeWrapper,
  # umu-launcher from the official flake (see flake.nix). Set to null to use
  # whatever `umu-run` is on PATH at runtime instead.
  umu-launcher ? null,
}:
let
  cargoToml = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "gameshelf";
  inherit (cargoToml.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../tests
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = lib.optionals (umu-launcher != null) [ makeWrapper ];

  postInstall = lib.optionalString (umu-launcher != null) ''
    wrapProgram $out/bin/gameshelf \
      --prefix PATH : ${lib.makeBinPath [ umu-launcher ]}
  '';

  meta = {
    description = cargoToml.package.description;
    homepage = "https://github.com/arielherself/gameshelf";
    mainProgram = "gameshelf";
    platforms = lib.platforms.linux;
  };
}
