; AccessKit Remote — per-user installer for the RDP DVC plug-in DLL.
;
; Driven by `cargo xtask dist`, which passes:
;   -DVERSION=<x.y.z>  -DSTAGING=<dir>  -DOUTFILE=<setup.exe>
; STAGING must contain windows\x86_64\<dll> and windows\aarch64\<dll>.

Unicode true
!include "x64.nsh"
!include "LogicLib.nsh"

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef STAGING
  !error "STAGING is required (cargo xtask dist passes -DSTAGING=<dir>)"
!endif
!ifndef OUTFILE
  !define OUTFILE "AccessKitRemote-Setup.exe"
!endif

!define DLL "accesskit_remote_dvc_plugin.dll"
!define ARP "Software\Microsoft\Windows\CurrentVersion\Uninstall\AccessKitRemote"

Name "AccessKit Remote ${VERSION}"
OutFile "${OUTFILE}"
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\AccessKit\Remote"
ShowInstDetails show
ShowUninstDetails show

Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

; Register the (64-bit) DLL with the native regsvr32. The NSIS stub is a 32-bit
; process, so $SYSDIR points at SysWOW64 until WoW64 redirection is disabled.
!macro Regsvr32 flags
  ${DisableX64FSRedirection}
  ExecWait '"$SYSDIR\regsvr32.exe" /s ${flags} "$INSTDIR\${DLL}"' $0
  ${EnableX64FSRedirection}
!macroend

Section "Install"
  SetOutPath "$INSTDIR"

  ; Both arch DLLs are packed at build time; only the native one is extracted.
  ${If} ${IsNativeARM64}
    File "/oname=${DLL}" "${STAGING}\windows\aarch64\${DLL}"
  ${Else}
    File "/oname=${DLL}" "${STAGING}\windows\x86_64\${DLL}"
  ${EndIf}

  !insertmacro Regsvr32 ""
  ${If} $0 <> 0
    DetailPrint "regsvr32 returned $0"
  ${EndIf}

  WriteUninstaller "$INSTDIR\uninstall.exe"

  WriteRegStr HKCU "${ARP}" "DisplayName" "AccessKit Remote"
  WriteRegStr HKCU "${ARP}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${ARP}" "Publisher" "Leonard de Ruijter"
  WriteRegStr HKCU "${ARP}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${ARP}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKCU "${ARP}" "NoModify" 1
  WriteRegDWORD HKCU "${ARP}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  !insertmacro Regsvr32 "/u"
  Delete "$INSTDIR\${DLL}"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  RMDir "$LOCALAPPDATA\AccessKit"
  DeleteRegKey HKCU "${ARP}"
SectionEnd
