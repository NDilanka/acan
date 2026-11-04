# Acan pre-release

A hackathon pre-release. The files are not code-signed, so Windows SmartScreen may warn. Choose
"More info", then "Run anyway".

## New in 0.1.0-pre.3

- Start with an empty world. Opening Acan shows an empty canvas. The plus button adds the folder of
  a git repository as a new plant while the server runs, and the camera flies to it. Added
  codebases are remembered for the next start.
- `acan open`. One command starts Acan in the background, or finds the copy already running, and
  opens your browser signed in. `--no-browser` skips the browser, and `--port` picks the port
  (default 7311). If it cannot start, the reason is in `open.log` in `%LOCALAPPDATA%\Acan` on
  Windows.
- Windows shortcuts. The Start menu and desktop shortcuts run `acan open` with no console window.
  If Acan cannot start, a message names the log file. Install, update and uninstall stop a running
  Acan first. After an in-app update, Acan restarts and the open tab reconnects, with no second
  tab.
- Minimap. A small map in the bottom-right corner of the world canvas shows every plant and the
  part you are looking at. Click or drag on it to move the view. Press G then M to reach it from
  the keyboard. It hides when there is only one plant.
- Plant layout. A codebase now grows as a round town: one block per crate or folder on a faint
  ground pad with one name sign, short machine names, wide streets, and belts that run along the
  streets and merge into trunks.
- More machine kinds. A machine's kind now also comes from its file names and the packages it
  imports, so codebases show more varied machines. Each crate's test folders group into one
  machine.
- Imports between the crates of a Rust workspace now show as belts.
- Settings. A settings file with keys this version does not know still loads. Those keys are kept
  when settings are saved, and the server names them once in a warning at startup.

## Files

- `acan-setup-<version>-windows-x64.exe`: Acan for Windows. It installs for your user with no admin
  rights, adds `acan` to your PATH, and adds a Start menu entry.
- `acan-demo-setup-<version>-windows-x64.exe`: the Acan Demo for Windows. A demo repository and a
  recorded run, with Start menu entries for each mode.
- `acan-<version>-linux-x64.tar.gz`: Acan for Linux (x86_64). Unpack it and run `sh install.sh`.
- `acan-demo-<version>-linux-x64.tar.gz`: the Acan Demo for Linux. Unpack it and run
  `sh start-acan-demo.sh`.
- `SHA256SUMS`: checksums of the four files above.

## Run it

1. Start the Acan Demo first. It needs no keys.
2. For your own project, start Acan and choose the folder of a git repository. Open the address
   that it prints.
3. To use a model, paste a key on the setup page of the server, or set `META_API_KEY`,
   `BASETEN_API_KEY` or `FIREWORKS_API_KEY`. Keys stay on your machine.

## Needs

- Git on your PATH.
- Docker Desktop for candidate code runs.

## Not in this pre-release

- macOS builds.
- Code signing.
