; Inno Setup script for Rebellion 2 (launcher model).
; Ships the asset-free game player + the launcher; runs the launcher on finish so
; the user verifies ownership and downloads Content before first play.
; Per-user install (no admin/UAC). Compile:
;   ISCC /DGameDir=<asset-free player dir> /DLauncherPath=<rebellion2-launcher.exe>
;        /DUpdateHelperPath=<rebellion2-self-update.exe>
;        /DLauncherManifestPath=<manifest.json> /DLauncherVersionPath=<version.txt>
;        /DGameManifestPath=<manifest.json> /DGameVersionPath=<version.txt>
;        /DAppVersion=<x.y.z>
;        /DOutputDir=<dir> /DIconFile=<.ico> rebellion2-launcher.iss

#ifndef GameDir
  #define GameDir "assetfree-player"
#endif
#ifndef LauncherPath
  #define LauncherPath "rebellion2-launcher.exe"
#endif
#ifndef UpdateHelperPath
  #define UpdateHelperPath "rebellion2-self-update.exe"
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
#define MyGameExe "Rebellion2.exe"

[Setup]
AppId={{7C3F1E92-5A4B-4D8E-9F21-3B6C8A2D4E10}
AppName={#MyAppName}
AppVersion={#AppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={localappdata}\Programs\{#MyAppPublisher}\{#MyAppName}
DefaultGroupName={#MyAppPublisher}\{#MyAppName}
UninstallDisplayIcon={app}\{#MyGameExe}
OutputDir={#OutputDir}
OutputBaseFilename=Rebellion2-{#AppVersion}-Setup
Compression=lzma2/max
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
WizardStyle=modern
PrivilegesRequired=lowest
DisableProgramGroupPage=yes
; In-use files must be closed before setup replaces them. The preparation step
; also rejects a launcher that currently owns the installation.
CloseApplications=yes
RestartApplications=no
#if IconFile != ""
SetupIconFile={#IconFile}
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Add &Rebellion 2 to the desktop"; GroupDescription: "Additional icons:"; Flags: unchecked

[InstallDelete]
; Rebuild the game-owned native plugin directory instead of retaining obsolete plugins.
Type: filesandordirs; Name: "{app}\Rebellion2_Data\Plugins\x86_64"

[Files]
; Keep recovery first so extracting it does not decompress the player payload.
Source: "{#UpdateHelperPath}"; DestName: "rebellion2-installer-recovery.exe"; Flags: dontcopy
; Asset-free game player (no baked art — Content is downloaded by the launcher).
Source: "{#GameDir}\*"; DestDir: "{app}"; Flags: recursesubdirs createallsubdirs ignoreversion
; The launcher (verifies ownership, downloads Content, starts the game).
Source: "{#LauncherPath}"; DestDir: "{app}"; Flags: ignoreversion
; A tiny installed helper promotes a staged launcher only after the running launcher exits.
Source: "{#UpdateHelperPath}"; DestDir: "{app}"; DestName: "rebellion2-update-helper.exe"; Flags: ignoreversion
; Independent baselines for launcher and game updates. Both exclude Content.
#ifdef LauncherManifestPath
Source: "{#LauncherManifestPath}"; DestDir: "{app}"; DestName: ".launcher-manifest.json"; Flags: ignoreversion
#endif
#ifdef LauncherVersionPath
Source: "{#LauncherVersionPath}"; DestDir: "{app}"; DestName: ".launcher-version"; Flags: ignoreversion
#endif
#ifdef GameManifestPath
Source: "{#GameManifestPath}"; DestDir: "{app}"; DestName: ".game-manifest.json"; Flags: ignoreversion
#endif
#ifdef GameVersionPath
Source: "{#GameVersionPath}"; DestDir: "{app}"; DestName: ".game-version"; Flags: ignoreversion
#endif
; The one-time transition installer retains the former combined baseline.
#ifdef ApplicationManifestPath
Source: "{#ApplicationManifestPath}"; DestDir: "{app}"; DestName: ".application-manifest.json"; Flags: ignoreversion
#endif
#ifdef ApplicationVersionPath
Source: "{#ApplicationVersionPath}"; DestDir: "{app}"; DestName: ".application-version"; Flags: ignoreversion
#endif

[Icons]
; The "Rebellion 2" shortcuts point at the launcher: it checks for updates, then
; plays (auto-launching straight through when nothing needs downloading). Repair
; stays available as an explicit re-verify/reinstall entry.
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyLauncherExe}"; WorkingDir: "{app}"
Name: "{group}\Repair {#MyAppName} Content"; Filename: "{app}\{#MyLauncherExe}"; Parameters: "--repair"; WorkingDir: "{app}"
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyLauncherExe}"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
; First-run setup still goes through the launcher so it can verify ownership, install
; Content, write the local version marker, and then start the game.
Filename: "{app}\{#MyLauncherExe}"; Description: "Run {#MyAppName}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; The launcher downloads Content (and writes launcher.log / a partial download) into
; {app} at runtime, so Inno's install log doesn't track them. Remove them explicitly
; on uninstall, then drop {app} if nothing else remains.
Type: filesandordirs; Name: "{app}\Content"
Type: filesandordirs; Name: "{app}\.download-cache"
Type: filesandordirs; Name: "{app}\.content-packs"
Type: filesandordirs; Name: "{app}\.rebellion2-update-transaction"
Type: filesandordirs; Name: "{app}\.content-install-staging"
Type: files; Name: "{app}\launcher.log"
Type: files; Name: "{app}\.session"
Type: files; Name: "{app}\.rebellion2-install.lock"
Type: files; Name: "{app}\content.zip"
Type: files; Name: "{app}\content.zip.part"
Type: files; Name: "{app}\content.zip.part.etag"
Type: files; Name: "{app}\content.zip.part.size"
Type: files; Name: "{app}\.approved-content-update"
Type: files; Name: "{app}\.launcher-manifest.json"
Type: files; Name: "{app}\.launcher-version"
Type: files; Name: "{app}\.launcher-manifest.pending.json"
Type: files; Name: "{app}\.launcher-version.pending"
Type: files; Name: "{app}\.game-manifest.json"
Type: files; Name: "{app}\.game-version"
Type: files; Name: "{app}\.application-manifest.json"
Type: files; Name: "{app}\.application-version"
Type: files; Name: "{app}\.application-manifest.pending.json"
Type: files; Name: "{app}\.application-version.pending"
Type: files; Name: "{app}\.rebellion2-launcher.next.exe"
Type: files; Name: "{app}\.rebellion2-launcher.next.exe.writing"
Type: files; Name: "{app}\rebellion2-update-helper.exe.writing"
Type: files; Name: "{app}\.launcher-manifest.pending.json.writing"
Type: files; Name: "{app}\.launcher-version.pending.writing"
Type: files; Name: "{app}\.application-manifest.pending.json.writing"
Type: files; Name: "{app}\.application-version.pending.writing"
Type: files; Name: "{app}\.game-manifest.json.writing"
Type: files; Name: "{app}\.game-version.writing"
Type: dirifempty; Name: "{app}"

[Code]
var
  InstallationPrepared: Boolean;
  InstallationLock: THandle;

function CreateInstallationLock(const FileName: String; Access, Sharing: LongWord;
  Security: THandle; Creation, Attributes: LongWord; Template: THandle): THandle;
  external 'CreateFileW@kernel32.dll stdcall';
function LockInstallation(FileHandle: THandle; OffsetLow, OffsetHigh, CountLow, CountHigh: LongWord): Boolean;
  external 'LockFile@kernel32.dll stdcall';
function CloseInstallationLock(FileHandle: THandle): Boolean;
  external 'CloseHandle@kernel32.dll stdcall';

procedure ReleaseInstallation;
begin
  if InstallationLock <> 0 then begin
    CloseInstallationLock(InstallationLock);
    InstallationLock := 0;
  end;
end;

procedure PrepareInstallation;
var
  ResultCode: Integer;
begin
  if InstallationPrepared then exit;
  ExtractTemporaryFile('rebellion2-installer-recovery.exe');
  WizardForm.StatusLabel.Caption := 'Restoring interrupted updates...';
  if not ExecAndLogOutput(ExpandConstant('{tmp}\rebellion2-installer-recovery.exe'),
    '--recover-installation "' + ExpandConstant('{app}') + '"', '', SW_HIDE,
    ewWaitUntilTerminated, ResultCode, nil) then
    RaiseException('The installation could not be prepared: ' + SysErrorMessage(ResultCode));
  if ResultCode <> 0 then
    RaiseException('The previous update could not be restored. Close the game and launcher, then retry setup.');

  { Keep the same lock until setup finishes replacing its files. }
  InstallationLock := CreateInstallationLock(ExpandConstant('{app}\.rebellion2-install.lock'),
    $C0000000, 7, 0, 4, $80, 0);
  if InstallationLock = THandle(-1) then begin
    InstallationLock := 0;
    RaiseException('The installation folder could not be locked.');
  end;
  if not LockInstallation(InstallationLock, 0, 0, 1, 0) then begin
    ReleaseInstallation;
    RaiseException('Another launcher is using this installation. Close it, then retry setup.');
  end;
  InstallationPrepared := True;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  try
    PrepareInstallation;
  except
    ReleaseInstallation;
    Result := GetExceptionMessage;
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then ReleaseInstallation;
end;

procedure DeinitializeSetup;
begin
  ReleaseInstallation;
end;
