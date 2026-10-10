# gameshelf

A terminal game launcher for Windows games on Linux, running them through
[umu-launcher](https://github.com/Open-Wine-Components/umu-launcher) and Proton.

- Every top-level directory in your library (default `~/Games`) is a game.
- New games are matched against the Steam store automatically (title,
  developer, release date, genres, description). You can also pick a match
  from search results or set the app id by hand.
- Each game gets its own wine prefix in `<library>/.proton-prefixes`, or
  shares one you choose.
- Proton builds live in `<library>/.proton-runtimes`. GE-Proton, UMU-Proton
  and Proton-CachyOS can be downloaded (checksum-verified) from inside the
  app, and existing builds from Steam's `compatibilitytools.d` can be linked.
- Everything is also scriptable: `gameshelf run celeste`.

## Install

With flakes:

```nix
# flake.nix
inputs.gameshelf.url = "github:arielherself/gameshelf";

# configuration.nix / home.nix
environment.systemPackages = [ inputs.gameshelf.packages.${pkgs.system}.default ];
# or: nixpkgs.overlays = [ inputs.gameshelf.overlays.default ];  then pkgs.gameshelf
```

Or try it directly: `nix run github:arielherself/gameshelf`.

The package uses umu-launcher from
[its official flake](https://github.com/Open-Wine-Components/umu-launcher#from-our-flake)
and puts `umu-run` on gameshelf's `PATH`, so nothing else is needed. Outside
Nix, `cargo install --path .` works too, with `umu-run` on your `PATH`.

## Usage

`gameshelf` with no arguments opens the TUI:

| Key | Action |
| --- | --- |
| `Enter` | launch the selected game |
| `/` | filter · `o` toggle sort (name / recently played) |
| `m` | search Steam and pick the match |
| `a` | set the Steam app id by hand (empty = none, `auto` = match again) |
| `e` / `p` / `r` | choose executable / wine prefix / Proton runtime |
| `A` | edit launch arguments |
| `R` | runtime manager: download, link, delete, set the library default |
| `s` | rescan the library · `?` help · `q` quit |

From the command line:

```sh
gameshelf list                      # all games (* = app id set by hand)
gameshelf info sinking              # details; games match by dir, title, app id or fragment
gameshelf run "Order of the Sinking Star" -- -windowed
gameshelf run celeste --dry-run     # print the umu-run command instead
gameshelf run celeste --detach      # return immediately, log to ~/.local/state/gameshelf/logs

gameshelf search paws shelter       # find an app id
gameshelf set Paws --appid 434000   # set it by hand (`auto` to re-match, `none` to clear)
gameshelf set celeste --exe bin/Celeste.exe --prefix shared --runtime GE-Proton11-7-x86_64
gameshelf set celeste --args "-windowed -nointro" --env DXVK_HUD=fps
gameshelf set celeste --args "" --unset-env DXVK_HUD
gameshelf set celeste --prefix new --runtime default --exe auto   # back to defaults

gameshelf runtime available         # GE-Proton, UMU-Proton, Proton-CachyOS releases
gameshelf runtime install ge        # latest GE-Proton (or ge:GE-Proton11-6, cachyos, umu, a full name)
gameshelf runtime link ~/.local/share/Steam/compatibilitytools.d/GE-Proton11-7-x86_64
gameshelf runtime default GE-Proton11-7-x86_64
gameshelf runtime list | remove <name>
```

Global options: `-L/--library DIR` (or `GAMESHELF_LIBRARY`) and `--offline`
(skip Steam lookups; new games are looked up on the next online run).

## How games are launched

For a game the command is equivalent to:

```sh
cd "<dir of the exe>" && \
PROTON_PREFER_SDL=1 \
WINEPREFIX="<library>/.proton-prefixes/<prefix>" \
PROTONPATH="<library>/.proton-runtimes/<runtime>" \
GAMEID=umu-<steam appid> \
umu-run "<exe>" <args>
```

- `GAMEID` is `umu-<appid>` when the app id is known, so umu can apply the
  game's protonfixes, and `0` otherwise. Override it per game with
  `set --game-id`, or set `gameid_from_appid = false` in the lock file.
- The runtime is the game's own setting, else the library default, else the
  most recently installed runtime.
- Environment variables come from `[settings.env]` (`PROTON_PREFER_SDL=1` by
  default) and then the game's own `env`.
- New games adopt an existing prefix named after the directory (as-is or
  snake_case, e.g. `OrderOfTheSinkingStar` → `order_of_the_sinking_star`), so
  prefixes made by hand keep working. Otherwise a new one is created on first
  launch.
- Games started from the TUI are detached (they keep running after you quit)
  and log to `~/.local/state/gameshelf/logs/<game>.log`. Playtime is recorded
  when the game exits, as long as gameshelf is still running.

## Files

| Path | Contents |
| --- | --- |
| `<library>/gameshelf.lock` | TOML: library settings and per-game metadata |
| `<library>/.proton-prefixes/` | wine prefixes |
| `<library>/.proton-runtimes/` | Proton builds (directories or symlinks) |
| `<library>/.gameshelf.flock` | advisory lock for concurrent updates |
| `~/.config/gameshelf/config.toml` | optional: `library = "~/Games"` |
| `~/.cache/gameshelf/` | GitHub release lists (15 min) |

Hidden directories in the library are ignored. A game whose directory
disappears keeps its entry (marked missing) until the directory comes back.

The lock file is plain TOML and safe to edit by hand, for example:

```toml
version = 1

[settings]
default_runtime = "GE-Proton11-7-x86_64"
gameid_from_appid = true
# umu_run = "/path/to/umu-run"

[settings.env]
PROTON_PREFER_SDL = "1"

[games.OrderOfTheSinkingStar]
match = "matched"          # pending | matched | unmatched | manual
appid = 499170
executable = "sinking_star.exe"
prefix = "order_of_the_sinking_star"
# runtime = "proton-cachyos-11.0-20261005-slr-x86_64"
# args = ["-windowed"]
# env = { DXVK_HUD = "fps" }
```

GitHub's API allows 60 unauthenticated requests per hour. gameshelf uses
`GITHUB_TOKEN`, `GH_TOKEN` or a logged-in GitHub CLI (`gh auth token`) when
available, and only sends it to api.github.com.

## Development

```sh
nix develop          # cargo, clippy, rustfmt, rust-analyzer, umu-run
cargo test           # unit tests + end-to-end tests against a mock Steam/GitHub
nix build            # the package (also runs the tests)
```

## License

[MIT](LICENSE)
