; Kinodisc installer hooks (tauri.conf.json > bundle > windows > nsis > installerHooks).

; 1.2.1 installed a developer tool, export-bindings.exe, next to the app.
; Updates keep files the new package does not list, so remove it here.
!macro NSIS_HOOK_PREINSTALL
  Delete "$INSTDIR\export-bindings.exe"
!macroend
