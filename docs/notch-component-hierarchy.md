# Notch — Hiérarchie des composants

## Arborescence

```
tauri.conf.json
└── fenêtre "notch"  (always-on-top, sans décorations, 300×48px, resizable=false)
    └── notch.html   (<div id="root"> + <script src="main.tsx">)
        └── main.tsx  (ReactDOM.createRoot → monte <NotchApp> + importe globals.css)
            └── NotchApp.tsx
                ├── useTauriEvents()         ← écoute agents:snapshot depuis Rust
                └── <NotchContainer agents={agents} />
                    └── (pour chaque agent, ou le placeholder)
                        └── <AgentCell agent={...} showStateLabel={true|false} />
                            ├── useSettings()            ← settings temps réel
                            ├── <MascotPixel state={agent.state} size={28} />
                            ├── <span> agent.name </span>
                            └── <span> WORKING / WAITING / IDLE / ... </span>
```

## Qui fait quoi ?

**`tauri.conf.json`** — déclare la fenêtre OS. C'est là que sont fixées les propriétés natives : `alwaysOnTop: true`, `decorations: false` (pas de barre de titre), dimensions initiales 300×48, `skipTaskbar: true`, `resizable: false` (mais `invoke("resize_notch")` programmatique reste possible).

**`notch.html`** — simple shell HTML. Son seul rôle est d'avoir un `<div id="root">` et d'importer `main.tsx`.

**`main.tsx`** — point d'entrée React. Monte l'arbre dans le `<div id="root">` et importe `globals.css` (tokens CSS : couleurs, polices, animations).

**`NotchApp.tsx`** — composant racine de l'application notch. Deux responsabilités :
1. Appelle `useTauriEvents()` qui écoute l'event `agents:snapshot` émis par le backend Rust et met à jour le store Zustand.
2. Lit `agents` depuis le store et les passe à `NotchContainer`.

**`useTauriEvents` (hook)** — la passerelle Rust → React. À chaque changement d'état d'un agent (session start, tool use, permission request, etc.), Rust émet un snapshot complet du tableau d'agents. Le hook reçoit ce snapshot et appelle `setAgents()`.

**`NotchContainer.tsx`** — la barre physique visible. Trois rôles :
- **Style visuel** : fond `--bg-notch` (`#080a0c`), `border-radius: 0 0 14px 14px`, hauteur 48px.
- **Drag natif** : `data-tauri-drag-region` → Windows interprète les clics sur cette zone comme un clic sur la barre de titre → drag natif sans plugin.
- **Redimensionnement dynamique** : un `ResizeObserver` mesure la largeur DOM réelle du contenu (`innerRef`) et appelle `invoke("resize_notch", { width })` avec `PADDING_H = 32` de marge. La largeur s'adapte automatiquement à la longueur réelle des noms de session, sans formule hardcodée.

**`AgentCell.tsx`** — une cellule par agent. Affiche la mascotte animée, le nom de session (tronqué à 80px avec ellipsis), et le label d'état. Gère le hover et le clic (`focus_terminal` si `quickTerminalAccess` est activé dans les settings). Utilise `onMouseDown stopPropagation` pour éviter que `data-tauri-drag-region` intercepte les clics.

**`useSettings` (hook)** — charge les settings au mount depuis `vigil-settings.json`, puis écoute l'event Tauri `vigil:settings-changed` pour se mettre à jour immédiatement quand l'utilisateur modifie quelque chose dans la fenêtre Settings. Toute modification est répercutée sans redémarrage.

**`MascotPixel.tsx`** — le petit personnage pixel art SVG. Reçoit un `AgentState` et joue l'animation correspondante (marche pour `working`, bras levé pour `waiting`, zZZ pour `idle`, etc.). Gère la transition douce entre états (attend la fin du cycle d'animation avant de changer).

---

## Ce que le Notch affiche concrètement

**Cas 1 — Aucun agent actif :**
Une barre sombre de 160px de large avec une cellule placeholder : mascotte `idle` grisée à 45% d'opacité avec des zZZ flottants + texte `"Waiting..."` sans label d'état.

**Cas 2 — N agents actifs :**
La barre s'étire pour accueillir toutes les cellules (largeur mesurée en temps réel + 32px de marge). Chaque agent occupe une cellule avec :
- La mascotte animée selon son état (`working` → marche, `waitingApprove` → bras levé, `ended` → bounce, `error` → shake, `idle` → zZZ, `disconnected` → grisé+transparent)
- Son nom de session en 13px gris clair
- Son état en 11px bold uppercase coloré (`WORKING` en vert, `WAITING` en jaune/orange, `ERROR` en rouge, etc.)

---

## Fonctionnalités opérationnelles

| Fonctionnalité | État |
|---|---|
| Affichage agents réels depuis le backend Rust | ✅ |
| Mascotte animée par état | ✅ |
| Redimensionnement dynamique (ResizeObserver) | ✅ |
| Drag pour repositionner (drag natif Windows) | ✅ |
| Position mémorisée au redémarrage | ✅ |
| Fallback haut-centre si position hors-écran | ✅ |
| Clic → focus terminal (`quickTerminalAccess`) | ✅ best-effort |
| Hover visuel sur les cellules | ✅ |
| Settings appliqués en temps réel (sans redémarrage) | ✅ |
| Overflow (+N agents) | ❌ Story 3.4 |
| Indicateur temporel ("attend depuis X min") | ❌ Story 3.5 |
| Clic → vue détail | ❌ Story 4.3 |

---

## Flux de données

```
Rust (state_engine.rs)
  → emit("agents:snapshot", Vec<AgentSnapshot>)
    → useTauriEvents  →  agentStore (Zustand)
      → NotchApp  →  NotchContainer  →  AgentCell

SettingsApp (fenêtre séparée)
  → saveSettings()  →  emit("vigil:settings-changed", VigilSettings)
    → useSettings (hook dans AgentCell)
      → re-render immédiat si quickTerminalAccess a changé
```

---

## Limitations connues

**Focus terminal (clic sur AgentCell)** : l'implémentation Win32 est best-effort.
- Focus en arrière-plan : fiable (SetForegroundWindow retourne TRUE).
- Restauration d'une fenêtre minimisée : heuristique (préférence pour la seule fenêtre CASCADIA minimisée), peut échouer si plusieurs fenêtres Windows Terminal sont minimisées.
- Plusieurs fenêtres Windows Terminal non-minimisées : impossible de distinguer laquelle correspond à quelle session via l'API Win32 publique — les deux agents focuseront la même fenêtre WT dans ce cas. Aucune alternative viable sans API interne WT (non documentée).

**zZZ clippés en haut** : les animations zZZ flottent vers Y négatif mais sont clippées par la bordure supérieure de la fenêtre Tauri quand le notch est positionné à y≈0. Comportement acceptable — le signal `idle` est porté par l'opacité réduite (45%) de la mascotte.
