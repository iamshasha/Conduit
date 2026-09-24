; Conduit Windows installer (Inno Setup 6).
;
; Built in CI, once per architecture. Pass the staged payload and metadata on
; the command line, e.g.:
;
;   iscc /DAppVersion=0.1.0 /DArch=x64 /DSourceDir=stage-x64 packaging\windows\conduit.iss
;
; The staged SourceDir must contain conduit.exe and a gui\ folder (the WinUI
; publish output), matching the release.yml layout.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef Arch
  #define Arch "x64"
#endif
#ifndef SourceDir
  #define SourceDir "stage"
#endif

[Setup]
AppId={{7E9C4B2A-1D3F-4E6A-9C2B-CD11A0F2E100}
AppName=Conduit
AppVersion={#AppVersion}
AppPublisher=Conduit
DefaultDirName={autopf}\Conduit
DefaultGroupName=Conduit
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\conduit.exe
OutputDir=..\..\dist
OutputBaseFilename=Conduit-{#AppVersion}-{#Arch}-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; Per-user by default (no UAC prompt); the user may still choose all-users.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
#if Arch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif

[Tasks]
Name: "autostart"; Description: "Start Conduit when I sign in (runs minimized in the tray)"; Flags: unchecked
Name: "protocol"; Description: "Register the conduit:// link so a page can open the consent window"

[Files]
Source: "{#SourceDir}\conduit.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\gui\*"; DestDir: "{app}\gui"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\Conduit"; Filename: "{app}\conduit.exe"
Name: "{group}\Uninstall Conduit"; Filename: "{uninstallexe}"

[Registry]
; conduit:// URL scheme — mirrors src/system/windows.rs set_protocol, under the
; hive Inno is installing into (HKCU for per-user, HKLM for all-users).
Root: HKA; Subkey: "Software\Classes\conduit"; ValueType: string; ValueName: ""; ValueData: "URL:Conduit"; Flags: uninsdeletekey; Tasks: protocol
Root: HKA; Subkey: "Software\Classes\conduit"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""; Tasks: protocol
Root: HKA; Subkey: "Software\Classes\conduit\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\conduit.exe"",0"; Tasks: protocol
Root: HKA; Subkey: "Software\Classes\conduit\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\conduit.exe"" --url ""%1"""; Tasks: protocol
; Start with Windows (per-user Run key).
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Conduit"; ValueData: """{app}\conduit.exe"" --minimized"; Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\conduit.exe"; Description: "Launch Conduit now"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Stop a running instance so its files can be removed.
Filename: "{cmd}"; Parameters: "/C taskkill /IM conduit.exe /F"; Flags: runhidden; RunOnceId: "KillConduit"
