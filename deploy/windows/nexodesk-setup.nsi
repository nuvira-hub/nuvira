; NexoDesk setup for Windows.
;
; A wrapper around the installation the program already does by itself
; ("nexodesk.exe service install", the same as the app's "Install on this
; computer"): one installation logic to maintain. It copies the program to
; Program Files, installs the service for unattended access, adds the Start menu
; shortcut and the entry in the installed apps (whose uninstall command is the
; program's own).
;
; Server: from the command line (/SERVER=), else from the setup's file name
; (nexodesk-setup-host=nexodesk.example.com.exe, ",port=N" optional, as for the
; app), else typed in the wizard.
;
;   nexodesk-setup.exe                                  wizard
;   nexodesk-setup.exe /S /SERVER=host[:port] [/PASSWORD=...] [/KEY=fingerprint]
;
; Built by tools/build-setup.sh: makensis -DPROGRAM=... -DVERSION=... -DOUTFILE=...

Unicode true
ManifestDPIAware true
RequestExecutionLevel admin
SetCompressor /SOLID lzma

!ifndef PROGRAM
  !error "PROGRAM (path of nexodesk.exe) is required"
!endif
!ifndef VERSION
  !define VERSION "0.0.1"
!endif
!ifndef OUTFILE
  !define OUTFILE "nexodesk-setup.exe"
!endif

Name "NexoDesk"
OutFile "${OUTFILE}"
BrandingText "NexoDesk ${VERSION} · AGPL-3.0"
Icon "..\..\client\app\icons\icon.ico"

VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=0 "ProductName" "NexoDesk"
VIAddVersionKey /LANG=0 "FileDescription" "NexoDesk setup"
VIAddVersionKey /LANG=0 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=0 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=0 "LegalCopyright" "© 2026 Alessandro Blason · AGPL-3.0"

!include "MUI2.nsh"
!include "nsDialogs.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"

!define MUI_ICON "..\..\client\app\icons\icon.ico"
!define MUI_ABORTWARNING

Var Server
Var ServerKey
Var Password
Var Password2
Var ServerField
Var PasswordField
Var Password2Field

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "..\..\LICENSE"
Page custom SettingsPage SettingsLeave
!insertmacro MUI_PAGE_INSTFILES
; The app must not run as administrator: started through Explorer, as the person.
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_FUNCTION StartApp
!insertmacro MUI_PAGE_FINISH

; The first one is the fallback; otherwise Windows' language is used.
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "Italian"
!insertmacro MUI_LANGUAGE "French"
!insertmacro MUI_LANGUAGE "German"
!insertmacro MUI_LANGUAGE "Spanish"

LangString SettingsTitle ${LANG_ENGLISH} "Server and password"
LangString SettingsTitle ${LANG_ITALIAN} "Server e password"
LangString SettingsTitle ${LANG_FRENCH} "Serveur et mot de passe"
LangString SettingsTitle ${LANG_GERMAN} "Server und Passwort"
LangString SettingsTitle ${LANG_SPANISH} "Servidor y contraseña"

LangString SettingsSubtitle ${LANG_ENGLISH} "This computer will be reachable even when nobody is in front of it."
LangString SettingsSubtitle ${LANG_ITALIAN} "Questo computer sarà raggiungibile anche quando non c'è nessuno davanti."
LangString SettingsSubtitle ${LANG_FRENCH} "Cet ordinateur sera accessible même lorsque personne n'est devant."
LangString SettingsSubtitle ${LANG_GERMAN} "Dieser Computer ist erreichbar, auch wenn niemand davor sitzt."
LangString SettingsSubtitle ${LANG_SPANISH} "Se podrá acceder a este equipo aunque no haya nadie delante."

LangString ServerLabel ${LANG_ENGLISH} "NexoDesk server (e.g. nexodesk.example.com):"
LangString ServerLabel ${LANG_ITALIAN} "Server NexoDesk (es. nexodesk.example.com):"
LangString ServerLabel ${LANG_FRENCH} "Serveur NexoDesk (ex. nexodesk.example.com) :"
LangString ServerLabel ${LANG_GERMAN} "NexoDesk-Server (z. B. nexodesk.example.com):"
LangString ServerLabel ${LANG_SPANISH} "Servidor NexoDesk (p. ej. nexodesk.example.com):"

LangString PasswordLabel ${LANG_ENGLISH} "Permanent password (empty: one is generated and shown at the end):"
LangString PasswordLabel ${LANG_ITALIAN} "Password permanente (vuota: viene generata e mostrata alla fine):"
LangString PasswordLabel ${LANG_FRENCH} "Mot de passe permanent (vide : il est généré et affiché à la fin) :"
LangString PasswordLabel ${LANG_GERMAN} "Dauerhaftes Passwort (leer: wird erzeugt und am Ende angezeigt):"
LangString PasswordLabel ${LANG_SPANISH} "Contraseña permanente (vacía: se genera y se muestra al final):"

LangString Password2Label ${LANG_ENGLISH} "Repeat the password:"
LangString Password2Label ${LANG_ITALIAN} "Ripeti la password:"
LangString Password2Label ${LANG_FRENCH} "Répétez le mot de passe :"
LangString Password2Label ${LANG_GERMAN} "Passwort wiederholen:"
LangString Password2Label ${LANG_SPANISH} "Repite la contraseña:"

LangString ServerMissing ${LANG_ENGLISH} "Enter the address of the NexoDesk server."
LangString ServerMissing ${LANG_ITALIAN} "Scrivi l'indirizzo del server NexoDesk."
LangString ServerMissing ${LANG_FRENCH} "Saisissez l'adresse du serveur NexoDesk."
LangString ServerMissing ${LANG_GERMAN} "Geben Sie die Adresse des NexoDesk-Servers ein."
LangString ServerMissing ${LANG_SPANISH} "Escribe la dirección del servidor NexoDesk."

LangString PasswordMismatch ${LANG_ENGLISH} "The two passwords do not match."
LangString PasswordMismatch ${LANG_ITALIAN} "Le due password non coincidono."
LangString PasswordMismatch ${LANG_FRENCH} "Les deux mots de passe ne correspondent pas."
LangString PasswordMismatch ${LANG_GERMAN} "Die beiden Passwörter stimmen nicht überein."
LangString PasswordMismatch ${LANG_SPANISH} "Las dos contraseñas no coinciden."

LangString PasswordShort ${LANG_ENGLISH} "The permanent password must have at least 8 characters."
LangString PasswordShort ${LANG_ITALIAN} "La password permanente deve avere almeno 8 caratteri."
LangString PasswordShort ${LANG_FRENCH} "Le mot de passe permanent doit comporter au moins 8 caractères."
LangString PasswordShort ${LANG_GERMAN} "Das dauerhafte Passwort muss mindestens 8 Zeichen haben."
LangString PasswordShort ${LANG_SPANISH} "La contraseña permanente debe tener al menos 8 caracteres."

LangString Installing ${LANG_ENGLISH} "Installing NexoDesk and its service (up to a minute)..."
LangString Installing ${LANG_ITALIAN} "Installazione di NexoDesk e del servizio (fino a un minuto)..."
LangString Installing ${LANG_FRENCH} "Installation de NexoDesk et de son service (jusqu'à une minute)..."
LangString Installing ${LANG_GERMAN} "NexoDesk und sein Dienst werden installiert (bis zu einer Minute)..."
LangString Installing ${LANG_SPANISH} "Instalando NexoDesk y su servicio (hasta un minuto)..."

LangString Failed ${LANG_ENGLISH} "The installation failed:"
LangString Failed ${LANG_ITALIAN} "L'installazione non è riuscita:"
LangString Failed ${LANG_FRENCH} "L'installation a échoué :"
LangString Failed ${LANG_GERMAN} "Die Installation ist fehlgeschlagen:"
LangString Failed ${LANG_SPANISH} "La instalación no se completó:"

LangString Done ${LANG_ENGLISH} "NexoDesk is installed. Device ID:"
LangString Done ${LANG_ITALIAN} "NexoDesk è installato. ID del dispositivo:"
LangString Done ${LANG_FRENCH} "NexoDesk est installé. ID de l'appareil :"
LangString Done ${LANG_GERMAN} "NexoDesk ist installiert. Geräte-ID:"
LangString Done ${LANG_SPANISH} "NexoDesk está instalado. ID del dispositivo:"

LangString GeneratedPassword ${LANG_ENGLISH} "Permanent password (write it down: it is shown only now):"
LangString GeneratedPassword ${LANG_ITALIAN} "Password permanente (annotala: viene mostrata solo ora):"
LangString GeneratedPassword ${LANG_FRENCH} "Mot de passe permanent (notez-le : il n'est affiché que maintenant) :"
LangString GeneratedPassword ${LANG_GERMAN} "Dauerhaftes Passwort (notieren Sie es: Es wird nur jetzt angezeigt):"
LangString GeneratedPassword ${LANG_SPANISH} "Contraseña permanente (anótala: solo se muestra ahora):"

; --- server from the file name ------------------------------------------------

; What follows NEEDLE in $R0, up to the end, into $R1 ("" if absent).
!macro After NEEDLE
  Push $R2
  Push $R3
  Push $R4
  StrCpy $R1 ""
  StrLen $R3 `${NEEDLE}`
  StrCpy $R2 0
  ${Do}
    StrCpy $R4 $R0 $R3 $R2
    ${If} $R4 == ""
      ${Break}
    ${EndIf}
    ${If} $R4 == `${NEEDLE}`
      IntOp $R2 $R2 + $R3
      StrCpy $R1 $R0 "" $R2
      ${Break}
    ${EndIf}
    IntOp $R2 $R2 + 1
  ${Loop}
  Pop $R4
  Pop $R3
  Pop $R2
!macroend

; $R1 up to the first STOP character (excluded).
!macro CutAt STOP
  Push $R2
  Push $R3
  Push $R4
  StrCpy $R2 0
  StrCpy $R3 ""
  ${Do}
    StrCpy $R4 $R1 1 $R2
    ${If} $R4 == ""
    ${OrIf} $R4 == `${STOP}`
      ${Break}
    ${EndIf}
    StrCpy $R3 "$R3$R4"
    IntOp $R2 $R2 + 1
  ${Loop}
  StrCpy $R1 $R3
  Pop $R4
  Pop $R3
  Pop $R2
!macroend

; Value of "<name>=" in $R0 (the file name), up to "," or the end, into $R1.
!macro OptionFromName NAME
  !insertmacro After "${NAME}="
  !insertmacro CutAt ","
!macroend

Function ServerFromFileName
  ; Without ".exe" and without the " (1)" a browser adds to a repeated download.
  StrCpy $R0 $EXEFILE -4
  ${GetFileName} $R0 $R0
  Push $R0
  Call TrimCopySuffix
  Pop $R0
  !insertmacro OptionFromName "host"
  ${If} $R1 != ""
    StrCpy $Server $R1
    !insertmacro OptionFromName "port"
    ${If} $R1 != ""
      StrCpy $Server "$Server:$R1"
    ${EndIf}
    !insertmacro OptionFromName "key"
    StrCpy $ServerKey $R1
  ${EndIf}
FunctionEnd

; "name (12)" → "name".
Function TrimCopySuffix
  Exch $0
  Push $1
  Push $2
  StrCpy $1 $0 1 -1
  ${If} $1 == ")"
    StrLen $2 $0
    ${Do}
      IntOp $2 $2 - 1
      ${If} $2 < 2
        ${Break}
      ${EndIf}
      StrCpy $1 $0 2 $2
      ${If} $1 == " ("
        StrCpy $0 $0 $2
        ${Break}
      ${EndIf}
    ${Loop}
  ${EndIf}
  Pop $2
  Pop $1
  Exch $0
FunctionEnd

Function .onInit
  ${GetParameters} $R9
  ClearErrors
  ${GetOptions} $R9 "/SERVER=" $Server
  ${If} ${Errors}
    StrCpy $Server ""
    Call ServerFromFileName
  ${EndIf}
  ClearErrors
  ${GetOptions} $R9 "/KEY=" $R1
  ${IfNot} ${Errors}
    StrCpy $ServerKey $R1
  ${EndIf}
  ClearErrors
  ${GetOptions} $R9 "/PASSWORD=" $Password
  ${If} ${Errors}
    StrCpy $Password ""
  ${EndIf}
  StrCpy $Password2 $Password
  ${If} ${Silent}
  ${AndIf} $Server == ""
    SetErrorLevel 2
    Quit
  ${EndIf}
FunctionEnd

; --- settings page --------------------------------------------------------------

Function SettingsPage
  !insertmacro MUI_HEADER_TEXT "$(SettingsTitle)" "$(SettingsSubtitle)"
  nsDialogs::Create 1018
  Pop $0
  ${NSD_CreateLabel} 0 0 100% 12u "$(ServerLabel)"
  ${NSD_CreateText} 0 13u 100% 13u $Server
  Pop $ServerField
  ${NSD_CreateLabel} 0 38u 100% 12u "$(PasswordLabel)"
  ${NSD_CreatePassword} 0 51u 100% 13u $Password
  Pop $PasswordField
  ${NSD_CreateLabel} 0 72u 100% 12u "$(Password2Label)"
  ${NSD_CreatePassword} 0 85u 100% 13u $Password2
  Pop $Password2Field
  nsDialogs::Show
FunctionEnd

Function SettingsLeave
  ${NSD_GetText} $ServerField $Server
  ${NSD_GetText} $PasswordField $Password
  ${NSD_GetText} $Password2Field $Password2
  ${If} $Server == ""
    MessageBox MB_ICONEXCLAMATION "$(ServerMissing)"
    Abort
  ${EndIf}
  ${If} $Password != $Password2
    MessageBox MB_ICONEXCLAMATION "$(PasswordMismatch)"
    Abort
  ${EndIf}
  StrLen $0 $Password
  ${If} $0 > 0
  ${AndIf} $0 < 8
    MessageBox MB_ICONEXCLAMATION "$(PasswordShort)"
    Abort
  ${EndIf}
FunctionEnd

; --- installation ---------------------------------------------------------------

; Value of the JSON string field "<name>" in $R0 into $R1 ("" if absent). The
; report is written by the program: flat, no escapes in the fields read here.
!macro JsonField NAME
  !insertmacro After `"${NAME}":"`
  !insertmacro CutAt `"`
!macroend

Section "NexoDesk"
  InitPluginsDir
  SetOutPath $PLUGINSDIR
  File "/oname=$PLUGINSDIR\nexodesk.exe" "${PROGRAM}"
  DetailPrint "$(Installing)"

  StrCpy $0 'service install --server "$Server" --report "$PLUGINSDIR\report.json"'
  ${If} $ServerKey != ""
    StrCpy $0 '$0 --server-key $ServerKey'
  ${EndIf}
  ; The password goes in a file, not on the command line (readable by other programs).
  ${If} $Password != ""
    FileOpen $1 "$PLUGINSDIR\password.txt" w
    FileWrite $1 $Password
    FileClose $1
    StrCpy $0 '$0 --password-file "$PLUGINSDIR\password.txt"'
  ${EndIf}
  nsExec::ExecToLog '"$PLUGINSDIR\nexodesk.exe" $0'
  Pop $1
  Delete "$PLUGINSDIR\password.txt"

  ClearErrors
  FileOpen $2 "$PLUGINSDIR\report.json" r
  ${If} ${Errors}
    StrCpy $R0 ""
  ${Else}
    FileRead $2 $R0
    FileClose $2
  ${EndIf}
  Delete "$PLUGINSDIR\report.json"

  ${If} $1 != 0
  ${OrIf} $R0 == ""
    !insertmacro JsonField "error"
    MessageBox MB_ICONSTOP "$(Failed)$\r$\n$R1" /SD IDOK
    SetErrorLevel 1
    Abort "$(Failed) $R1"
  ${EndIf}
  !insertmacro JsonField "id"
  StrCpy $R6 $R1
  !insertmacro JsonField "password"
  StrCpy $R7 $R1
  DetailPrint "$(Done) $R6"
  ; Generated only on a first installation without a password (otherwise it is kept,
  ; and only its hash exists).
  ${If} $R7 != ""
    MessageBox MB_ICONINFORMATION "$(Done) $R6$\r$\n$\r$\n$(GeneratedPassword)$\r$\n$R7" /SD IDOK
  ${EndIf}
SectionEnd

Function StartApp
  Exec '"$WINDIR\explorer.exe" "$PROGRAMFILES64\NexoDesk\nexodesk.exe"'
FunctionEnd
