//! Admin password hashing, login sessions and brute-force protection for the web UI.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use axum::response::Redirect;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};

use crate::state::AppState;

/// Name of the cookie that holds the admin session id.
pub const SESSION_COOKIE: &str = "stalias_session";

const MAX_FAILURES: usize = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// Upper bound on tracked clients, so a flood of spoofed addresses cannot grow the map forever.
const MAX_TRACKED_CLIENTS: usize = 10_000;

/// Argon2 is deliberately slow, so it runs on the blocking pool instead of an async worker.
pub async fn hash_password(password: String) -> String {
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes())
            .expect("hashing with default parameters cannot fail")
            .to_string()
    })
    .await
    .expect("hashing task panicked")
}

/// Checks a password against an Argon2 hash. A malformed hash never matches.
pub async fn verify_password(password: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .verify_password(password.as_bytes(), hash.as_str())
            .is_ok()
    })
    .await
    .expect("hashing task panicked")
}

/// Compares secrets without stopping at the first differing byte.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// The address a request came from. `X-Forwarded-For` is only believed when the operator
/// says a reverse proxy sits in front (`TRUST_PROXY`); otherwise anybody could pick their
/// own address and dodge the login limit.
pub struct ClientIp(pub IpAddr);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Infallible> {
        let forwarded = state
            .trust_proxy
            .then(|| {
                // The last entry is the one our own proxy appended; earlier ones are client-supplied.
                // A proxy may add its own header line instead of extending the existing one, so
                // the last line counts, not the first.
                parts
                    .headers
                    .get_all("x-forwarded-for")
                    .iter()
                    .next_back()
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.rsplit(',').next())
                    .and_then(|v| v.trim().parse().ok())
            })
            .flatten();
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip());
        Ok(Self(
            forwarded
                .or(peer)
                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        ))
    }
}

/// Counts failed and in-flight logins per client address.
#[derive(Default)]
pub struct LoginLimiter {
    clients: Mutex<HashMap<IpAddr, ClientAttempts>>,
}

#[derive(Default)]
struct ClientAttempts {
    failures: VecDeque<Instant>,
    in_flight: usize,
}

/// Reservations are held across password verification. Cancellation counts as a failure.
pub struct LoginAttempt<'a> {
    limiter: &'a LoginLimiter,
    client: IpAddr,
    failed: bool,
}

impl LoginAttempt<'_> {
    /// Marks the attempt as successful, so it does not count as a failure.
    pub fn success(&mut self) {
        self.failed = false;
    }
}

impl Drop for LoginAttempt<'_> {
    fn drop(&mut self) {
        let mut all = self
            .limiter
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let attempts = all.get_mut(&self.client).expect("reserved client exists");
        attempts.in_flight -= 1;
        prune(&mut attempts.failures);
        if self.failed {
            attempts.failures.push_back(Instant::now());
        }
        if attempts.in_flight == 0 && attempts.failures.is_empty() {
            all.remove(&self.client);
        }
    }
}

impl LoginLimiter {
    /// Checks the budget and reserves a slot under the same mutex.
    pub fn try_attempt(&self, client: IpAddr) -> Option<LoginAttempt<'_>> {
        let mut all = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if all.len() >= MAX_TRACKED_CLIENTS && !all.contains_key(&client) {
            all.retain(|_, attempts| {
                prune(&mut attempts.failures);
                attempts.in_flight > 0 || !attempts.failures.is_empty()
            });
            if all.len() >= MAX_TRACKED_CLIENTS {
                return None;
            }
        }
        let attempts = all.entry(client).or_default();
        prune(&mut attempts.failures);
        if attempts.failures.len() + attempts.in_flight >= MAX_FAILURES {
            return None;
        }
        attempts.in_flight += 1;
        Some(LoginAttempt {
            limiter: self,
            client,
            failed: true,
        })
    }
}

fn prune(failures: &mut VecDeque<Instant>) {
    while failures
        .front()
        .is_some_and(|t| t.elapsed() > FAILURE_WINDOW)
    {
        failures.pop_front();
    }
}

/// The login cookie: HTTP-only, `SameSite=Lax`, valid for seven days. `secure` adds the `Secure` flag.
pub fn session_cookie(value: String, secure: bool) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, value))
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(secure)
        .path("/")
        .max_age(time::Duration::days(7))
        .build()
}

/// True when the request reached us over HTTPS through a reverse proxy.
pub fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"))
}

/// Extractor for pages that need a logged-in admin. Unauthenticated visitors are
/// sent to the signup page until an admin exists, and to the login page afterwards.
pub struct Admin;

impl FromRequestParts<AppState> for Admin {
    type Rejection = Redirect;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Redirect> {
        let jar = CookieJar::from_headers(&parts.headers);
        let logged_in = jar.get(SESSION_COOKIE).map(|c| c.value().to_owned());
        let logged_in = match logged_in {
            Some(session) => state.store.session_valid(&session).await.unwrap_or(false),
            None => false,
        };
        if logged_in {
            return Ok(Admin);
        }
        let has_admin = state
            .store
            .settings()
            .await
            .is_ok_and(|c| c.admin_hash.is_some());
        Err(Redirect::to(if has_admin { "/login" } else { "/setup" }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    const B: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));

    #[tokio::test]
    async fn password_roundtrip() {
        let hash = hash_password("correct horse".into()).await;
        assert!(verify_password("correct horse".into(), hash.clone()).await);
        assert!(!verify_password("wrong".into(), hash).await);
        assert!(!verify_password("x".into(), "not-a-hash".into()).await);
    }

    #[test]
    fn limiter_blocks_only_the_failing_client() {
        let limiter = LoginLimiter::default();
        for _ in 0..MAX_FAILURES - 1 {
            drop(limiter.try_attempt(A).unwrap());
        }
        drop(limiter.try_attempt(A).unwrap());
        assert!(limiter.try_attempt(A).is_none());
        assert!(limiter.try_attempt(B).is_some());
    }

    #[test]
    fn running_attempts_reserve_slots_and_success_releases_them() {
        let limiter = LoginLimiter::default();
        let mut attempts: Vec<_> = (0..MAX_FAILURES)
            .map(|_| limiter.try_attempt(A).unwrap())
            .collect();
        assert!(limiter.try_attempt(A).is_none());
        attempts.last_mut().unwrap().success();
        drop(attempts.pop());
        assert!(limiter.try_attempt(A).is_some());
        // The remaining canceled attempts become failures, so capacity stays bounded.
        drop(attempts);
        assert!(limiter.try_attempt(A).is_none());
    }

    #[test]
    fn expired_failures_are_pruned() {
        let limiter = LoginLimiter::default();
        let mut attempts = limiter.clients.lock().unwrap();
        attempts.insert(
            A,
            ClientAttempts {
                failures: VecDeque::from(
                    [Instant::now() - FAILURE_WINDOW - Duration::from_secs(1); MAX_FAILURES],
                ),
                in_flight: 0,
            },
        );
        drop(attempts);
        assert!(limiter.try_attempt(A).is_some());
    }

    #[test]
    fn constant_time_eq_matches_normal_equality() {
        assert!(constant_time_eq("abc123", "abc123"));
        assert!(!constant_time_eq("abc123", "abc124"));
        assert!(!constant_time_eq("abc", "abc123"));
    }
}
