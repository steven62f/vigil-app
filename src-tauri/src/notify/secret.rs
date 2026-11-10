//! Rangement du secret d'un canal de notification dans le gestionnaire d'identifiants de l'OS.
//!
//! Wrapper VOLONTAIREMENT mince et **agnostique du canal** : le nom de compte est fourni par
//! l'implémentation de canal (cf. `ActiveChannel::secret_account`), jamais codé ici. Aucun
//! secret ne transite donc par `vigil-settings.json` — la structure `NotifSettings` n'a même
//! pas de champ pour ça (test `token_stocke_hors_config_claire`).
//!
//! Toutes les erreurs sont des `String` (patron du projet) et **rien ne panique** si le coffre
//! est indisponible : sur un poste où le Credential Manager est verrouillé par stratégie, la
//! lecture rend `None` → le manager court-circuite l'envoi, l'app continue.

use keyring::Entry;

/// Service tel qu'il apparaît dans le Gestionnaire d'identifiants Windows.
const SERVICE: &str = "vigil";

fn entry(account: &str) -> Result<Entry, String> {
    Entry::new(SERVICE, account).map_err(|e| format!("keyring_error: {e}"))
}

pub fn set_token(account: &str, token: &str) -> Result<(), String> {
    entry(account)?
        .set_password(token)
        .map_err(|e| format!("keyring_error: {e}"))
}

/// `None` = pas de secret **ou** coffre inaccessible : les deux cas se traitent pareil côté
/// appelant (aucun envoi possible), inutile de les distinguer.
pub fn get_token(account: &str) -> Option<String> {
    entry(account).ok()?.get_password().ok()
}

/// Suppression idempotente : l'absence d'entrée n'est PAS une erreur (« Déconnecter » appuyé
/// deux fois, ou secret déjà retiré à la main du gestionnaire d'identifiants).
/// ⚠️ API v3 : la méthode s'appelle `delete_credential` (le `delete_password` de la v2 n'existe
/// plus).
pub fn delete_token(account: &str) -> Result<(), String> {
    match entry(account)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("keyring_error: {e}")),
    }
}
