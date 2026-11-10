//! Story 10.3 — Miroir des binaires de hook à un chemin SANS ESPACE.
//!
//! **L'invariant, en une phrase :** *le chemin d'exe inscrit dans la config d'un outil ne contient
//! jamais d'espace.* Un invariant unique remplace cinq comportements de quoting différents — dont
//! **trois réfutés à la sonde** (Antigravity appende la commande brute, Grok découpe sur les
//! espaces sans shell, Codex annonce « completed » sans exécuter). Voir le spike
//! `spike-cmd-quoting-espaces-2026-07-24/README.md`, F4bis / F6 / F7.
//!
//! Les binaires vivaient jusqu'ici à côté de l'exe principal (`%LOCALAPPDATA%\vigil\`), donc **sous
//! le profil utilisateur, porteur de l'espace du nom de compte** (`C:\Users\John Doe\…`). Ils sont
//! désormais recopiés sous `%ProgramData%\Vigil\hooks\<SID>\` — `C:\ProgramData` ne contient
//! d'espace sur aucune installation Windows, et le sous-dossier par SID (jamais d'espace, jamais de
//! collision, contrairement au nom d'utilisateur) isole les comptes entre eux.
//!
//! # Le vrai travail : l'ACL
//!
//! `C:\ProgramData` accorde nativement à `BUILTIN\Utilisateurs` le droit de **créer** (mesuré sur le
//! poste : ACE `0x116` = AddFile+AddSubdirectory+WriteEA+WriteAttributes, hérité aux conteneurs).
//! Déposer un **exécutable** dans un emplacement où d'autres comptes peuvent écrire, et le faire
//! lancer automatiquement sous l'identité d'un utilisateur, est un vecteur classique d'élévation de
//! privilèges. Le dossier `<SID>` rompt donc l'héritage et n'accorde l'écriture qu'au
//! **propriétaire**, à **SYSTEM** et aux **Administrateurs**.
//!
//! Deux contraintes MESURÉES au gate T0a (2026-07-28), à ne jamais éroder :
//!   1. **N'écrire que le DACL** (`SetNamedSecurityInfoW` + `DACL_SECURITY_INFORMATION`). Écrire un
//!      descripteur complet touche le SACL et exige `SeSecurityPrivilege` → échec sans élévation.
//!   2. **Construire le DACL de zéro.** « Copier les ACE héritées puis les retirer en mémoire » ne
//!      retire rien : les ACE héritées ne deviennent explicites qu'à l'écriture.
//!
//! # Topologie (décision Steven, 2026-07-28 — gate T0c)
//!
//! ```text
//! C:\ProgramData\Vigil\            <- Utilisateurs : lecture + création de sous-dossiers SEULEMENT
//!               \hooks\            <- idem
//!                     \<SID-A>\    <- PROTÉGÉ : A + SYSTEM + Administrateurs, rien d'autre
//!                     \<SID-B>\    <- PROTÉGÉ : B + SYSTEM + Administrateurs, rien d'autre
//! ```
//!
//! Le parent reste franchissable pour qu'un **second compte Windows** puisse créer son propre
//! `<SID>` (l'option « tout durcir au premier compte » l'aurait renvoyé au repli, donc au fail-open
//! d'origine). Il n'accorde jamais l'écriture de *fichiers* : rien d'exécutable n'y vit.
//!
//! ⚠️ **Risque accepté, hors périmètre (décision Steven, 2026-07-28)** : `C:\ProgramData` laissant
//! tout compte local créer un dossier, un compte B peut *pré-créer* notre dossier avant nous. Aucune
//! topologie n'y échappe sans élévation. Aucune vérification de propriétaire n'est faite ici. La
//! couverture qui en subsiste est structurelle et non intentionnelle : le durcissement du `<SID>`
//! exige `WRITE_DAC`, donc échoue sur un dossier possédé par un tiers → `ensure_mirrored` rend
//! `None` → repli tracé, et on n'exécute pas depuis ce dossier.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Nom du dossier racine sous `%ProgramData%`.
const MIRROR_ROOT_NAME: &str = "Vigil";
/// Sous-dossier portant les binaires (un niveau `<SID>` en dessous).
const MIRROR_HOOKS_NAME: &str = "hooks";

/// Binaires dont la DERNIÈRE tentative de miroir a échoué, **par chemin source**.
///
/// Sert un seul but, mais critique : empêcher la **boucle de réparation**. Si le miroir est
/// définitivement impossible sur ce poste (ACL refusée, disque plein, dossier squatté), la
/// conformité doit se stabiliser sur le chemin de repli au lieu de déclarer les hooks non conformes
/// à chaque démarrage — sans quoi l'auto-repair réécrirait les cinq configs en boucle et l'interface
/// afficherait un défaut permanent. Le binaire sort de l'ensemble dès qu'une tentative réussit : une
/// condition transitoire (disque plein puis libéré) doit pouvoir se rattraper sans redémarrage.
///
/// ⚠️ **PAR BINAIRE, jamais global** (revue 10.3). Un `AtomicBool` unique pour les cinq outils
/// laissait l'échec de l'un — un `vigil-kimi.exe` absent, une quarantaine antivirus — désarmer la
/// détection de dérive des QUATRE AUTRES, de façon dépendante de l'ordre d'évaluation
/// (`are_hooks_conformant_for_tool`, cinq appels consécutifs). C'est exactement le motif
/// `transcript_hash` que l'AC4 dit rendre structurel.
static MIRROR_FAILED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

/// Marque (ou démarque) `src` comme « miroir hors d'atteinte ».
fn set_mirror_failed(src: &Path, failed: bool) {
    let mut guard = match MIRROR_FAILED.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let set = guard.get_or_insert_with(HashSet::new);
    if failed {
        set.insert(src.to_path_buf());
    } else {
        set.remove(src);
    }
}

/// `true` si la dernière tentative de miroir de `src` a échoué.
fn mirror_failed_for(src: &Path) -> bool {
    let guard = match MIRROR_FAILED.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    guard.as_ref().is_some_and(|set| set.contains(src))
}

// ── Cœurs purs (aucune dépendance au système de fichiers) ────────────────────

/// `true` si `raw` peut être inscrit TEL QUEL dans la commande d'un outil — **cœur pur**.
///
/// Deux exigences, pas une seule :
///   1. **Chemin local à lettre de lecteur** (`X:\…`). Un `%ProgramData%` redirigé en UNC
///      (`\\serveur\part`) ferait porter aux cinq configs un chemin d'exe **distant** : hors ligne,
///      tous les hooks meurent alors que la config est déjà posée. (Un lecteur RÉSEAU mappé reste
///      indiscernable d'un disque local sans appel système — limite assumée de cette garde.)
///   2. **Jeu de caractères en liste blanche.** La garde d'origine ne rejetait que `U+0020`, ce qui
///      laissait passer la tabulation, l'espace insécable, et surtout les métacaractères `cmd`
///      (`&`, `^`, `(`, `)`, `%`) — injectés tels quels dans `cmd /d /s /c` pour Antigravity et
///      Kimi, qui n'appliquent aucun quoting : `D:\Data&Apps` COUPE la commande. C'est la même
///      classe de fail-open silencieux que celle que cette story referme.
fn path_is_command_safe(raw: &str) -> bool {
    let mut chars = raw.chars();
    let Some(drive) = chars.next() else {
        return false;
    };
    if !drive.is_ascii_alphabetic() || chars.next() != Some(':') {
        return false;
    }
    if !matches!(chars.next(), Some('\\') | Some('/')) {
        return false;
    }
    raw.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '\\' | '/' | ':' | '-' | '_' | '.'))
}

/// Racine du miroir déduite de `%ProgramData%` — **cœur pur**, paramétré par la valeur d'environnement.
///
/// `None` (donc repli, AC5) si la variable est absente, vide, ou porte un chemin que l'on ne peut
/// pas inscrire sans risque dans une commande (cf. `path_is_command_safe`) : la valeur est
/// théoriquement relocalisable, et tout l'intérêt de cette story est de ne rien supposer sur la
/// forme d'un chemin. On vérifie, on ne présume pas.
pub(crate) fn mirror_root_from(program_data: Option<&str>) -> Option<PathBuf> {
    let raw = program_data?.trim();
    if raw.is_empty() || !path_is_command_safe(raw) {
        return None;
    }
    Some(PathBuf::from(raw).join(MIRROR_ROOT_NAME))
}

/// Dossier miroir final : `<root>\hooks\<SID>` — **cœur pur**.
///
/// `None` si le SID est vide ou porte autre chose que la forme `S-1-5-21-…` (impossible en pratique ;
/// la garde évite qu'une valeur dégradée réintroduise le bug — espace, métacaractère `cmd`).
pub(crate) fn mirror_dir_from(root: &Path, sid: &str) -> Option<PathBuf> {
    if sid.is_empty()
        || !sid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    Some(root.join(MIRROR_HOOKS_NAME).join(sid))
}

/// Commande d'exe finale, **cœur pur** de la cascade de l'AC5 : miroir → forme courte 8.3 → chemin
/// long. `bash_slashes` = mise en forme de l'outil (forward-slashes pour Claude/Codex/Grok qui
/// passent par bash, backslashes natifs pour Antigravity/Kimi qui passent par `cmd`).
///
/// Le miroir change le **dossier**, jamais la convention d'écriture de l'outil.
pub(crate) fn hook_command_core(
    src: &Path,
    mirror: Option<&Path>,
    short: Option<&str>,
    bash_slashes: bool,
) -> String {
    let chosen: String = match (mirror, short) {
        (Some(m), _) => m.to_string_lossy().into_owned(),
        (None, Some(s)) => s.to_string(),
        (None, None) => src.to_string_lossy().into_owned(),
    };
    if bash_slashes {
        chosen.replace('\\', "/")
    } else {
        chosen
    }
}

/// Dernier filet : trace quand la commande produite RETOMBE dans le bug d'origine.
///
/// L'invariant de la story — « le chemin d'exe inscrit dans la config ne contient jamais d'espace »
/// — n'était **jamais évalué sur la valeur produite** (revue 10.3). Toutes les gardes sont en amont ;
/// or le 3ᵉ étage de la cascade rend un chemin à espace par définition, et le 2ᵉ aussi lorsque la
/// génération 8.3 est désactivée sur le volume (`GetShortPathNameW` réussit en rendant le chemin
/// LONG — c'est le fail-open d'origine). C'est exactement l'instant où l'utilisateur y retombe, et
/// c'était le seul endroit où le code se taisait.
pub(crate) fn warn_if_space_survives(cmd: &str) {
    if cmd.contains(' ') {
        trace_once(&format!(
            "repli avec espace RÉSIDUELLE dans `{cmd}` — le hook est susceptible d'échouer \
             SILENCIEUSEMENT (Antigravity appende la commande brute, Grok découpe sur les espaces, \
             Codex annonce « completed » sans exécuter — spike F4bis/F6/F7)"
        ));
    }
}

/// Empreinte d'un fichier pour la comparaison de fraîcheur : `(taille, mtime en ns depuis EPOCH)`.
pub(crate) type Stamp = (u64, u128);

/// `true` si le miroir doit être (re)copié — **cœur pur** (AC4/T3).
///
/// Comparaison par **taille + mtime**, jamais par hash à ce niveau : le prédicat de conformité qui
/// s'appuie dessus tourne à chaque démarrage et pour les cinq outils. Le hash n'intervient qu'en
/// aval, uniquement quand ces deux-là diffèrent (cf. `same_binary`), pour ne pas recopier sur une
/// simple dérive d'horodatage.
pub(crate) fn needs_copy_core(src: Option<Stamp>, dst: Option<Stamp>) -> bool {
    match (src, dst) {
        // Source illisible : on ne peut rien affirmer, donc rien à recopier (le repli tranchera).
        (None, _) => false,
        (Some(_), None) => true,
        (Some(s), Some(d)) => s != d,
    }
}

/// Refuse un chemin qui existe mais n'est **pas** un vrai dossier nous appartenant en propre.
///
/// ⚠️ **Le garde-fou le plus important du module** (revue 10.3). `Path::exists()` **suit** les points
/// d'analyse, et `SetNamedSecurityInfoW`/`SE_FILE_OBJECT` aussi (aucun `FILE_FLAG_OPEN_REPARSE_POINT`).
/// Sans cette garde, un compte B pouvait créer `hooks\<SID-A>` non pas comme dossier mais comme
/// **jonction** — aucun privilège requis, et la DACL parente accorde volontairement
/// `FILE_ADD_SUBDIRECTORY` à `BUILTIN\Utilisateurs` — vers un dossier qu'il possède, en s'y accordant
/// `WRITE_DAC` pour A. `harden_leaf` réussissait alors **sur la cible**, `ensure_mirrored` rendait
/// `Some(…)`, et A copiait puis exécutait automatiquement un binaire dans un dossier dont B reste
/// **propriétaire** — donc où B conserve `WRITE_DAC` à vie et peut substituer l'exe.
///
/// Le risque de squat est déclaré accepté (décision Steven), mais la note en tête de module fonde ce
/// qui en subsiste sur : « le durcissement exige `WRITE_DAC`, donc échoue sur un dossier possédé par
/// un tiers → `None` → on n'exécute pas depuis là ». La jonction annulait exactement cette phrase, et
/// l'issue devenait **pire** que le squat accepté : au lieu d'un repli, une exécution.
///
/// Referme au passage le cas « le chemin existe et est un **fichier** », où `prepare_dir` rendait
/// `Ok` après avoir posé une DACL protégée sur un fichier étranger.
fn reject_reparse_or_file(path: &Path) -> Result<(), String> {
    // `symlink_metadata` ne suit PAS le point d'analyse — c'est tout l'objet de l'appel.
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(()); // absent (ou illisible) : rien à refuser, la création tranchera
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        // Côté Rust/Windows, `is_symlink()` couvre AUSSI les jonctions (FILE_ATTRIBUTE_REPARSE_POINT).
        return Err(format!(
            "{} est un point d'analyse (jonction ou lien) — refusé : le durcissement porterait sur \
             sa cible, possédée par un tiers",
            path.display()
        ));
    }
    if !ft.is_dir() {
        return Err(format!("{} existe et n'est pas un dossier", path.display()));
    }
    Ok(())
}

/// `true` si le miroir est en dérive et doit invalider la conformité — **cœur pur** (AC4).
///
/// > Motif : deux copies d'un même binaire qui divergent en silence, c'est exactement le contrat
/// > `transcript_hash` — deux mois de bug muet. La parade est structurelle : un miroir absent ou
/// > différent rend `are_*_hooks_installed` faux, donc l'auto-repair recopie puis réécrit les
/// > configs. Une dérive de version ne peut pas survivre à un démarrage.
///
/// `mirror_failed` court-circuite tout : quand le miroir est hors d'atteinte, le mode nominal EST le
/// repli, et il n'y a plus rien à déclarer périmé (cf. `MIRROR_LAST_FAILED`).
pub(crate) fn stale_core(
    mirror_failed: bool,
    target_exists: Option<bool>,
    same_binary: bool,
) -> bool {
    if mirror_failed {
        return false;
    }
    match target_exists {
        None => false,          // pas de chemin miroir possible → repli assumé
        Some(false) => true,    // miroir ABSENT → non conforme
        Some(true) => !same_binary, // miroir DIFFÉRENT du binaire source → non conforme
    }
}

// ── Implémentation Windows ───────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::OnceLock;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
        NO_MULTIPLE_TRUSTEE, SET_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
        TRUSTEE_W,
    };
    use windows::Win32::Security::{
        CreateWellKnownSid, GetTokenInformation, ACE_FLAGS, ACL, CONTAINER_INHERIT_ACE,
        DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE, OBJECT_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
        WELL_KNOWN_SID_TYPE, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Contrôle total (`FILE_ALL_ACCESS`).
    const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
    /// Lecture + exécution (`FILE_GENERIC_READ | FILE_GENERIC_EXECUTE`) **plus**
    /// `FILE_ADD_SUBDIRECTORY` (0x4) — le strict nécessaire pour qu'un SECOND compte Windows puisse
    /// créer son propre `<SID>` sous le parent. Ni écriture de fichier, ni suppression : rien
    /// d'exécutable ne vit à ce niveau.
    const FILE_TRAVERSE_AND_MKDIR: u32 = 0x0012_00A9 | 0x0000_0004;

    /// Héritage aux conteneurs ET aux objets — le `<SID>` protège de toute façon sa propre DACL.
    fn inherit_all() -> ACE_FLAGS {
        ACE_FLAGS(CONTAINER_INHERIT_ACE.0 | OBJECT_INHERIT_ACE.0)
    }

    /// Héritage aux **conteneurs seuls**.
    ///
    /// ⚠️ Réservé à l'ACE `Utilisateurs` du parent (revue 10.3) : avec `OBJECT_INHERIT_ACE`, le bit
    /// `0x0004` n'est plus `FILE_ADD_SUBDIRECTORY` mais `FILE_APPEND_DATA` dès qu'il retombe sur un
    /// objet **fichier** — un grant latent, exactement là où le commentaire de
    /// `FILE_TRAVERSE_AND_MKDIR` promet « ni écriture de fichier, ni suppression ».
    fn inherit_containers_only() -> ACE_FLAGS {
        ACE_FLAGS(CONTAINER_INHERIT_ACE.0)
    }

    /// Tampon aligné 8 octets portant un SID (un SID exige un alignement 4 au minimum).
    struct SidBuf(Vec<u64>);

    impl SidBuf {
        fn with_bytes(n: usize) -> Self {
            SidBuf(vec![0u64; n.div_ceil(8) + 1])
        }
        fn psid(&self) -> PSID {
            PSID(self.0.as_ptr() as *mut c_void)
        }
        fn as_mut_ptr(&mut self) -> *mut c_void {
            self.0.as_mut_ptr() as *mut c_void
        }
    }

    /// SID « bien connu » (SYSTEM, Administrateurs) — jamais son NOM : les noms de groupes intégrés
    /// sont **localisés** (`BUILTIN\Utilisateurs` sur un Windows français).
    fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> Option<SidBuf> {
        // SECURITY_MAX_SID_SIZE = 68 octets.
        let mut buf = SidBuf::with_bytes(68);
        let mut cb: u32 = 68;
        unsafe {
            CreateWellKnownSid(kind, PSID(std::ptr::null_mut()), PSID(buf.as_mut_ptr()), &mut cb)
                .ok()?;
        }
        Some(buf)
    }

    /// SID de l'utilisateur courant, lu dans le jeton du processus.
    fn current_user_sid() -> Option<SidBuf> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
            let mut needed: u32 = 0;
            // 1er appel : taille requise (échoue avec ERROR_INSUFFICIENT_BUFFER, attendu).
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
            if needed == 0 {
                let _ = CloseHandle(token);
                return None;
            }
            let mut raw = SidBuf::with_bytes(needed as usize);
            let res = GetTokenInformation(
                token,
                TokenUser,
                Some(raw.as_mut_ptr()),
                needed,
                &mut needed,
            );
            let _ = CloseHandle(token);
            res.ok()?;
            // Le SID pointé par TOKEN_USER vit DANS `raw` : on le recopie dans un tampon autonome
            // pour ne pas dépendre de la disposition mémoire de la structure.
            let tu = &*(raw.0.as_ptr() as *const TOKEN_USER);
            let sid_ptr = tu.User.Sid.0 as *const u8;
            // SubAuthorityCount est le 2e octet du SID ; longueur = 8 + 4 * count.
            let count = *sid_ptr.add(1) as usize;
            let len = 8 + 4 * count;
            let mut out = SidBuf::with_bytes(len);
            std::ptr::copy_nonoverlapping(sid_ptr, out.as_mut_ptr() as *mut u8, len);
            Some(out)
        }
    }

    /// Forme textuelle d'un SID (`S-1-5-21-…`) — le nom du sous-dossier miroir.
    fn sid_to_string(sid: &SidBuf) -> Option<String> {
        unsafe {
            let mut out = PWSTR::null();
            ConvertSidToStringSidW(sid.psid(), &mut out).ok()?;
            if out.is_null() {
                return None;
            }
            let mut len = 0usize;
            while *out.0.add(len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(out.0, len));
            let _ = LocalFree(HLOCAL(out.0 as *mut c_void));
            Some(s)
        }
    }

    /// SID courant, résolu une fois par processus (il ne change pas).
    ///
    /// ⚠️ **Seul le SUCCÈS est mémoïsé** (revue 10.3). Un `OnceLock<Option<String>>` figeait aussi
    /// l'échec : une défaillance ponctuelle d'`OpenProcessToken`/`GetTokenInformation` désactivait le
    /// miroir jusqu'au redémarrage de Vigil, alors que `MIRROR_FAILED` est explicitement conçu pour
    /// permettre le rattrapage sans redémarrage.
    pub(super) fn current_sid_string() -> Option<&'static str> {
        static SID: OnceLock<String> = OnceLock::new();
        if let Some(s) = SID.get() {
            return Some(s.as_str());
        }
        let resolved = current_user_sid().as_ref().and_then(sid_to_string)?;
        let _ = SID.set(resolved);
        SID.get().map(|s| s.as_str())
    }

    /// Pose un DACL **explicite et protégé** (héritage rompu) sur `path`.
    ///
    /// ⚠️ Deux invariants issus du gate T0a, ne pas les éroder :
    ///   - `DACL_SECURITY_INFORMATION` **seul** (+ le drapeau de protection) : écrire un descripteur
    ///     complet touche le SACL et exige `SeSecurityPrivilege`, absent sans élévation ;
    ///   - le DACL est construit **de zéro** : recopier l'héritage pour le purger ensuite ne retire
    ///     rien, les ACE héritées ne devenant explicites qu'à l'écriture.
    fn set_protected_dacl(path: &Path, entries: &[(PSID, u32, ACE_FLAGS)]) -> Result<(), u32> {
        let mut ea: Vec<EXPLICIT_ACCESS_W> = entries
            .iter()
            .map(|(sid, mask, inherit)| EXPLICIT_ACCESS_W {
                grfAccessPermissions: *mask,
                grfAccessMode: SET_ACCESS,
                grfInheritance: *inherit,
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: std::ptr::null_mut(),
                    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_UNKNOWN,
                    ptstrName: PWSTR(sid.0 as *mut u16),
                },
            })
            .collect();

        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        unsafe {
            let mut acl: *mut ACL = std::ptr::null_mut();
            let rc = SetEntriesInAclW(Some(ea.as_mut_slice()), None, &mut acl);
            if rc.0 != 0 {
                return Err(rc.0);
            }
            let rc = SetNamedSecurityInfoW(
                windows::core::PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                OBJECT_SECURITY_INFORMATION(
                    DACL_SECURITY_INFORMATION.0 | PROTECTED_DACL_SECURITY_INFORMATION.0,
                ),
                PSID(std::ptr::null_mut()),
                PSID(std::ptr::null_mut()),
                Some(acl),
                None,
            );
            if !acl.is_null() {
                let _ = LocalFree(HLOCAL(acl as *mut c_void));
            }
            if rc.0 != 0 {
                return Err(rc.0);
            }
        }
        Ok(())
    }

    /// DACL du **parent** (`Vigil\`, `Vigil\hooks\`) : SYSTEM, Administrateurs et le **créateur** en
    /// contrôle total, les autres utilisateurs en lecture + création de sous-dossiers seulement.
    ///
    /// ⚠️ L'ACE du créateur n'est PAS décorative — sans elle, le dossier devient indéracinable par
    /// celui-là même qui l'a posé : un propriétaire NTFS n'hérite que de `READ_CONTROL` et
    /// `WRITE_DAC`, jamais de `DELETE`. Le désinstalleur (AC7) échouerait alors à retirer la coquille
    /// vide `%ProgramData%\Vigil\hooks\`. Constaté au nettoyage des bancs de test, pas déduit.
    ///
    /// La contrepartie assumée est que le créateur obtient `DELETE_CHILD` sur `hooks\`, donc peut
    /// supprimer le `<SID>` d'un autre compte — c'est exactement la sémantique `CREATOR OWNER` de
    /// `C:\ProgramData` lui-même, et cela reste un déni de service, pas une exécution de code : le
    /// dossier recréé par un tiers n'est plus durcissable par sa victime, donc `ensure_mirrored` rend
    /// `None` et l'on n'exécute pas depuis là (cf. le risque de squat en tête de module).
    ///
    /// Best-effort : un **second** compte n'est ni propriétaire du parent ni titulaire de
    /// `WRITE_DAC` dessus — son échec ici est normal et sans conséquence, la sécurité tient au
    /// niveau `<SID>`.
    fn harden_parent(path: &Path) {
        let (Some(system), Some(admins), Some(users), Some(me)) = (
            well_known_sid(WinLocalSystemSid),
            well_known_sid(WinBuiltinAdministratorsSid),
            well_known_sid(windows::Win32::Security::WinBuiltinUsersSid),
            current_user_sid(),
        ) else {
            return;
        };
        let _ = set_protected_dacl(
            path,
            &[
                (me.psid(), FILE_ALL_ACCESS, inherit_all()),
                (system.psid(), FILE_ALL_ACCESS, inherit_all()),
                (admins.psid(), FILE_ALL_ACCESS, inherit_all()),
                // Conteneurs SEULS : cf. `inherit_containers_only`.
                (users.psid(), FILE_TRAVERSE_AND_MKDIR, inherit_containers_only()),
            ],
        );
    }

    /// DACL du dossier `<SID>` : **propriétaire + SYSTEM + Administrateurs, rien d'autre** (AC3).
    /// Son échec est bloquant — c'est de lui que dépend la sûreté de l'exécutable déposé.
    ///
    /// Le motif d'échec est rendu TEL QUEL et non aplati en code numérique : « résolution des SID
    /// impossible » et « `SetNamedSecurityInfoW` a refusé » envoient l'opérateur chercher dans deux
    /// directions opposées, et l'ancien `Err(u32::MAX)` s'affichait en « code 4294967295 », qui
    /// n'est pas un code Win32 et ne désigne aucun problème de permission NTFS.
    fn harden_leaf(path: &Path) -> Result<(), String> {
        let (Some(system), Some(admins), Some(me)) = (
            well_known_sid(WinLocalSystemSid),
            well_known_sid(WinBuiltinAdministratorsSid),
            current_user_sid(),
        ) else {
            return Err(
                "résolution des SID (SYSTEM / Administrateurs / compte courant) impossible — \
                 aucun appel d'ACL n'a été tenté"
                    .to_string(),
            );
        };
        set_protected_dacl(
            path,
            &[
                (me.psid(), FILE_ALL_ACCESS, inherit_all()),
                (system.psid(), FILE_ALL_ACCESS, inherit_all()),
                (admins.psid(), FILE_ALL_ACCESS, inherit_all()),
            ],
        )
        .map_err(|code| format!("SetNamedSecurityInfoW a refusé (code Win32 {code})"))
    }

    /// Crée l'arborescence miroir et applique les DACL. `Ok` = le dossier `<SID>` est utilisable.
    pub(super) fn prepare_dir(dir: &Path) -> Result<(), String> {
        let hooks = dir.parent().ok_or_else(|| "chemin miroir sans parent".to_string())?;
        let root = hooks.parent().ok_or_else(|| "chemin miroir sans racine".to_string())?;

        for parent in [root, hooks] {
            reject_reparse_or_file(parent)?;
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("création de {} impossible : {e}", parent.display()))?;
            // Best-effort : voir `harden_parent`.
            harden_parent(parent);
        }

        reject_reparse_or_file(dir)?;
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("création de {} impossible : {e}", dir.display()))?;
        // Re-contrôlé APRÈS création : entre le premier contrôle et ici, un tiers a pu poser une
        // jonction à la place. La fenêtre n'est pas nulle, mais elle passe de « permanente » à
        // « une course à gagner » — et le durcissement qui suit ne portera jamais sur une cible
        // étrangère sans qu'on l'ait vu.
        reject_reparse_or_file(dir)?;
        // Réappliqué à CHAQUE passage, jamais seulement à la création : un dossier préexistant mal
        // permissionné (ancienne version, création manuelle) doit être corrigé, pas hérité.
        harden_leaf(dir)
            .map_err(|why| format!("durcissement ACL de {} : {why}", dir.display()))
    }

    /// Une ACE d'autorisation telle que mesurée : `(SID, masque d'accès, drapeaux d'héritage)`.
    #[cfg(test)]
    pub(super) type AceView = (String, u32, u8);

    /// `(dacl_protégée, ACE d'AUTORISATION, nombre d'ACE d'un AUTRE type)`.
    #[cfg(test)]
    pub(super) type DaclSnapshot = (bool, Vec<AceView>, usize);

    /// Lecture de la DACL effective — support du test d'intégration de l'AC3.
    ///
    /// ⚠️ Le 3ᵉ terme n'est pas décoratif (revue 10.3) : la version d'origine castait **toute** ACE
    /// en `ACCESS_ALLOWED_ACE` sans lire `AceType`. Une ACE de refus, et surtout une
    /// `ACCESS_ALLOWED_OBJECT_ACE` (disposition différente : `Flags`/`ObjectType` intercalés avant
    /// `SidStart`), faisait lire le SID au mauvais offset — et c'est le SEUL instrument de mesure de
    /// la propriété de sécurité centrale de la story. Un instrument qui ne distingue pas
    /// « autoriser » de « refuser » ne peut pas valider une DACL.
    ///
    /// ⚠️ `AceFlags` est rendu pour la MÊME raison (2ᵉ passe de revue 10.3) : sans lui, la sonde ne
    /// mesurait que SID + masque, donc était **structurellement incapable** de voir l'héritage —
    /// et `inherit_containers_only` est précisément une propriété d'héritage. La suite pouvait
    /// rester verte pendant qu'un `OBJECT_INHERIT_ACE` sur l'ACE `Utilisateurs` du parent
    /// transformait `FILE_ADD_SUBDIRECTORY` en `FILE_APPEND_DATA` sur les fichiers hérités. Cas
    /// réellement observé sur disque avant le correctif, pas hypothétique.
    #[cfg(test)]
    pub(super) fn dacl_snapshot(path: &Path) -> Option<DaclSnapshot> {
        use windows::Win32::Security::Authorization::GetNamedSecurityInfoW;
        use windows::Win32::Security::{
            GetAce, GetSecurityDescriptorControl, ACCESS_ALLOWED_ACE, PSECURITY_DESCRIPTOR,
            SE_DACL_PROTECTED,
        };

        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut sd = PSECURITY_DESCRIPTOR::default();
            let rc = GetNamedSecurityInfoW(
                windows::core::PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                &mut sd,
            );
            if rc.0 != 0 || dacl.is_null() {
                return None;
            }
            let mut control: u16 = 0;
            let mut revision: u32 = 0;
            GetSecurityDescriptorControl(sd, &mut control, &mut revision).ok()?;
            let protected = control & SE_DACL_PROTECTED.0 != 0;

            // `ACCESS_ALLOWED_ACE_TYPE` = 0. Toute autre valeur a une disposition mémoire
            // différente : on la COMPTE au lieu de la mal lire.
            const ACCESS_ALLOWED: u8 = 0;
            let mut aces = Vec::new();
            let mut autres = 0usize;
            for i in 0..(*dacl).AceCount as u32 {
                let mut ace: *mut c_void = std::ptr::null_mut();
                if GetAce(dacl, i, &mut ace).is_err() || ace.is_null() {
                    continue;
                }
                let allowed = &*(ace as *const ACCESS_ALLOWED_ACE);
                if allowed.Header.AceType != ACCESS_ALLOWED {
                    autres += 1;
                    continue;
                }
                let sid_ptr = std::ptr::addr_of!(allowed.SidStart) as *const c_void;
                let mut out = PWSTR::null();
                if ConvertSidToStringSidW(PSID(sid_ptr as *mut c_void), &mut out).is_err() {
                    continue;
                }
                let mut len = 0usize;
                while *out.0.add(len) != 0 {
                    len += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(out.0, len));
                let _ = LocalFree(HLOCAL(out.0 as *mut c_void));
                aces.push((s, allowed.Mask, allowed.Header.AceFlags));
            }
            let _ = LocalFree(HLOCAL(sd.0));
            Some((protected, aces, autres))
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use super::*;

    pub(super) fn current_sid_string() -> Option<&'static str> {
        None
    }
    pub(super) fn prepare_dir(_dir: &Path) -> Result<(), String> {
        Err("miroir de hook non supporté hors Windows".to_string())
    }
}

// ── API publique du module ───────────────────────────────────────────────────

/// Dossier miroir de CE compte, sans effet de bord — `%ProgramData%\Vigil\hooks\<SID>`.
pub(crate) fn mirror_dir() -> Option<PathBuf> {
    let program_data = std::env::var("ProgramData").ok();
    let root = mirror_root_from(program_data.as_deref())?;
    mirror_dir_from(&root, imp::current_sid_string()?)
}

/// Chemin que le binaire `src` OCCUPERAIT dans le miroir, sans effet de bord.
pub(crate) fn mirror_target(src: &Path) -> Option<PathBuf> {
    Some(mirror_dir()?.join(src.file_name()?))
}

/// Empreinte `(taille, mtime)` d'un fichier, `None` s'il est illisible.
fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((meta.len(), mtime))
}

/// SHA-256 d'un fichier — n'intervient QU'EN CAS de divergence taille/mtime.
fn digest(path: &Path) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Some(h.finalize().into())
}

/// `true` si les deux fichiers portent le MÊME binaire.
///
/// Taille + mtime d'abord (le prédicat de conformité tourne à chaque démarrage, pour cinq outils) ;
/// le hash ne départage que les cas où ces deux-là diffèrent — typiquement une dérive d'horodatage
/// après une restauration de sauvegarde, qui ne justifie pas une recopie.
fn same_binary(src: &Path, dst: &Path) -> bool {
    if !needs_copy_core(stamp(src), stamp(dst)) {
        return true;
    }
    match (digest(src), digest(dst)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn trace(message: &str) {
    eprintln!("[vigil/hook-mirror] {message}");
}

/// Copie `src` vers `dst` sans jamais laisser de fichier PARTIEL à `dst`.
///
/// ⚠️ `fs::copy` (CopyFileExW) écrit **en place** : un disque plein, une erreur d'E/S ou un processus
/// tué en cours de route laissaient un exécutable **tronqué** au chemin déjà inscrit dans les cinq
/// configs — et chaque invocation de hook l'exécutait jusqu'à la réécriture suivante. Même sans
/// erreur, un hook lancé pendant la fenêtre de copie mappait un fichier partiel.
///
/// On écrit donc à côté puis on renomme. Le `rename` échoue si `dst` est une image **en cours
/// d'exécution** : c'est attendu, et l'appelant conserve alors le miroir périmé (cf.
/// `ensure_mirrored_inner`).
fn copy_atomic(src: &Path, dst: &Path) -> std::io::Result<()> {
    // Extension volontairement SANS `.exe` : un résidu ne doit jamais passer pour un binaire de hook.
    let tmp = dst.with_extension("vigil-tmp");
    let _ = std::fs::remove_file(&tmp);
    if let Err(e) = std::fs::copy(src, &tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    match std::fs::rename(&tmp, dst) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Aligne le mtime de `dst` sur celui de `src` — best-effort, jamais bloquant.
///
/// Appelé QUAND le contenu est identique mais l'empreinte `(taille, mtime)` divergente. Sans cet
/// alignement la comparaison rapide ne converge **jamais** : chaque contrôle de conformité repaie
/// deux SHA-256 complets, dans `is_stale` **et** dans `ensure_mirrored`, indéfiniment. Atteignable
/// sur un `%ProgramData%` relocalisé en exFAT ou sur un partage (granularité de mtime) et après une
/// restauration de sauvegarde.
fn align_mtime(src: &Path, dst: &Path) {
    let Ok(mtime) = std::fs::metadata(src).and_then(|m| m.modified()) else {
        return;
    };
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(dst) else {
        return; // dst en cours d'exécution ou verrouillé : sans conséquence, on retentera
    };
    let _ = file.set_times(std::fs::FileTimes::new().set_modified(mtime));
}

/// Garantit la présence du binaire `src` dans le miroir et rend son chemin miroir.
///
/// `None` = miroir indisponible → l'appelant retombe sur la cascade de l'AC5 (8.3 puis chemin long).
/// **Ne panique jamais et ne laisse jamais un outil sans hook** : l'échec du miroir dégrade la
/// résolution, il n'interrompt rien.
pub(crate) fn ensure_mirrored(src: &Path) -> Option<PathBuf> {
    if !src.is_file() {
        // ⚠️ Traité AVANT `ensure_mirrored_inner`, et sans armer `MIRROR_FAILED` (revue 10.3) :
        // « ce binaire-là n'existe pas » n'est pas « le miroir est hors d'atteinte sur ce poste ».
        // Confondre les deux, c'était laisser un outil non déployé désarmer la détection de dérive
        // des autres.
        trace_once(&format!("binaire source introuvable : {}", src.display()));
        return None;
    }
    let outcome = ensure_mirrored_inner(src);
    set_mirror_failed(src, outcome.is_none());
    outcome
}

fn ensure_mirrored_inner(src: &Path) -> Option<PathBuf> {
    let dir = match mirror_dir() {
        Some(d) => d,
        None => {
            trace_once(
                "%ProgramData% absent, vide, distant ou porteur d'un caractère interdit — \
                 repli sur le chemin d'origine",
            );
            return None;
        }
    };
    if let Err(e) = imp::prepare_dir(&dir) {
        trace_once(&format!("{e} — repli sur le chemin d'origine"));
        return None;
    }
    let dst = dir.join(src.file_name()?);
    if needs_copy_core(stamp(src), stamp(&dst)) {
        if same_binary(src, &dst) {
            align_mtime(src, &dst);
        } else if let Err(e) = copy_atomic(src, &dst) {
            // Décision Steven (revue 10.3) : un miroir PÉRIMÉ mais présent l'emporte sur un repli
            // hors invariant. `ERROR_SHARING_VIOLATION` — Windows refusant d'écraser un exe en
            // cours d'exécution — est le cas NOMINAL juste après une mise à jour, pendant qu'un
            // hook est en vol ; y répondre par le chemin `C:\Users\John Doe\…` renverrait
            // Antigravity, Grok et Codex au fail-open que cette story referme. On dégrade la
            // VERSION du hook, jamais l'invariant ; la fraîcheur (AC4) rattrape au passage suivant.
            if dst.is_file() {
                trace_once(&format!(
                    "copie de {} impossible : {e} — miroir CONSERVÉ en version périmée",
                    dst.display()
                ));
                return Some(dst);
            }
            trace_once(&format!(
                "copie de {} vers {} impossible : {e} — repli sur le chemin d'origine",
                src.display(),
                dst.display()
            ));
            return None;
        }
    }
    // ⚠️ Ne JAMAIS rendre un chemin qui ne porte pas de fichier (revue 10.3). `needs_copy_core`
    // rend `false` quand `stamp(src)` échoue — mtime antérieur à 1970 après restauration, ou mise
    // en quarantaine entre `is_file()` et `stamp()` : la copie était alors sautée et le chemin
    // rendu quand même. Les cinq configs pointaient vers un fichier inexistant (hooks morts en
    // silence) et le repli n'était jamais emprunté, contrairement à ce qu'annonçait le commentaire
    // « le repli tranchera ».
    if !dst.is_file() {
        trace_once(&format!(
            "miroir {} absent après tentative de copie — repli sur le chemin d'origine",
            dst.display()
        ));
        return None;
    }
    Some(dst)
}

/// Une seule trace par message et par processus : `ensure_mirrored` est appelé à chaque contrôle de
/// conformité, pour les cinq outils — un poste en repli permanent noierait sinon la sortie d'erreur.
fn trace_once(message: &str) {
    static SEEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let mut guard = match SEEN.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let seen = guard.get_or_insert_with(HashSet::new);
    if seen.insert(message.to_string()) {
        trace(message);
    }
}

/// `true` si le miroir de `src` est absent ou a divergé du binaire source (AC4).
///
/// Lecture SEULE : appelé par la conformité AVANT toute résolution de chemin, sinon la réparation
/// implicite de `ensure_mirrored` masquerait la dérive qu'on cherche justement à révéler.
pub(crate) fn is_stale(src: &Path) -> bool {
    // Binaire source absent : rien à comparer, donc rien à déclarer périmé. Sans cette sortie, un
    // outil non déployé rendrait la conformité fausse en permanence → auto-repair et notification à
    // CHAQUE démarrage.
    if !src.is_file() {
        return false;
    }
    let target = mirror_target(src);
    let exists = target.as_ref().map(|t| t.is_file());
    let same = match (&target, exists) {
        (Some(t), Some(true)) => same_binary(src, t),
        _ => false,
    };
    stale_core(mirror_failed_for(src), exists, same)
}

/// Amorce le miroir de `src` — à appeler UNE FOIS au démarrage, avant la première évaluation de
/// conformité (revue 10.3, décision Steven).
///
/// Referme deux défauts d'un seul geste :
///   1. Après chaque **mise à jour** de Vigil, le miroir porte encore l'ancienne version. La
///      conformité le voyait périmé, `auto_repair_hooks` réinstallait, et l'utilisateur recevait une
///      notification « Hooks repaired for X » par outil — le comportement était juste, le message
///      annonçait une compromission là où il n'y avait qu'une maintenance.
///   2. `mirror_is_stale_for_tool` est évalué AVANT tout `get_vigil_*_exe`, donc avant la moindre
///      tentative de miroir : à chaque démarrage `MIRROR_FAILED` était encore vide, et sur un poste
///      où le miroir est définitivement impossible le premier outil évalué était réparé et notifié à
///      chaque lancement, indéfiniment — ce que la doc du drapeau affirme empêcher.
///
/// L'ordre « fraîcheur avant résolution » de la T3 reste intact : on amorce **hors** du prédicat,
/// pas à l'intérieur.
pub(crate) fn prime(src: &Path) {
    let _ = ensure_mirrored(src);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Cœurs purs : racine du miroir ─────────────────────────────────────────

    #[test]
    fn racine_miroir_sur_program_data_nominal() {
        assert_eq!(
            mirror_root_from(Some(r"C:\ProgramData")),
            Some(PathBuf::from(r"C:\ProgramData\Vigil"))
        );
    }

    #[test]
    fn racine_miroir_refusee_si_program_data_porte_une_espace() {
        // Toute la story consiste à ne pas SUPPOSER l'absence d'espace : `%ProgramData%` est
        // théoriquement relocalisable, on vérifie.
        assert_eq!(mirror_root_from(Some(r"D:\Program Data")), None);
    }

    #[test]
    fn racine_miroir_refusee_si_variable_absente_ou_vide() {
        assert_eq!(mirror_root_from(None), None);
        assert_eq!(mirror_root_from(Some("")), None);
        assert_eq!(mirror_root_from(Some("   ")), None);
    }

    #[test]
    fn racine_miroir_refusee_sur_metacaractere_cmd_ou_chemin_distant() {
        // Revue 10.3 — la garde d'origine ne rejetait que `U+0020`. Or Antigravity et Kimi
        // reçoivent le chemin dans `cmd /d /s /c` SANS aucun quoting : `&` coupe la commande,
        // exactement la classe de fail-open silencieux que cette story referme.
        for interdit in [
            r"D:\Data&Apps",
            r"D:\Data^Apps",
            r"D:\Data(1)",
            r"D:\Data%TEMP%",
            "D:\\Data\tApps",
            "D:\\Data\u{a0}Apps", // espace insécable
        ] {
            assert_eq!(mirror_root_from(Some(interdit)), None, "doit refuser `{interdit}`");
        }
        // Un `%ProgramData%` redirigé en UNC ferait porter aux cinq configs un chemin d'exe
        // DISTANT : hors ligne, tous les hooks meurent alors que la config est déjà posée.
        assert_eq!(mirror_root_from(Some(r"\\serveur\partage\ProgramData")), None);
        assert_eq!(mirror_root_from(Some(r"ProgramData")), None, "chemin relatif");
        // Le cas nominal reste accepté, y compris sur une autre lettre de lecteur.
        assert!(mirror_root_from(Some(r"D:\ProgramData")).is_some());
    }

    #[test]
    fn dossier_miroir_refuse_un_sid_degrade() {
        let root = PathBuf::from(r"C:\ProgramData\Vigil");
        for interdit in ["S-1-5 21", "S-1-5&21", "S-1-5\t21", ""] {
            assert_eq!(mirror_dir_from(&root, interdit), None, "doit refuser `{interdit}`");
        }
    }

    #[test]
    fn dossier_miroir_par_sid() {
        let root = PathBuf::from(r"C:\ProgramData\Vigil");
        assert_eq!(
            mirror_dir_from(&root, "S-1-5-21-1-2-3-1001"),
            Some(PathBuf::from(r"C:\ProgramData\Vigil\hooks\S-1-5-21-1-2-3-1001"))
        );
        assert_eq!(mirror_dir_from(&root, ""), None);
        assert_eq!(mirror_dir_from(&root, "S-1-5 21"), None);
    }

    // ── Cœur pur : cascade de résolution (AC1, AC5, AC8) ──────────────────────

    const SRC_A_ESPACE: &str = r"C:\Users\John Doe\AppData\Local\vigil\vigil-claude.exe";
    const MIROIR: &str = r"C:\ProgramData\Vigil\hooks\S-1-5-21-1-2-3-1001\vigil-claude.exe";

    #[test]
    fn cascade_prefere_le_miroir_meme_quand_la_83_est_disponible() {
        let cmd = hook_command_core(
            Path::new(SRC_A_ESPACE),
            Some(Path::new(MIROIR)),
            Some(r"C:\Users\JOHNDO~1\AppData\Local\vigil\VIGIL-~1.EXE"),
            false,
        );
        assert_eq!(cmd, MIROIR);
    }

    #[test]
    fn cascade_repli_sur_la_83_puis_sur_le_chemin_long() {
        let court = r"C:\Users\JOHNDO~1\AppData\Local\vigil\VIGIL-~1.EXE";
        assert_eq!(
            hook_command_core(Path::new(SRC_A_ESPACE), None, Some(court), false),
            court
        );
        assert_eq!(
            hook_command_core(Path::new(SRC_A_ESPACE), None, None, false),
            SRC_A_ESPACE
        );
    }

    #[test]
    fn cascade_conserve_la_convention_decriture_de_chaque_outil() {
        // Claude/Codex/Grok passent par bash → forward-slashes ; Antigravity/Kimi par `cmd` →
        // backslashes natifs. Le miroir change le DOSSIER, jamais la convention.
        assert_eq!(
            hook_command_core(Path::new(SRC_A_ESPACE), Some(Path::new(MIROIR)), None, true),
            MIROIR.replace('\\', "/")
        );
        assert_eq!(
            hook_command_core(Path::new(SRC_A_ESPACE), Some(Path::new(MIROIR)), None, false),
            MIROIR
        );
    }

    #[test]
    fn commande_miroir_sans_espace_ni_guillemet_dans_les_deux_conventions() {
        for bash in [true, false] {
            let cmd =
                hook_command_core(Path::new(SRC_A_ESPACE), Some(Path::new(MIROIR)), None, bash);
            assert!(!cmd.contains(' '), "espace résiduelle dans `{cmd}`");
            assert!(!cmd.contains('"'), "guillemet dans `{cmd}`");
        }
    }

    // ── Cœur pur : fraîcheur (AC4) ────────────────────────────────────────────

    #[test]
    fn recopie_si_le_miroir_est_absent_ou_de_taille_ou_de_date_differente() {
        assert!(needs_copy_core(Some((10, 100)), None));
        assert!(needs_copy_core(Some((10, 100)), Some((11, 100))));
        assert!(needs_copy_core(Some((10, 100)), Some((10, 101))));
        assert!(!needs_copy_core(Some((10, 100)), Some((10, 100))));
    }

    #[test]
    fn source_illisible_ne_declenche_aucune_recopie() {
        assert!(!needs_copy_core(None, None));
        assert!(!needs_copy_core(None, Some((10, 100))));
    }

    #[test]
    fn conformite_fausse_quand_le_miroir_est_absent_ou_different() {
        assert!(stale_core(false, Some(false), false), "miroir absent → non conforme");
        assert!(stale_core(false, Some(true), false), "miroir différent → non conforme");
        assert!(!stale_core(false, Some(true), true), "miroir à jour → conforme");
    }

    #[test]
    fn le_drapeau_dechec_est_par_binaire_et_jamais_global() {
        // Revue 10.3 — LE défaut que les trois couches de revue ont trouvé indépendamment. Un
        // `AtomicBool` unique pour cinq binaires laissait l'échec de l'un désarmer la détection de
        // dérive des QUATRE AUTRES, de façon dépendante de l'ordre d'évaluation. C'est exactement le
        // motif `transcript_hash` que l'AC4 dit rendre structurel.
        let kimi = PathBuf::from(r"C:\vigil\vigil-kimi.exe");
        let claude = PathBuf::from(r"C:\vigil\vigil-claude.exe");

        set_mirror_failed(&kimi, true);
        assert!(mirror_failed_for(&kimi));
        assert!(
            !mirror_failed_for(&claude),
            "l'échec de Kimi ne doit RIEN dire du miroir de Claude"
        );

        // Et le rattrapage reste possible sans redémarrage (condition transitoire).
        set_mirror_failed(&kimi, false);
        assert!(!mirror_failed_for(&kimi));
    }

    #[test]
    fn conformite_ne_boucle_pas_quand_le_miroir_est_hors_datteinte() {
        // Le garde-fou anti-boucle : sur un poste où le miroir échoue toujours, la conformité doit
        // se stabiliser sur le chemin de repli, sinon l'auto-repair réécrirait les cinq configs à
        // chaque démarrage et l'interface signalerait un défaut permanent.
        assert!(!stale_core(true, Some(false), false));
        assert!(!stale_core(true, None, false));
        assert!(!stale_core(false, None, false), "pas de chemin miroir → repli assumé");
    }

    // ── Intégration Windows : création, ACL, copie ────────────────────────────

    #[cfg(target_os = "windows")]
    mod windows_integration {
        use super::*;

        /// SID de `BUILTIN\Utilisateurs` — **en dur**, car les noms de groupes intégrés sont
        /// localisés (`BUILTIN\Users` en anglais, `BUILTIN\Utilisateurs` en français).
        const BUILTIN_USERS: &str = "S-1-5-32-545";
        /// `NT AUTHORITY\SYSTEM`.
        const LOCAL_SYSTEM: &str = "S-1-5-18";
        /// `BUILTIN\Administrateurs`.
        const BUILTIN_ADMINS: &str = "S-1-5-32-544";
        /// `FILE_ALL_ACCESS`.
        const FULL: u32 = 0x001F_01FF;
        /// `OBJECT_INHERIT_ACE` — l'ACE retombe sur les **fichiers** créés en dessous.
        const OI: u8 = 0x01;
        /// `CONTAINER_INHERIT_ACE` — l'ACE retombe sur les **dossiers** créés en dessous.
        const CI: u8 = 0x02;
        /// Héritage complet (dossiers + fichiers).
        const CI_OI: u8 = CI | OI;

        fn banc(nom: &str) -> PathBuf {
            let dir = std::env::temp_dir()
                .join(format!("vigil-miroir-{}-{}", nom, std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            dir
        }

        #[test]
        fn le_sid_courant_se_resout_et_ne_contient_pas_despace() {
            let sid = imp::current_sid_string().expect("le SID du compte courant doit se résoudre");
            assert!(sid.starts_with("S-1-"), "forme de SID inattendue : {sid}");
            assert!(!sid.contains(' '));
        }

        #[test]
        fn dossier_durci_heritage_rompu_et_aucun_droit_pour_les_utilisateurs() {
            let racine = banc("acl");
            let dir = racine.join("hooks").join("S-1-5-21-0-0-0-1001");
            imp::prepare_dir(&dir).expect("le durcissement doit passer SANS élévation");

            let (protege, aces, autres) =
                imp::dacl_snapshot(&dir).expect("la DACL du dossier doit être lisible");
            assert!(protege, "l'héritage doit être rompu (SE_DACL_PROTECTED)");
            assert_eq!(autres, 0, "aucune ACE d'un type autre qu'AUTORISATION n'est attendue");
            assert!(
                !aces.iter().any(|(sid, _, _)| sid == BUILTIN_USERS),
                "aucune ACE `BUILTIN\\Utilisateurs` ne doit subsister : {aces:?}"
            );

            // ⚠️ Revue 10.3 — on nomme les trois SID, leurs masques ET leurs drapeaux d'héritage.
            // `aces.len() == 3` seul laissait passer un `S-1-1-0` (Tout le monde) en contrôle total
            // posé à la place des Administrateurs : trois ACE, test vert, et le dossier ouvert à
            // tous. Les drapeaux comptent autant : c'est par eux qu'un droit se propage aux
            // binaires déposés dans le `<SID>`.
            let moi = imp::current_sid_string().expect("SID du compte courant");
            let mut attendu = vec![
                (moi.to_string(), FULL, CI_OI),
                (LOCAL_SYSTEM.to_string(), FULL, CI_OI),
                (BUILTIN_ADMINS.to_string(), FULL, CI_OI),
            ];
            let mut obtenu = aces.clone();
            attendu.sort();
            obtenu.sort();
            assert_eq!(
                obtenu, attendu,
                "DACL du <SID> attendue = propriétaire + SYSTEM + Administrateurs en contrôle total, \
                 et RIEN d'autre"
            );

            let _ = std::fs::remove_dir_all(&racine);
        }

        #[test]
        fn le_parent_reste_franchissable_pour_un_second_compte() {
            // Topologie retenue (gate T0c) : le parent laisse tout utilisateur CRÉER son propre
            // `<SID>` — sans quoi un second compte Windows serait renvoyé au repli, donc au
            // fail-open d'origine. Il n'accorde jamais l'écriture de fichiers.
            let racine = banc("parent");
            let dir = racine.join("hooks").join("S-1-5-21-0-0-0-1001");
            imp::prepare_dir(&dir).unwrap();

            let (_, aces, autres) = imp::dacl_snapshot(&racine.join("hooks")).unwrap();
            assert_eq!(autres, 0, "aucune ACE d'un type autre qu'AUTORISATION n'est attendue");
            let users = aces
                .iter()
                .find(|(sid, _, _)| sid == BUILTIN_USERS)
                .expect("le parent doit rester franchissable par les utilisateurs");
            assert_eq!(users.1 & 0x4, 0x4, "création de sous-dossier attendue");
            assert_eq!(users.1 & 0x2, 0, "aucune création de FICHIER au niveau parent");
            assert_eq!(users.1 & 0x40, 0, "aucun droit de suppression d'enfant");

            // ⚠️ 2ᵉ passe de revue 10.3 — LE verrou de `inherit_containers_only`, et la seule
            // assertion de tout le module qui porte sur l'héritage. Le masque ci-dessus se lit
            // comme un masque de DOSSIER : le bit 0x4 y vaut `FILE_ADD_SUBDIRECTORY`. Sur un
            // objet FICHIER, ce même bit vaut `FILE_APPEND_DATA`. Laisser `OBJECT_INHERIT_ACE`
            // ici propagerait donc aux fichiers hérités un droit d'écriture que les trois
            // assertions précédentes croient avoir refusé — elles ne peuvent pas le voir, car un
            // masque ne dit rien de sa propre portée.
            assert_eq!(
                users.2 & OI,
                0,
                "l'ACE `Utilisateurs` du parent ne doit JAMAIS porter OBJECT_INHERIT_ACE \
                 (0x4 y deviendrait FILE_APPEND_DATA) : drapeaux = {:#04x}",
                users.2
            );
            assert_eq!(
                users.2 & CI,
                CI,
                "…mais elle DOIT porter CONTAINER_INHERIT_ACE, sans quoi un second compte ne peut \
                 pas créer son propre <SID> : drapeaux = {:#04x}",
                users.2
            );

            // Revue 10.3 — les TROIS autres ACE sont nommées aussi : le test ne regardait que celle
            // des utilisateurs, donc n'aurait pas vu un tiers ajouté en contrôle total.
            let moi = imp::current_sid_string().expect("SID du compte courant");
            let pleins: Vec<&String> = aces
                .iter()
                .filter(|(_, mask, _)| *mask == FULL)
                .map(|(sid, _, _)| sid)
                .collect();
            for attendu in [moi, LOCAL_SYSTEM, BUILTIN_ADMINS] {
                assert!(pleins.iter().any(|s| s.as_str() == attendu), "{attendu} attendu : {aces:?}");
            }
            assert_eq!(
                pleins.len(),
                3,
                "seuls le créateur, SYSTEM et les Administrateurs ont le contrôle total : {aces:?}"
            );
            assert_eq!(aces.len(), 4, "les 3 pleins + les utilisateurs, rien d'autre : {aces:?}");

            let _ = std::fs::remove_dir_all(&racine);
        }

        #[test]
        fn arborescence_miroir_supprimable_par_son_createur() {
            // AC7 — le désinstalleur tourne sous le compte qui a posé le miroir. Une DACL protégée
            // qui omettrait le créateur rendrait les dossiers INDÉRACINABLES : un propriétaire NTFS
            // n'hérite que de `READ_CONTROL`/`WRITE_DAC`, jamais de `DELETE`. Constaté pour de vrai
            // au nettoyage des bancs de test — d'où ce verrou.
            let racine = banc("suppression");
            let dir = racine.join("hooks").join("S-1-5-21-0-0-0-1001");
            imp::prepare_dir(&dir).unwrap();
            std::fs::write(dir.join("vigil-faux.exe"), b"v1").unwrap();

            std::fs::remove_dir_all(&dir).expect("le dossier <SID> doit être supprimable");
            std::fs::remove_dir(racine.join("hooks")).expect("`hooks` doit être supprimable");
            std::fs::remove_dir(&racine).expect("la racine doit être supprimable");
            assert!(!racine.exists());
        }

        #[test]
        fn dossier_preexistant_mal_permissionne_est_corrige() {
            // Un dossier créé sans nous hérite des ACE permissives de son parent. Le durcissement
            // est réappliqué à CHAQUE passage, pas seulement à la création.
            let racine = banc("repermission");
            let dir = racine.join("hooks").join("S-1-5-21-0-0-0-1001");
            std::fs::create_dir_all(&dir).unwrap();
            imp::prepare_dir(&dir).unwrap();

            let (protege, aces, autres) = imp::dacl_snapshot(&dir).unwrap();
            assert!(protege);
            assert_eq!(autres, 0);
            assert!(!aces.iter().any(|(sid, _, _)| sid == BUILTIN_USERS));

            let _ = std::fs::remove_dir_all(&racine);
        }

        #[test]
        fn jonction_a_la_place_du_dossier_sid_est_refusee() {
            // ⚠️ Revue 10.3 — le constat de tête. `Path::exists()` et `SetNamedSecurityInfoW` suivent
            // tous deux les points d'analyse : sans garde, un compte tiers pouvait poser une
            // JONCTION à la place de notre `<SID>` vers un dossier qu'il possède, faire réussir le
            // durcissement SUR LA CIBLE, et obtenir que Vigil copie puis exécute automatiquement un
            // binaire dans un dossier où il conserve `WRITE_DAC` à vie. L'issue devenait PIRE que le
            // squat accepté : au lieu d'un repli, une exécution.
            let base = banc("jonction");
            let cible = base.join("cible");
            std::fs::create_dir_all(&cible).unwrap();
            let dir = base.join("hooks").join("S-1-5-21-0-0-0-1001");
            std::fs::create_dir_all(dir.parent().unwrap()).unwrap();

            // `mklink /J` : une jonction ne demande AUCUN privilège — c'est tout le problème.
            let ok = std::process::Command::new("cmd")
                .args(["/d", "/s", "/c", "mklink", "/J"])
                .arg(&dir)
                .arg(&cible)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            assert!(ok, "la jonction de test n'a pas pu être créée");

            let err = imp::prepare_dir(&dir)
                .expect_err("une jonction à la place du <SID> DOIT être refusée");
            assert!(
                err.contains("point d'analyse"),
                "le motif du refus doit nommer le point d'analyse : {err}"
            );
            // Et la cible n'a PAS été durcie au passage.
            let (protege, _, _) = imp::dacl_snapshot(&cible).unwrap();
            assert!(!protege, "la DACL de la cible ne doit pas avoir été touchée");

            let _ = std::fs::remove_dir(&dir);
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn fichier_a_la_place_du_dossier_sid_est_refuse() {
            let base = banc("fichier");
            let dir = base.join("hooks").join("S-1-5-21-0-0-0-1001");
            std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
            std::fs::write(&dir, b"pas un dossier").unwrap();

            let err = imp::prepare_dir(&dir).expect_err("un fichier à la place du <SID> est refusé");
            assert!(err.contains("n'est pas un dossier"), "{err}");

            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn copie_atomique_ne_laisse_aucun_residu_ni_fichier_partiel() {
            // La copie passe désormais par un temporaire renommé : `fs::copy` écrivait EN PLACE, et
            // une interruption laissait un exécutable TRONQUÉ au chemin déjà inscrit dans les cinq
            // configs.
            let base = banc("atomique");
            std::fs::create_dir_all(&base).unwrap();
            let src = base.join("source.exe");
            let dst = base.join("vigil-faux.exe");
            std::fs::write(&src, b"contenu-v1").unwrap();

            copy_atomic(&src, &dst).expect("la copie doit passer");
            assert_eq!(std::fs::read(&dst).unwrap(), b"contenu-v1");
            assert!(
                !dst.with_extension("vigil-tmp").exists(),
                "le temporaire ne doit jamais survivre à une copie réussie"
            );

            // Source absente : échec propre, cible d'origine INTACTE, aucun résidu.
            let fantome = base.join("absent.exe");
            assert!(copy_atomic(&fantome, &dst).is_err());
            assert_eq!(std::fs::read(&dst).unwrap(), b"contenu-v1", "la cible ne doit pas être touchée");
            assert!(!dst.with_extension("vigil-tmp").exists());

            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn copie_puis_idempotence_de_la_fraicheur() {
            let base = banc("copie");
            // ⚠️ La source vit HORS de l'arborescence miroir : durcir la racine re-propage
            // l'héritage à toute sa descendance, et le compte courant n'y garde alors que le droit
            // de lecture accordé à `BUILTIN\Utilisateurs`. En production `%ProgramData%\Vigil`
            // n'abrite que nos binaires, mais le banc de test doit rester réinscriptible.
            let src_dir = base.join("source avec espace");
            std::fs::create_dir_all(&src_dir).unwrap();
            let src = src_dir.join("vigil-faux.exe");
            std::fs::write(&src, b"v1").unwrap();

            let racine = base.join("miroir");
            let dir = racine.join("hooks").join("S-1-5-21-0-0-0-1001");
            imp::prepare_dir(&dir).unwrap();
            let dst = dir.join("vigil-faux.exe");

            assert!(needs_copy_core(stamp(&src), stamp(&dst)), "miroir absent → à copier");
            std::fs::copy(&src, &dst).expect("la copie dans le dossier durci doit passer");
            assert!(
                !needs_copy_core(stamp(&src), stamp(&dst)),
                "après copie, taille et mtime doivent coïncider (CopyFileExW préserve la date)"
            );
            assert!(same_binary(&src, &dst));

            // Dérive de version : le miroir doit redevenir « à recopier ».
            std::fs::write(&src, b"v2-plus-long").unwrap();
            assert!(needs_copy_core(stamp(&src), stamp(&dst)));
            assert!(!same_binary(&src, &dst));

            let _ = std::fs::remove_dir_all(&base);
        }
    }
}



