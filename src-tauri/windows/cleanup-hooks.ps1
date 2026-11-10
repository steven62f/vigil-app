# ── Motifs d'identité d'un binaire de hook Vigil ─────────────────────────────
#
# ⚠️ Le JOKER FINAL n'est pas cosmétique. Sans lui (`'*vigil-claude.exe'`), le motif exigeait que la
# commande TERMINE par le basename : dès que la Story 10.2 quote l'exe (`"<exe>"`), le guillemet
# final faisait échouer le `-like` et ce script cessait SILENCIEUSEMENT de nettoyer Claude et Codex —
# des blocs pointant un binaire supprimé restaient dans la config de l'utilisateur (revue 10.1).
# Le joker aligne le script sur le matcher tolérant de `config/mod.rs` (`command_designates_vigil`),
# qui reconnaît la forme quotée ET une commande qui embarque notre chemin.
#
# ⚠️ Les motifs Claude/Codex restent ANCRÉS sur le basename exact, là où Antigravity/Kimi peuvent se
# permettre `*vigil*.exe*` : `settings.json` et `hooks.json` sont PARTAGÉS avec des hooks tiers, et
# le motif large happe `vigilant.exe` (vérifié). Antigravity/Kimi n'opèrent que dans un namespace
# Vigil (clé `vigil`, bloc marqué), où ce risque n'existe pas.
$claudePattern = '*vigil-claude.exe*'
$codexPattern = '*vigil-codex.exe*'

# ── Claude Code : supprimer vigil-claude.exe de ~/.claude/settings.json ──────

$claudePath = [IO.Path]::Combine($env:USERPROFILE, '.claude', 'settings.json')
if (Test-Path $claudePath) {
    try {
        $content = Get-Content $claudePath -Raw -Encoding UTF8
        $json = $content | ConvertFrom-Json

        if ($json.hooks) {
            $claudeEvents = @(
                'SessionStart', 'SessionEnd',
                'PreToolUse', 'PostToolUse', 'PostToolUseFailure',
                'PermissionRequest', 'Notification',
                'Stop', 'StopFailure',
                'UserPromptSubmit', 'SubagentStart', 'SubagentStop'
            )

            $changed = $false
            foreach ($ev in $claudeEvents) {
                $groups = $json.hooks.$ev
                if ($null -eq $groups) { continue }

                $anyChanged = $false
                $filteredGroups = @($groups | ForEach-Object {
                    # Élément portant NOTRE commande à PLAT (orphelin de mauvaise forme sous un
                    # event Claude) : le filtre `hooks` ne peut pas le voir. Aligné sur le retrait
                    # du cœur pur (revue 10.1) — sinon l'orphelin survit à la désinstallation.
                    if ($_.command -like $claudePattern) {
                        $anyChanged = $true
                        return
                    }
                    $innerFiltered = @($_.hooks | Where-Object { $_.command -notlike $claudePattern })
                    if ($innerFiltered.Count -lt @($_.hooks).Count) {
                        $anyChanged = $true
                        if ($innerFiltered.Count -gt 0) {
                            $_.hooks = $innerFiltered
                            $_
                        }
                    } else {
                        $_
                    }
                })

                if ($anyChanged) {
                    $changed = $true
                    if ($filteredGroups.Count -eq 0) {
                        $json.hooks.PSObject.Properties.Remove($ev)
                    } else {
                        $json.hooks.$ev = $filteredGroups
                    }
                }
            }

            if ($changed) {
                $output = $json | ConvertTo-Json -Depth 20
                $tempPath = "$claudePath.tmp"
                [System.IO.File]::WriteAllText($tempPath, $output, [System.Text.UTF8Encoding]::new($false))
                Move-Item -Path $tempPath -Destination $claudePath -Force
            }
        }
    } catch {}
}

# ── Codex CLI : supprimer vigil-codex.exe de ~/.codex/hooks.json ─────────────

$codexPath = [IO.Path]::Combine($env:USERPROFILE, '.codex', 'hooks.json')
if (Test-Path $codexPath) {
    try {
        $content = Get-Content $codexPath -Raw -Encoding UTF8
        $json = $content | ConvertFrom-Json

        if ($json.hooks) {
            # ⚠️ Doit refléter `CODEX_EVENTS` (`src-tauri/src/config/mod.rs`) — source de vérité.
            # Les 3 derniers (Story 6.14) MANQUAIENT ici : le désinstalleur laissait donc 3 blocs
            # orphelins pointant un binaire supprimé, à chaque désinstallation (revue 10.1).
            $codexEvents = @(
                'SessionStart', 'Stop',
                'PreToolUse', 'PostToolUse',
                'PermissionRequest',
                'UserPromptSubmit', 'SubagentStart', 'SubagentStop'
            )

            $changed = $false
            foreach ($ev in $codexEvents) {
                $groups = $json.hooks.$ev
                if ($null -eq $groups) { continue }

                $anyChanged = $false
                $filteredGroups = @($groups | ForEach-Object {
                    # Orphelin Vigil à PLAT sous un event Codex, cf. section Claude ci-dessus.
                    if ($_.command -like $codexPattern) {
                        $anyChanged = $true
                        return
                    }
                    $innerFiltered = @($_.hooks | Where-Object { $_.command -notlike $codexPattern })
                    if ($innerFiltered.Count -lt @($_.hooks).Count) {
                        $anyChanged = $true
                        if ($innerFiltered.Count -gt 0) {
                            $_.hooks = $innerFiltered
                            $_
                        }
                    } else {
                        $_
                    }
                })

                if ($anyChanged) {
                    $changed = $true
                    if ($filteredGroups.Count -eq 0) {
                        $json.hooks.PSObject.Properties.Remove($ev)
                    } else {
                        $json.hooks.$ev = $filteredGroups
                    }
                }
            }

            if ($changed) {
                $output = $json | ConvertTo-Json -Depth 20
                $tempPath = "$codexPath.tmp"
                [System.IO.File]::WriteAllText($tempPath, $output, [System.Text.UTF8Encoding]::new($false))
                Move-Item -Path $tempPath -Destination $codexPath -Force
            }
        }
    } catch {}
}

# ── Grok Build : supprimer le fichier de hook DÉDIÉ ~/.grok/hooks/vigil.json ──
# Story 6.26 : Vigil possède ce fichier en propre (Grok globbe ~/.grok/hooks/*.json) → cleanup =
# suppression pure, aucun autre fichier de ~/.grok/hooks/ n'est touché.

$grokHookFile = [IO.Path]::Combine($env:USERPROFILE, '.grok', 'hooks', 'vigil.json')
if (Test-Path $grokHookFile) {
    try {
        Remove-Item -Path $grokHookFile -Force
    } catch {}
}

# ── Antigravity CLI : supprimer vigil-antigravity.exe de ~/.gemini/config/hooks.json ──
# Structure DIFFÉRENTE (Story 6.18) : top-level = nom de hook "vigil" → events ; Grouped
# (PreToolUse/PostToolUse : matcher + hooks[]) vs Flat (PreInvocation/Stop : handlers directs).
# Commande = "<exe> <event>".
# ⚠️ Le chemin est stocké en nom COURT 8.3 (Story 6.18, GetShortPathNameW pour `cmd /c` sans
# espaces) → ex. `…\vigil\VIGIL-~1.EXE`. Le match doit donc couvrir le nom court ET le nom long :
# on cible tout binaire Vigil (`*vigil*.exe*`, insensible à la casse). Sûr : on n'opère QUE sous la
# clé de hook "vigil" (namespace dédié à Vigil).

$antigravityPath = [IO.Path]::Combine($env:USERPROFILE, '.gemini', 'config', 'hooks.json')
if (Test-Path $antigravityPath) {
    try {
        $content = Get-Content $antigravityPath -Raw -Encoding UTF8
        $json = $content | ConvertFrom-Json

        $vigilHook = $json.vigil
        if ($vigilHook) {
            $groupedEvents = @('PreToolUse', 'PostToolUse')
            $flatEvents = @('PreInvocation', 'Stop')
            $changed = $false

            # Grouped : filtrer les handlers Vigil dans chaque groupe, puis les groupes vidés.
            foreach ($ev in $groupedEvents) {
                $groups = $vigilHook.$ev
                if ($null -eq $groups) { continue }

                $anyChanged = $false
                $filteredGroups = @($groups | ForEach-Object {
                    $innerFiltered = @($_.hooks | Where-Object { $_.command -notlike '*vigil*.exe*' })
                    if ($innerFiltered.Count -lt @($_.hooks).Count) {
                        $anyChanged = $true
                        if ($innerFiltered.Count -gt 0) {
                            $_.hooks = $innerFiltered
                            $_
                        }
                    } else {
                        $_
                    }
                })

                if ($anyChanged) {
                    $changed = $true
                    if ($filteredGroups.Count -eq 0) {
                        $vigilHook.PSObject.Properties.Remove($ev)
                    } else {
                        $vigilHook.$ev = $filteredGroups
                    }
                }
            }

            # Flat : filtrer directement les handlers Vigil.
            foreach ($ev in $flatEvents) {
                $handlers = $vigilHook.$ev
                if ($null -eq $handlers) { continue }

                $filtered = @($handlers | Where-Object { $_.command -notlike '*vigil*.exe*' })
                if ($filtered.Count -lt @($handlers).Count) {
                    $changed = $true
                    if ($filtered.Count -eq 0) {
                        $vigilHook.PSObject.Properties.Remove($ev)
                    } else {
                        $vigilHook.$ev = $filtered
                    }
                }
            }

            # Retirer la clé de hook "vigil" si elle ne contient plus aucun event.
            if (@($vigilHook.PSObject.Properties).Count -eq 0) {
                $json.PSObject.Properties.Remove('vigil')
                $changed = $true
            }

            if ($changed) {
                $output = $json | ConvertTo-Json -Depth 20
                $tempPath = "$antigravityPath.tmp"
                [System.IO.File]::WriteAllText($tempPath, $output, [System.Text.UTF8Encoding]::new($false))
                Move-Item -Path $tempPath -Destination $antigravityPath -Force
            }
        }
    } catch {}
}

# ── Kimi CLI : supprimer les blocs [[hooks]] Vigil de ~/.kimi-code/config.toml ────
# Story 9.2. Traitement TEXTUEL ligne à ligne : PowerShell n'a pas de parser TOML, et une
# réécriture depuis un modèle détruirait providers, alias de modèles et commentaires de
# l'utilisateur. Un bloc court de sa ligne `[[hooks]]` jusqu'à la prochaine ligne commençant par
# `[` (nouvelle table) — les lignes vides et commentaires en fin de span appartiennent au bloc
# SUIVANT, pas à celui-ci, sinon retirer un bloc Vigil emporterait le marqueur du bloc d'après.
#
# ⚠️ Le chemin est stocké en nom COURT 8.3 (`cmd /d /s /c` de Kimi, cf. `get_vigil_kimi_exe`) →
# `VIGIL-~1.EXE`, qui ne contient PAS la chaîne « kimi ». Le motif couvre donc nom court ET nom
# long, comme la section Antigravity ci-dessus. Il reste étroit (`vigil-~<chiffres>.exe`) pour ne
# jamais happer un binaire tiers de l'utilisateur.
#
# ⚠️ Ce script NE TOUCHE PAS `enabled-tools.json` : il tourne aussi au NSIS_HOOK_PREUNINSTALL
# d'une MISE À JOUR, et y nettoyer une clé ferait perdre l'activation à chaque update
# (décision Steven, 6.26). La désactivation est gérée par `uninstall_hooks_for_tool`.

$kimiPath = [IO.Path]::Combine($env:USERPROFILE, '.kimi-code', 'config.toml')
if (Test-Path $kimiPath) {
    try {
        $raw = [System.IO.File]::ReadAllText($kimiPath)
        # Préserve le style de fin de ligne du fichier (ne jamais convertir LF ↔ CRLF).
        $nl = if ($raw.Contains("`r`n")) { "`r`n" } else { "`n" }
        $lines = $raw -split "`r?`n"
        $drop = New-Object 'System.Collections.Generic.HashSet[int]'

        for ($i = 0; $i -lt $lines.Count; $i++) {
            if ($lines[$i].Trim() -ne '[[hooks]]') { continue }

            # Fin de span : prochaine entête de table.
            $end = $lines.Count
            for ($j = $i + 1; $j -lt $lines.Count; $j++) {
                if ($lines[$j].TrimStart().StartsWith('[')) { $end = $j; break }
            }
            # Recule sur les lignes vides / commentaires : elles préfixent le bloc SUIVANT.
            $last = $end - 1
            while ($last -gt $i -and ($lines[$last].Trim() -eq '' -or $lines[$last].TrimStart().StartsWith('#'))) {
                $last--
            }

            $isVigil = $false
            for ($k = $i + 1; $k -le $last; $k++) {
                if ($lines[$k] -match '^\s*command\s*=' -and $lines[$k] -match '(?i)(vigil-kimi\.exe|vigil-~\d+\.exe)') {
                    $isVigil = $true
                    break
                }
            }
            if (-not $isVigil) { continue }

            for ($k = $i; $k -le $last; $k++) { [void]$drop.Add($k) }

            # Le marqueur (et sa ligne vide) qui PRÉCÈDENT le bloc, tels que Vigil les écrit.
            # Absents si Kimi a déjà réécrit le fichier (`smol-toml` efface les commentaires) —
            # c'est pourquoi l'identité est la ligne `command`, jamais le marqueur.
            $p = $i - 1
            if ($p -ge 0 -and $lines[$p].Trim() -eq '# vigil: managed hook') {
                [void]$drop.Add($p)
                $p--
                if ($p -ge 0 -and $lines[$p].Trim() -eq '') { [void]$drop.Add($p) }
            }
        }

        # Aucun bloc retiré → le fichier n'est PAS réécrit.
        if ($drop.Count -gt 0) {
            $kept = @(for ($i = 0; $i -lt $lines.Count; $i++) { if (-not $drop.Contains($i)) { $lines[$i] } })
            $output = ($kept -join $nl)
            $tempPath = "$kimiPath.tmp"
            [System.IO.File]::WriteAllText($tempPath, $output, [System.Text.UTF8Encoding]::new($false))
            Move-Item -Path $tempPath -Destination $kimiPath -Force
        }
    } catch {}
}
