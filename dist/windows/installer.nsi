; AccessKit Remote — per-user installer for the RDP DVC plug-in DLL and the
; WSL-side daemon.
;
; Driven by `cargo xtask dist`, which passes:
;   -DVERSION=<x.y.z>  -DSTAGING=<dir>  -DOUTFILE=<setup.exe>
; STAGING contains windows\<arch>\<dll>, linux\<arch>\<daemon>, and
; linux\{install.sh,accesskit-remoted.service}.

Unicode true
SetCompressor /SOLID LZMA

!include "MUI2.nsh"
!include "x64.nsh"
!include "LogicLib.nsh"
!include "nsDialogs.nsh"

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
!define DAEMON "accesskit_remoted"
!define ARP "Software\Microsoft\Windows\CurrentVersion\Uninstall\AccessKitRemote"

Name "AccessKit Remote ${VERSION}"
OutFile "${OUTFILE}"
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\AccessKit\Remote"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "AccessKit Remote"
VIAddVersionKey "FileDescription" "AccessKit Remote installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "Leonard de Ruijter"

Var Distro       ; WSL distro to provision (empty => skip WSL side)
Var Dialog
Var DistroList

!define MUI_ABORTWARNING
!insertmacro MUI_PAGE_WELCOME
Page custom DistroPageCreate DistroPageLeave
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; Read the default WSL distro name from the registry into $Distro.
!macro ReadDefaultDistro
  Push $0
  ReadRegStr $0 HKCU "Software\Microsoft\Windows\CurrentVersion\Lxss" "DefaultDistribution"
  ${If} $0 != ""
    ReadRegStr $Distro HKCU "Software\Microsoft\Windows\CurrentVersion\Lxss\$0" "DistributionName"
  ${EndIf}
  Pop $0
!macroend

Function .onInit
  !insertmacro ReadDefaultDistro
FunctionEnd

; --- distro selection page (interactive only) ---------------------------------

Function DistroPageCreate
  ; Silent install keeps the registry default distro; no page.
  ${If} ${Silent}
    Abort
  ${EndIf}
  !insertmacro MUI_HEADER_TEXT "WSL distribution" \
    "Choose the WSL distribution to run the AccessKit Remote daemon in."
  nsDialogs::Create 1018
  Pop $Dialog
  ${If} $Dialog == error
    Abort
  ${EndIf}
  ${NSD_CreateLabel} 0 0 100% 12u "WSL &distribution to provision:"
  Pop $0
  ${NSD_CreateDropList} 0 14u 100% 80u ""
  Pop $DistroList
  Call PopulateDistros
  nsDialogs::Show
FunctionEnd

Function PopulateDistros
  Push $0
  Push $1
  Push $2
  Push $3
  Push $4
  Push $5
  ; WSL_UTF8=1 makes wsl emit UTF-8 (its default UTF-16 is unreadable here).
  ; wsl.exe lives only in System32; call it by full path with WoW64 redirection
  ; off, so the 32-bit stub does not resolve it under SysWOW64.
  System::Call 'kernel32::SetEnvironmentVariable(t "WSL_UTF8", t "1")'
  ${DisableX64FSRedirection}
  nsExec::ExecToStack '"$SYSDIR\wsl.exe" -l -q'
  Pop $0
  Pop $1
  ${EnableX64FSRedirection}
  ${If} $0 != 0
    Goto done
  ${EndIf}
  loop:
    StrCmp $1 "" done
    StrCpy $2 0
    scan:
      StrCpy $3 $1 1 $2
      StrCmp $3 "" lineAll
      StrCmp $3 "$\n" line
      IntOp $2 $2 + 1
      Goto scan
    line:
      StrCpy $4 $1 $2
      IntOp $5 $2 + 1
      StrCpy $1 $1 "" $5
      Goto add
    lineAll:
      StrCpy $4 $1
      StrCpy $1 ""
    add:
      StrCpy $3 $4 1 -1
      StrCmp $3 "$\r" 0 +2
        StrCpy $4 $4 -1
      StrCmp $4 "" loop
      ${NSD_CB_AddString} $DistroList $4
      Goto loop
  done:
  ${If} $Distro != ""
    ${NSD_CB_SelectString} $DistroList $Distro
  ${EndIf}
  Pop $5
  Pop $4
  Pop $3
  Pop $2
  Pop $1
  Pop $0
FunctionEnd

Function DistroPageLeave
  ${NSD_GetText} $DistroList $0
  ${If} $0 != ""
    StrCpy $Distro $0
  ${EndIf}
FunctionEnd

; Register the (64-bit) DLL via the native regsvr32. The NSIS stub is 32-bit,
; so $SYSDIR is SysWOW64 until WoW64 redirection is disabled.
!macro Regsvr32 flags
  ${DisableX64FSRedirection}
  ExecWait '"$SYSDIR\regsvr32.exe" /s ${flags} "$INSTDIR\${DLL}"' $0
  ${EnableX64FSRedirection}
!macroend

; Run the staged install.sh inside $Distro (${args} = "" or "--uninstall").
; wsl.exe lives only in System32; disable WoW64 redirection so the 32-bit stub
; does not look for it in SysWOW64.
; -d takes the distro name UNQUOTED: wsl.exe does not strip quotes there and
; would treat "Debian" (with quotes) as the literal name. --cd does strip them,
; so it stays quoted to tolerate spaces in the profile path.
!macro WslInstall args
  ${DisableX64FSRedirection}
  nsExec::ExecToLog '"$SYSDIR\wsl.exe" -d $Distro --cd "$INSTDIR\wsl" bash ./install.sh ${args}'
  Pop $0
  ${EnableX64FSRedirection}
!macroend

Section "Install"
  SetOutPath "$INSTDIR"
  ; Both arch DLLs are packed; only the native one is extracted.
  ${If} ${IsNativeARM64}
    File "/oname=${DLL}" "${STAGING}\windows\aarch64\${DLL}"
  ${Else}
    File "/oname=${DLL}" "${STAGING}\windows\x86_64\${DLL}"
  ${EndIf}

  !insertmacro Regsvr32 ""
  ${If} $0 <> 0
    DetailPrint "regsvr32 returned $0"
  ${EndIf}

  ; WSL payload: arch-matched daemon + shared install assets.
  SetOutPath "$INSTDIR\wsl"
  ${If} ${IsNativeARM64}
    File "/oname=${DAEMON}" "${STAGING}\linux\aarch64\${DAEMON}"
  ${Else}
    File "/oname=${DAEMON}" "${STAGING}\linux\x86_64\${DAEMON}"
  ${EndIf}
  File "${STAGING}\linux\install.sh"
  File "${STAGING}\linux\accesskit-remoted.service"

  ${If} $Distro != ""
    DetailPrint "Provisioning daemon in WSL distro: $Distro"
    !insertmacro WslInstall ""
    ${If} $0 <> 0
      DetailPrint "WSL provisioning failed (exit $0)."
      ${IfNot} ${Silent}
        MessageBox MB_OK|MB_ICONEXCLAMATION \
          "Installed the Windows plug-in, but provisioning the WSL daemon failed.$\r$\nYou can run install.sh in your distro manually from:$\r$\n$INSTDIR\wsl" /SD IDOK
      ${EndIf}
    ${EndIf}
  ${Else}
    DetailPrint "No WSL distro selected; skipping daemon provisioning."
  ${EndIf}

  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "${ARP}" "DisplayName" "AccessKit Remote"
  WriteRegStr HKCU "${ARP}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${ARP}" "Publisher" "Leonard de Ruijter"
  WriteRegStr HKCU "${ARP}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${ARP}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${ARP}" "WslDistro" "$Distro"
  WriteRegDWORD HKCU "${ARP}" "NoModify" 1
  WriteRegDWORD HKCU "${ARP}" "NoRepair" 1
SectionEnd

Function un.onInit
  ReadRegStr $Distro HKCU "${ARP}" "WslDistro"
FunctionEnd

Section "Uninstall"
  ; Tear down the WSL daemon first, while the payload still exists.
  ${If} $Distro != ""
    !insertmacro WslInstall "--uninstall"
  ${EndIf}

  !insertmacro Regsvr32 "/u"
  Delete "$INSTDIR\wsl\${DAEMON}"
  Delete "$INSTDIR\wsl\install.sh"
  Delete "$INSTDIR\wsl\accesskit-remoted.service"
  RMDir "$INSTDIR\wsl"
  ; msrdc may still hold the DLL if a WSLg/RDP session is active; unregistration
  ; already ran, so if the file is locked, remove it on the next reboot.
  Delete /REBOOTOK "$INSTDIR\${DLL}"
  Delete "$INSTDIR\uninstall.exe"
  RMDir /REBOOTOK "$INSTDIR"
  RMDir "$LOCALAPPDATA\AccessKit"
  DeleteRegKey HKCU "${ARP}"
SectionEnd
