; Per-user installer for Deltos: installs in %LOCALAPPDATA%\Programs\Deltos with Start menu and
; desktop shortcuts and no admin prompt. Built by the release workflow with
;   iscc /DVersion=0.1.0 /DSource=<folder with deltos.exe> packaging\windows\deltos.iss

#ifndef Version
  #define Version "0.0.0"
#endif
#ifndef Source
  #define Source "..\..\target\release"
#endif

[Setup]
AppId={{9BC9DA39-56C4-4300-93E4-7CF0805D55E0}
AppName=Deltos
AppVersion={#Version}
AppVerName=Deltos {#Version}
AppPublisher=khaodius
AppPublisherURL=https://github.com/khaodius/deltos
AppSupportURL=https://github.com/khaodius/deltos/issues
AppUpdatesURL=https://github.com/khaodius/deltos/releases
DefaultDirName={localappdata}\Programs\Deltos
DefaultGroupName=Deltos
DisableProgramGroupPage=yes
DisableDirPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\..\dist
OutputBaseFilename=deltos-{#Version}-x64-setup
SetupIconFile=..\..\assets\icons\deltos.ico
UninstallDisplayIcon={app}\deltos.exe
UninstallDisplayName=Deltos
LicenseFile=..\..\LICENSE
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
CloseApplications=force
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "{#Source}\deltos.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\Deltos"; Filename: "{app}\deltos.exe"
Name: "{userdesktop}\Deltos"; Filename: "{app}\deltos.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\deltos.exe"; Description: "{cm:LaunchProgram,Deltos}"; Flags: nowait postinstall skipifsilent
; Deltos's own updater passes /RESTARTDELTOS=1 so the new version starts after a silent upgrade.
Filename: "{app}\deltos.exe"; Flags: nowait; Check: RestartRequested

[Code]
function RestartRequested: Boolean;
begin
  Result := ExpandConstant('{param:RESTARTDELTOS|0}') = '1';
end;
