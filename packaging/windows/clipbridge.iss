; Inno Setup script for ClipBridge.
; Build: iscc /DAppVersion=0.1.0 /DSourceExe=path\to\clipbridge.exe packaging\windows\clipbridge.iss

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\..\target\release\clipbridge.exe"
#endif

[Setup]
AppId={{6F0B7C2E-3A51-4E8B-9C2D-1B7A4E5F9D30}
AppName=ClipBridge
AppVersion={#AppVersion}
AppPublisher=ClipBridge
DefaultDirName={localappdata}\Programs\ClipBridge
DisableDirPage=yes
DisableProgramGroupPage=yes
; Per-user install, no admin prompt.
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
SetupIconFile=..\..\assets\icon.ico
UninstallDisplayIcon={app}\clipbridge.exe
UninstallDisplayName=ClipBridge
OutputDir=..\..\dist
OutputBaseFilename=ClipBridge-{#AppVersion}-windows-x64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Tasks]
Name: "autostart"; Description: "Start ClipBridge when I sign in"; Flags: checkedonce
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "clipbridge.exe"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\ClipBridge"; Filename: "{app}\clipbridge.exe"
Name: "{userdesktop}\ClipBridge"; Filename: "{app}\clipbridge.exe"; Tasks: desktopicon

[Registry]
; Same value name the app's "Start at login" toggle uses, so both stay in sync.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "ClipBridge"; ValueData: """{app}\clipbridge.exe"""; Tasks: autostart

[Run]
Filename: "{app}\clipbridge.exe"; Description: "Launch ClipBridge"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM clipbridge.exe"; Flags: runhidden; RunOnceId: "KillClipBridge"

[Code]
// Close a running copy before files are replaced on upgrade.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM clipbridge.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Sleep(300);
  Result := '';
end;

// Autostart may also have been enabled from the tray; remove it either way.
// User settings in %APPDATA%\ClipBridge are kept.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'ClipBridge');
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run', 'ClipBridge');
  end;
end;
