{
  lib,
  rustPlatform,
  makeWrapper,
  sdl3,
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

  # Controllers are read through SDL.
  buildInputs = [ sdl3 ];

  # Appended, not prepended: a umu-run installed by the host distro wins.
  # Nix's umu-launcher runs games in an FHS sandbox that only finds GPU
  # drivers via /run/opengl-driver, i.e. on NixOS; on other distros the
  # distro package is the one that can actually render.
  postInstall = lib.optionalString (umu-launcher != null) ''
    wrapProgram $out/bin/gameshelf \
      --suffix PATH : ${lib.makeBinPath [ umu-launcher ]}
  '';

  meta = {
    description = cargoToml.package.description;
    homepage = "https://github.com/arielherself/gameshelf";
    license = lib.licenses.mit;
    mainProgram = "gameshelf";
    platforms = lib.platforms.linux;
  };
}
