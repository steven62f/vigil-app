pub struct ModelPricing {
    pub input_per_m: f64,
    pub output_per_m: f64,
    pub cache_creation_per_m: f64,
    pub cache_read_per_m: f64,
}

pub fn pricing_for_claude_model(model: &str) -> ModelPricing {
    if model.contains("opus") {
        ModelPricing { input_per_m: 15.0, output_per_m: 75.0, cache_creation_per_m: 18.75, cache_read_per_m: 1.50 }
    } else if model.contains("haiku") {
        ModelPricing { input_per_m: 0.80, output_per_m: 4.0, cache_creation_per_m: 1.0, cache_read_per_m: 0.08 }
    } else {
        ModelPricing { input_per_m: 3.0, output_per_m: 15.0, cache_creation_per_m: 3.75, cache_read_per_m: 0.30 }
    }
}

/// Grille tarifaire OpenAI (famille GPT-5) pour Codex CLI — estimation V1.
/// À reconfirmer sur https://openai.com/api/pricing ; le préfixe `~$` de l'UI signale l'approximation.
/// `cache_creation_per_m` n'est pas utilisé pour Codex (l'appel passe toujours cache_creation = 0).
pub fn pricing_for_codex_model(model: &str) -> ModelPricing {
    if model.contains("mini") {
        ModelPricing { input_per_m: 0.25, output_per_m: 2.0, cache_creation_per_m: 0.25, cache_read_per_m: 0.025 }
    } else {
        ModelPricing { input_per_m: 1.25, output_per_m: 10.0, cache_creation_per_m: 1.25, cache_read_per_m: 0.125 }
    }
}

/// Grille tarifaire xAI (famille Grok) pour Grok Build — estimation V1.
/// À reconfirmer sur la doc pricing xAI ; le préfixe `~$` de l'UI signale l'approximation.
/// `cache_creation_per_m` n'est pas utilisé pour Grok (l'appel passe toujours cache_creation = 0).
/// Clé = sous-chaîne du `model_id` : les modèles « code » (ex. `grok-code-fast`) sont moins chers.
pub fn pricing_for_grok_model(model: &str) -> ModelPricing {
    if model.contains("code") {
        ModelPricing { input_per_m: 0.20, output_per_m: 1.50, cache_creation_per_m: 0.20, cache_read_per_m: 0.05 }
    } else {
        ModelPricing { input_per_m: 3.0, output_per_m: 15.0, cache_creation_per_m: 3.0, cache_read_per_m: 0.75 }
    }
}

/// Grille tarifaire Kimi CLI (Moonshot) — estimation V1 (Story 9.4, §5, décision (a)).
/// À reconfirmer sur https://platform.kimi.ai/docs/pricing/chat ; le préfixe `~$` de l'UI signale
/// l'approximation (comme Codex/Grok).
///
/// **Le CAS GÉNÉRAL — et donc le défaut — est l'utilisateur qui consomme les modèles Moonshot Kimi
/// via l'API payante** (décision Steven : ne PAS pricer sur son cas local, qui est l'exception). Grille
/// ancrée sur **Kimi K2.6** (lignée courante juil. 2026 : K3 / K2.7 Code / K2.6 / Moonshot V1) :
/// input ~$0.95/M (cache-miss), output ~$4.00/M, cache-hit ~$0.16/M. `cache_creation` = tarif input
/// (Moonshot facture l'écriture de cache au prix input plein, puis le hit au rabais).
///
/// **Garde-fou d'honnêteté** : un modèle **auto-hébergé** (LM Studio / Ollama / llama.cpp / vLLM /
/// localhost, reconnu au préfixe de provider du champ `usage.record.model`, ex. `lmstudio/qwen/…`,
/// détection insensible à la casse) n'a **AUCUN coût d'API** → grille à
/// zéro. La colonne coût de Vigil traque des dollars d'API ; afficher un tarif Moonshot sur un modèle
/// local serait le même « plausible mais FAUX » que §5 proscrit. (Retirer cette branche = pricer tout
/// au tarif Moonshot, un seul `if` à supprimer.)
///
/// Le bras `"kimi"` EXPLICITE dans `calculate_cost_for_tool` reste l'essentiel : sans lui, Kimi
/// retomberait EN SILENCE sur `pricing_for_claude_model` (modèle Moonshot facturé au tarif Anthropic).
pub fn pricing_for_kimi_model(model: &str) -> ModelPricing {
    // Modèle auto-hébergé → aucun coût d'API. Détection insensible à la casse (ex. `LMStudio/…`) sur
    // les préfixes de provider local courants ; tout le reste → grille Moonshot (défaut plausible).
    let m = model.to_lowercase();
    const LOCAL_PROVIDERS: [&str; 7] = ["lmstudio", "lm-studio", "ollama", "llama.cpp", "llamacpp", "vllm", "localhost"];
    if LOCAL_PROVIDERS.iter().any(|p| m.contains(p)) {
        return ModelPricing { input_per_m: 0.0, output_per_m: 0.0, cache_creation_per_m: 0.0, cache_read_per_m: 0.0 };
    }
    // Tarif Moonshot standard (Kimi K2.6, estimation V1).
    ModelPricing { input_per_m: 0.95, output_per_m: 4.0, cache_creation_per_m: 0.95, cache_read_per_m: 0.16 }
}

/// Coût à partir d'une grille tarifaire et des volumes de tokens.
fn cost_from_pricing(
    p: &ModelPricing,
    input: u64, output: u64,
    cache_creation: u64, cache_read: u64,
) -> f64 {
    (input as f64 * p.input_per_m
        + output as f64 * p.output_per_m
        + cache_creation as f64 * p.cache_creation_per_m
        + cache_read as f64 * p.cache_read_per_m)
        / 1_000_000.0
}

/// Coût Anthropic (Claude) — INCHANGÉ (chemin par défaut, tests 4.13 intacts).
pub fn calculate_cost(
    input: u64, output: u64,
    cache_creation: u64, cache_read: u64,
    model: &str,
) -> f64 {
    cost_from_pricing(&pricing_for_claude_model(model), input, output, cache_creation, cache_read)
}

/// Coût tool-aware : grille OpenAI pour `"codex"`, sinon délègue à la grille Anthropic.
pub fn calculate_cost_for_tool(
    tool: &str,
    input: u64, output: u64,
    cache_creation: u64, cache_read: u64,
    model: &str,
) -> f64 {
    let p = match tool {
        "codex" => pricing_for_codex_model(model),
        "grok" => pricing_for_grok_model(model),
        // Kimi — grille Moonshot (Story 9.4 §5). Bras EXPLICITE et non le `_` Claude : sans lui, un
        // modèle Moonshot serait facturé au tarif Anthropic (faux, silencieux). Modèle local → 0.
        "kimi" => pricing_for_kimi_model(model),
        _ => pricing_for_claude_model(model),
    };
    cost_from_pricing(&p, input, output, cache_creation, cache_read)
}
