use std::fmt;

/// Application error carrying an exit code:
/// 0 success, 1 configuration error, 2 download/network error,
/// 3 authentication error, 130 interrupted (Ctrl+C).
#[derive(Debug)]
pub enum AppError {
    Config(String),
    Download { status: u16, message: String },
    Auth(String),
    // Reached via Ctrl+C handling in main; kept for the exit-code contract.
    #[allow(dead_code)]
    Interrupted,
}

impl AppError {
    pub fn exit_code(&self) -> i32 {
        match self {
            AppError::Config(_) => 1,
            AppError::Download { .. } => 2,
            AppError::Auth(_) => 3,
            AppError::Interrupted => 130,
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::Config(message)
            | AppError::Auth(message)
            | AppError::Download { message, .. } => write!(f, "{message}"),
            AppError::Interrupted => write!(f, "Cancelled."),
        }
    }
}

impl std::error::Error for AppError {}
