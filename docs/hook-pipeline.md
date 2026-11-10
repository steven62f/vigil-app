# Pipeline Hook → Vigil

## Vue d'ensemble

À chaque action d'un outil CLI compatible (Claude Code, Codex CLI, Grok Build), un événement est transmis à Vigil via une chaîne en trois étapes.

```
Outil CLI  →  vigil-<outil>.exe  →  Named Pipe  →  Vigil (Tauri)  →  UI React
```

Depuis l'Epic 6, **un binaire hook dédié existe par outil** :

| Outil CLI | Binaire | `source_tool` injecté |
|---|---|---|
| Claude Code | `vigil-claude.exe` | `claude_code` |
| Codex CLI | `vigil-codex.exe` | `codex` |
| Grok Build | `vigil-grok.exe` | `grok` |

Le code commun (lecture stdin, ouverture pipe, snapshot des processus, gestion du filtrage `enabled-tools.json`, etc.) vit dans la crate lib **`vigil-hook-core`**. Chaque binaire ne contient que sa constante `SOURCE_TOOL` + son flow `main()` (typiquement ~50 lignes).

---

## Étape 1 — L'outil CLI déclenche le hook

L'outil est configuré dans son propre fichier de config pour exécuter SON binaire dédié :

- Claude Code → `~/.claude/settings.json` → `vigil-claude.exe`
- Codex CLI → (story 6.3) → `vigil-codex.exe`
- Grok Build → (story 6.3) → `vigil-grok.exe`

La couverture **diffère par outil** (source de vérité : `CLAUDE_EVENTS` / `CODEX_EVENTS` / `GROK_EVENTS` dans `src-tauri/src/config/mod.rs`). Claude Code couvre la liste complète ; Codex et Grok un sous-ensemble.

| Événement | Quand | Claude Code | Codex | Grok |
|---|---|:---:|:---:|:---:|
| `SessionStart` | Démarrage ou reprise d'une session | ✅ | ✅ | ✅ |
| `SessionEnd` | Fin de session (`/clear`, `/resume`, fermeture) | ✅ | ❌ | ❌ |
| `PreToolUse` | Juste avant l'exécution d'un outil | ✅ | ✅ | ✅ |
| `PostToolUse` | Après l'exécution réussie d'un outil | ✅ | ✅ | ✅ |
| `PostToolUseFailure` | Après l'échec d'un outil | ✅ | ❌ | ❌ |
| `PermissionRequest` | Quand l'outil demande une permission à l'utilisateur | ✅ | ✅ | ❌ |
| `Notification` | Notification système de l'outil | ✅ | ❌ | ❌ |
| `Stop` | L'outil a fini de répondre | ✅ | ✅ | ✅ |
| `StopFailure` | Fin de tour suite à une erreur API | ✅ | ❌ | ❌ |
| `UserPromptSubmit` | L'utilisateur soumet un prompt | ✅ | ❌ | ❌ |
| `SubagentStart` / `SubagentStop` | Cycle de vie d'un sous-agent | ✅ | ❌ | ❌ |

> **Note Codex :** pas de `SessionEnd` — la fin de session est détectée via le `PidWatcher` (mort du terminal) ou le `Stop`. Pas de sous-agents, ni de notifications. `PermissionRequest` est le seul hook bidirectionnel (bloquant). `PostToolUse` est en pratique limité à Bash.

L'outil passe le détail de l'événement en JSON sur le **stdin** du binaire hook.

---

## Étape 2 — vigil-<outil>.exe (processus éphémère)

**Point clé : ces binaires ne sont PAS des daemons.** Ils sont spawnés par l'outil CLI, font leur travail, et meurent. Durée de vie typique : < 100ms.

Leur rôle (générique) :

1. Lire le JSON de l'événement depuis `stdin`
2. **Filtrage (story 6.1)** : lire `%APPDATA%\Vigil\enabled-tools.json` et exit 0 silencieux si l'outil est explicitement désactivé (`false`). Fail-open si le fichier est absent, vide, ou corrompu.
3. Remonter la chaîne de processus parents pour trouver le process de l'outil (`claude.exe`, `codex.exe`, `grok.exe`…), puis retourner son parent (le terminal : `pwsh.exe`, `cmd.exe`, `WindowsTerminal.exe`…) — ce PID est injecté dans le JSON sous le champ `parent_pid`
4. Injecter le champ `source_tool` (`"claude_code"`, `"codex"`, `"grok"`) — connu par construction, pas de détection runtime
5. Ouvrir `\\.\pipe\vigil` (connexion one-shot vers Vigil)
6. Envoyer le JSON enrichi + `\n`
7. **Cas `PermissionRequest`** :
   - `vigil-claude` relaie la réponse Vigil (format Claude Code) telle quelle sur stdout
   - `vigil-codex` réémet la réponse au format `hookSpecificOutput.decision.behavior` attendu par Codex — **identique** à l'enveloppe Claude Code (`{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow|deny"}}}`, `message` optionnel pour `deny`). Codex partage exactement ce format ; l'ancien `{"decision":"allow","reason":"..."}` était silencieusement ignoré (bug corrigé story 6.7).
   - `vigil-grok` n'attend AUCUNE réponse (Grok ne supporte pas `PermissionRequest`)
8. Exit

Le `parent_pid` injecté permet à Vigil de surveiller la vie du terminal. Si le terminal est tué brutalement (sans hook `SessionEnd`), Vigil le détecte via le `PidWatcher`.

---

## Étape 3 — Vigil reçoit et traite l'événement

**PipeManager** tourne en boucle dans Vigil et maintient un pool de 20 instances du Named Pipe `\\.\pipe\vigil`. Chaque connexion entrante est traitée dans sa propre tâche tokio asynchrone.

Pour chaque connexion :

1. Lire le JSON → parser en `HookEvent` typé (les champs non listés dans les structs sont ignorés par `serde` — c'est ce qui permet à `source_tool` d'être présent sans casser le parsing tant que la story 6.2 ne l'a pas câblé)
2. Appeler `AgentStateEngine::apply_event()` → mettre à jour l'état de l'agent
3. Émettre l'événement Tauri `agents:snapshot` avec la liste complète des agents
4. **Cas `PermissionRequest`** : stocker un canal `oneshot` dans `PendingApprovals` et attendre qu'`approve_agent` ou `deny_agent` (commandes Tauri appelées par l'UI) envoie la réponse. Timeout 120s → deny automatique.

**AgentStateEngine** maintient une `HashMap<transcript_path, AgentSnapshot>`. Le `transcript_path` (chemin du fichier `.jsonl` de la session) est la **clé unique** de chaque agent.

---

## Pourquoi un pool de 20 instances ?

Parce que les binaires hook sont one-shot : ils ouvrent une connexion, envoient, et ferment. Si 3 hooks se déclenchent quasi-simultanément (ex. `PostToolUse` + `Notification` + `PermissionRequest`), il faut 3 instances pipe disponibles en même temps. Avec 20 instances, on couvre largement le cas de plusieurs agents actifs en parallèle.

---

## Détection de mort brutale (PidWatcher)

Si l'utilisateur ferme directement le terminal (sans `/clear` ou `/exit`), aucun hook `SessionEnd` n'est envoyé. Vigil gère ce cas via le **PidWatcher** :

- Toutes les 2 secondes, pour chaque agent actif ayant un `terminal_pid` connu
- Appel `OpenProcess(terminal_pid)` pour vérifier si le processus est vivant
- `ERROR_INVALID_PARAMETER` (0x80070057) → PID inexistant → terminal mort → `set_disconnected()`
- `ERROR_ACCESS_DENIED` ou succès → terminal vivant → rien à faire
