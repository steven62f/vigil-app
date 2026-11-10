# Claude Code — Référence complète des Hooks

> Source : documentation officielle [code.claude.com/docs/en/hooks](https://code.claude.com/docs/en/hooks) (consultée le 1er mai 2026)

Les hooks sont des commandes shell, endpoints HTTP, prompts LLM ou agents qui s'exécutent automatiquement à des points précis du cycle de vie d'une session Claude Code. Pour les hooks de type `command` et `http`, le payload arrive en JSON sur **stdin** (command) ou comme **body POST** (http).

---

## Champs communs à TOUS les hooks (payload stdin)

Ces champs sont présents dans le payload JSON de chaque hook, en plus des champs spécifiques à l'événement.

| Champ | Description | Valeurs possibles |
|---|---|---|
| `session_id` | Identifiant unique de la session courante | string |
| `transcript_path` | Chemin absolu vers le fichier JSONL contenant le transcript de la conversation | string (path) |
| `cwd` | Répertoire de travail courant au moment où le hook est invoqué | string (path) |
| `hook_event_name` | Nom de l'événement qui a déclenché le hook | Nom exact de l'événement (ex. `"PreToolUse"`) |
| `permission_mode` | Mode de permission actif. **N'est pas présent sur tous les events** (cf. exemples par event) | `"default"`, `"plan"`, `"acceptEdits"`, `"auto"`, `"dontAsk"`, `"bypassPermissions"` |

### Champs additionnels en contexte subagent

Présents uniquement si le hook fire dans un subagent ou avec `--agent` :

| Champ | Description |
|---|---|
| `agent_id` | Identifiant unique du subagent. Présent uniquement quand le hook fire à l'intérieur d'un subagent |
| `agent_type` | Nom de l'agent (ex. `"Explore"`, `"security-reviewer"`). Le type du subagent prime sur la valeur `--agent` de la session |

---

# Listing des Hooks (par ordre du cycle de vie)

## 1. `SessionStart`

**Trigger** : début de session ou reprise d'une session existante. Utile pour charger du contexte (issues, branche git, etc.) ou poser des variables d'environnement.

**Matcher** : filtre sur le mode de démarrage.
- `startup` → nouvelle session
- `resume` → `--resume`, `--continue`, ou `/resume`
- `clear` → après `/clear`
- `compact` → après une compaction (auto ou manuelle)

> Seuls les types `command` et `mcp_tool` sont supportés.

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `source` | Comment la session a démarré. Valeurs : `"startup"`, `"resume"`, `"clear"`, `"compact"` |
| `model` | Identifiant du modèle utilisé (ex. `"claude-sonnet-4-6"`) |
| `agent_type` | Optionnel. Présent si `claude --agent <name>` |

> ⚠ Pas de `permission_mode` ici.

---

## 2. `Setup`

**Trigger** : seulement quand Claude Code est lancé avec `--init-only`, ou avec `--init` / `--maintenance` en mode `-p` (print). **Ne fire pas au démarrage normal**. Pour de l'init par-session, utiliser `SessionStart`.

**Matcher** : flag CLI déclenchant le hook.
- `init` → `claude --init-only` ou `claude -p --init`
- `maintenance` → `claude -p --maintenance`

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `trigger` | Flag déclencheur. Valeurs : `"init"` ou `"maintenance"` |

> ⚠ Pas de `permission_mode`. Accès à `CLAUDE_ENV_FILE`.

---

## 3. `InstructionsLoaded`

**Trigger** : quand un fichier `CLAUDE.md` ou `.claude/rules/*.md` est chargé en contexte. Fire au session start (chargement eager) puis lors des chargements lazy (sous-répertoires, règles conditionnelles avec `paths:` frontmatter).

**Matcher** : filtre sur `load_reason`.
- `session_start`
- `nested_traversal`
- `path_glob_match`
- `include`
- `compact` (rechargement après compaction)

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `file_path` | Chemin absolu du fichier d'instructions chargé |
| `memory_type` | Scope du fichier. Valeurs : `"User"`, `"Project"`, `"Local"`, `"Managed"` |
| `load_reason` | Raison du chargement. Valeurs : `"session_start"`, `"nested_traversal"`, `"path_glob_match"`, `"include"`, `"compact"` |
| `globs` | Patterns glob du frontmatter `paths:`. Présent uniquement pour `path_glob_match` |
| `trigger_file_path` | Chemin du fichier dont l'accès a déclenché ce chargement (chargements lazy) |
| `parent_file_path` | Chemin du fichier parent (chargements via `include`) |

> ⚠ Aucun decision control. Pas de blocage possible. Pour observabilité/audit uniquement.

---

## 4. `UserPromptSubmit`

**Trigger** : quand l'utilisateur soumet un prompt, **avant** que Claude le traite.

**Matcher** : aucun (fire toujours).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `prompt` | Texte exact soumis par l'utilisateur |

---

## 5. `UserPromptExpansion`

**Trigger** : quand une slash command tapée par l'utilisateur est expansée en prompt, **avant** d'atteindre Claude. Couvre le chemin que `PreToolUse` ne couvre pas (un `/skillname` tapé directement bypass `PreToolUse`).

**Matcher** : nom de la commande (ex. `"deploy"` ou nom d'un skill).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `expansion_type` | Type d'expansion. Valeurs : `"slash_command"` (skill/custom command), `"mcp_prompt"` (MCP server prompt) |
| `command_name` | Nom de la commande invoquée |
| `command_args` | Arguments fournis à la commande (string) |
| `command_source` | Origine de la commande. Valeurs courantes : `"plugin"`, `"user"`, `"project"`, `"builtin"` |
| `prompt` | Le prompt complet tapé (incluant le `/`) |

---

## 6. `PreToolUse`

**Trigger** : après que Claude ait construit les paramètres d'un tool call, **avant** son exécution. Peut bloquer/modifier l'appel.

**Matcher** : nom du tool. Tools natifs : `Bash`, `Edit`, `Write`, `Read`, `Glob`, `Grep`, `Agent`, `WebFetch`, `WebSearch`, `AskUserQuestion`, `ExitPlanMode`. + tools MCP (pattern `mcp__<server>__<tool>`).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `tool_name` | Nom du tool sur le point d'être appelé |
| `tool_input` | Objet contenant les paramètres du tool. **Le schéma dépend du tool** (cf. ci-dessous) |
| `tool_use_id` | Identifiant unique du tool call (corrèle avec `PostToolUse`) |

### Schéma de `tool_input` selon le tool

#### `Bash`
| Champ | Type | Description |
|---|---|---|
| `command` | string | Commande shell à exécuter |
| `description` | string | Description optionnelle |
| `timeout` | number | Timeout optionnel (ms) |
| `run_in_background` | boolean | Exécution en arrière-plan |

#### `Write`
| Champ | Type | Description |
|---|---|---|
| `file_path` | string | Chemin absolu |
| `content` | string | Contenu à écrire |

#### `Edit`
| Champ | Type | Description |
|---|---|---|
| `file_path` | string | Chemin absolu |
| `old_string` | string | Texte à remplacer |
| `new_string` | string | Texte de remplacement |
| `replace_all` | boolean | Remplacer toutes les occurrences |

#### `Read`
| Champ | Type | Description |
|---|---|---|
| `file_path` | string | Chemin absolu |
| `offset` | number | Ligne de départ optionnelle |
| `limit` | number | Nombre de lignes à lire |

#### `Glob`
| Champ | Type | Description |
|---|---|---|
| `pattern` | string | Pattern glob |
| `path` | string | Répertoire de recherche optionnel |

#### `Grep`
| Champ | Type | Description |
|---|---|---|
| `pattern` | string | Regex à rechercher |
| `path` | string | Fichier ou répertoire optionnel |
| `glob` | string | Filtre glob optionnel |
| `output_mode` | string | Valeurs : `"content"`, `"files_with_matches"` (défaut), `"count"` |
| `-i` | boolean | Insensible à la casse |
| `multiline` | boolean | Mode multiligne |

#### `WebFetch`
| Champ | Type | Description |
|---|---|---|
| `url` | string | URL à fetcher |
| `prompt` | string | Prompt à exécuter sur le contenu |

#### `WebSearch`
| Champ | Type | Description |
|---|---|---|
| `query` | string | Requête de recherche |
| `allowed_domains` | array | Domaines autorisés |
| `blocked_domains` | array | Domaines bloqués |

#### `Agent`
| Champ | Type | Description |
|---|---|---|
| `prompt` | string | Tâche pour le subagent |
| `description` | string | Courte description |
| `subagent_type` | string | Type d'agent (ex. `"Explore"`) |
| `model` | string | Alias modèle optionnel |

#### `AskUserQuestion`
| Champ | Type | Description |
|---|---|---|
| `questions` | array | Liste de 1 à 4 questions à choix multiples avec `question`, `header`, `options`, `multiSelect` |
| `answers` | object | Optionnel. Map question→label sélectionné. Fourni via `updatedInput` pour répondre programmatiquement |

---

## 7. `PermissionRequest`

**Trigger** : quand un dialogue de permission est sur le point d'être affiché à l'utilisateur. Fire **après** `PreToolUse`, uniquement si une permission est demandée.

**Matcher** : nom du tool (mêmes valeurs que `PreToolUse`).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `tool_name` | Nom du tool concerné |
| `tool_input` | Paramètres du tool (même schéma que `PreToolUse`) |
| `permission_suggestions` | Tableau optionnel des options "always allow" qui seraient présentées à l'utilisateur. Chaque entrée a `type`, `rules`, `behavior`, `destination` |

> ⚠ Pas de `tool_use_id` ici (à la différence de `PreToolUse`).

---

## 8. `PermissionDenied`

**Trigger** : quand le classifier en mode auto refuse un tool call. **Fire uniquement en mode auto** : pas pour les refus manuels d'un dialogue, ni pour les blocages via `PreToolUse`, ni pour les deny rules.

**Matcher** : nom du tool (mêmes valeurs que `PreToolUse`).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `tool_name` | Nom du tool dont l'appel a été refusé |
| `tool_input` | Paramètres du tool |
| `tool_use_id` | Identifiant du tool call |
| `reason` | Explication du classifier sur la raison du refus |

---

## 9. `PostToolUse`

**Trigger** : immédiatement après qu'un tool ait terminé avec **succès**.

**Matcher** : nom du tool (mêmes valeurs que `PreToolUse`).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `tool_name` | Nom du tool exécuté |
| `tool_input` | Arguments envoyés au tool |
| `tool_response` | Résultat retourné par le tool. **Le schéma dépend du tool** (objet structuré, ex. `{filePath, success}` pour `Write`, `{stdout, stderr, interrupted, isImage}` pour `Bash`) |
| `tool_use_id` | Identifiant du tool call (corrèle avec `PreToolUse`) |
| `duration_ms` | Optionnel (v2.1.119+). Durée d'exécution en ms (exclut les permission prompts et les hooks `PreToolUse`) |

---

## 10. `PostToolUseFailure`

**Trigger** : quand un tool **échoue** (exception ou résultat d'échec).

**Matcher** : nom du tool (mêmes valeurs que `PreToolUse`).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `tool_name` | Nom du tool ayant échoué |
| `tool_input` | Arguments du tool |
| `tool_use_id` | Identifiant du tool call |
| `error` | Description du problème (string) |
| `is_interrupt` | Boolean optionnel. `true` si l'échec vient d'une interruption utilisateur |
| `duration_ms` | Optionnel (v2.1.119+). Durée d'exécution en ms |

---

## 11. `PostToolBatch`

**Trigger** : une fois après que **tous les tool calls d'un batch** soient résolus, avant le prochain appel modèle. Idéal pour injecter du contexte qui dépend du **set** de tools, pas d'un seul.

**Matcher** : aucun (fire toujours).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `tool_calls` | Tableau décrivant chaque tool call du batch. Chaque entrée contient `tool_name`, `tool_input`, `tool_use_id`, `tool_response` |

> ⚠ `tool_response` dans `PostToolBatch` est le **string sérialisé** (tel que vu par le modèle dans le bloc `tool_result`), pas l'objet structuré comme dans `PostToolUse`. Pour `Read`, c'est le texte avec préfixes de numéros de ligne.

---

## 12. `Notification`

**Trigger** : quand Claude Code envoie une notification.

**Matcher** : type de notification.
- `permission_prompt`
- `idle_prompt`
- `auth_success`
- `elicitation_dialog`
- `elicitation_complete`
- `elicitation_response`

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `message` | Texte de la notification |
| `title` | Titre optionnel |
| `notification_type` | Type qui a fire (mêmes valeurs que le matcher) |

> ⚠ Pas de `permission_mode`. Aucun decision control (effets de bord uniquement).

---

## 13. `SubagentStart`

**Trigger** : quand un subagent est spawné via le tool `Agent`.

**Matcher** : type d'agent. Valeurs natives : `general-purpose`, `Explore`, `Plan`. + noms d'agents custom (`.claude/agents/`).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `agent_id` | Identifiant unique du subagent |
| `agent_type` | Nom de l'agent (built-in ou custom) |

> ⚠ Pas de `permission_mode`. Ne peut pas bloquer la création.

---

## 14. `SubagentStop`

**Trigger** : quand un subagent termine de répondre.

**Matcher** : type d'agent (mêmes valeurs que `SubagentStart`).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `stop_hook_active` | Boolean. `true` si Claude Code continue déjà suite à un stop hook (utiliser pour éviter une boucle infinie) |
| `agent_id` | Identifiant du subagent |
| `agent_type` | Type de l'agent (utilisé pour le matcher) |
| `agent_transcript_path` | Transcript propre du subagent (dans un dossier `subagents/` imbriqué) |
| `last_assistant_message` | Texte de la réponse finale du subagent (évite de parser le transcript) |

> Note : `transcript_path` (champ commun) ici = transcript de la session principale, distinct de `agent_transcript_path`.

---

## 15. `TaskCreated`

**Trigger** : quand une tâche est en train d'être créée via le tool `TaskCreate`. Permet d'enforcer des conventions de nommage, d'exiger des descriptions, etc.

**Matcher** : aucun (fire toujours).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `task_id` | Identifiant de la tâche |
| `task_subject` | Titre de la tâche |
| `task_description` | Description détaillée. Peut être absent |
| `teammate_name` | Nom du teammate qui crée la tâche. Peut être absent |
| `team_name` | Nom de l'équipe. Peut être absent |

---

## 16. `TaskCompleted`

**Trigger** : quand une tâche est sur le point d'être marquée comme complétée. Fire dans 2 cas : (1) un agent marque explicitement via `TaskUpdate`, (2) un teammate d'agent team finit son tour avec des tâches in-progress.

**Matcher** : aucun (fire toujours).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `task_id` | Identifiant de la tâche |
| `task_subject` | Titre de la tâche |
| `task_description` | Description détaillée. Peut être absent |
| `teammate_name` | Nom du teammate. Peut être absent |
| `team_name` | Nom de l'équipe. Peut être absent |

---

## 17. `Stop`

**Trigger** : quand l'agent principal Claude Code a fini de répondre. **Ne fire pas** sur interruption utilisateur (les erreurs API déclenchent `StopFailure`).

**Matcher** : aucun (fire toujours).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `stop_hook_active` | Boolean. `true` si Claude Code continue déjà à cause d'un précédent stop hook (à utiliser pour éviter les boucles infinies) |
| `last_assistant_message` | Texte de la réponse finale de Claude |

---

## 18. `StopFailure`

**Trigger** : à la place de `Stop` quand le tour se termine sur une **erreur API**. Output et exit code sont **ignorés**.

**Matcher** : type d'erreur.
- `rate_limit`
- `authentication_failed`
- `oauth_org_not_allowed`
- `billing_error`
- `invalid_request`
- `server_error`
- `max_output_tokens`
- `unknown`

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `error` | Type d'erreur (mêmes valeurs que le matcher) |
| `error_details` | Détails additionnels quand disponibles |
| `last_assistant_message` | Le texte d'erreur affiché dans la conversation (ex. `"API Error: Rate limit reached"`). À la différence de `Stop`, ce champ contient l'erreur API, pas la réponse de Claude |

> ⚠ Pas de `permission_mode`. Aucun decision control.

---

## 19. `TeammateIdle`

**Trigger** : quand un teammate d'agent team est sur le point de passer en idle après son tour. Permet d'enforcer des quality gates avant qu'un teammate ne s'arrête.

**Matcher** : aucun (fire toujours).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `teammate_name` | Nom du teammate qui va passer en idle |
| `team_name` | Nom de l'équipe |

---

## 20. `ConfigChange`

**Trigger** : quand un fichier de configuration change pendant la session.

**Matcher** : source de configuration.
- `user_settings`
- `project_settings`
- `local_settings`
- `policy_settings`
- `skills`

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `source` | Source de configuration (mêmes valeurs que le matcher) |
| `file_path` | Chemin du fichier modifié |

> Peut bloquer la modification (sauf `policy_settings`).

---

## 21. `CwdChanged`

**Trigger** : quand le working directory change (ex. `cd` exécuté par Claude). Utile pour environment management réactif (direnv).

**Matcher** : aucun (fire toujours).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `old_cwd` | Ancien répertoire |
| `new_cwd` | Nouveau répertoire |

> Accès à `CLAUDE_ENV_FILE`. Aucun decision control. Peut retourner `watchPaths` dans le JSON output pour configurer dynamiquement les paths surveillés par `FileChanged`.

---

## 22. `FileChanged`

**Trigger** : quand un fichier surveillé change sur le disque (détection native via FSEvents/inotify, pas de polling).

**Matcher** : sert deux rôles particuliers :
1. **Liste de fichiers à surveiller** : la valeur est splittée sur `|`, chaque segment devient un nom de fichier littéral à surveiller dans le cwd. Ex. `".envrc|.env"` surveille exactement ces 2 fichiers.
2. **Filtre des hooks à exécuter** : quand un fichier surveillé change, la même valeur filtre quels groupes de hooks sont exécutés (matching standard sur le basename).

> Les regex ne sont **pas** utiles ici (un matcher `^\.env` surveillerait un fichier nommé littéralement `^\.env`).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `file_path` | Chemin du fichier modifié |
| `change_type` | Type de modification. Valeurs typiques : `"created"`, `"modified"`, `"deleted"` |

> Accès à `CLAUDE_ENV_FILE`. Aucun decision control.

---

## 23. `WorktreeCreate`

**Trigger** : quand un worktree est en train d'être créé via `--worktree` ou `isolation: "worktree"`. **Remplace le comportement git par défaut** (utile pour SVN, Perforce, Mercurial). Note : `.worktreeinclude` n'est pas traité quand un hook est configuré.

**Matcher** : aucun (fire toujours).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `source_path` | Chemin source à partir duquel créer le worktree |
| `target_path` | Chemin cible suggéré pour le worktree |
| `branch` | Nom de branche (le cas échéant) |

> ⚠ Le hook **doit** retourner le path absolu du worktree créé : commande hook → sur stdout, http hook → via `hookSpecificOutput.worktreePath`. Toute exit code non-zéro fait échouer la création.

---

## 24. `WorktreeRemove`

**Trigger** : quand un worktree est en train d'être supprimé (en fin de session ou quand un subagent termine).

**Matcher** : aucun (fire toujours).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `worktree_path` | Chemin du worktree supprimé |

> Aucun decision control. Les échecs sont loggés en debug mode uniquement.

---

## 25. `PreCompact`

**Trigger** : avant la compaction du contexte.

**Matcher** : déclencheur de la compaction.
- `manual` (commande `/compact`)
- `auto` (compaction automatique)

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `trigger` | `"manual"` ou `"auto"` |
| `custom_instructions` | Instructions custom passées à `/compact` (présent uniquement pour `manual`) |

> Peut bloquer la compaction.

---

## 26. `PostCompact`

**Trigger** : après que la compaction du contexte soit terminée.

**Matcher** : `manual` ou `auto` (mêmes valeurs que `PreCompact`).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `trigger` | `"manual"` ou `"auto"` |

> Aucun decision control.

---

## 27. `SessionEnd`

**Trigger** : quand une session se termine.

**Matcher** : raison de la fin.
- `clear`
- `resume`
- `logout`
- `prompt_input_exit`
- `bypass_permissions_disabled`
- `other`

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `reason` | Raison de fin (mêmes valeurs que le matcher) |

> Aucun decision control. Pas de blocage de la fin de session.

---

## 28. `Elicitation`

**Trigger** : quand un MCP server demande une saisie utilisateur pendant un tool call.

**Matcher** : nom du serveur MCP.

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `server_name` | Nom du serveur MCP qui fait la demande |
| `message` | Message présenté à l'utilisateur |
| `requestedSchema` | Schéma JSON des champs demandés |

> Decision control via `hookSpecificOutput.action` : `"accept"` (avec `content`), `"decline"`, `"cancel"`.

---

## 29. `ElicitationResult`

**Trigger** : après que l'utilisateur ait répondu à une elicitation MCP, **avant** que la réponse soit renvoyée au serveur. Permet de valider/modifier la réponse.

**Matcher** : nom du serveur MCP (mêmes valeurs que `Elicitation`).

**Champs spécifiques du payload** :

| Champ | Description |
|---|---|
| `server_name` | Nom du serveur MCP |
| `action` | Action de l'utilisateur. Valeurs : `"accept"`, `"decline"`, `"cancel"` |
| `content` | Valeurs des champs du formulaire (présent si `action` = `"accept"`) |

> Decision control via `hookSpecificOutput.action` (peut override) et `content` (peut modifier les valeurs).

---

# Résumé : capacité de blocage par hook (exit code 2)

| Bloque | Hooks |
|---|---|
| **Oui** | `PreToolUse`, `PermissionRequest`, `UserPromptSubmit`, `UserPromptExpansion`, `Stop`, `SubagentStop`, `TeammateIdle`, `TaskCreated`, `TaskCompleted`, `ConfigChange` (sauf `policy_settings`), `PostToolBatch`, `PreCompact`, `Elicitation`, `ElicitationResult`, `WorktreeCreate` (any non-zero) |
| **Non** | `SessionStart`, `Setup`, `InstructionsLoaded`, `PostToolUse`, `PostToolUseFailure`, `PermissionDenied`, `Notification`, `SubagentStart`, `StopFailure`, `CwdChanged`, `FileChanged`, `WorktreeRemove`, `PostCompact`, `SessionEnd` |

# Variables d'environnement disponibles dans les hooks

| Variable | Description | Hooks supportés |
|---|---|---|
| `$CLAUDE_PROJECT_DIR` | Racine du projet | Tous |
| `${CLAUDE_PLUGIN_ROOT}` | Répertoire d'installation du plugin (change à chaque update) | Tous (hooks de plugin) |
| `${CLAUDE_PLUGIN_DATA}` | Répertoire de données persistantes du plugin | Tous (hooks de plugin) |
| `$CLAUDE_CODE_REMOTE` | `"true"` en environnement web, non set en CLI local | Tous |
| `$CLAUDE_ENV_FILE` | Fichier pour persister des `export VAR=value` dans les Bash suivants | `SessionStart`, `Setup`, `CwdChanged`, `FileChanged` |
