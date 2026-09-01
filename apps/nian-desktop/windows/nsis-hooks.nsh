!macro NSIS_HOOK_PREINSTALL
  ; Ensure an old worker cannot keep the sidecar/DLLs locked during upgrade.
  nsExec::ExecToLog 'taskkill /F /IM nian-media-worker.exe'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; M7 autostart is authoritative in settings. Uninstall removes only the stale
  ; OS registration and deliberately leaves settings/recordings/credentials.
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Nian Vision"
  nsExec::ExecToLog 'taskkill /F /IM nian-media-worker.exe'
!macroend
