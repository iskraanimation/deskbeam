; DeskBeam's Windows installer (plan §4, Packaging), built with Inno Setup 6:
;
;   iscc /DAppVersion=0.1.0 /DBuildDir=..\..\..\..\target\release apps\deskbeam\packaging\windows\deskbeam.iss
;
; BuildDir must hold deskbeam.exe, wintun.dll (amd64, from https://www.wintun.net/builds) and
; that zip's LICENSE.txt as wintun-LICENSE.txt.
; .github/workflows/release.yml puts them there. The installer lands in BuildDir too.
;
; It installs for all users under Program Files, which needs admin rights once. That is on
; purpose: LAN mode starts deskbeam.exe with admin rights and it loads wintun.dll, so neither
; may sit in a folder a normal user can write to.
;
; The data folder (%APPDATA%\DeskBeam: identity, pairings, chat history) is left in place on
; uninstall, as the user's own data.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef BuildDir
  #define BuildDir "..\..\..\..\target\release"
#endif
#define Root "..\..\..\.."

[Setup]
; Never change AppId: Windows uses it to find the installed copy for upgrades and uninstall.
AppId={{2A3FFF43-76CB-45EE-87F5-143AE0630202}
AppName=DeskBeam
AppVersion={#AppVersion}
AppVerName=DeskBeam {#AppVersion}
AppPublisher=DeskBeam contributors
AppComments=Remote desktop, file sharing and chat between paired devices
DefaultDirName={autopf}\DeskBeam
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#BuildDir}
OutputBaseFilename=DeskBeam-{#AppVersion}-windows-x64-setup
SetupIconFile={#Root}\apps\deskbeam\assets\deskbeam.ico
UninstallDisplayIcon={app}\deskbeam.exe
UninstallDisplayName=DeskBeam
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; A running DeskBeam is closed for the upgrade (Restart Manager) and started again after.
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#BuildDir}\deskbeam.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BuildDir}\wintun.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BuildDir}\wintun-LICENSE.txt"; DestDir: "{app}"
Source: "{#Root}\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"
Source: "{#Root}\NOTICE"; DestDir: "{app}"; DestName: "NOTICE.txt"

[Icons]
Name: "{autoprograms}\DeskBeam"; Filename: "{app}\deskbeam.exe"
Name: "{autodesktop}\DeskBeam"; Filename: "{app}\deskbeam.exe"; Tasks: desktopicon

[Registry]
; "Start at login" (Settings) writes these for the user; remove them with the app so Windows
; doesn't try to start a program that is gone.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "DeskBeam"; Flags: uninsdeletevalue dontcreatekey
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"; ValueType: none; ValueName: "DeskBeam"; Flags: uninsdeletevalue dontcreatekey

[Run]
Filename: "{app}\deskbeam.exe"; Description: "{cm:LaunchProgram,DeskBeam}"; Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
; Close DeskBeam so its files can be removed.
Filename: "{sys}\taskkill.exe"; Parameters: "/f /im deskbeam.exe"; Flags: runhidden; RunOnceId: "StopDeskBeam"
