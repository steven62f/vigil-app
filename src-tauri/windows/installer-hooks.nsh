!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "Nettoyage des hooks Vigil (Claude Code, Codex, Grok, Antigravity, Kimi)..."
  nsExec::ExecToLog 'powershell.exe -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File "$INSTDIR\cleanup-hooks.ps1"'
!macroend

; Story 10.3 — suppression du miroir des binaires de hook (%ProgramData%\Vigil\hooks\<SID>\).
;
; ⚠️ CE TRAVAIL N'APPARTIENT PAS À `cleanup-hooks.ps1` : ce script tourne AUSSI au pre-uninstall
; d'une simple MISE À JOUR (l'installeur désinstalle l'ancienne version avant de poser la nouvelle).
; Y supprimer les binaires provoquerait une recopie complète à chaque update — du churn pur. La
; suppression appartient donc au désinstalleur, et seulement quand il ne s'agit PAS d'une mise à
; jour : `$UpdateMode` vaut 1 lorsque l'uninstaller est appelé avec `/UPDATE`.
;
; POST-uninstall et non PRE- : `CheckIfAppIsRunning` a fermé Vigil entre-temps, donc plus aucun
; binaire de hook n'est verrouillé par un processus en vol (un fichier en cours d'exécution ne se
; supprime pas sous Windows, et laisserait un résidu).
;
; Le dossier est nommé par le SID du compte courant : chaque compte Windows ne retire QUE le sien,
; ceux des autres comptes survivent (ils lui sont de toute façon inaccessibles en écriture). Les deux
; dossiers parents ne partent que s'ils sont VIDES — d'où le test explicite plutôt qu'un
; `Remove-Item` qui échouerait bruyamment sur un dossier peuplé.
;
; `-NoProfile` (ajouté en revue 10.3) : sans lui, le profil PowerShell du compte s'exécute avant la
; charge utile et peut redéfinir `Remove-Item`, écrire sur la sortie, ou échouer. Le drapeau se place
; AVANT `-Command` et ne touche donc pas au quoting de la charge utile, dont l'avertissement plus bas
; interdit à juste titre la modification sans remesure.
;
; Charge utile SANS AUCUN guillemet, ni externe ni interne (`Join-Path $racine hooks` prend ses
; arguments en mots nus) : PowerShell recolle les arguments restants après `-Command`. Les `$$` sont
; l'échappement NSIS du `$`, et la chaîne embarquée a été relue dans l'exe compilé.
;
; ⚠️ MESURÉ, PAS SUPPOSÉ (2026-07-28, `CreateProcess` avec ligne de commande brute — ce que fait
; `nsExec`) : la forme « prudente » qui entoure la charge utile de guillemets échappés (`\"…\"`)
; **sort en 0 SANS RIEN SUPPRIMER**. Un faux succès silencieux, exactement le motif F7 du spike
; (Codex annonce « completed » sans exécuter). Ne pas « sécuriser » ce quoting sans le remesurer.
;
; Comportement vérifié sur installeur NSIS réellement compilé : `$UpdateMode = 1` → le miroir
; SURVIT ; `$UpdateMode = 0` → il part. Et côté charge utile : deux comptes → seul le nôtre part ;
; dernier compte → l'arborescence entière part ; second passage → aucun effet, sortie 0.
!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    DetailPrint "Suppression des binaires de hook Vigil (ProgramData)..."
    nsExec::ExecToLog 'powershell.exe -NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -Command $$sid=([Security.Principal.WindowsIdentity]::GetCurrent()).User.Value; $$racine=Join-Path $$env:ProgramData Vigil; $$hooks=Join-Path $$racine hooks; $$mien=Join-Path $$hooks $$sid; if (Test-Path -LiteralPath $$mien) { Remove-Item -LiteralPath $$mien -Recurse -Force -ErrorAction SilentlyContinue }; foreach ($$p in @($$hooks,$$racine)) { if ((Test-Path -LiteralPath $$p) -and -not (Get-ChildItem -LiteralPath $$p -Force)) { Remove-Item -LiteralPath $$p -Force -ErrorAction SilentlyContinue } }'
  ${EndIf}
!macroend
