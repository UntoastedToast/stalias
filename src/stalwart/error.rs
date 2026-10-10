/// reqwest's message hides the cause (e.g. an untrusted certificate), so list the whole chain.
fn chain(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Why a request to Stalwart failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not reach Stalwart: {}", chain(.0))]
    Http(#[from] reqwest::Error),
    #[error("Stalwart rejected the credentials")]
    Unauthorized,
    #[error("the API key lacks a permission: {0}")]
    Forbidden(String),
    #[error("this does not look like Stalwart 0.16 or newer: {0}")]
    Unsupported(String),
    #[error("address is already in use")]
    Conflict,
    #[error("Stalwart returned an error ({kind}): {description}")]
    Jmap { kind: String, description: String },
    #[error("unexpected response from Stalwart: {0}")]
    Protocol(String),
}
