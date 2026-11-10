; Jarvis Clip installer (Inno Setup 6). CI builds it with:
;   iscc /DAppVersion=1.2.3 installer.iss
; Per-user install, no admin prompt. Bundles the Vosk voice files from
; dist\vosk (CI downloads and checks them first), so voice works offline right
; away. The app stays portable: config, log and buffer live next to the exe in
; the install folder; clips go to Videos\Jarvis Clip.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{6E3B1F0A-8C1D-4B7E-9C52-3A1E2D7F9B40}
AppName=Jarvis Clip
AppVersion={#AppVersion}
AppPublisher=YaanFzl
AppPublisherURL=https://github.com/YaanFzl/JarvisClip
DefaultDirName={localappdata}\Programs\Jarvis Clip
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
OutputDir=dist
OutputBaseFilename=JarvisClip-Setup
UninstallDisplayIcon={app}\jarvis-clip.exe
UninstallDisplayName=Jarvis Clip
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17134

[Tasks]
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "target\release\jarvis-clip.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "dist\vosk\*.dll"; DestDir: "{app}\vosk"; Flags: ignoreversion
Source: "dist\vosk\model\*"; DestDir: "{app}\vosk\model"; Flags: ignoreversion recursesubdirs
Source: "LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"
Source: "THIRD-PARTY-NOTICES.md"; DestDir: "{app}"

[Icons]
Name: "{userprograms}\Jarvis Clip"; Filename: "{app}\jarvis-clip.exe"
Name: "{userdesktop}\Jarvis Clip"; Filename: "{app}\jarvis-clip.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\jarvis-clip.exe"; Description: "Start Jarvis Clip"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Everything the app makes next to itself. Clips in Videos are left alone.
Type: filesandordirs; Name: "{app}\buffer"
Type: filesandordirs; Name: "{app}\vosk"
Type: filesandordirs; Name: "{app}\vosk.download"
Type: filesandordirs; Name: "{app}\ui-profile"
Type: files; Name: "{app}\config.toml"
Type: files; Name: "{app}\jarvis-clip.log"

[Code]
const
  CSIDL_MYVIDEO = $000E;

{ Ask a running copy to quit (it finishes any recording), then make sure. }
procedure StopApp;
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/im jarvis-clip.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Sleep(1500);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/f /im jarvis-clip.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopApp;
  Result := '';
end;

{ First install only: save clips to Videos\Jarvis Clip rather than inside the
  install folder, where nobody would find them. }
procedure CurStepChanged(CurStep: TSetupStep);
var
  Config: String;
begin
  if CurStep = ssPostInstall then
  begin
    Config := ExpandConstant('{app}\config.toml');
    if not FileExists(Config) then
      SaveStringToFile(Config, 'save_folder = ''' + GetShellFolderByCSIDL(CSIDL_MYVIDEO, False) + '\Jarvis Clip''' + #13#10, False);
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    StopApp;
    { "Start with Windows" points at the exe that's about to go. }
    RegDeleteValue(HKEY_CURRENT_USER, 'Software\Microsoft\Windows\CurrentVersion\Run', 'Jarvis Clip');
  end;
end;
