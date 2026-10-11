//! Password policy, overload, breach failures and transactional credential races.
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sha1::{Digest, Sha1};
use stalias::auth::LoginLimiter;
use stalias::password::{BreachCheck, BreachCheckError, BreachStatus, PasswordProcessor};
use stalias::state::AppState;
use stalias::store::Store;
use stalias::web;
use wiremock::matchers::{header as match_header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::{Reply, form, get, send};

const OLD: &str = "pflaume-Nebel-7-Kupfer-Zitrone";
const NEW: &str = "Quartz-lantern-42-Cobalt-Willow";

async fn hash(password: &str) -> String {
    PasswordProcessor::default()
        .reserve()
        .unwrap()
        .hash(password.into())
        .await
        .unwrap()
}

struct Harness {
    app: Router,
    state: AppState,
    hash: String,
    sessions: Vec<String>,
    token: String,
}

async fn harness(check: Option<&str>) -> Harness {
    let store = Store::in_memory().unwrap();
    let hash = hash(OLD).await;
    store.create_admin(hash.clone()).await.unwrap();
    let sessions = vec![
        store.create_session(hash.clone()).await.unwrap().unwrap(),
        store.create_session(hash.clone()).await.unwrap().unwrap(),
    ];
    let token = store
        .create_token("manager".into(), "a".into(), "alice".into())
        .await
        .unwrap();
    let mut state = AppState::new(store, Some("CODE".into()), true)
        .await
        .unwrap();
    if let Some(base) = check {
        state = state.with_breach_check(BreachCheck::new(base).unwrap());
    }
    Harness {
        app: web::router(state.clone()),
        state,
        hash,
        sessions,
        token,
    }
}

impl Harness {
    fn cookie(&self) -> String {
        format!("stalias_session={}", self.sessions[0])
    }
    async fn change(&self, current: &str, new: &str, confirm: &str) -> Reply {
        send(
            &self.app,
            form(
                "/settings/password",
                Some(&self.cookie()),
                &[
                    ("current_password", current),
                    ("password", new),
                    ("confirm", confirm),
                ],
            ),
        )
        .await
    }
    async fn unchanged(&self) {
        assert_eq!(
            self.state.store.settings().await.unwrap().admin_hash,
            Some(self.hash.clone())
        );
        for session in &self.sessions {
            assert!(self.state.store.session_valid(session).await.unwrap());
        }
        assert!(
            self.state
                .store
                .find_token(&self.token)
                .await
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test]
async fn password_change_works_during_setup_and_revokes_all_sessions_only() {
    let h = harness(None).await;
    assert!(
        send(&h.app, get("/settings", Some(&h.cookie())))
            .await
            .body
            .contains("/settings/password")
    );
    let page = send(&h.app, get("/settings/password", Some(&h.cookie()))).await;
    assert_eq!(page.status, 200);
    assert!(page.body.contains("autocomplete=\"current-password\""));
    assert!(page.body.contains("autocomplete=\"new-password\""));
    assert_eq!(
        send(&h.app, get("/settings/password", None))
            .await
            .location(),
        "/login"
    );
    for (current, new, confirm, status) in [
        ("wrong", NEW, NEW, 401),
        (OLD, "short", "short", 422),
        (OLD, "password1234", "password1234", 422),
        (OLD, NEW, OLD, 422),
    ] {
        let reply = h.change(current, new, confirm).await;
        assert_eq!(reply.status, status);
        assert!(!reply.body.contains(&format!("value=\"{current}\"")));
        assert!(!reply.body.contains(&format!("value=\"{new}\"")));
        h.unchanged().await;
    }
    let reply = h.change(OLD, NEW, NEW).await;
    assert_eq!(reply.status, 303);
    assert_eq!(reply.location(), "/login?password_changed=1");
    assert!(
        reply.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    for session in &h.sessions {
        assert!(!h.state.store.session_valid(session).await.unwrap());
    }
    assert!(h.state.store.find_token(&h.token).await.unwrap().is_some());
    assert_eq!(
        send(&h.app, form("/login", None, &[("password", OLD)]))
            .await
            .status,
        401
    );
    assert_eq!(
        send(&h.app, form("/login", None, &[("password", NEW)]))
            .await
            .status,
        303
    );
    let login = send(&h.app, get("/login?password_changed=1", None)).await;
    assert!(login.body.contains("all devices were logged out"));
    assert!(
        !send(&h.app, get("/login?password_changed=%3Cscript%3E", None))
            .await
            .body
            .contains("<script>")
    );
}

#[tokio::test]
async fn password_change_is_available_with_a_broken_stalwart_connection_and_checks_csrf() {
    let h = harness(None).await;
    h.state
        .store
        .set_stalwart("http://127.0.0.1:1".into(), "key".into())
        .await
        .unwrap();
    h.state.reload_backend().await.unwrap();
    assert_eq!(
        send(&h.app, get("/settings/password", Some(&h.cookie())))
            .await
            .status,
        200
    );
    let mut req = form(
        "/settings/password",
        Some(&h.cookie()),
        &[
            ("current_password", OLD),
            ("password", NEW),
            ("confirm", NEW),
        ],
    );
    req.headers_mut()
        .insert("sec-fetch-site", "cross-site".parse().unwrap());
    assert_eq!(send(&h.app, req).await.status, 403);
    h.unchanged().await;
}

#[tokio::test]
async fn overloaded_routes_do_not_consume_authentication_budgets() {
    let h = harness(None).await;
    let slots = [
        h.state.passwords.reserve().unwrap(),
        h.state.passwords.reserve().unwrap(),
    ];
    for _ in 0..25 {
        let reply = send(&h.app, form("/login", None, &[("password", "wrong")])).await;
        assert_eq!(reply.status, 503);
        assert_eq!(reply.headers[header::RETRY_AFTER], "1");
        assert_eq!(h.change("wrong", NEW, NEW).await.status, 503);
    }
    drop(slots);
    // Both handlers share the ten-IP limit, across different kinds of checks.
    for _ in 0..5 {
        assert_eq!(
            send(&h.app, form("/login", None, &[("password", "wrong")]))
                .await
                .status,
            401
        );
        assert_eq!(h.change("wrong", NEW, NEW).await.status, 401);
    }
    let reply = h.change(OLD, NEW, NEW).await;
    assert_eq!(reply.status, 429);
    assert!(
        reply.headers[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            <= 60
    );
    h.unchanged().await;

    let state = AppState::new(Store::in_memory().unwrap(), Some("CODE".into()), true)
        .await
        .unwrap();
    let app = web::router(state.clone());
    let slots = [
        state.passwords.reserve().unwrap(),
        state.passwords.reserve().unwrap(),
    ];
    for _ in 0..25 {
        let reply = send(
            &app,
            form(
                "/setup",
                None,
                &[("code", "bad"), ("password", NEW), ("confirm", NEW)],
            ),
        )
        .await;
        assert_eq!(reply.status, 503);
        assert_eq!(reply.headers[header::RETRY_AFTER], "1");
    }
    drop(slots);
    for _ in 0..10 {
        assert_eq!(
            send(
                &app,
                form(
                    "/setup",
                    None,
                    &[("code", "bad"), ("password", NEW), ("confirm", NEW)]
                )
            )
            .await
            .status,
            401
        );
    }
    assert_eq!(
        send(
            &app,
            form(
                "/setup",
                None,
                &[("code", "CODE"), ("password", NEW), ("confirm", NEW)]
            )
        )
        .await
        .status,
        429
    );
    assert!(
        state
            .login_limiter
            .try_attempt(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .is_ok()
    );
}

#[tokio::test]
async fn rotating_ips_cannot_exceed_twenty_failures_and_success_clears_history() {
    let h = harness(None).await;
    for i in 1..=20 {
        let mut req = form("/login", None, &[("password", "wrong")]);
        req.headers_mut()
            .insert("x-forwarded-for", format!("192.0.2.{i}").parse().unwrap());
        assert_eq!(send(&h.app, req).await.status, 401);
    }
    let mut req = form("/login", None, &[("password", OLD)]);
    req.headers_mut()
        .insert("x-forwarded-for", "192.0.2.100".parse().unwrap());
    let reply = send(&h.app, req).await;
    assert_eq!(reply.status, 429);
    assert!(reply.headers.contains_key(header::RETRY_AFTER));
    // Exercise successful completion of an already admitted parallel attempt.
    let limiter = Arc::new(LoginLimiter::default());
    let good = limiter
        .try_attempt(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .unwrap();
    for i in 1..20 {
        drop(
            limiter
                .try_attempt(IpAddr::V4(Ipv4Addr::new(192, 0, 2, i)))
                .unwrap(),
        );
    }
    assert!(
        limiter
            .try_attempt(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .is_err()
    );
    good.success();
    assert!(limiter.try_attempt(IpAddr::V4(Ipv4Addr::LOCALHOST)).is_ok());
}

#[tokio::test]
async fn input_limits_are_local_to_authentication_and_cover_each_password_field() {
    let h = harness(None).await;
    let too_many_bytes = "\u{301}".repeat(2049);
    let too_many_chars = "a".repeat(129);
    for bad in [&too_many_bytes, &too_many_chars] {
        assert_eq!(
            send(&h.app, form("/login", None, &[("password", bad)]))
                .await
                .status,
            422
        );
        for fields in [
            (bad.as_str(), NEW, NEW),
            (OLD, bad.as_str(), NEW),
            (OLD, NEW, bad.as_str()),
        ] {
            assert_eq!(h.change(fields.0, fields.1, fields.2).await.status, 422);
        }
        let state = AppState::new(Store::in_memory().unwrap(), Some("CODE".into()), false)
            .await
            .unwrap();
        let app = web::router(state);
        for fields in [(bad.as_str(), NEW), (NEW, bad.as_str())] {
            assert_eq!(
                send(
                    &app,
                    form(
                        "/setup",
                        None,
                        &[
                            ("code", "CODE"),
                            ("password", fields.0),
                            ("confirm", fields.1)
                        ]
                    )
                )
                .await
                .status,
                422
            );
        }
    }
    for route in ["/setup", "/login", "/settings/password"] {
        let req = Request::builder()
            .method("POST")
            .uri(route)
            .header(header::COOKIE, h.cookie())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("password={}", "a".repeat(64 * 1024))))
            .unwrap();
        assert_eq!(
            send(&h.app, req).await.status,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
    let req = form(
        "/settings/alias-format",
        Some(&h.cookie()),
        &[("alias_format", &"a".repeat(70 * 1024))],
    );
    assert_eq!(send(&h.app, req).await.status, 303);
    // Login does not rerun creation rules (short/space-containing legacy ASCII passwords).
    let store = Store::in_memory().unwrap();
    store.create_admin(hash("a b").await).await.unwrap();
    let app = web::router(AppState::new(store, None, false).await.unwrap());
    assert_eq!(
        send(&app, form("/login", None, &[("password", "a b")]))
            .await
            .status,
        303
    );
    h.unchanged().await;
}

#[tokio::test]
async fn hibp_range_checks_prefix_padding_case_and_full_validation() {
    let server = MockServer::start().await;
    let digest = hex::encode_upper(Sha1::digest(NEW.as_bytes()));
    for (body, expected) in [
        (
            format!("{}:10\r\n", digest[5..].to_lowercase()),
            Ok(BreachStatus::Breached),
        ),
        (
            format!("{}:0\r\n", &digest[5..]),
            Ok(BreachStatus::NotBreached),
        ),
        (
            format!("{}:0\r\n", "A".repeat(35)),
            Ok(BreachStatus::NotBreached),
        ),
        (
            format!("{}:10\r\nINVALID", &digest[5..]),
            Err(BreachCheckError::InvalidResponse),
        ),
    ] {
        server.reset().await;
        Mock::given(method("GET"))
            .and(path(format!("/range/{}", &digest[..5])))
            .and(match_header("add-padding", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            BreachCheck::new(&server.uri()).unwrap().check(NEW).await,
            expected
        );
        let requests = server.received_requests().await.unwrap();
        assert!(requests[0].body.is_empty());
        assert!(requests[0].url.query().is_none());
    }
}

#[tokio::test]
async fn every_breach_error_preserves_password_and_sessions_in_setup_and_change() {
    let server = MockServer::start().await;
    let cases = vec![
        (ResponseTemplate::new(500), BreachCheckError::HttpStatus),
        (ResponseTemplate::new(204), BreachCheckError::HttpStatus),
        (
            ResponseTemplate::new(302).insert_header("location", "https://example.invalid/"),
            BreachCheckError::Redirect,
        ),
        (
            ResponseTemplate::new(200).set_body_string(""),
            BreachCheckError::InvalidResponse,
        ),
        (
            ResponseTemplate::new(200).set_body_string("<html>maintenance</html>"),
            BreachCheckError::InvalidResponse,
        ),
        (
            ResponseTemplate::new(200).set_body_bytes(vec![0xff]),
            BreachCheckError::InvalidResponse,
        ),
        (
            ResponseTemplate::new(200).set_body_string("x".repeat(1024 * 1024 + 1)),
            BreachCheckError::TooLarge,
        ),
        (
            ResponseTemplate::new(200).set_delay(Duration::from_secs(8)),
            BreachCheckError::Timeout,
        ),
    ];
    let h = harness(Some(&server.uri())).await;
    let setup_store = Store::in_memory().unwrap();
    let state = AppState::new(setup_store.clone(), Some("CODE".into()), false)
        .await
        .unwrap()
        .with_breach_check(BreachCheck::new(&server.uri()).unwrap());
    let app = web::router(state);
    for (response, error) in cases {
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(response)
            .mount(&server)
            .await;
        assert_eq!(
            BreachCheck::new(&server.uri()).unwrap().check(NEW).await,
            Err(error)
        );
        let (change, setup) = tokio::join!(
            h.change(OLD, NEW, NEW),
            send(
                &app,
                form(
                    "/setup",
                    None,
                    &[("code", "CODE"), ("password", NEW), ("confirm", NEW)]
                )
            ),
        );
        assert_eq!(change.status, 503);
        assert_eq!(setup.status, 503);
        assert!(change.body.contains("breach check is unavailable"));
        h.unchanged().await;
        assert!(setup_store.settings().await.unwrap().admin_hash.is_none());
    }
    // A successful response with a real hit refuses the change too.
    server.reset().await;
    let digest = hex::encode_upper(Sha1::digest(NEW.as_bytes()));
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("{}:42\r\n", &digest[5..])),
        )
        .mount(&server)
        .await;
    assert_eq!(h.change(OLD, NEW, NEW).await.status, 422);
    h.unchanged().await;
    // Neither normal login nor checking the current password needs HIBP.
    server.reset().await;
    assert_eq!(
        send(&h.app, form("/login", None, &[("password", OLD)]))
            .await
            .status,
        303
    );
    assert_eq!(h.change("wrong", NEW, NEW).await.status, 401);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn transport_failure_is_not_a_checked_non_hit() {
    let check = BreachCheck::new("http://127.0.0.1:1").unwrap();
    assert_eq!(check.check(NEW).await, Err(BreachCheckError::Connection));
    let h = harness(Some("http://127.0.0.1:1")).await;
    assert_eq!(h.change(OLD, NEW, NEW).await.status, 503);
    h.unchanged().await;
}

/// Hold HIBP replies until each credential check has definitely completed.
struct GatedBreach {
    uri: String,
    arrived: Arc<tokio::sync::Barrier>,
    release: Arc<tokio::sync::Semaphore>,
    task: tokio::task::JoinHandle<()>,
}

impl GatedBreach {
    async fn new(requests: usize) -> Self {
        let arrived = Arc::new(tokio::sync::Barrier::new(requests + 1));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let arrival = arrived.clone();
        let gate = release.clone();
        let app = Router::new().route(
            "/range/{prefix}",
            axum::routing::get(move || {
                let arrived = arrival.clone();
                let release = gate.clone();
                async move {
                    arrived.wait().await;
                    release.acquire().await.unwrap().forget();
                    format!("{}:0\r\n", "A".repeat(35))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            uri,
            arrived,
            release,
            task,
        }
    }

    async fn wait(&self) {
        tokio::time::timeout(Duration::from_secs(4), self.arrived.wait())
            .await
            .unwrap();
    }
}

impl Drop for GatedBreach {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn two_password_changes_have_exactly_one_winner() {
    let gate = GatedBreach::new(2).await;
    let h = harness(Some(&gate.uri)).await;
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let app = h.app.clone();
        let request = form(
            "/settings/password",
            Some(&h.cookie()),
            &[
                ("current_password", OLD),
                ("password", NEW),
                ("confirm", NEW),
            ],
        );
        tasks.push(tokio::spawn(async move { send(&app, request).await }));
    }
    gate.wait().await;
    gate.release.add_permits(2);
    let mut statuses = Vec::new();
    for task in tasks {
        statuses.push(task.await.unwrap().status.as_u16());
    }
    statuses.sort();
    assert_eq!(statuses, [303, 409]);
    assert!(h.state.store.find_token(&h.token).await.unwrap().is_some());
}

#[tokio::test]
async fn reset_or_logout_during_breach_check_prevents_the_web_change() {
    for reset in [false, true] {
        let gate = GatedBreach::new(1).await;
        let h = harness(Some(&gate.uri)).await;
        let app = h.app.clone();
        let req = form(
            "/settings/password",
            Some(&h.cookie()),
            &[
                ("current_password", OLD),
                ("password", NEW),
                ("confirm", NEW),
            ],
        );
        let change = tokio::spawn(async move { send(&app, req).await });
        gate.wait().await;
        let expected = if reset {
            let reset_hash = hash("Cobalt-Moon-53-cypress-Silver").await;
            assert!(
                h.state
                    .store
                    .change_admin_password(h.hash.clone(), reset_hash.clone(), None)
                    .await
                    .unwrap()
            );
            reset_hash
        } else {
            h.state.store.delete_session(&h.sessions[0]).await.unwrap();
            h.hash.clone()
        };
        gate.release.add_permits(1);
        assert_eq!(change.await.unwrap().status, 409);
        assert_eq!(
            h.state.store.settings().await.unwrap().admin_hash,
            Some(expected)
        );
        assert!(h.state.store.find_token(&h.token).await.unwrap().is_some());
    }
}

#[tokio::test]
async fn verified_old_login_cannot_survive_a_password_change_in_either_transaction_order() {
    for login_first in [false, true] {
        let h = harness(None).await;
        assert!(
            h.state
                .passwords
                .reserve()
                .unwrap()
                .verify(OLD.into(), h.hash.clone())
                .await
                .unwrap()
        );
        let session = if login_first {
            h.state.store.create_session(h.hash.clone()).await.unwrap()
        } else {
            None
        };
        assert!(
            h.state
                .store
                .change_admin_password(h.hash.clone(), hash(NEW).await, None)
                .await
                .unwrap()
        );
        assert!(
            h.state
                .store
                .create_session(h.hash.clone())
                .await
                .unwrap()
                .is_none()
        );
        if let Some(session) = session {
            assert!(!h.state.store.session_valid(&session).await.unwrap());
        }
    }
}

#[tokio::test]
async fn selected_policy_and_upgrade_break_are_explicit_regressions() {
    // Accepted policy: twelve characters and no whitespace, without a legacy NFC fallback.
    assert!(stalias::password::validate("K7!qR9@vN2#x").is_ok());
    assert!(stalias::password::validate("pflaume Nebel 7 Kupfer Zitrone").is_err());
    let legacy = "Pfla\u{308}ume-Nebel-7-Kupfer-Zitrone";
    let store = Store::in_memory().unwrap();
    store.create_admin(hash(legacy).await).await.unwrap();
    let app = web::router(AppState::new(store, None, false).await.unwrap());
    assert_eq!(
        send(&app, form("/login", None, &[("password", legacy)]))
            .await
            .status,
        401
    );
}

#[tokio::test]
async fn argon2id_keeps_its_parameters_and_fresh_salts() {
    let a = hash(OLD).await;
    let b = hash(OLD).await;
    assert!(a.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
    assert_ne!(a, b);
}

#[tokio::test]
async fn concurrent_setup_issues_only_one_session() {
    let store = Store::in_memory().unwrap();
    let app = web::router(
        AppState::new(store.clone(), Some("CODE".into()), false)
            .await
            .unwrap(),
    );
    let req = || {
        form(
            "/setup",
            None,
            &[("code", "CODE"), ("password", NEW), ("confirm", NEW)],
        )
    };
    let (a, b) = tokio::join!(send(&app, req()), send(&app, req()));
    assert_eq!([a.status.as_u16(), b.status.as_u16()], [303, 303]);
    assert_eq!(
        [a, b]
            .iter()
            .filter(|r| r.headers.contains_key(header::SET_COOKIE))
            .count(),
        1
    );
    assert!(store.settings().await.unwrap().admin_hash.is_some());
}

#[tokio::test]
async fn concurrent_database_connections_serialize_old_login_and_two_changes() {
    let directory = std::env::temp_dir().join(format!("stalias-cas-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("stalias.db");
    let server = Store::open(&path).unwrap();
    let reset = Store::open_existing(&path).unwrap();
    let login = Store::open_existing(&path).unwrap();
    server.create_admin("old".into()).await.unwrap();
    let session = server.create_session("old".into()).await.unwrap().unwrap();
    let (web_change, cli_reset, old_login) = tokio::join!(
        server.change_admin_password("old".into(), "web".into(), Some(session.clone())),
        reset.change_admin_password("old".into(), "cli".into(), None),
        login.create_session("old".into()),
    );
    assert_ne!(
        web_change.unwrap(),
        cli_reset.unwrap(),
        "exactly one password writer wins"
    );
    assert!(!server.session_valid(&session).await.unwrap());
    if let Some(session) = old_login.unwrap() {
        assert!(!server.session_valid(&session).await.unwrap());
    }
    assert!(login.create_session("old".into()).await.unwrap().is_none());
    drop((server, reset, login));
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn redirects_are_not_followed_even_when_the_destination_would_return_valid_data() {
    let destination = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("{}:0\r\n", "A".repeat(35))),
        )
        .expect(0)
        .mount(&destination)
        .await;
    let source = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", destination.uri()))
        .mount(&source)
        .await;
    assert_eq!(
        BreachCheck::new(&source.uri()).unwrap().check(NEW).await,
        Err(BreachCheckError::Redirect)
    );
    assert!(destination.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn chunked_response_without_content_length_is_still_size_limited() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            assert!(request.len() < 4096);
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        let chunk = vec![b'A'; 64 * 1024];
        for _ in 0..32 {
            if stream
                .write_all(b"10000\r\n")
                .and_then(|()| stream.write_all(&chunk))
                .and_then(|()| stream.write_all(b"\r\n"))
                .is_err()
            {
                break;
            }
        }
        let _ = stream.write_all(b"0\r\n\r\n");
    });
    assert_eq!(
        BreachCheck::new(&format!("http://{address}"))
            .unwrap()
            .check(NEW)
            .await,
        Err(BreachCheckError::TooLarge)
    );
    worker.join().unwrap();
}

#[tokio::test]
async fn current_password_verification_preserves_legacy_creation_rules_and_handles_hash_errors() {
    for current in ["a b", "malformed-hash-case"] {
        let store = Store::in_memory().unwrap();
        let expected_hash = if current == "a b" {
            hash(current).await
        } else {
            "invalid-hash".into()
        };
        store.create_admin(expected_hash.clone()).await.unwrap();
        let session = store
            .create_session(expected_hash.clone())
            .await
            .unwrap()
            .unwrap();
        let cookie = format!("stalias_session={session}");
        let app = web::router(AppState::new(store.clone(), None, false).await.unwrap());
        let reply = send(
            &app,
            form(
                "/settings/password",
                Some(&cookie),
                &[
                    ("current_password", current),
                    ("password", NEW),
                    ("confirm", NEW),
                ],
            ),
        )
        .await;
        if current == "a b" {
            assert_eq!(reply.status, 303);
            assert!(!store.session_valid(&session).await.unwrap());
        } else {
            assert_eq!(reply.status, 500);
            assert_eq!(
                send(&app, form("/login", None, &[("password", "anything")]))
                    .await
                    .status,
                500
            );
            assert_eq!(
                store.settings().await.unwrap().admin_hash,
                Some(expected_hash)
            );
            assert!(store.session_valid(&session).await.unwrap());
        }
    }
}

#[tokio::test]
async fn password_form_and_errors_stay_in_the_security_tab_after_setup() {
    let h = harness(None).await;
    h.state.store.finish_setup().await.unwrap();
    let page = send(&h.app, get("/settings?tab=security", Some(&h.cookie()))).await;
    assert_eq!(page.status, 200);
    assert!(page.body.contains("Alias configuration"));
    assert!(page.body.contains("settings-tabs-tab-security\" aria-controls=\"settings-tabs-panel-security\" aria-selected=\"true\""));
    assert!(page.body.contains("name=\"current_password\""));
    let reply = h.change("wrong", NEW, NEW).await;
    assert_eq!(reply.status, 401);
    assert!(reply.body.contains("Wrong current password."));
    assert!(reply.body.contains("settings-tabs-tab-security\" aria-controls=\"settings-tabs-panel-security\" aria-selected=\"true\""));
    assert!(!reply.body.contains(&format!("value=\"{NEW}\"")));
    h.unchanged().await;
}
