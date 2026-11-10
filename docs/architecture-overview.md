# Architecture Vigil — Vue d'ensemble

## La structure des dossiers

**`src/`** — Le code **frontend** (ce que l'utilisateur voit). React, TypeScript, Tailwind. C'est Vite qui le compile en HTML/CSS/JS dans `dist/`.

**`src-tauri/`** — Le code **backend natif** écrit en Rust. C'est lui qui crée les fenêtres Windows, accède au système de fichiers, aux Named Pipes, etc. Tauri fait le pont entre ce code Rust et ton frontend React.

**`target/`** — Le dossier de **sortie de la compilation Rust** (comme `node_modules` mais pour Cargo). Il est généré automatiquement, ne jamais le commiter. Le `.exe` dans `target/debug/` c'est le binaire de ton app Tauri.

**`vigil-hook-core/`, `vigil-claude/`, `vigil-codex/`, `vigil-grok/`** — Depuis l'Epic 6, **un binaire hook dédié par outil CLI** (Claude Code → `vigil-claude.exe`, Codex CLI → `vigil-codex.exe`, Grok Build → `vigil-grok.exe`). Tout le code commun (lecture stdin, ouverture pipe, filtrage `enabled-tools.json`, snapshot des process…) vit dans la crate lib `vigil-hook-core/`. Chaque binaire est un petit programme spawné par son outil CLI à chaque événement, qui pousse les événements vers Vigil via Named Pipe puis meurt. Séparés de `src-tauri` car ils s'installent dans l'environnement de chaque outil, pas dans Vigil.

---

## Rust et Cargo

**Rust** est le langage de programmation. **Cargo** est son gestionnaire de projets/dépendances — l'équivalent de `npm` pour Rust. Là où npm lit `package.json`, Cargo lit `Cargo.toml`.

Une **crate** = un projet/package Rust (comme un package npm). Le projet a un **workspace** (le `Cargo.toml` racine) qui regroupe cinq crates : `src-tauri` (l'app Tauri), `vigil-hook-core` (lib du socle commun) et les trois binaires hook `vigil-claude`, `vigil-codex`, `vigil-grok`. Ça permet de les compiler ensemble.

**Rustup** = l'installateur de Rust lui-même (comme `nvm` pour Node).

---

## `npm run dev` vs `npm run tauri dev`

| | `npm run dev` | `npm run tauri dev` |
|---|---|---|
| Lance | Vite uniquement | Vite **+** compilation Rust **+** fenêtres Tauri |
| Résultat | Page web dans le navigateur | Vraie application Windows native |
| Utile pour | Déboguer le CSS/React rapidement | Tester l'app complète avec IPC, fenêtres, etc. |

`npm run dev` est plus rapide (pas de compilation Rust) mais tu n'as pas les fenêtres natives ni l'accès aux APIs système. Pour Vigil, tu utiliseras presque toujours `tauri dev`.
