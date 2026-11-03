; The Acan Demo installer, for one Windows user and no admin rights.
;
; It installs a demo copy of Acan to %LOCALAPPDATA%\Programs\Acan Demo: the server, its web UI,
; the recorded run, and a git bundle of the demo repository. The Start menu entries run
; start-acan-demo.ps1, which makes a fresh copy of the bundle on each start. Build it with:
;   ISCC /DStageDir=<stage folder> /DVersion=<version> /DOutName=<file name without .exe> /O<folder> installer\windows\acan-demo.iss

#ifndef StageDir
  #error StageDir must be passed to ISCC with /DStageDir=<folder>
#endif
#ifndef Version
  #define Version "0.0.0-dev"
#endif
#ifndef OutName
  #define OutName "acan-demo-setup"
#endif

[Setup]
AppId={{A1D4C0E7-3B2F-4E8A-9C65-7D2B0F4E1A02}
AppName=Acan Demo
AppVersion={#Version}
AppPublisher=Acan pre-release
DefaultDirName={localappdata}\Programs\Acan Demo
DefaultGroupName=Acan Demo
DisableProgramGroupPage=yes
DisableDirPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=force
CloseApplicationsFilter=acan.exe,acan-worker.exe
RestartApplications=no
UninstallDisplayName=Acan Demo
UninstallDisplayIcon={app}\acan.exe
OutputBaseFilename={#OutName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\Start Acan Demo"; Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\start-acan-demo.ps1"""; WorkingDir: "{app}"; Comment: "Start the demo in its default mode"
Name: "{group}\Acan Demo, replay"; Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\start-acan-demo.ps1"" -Mode replay"; WorkingDir: "{app}"; Comment: "The recorded run. No keys needed."
Name: "{group}\Acan Demo, live"; Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\start-acan-demo.ps1"" -Mode live"; WorkingDir: "{app}"; Comment: "The live page. No keys needed for the offline replies."
Name: "{group}\Uninstall Acan Demo"; Filename: "{uninstallexe}"

[Run]
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\start-acan-demo.ps1"""; Description: "Start Acan Demo"; WorkingDir: "{app}"; Flags: nowait postinstall skipifsilent
