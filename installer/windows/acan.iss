; The Acan app installer, for one Windows user and no admin rights.
;
; It installs to %LOCALAPPDATA%\Programs\Acan, so the in-app updater can replace the files without
; elevation. It packs the stage folder as it is, so the installer holds exactly the files that
; scripts/package.ps1 checked. Build it with:
;   ISCC /DStageDir=<stage folder> /DVersion=<version> /DOutName=<file name without .exe> /O<folder> installer\windows\acan.iss
;
; The in-app updater runs it silently:
;   acan-setup-<version>-windows-x64.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART
; Restart Manager closes a running acan.exe from this folder first. Acan starts again at the end,
; unless /NOLAUNCH=1 is given. The PATH entry is for this user only, and it is removed on uninstall.

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

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\Acan"; Filename: "{app}\acan-start.cmd"; WorkingDir: "{app}"; Comment: "Start Acan on your repository"
Name: "{group}\Uninstall Acan"; Filename: "{uninstallexe}"

[Run]
Filename: "{app}\acan-start.cmd"; Description: "Start Acan"; WorkingDir: "{app}"; Flags: nowait postinstall runasoriginaluser shellexec; Check: LaunchWanted

[Code]
const
  EnvKey = 'Environment';

// True unless the setup ran with /NOLAUNCH=1. The in-app updater does not pass that flag, so Acan
// starts again after an update.
function LaunchWanted: Boolean;
begin
  Result := ExpandConstant('{param:NOLAUNCH|0}') <> '1';
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

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then AddToUserPath;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then RemoveFromUserPath;
end;
