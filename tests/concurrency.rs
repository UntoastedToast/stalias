//! Regression tests for backend transitions, alias writes and login concurrency.
use serde_json::{Value, json};
use stalias::{aliases::NewAlias, auth::hash_password, state::AppState, store::Store, web};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};
mod common;
use common::{api, form, send};

#[derive(Default)]
struct Fake {
    aliases: Mutex<BTreeMap<String, Value>>,
    reads: AtomicUsize,
    delay_first_read: bool,
    read_started: tokio::sync::Notify,
}
struct Handler(Arc<Fake>);
impl Respond for Handler {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let call = &body["methodCalls"][0];
        let name = call[0].as_str().unwrap();
        let mut delay = Duration::ZERO;
        let payload = match name {
            "x:Domain/get" => json!({"list":[{"id":"d1","name":"example.org"}]}),
            "x:Account/get" => {
                let snapshot = self.0.aliases.lock().unwrap().clone();
                let count = self.0.reads.fetch_add(1, Ordering::SeqCst);
                if count == 0 {
                    self.0.read_started.notify_one();
                }
                if count == 0 && self.0.delay_first_read {
                    delay = Duration::from_millis(500);
                }
                json!({"list":[{"id":"acc1","name":"alice","@type":"User","aliases":snapshot}]})
            }
            "x:Account/set" => {
                let empty = serde_json::Map::new();
                let patch = call[1]["update"]["acc1"].as_object().unwrap_or(&empty);
                for (key, value) in patch {
                    let index = key.strip_prefix("aliases/").unwrap();
                    self.0
                        .aliases
                        .lock()
                        .unwrap()
                        .insert(index.into(), value.clone());
                }
                json!({"updated":{"acc1":null}})
            }
            _ => unreachable!(),
        };
        ResponseTemplate::new(200)
            .set_body_json(json!({"methodResponses":[[name,payload,"0"]]}))
            .set_delay(delay)
    }
}
async fn server(fake: Arc<Fake>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jmap/session"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"apiUrl":"/jmap/","accounts":{"acc1":{}}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/jmap/"))
        .respond_with(Handler(fake))
        .mount(&server)
        .await;
    server
}
async fn state(server: &MockServer) -> AppState {
    let store = Store::in_memory().unwrap();
    store
        .set_stalwart(server.uri(), "key".into())
        .await
        .unwrap();
    store.set_domains(&["example.org".into()]).await.unwrap();
    AppState::new(store, None, false).await.unwrap()
}
fn alias(name: &str) -> NewAlias {
    NewAlias {
        local_part: Some(name.into()),
        description: None,
    }
}

async fn first_read(fake: &Fake) {
    tokio::time::timeout(Duration::from_secs(5), fake.read_started.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn backend_reload_keeps_both_concurrent_alias_writes() {
    let fake = Arc::new(Fake {
        delay_first_read: true,
        ..Default::default()
    });
    let server = server(fake.clone()).await;
    let state = state(&server).await;
    let old = state.backend().unwrap();
    let first = tokio::spawn(async move {
        old.aliases
            .create("acc1", "example.org", alias("first"))
            .await
            .unwrap()
    });
    first_read(&fake).await;
    state.reload_backend().await.unwrap();
    state
        .backend()
        .unwrap()
        .aliases
        .create("acc1", "example.org", alias("second"))
        .await
        .unwrap();
    first.await.unwrap();
    let aliases = fake.aliases.lock().unwrap();
    assert_eq!(aliases.len(), 2);
    assert_eq!(aliases["0"]["name"], "first");
    assert_eq!(aliases["1"]["name"], "second");
}

#[tokio::test]
async fn old_token_is_rejected_on_replacement_mail_server() {
    let a = Arc::new(Fake::default());
    let b = Arc::new(Fake::default());
    let server_a = server(a.clone()).await;
    let server_b = server(b.clone()).await;
    let state = state(&server_a).await;
    let token = state
        .store
        .create_token(
            "old-client".into(),
            "acc1".into(),
            "original-mailbox".into(),
        )
        .await
        .unwrap();
    state
        .store
        .create_admin("unused-hash".into())
        .await
        .unwrap();
    let cookie = format!(
        "stalias_session={}",
        state.store.create_session().await.unwrap()
    );
    let app = web::router(state.clone());
    let saved = send(
        &app,
        form(
            "/settings",
            Some(&cookie),
            &[("url", &server_b.uri()), ("key", "different-key")],
        ),
    )
    .await;
    assert_eq!(saved.status, 303);
    assert!(state.store.settings().await.unwrap().domains.is_empty());
    state
        .store
        .set_domains(&["example.org".into()])
        .await
        .unwrap();
    let reply = send(
        &app,
        api(
            "POST",
            "/api/v1/aliases",
            Some(&token),
            Some(json!({"format":"custom","local_part":"old-token"})),
        ),
    )
    .await;
    assert_eq!(reply.status, 401);
    assert!(a.aliases.lock().unwrap().is_empty());
    assert!(b.aliases.lock().unwrap().is_empty());
    let new_token = state
        .store
        .create_token("new-client".into(), "acc1".into(), "new-mailbox".into())
        .await
        .unwrap();
    let reply = send(
        &app,
        api(
            "POST",
            "/api/v1/aliases",
            Some(&new_token),
            Some(json!({"format":"custom","local_part":"new-token"})),
        ),
    )
    .await;
    assert_eq!(reply.status, 201);
    assert_eq!(b.aliases.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn alias_form_emits_reset_event_only_when_creation_succeeds() {
    let server = server(Arc::new(Fake::default())).await;
    let state = state(&server).await;
    state
        .store
        .create_admin("unused-hash".into())
        .await
        .unwrap();
    let cookie = format!(
        "stalias_session={}",
        state.store.create_session().await.unwrap()
    );
    let mut req = form(
        "/aliases",
        Some(&cookie),
        &[
            ("account", "acc1"),
            ("domain", "example.org"),
            ("local_part", "invalid..name"),
            ("description", "keep this text"),
        ],
    );
    req.headers_mut()
        .insert("hx-request", "true".parse().unwrap());
    let app = web::router(state);
    let reply = send(&app, req).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.headers["hx-reswap"], "none");
    assert!(reply.body.contains("invalid local part"));
    assert!(!reply.headers.contains_key("hx-trigger-after-swap"));
    let mut req = form(
        "/aliases",
        Some(&cookie),
        &[
            ("account", "acc1"),
            ("domain", "example.org"),
            ("local_part", "valid"),
            ("description", "description"),
        ],
    );
    req.headers_mut()
        .insert("hx-request", "true".parse().unwrap());
    let reply = send(&app, req).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.headers["hx-trigger-after-swap"], "aliasCreated");
    assert!(reply.body.contains("valid@example.org"));
}

#[tokio::test]
async fn parallel_login_batch_respects_ten_failure_limit() {
    let store = Store::in_memory().unwrap();
    store
        .create_admin(hash_password("correct-password".into()).await)
        .await
        .unwrap();
    let app = web::router(AppState::new(store, None, false).await.unwrap());
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let app = app.clone();
        tasks.spawn(async move {
            send(&app, form("/login", None, &[("password", "wrong")]))
                .await
                .status
        });
    }
    let mut unauthorized = 0;
    while let Some(reply) = tasks.join_next().await {
        match reply.unwrap().as_u16() {
            401 => unauthorized += 1,
            429 => {}
            status => panic!("unexpected login status: {status}"),
        }
    }
    assert_eq!(
        unauthorized, 10,
        "only ten attempts may perform verification"
    );
    assert_eq!(
        send(&app, form("/login", None, &[("password", "wrong")]))
            .await
            .status,
        429
    );
}

#[tokio::test]
async fn server_switch_finishes_in_flight_api_requests_on_the_original_server() {
    let a = Arc::new(Fake {
        delay_first_read: true,
        ..Default::default()
    });
    let b = Arc::new(Fake::default());
    let server_a = server(a.clone()).await;
    let server_b = server(b.clone()).await;
    let state = state(&server_a).await;
    let token = state
        .store
        .create_token("test".into(), "acc1".into(), "alice".into())
        .await
        .unwrap();
    let app = web::router(state.clone());
    let request_app = app.clone();
    let request_token = token.clone();
    let pending = tokio::spawn(async move {
        send(
            &request_app,
            api(
                "POST",
                "/api/v1/aliases",
                Some(&request_token),
                Some(json!({"format":"custom","local_part":"pending"})),
            ),
        )
        .await
    });
    first_read(&a).await;
    state
        .set_connection(server_b.uri(), "new-key".into())
        .await
        .unwrap();
    let reply = pending.await.unwrap();
    assert_eq!(reply.status, 201);
    assert_eq!(a.aliases.lock().unwrap().len(), 1);
    assert!(b.aliases.lock().unwrap().is_empty());
    assert_eq!(
        send(&app, api("GET", "/api/v1/aliases", Some(&token), None))
            .await
            .status,
        401
    );
}

#[tokio::test]
async fn server_switch_revokes_tokens_created_by_in_flight_ui_requests() {
    let a = Arc::new(Fake {
        delay_first_read: true,
        ..Default::default()
    });
    let b = Arc::new(Fake::default());
    let server_a = server(a.clone()).await;
    let server_b = server(b.clone()).await;
    let state = state(&server_a).await;
    state
        .store
        .create_admin("unused-hash".into())
        .await
        .unwrap();
    let cookie = format!(
        "stalias_session={}",
        state.store.create_session().await.unwrap()
    );
    let app = web::router(state.clone());
    let pending = tokio::spawn(async move {
        send(
            &app,
            form(
                "/tokens",
                Some(&cookie),
                &[("account_id", "acc1"), ("label", "pending")],
            ),
        )
        .await
    });
    first_read(&a).await;
    state
        .set_connection(server_b.uri(), "new-key".into())
        .await
        .unwrap();
    let reply = pending.await.unwrap();
    assert!(reply.body.contains("Token created"));
    assert!(state.store.tokens().await.unwrap().is_empty());
    assert_eq!(b.reads.load(Ordering::SeqCst), 0);
}
