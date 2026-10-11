//! Rules for the admin password: normalization, length, characters, strength and leaks.

use std::sync::Arc;
use std::time::Duration;

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use sha1::{Digest, Sha1};
use unicode_normalization::UnicodeNormalization;
use zxcvbn::Score;

/// Fewer characters are refused (counted after normalization, in Unicode scalar values).
pub const MIN_LEN: usize = 12;
/// Longer passwords are refused, which also bounds the work Argon2 and zxcvbn do.
pub const MAX_LEN: usize = 128;
pub const MAX_RAW_BYTES: usize = 4096;

/// Why a password was refused. The text is shown to the admin.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum PasswordError {
    #[error("The password must have at least {MIN_LEN} characters.")]
    TooShort,
    #[error("The password must have at most {MAX_LEN} characters.")]
    TooLong,
    #[error("Each password field must have at most {MAX_RAW_BYTES} UTF-8 bytes.")]
    TooManyBytes,
    #[error("The passwords do not match.")]
    Mismatch,
    #[error("The password must not contain spaces or control characters.")]
    ForbiddenCharacter,
    #[error("The password is not valid UTF-8 text.")]
    InvalidEncoding,
    #[error("The password is too easy to guess. {0}")]
    Weak(String),
    #[error("This password appeared in a data breach. Choose another one.")]
    Breached,
}

/// Brings the password into Unicode normalization form C, so that the same text typed on
/// another device or keyboard produces the same hash. Applied when setting and when checking.
#[must_use]
pub fn normalize(raw: &str) -> String {
    raw.nfc().collect()
}

/// Checks a normalized password against the offline rules.
///
/// # Errors
///
/// Returns the first rule the password breaks.
pub fn validate(password: &str) -> Result<(), PasswordError> {
    // Form decoding replaces bytes that are not UTF-8 with U+FFFD instead of failing.
    if password.contains(char::REPLACEMENT_CHARACTER) {
        return Err(PasswordError::InvalidEncoding);
    }
    if password
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(PasswordError::ForbiddenCharacter);
    }
    let len = password.chars().count();
    if len < MIN_LEN {
        return Err(PasswordError::TooShort);
    }
    if len > MAX_LEN {
        return Err(PasswordError::TooLong);
    }
    let estimate = zxcvbn::zxcvbn(password, &["stalias", "stalwart"]);
    if estimate.score() < Score::Three {
        let hint = estimate
            .feedback()
            .map(ToString::to_string)
            .unwrap_or_default();
        return Err(PasswordError::Weak(hint.trim().to_owned()));
    }
    Ok(())
}

/// Bounds raw input before normalization; existing passwords need no strength check.
///
/// # Errors
///
/// Returns [`PasswordError::TooManyBytes`] or [`PasswordError::TooLong`].
pub fn bounded_normalize(raw: &str) -> Result<String, PasswordError> {
    if raw.len() > MAX_RAW_BYTES {
        return Err(PasswordError::TooManyBytes);
    }
    let password = normalize(raw);
    if password.chars().count() > MAX_LEN {
        return Err(PasswordError::TooLong);
    }
    Ok(password)
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessingError {
    #[error("Password processing is busy. Please try again in a moment.")]
    Busy,
    #[error("Password processing failed. Please try again.")]
    Worker,
    #[error("Password hashing failed. Please try again.")]
    Hash,
    #[error(transparent)]
    Invalid(#[from] PasswordError),
    #[error(
        "The password breach check is unavailable. Please try again later, or set HIBP_CHECK=false \
         if this server cannot reach api.pwnedpasswords.com."
    )]
    Breach(#[from] BreachCheckError),
}

/// Shared by all authentication routes. There is no application-level waiting queue.
#[derive(Clone)]
pub struct PasswordProcessor(Arc<Semaphore>);

impl Default for PasswordProcessor {
    fn default() -> Self {
        Self(Arc::new(Semaphore::new(2)))
    }
}

/// One of the expensive-work slots. A request reserves it once and keeps it for everything it
/// does, so an admitted request never fails with `Busy` halfway through. Blocking jobs hold their
/// own handle, so the slot stays taken until the worker has finished, even if the request is
/// cancelled.
pub struct PasswordWork(Arc<OwnedSemaphorePermit>);

impl PasswordProcessor {
    /// # Errors
    ///
    /// Returns [`ProcessingError::Busy`] when both slots are taken.
    pub fn reserve(&self) -> Result<PasswordWork, ProcessingError> {
        self.0
            .clone()
            .try_acquire_owned()
            .map(|permit| PasswordWork(Arc::new(permit)))
            .map_err(|_| ProcessingError::Busy)
    }
}

impl PasswordWork {
    /// Used by setup, password change and local recovery, with identical policy and HIBP behavior.
    /// The slot is held during the breach lookup, which is bounded by its timeouts.
    ///
    /// # Errors
    ///
    /// Fails when the input is out of bounds, the passwords differ, the policy or the breach
    /// check refuses the password, or hashing fails.
    pub async fn new_hash(
        &self,
        raw: String,
        confirm: String,
        check: Option<&BreachCheck>,
    ) -> Result<String, ProcessingError> {
        let password = bounded_normalize(&raw)?;
        let confirm = bounded_normalize(&confirm)?;
        if password != confirm {
            return Err(PasswordError::Mismatch.into());
        }
        let password = self
            .run(move || {
                validate(&password)?;
                Ok(password)
            })
            .await?;
        if let Some(check) = check {
            match check.check(&password).await {
                Ok(BreachStatus::NotBreached) => {}
                Ok(BreachStatus::Breached) => return Err(PasswordError::Breached.into()),
                Err(e) => {
                    // Error types contain no URL, prefix, digest or input.
                    tracing::warn!(category = %e, "password breach check failed");
                    return Err(e.into());
                }
            }
        }
        self.hash(password).await
    }

    async fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce() -> Result<T, ProcessingError> + Send + 'static,
    ) -> Result<T, ProcessingError> {
        let permit = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            job()
        })
        .await
        .map_err(|_| ProcessingError::Worker)?
    }

    /// # Errors
    ///
    /// Fails when Argon2 cannot hash the password or its worker dies.
    pub async fn hash(&self, password: String) -> Result<String, ProcessingError> {
        self.run(move || {
            Argon2::default()
                .hash_password(password.as_bytes())
                .map(|h| h.to_string())
                .map_err(|_| ProcessingError::Hash)
        })
        .await
    }

    /// `Ok(false)` means the password is wrong.
    ///
    /// # Errors
    ///
    /// Fails when the stored hash is malformed or the worker dies.
    pub async fn verify(&self, password: String, hash: String) -> Result<bool, ProcessingError> {
        self.run(move || {
            match Argon2::default().verify_password(password.as_bytes(), hash.as_str()) {
                Ok(()) => Ok(true),
                Err(argon2::password_hash::Error::PasswordInvalid) => Ok(false),
                Err(_) => Err(ProcessingError::Hash),
            }
        })
        .await
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum BreachStatus {
    NotBreached,
    Breached,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum BreachCheckError {
    #[error("timeout")]
    Timeout,
    #[error("connection")]
    Connection,
    #[error("http-status")]
    HttpStatus,
    #[error("redirect")]
    Redirect,
    #[error("invalid-response")]
    InvalidResponse,
    #[error("response-too-large")]
    TooLarge,
}

impl BreachCheckError {
    fn transport(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            Self::Timeout
        } else {
            Self::Connection
        }
    }
}

/// HIBP range client. Only a SHA-1 prefix is sent; errors never expose it.
pub struct BreachCheck {
    client: reqwest::Client,
    base: String,
}

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

impl BreachCheck {
    /// The public Have I Been Pwned range API.
    pub const DEFAULT_BASE: &'static str = "https://api.pwnedpasswords.com";

    /// Production uses HTTPS; a custom base supports local mock tests.
    ///
    /// # Errors
    ///
    /// Fails when the HTTP client cannot be built.
    pub fn new(base: &str) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            base: base.trim_end_matches('/').to_owned(),
        })
    }

    /// # Errors
    ///
    /// Fails on any network, status, size or format problem; none of them count as "not breached".
    pub async fn check(&self, password: &str) -> Result<BreachStatus, BreachCheckError> {
        let hash = hex::encode_upper(Sha1::digest(password.as_bytes()));
        let (prefix, suffix) = hash.split_at(5);
        let mut response = self
            .client
            .get(format!("{}/range/{prefix}", self.base))
            .header("Add-Padding", "true")
            .send()
            .await
            .map_err(BreachCheckError::transport)?;
        if response.status().is_redirection() {
            return Err(BreachCheckError::Redirect);
        }
        if response.status() != reqwest::StatusCode::OK {
            return Err(BreachCheckError::HttpStatus);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(BreachCheckError::transport)?
        {
            if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
                return Err(BreachCheckError::TooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        let body = std::str::from_utf8(&bytes).map_err(|_| BreachCheckError::InvalidResponse)?;
        range_status(body, suffix)
    }
}

fn range_status(body: &str, suffix: &str) -> Result<BreachStatus, BreachCheckError> {
    let mut records = 0;
    let mut status = BreachStatus::NotBreached;
    for line in body.lines() {
        let (s, count) = line
            .split_once(':')
            .ok_or(BreachCheckError::InvalidResponse)?;
        if s.len() != 35
            || !s.bytes().all(|b| b.is_ascii_hexdigit())
            || count.is_empty()
            || !count.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(BreachCheckError::InvalidResponse);
        }
        let count: u64 = count
            .parse()
            .map_err(|_| BreachCheckError::InvalidResponse)?;
        records += 1;
        if s.eq_ignore_ascii_case(suffix) && count > 0 {
            status = BreachStatus::Breached;
        }
    }
    if records == 0 {
        return Err(BreachCheckError::InvalidResponse);
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRONG: &str = "Tr0ub4dor&3-lantern-quartz";

    #[test]
    fn composed_and_decomposed_letters_normalize_equally() {
        assert_eq!(normalize("caf\u{e9}"), normalize("cafe\u{301}"));
    }

    #[test]
    fn accepts_a_strong_password_without_spaces() {
        assert_eq!(validate(&normalize(STRONG)), Ok(()));
        assert_eq!(validate("pflaume-Nebel-7-Kupfer-Zitrone"), Ok(()));
    }

    #[test]
    fn length_counts_characters_not_bytes() {
        // Twelve two-byte characters pass the length rule and reach the strength check.
        let umlauts = "ü".repeat(MIN_LEN);
        assert!(matches!(validate(&umlauts), Err(PasswordError::Weak(_))));
        assert_eq!(
            validate(&"ü".repeat(MIN_LEN - 1)),
            Err(PasswordError::TooShort)
        );
        assert_eq!(
            validate(&"a".repeat(MAX_LEN + 1)),
            Err(PasswordError::TooLong)
        );
    }

    #[test]
    fn refuses_whitespace_and_control_characters() {
        for bad in [
            "correct horse battery staple",
            "Tr0ub4dor&3-lantern\tquartz",
            "Tr0ub4dor&3-lantern\u{a0}quartz",
            "Tr0ub4dor&3-lantern\nquartz",
            "Tr0ub4dor&3-lantern\0quartz",
            "            ",
        ] {
            assert_eq!(
                validate(bad),
                Err(PasswordError::ForbiddenCharacter),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn refuses_text_that_was_not_utf8() {
        let lossy = String::from_utf8_lossy(&[0xff; 16]).into_owned();
        assert_eq!(validate(&lossy), Err(PasswordError::InvalidEncoding));
    }

    #[test]
    fn refuses_guessable_passwords() {
        for weak in [
            "password1234",
            "qwertyuiop12",
            "aaaaaaaaaaaaaaaa",
            "abcdefghijkl",
            "stalias-stalias-2",
        ] {
            assert!(
                matches!(validate(weak), Err(PasswordError::Weak(_))),
                "{weak}"
            );
        }
    }

    #[test]
    fn range_validates_every_record_even_after_a_hit() {
        let suffix = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert_eq!(
            range_status(&format!("{suffix}:3\r\n"), suffix),
            Ok(BreachStatus::Breached)
        );
        assert_eq!(
            range_status(&format!("{suffix}:0\r\n"), suffix),
            Ok(BreachStatus::NotBreached)
        );
        assert_eq!(
            range_status(&format!("{suffix}:3\r\ngarbage"), suffix),
            Err(BreachCheckError::InvalidResponse)
        );
        for body in ["", "\n", "<html>maintenance</html>", "ABC:1"] {
            assert_eq!(
                range_status(body, suffix),
                Err(BreachCheckError::InvalidResponse)
            );
        }
        for count in ["-1", "+1", "", "1:2", " 1", "18446744073709551616"] {
            assert_eq!(
                range_status(&format!("{suffix}:{count}"), suffix),
                Err(BreachCheckError::InvalidResponse)
            );
        }
    }

    #[tokio::test]
    async fn password_roundtrip_and_hash_errors_are_controlled() {
        let processor = PasswordProcessor::default();
        let work = processor.reserve().unwrap();
        // One slot serves every step of a request.
        let hash = work.hash("correct horse".into()).await.unwrap();
        assert!(
            work.verify("correct horse".into(), hash.clone())
                .await
                .unwrap()
        );
        assert!(!work.verify("wrong".into(), hash).await.unwrap());
        assert!(matches!(
            work.verify("x".into(), "bad-hash".into()).await,
            Err(ProcessingError::Hash)
        ));
        assert!(matches!(
            work.run::<()>(|| panic!("worker failed")).await,
            Err(ProcessingError::Worker)
        ));
    }

    #[tokio::test]
    async fn cancellation_keeps_the_slot_until_the_blocking_job_finishes() {
        let processor = PasswordProcessor::default();
        let second = processor.reserve().unwrap();
        let work = processor.reserve().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            work.run(move || {
                started_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
                Ok(())
            })
            .await
        });
        started_rx.await.unwrap();
        assert!(matches!(processor.reserve(), Err(ProcessingError::Busy)));
        task.abort();
        let _ = task.await;
        assert!(matches!(processor.reserve(), Err(ProcessingError::Busy)));
        finish_tx.send(()).unwrap();
        // Waiting for the permit tests actual worker completion, not an arbitrary sleep.
        let released = tokio::time::timeout(Duration::from_secs(2), processor.0.acquire())
            .await
            .unwrap()
            .unwrap();
        drop(released);
        drop(second);
        assert_eq!(processor.0.available_permits(), 2);
    }

    #[tokio::test]
    async fn exactly_two_blocking_jobs_can_run_at_once() {
        let processor = PasswordProcessor::default();
        let mut tasks = Vec::new();
        let mut finish = Vec::new();
        for _ in 0..2 {
            let work = processor.reserve().unwrap();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (finish_tx, finish_rx) = std::sync::mpsc::channel();
            tasks.push(tokio::spawn(async move {
                work.run(move || {
                    started_tx.send(()).unwrap();
                    finish_rx.recv().unwrap();
                    Ok(())
                })
                .await
            }));
            started_rx.await.unwrap();
            finish.push(finish_tx);
        }
        assert!(matches!(processor.reserve(), Err(ProcessingError::Busy)));
        for tx in finish {
            tx.send(()).unwrap();
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(processor.0.available_permits(), 2);
    }

    #[test]
    fn bounds_apply_before_and_after_nfc_without_new_password_policy() {
        assert_eq!(
            bounded_normalize(&"a".repeat(4097)),
            Err(PasswordError::TooManyBytes)
        );
        assert_eq!(
            bounded_normalize(&"a".repeat(129)),
            Err(PasswordError::TooLong)
        );
        assert_eq!(bounded_normalize("x"), Ok("x".into()));
        assert_eq!(
            bounded_normalize(&"e\u{301}".repeat(128)),
            Ok("é".repeat(128))
        );
    }

    #[test]
    fn sha1_hex_matches_the_known_digest() {
        assert_eq!(
            hex::encode_upper(Sha1::digest(b"password")),
            "5BAA61E4C9B93F3F0682250B6CF8331B7EE68FD8"
        );
    }
}
