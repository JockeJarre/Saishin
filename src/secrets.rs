use crate::config::AppConfig;
use crate::error::AppError;
use crate::{log, log::warn};
use std::io::{self, IsTerminal, Read};
use std::sync::atomic::{AtomicBool, Ordering};

// The token lives in the OS secure store under "{keyring_service}.github/token-rs"
// (service plus user name; on Windows that is the Credential Manager target).
// Older retired builds used a neighbouring "{service}.github/token" entry whose
// secret bytes are UTF-8, while this crate reads and writes UTF-16 — sharing
// one entry would hand both sides garbage, so this entry is kept separate and
// the old one is never touched. Run `saishin --login` once to create it;
// SAISHIN_TOKEN / GITHUB_TOKEN keep working without any of this mattering.
const TOKEN_SERVICE: &str = "github";
const TOKEN_USER: &str = "token-rs";

static WARNED_ABOUT_KEYRING: AtomicBool = AtomicBool::new(false);

pub fn from_environment() -> Option<String> {
    for name in ["SAISHIN_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(value) = std::env::var(name) {
            let trimmed = value.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

fn entry(config: &AppConfig) -> Result<keyring::Entry, keyring::Error> {
    let target = format!("{}.{TOKEN_SERVICE}/{TOKEN_USER}", config.keyring_service);
    keyring::Entry::new_with_target(&target, &config.keyring_service, TOKEN_USER)
}

pub fn from_keyring(config: &AppConfig) -> Option<String> {
    let entry = match entry(config) {
        Ok(entry) => entry,
        Err(_) => {
            warn_no_keyring();
            return None;
        }
    };
    match entry.get_password() {
        Ok(password) => Some(password),
        Err(ex) => {
            match ex {
                keyring::Error::NoEntry => None,
                keyring::Error::NoStorageAccess(_) => {
                    warn_no_keyring();
                    None
                }
                _ => {
            // A missing native backend warns once, other store
            // failures degrade to anonymous access.
                    warn_no_keyring();
                    None
                }
            }
        }
    }
}

pub fn store(config: &AppConfig, token: &str) -> Result<(), AppError> {
    let entry = entry(config).map_err(|_| {
        AppError::Auth(
            "No secure credential store is available on this machine, so the token cannot be saved. \
             Export GITHUB_TOKEN instead, for example from your scheduler's EnvironmentFile."
                .to_string(),
        )
    })?;
    entry.set_password(token).map_err(|ex| {
        let first = first_line(&ex.to_string());
        // Heuristic: platform errors mentioning missing backends/daemons map
        // to the missing-backend case.
        let lowered = first.to_lowercase();
        if lowered.contains("no storage")
            || lowered.contains("not available")
            || lowered.contains("dbus")
            || lowered.contains("service unknown")
        {
            AppError::Auth(
                "No secure credential store is available on this machine, so the token cannot be saved. \
                 Export GITHUB_TOKEN instead, for example from your scheduler's EnvironmentFile."
                    .to_string(),
            )
        } else {
            AppError::Auth(format!(
                "Could not write to the secure credential store: {first}"
            ))
        }
    })
}

pub fn remove(config: &AppConfig) {
    let entry = match entry(config) {
        Ok(entry) => entry,
        Err(_) => {
            log::info("No secure credential store is available on this machine, so there is nothing to remove.");
            return;
        }
    };
    match entry.delete_credential() {
        Ok(()) => log::info("Stored GitHub token removed from the keyring."),
        Err(keyring::Error::NoEntry) => log::info("No stored GitHub token found."),
        Err(_) => log::info("No stored GitHub token found."),
    }
}

pub fn prompt_for_token() -> Result<String, AppError> {
    log::info("");
    log::info("A GitHub personal access token is required.");
    log::info("Create one at https://github.com/settings/tokens: 'Generate new token (classic)'.");
    log::info("Tick 'public_repo': it reads public repositories and their build downloads.");
    log::info("Add 'repo' for a private repository. A fine-grained token reaches only");
    log::info("the repositories it was granted on, so it cannot read one you do not control.");
    log::info("It is stored in this machine's secure credential store and never written to disk by saishin.");

    if !io::stdin().is_terminal() {
        let mut piped = String::new();
        io::stdin()
            .read_to_string(&mut piped)
            .map_err(|_| AppError::Auth("No token was provided on stdin.".to_string()))?;
        let trimmed = piped.trim().to_string();
        if trimmed.is_empty() {
            return Err(AppError::Auth("No token was provided on stdin.".to_string()));
        }
        return Ok(trimmed);
    }

    print!("Token: ");
    use std::io::Write as _;
    let _ = io::stdout().flush();
    let token = rpassword::read_password().unwrap_or_default();
    let trimmed = token.trim().to_string();
    if trimmed.is_empty() {
        return Err(AppError::Auth("No token entered.".to_string()));
    }
    Ok(trimmed)
}

fn warn_no_keyring() {
    if WARNED_ABOUT_KEYRING.swap(true, Ordering::Relaxed) {
        return;
    }
    warn(
        "No secure credential store is available on this machine, so no token can be read or stored. \
         Set GITHUB_TOKEN (or SAISHIN_TOKEN) for private sources.",
    );
}

fn first_line(message: &str) -> String {
    message
        .split(['\n', '\r'])
        .next()
        .unwrap_or(message)
        .trim()
        .to_string()
}
