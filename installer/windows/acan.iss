; The Acan app installer, for one Windows user and no admin rights.
;
; It installs to %LOCALAPPDATA%\Programs\Acan, so the in-app updater can replace the files without
; elevation. It packs the stage folder as it is, so the installer holds exactly the files that
; scripts/package.ps1 checked. Build it with:
;   ISCC /DStageDir=<stage folder> /DVersion=<version> /DOutName=<file name without .exe> /O<folder> installer\windows\acan.iss
;
; The in-app updater runs it silently:
;   acan-setup-<version>-windows-x64.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART
; Restart Manager closes a running acan.exe from this folder first. Acan starts again at the end
; with --no-browser, so the tab already open reconnects, unless /NOLAUNCH=1 is given. The PATH
; entry is for this user only, and it is removed on uninstall.

#ifndef StageDir
  #error StageDir must be passed to ISCC with /DStageDir=<folder>
#endif
#ifndef Version
  #define Version "0.0.0-dev"
#endif
#ifndef OutName
  #define OutName "acan-setup"
#endif

[Setup]
AppId={{6E3B2F1A-9C4D-4A7E-8B51-1F0C7A2D9E01}
AppName=Acan
AppVersion={#Version}
AppPublisher=Acan pre-release
DefaultDirName={localappdata}\Programs\Acan
DefaultGroupName=Acan
DisableProgramGroupPage=yes
DisableDirPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
ChangesEnvironment=yes
CloseApplications=force
CloseApplicationsFilter=acan.exe,acan-worker.exe
RestartApplications=no
UninstallDisplayName=Acan
UninstallDisplayIcon={app}\acan.exe
OutputBaseFilename={#OutName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: checkedonce

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\Acan"; Filename: "{sys}\wscript.exe"; Parameters: """{app}\acan-open.vbs"""; WorkingDir: "{app}"; IconFilename: "{app}\acan.exe"; Comment: "Open Acan in your browser"
Name: "{autodesktop}\Acan"; Filename: "{sys}\wscript.exe"; Parameters: """{app}\acan-open.vbs"""; WorkingDir: "{app}"; IconFilename: "{app}\acan.exe"; Comment: "Open Acan in your browser"; Tasks: desktopicon
Name: "{group}\Uninstall Acan"; Filename: "{uninstallexe}"

[Run]
Filename: "{sys}\wscript.exe"; Parameters: "{code:OpenParams}"; Description: "Open Acan"; WorkingDir: "{app}"; Flags: nowait postinstall runasoriginaluser; Check: LaunchWanted

[Code]
const
  EnvKey = 'Environment';

// True unless the setup ran with /NOLAUNCH=1. The in-app updater does not pass that flag, so Acan
// starts again after an update.
function LaunchWanted: Boolean;
begin
  Result := ExpandConstant('{param:NOLAUNCH|0}') <> '1';
end;

// The launcher and its argument. A silent setup is the in-app update, and the browser tab that
// started it is still open, so Acan starts without opening another one.
function OpenParams(Param: String): String;
begin
  Result := '"' + ExpandConstant('{app}\acan-open.vbs') + '"';
  if WizardSilent then Result := Result + ' --no-browser';
end;

// Drops every PATH entry equal to Dir, ignoring case. Empty entries go too.
function WithoutEntry(Path, Dir: String): String;
var
  Rest, Part: String;
  P: Integer;
begin
  Result := '';
  Rest := Path + ';';
  while Rest <> '' do
  begin
    P := Pos(';', Rest);
    Part := Copy(Rest, 1, P - 1);
    Delete(Rest, 1, P);
    if (Part <> '') and (Uppercase(Part) <> Uppercase(Dir)) then
    begin
      if Result <> '' then Result := Result + ';';
      Result := Result + Part;
    end;
  end;
end;

function HasEntry(Path, Dir: String): Boolean;
begin
  Result := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Path) + ';') > 0;
end;

procedure AddToUserPath;
var
  Path, Dir: String;
begin
  Dir := ExpandConstant('{app}');
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', Path) then Path := '';
  if HasEntry(Path, Dir) then Exit;
  if (Path <> '') and (Copy(Path, Length(Path), 1) <> ';') then Path := Path + ';';
  RegWriteExpandStringValue(HKCU, EnvKey, 'Path', Path + Dir);
end;

procedure RemoveFromUserPath;
var
  Path: String;
begin
  if RegQueryStringValue(HKCU, EnvKey, 'Path', Path) then
    RegWriteExpandStringValue(HKCU, EnvKey, 'Path', WithoutEntry(Path, ExpandConstant('{app}')));
end;

// Stops every acan and acan-worker process that runs from this install folder, so its files can
// be replaced or removed. The shortcuts start the server with no window, so Restart Manager may not
// find one to close. This stop covers installs, updates and uninstalls.
// Each process is asked to stop first and gets 3 seconds, then any that is left is forced.
// After that, the single-server files of acan open (server.lock, server.pid, server.port) are
// removed when the process named in server.pid is gone, so a server from another folder keeps its
// files. The logs next to them stay.
// Both folders go into single-quoted PowerShell strings, so their own single quotes are doubled.
// The command has no braces: the removal is a list repeated once or zero times, not an if block.
// Get-Process leaves an error for a name with no process, so the command ends with exit 0.
procedure StopServers;
var
  App, Data: String;
  ResultCode: Integer;
begin
  App := ExpandConstant('{app}');
  StringChangeEx(App, '''', '''''', True);
  Data := ExpandConstant('{localappdata}\Acan');
  StringChangeEx(Data, '''', '''''', True);
  Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    '-NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -Command "' +
    '$p = @(Get-Process -Name acan,acan-worker -ErrorAction SilentlyContinue | Where-Object Path -like ''' + App + '\*''); ' +
    '$p | Stop-Process -ErrorAction SilentlyContinue; ' +
    '$p | Wait-Process -Timeout 3 -ErrorAction SilentlyContinue; ' +
    '$p | Where-Object HasExited -eq $false | Stop-Process -Force -ErrorAction SilentlyContinue; ' +
    '$d = ''' + Data + '''; ' +
    '$sp = Get-Content -LiteralPath (Join-Path $d ''server.pid'') -ErrorAction SilentlyContinue | Select-Object -First 1; ' +
    '$live = [bool]($sp -and (Get-Process -Id $sp -ErrorAction SilentlyContinue)); ' +
    '@((Join-Path $d ''server.lock''), (Join-Path $d ''server.pid''), (Join-Path $d ''server.port'')) * [int](-not $live) | Remove-Item -Force -ErrorAction SilentlyContinue; ' +
    'exit 0"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopServers;
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then AddToUserPath;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then StopServers;
  if CurUninstallStep = usPostUninstall then RemoveFromUserPath;
end;
