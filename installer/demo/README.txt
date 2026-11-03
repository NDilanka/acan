Acan Demo, pre-release

This is a demo copy of a small repository with Acan running on it. Each start makes a new copy of
the demo repository, so the copy that came with this install is never changed.

Modes. The default is replay until the offline scope draft ships, then live.
  default   the recorded run now, and the live page once the offline scope draft ships
  replay    the recorded run. No keys needed.
  live      the live page, with the owner's link. No keys needed for the built-in offline replies.

Windows: start Acan Demo from the Start menu. The Start menu has an entry for each mode.
  Each copy goes under %LOCALAPPDATA%\Acan Demo\runs. Close the Acan server window to stop it.

Linux: from this folder, run
  sh start-acan-demo.sh [replay|live]
  Each copy goes under ~/.local/share/acan-demo/runs. The server runs in the background, and the
  script prints the command that stops it.

Needs Git: https://git-scm.com/download/win on Windows, or your package manager on Linux.
Acan runs candidate code in Docker containers, so Docker should be running for live work.

Keys: none are needed for the demo. To use a model, paste a key on the server's setup page
(http://127.0.0.1:7311/setup), or set META_API_KEY, BASETEN_API_KEY or FIREWORKS_API_KEY. A key
stays on this machine and is never written into a repository.

Pre-release: the files are not code-signed. Windows SmartScreen may warn. Choose "More info", then
"Run anyway".
