!macro NSIS_HOOK_PREINSTALL
  ; Do not kill sidecars by process basename. The desktop owns its worker through
  ; the Windows Job Object, and Tauri/NSIS handles the running desktop instance.
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; M7 autostart is authoritative in settings. Uninstall removes only the stale
  ; OS registration and deliberately leaves settings/recordings/credentials.
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Nian Vision"
  ; Closing the owning desktop closes the Job Object and reaps only its worker.
!macroend
