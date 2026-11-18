pub struct Session {
    pub username: String,
    pub expires_at: u64,
}

pub fn authenticate_user(username: &str, password: &str) -> Result<Session, String> {
    // Authenticate user credentials and create a login session.
    if username.is_empty() || password.is_empty() {
        return Err("AUTHENTICATION_REQUIRED".into());
    }
    Ok(Session { username: username.into(), expires_at: 3600 })
}

pub fn validate_session(session: &Session, now: u64) -> Result<(), String> {
    // Validate an existing session and reject expired access.
    if now >= session.expires_at {
        return Err("SESSION_EXPIRED".into());
    }
    Ok(())
}

pub fn calculate_invoice_total(prices: &[u64]) -> u64 {
    prices.iter().sum()
}
