pub fn auth_token_from_env() -> Option<String> {
    std::env::var("ANTHROPIC_AUTH_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
