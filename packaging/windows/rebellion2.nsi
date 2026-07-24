; NSIS installer for Rebellion 2.
; Built on Linux via makensis. Required defines:
;   -DVERSION=<x.y.z>  -DSRCDIR=<path to StandaloneWindows64>  -DOUTFILE=<setup exe path>
; Optional define:
;   -DICONFILE=<path to .ico>  (installer wizard + Start-Menu shortcut icon)

!include "MUI2.nsh"

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef SRCDIR
  !error "SRCDIR must be defined: the Unity StandaloneWindows64 output directory."
!endif
!ifndef OUTFILE
  !define OUTFILE "Rebellion2-Setup.exe"
!endif

!define APPNAME "Rebellion 2"
!define PUBLISHER "AdasGames"
!define EXENAME "Rebellion2.exe"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Rebellion2"

Name "${APPNAME}"
OutFile "${OUTFILE}"
Unicode true
InstallDir "$PROGRAMFILES64\${APPNAME}"
InstallDirRegKey HKLM "Software\${PUBLISHER}\${APPNAME}" "InstallDir"
RequestExecutionLevel admin
ShowInstDetails show
ShowUninstDetails show

!define MUI_ABORTWARNING

!ifdef ICONFILE
  !define MUI_ICON "${ICONFILE}"
  !define MUI_UNICON "${ICONFILE}"
!endif

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXENAME}"
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Section "Rebellion 2" SecMain
  SetOutPath "$INSTDIR"
  File /r "${SRCDIR}/*"

!ifdef ICONFILE
  File "/oname=rebellion2.ico" "${ICONFILE}"
!endif

  WriteUninstaller "$INSTDIR\Uninstall.exe"

  CreateDirectory "$SMPROGRAMS\${APPNAME}"
!ifdef ICONFILE
  CreateShortcut "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" "$INSTDIR\${EXENAME}" "" "$INSTDIR\rebellion2.ico" 0
!else
  CreateShortcut "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" "$INSTDIR\${EXENAME}"
!endif
  CreateShortcut "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk" "$INSTDIR\Uninstall.exe"

  WriteRegStr HKLM "Software\${PUBLISHER}\${APPNAME}" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayName" "${APPNAME}"
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINSTKEY}" "Publisher" "${PUBLISHER}"
!ifdef ICONFILE
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\rebellion2.ico"
!else
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\${EXENAME}"
!endif
  WriteRegStr HKLM "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTKEY}" "UninstallString" "$INSTDIR\Uninstall.exe"
  WriteRegDWORD HKLM "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINSTKEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
  Delete "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk"
  RMDir "$SMPROGRAMS\${APPNAME}"

  RMDir /r "$INSTDIR"

  DeleteRegKey HKLM "${UNINSTKEY}"
  DeleteRegKey HKLM "Software\${PUBLISHER}\${APPNAME}"
SectionEnd
