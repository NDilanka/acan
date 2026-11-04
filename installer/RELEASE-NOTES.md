# Acan pre-release

A hackathon pre-release. The files are not code-signed, so Windows SmartScreen may warn. Choose
"More info", then "Run anyway".

## Files

- `acan-setup-<version>-windows-x64.exe`: Acan for Windows. It installs for your user with no admin
  rights, adds `acan` to your PATH, and adds Start menu and desktop shortcuts that open Acan in your browser.
- `acan-demo-setup-<version>-windows-x64.exe`: the Acan Demo for Windows. A demo repository and a
  recorded run, with Start menu entries for each mode.
- `acan-<version>-linux-x64.tar.gz`: Acan for Linux (x86_64). Unpack it and run `sh install.sh`.
- `acan-demo-<version>-linux-x64.tar.gz`: the Acan Demo for Linux. Unpack it and run
  `sh start-acan-demo.sh`.
- `SHA256SUMS`: checksums of the four files above.

## Run it

1. Start the Acan Demo first. It needs no keys.
2. For your own project, start Acan and add the folder of a git repository. On Windows, the Start
   menu opens an empty canvas in your browser, and its plus button adds the folder. On Linux, open
   the address that Acan prints.
3. To use a model, paste a key on the setup page of the server, or set `META_API_KEY`,
   `BASETEN_API_KEY` or `FIREWORKS_API_KEY`. Keys stay on your machine.

## Needs

- Git on your PATH.
- Docker Desktop for candidate code runs.

## Not in this pre-release

- macOS builds.
- Code signing.
