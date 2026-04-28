/// Provider-specific OAuth endpoint configuration.
pub struct ProviderOAuthConfig {
    pub name: &'static str,
    pub auth_url: &'static str,
    pub token_url: &'static str,
    pub scopes: &'static [&'static str],
    /// Extra query params to append to the authorization URL.
    /// Dropbox needs `token_access_type=offline` to get a refresh token.
    pub extra_auth_params: &'static [(&'static str, &'static str)],
}

pub const GDRIVE_CONFIG: ProviderOAuthConfig = ProviderOAuthConfig {
    name: "gdrive",
    auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: "https://oauth2.googleapis.com/token",
    scopes: &["https://www.googleapis.com/auth/drive.readonly"],
    extra_auth_params: &[
        ("access_type", "offline"), // request refresh_token
        ("prompt", "consent"),      // force consent to guarantee refresh_token on re-auth
    ],
};

pub const DROPBOX_CONFIG: ProviderOAuthConfig = ProviderOAuthConfig {
    name: "dropbox",
    auth_url: "https://www.dropbox.com/oauth2/authorize",
    token_url: "https://api.dropboxapi.com/oauth2/token",
    scopes: &["sharing.read", "files.content.read"],
    extra_auth_params: &[
        ("token_access_type", "offline"), // required for refresh_token
    ],
};

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    #[test]
    fn configs_are_valid_urls() {
        for config in &[&GDRIVE_CONFIG, &DROPBOX_CONFIG] {
            Url::parse(config.auth_url).expect("invalid auth_url");
            Url::parse(config.token_url).expect("invalid token_url");
        }
    }
}
