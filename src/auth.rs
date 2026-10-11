//! Admin password hashing, login sessions and brute-force protection for the web UI.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use axum::response::Redirect;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};

use crate::state::AppState;

/// Name of the cookie that holds the admin session id.
pub const SESSION_COOKIE: &str = "stalias_session";

const MAX_FAILURES: usize = 10;
const MAX_ACCOUNT_FAILURES: usize = 20;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// Upper bound on tracked clients, so a flood of spoofed addresses cannot grow the map forever.
const MAX_TRACKED_CLIENTS: usize = 10_000;

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

/// Atomically counts IP and account failures, including running checks.
#[derive(Default)]
pub struct LoginLimiter {
    attempts: Mutex<Attempts>,
}

#[derive(Default)]
struct Attempts {
    clients: HashMap<IpAddr, ClientAttempts>,
    account: ClientAttempts,
    next_id: u64,
}

#[derive(Default)]
struct ClientAttempts {
    failures: VecDeque<Instant>,
    running: HashMap<u64, Instant>,
}

impl ClientAttempts {
    fn prune(&mut self, now: Instant) {
        self.failures
            .retain(|t| now.duration_since(*t) < FAILURE_WINDOW);
    }

    fn retry_after(&self, limit: usize, now: Instant) -> Option<u64> {
        if self.failures.len() + self.running.len() < limit {
            return None;
        }
        let earliest = self.failures.iter().chain(self.running.values()).min()?;
        let remaining = FAILURE_WINDOW.saturating_sub(now.duration_since(*earliest));
        Some((remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0)).max(1))
    }
}

#[derive(Debug)]
pub struct RateLimit {
    pub retry_after: u64,
}

/// Cancellation of an admitted authentication check records a failure.
pub struct LoginAttempt {
    limiter: Arc<LoginLimiter>,
    client: IpAddr,
    id: u64,
}

impl LoginAttempt {
    /// Clears the account and this client's failures, retaining concurrent checks.
    pub fn success(self) {
        self.finish(true, Instant::now());
    }

    fn finish(&self, success: bool, now: Instant) {
        let mut all = self
            .limiter
            .attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Removing the reservation makes a following Drop a no-op.
        if all.account.running.remove(&self.id).is_none() {
            return;
        }
        let client = all
            .clients
            .get_mut(&self.client)
            .expect("reserved client exists");
        client.running.remove(&self.id);
        client.prune(now);
        if success {
            client.failures.clear();
        } else {
            client.failures.push_back(now);
        }
        all.account.prune(now);
        if success {
            all.account.failures.clear();
        } else {
            all.account.failures.push_back(now);
        }
    }
}

impl Drop for LoginAttempt {
    fn drop(&mut self) {
        self.finish(false, Instant::now());
    }
}

impl LoginLimiter {
    /// Both budgets are reserved under one lock. Rejections never modify their histories.
    ///
    /// # Errors
    ///
    /// Returns when the client or the account used up its budget, with the wait to show.
    pub fn try_attempt(self: &Arc<Self>, client: IpAddr) -> Result<LoginAttempt, RateLimit> {
        self.try_at(client, Instant::now())
    }

    fn try_at(self: &Arc<Self>, client: IpAddr, now: Instant) -> Result<LoginAttempt, RateLimit> {
        let mut all = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        all.account.prune(now);
        all.clients.retain(|_, attempts| {
            attempts.prune(now);
            !attempts.running.is_empty() || !attempts.failures.is_empty()
        });
        let ip_wait = all
            .clients
            .get(&client)
            .and_then(|c| c.retry_after(MAX_FAILURES, now));
        let account_wait = all.account.retry_after(MAX_ACCOUNT_FAILURES, now);
        if let Some(retry_after) = ip_wait.into_iter().chain(account_wait).max() {
            return Err(RateLimit { retry_after });
        }
        if all.clients.len() >= MAX_TRACKED_CLIENTS && !all.clients.contains_key(&client) {
            return Err(RateLimit { retry_after: 1 });
        }
        let id = all.next_id;
        all.next_id += 1;
        all.account.running.insert(id, now);
        all.clients
            .entry(client)
            .or_default()
            .running
            .insert(id, now);
        Ok(LoginAttempt {
            limiter: self.clone(),
            client,
            id,
        })
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

    #[test]
    fn rotating_ips_and_running_checks_share_twenty_slots() {
        let limiter = Arc::new(LoginLimiter::default());
        let now = Instant::now();
        let attempts: Vec<_> = (1..=20)
            .map(|n| {
                limiter
                    .try_at(IpAddr::V4(Ipv4Addr::new(192, 0, 2, n)), now)
                    .unwrap()
            })
            .collect();
        assert_eq!(limiter.try_at(B, now).err().unwrap().retry_after, 60);
        drop(attempts);
        assert!(limiter.try_attempt(B).is_err());
    }

    #[test]
    fn expiry_and_retry_after_do_not_move_on_rejection() {
        let limiter = Arc::new(LoginLimiter::default());
        let now = Instant::now();
        for _ in 0..10 {
            let attempt = limiter.try_at(A, now).unwrap();
            attempt.finish(false, now);
        }
        assert_eq!(
            limiter
                .try_at(A, now + Duration::from_millis(10500))
                .err()
                .unwrap()
                .retry_after,
            50
        );
        assert_eq!(
            limiter
                .try_at(A, now + Duration::from_secs(59))
                .err()
                .unwrap()
                .retry_after,
            1
        );
        assert!(limiter.try_at(A, now + FAILURE_WINDOW).is_ok());
    }

    #[test]
    fn success_clears_failures_without_releasing_other_running_checks() {
        let limiter = Arc::new(LoginLimiter::default());
        for _ in 0..9 {
            drop(limiter.try_attempt(A).unwrap());
        }
        let attempt = limiter.try_attempt(A).unwrap();
        assert!(limiter.try_attempt(A).is_err());
        attempt.success();
        let attempts: Vec<_> = (0..10).map(|_| limiter.try_attempt(A).unwrap()).collect();
        assert!(limiter.try_attempt(A).is_err());
        drop(attempts);
        assert!(limiter.try_attempt(A).is_err());
        assert!(limiter.try_attempt(B).is_ok());
    }

    #[test]
    fn retry_after_covers_both_budgets_and_account_expiry() {
        let limiter = Arc::new(LoginLimiter::default());
        let now = Instant::now();
        // A's older IP failures survive a successful login from B.
        for _ in 0..10 {
            limiter.try_at(A, now).unwrap().finish(false, now);
        }
        limiter.try_at(B, now).unwrap().finish(true, now);
        for n in 3..23 {
            limiter
                .try_at(IpAddr::V4(Ipv4Addr::new(192, 0, 2, n)), now)
                .unwrap()
                .finish(false, now + Duration::from_secs(10));
        }
        assert_eq!(
            limiter
                .try_at(A, now + Duration::from_secs(20))
                .err()
                .unwrap()
                .retry_after,
            50
        );
        assert_eq!(
            limiter
                .try_at(A, now + Duration::from_secs(60))
                .err()
                .unwrap()
                .retry_after,
            10
        );
        assert!(limiter.try_at(A, now + Duration::from_secs(70)).is_ok());
    }

    #[test]
    fn parallel_reservations_cannot_bypass_either_budget() {
        let limiter = Arc::new(LoginLimiter::default());
        let barrier = Arc::new(std::sync::Barrier::new(40));
        let attempts: Vec<_> = std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..40)
                .map(|n| {
                    let limiter = limiter.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        limiter.try_attempt(IpAddr::V4(Ipv4Addr::new(192, 0, 2, n)))
                    })
                })
                .collect();
            tasks.into_iter().map(|t| t.join().unwrap()).collect()
        });
        assert_eq!(attempts.iter().filter(|a| a.is_ok()).count(), 20);
    }

    #[test]
    fn constant_time_eq_matches_normal_equality() {
        assert!(constant_time_eq("abc123", "abc123"));
        assert!(!constant_time_eq("abc123", "abc124"));
        assert!(!constant_time_eq("abc", "abc123"));
    }
}
