//! Implémentation Telegram du trait `NotificationChannel`.
//!
//! **Tout** le vocabulaire propre à ce canal est confiné ici : URL d'API, forme du jeton,
//! identifiant de destinataire, format des erreurs, liens de l'assistant d'appairage. Le reste du
//! code (manager, pipe, commandes, front) ne connaît que `ActiveChannel` / `NotificationChannel`
//! → brancher Discord = un fichier voisin + une variante d'enum.
//!
//! ## Modèle BYO (« bring your own bot »)
//!
//! L'utilisateur crée SON bot via @BotFather ; le jeton reste sur SA machine (coffre de l'OS) et
//! Vigil parle **directement** à l'API Telegram. Aucun serveur intermédiaire, donc aucune
//! infrastructure à payer — c'est la contrainte dure de l'Epic 8.
//!
//! ## Parsing pur, réseau à part
//!
//! Les fonctions de décodage (`parse_get_me`, `extract_chat_id`, `parse_api_error`) sont **pures**
//! et testées sans réseau (patron `usage/fetcher.rs`) ; les fonctions réseau ne font que
//! transporter.

use std::time::Duration;

use super::{
    ActionCursor, ActionKeyboard, ActionRef, ChannelAction, ChannelIdentity, IncomingKind,
    MessageRef, MobileNotif, NotificationChannel,
};

/// Normalise une erreur de transport **sans jamais laisser filtrer l'URL**.
///
/// ⚠️ Le `Display` de `reqwest::Error` termine par ` for url ({url})` — et l'URL de ce canal
/// **porte le jeton du bot** (`api_url`). Formater l'erreur telle quelle ferait donc traverser le
/// secret jusqu'à `last_error`, jusqu'à l'événement `notify:state` poussé au webview et jusqu'à la
/// valeur de retour des commandes d'appairage. `without_url()` le retire à la source.
fn transport_error(prefix: &str, e: reqwest::Error) -> String {
    format!("{prefix}: {}", e.without_url())
}

/// Compte sous lequel le jeton du bot est rangé dans le gestionnaire d'identifiants de l'OS
/// (service `vigil`). Nommé ici, pas dans `secret.rs`, qui reste agnostique du canal.
pub const SECRET_ACCOUNT: &str = "notify-telegram-token";

const API_BASE: &str = "https://api.telegram.org";

/// Borne dure des appels courts. `reqwest::Client` n'a **aucun** timeout par défaut : sans ça, un
/// proxy pendu figerait la tâche d'envoi indéfiniment (leçon `usage/fetcher.rs`).
const HTTP_TIMEOUT_SECS: u64 = 10;

/// Durée d'attente demandée à Telegram pour UN tour de long-poll d'appairage.
const PAIRING_POLL_SECS: u64 = 25;
/// Nombre de tours → ≈60 s d'attente totale, puis abandon (`pairing_timeout`).
const PAIRING_ROUNDS: u32 = 3;
/// Le timeout HTTP d'un tour de long-poll doit dépasser la durée demandée au serveur, sinon on
/// couperait systématiquement la connexion avant qu'il ne réponde.
const PAIRING_HTTP_TIMEOUT_SECS: u64 = PAIRING_POLL_SECS + 10;

/// Durée d'attente demandée pour UN tour de la boucle retour (Story 8.3). Même patron que
/// l'appairage : le timeout HTTP doit dépasser la durée demandée au serveur, sinon on couperait
/// la connexion avant qu'il ne réponde.
const ACTION_POLL_SECS: u64 = 25;
const ACTION_HTTP_TIMEOUT_SECS: u64 = ACTION_POLL_SECS + 10;

pub struct TelegramChannel {
    token: String,
    /// Destinataire capté à l'appairage. Vide tant que le canal ne sert qu'à valider un jeton
    /// candidat (`ActiveChannel::for_candidate_token`).
    chat_id: String,
}

impl TelegramChannel {
    pub fn new(token: String, chat_id: String) -> Self {
        Self { token, chat_id }
    }

    fn url(&self, method: &str) -> String {
        api_url(&self.token, method)
    }
}

impl NotificationChannel for TelegramChannel {
    async fn send(&self, notif: &MobileNotif) -> Result<Option<MessageRef>, String> {
        // ⚠️ PAS de `parse_mode` : le message part en texte brut. Un résumé contient des
        // caractères de balisage (`_`, `*`, `` ` ``, `[`) qui feraient échouer le parsing
        // MarkdownV2 (400 → notification perdue) et ouvriraient une injection de balisage depuis
        // un simple nom de fichier.
        let body = send_body(&self.chat_id, notif);
        let resp = client()
            .post(self.url("sendMessage"))
            .json(&body)
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|e| transport_error("network_error", e))?;

        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(parse_api_error(status, &text));
        }
        // Un 2xx ne suffit PAS : Telegram peut répondre 200 avec `{"ok":false}`, et un portail
        // captif ou un proxy d'entreprise renvoie volontiers un 200 qui n'est pas une réponse
        // d'API. Sans cette vérification, la notification serait perdue en silence pendant que
        // `last_error` est effacé et que l'UI affiche un canal sain.
        check_ok(&text)?;
        Ok(parse_sent_message(&text))
    }

    async fn validate(&self) -> Result<ChannelIdentity, String> {
        let resp = client()
            .get(self.url("getMe"))
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|e| transport_error("network_error", e))?;

        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| transport_error("read_error", e))?;
        if !(200..300).contains(&status) {
            return Err(parse_api_error(status, &text));
        }
        parse_get_me(&text)
    }

    /// Un tour de long-poll de la boucle retour.
    ///
    /// ⚠️ Contrairement à `pair()`, cette boucle **confirme un offset** — sans quoi elle
    /// retraiterait le même tap indéfiniment. Confirmer un offset consomme aussi les `message` en
    /// attente : d'où `allowed_updates=["callback_query"]`, qui laisse intacts les messages dont
    /// la **Story 8.6** (réponse texte libre, différée) aura besoin. C'est aussi pourquoi cette
    /// boucle et l'appairage
    /// ne doivent jamais tourner ensemble (garde explicite côté manager, + `409 Conflict` sinon).
    async fn poll_actions(&self, cursor: &mut ActionCursor) -> Result<Vec<ChannelAction>, String> {
        let mut query = vec![
            ("timeout", ACTION_POLL_SECS.to_string()),
            ("allowed_updates", r#"["callback_query"]"#.to_string()),
        ];
        if let Some(offset) = cursor.get() {
            query.push(("offset", offset.to_string()));
        }
        let resp = client()
            .get(self.url("getUpdates"))
            .query(&query)
            .timeout(Duration::from_secs(ACTION_HTTP_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|e| transport_error("network_error", e))?;

        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| transport_error("read_error", e))?;
        if !(200..300).contains(&status) {
            return Err(parse_api_error(status, &text));
        }
        let (actions, last_seen) = parse_action_updates(&text);
        // L'offset avance même sur un update NON décodable : sinon un update parasite (autre type,
        // charge utile étrangère) bloquerait la boucle sur lui pour toujours.
        if let Some(last) = last_seen {
            cursor.set(last + 1);
        }
        Ok(actions)
    }

    async fn ack_action(&self, action: &ActionRef, text: &str) -> Result<(), String> {
        let body = serde_json::json!({
            "callback_query_id": action.as_str(),
            "text": text,
        });
        let resp = client()
            .post(self.url("answerCallbackQuery"))
            .json(&body)
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|e| transport_error("network_error", e))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(parse_api_error(status, &body));
        }
        check_ok(&body)
    }

    async fn disarm(&self, message: &MessageRef, text: &str) -> Result<(), String> {
        // `message_id` est un entier côté API : la référence opaque le transporte en chaîne pour
        // que personne d'autre n'ait à connaître son type.
        let message_id: i64 = message.as_str().parse().map_err(|_| "invalid_message_ref".to_string())?;
        let body = serde_json::json!({
            "chat_id": self.chat_id,
            "message_id": message_id,
            "text": text,
            // Clavier VIDE = retrait des boutons. Un second tap devient impossible côté client
            // (l'anti-rejeu du manager reste néanmoins la garantie de correction).
            "reply_markup": { "inline_keyboard": [] },
        });
        let resp = client()
            .post(self.url("editMessageText"))
            .json(&body)
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|e| transport_error("network_error", e))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(parse_api_error(status, &body));
        }
        check_ok(&body)
    }

    /// Ré-édition du clavier seul (bascule multi-choix 8.4) — le corps du message reste intact.
    async fn update_actions(
        &self,
        message: &MessageRef,
        keyboard: &ActionKeyboard,
    ) -> Result<(), String> {
        let message_id: i64 = message
            .as_str()
            .parse()
            .map_err(|_| "invalid_message_ref".to_string())?;
        let body = serde_json::json!({
            "chat_id": self.chat_id,
            "message_id": message_id,
            "reply_markup": encode_keyboard(keyboard),
        });
        let resp = client()
            .post(self.url("editMessageReplyMarkup"))
            .json(&body)
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|e| transport_error("network_error", e))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(parse_api_error(status, &body));
        }
        check_ok(&body)
    }
}

/// Appairage : attend que l'utilisateur envoie `/start` à son bot et rend l'identifiant de
/// conversation.
///
/// Long-poll **borné** (≈60 s), pas une boucle de polling permanente : le backoff / offset disque
/// / déduplication sont la Story 8.5. Ici on lit les updates **sans confirmer d'offset** —
/// Telegram les conserve 24 h, donc un `/start` tapé AVANT l'ouverture de l'assistant est bien vu,
/// et rien n'est consommé destructivement pour les stories suivantes.
///
/// ⚠️ Opération **exclusive** : deux `getUpdates` concurrents sur le même bot donnent un
/// `409 Conflict` côté Telegram. L'UI désactive le bouton pendant l'attente.
pub async fn pair(token: &str) -> Result<PairedRecipient, String> {
    for _ in 0..PAIRING_ROUNDS {
        let resp = client()
            .get(api_url(token, "getUpdates"))
            .query(&[
                ("timeout", PAIRING_POLL_SECS.to_string()),
                // Sans ce filtre, `getUpdates` rend IMMÉDIATEMENT dès qu'un update quelconque
                // traîne dans la file de 24 h (`my_chat_member` après un Start/Block,
                // `edited_message`, post de canal…). Comme aucun offset n'est confirmé, les 3
                // tours reviennent alors instantanément et l'appairage échoue en quelques
                // centaines de ms — de façon reproductible pendant 24 h — pendant que l'UI
                // annonce « ~1 min ». En restreignant aux messages, l'attente de 25 s a
                // réellement lieu.
                ("allowed_updates", r#"["message"]"#.to_string()),
            ])
            .timeout(Duration::from_secs(PAIRING_HTTP_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|e| transport_error("network_error", e))?;

        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| transport_error("read_error", e))?;
        if !(200..300).contains(&status) {
            // Un 5xx est typiquement transitoire sur un long-poll (Telegram rend volontiers des
            // 502/504) : il ne doit coûter QUE le tour en cours, pas l'appairage entier.
            if (500..600).contains(&status) {
                continue;
            }
            return Err(parse_api_error(status, &text));
        }
        if let Some(recipient) = extract_recipient(&text) {
            return Ok(recipient);
        }
    }
    Err("pairing_timeout".to_string())
}

/// URL à ouvrir dans le navigateur pour une étape de l'assistant. `None` si l'étape demandée n'a
/// pas de lien possible (« ouvrir mon bot » sans bot connu) — l'appelant remonte une erreur claire
/// plutôt que d'ouvrir une URL bancale.
pub fn link_url(kind: &str, bot_username: Option<&str>) -> Option<String> {
    match kind {
        "botfather" => Some("https://t.me/BotFather".to_string()),
        "bot" => {
            // Le nom vient d'une réponse réseau et part vers le gestionnaire de protocole de
            // l'OS : on le contraint plutôt que de faire confiance au serveur.
            let name = bot_username.map(str::trim).filter(|n| username_is_well_formed(n))?;
            Some(format!("https://t.me/{name}?start=vigil"))
        }
        _ => None,
    }
}

/// Client HTTP partagé : reconstruire un `reqwest::Client` à chaque envoi refait la poignée TLS
/// et jette le pool de connexions. `Client::new()` peut en outre **paniquer** si l'initialisation
/// TLS échoue — une panique dans `run_loop` tuerait la boucle d'envoi en silence.
fn client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

// ── Fonctions pures (testées sans réseau) ────────────────────────────────────────

/// L'URL porte le jeton : elle ne doit JAMAIS être journalisée ni remontée dans un message
/// d'erreur (`parse_api_error` ne reprend que le code et un motif normalisé, et les erreurs de
/// transport passent par `transport_error` qui retire l'URL).
fn api_url(token: &str, method: &str) -> String {
    format!("{API_BASE}/bot{token}/{method}")
}

/// Forme attendue d'un jeton de bot : `<id numérique>:<secret>`.
///
/// Le jeton est interpolé dans le **chemin** de l'URL : un collage contenant `/`, `?` ou `#`
/// (typiquement l'URL BotFather complète, ou un copier-coller élargi) redirigerait la requête
/// vers une autre route au lieu de produire une erreur lisible. On refuse en amont.
pub fn token_is_well_formed(token: &str) -> bool {
    let Some((id, secret)) = token.split_once(':') else {
        return false;
    };
    !id.is_empty()
        && id.chars().all(|c| c.is_ascii_digit())
        && secret.len() >= 20
        && secret.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Un nom de bot Telegram est alphanumérique ASCII + `_`. La valeur vient d'une réponse réseau et
/// finit dans une URL passée à `open::that` (donc au gestionnaire de protocole de l'OS) : on la
/// contraint avant de la stocker plutôt que de faire confiance au serveur.
pub fn username_is_well_formed(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Vérifie qu'une réponse d'API 2xx est bien un succès applicatif (`{"ok":true}`).
fn check_ok(body: &str) -> Result<(), String> {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) if v.get("ok").and_then(|o| o.as_bool()) == Some(true) => Ok(()),
        Ok(_) => Err("api_rejected".to_string()),
        Err(_) => Err("invalid_response".to_string()),
    }
}

/// Décode la réponse de `getMe` en identité de bot.
pub fn parse_get_me(body: &str) -> Result<ChannelIdentity, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "invalid_response".to_string())?;
    if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
        return Err("invalid_response".to_string());
    }
    v.get("result")
        .and_then(|r| r.get("username"))
        .and_then(|u| u.as_str())
        .filter(|u| !u.is_empty())
        .map(|u| ChannelIdentity { username: u.to_string() })
        .ok_or_else(|| "invalid_response".to_string())
}

/// Charge utile du lien d'appairage : `link_url` génère `?start=vigil`, que Telegram transforme
/// en message `/start vigil`. Sa présence prouve que l'expéditeur est passé par NOTRE lien.
const START_PAYLOAD: &str = "/start vigil";

/// Destinataire capté à l'appairage, avec de quoi le montrer à l'utilisateur pour confirmation.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedRecipient {
    /// Identifiant opaque, seul élément réellement utilisé pour l'envoi.
    pub chat_id: String,
    /// Nom affichable (prénom/nom, `@username`, ou titre de groupe) — sert UNIQUEMENT à
    /// l'écran de confirmation « c'est bien vous ? ». Jamais envoyé nulle part.
    pub display_name: String,
}

/// Extrait le destinataire à appairer.
///
/// ## Priorité au message porteur de la charge utile
///
/// Un message contenant `/start vigil` prouve que son auteur est passé par le lien de
/// l'assistant ; il l'emporte donc sur la simple récence. Sans cette priorité, un tiers qui écrit
/// au bot **après** l'utilisateur légitime serait retenu — et comme `pair()` ne confirme aucun
/// offset, « Recommencer » relirait la même file de 24 h et recapterait indéfiniment le même
/// intrus : la confirmation d'identité donnerait la visibilité sans le moindre remède.
///
/// À défaut de charge utile (l'utilisateur a ouvert son bot à la main et appuyé sur le bouton
/// START natif, qui envoie un `/start` nu), on retombe sur le dernier message reçu — c'est
/// l'écran de confirmation qui fait alors office de garde-fou.
pub fn extract_recipient(body: &str) -> Option<PairedRecipient> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
        return None;
    }
    let messages: Vec<&serde_json::Value> =
        v.get("result")?.as_array()?.iter().filter_map(|u| u.get("message")).collect();

    let chosen = messages
        .iter()
        .rev()
        .find(|m| {
            m.get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.trim().starts_with(START_PAYLOAD))
        })
        .or_else(|| messages.last())?;

    let chat = chosen.get("chat")?;
    // `id` est un entier 64 bits côté API ; on le garde en chaîne (opaque pour le reste du code)
    // sans jamais passer par un f64 qui perdrait de la précision (un identifiant de groupe est
    // négatif et à 16 chiffres).
    let chat_id = chat.get("id")?.as_i64()?.to_string();
    Some(PairedRecipient { chat_id, display_name: display_name_of(chat) })
}

/// Nom lisible d'une conversation : « Prénom Nom », sinon `@username`, sinon le titre (groupe),
/// sinon un repli neutre. Purement décoratif — seul `chat_id` compte pour l'envoi.
fn display_name_of(chat: &serde_json::Value) -> String {
    let field = |k: &str| chat.get(k).and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    let full = [field("first_name"), field("last_name")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    if !full.is_empty() {
        return full;
    }
    if let Some(u) = field("username") {
        return format!("@{u}");
    }
    field("title").unwrap_or("this conversation").to_string()
}

// ── Boucle retour : encodage / décodage purs (Story 8.3 + 8.4) ───────────────────

/// Préfixes de la charge utile des boutons. Volontairement courts : Telegram plafonne
/// `callback_data` à 64 octets, et un `request_id` (`{millis}-{n}`) en occupe déjà une vingtaine.
const CB_APPROVE: &str = "a";
const CB_DENY: &str = "d";
/// Story 8.4 — option (indice) / validation multi.
const CB_OPTION: &str = "o";
const CB_SEND: &str = "s";

/// Plafond pratique d'affichage d'un libellé de bouton (caractères, pas octets).
const BUTTON_LABEL_MAX_CHARS: usize = 40;

/// Corps de `sendMessage`, **pur** (donc testable sans réseau).
///
/// Le clavier n'apparaît QUE si la demande est actionnable — le manager n'a réservé un
/// `request_id` / clavier que dans ce cas.
fn send_body(chat_id: &str, notif: &MobileNotif) -> serde_json::Value {
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "text": super::format_notif_text(notif),
    });
    if let Some(kb) = &notif.keyboard {
        body["reply_markup"] = encode_keyboard(kb);
    } else if let Some(request_id) = &notif.request_id {
        // Repli 8.3 : request_id seul → Approve/Deny.
        body["reply_markup"] = encode_keyboard(&ActionKeyboard::ApproveDeny {
            request_id: request_id.clone(),
        });
    }
    body
}

/// Encode un clavier neutre en markup Telegram. **Seul** endroit où vivent `a`/`d`/`o`/`s`.
fn encode_keyboard(keyboard: &ActionKeyboard) -> serde_json::Value {
    match keyboard {
        ActionKeyboard::ApproveDeny { request_id } => serde_json::json!({
            "inline_keyboard": [[
                { "text": "✅ Approve", "callback_data": format!("{CB_APPROVE}:{request_id}") },
                { "text": "❌ Deny",    "callback_data": format!("{CB_DENY}:{request_id}") },
            ]]
        }),
        ActionKeyboard::Choice {
            request_id,
            questions,
            selected,
        } => {
            // Story 8.9 — `needs_confirm` gouverne À LA FOIS le marqueur d'état et le bouton Send :
            // une SEULE question mono reste le rendu 8.4 (libellé nu, aucun Send, un tap répond) ;
            // dès qu'il y a un état à montrer, il est montré.
            let confirm = super::needs_confirm(questions);
            // Prédicat PARTAGÉ avec `format_choice_body` : le corps et les boutons doivent porter
            // la même numérotation, sinon le rattachement bouton→question disparaît (piège n°1).
            let numeroter = super::numbers_questions(questions);
            let vide = std::collections::BTreeSet::new();
            let mut rows: Vec<serde_json::Value> = Vec::new();
            for (q, question) in questions.iter().enumerate() {
                let sel = selected.get(q).unwrap_or(&vide);
                for (i, label) in question.options.iter().enumerate() {
                    // Le plafond de 40 caractères porte sur le LIBELLÉ : le préfixe (marqueur +
                    // numéro) est ajouté APRÈS troncature, comme en 8.4.
                    let label = truncate_button_label(label);
                    let text = if confirm {
                        let mark = match (question.multi, sel.contains(&i)) {
                            (true, true) => "☑",
                            (true, false) => "☐",
                            (false, true) => "●",
                            (false, false) => "○",
                        };
                        if numeroter {
                            format!("{mark} {}· {label}", q + 1)
                        } else {
                            format!("{mark} {label}")
                        }
                    } else {
                        label
                    };
                    rows.push(serde_json::json!([{
                        "text": text,
                        "callback_data": format!("{CB_OPTION}:{request_id}:{q}:{i}"),
                    }]));
                }
            }
            if confirm {
                rows.push(serde_json::json!([{
                    "text": "✅ Send",
                    "callback_data": format!("{CB_SEND}:{request_id}"),
                }]));
            }
            serde_json::json!({ "inline_keyboard": rows })
        }
    }
}

fn truncate_button_label(label: &str) -> String {
    let flat: String = label
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > BUTTON_LABEL_MAX_CHARS {
        let kept: String = flat.chars().take(BUTTON_LABEL_MAX_CHARS.saturating_sub(1)).collect();
        format!("{kept}…")
    } else {
        flat
    }
}

/// Décode la charge utile d'un bouton. Toute forme inattendue rend `None` — jamais une décision
/// devinée : dans le doute, on ne fabrique aucune approbation.
fn decode_callback_data(data: &str) -> Option<(String, IncomingKind)> {
    // Story 8.9 — 4 segments : `o:{request_id}:{q}:{i}`. Le 4ᵉ segment étant le RESTE de la chaîne,
    // un `:` surnuméraire fait échouer `parse::<u8>()` → `None`, donc aucune décision devinée.
    let mut parts = data.splitn(4, ':');
    let tag = parts.next()?;
    let request_id = parts.next()?;
    if request_id.is_empty() {
        return None;
    }
    match tag {
        // Approve/Deny/Send n'ont AUCUN segment d'argument : un segment de trop est une forme
        // inattendue, donc rien — jamais une décision devinée.
        CB_APPROVE => {
            if parts.next().is_some() {
                return None;
            }
            Some((request_id.to_string(), IncomingKind::Approve))
        }
        CB_DENY => {
            if parts.next().is_some() {
                return None;
            }
            Some((request_id.to_string(), IncomingKind::Deny))
        }
        CB_OPTION => {
            let question: u8 = parts.next()?.parse().ok()?;
            let index: u8 = parts.next()?.parse().ok()?;
            Some((
                request_id.to_string(),
                IncomingKind::Choose { question, index },
            ))
        }
        CB_SEND => {
            // Aucun segment d'argument. Avec `splitn(4)`, ce reste couvre TOUT ce qui suit le
            // `request_id` — 4ᵉ segment compris, pas seulement un 3ᵉ.
            if parts.next().is_some() {
                return None;
            }
            Some((request_id.to_string(), IncomingKind::Confirm))
        }
        _ => None,
    }
}

/// Référence de message rendue par un envoi réussi. `None` si la réponse ne la porte pas : le
/// désarmement sera alors impossible pour cette demande, ce qui reste **inoffensif** (l'anti-rejeu
/// par consommation unique est la garantie de correction, pas le désarmement).
pub fn parse_sent_message(body: &str) -> Option<MessageRef> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let id = v.get("result")?.get("message_id")?.as_i64()?;
    Some(MessageRef::new(id.to_string()))
}

/// Décode un lot d'updates en actions neutres, et rend le plus grand identifiant d'update VU
/// (décodable ou non), qui sert de point de reprise.
///
/// Fonction **pure**, testée sans réseau (patron `extract_recipient` / `parse_get_me`).
///
/// L'identité retenue comme `sender_id` est celle de la **conversation** où vit le message, pas
/// celle de l'auteur du tap : c'est précisément le destinataire auquel Vigil a envoyé le clavier
/// (et les deux coïncident dans une conversation privée). Le repli sur l'auteur ne sert qu'aux
/// charges utiles tronquées.
pub fn parse_action_updates(body: &str) -> (Vec<ChannelAction>, Option<i64>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return (Vec::new(), None);
    };
    if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
        return (Vec::new(), None);
    }
    let Some(updates) = v.get("result").and_then(|r| r.as_array()) else {
        return (Vec::new(), None);
    };

    let mut actions = Vec::new();
    let mut last_seen: Option<i64> = None;
    for update in updates {
        if let Some(id) = update.get("update_id").and_then(|i| i.as_i64()) {
            last_seen = Some(last_seen.map_or(id, |prev: i64| prev.max(id)));
        }
        let Some(action) = decode_callback_query(update.get("callback_query")) else {
            continue;
        };
        actions.push(action);
    }
    (actions, last_seen)
}

fn decode_callback_query(query: Option<&serde_json::Value>) -> Option<ChannelAction> {
    let query = query?;
    let action_ref = query.get("id").and_then(|i| i.as_str()).filter(|i| !i.is_empty())?;
    let data = query.get("data").and_then(|d| d.as_str())?;
    let (request_id, kind) = decode_callback_data(data)?;
    let sender_id = query
        .get("message")
        .and_then(|m| m.get("chat"))
        .and_then(|c| c.get("id"))
        .and_then(|i| i.as_i64())
        .or_else(|| query.get("from")?.get("id")?.as_i64())?
        .to_string();
    Some(ChannelAction {
        request_id,
        kind,
        sender_id,
        action_ref: ActionRef::new(action_ref),
    })
}

/// Normalise une erreur d'API en code stable, **sans jamais recopier le corps brut** (il peut
/// contenir l'écho de la requête). Le vocabulaire produit (`invalid_token`, `rate_limited:{secs}`,
/// `api_error_{code}`) est celui que comprend le manager — il est donc partagé par tous les
/// canaux, pas propre à celui-ci.
pub fn parse_api_error(status: u16, body: &str) -> String {
    match status {
        401 => "invalid_token".to_string(),
        429 => {
            let retry = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v.get("parameters")?.get("retry_after")?.as_u64())
                .unwrap_or(1);
            format!("{}{retry}", super::RATE_LIMITED_PREFIX)
        }
        code => format!("api_error_{code}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_get_me_lit_le_username() {
        let body = r#"{"ok":true,"result":{"id":42,"is_bot":true,"username":"vigil_test_bot"}}"#;
        assert_eq!(
            parse_get_me(body).unwrap(),
            ChannelIdentity { username: "vigil_test_bot".into() }
        );
    }

    #[test]
    fn parse_get_me_rejette_les_reponses_inexploitables() {
        for body in [
            r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#,
            r#"{"ok":true,"result":{"id":42}}"#,      // pas de username
            r#"{"ok":true,"result":{"username":""}}"#, // username vide
            "pas du json",
            "{}",
        ] {
            assert_eq!(parse_get_me(body), Err("invalid_response".to_string()), "{body}");
        }
    }

    #[test]
    fn getme_401_mappe_invalid_token() {
        // Réponse réelle de Telegram sur jeton invalide.
        let body = r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#;
        assert_eq!(parse_api_error(401, body), "invalid_token");
        // Le corps brut n'est jamais recopié dans l'erreur.
        assert!(!parse_api_error(401, body).contains("Unauthorized"));
    }

    #[test]
    fn parse_api_error_couvre_429_et_le_reste() {
        let body = r#"{"ok":false,"error_code":429,"description":"Too Many Requests","parameters":{"retry_after":7}}"#;
        assert_eq!(parse_api_error(429, body), "rate_limited:7");
        // 429 sans `parameters` → 1 s par défaut, jamais une absence de ré-essai.
        assert_eq!(parse_api_error(429, "{}"), "rate_limited:1");
        assert_eq!(parse_api_error(500, ""), "api_error_500");
        assert_eq!(parse_api_error(400, r#"{"description":"chat not found"}"#), "api_error_400");
        // Le code produit est celui que sait relire le manager (contrat inter-modules).
        assert_eq!(super::super::retry_after_secs(&parse_api_error(429, body)), Some(7));
        assert_eq!(super::super::retry_after_secs(&parse_api_error(500, "")), None);
    }

    #[test]
    fn pairing_capte_chat_id() {
        // Réponse réelle de `getUpdates` après un `/start`.
        let body = r#"{"ok":true,"result":[{"update_id":1,"message":{"message_id":1,"chat":{"id":123456789,"type":"private","first_name":"Steven"},"text":"/start vigil"}}]}"#;
        let r = extract_recipient(body).unwrap();
        assert_eq!(r.chat_id, "123456789");
        assert_eq!(r.display_name, "Steven");
    }

    #[test]
    fn pairing_prefere_le_payload_a_la_recence() {
        // ⚠️ Test de sécurité (revue 8.1). Un tiers écrit APRÈS l'utilisateur légitime : sans la
        // priorité au payload, `.next_back()` retiendrait l'intrus (333) — et comme aucun offset
        // n'est confirmé, « Recommencer » le recapterait indéfiniment.
        let body = r#"{"ok":true,"result":[
            {"update_id":1,"edited_message":{"chat":{"id":111}}},
            {"update_id":2,"message":{"chat":{"id":222,"type":"private","first_name":"Steven"},"text":"/start vigil"}},
            {"update_id":3,"message":{"chat":{"id":333,"type":"private","first_name":"Intrus"},"text":"coucou"}}
        ]}"#;
        let r = extract_recipient(body).unwrap();
        assert_eq!(r.chat_id, "222", "le porteur du payload doit gagner contre le plus récent");
        assert_eq!(r.display_name, "Steven");
    }

    #[test]
    fn pairing_retombe_sur_le_dernier_message_sans_payload() {
        // Bouton START natif de Telegram → `/start` NU, aucun payload. On accepte, et c'est
        // l'écran de confirmation d'identité qui sert alors de garde-fou.
        let body = r#"{"ok":true,"result":[
            {"update_id":1,"message":{"chat":{"id":222,"type":"private"},"text":"/start"}},
            {"update_id":2,"message":{"chat":{"id":333,"type":"private","username":"steve"},"text":"/start"}}
        ]}"#;
        let r = extract_recipient(body).unwrap();
        assert_eq!(r.chat_id, "333");
        assert_eq!(r.display_name, "@steve");
    }

    #[test]
    fn pairing_sans_message_ne_capte_rien() {
        // Aucun `/start` encore reçu → on repart pour un tour de long-poll.
        assert_eq!(extract_recipient(r#"{"ok":true,"result":[]}"#), None);
        assert_eq!(extract_recipient(r#"{"ok":false,"error_code":409}"#), None);
        assert_eq!(extract_recipient("pas du json"), None);
        // Que des updates sans nœud `message` : rien à capter non plus.
        assert_eq!(
            extract_recipient(r#"{"ok":true,"result":[{"my_chat_member":{"chat":{"id":1}}}]}"#),
            None
        );
    }

    #[test]
    fn chat_id_negatif_preserve() {
        // Un groupe a un identifiant NÉGATIF et très grand : il doit survivre tel quel
        // (aucun passage par un flottant, aucune troncature).
        let body =
            r#"{"ok":true,"result":[{"message":{"chat":{"id":-1001234567890123,"title":"Equipe"}}}]}"#;
        let r = extract_recipient(body).unwrap();
        assert_eq!(r.chat_id, "-1001234567890123");
        assert_eq!(r.display_name, "Equipe");
    }

    #[test]
    fn jeton_mal_forme_refuse_avant_toute_requete() {
        // ⚠️ Le jeton est interpolé dans le CHEMIN de l'URL : un `/` ou `?` collé par erreur
        // redirigerait la requête au lieu de produire une erreur lisible.
        assert!(token_is_well_formed("123456789:AAHfimoWJ3Bd0m3PPGCk3sSJhaLB6VXTZ_c"));
        for mauvais in [
            "",
            "pas-de-deux-points",
            "https://api.telegram.org/bot123:ABCDEFGHIJKLMNOPQRSTU/getMe",
            "123:court",
            "abc:AAHfimoWJ3Bd0m3PPGCk3sSJhaLB6VXTZ_c", // id non numérique
            "123:AAHfimoWJ3Bd0m3PP/Ck3sSJhaLB6VXTZ_c", // slash dans le secret
        ] {
            assert!(!token_is_well_formed(mauvais), "{mauvais}");
        }
    }

    #[test]
    fn deux_cents_ok_mais_ok_false_est_un_echec() {
        // Telegram peut répondre 200 avec `{"ok":false}` ; un portail captif renvoie un 200 qui
        // n'est pas une réponse d'API. Les deux doivent échouer, pas passer pour un envoi réussi.
        assert_eq!(check_ok(r#"{"ok":true,"result":{}}"#), Ok(()));
        assert_eq!(check_ok(r#"{"ok":false,"description":"chat not found"}"#), Err("api_rejected".into()));
        assert_eq!(check_ok("<html>portail captif</html>"), Err("invalid_response".into()));
    }

    #[test]
    fn liens_de_l_assistant() {
        assert_eq!(link_url("botfather", None).unwrap(), "https://t.me/BotFather");
        assert_eq!(
            link_url("bot", Some("monbot")).unwrap(),
            "https://t.me/monbot?start=vigil"
        );
        // Pas de bot connu (ou nom vide) → pas de lien bancal, l'appelant remonte une erreur.
        assert_eq!(link_url("bot", None), None);
        assert_eq!(link_url("bot", Some("  ")), None);
        assert_eq!(link_url("autre", Some("monbot")), None);
        // Nom hors charset : la valeur vient du réseau et finit dans `open::that`.
        assert_eq!(link_url("bot", Some("mon bot")), None);
        assert_eq!(link_url("bot", Some("bot/../evil")), None);
    }

    // ══ Story 8.3 : boucle retour ════════════════════════════════════════════════

    fn notif(request_id: Option<&str>) -> MobileNotif {
        MobileNotif {
            kind: super::super::NotifKind::Waiting,
            agent_label: "vigil".into(),
            tool: crate::agent::state::SourceTool::ClaudeCode,
            summary: Some("Wants to run: Bash".into()),
            request_id: request_id.map(str::to_string),
            keyboard: None,
            body_extra: None,
        }
    }

    fn qd(question: &str, options: &[&str], multi: bool) -> super::super::QuestionData {
        super::super::QuestionData {
            multi,
            options: options.iter().map(|s| (*s).to_string()).collect(),
            header: None,
            question: question.into(),
        }
    }

    fn notif_choice(request_id: &str, multi: bool, options: &[&str]) -> MobileNotif {
        notif_lot(request_id, &[qd("Pick?", options, multi)])
    }

    /// Story 8.9 — notif portant un lot de N questions.
    fn notif_lot(request_id: &str, questions: &[super::super::QuestionData]) -> MobileNotif {
        use std::collections::BTreeSet;
        MobileNotif {
            kind: super::super::NotifKind::Waiting,
            agent_label: "vigil".into(),
            tool: crate::agent::state::SourceTool::ClaudeCode,
            summary: Some("Asked you a question".into()),
            request_id: Some(request_id.into()),
            keyboard: Some(ActionKeyboard::Choice {
                request_id: request_id.into(),
                questions: questions.to_vec(),
                selected: vec![BTreeSet::new(); questions.len()],
            }),
            body_extra: Some(super::super::format_choice_body(questions)),
        }
    }

    /// AC1 côté canal : le clavier suit le `request_id`, et rien d'autre. Aucune branche sur
    /// l'outil ici non plus — `telegram.rs` ne sait même pas ce qu'est un `source_tool` en matière
    /// de capacité.
    #[test]
    fn clavier_seulement_si_demande_armee() {
        let arme = send_body("123", &notif(Some("run-7")));
        let clavier = arme.get("reply_markup").expect("clavier attendu");
        let boutons = clavier["inline_keyboard"][0].as_array().unwrap();
        assert_eq!(boutons.len(), 2);
        assert_eq!(boutons[0]["text"], "✅ Approve");
        assert_eq!(boutons[0]["callback_data"], "a:run-7");
        assert_eq!(boutons[1]["text"], "❌ Deny");
        assert_eq!(boutons[1]["callback_data"], "d:run-7");

        // Notif-only : PAS de clé `reply_markup` du tout (et non un clavier vide).
        let nu = send_body("123", &notif(None));
        assert!(nu.get("reply_markup").is_none());
        // Le texte est le MÊME dans les deux cas : les boutons n'ajoutent aucun contenu.
        assert_eq!(arme["text"], nu["text"]);
        assert_eq!(nu["text"], "⏸ Waiting for you\nvigil · Claude Code\nWants to run: Bash");
    }

    #[test]
    fn callback_data_tient_dans_la_limite_du_canal() {
        // Telegram plafonne `callback_data` à 64 OCTETS. Un `request_id` réel fait ~20 caractères
        // (millis Unix + compteur) : la marge est confortable, mais elle doit rester vérifiée —
        // au-delà, l'API rejette le message ENTIER et la notification est perdue.
        let realiste = super::super::next_request_id();
        for data in [format!("a:{realiste}"), format!("d:{realiste}")] {
            assert!(data.len() <= 64, "charge utile trop longue : {data}");
        }
    }

    #[test]
    fn parse_message_envoye_rend_une_reference() {
        let body = r#"{"ok":true,"result":{"message_id":4242,"chat":{"id":123},"text":"⏸"}}"#;
        assert_eq!(parse_sent_message(body), Some(MessageRef::new("4242")));
        // Réponse inexploitable → pas de référence. Conséquence : ce message ne pourra pas être
        // désarmé, ce qui est INOFFENSIF (l'anti-rejeu est la garantie de correction).
        for body in [r#"{"ok":true,"result":{}}"#, "{}", "pas du json", r#"{"result":null}"#] {
            assert_eq!(parse_sent_message(body), None, "{body}");
        }
    }

    /// Test nommé par la story : décodage d'un update RÉEL, sans réseau.
    #[test]
    fn parse_action_entrante_pure() {
        // Forme réelle d'un `callback_query` Telegram après un tap sur ✅.
        let body = r#"{"ok":true,"result":[{"update_id":770001,"callback_query":{
            "id":"4382bfdwbob98","from":{"id":123456789,"first_name":"Steven"},
            "message":{"message_id":4242,"chat":{"id":123456789,"type":"private"},"text":"⏸ Waiting for you"},
            "chat_instance":"-100","data":"a:1753000000000-3"}}]}"#;
        let (actions, last) = parse_action_updates(body);
        assert_eq!(last, Some(770001), "l'offset doit avancer");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].request_id, "1753000000000-3");
        assert_eq!(actions[0].kind, IncomingKind::Approve);
        assert_eq!(actions[0].sender_id, "123456789");
        assert_eq!(actions[0].action_ref, ActionRef::new("4382bfdwbob98"));

        // Le refus décode symétriquement.
        let deny = body.replace("\"a:", "\"d:");
        assert_eq!(parse_action_updates(&deny).0[0].kind, IncomingKind::Deny);
    }

    #[test]
    fn parse_action_avance_l_offset_meme_sur_un_update_indecodable() {
        // ⚠️ Sans ça, un update parasite (autre type, charge utile étrangère, bouton d'une autre
        // application) bloquerait la boucle sur lui INDÉFINIMENT : le même lot reviendrait à
        // chaque tour et le vrai tap suivant ne serait jamais atteint.
        let body = r#"{"ok":true,"result":[
            {"update_id":10,"message":{"chat":{"id":1},"text":"coucou"}},
            {"update_id":11,"callback_query":{"id":"x","from":{"id":1},"data":"z:nawak"}},
            {"update_id":12,"callback_query":{"id":"y","from":{"id":1}}},
            {"update_id":13,"callback_query":{"id":"","from":{"id":1},"data":"a:run-7"}}
        ]}"#;
        let (actions, last) = parse_action_updates(body);
        assert!(actions.is_empty(), "aucune de ces formes n'est une décision valide");
        assert_eq!(last, Some(13), "l'offset avance quand même");
    }

    #[test]
    fn parse_action_rejette_les_reponses_inexploitables() {
        for body in [
            r#"{"ok":false,"error_code":409,"description":"Conflict"}"#,
            r#"{"ok":true,"result":[]}"#,
            "pas du json",
            "{}",
        ] {
            let (actions, _) = parse_action_updates(body);
            assert!(actions.is_empty(), "{body}");
        }
        // `409 Conflict` (deux pollers concurrents) ne fait PAS avancer l'offset : aucun update
        // n'a été lu, en confirmer un perdrait un tap.
        assert_eq!(parse_action_updates(r#"{"ok":false,"error_code":409}"#).1, None);
    }

    /// AC5 (Story 8.5) — **aucune fenêtre d'oubli** : un tour de réception raté ne fait JAMAIS
    /// avancer le curseur, donc la même update est re-demandée au tour suivant. C'est l'invariant
    /// sur lequel repose le backoff : dormir entre deux essais n'est sans danger que si un essai
    /// raté ne consomme rien.
    ///
    /// Verrouille la moitié testable sans réseau : `poll_actions` n'appelle `cursor.set` qu'avec
    /// le `last_seen` de `parse_action_updates` — pas de `last_seen`, pas de `set`. L'autre
    /// moitié (les erreurs de transport et de statut sortent AVANT le `set`) reste une propriété
    /// de lecture du corps de `poll_actions`.
    #[test]
    fn un_tour_rate_navance_jamais_le_curseur() {
        for body in [
            r#"{"ok":false,"error_code":409,"description":"Conflict"}"#, // deux pollers
            r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#, // jeton révoqué
            r#"{"ok":true,"result":[]}"#,                                // tour vide, sans tap
            "pas du json",                                              // proxy / portail captif
            "{}",
        ] {
            let (_, last_seen) = parse_action_updates(body);
            assert_eq!(last_seen, None, "aucun offset à confirmer : {body}");

            // Ce que `poll_actions` en ferait : sans `last_seen`, aucun `set` → curseur intact.
            let mut cursor = ActionCursor::default();
            cursor.set(42);
            if let Some(last) = last_seen {
                cursor.set(last + 1);
            }
            assert_eq!(cursor.get(), Some(42), "curseur inchangé par un tour raté : {body}");
        }
    }

    #[test]
    fn identite_retenue_est_celle_de_la_conversation() {
        // Dans un GROUPE appairé, `chat.id` (négatif) est le destinataire auquel Vigil a envoyé
        // le clavier ; `from.id` est le membre qui a tapé. C'est le premier qui doit être comparé
        // à l'appairage — sinon un tap légitime dans le groupe appairé serait rejeté.
        let body = r#"{"ok":true,"result":[{"update_id":1,"callback_query":{
            "id":"cb","from":{"id":555},
            "message":{"message_id":9,"chat":{"id":-1001234567890123,"title":"Equipe"}},
            "data":"a:run-7"}}]}"#;
        assert_eq!(parse_action_updates(body).0[0].sender_id, "-1001234567890123");

        // Charge utile tronquée (pas de `message`) : repli sur l'auteur, jamais sur rien.
        let sans_message = r#"{"ok":true,"result":[{"update_id":1,"callback_query":{
            "id":"cb","from":{"id":555},"data":"a:run-7"}}]}"#;
        assert_eq!(parse_action_updates(sans_message).0[0].sender_id, "555");
    }

    #[test]
    fn decode_callback_data_refuse_les_formes_inattendues() {
        // Dans le doute, AUCUNE décision n'est devinée.
        assert_eq!(
            decode_callback_data("a:run-7"),
            Some(("run-7".to_string(), IncomingKind::Approve))
        );
        assert_eq!(
            decode_callback_data("d:run-7"),
            Some(("run-7".to_string(), IncomingKind::Deny))
        );
        assert_eq!(
            decode_callback_data("o:run-7:0:2"),
            Some(("run-7".to_string(), IncomingKind::Choose { question: 0, index: 2 }))
        );
        // Story 8.9 — 4ᵉ segment : l'indice de question.
        assert_eq!(
            decode_callback_data("o:run-7:3:2"),
            Some(("run-7".to_string(), IncomingKind::Choose { question: 3, index: 2 }))
        );
        assert_eq!(
            decode_callback_data("o:run-7:255:255"),
            Some(("run-7".to_string(), IncomingKind::Choose { question: 255, index: 255 }))
        );
        assert_eq!(
            decode_callback_data("s:run-7"),
            Some(("run-7".to_string(), IncomingKind::Confirm))
        );
        for mauvais in [
            "",
            "a",
            "a:",
            "x:run-7",
            "approve:run-7",
            ":run-7",
            "A:run-7",
            "o:run-7",
            "o:run-7:x",
            "s:run-7:extra",
            // Story 8.9 — `o:` exige EXACTEMENT deux indices `u8`. Le 4ᵉ segment étant le reste de
            // la chaîne, un `:` surnuméraire fait échouer `parse::<u8>()`.
            "o:run-7:1",
            "o:run-7:1:2:3",
            "o:run-7:a:1",
            "o:run-7:1:a",
            "o:run-7:256:0",
            "o:run-7:0:256",
            "o:run-7:-1:0",
            "a:run-7:extra",
            "d:run-7:extra",
        ] {
            assert_eq!(decode_callback_data(mauvais), None, "{mauvais}");
        }
    }

    #[test]
    fn url_api_bien_formee() {
        assert_eq!(
            api_url("123:ABC", "sendMessage"),
            "https://api.telegram.org/bot123:ABC/sendMessage"
        );
    }

    // ── Story 8.4 ────────────────────────────────────────────────────────────

    #[test]
    fn clavier_choix_encode_indices_pas_libelles() {
        let body = send_body("123", &notif_choice("run-7", false, &["Alpha", "Beta"]));
        let rows = body["reply_markup"]["inline_keyboard"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0]["text"], "Alpha");
        assert_eq!(rows[0][0]["callback_data"], "o:run-7:0:0");
        assert_eq!(rows[1][0]["callback_data"], "o:run-7:0:1");
        // Libellé jamais dans callback_data.
        let dump = body["reply_markup"].to_string();
        assert!(!dump.contains("Alpha") || dump.contains("\"text\":\"Alpha\""));
        for row in rows {
            let data = row[0]["callback_data"].as_str().unwrap();
            assert!(!data.contains("Alpha") && !data.contains("Beta"));
        }
        assert!(body["text"].as_str().unwrap().contains("Asked you a question"));
        assert!(body["text"].as_str().unwrap().contains("• Alpha"));
    }

    #[test]
    fn clavier_multi_a_send_et_coche() {
        let mut n = notif_choice("run-9", true, &["A", "B"]);
        if let Some(ActionKeyboard::Choice { selected, .. }) = &mut n.keyboard {
            selected[0].insert(1);
        }
        let rows = send_body("1", &n)["reply_markup"]["inline_keyboard"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(rows.len(), 3, "2 options + Send");
        assert_eq!(rows[0][0]["text"], "☐ A");
        assert_eq!(rows[1][0]["text"], "☑ B");
        assert_eq!(rows[2][0]["text"], "✅ Send");
        assert_eq!(rows[2][0]["callback_data"], "s:run-9");
    }

    // ── Story 8.9 : lot de N questions ───────────────────────────────────────

    fn lot_canonique() -> Vec<super::super::QuestionData> {
        vec![
            qd("Deploy?", &["prod", "staging"], false),
            qd("Which checks?", &["lint", "tests"], true),
        ]
    }

    /// AC2 — un SEUL clavier pour tout le lot ; chaque bouton porte le marqueur de son type ET le
    /// numéro de sa question (piège n°1 : sans le numéro, deux libellés « Yes » sont ambigus,
    /// puisque le corps n'est jamais ré-édité pendant la sélection).
    #[test]
    fn clavier_multi_questions_encode_question_et_indice() {
        use std::collections::BTreeSet;
        let mut n = notif_lot("run-l", &lot_canonique());
        if let Some(ActionKeyboard::Choice { selected, .. }) = &mut n.keyboard {
            selected[0].insert(1); // Q1 mono → staging
            selected[1].insert(0); // Q2 multi → lint
        }
        let rows = send_body("1", &n)["reply_markup"]["inline_keyboard"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(rows.len(), 5, "2 + 2 options + Send");
        // Ordre : question puis option.
        assert_eq!(rows[0][0]["text"], "○ 1· prod");
        assert_eq!(rows[0][0]["callback_data"], "o:run-l:0:0");
        assert_eq!(rows[1][0]["text"], "● 1· staging");
        assert_eq!(rows[1][0]["callback_data"], "o:run-l:0:1");
        assert_eq!(rows[2][0]["text"], "☑ 2· lint");
        assert_eq!(rows[2][0]["callback_data"], "o:run-l:1:0");
        assert_eq!(rows[3][0]["text"], "☐ 2· tests");
        assert_eq!(rows[3][0]["callback_data"], "o:run-l:1:1");
        // Piège n°2 — le Send existe même sur un lot dont la 1ʳᵉ question est mono.
        assert_eq!(rows[4][0]["text"], "✅ Send");
        assert_eq!(rows[4][0]["callback_data"], "s:run-l");
        // Le corps porte la MÊME numérotation que les boutons.
        let text = send_body("1", &n)["text"].as_str().unwrap().to_string();
        assert!(text.contains("1. Deploy?"), "{text}");
        assert!(text.contains("2. Which checks?"), "{text}");

        // Un lot TOUT-mono a aussi un Send (sinon le 1ᵉʳ tap enverrait une réponse partielle).
        let tout_mono = notif_lot(
            "run-t",
            &[qd("A?", &["x"], false), qd("B?", &["y"], false)],
        );
        let rows = send_body("1", &tout_mono)["reply_markup"]["inline_keyboard"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(rows.len(), 3, "2 options + Send");
        assert_eq!(rows[2][0]["text"], "✅ Send");
    }

    /// AC9 / décision 9 — une SEULE question mono : rendu 8.4 strictement inchangé (libellé nu,
    /// aucun marqueur, aucun bouton Send). C'est l'ABSENCE du Send qui est le verrou.
    #[test]
    fn send_absent_en_question_unique_mono() {
        let body = send_body("1", &notif_choice("run-u", false, &["Alpha", "Beta"]));
        let rows = body["reply_markup"]["inline_keyboard"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "2 options, AUCUN Send");
        assert_eq!(rows[0][0]["text"], "Alpha", "aucun marqueur en question unique mono");
        assert_eq!(rows[1][0]["text"], "Beta");
        for row in rows {
            let data = row[0]["callback_data"].as_str().unwrap();
            assert!(!data.starts_with("s:"), "aucun bouton Send : {data}");
        }
        // ...alors qu'une seule question MULTI en a un (frontière du pivot `needs_confirm`).
        let body = send_body("1", &notif_choice("run-v", true, &["Alpha", "Beta"]));
        let rows = body["reply_markup"]["inline_keyboard"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2][0]["callback_data"], "s:run-v");
    }

    /// Le plafond de 40 caractères porte sur le LIBELLÉ : le préfixe s'ajoute après troncature.
    #[test]
    fn prefixe_de_bouton_ajoute_apres_troncature() {
        let long = "x".repeat(80);
        let n = notif_lot("r", &[qd("A?", &[&long], false), qd("B?", &["y"], false)]);
        let rows = send_body("1", &n)["reply_markup"]["inline_keyboard"]
            .as_array()
            .unwrap()
            .clone();
        let texte = rows[0][0]["text"].as_str().unwrap();
        let libelle = texte.strip_prefix("○ 1· ").expect("préfixe attendu");
        assert_eq!(libelle.chars().count(), BUTTON_LABEL_MAX_CHARS);
    }

    #[test]
    fn libelle_option_jamais_dans_le_callback_data() {
        let long = "Super long option label that must not appear in callback";
        let body = send_body("1", &notif_choice("rid", false, &[long]));
        let data = body["reply_markup"]["inline_keyboard"][0][0]["callback_data"]
            .as_str()
            .unwrap();
        assert_eq!(data, "o:rid:0:0");
        assert!(!data.contains("Super"));
        assert!(!data.contains("option"));
    }

    #[test]
    fn callback_data_choix_tient_dans_64_octets() {
        let realiste = super::super::next_request_id();
        for data in [
            format!("a:{realiste}"),
            format!("d:{realiste}"),
            // Story 8.9 — vecteur pathologique : 4ᵉ segment + les deux indices au maximum.
            format!("o:{realiste}:255:255"),
            format!("s:{realiste}"),
        ] {
            assert!(data.len() <= 64, "trop long : {data} ({})", data.len());
        }
    }

    #[test]
    fn parse_choix_et_send() {
        let body = r#"{"ok":true,"result":[
            {"update_id":1,"callback_query":{"id":"c1","from":{"id":9},
                "message":{"chat":{"id":9}},"data":"o:run-1:0:0"}},
            {"update_id":2,"callback_query":{"id":"c2","from":{"id":9},
                "message":{"chat":{"id":9}},"data":"s:run-1"}}
        ]}"#;
        let (actions, last) = parse_action_updates(body);
        assert_eq!(last, Some(2));
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].kind, IncomingKind::Choose { question: 0, index: 0 });
        assert_eq!(actions[1].kind, IncomingKind::Confirm);
    }
}
