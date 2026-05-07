; Inno Setup script for the LeSwitcheur Windows installer.
;
; Built by .github/workflows/release.yml on tag push, alongside the macOS
; .dmg and the existing portable .zip. Per-user install (no UAC), unsigned
; for now (see TODO(signing) below).
;
; The app self-registers its `leswitcheur://` URL scheme and its
; "launch at login" Run key on first launch — see
; crates/switcheur-platform/src/windows/{url_scheme,startup}.rs. This
; installer therefore deliberately ships no [Registry] section: a stale
; installer-written value would fight a freshly self-installed binary.
; User config in %APPDATA%\gmbl\LeSwitcheur and %LOCALAPPDATA%\fr.gmbl.LeSwitcheur
; is preserved across uninstalls (matches the macOS .app removal behaviour),
; hence no [UninstallDelete] section either.
;
; Local build (after `cargo build --release -p switcheur`):
;   & "${env:ProgramFiles(x86)}\Inno Setup 6\iscc.exe" `
;       /DAppVersion=0.0.0-dev bundle\windows\installer.iss

#define AppName       "LeSwitcheur"
#define AppPublisher  "Olivier Guimbal"
#define AppExeName    "LeSwitcheur.exe"
#define AppURL        "https://leswitcheur.app"

#ifndef AppVersion
  #error AppVersion must be supplied: iscc /DAppVersion=X.Y.Z installer.iss
#endif

[Setup]
; AppId keys the uninstaller and Inno's upgrade detection — must stay
; constant across every release. Do not regenerate.
AppId={{4DE3BF14-63B4-4849-A3F6-FC9C1F3F5EBF}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}
AppUpdatesURL={#AppURL}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesInstallIn64BitMode=x64compatible
ArchitecturesAllowed=x64compatible
OutputDir=..\..\dist
OutputBaseFilename=LeSwitcheur-v{#AppVersion}-windows-x86_64-setup
SetupIconFile=AppIcon.ico
UninstallDisplayIcon={app}\{#AppExeName}
UninstallDisplayName={#AppName}
WizardStyle=modern
Compression=lzma2
SolidCompression=yes
LicenseFile=..\..\LICENSE
; Force-close any running instance so Inno can replace the exe in-place.
; Without this, the named-mutex single-instance lock in
; crates/switcheur-platform/src/windows/single_instance.rs would refuse the
; overwrite and the installer would error out on upgrade.
CloseApplications=force
RestartApplications=no
MinVersion=10.0

; TODO(signing): when an EV/OV cert lands, add
;   SignTool=signtool $f
; here and configure the named "signtool" via /Ssigntool=... in CI. Until
; then SmartScreen will warn on first download.

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "..\..\target\release\switcheur.exe"; DestDir: "{app}"; DestName: "{#AppExeName}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent
