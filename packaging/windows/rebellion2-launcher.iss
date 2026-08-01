; Inno Setup script for Rebellion 2 (launcher model).
; Ships the asset-free game player + the launcher; runs the launcher on finish so
; the user verifies ownership and downloads Content before first play.
; Per-user install (no admin/UAC). Compile:
;   ISCC /DGameDir=<asset-free player dir> /DLauncherPath=<rebellion2-launcher.exe>
;        /DAppVersion=<x.y.z> /DOutputDir=<dir> /DIconFile=<.ico> rebellion2-launcher.iss

#ifndef GameDir
  #define GameDir "assetfree-player"
#endif
#ifndef LauncherPath
  #define LauncherPath "rebellion2-launcher.exe"
#endif
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef OutputDir
  #define OutputDir "dist"
#endif
#ifndef IconFile
  #define IconFile ""
#endif

#define MyAppName "Rebellion 2"
#define MyAppPublisher "AdasGames"
#define MyLauncherExe "rebellion2-launcher.exe"

[Setup]
AppId={{7C3F1E92-5A4B-4D8E-9F21-3B6C8A2D4E10}
AppName={#MyAppName}
AppVersion={#AppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={localappdata}\Programs\{#MyAppPublisher}\{#MyAppName}
DefaultGroupName={#MyAppPublisher}\{#MyAppName}
UninstallDisplayIcon={app}\{#MyLauncherExe}
OutputDir={#OutputDir}
OutputBaseFilename=Rebellion2-{#AppVersion}-Setup
Compression=lzma2/max
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
WizardStyle=modern
PrivilegesRequired=lowest
DisableProgramGroupPage=yes
#if IconFile != ""
SetupIconFile={#IconFile}
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional icons:"; Flags: unchecked

[Files]
; Asset-free game player (no baked art — Content is downloaded by the launcher).
Source: "{#GameDir}\*"; DestDir: "{app}"; Flags: recursesubdirs createallsubdirs ignoreversion
; The launcher (verifies ownership, downloads Content, starts the game).
Source: "{#LauncherPath}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; Shortcuts point at the launcher — it is the entry point.
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyLauncherExe}"
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyLauncherExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyLauncherExe}"; Description: "Launch {#MyAppName}"; Flags: nowait postinstall skipifsilent
