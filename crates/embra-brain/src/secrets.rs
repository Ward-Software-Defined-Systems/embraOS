//! The operator's stored secrets, collected by value for the outbound
//! guard. The dispatcher refuses the input of an outbound tool that carries
//! one (`tools/registry.rs::OUTBOUND_TOOLS`), and the Guardian broker
//! refuses a request that carries one (`Capabilities.outbound_secrets`),
//! both naming the kind and never the value. The set is collected per
//! dispatch — the configuration, two small STATE files and the environment —
//! so a key stored a moment ago is in it.
//!
//! A secret the operator pasted into the conversation and never stored is
//! not in the set; with the read jail (`tools/jail.rs::readable`) a stored
//! one cannot reach the model's hands in the first place. The provider sees
//! the conversation regardless; that boundary is documented, not closed.

use embra_tools_core::SecretSet;

use crate::config::SystemConfig;

/// Every stored secret the OS holds, in the forms it could take on the wire.
pub(crate) fn collect(cfg: Option<&SystemConfig>) -> SecretSet {
    let mut set = SecretSet::new();
    if let Some(cfg) = cfg {
        set.add("the provider API key", &cfg.api_key);
        for key in [&cfg.anthropic_api_key, &cfg.gemini_api_key].into_iter().flatten() {
            set.add("the provider API key", key);
        }
        if let Some(token) = &cfg.github_token {
            set.add("the GitHub token", token);
        }
        for token in cfg.git_tokens.iter().flat_map(|hosts| hosts.values()) {
            set.add("a git host token", token);
        }
        for value in cfg
            .guardian_secrets
            .iter()
            .flat_map(|hosts| hosts.values())
            .flat_map(|headers| headers.values())
        {
            set.add("a /guardian secret", value);
        }
        if let Some((key, _source)) = crate::provider::image::resolve_image_key(cfg) {
            set.add("the image key", &key);
        }
    }
    if let Some(key) = crate::guardian::read_brave_key() {
        set.add("the Brave key", &key);
    }
    for (var, kind) in [
        ("GITHUB_TOKEN", "the GitHub token"),
        ("ANTHROPIC_API_KEY", "the provider API key"),
        (crate::provider::image::IMAGE_KEY_ENV, "the image key"),
        ("EMBRA_OLLAMA_BEARER", "a provider bearer"),
        ("EMBRA_LM_STUDIO_BEARER", "a provider bearer"),
    ] {
        if let Ok(value) = std::env::var(var) {
            set.add(kind, &value);
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::collect;
    use crate::config::SystemConfig;

    // Deserialized, not a struct literal: see `learning::phases` tests.
    fn config() -> SystemConfig {
        serde_json::from_value(serde_json::json!({
            "name": "Embra",
            "api_key": "sk-ant-api03-0123456789abcdef",
            "anthropic_api_key": "sk-ant-second-0123456789",
            "gemini_api_key": "AIzaSy-gemini-0123456789",
            "github_token": "ghp_0123456789abcdefghij",
            "git_tokens": {"gitlab.ops.wsds": "glpat-0123456789abcdef"},
            "guardian_secrets": {"graph.microsoft.com": {"Authorization": "Bearer eyJ0eXAiOiJKV1Qi.body"}},
            "timezone": "UTC",
            "deployment_mode": "phase1",
            "created_at": "",
            "version": "test"
        }))
        .expect("config deserializes")
    }

    #[test]
    fn every_configured_secret_is_found_by_its_kind_and_never_by_its_value() {
        let set = collect(Some(&config()));
        for (text, kind) in [
            ("key sk-ant-api03-0123456789abcdef here", "the provider API key"),
            ("sk-ant-second-0123456789", "the provider API key"),
            ("AIzaSy-gemini-0123456789", "the provider API key"),
            ("token: ghp_0123456789abcdefghij", "the GitHub token"),
            ("glpat-0123456789abcdef", "a git host token"),
            ("Bearer eyJ0eXAiOiJKV1Qi.body", "a /guardian secret"),
            ("eyJ0eXAiOiJKV1Qi.body", "a /guardian secret"),
            // base64 of the GitHub token
            ("Z2hwXzAxMjM0NTY3ODlhYmNkZWZnaGlq", "the GitHub token"),
        ] {
            let found = set.found_in(text);
            assert_eq!(found, Some(kind), "{text}");
            assert!(!text.contains(found.unwrap()), "the kind is a phrase, not the value");
        }
        assert_eq!(set.found_in("an ordinary comment"), None);
    }

    #[test]
    fn a_test_config_with_a_one_letter_key_adds_nothing_and_no_config_is_an_empty_set() {
        let cfg: SystemConfig = serde_json::from_value(serde_json::json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
            "created_at": "", "version": "test"
        }))
        .unwrap();
        // The environment may carry a real key on a developer host; the
        // assertion is on the configured value alone.
        assert_eq!(collect(Some(&cfg)).found_in("k"), None);
        assert_eq!(collect(None).found_in("sk-ant-api03-0123456789abcdef"), None);
    }
}
