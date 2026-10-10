//! HTTP-level tests against a small fake of Stalwart's JMAP API, so they run everywhere.
//! `tests/web_live.rs` covers the same ground against the real server.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Map, Value, json};
use stalias::state::AppState;
use stalias::store::Store;
use stalias::web;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

mod common;
use common::{api, form, get, send};

const ACCOUNT: &str = "acc1";
const DOMAIN: &str = "example.org";

/// One mailbox with a numbered alias map, like Stalwart keeps it.
#[derive(Default)]
struct Fake {
    aliases: Mutex<BTreeMap<String, Value>>,
    domain_lookups: AtomicUsize,
}

impl Fake {
    fn apply(&self, patch: &Map<String, Value>) -> Result<(), Value> {
        let mut aliases = self.aliases.lock().unwrap();
        for (key, value) in patch {
            let parts: Vec<&str> = key.split('/').collect();
            match (parts.as_slice(), value) {
                (["aliases", index], Value::Null) => {
                    aliases.remove(*index);
                }
                (["aliases", index], alias) => {
                    let taken = aliases.values().any(|a| a["name"] == alias["name"]);
                    if taken {
                        return Err(json!({ "type": "primaryKeyViolation" }));
                    }
                    aliases.insert((*index).to_owned(), alias.clone());
                }
                (["aliases", index, field], value) => {
                    aliases.get_mut(*index).unwrap()[*field] = value.clone();
                }
                _ => return Err(json!({ "type": "invalidPatch" })),
            }
        }
        Ok(())
    }
}

struct Responder(Arc<Fake>);

impl Respond for Responder {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let fake = &self.0;
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let [name, args, _] = body["methodCalls"][0].as_array().unwrap().as_slice() else {
            panic!("unexpected call");
        };
        let payload = match name.as_str().unwrap() {
            "x:Domain/get" => {
                fake.domain_lookups.fetch_add(1, Ordering::SeqCst);
                json!({ "list": [{ "id": "d1", "name": DOMAIN }] })
            }
            "x:Account/get" => json!({ "list": [{
                "id": ACCOUNT,
                "name": "alice",
                "@type": "User",
                "emailAddress": format!("alice@{DOMAIN}"),
                "aliases": *fake.aliases.lock().unwrap(),
            }] }),
            // An update without a target (the connection check) changes nothing.
            "x:Account/set" => match args["update"][ACCOUNT]
                .as_object()
                .map_or(Ok(()), |patch| fake.apply(patch))
            {
                Ok(()) => json!({ "notUpdated": {} }),
                Err(err) => json!({ "notUpdated": { ACCOUNT: err } }),
            },
            other => panic!("unexpected method {other}"),
        };
        ResponseTemplate::new(200)
            .set_body_json(json!({ "methodResponses": [[name, payload, "0"]] }))
    }
}

struct Harness {
    app: Router,
    store: Store,
    fake: Arc<Fake>,
    token: String,
    _server: MockServer,
}

async fn harness(trust_proxy: bool) -> Harness {
    let fake = Arc::new(Fake::default());
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jmap/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "apiUrl": "https://mail.invalid/jmap/",
            "accounts": { ACCOUNT: {} },
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/jmap/"))
        .respond_with(Responder(fake.clone()))
        .mount(&server)
        .await;

    let store = Store::in_memory().unwrap();
    store
        .set_stalwart(server.uri(), "key".into())
        .await
        .unwrap();
    store.set_domains(&[DOMAIN.into()]).await.unwrap();
    let token = store
        .create_token("test".into(), ACCOUNT.into(), "alice".into())
        .await
        .unwrap();
    let state = AppState::new(store.clone(), None, trust_proxy)
        .await
        .unwrap();
    Harness {
        app: web::router(state),
        store,
        fake,
        token,
        _server: server,
    }
}

#[tokio::test]
async fn addy_api_lifecycle() {
    let h = harness(false).await;
    let t = Some(h.token.as_str());

    let created = send(
        &h.app,
        api(
            "POST",
            "/api/v1/aliases",
            t,
            Some(json!({ "format": "custom", "local_part": "shop", "description": "Shop" })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let alias = &created.json()["data"];
    assert_eq!(alias["email"], format!("shop@{DOMAIN}"));
    assert_eq!(alias["active"], true);

    let random = send(&h.app, api("POST", "/api/v1/aliases", t, Some(json!({})))).await;
    assert_eq!(random.status, StatusCode::CREATED);
    let random_local = random.json()["data"]["local_part"].as_str().unwrap().len();
    assert_eq!(random_local, 10);

    let path = format!("/api/v1/aliases/shop@{DOMAIN}");
    let patched = send(
        &h.app,
        api("PATCH", &path, t, Some(json!({ "description": "Renamed" }))),
    )
    .await;
    assert_eq!(patched.json()["data"]["description"], "Renamed");

    let off = send(
        &h.app,
        api(
            "DELETE",
            &format!("/api/v1/active-aliases/shop@{DOMAIN}"),
            t,
            None,
        ),
    )
    .await;
    assert_eq!(off.status, StatusCode::NO_CONTENT);
    assert_eq!(
        send(&h.app, api("GET", &path, t, None)).await.json()["data"]["active"],
        false
    );
    let on = send(
        &h.app,
        api(
            "POST",
            "/api/v1/active-aliases",
            t,
            Some(json!({ "id": format!("shop@{DOMAIN}") })),
        ),
    )
    .await;
    assert_eq!(on.json()["data"]["active"], true);

    let listed = send(&h.app, api("GET", "/api/v1/aliases", t, None)).await;
    assert_eq!(listed.json()["data"].as_array().unwrap().len(), 2);

    assert_eq!(
        send(&h.app, api("DELETE", &path, t, None)).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(&h.app, api("GET", &path, t, None)).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn addy_api_rejects_bad_requests() {
    let h = harness(false).await;
    let t = Some(h.token.as_str());
    let create = |body: Value| api("POST", "/api/v1/aliases", t, Some(body));

    let no_auth = send(
        &h.app,
        api("POST", "/api/v1/aliases", None, Some(json!({}))),
    )
    .await;
    assert_eq!(no_auth.status, StatusCode::UNAUTHORIZED);
    let bad_token = send(
        &h.app,
        api("GET", "/api/v1/aliases", Some("stalias_nope"), None),
    )
    .await;
    assert_eq!(bad_token.status, StatusCode::UNAUTHORIZED);

    let cases = [
        json!({ "domain": "other.org" }),
        json!({ "format": "custom" }),
        json!({ "format": "custom", "local_part": "Not Valid" }),
        json!({ "description": "x".repeat(256) }),
    ];
    for body in cases {
        let res = send(&h.app, create(body.clone())).await;
        assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }

    let first = json!({ "format": "custom", "local_part": "taken" });
    assert_eq!(
        send(&h.app, create(first.clone())).await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        send(&h.app, create(first)).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    let missing = send(
        &h.app,
        api("GET", "/api/v1/aliases/nobody@example.org", t, None),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn domain_list_is_not_fetched_for_every_request() {
    let h = harness(false).await;
    let t = Some(h.token.as_str());
    send(&h.app, api("POST", "/api/v1/aliases", t, Some(json!({})))).await;
    let after_create = h.fake.domain_lookups.load(Ordering::SeqCst);
    for _ in 0..5 {
        send(&h.app, api("GET", "/api/v1/aliases", t, None)).await;
    }
    assert_eq!(h.fake.domain_lookups.load(Ordering::SeqCst), after_create);
}

/// A logged-in admin cookie for a fully set-up instance.
async fn admin_cookie(h: &Harness) -> String {
    h.store
        .create_admin("not-a-real-hash".into())
        .await
        .unwrap();
    format!(
        "stalias_session={}",
        h.store.create_session().await.unwrap()
    )
}

#[tokio::test]
async fn dashboard_never_shows_text_from_the_url() {
    let h = harness(false).await;
    let cookie = admin_cookie(&h).await;
    let page = send(
        &h.app,
        get("/?error=Call+support+on+555-0100", Some(&cookie)),
    )
    .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.body.contains("555-0100"));
}

#[tokio::test]
async fn failed_form_actions_render_the_error_instead_of_redirecting() {
    let h = harness(false).await;
    let cookie = admin_cookie(&h).await;
    let res = send(
        &h.app,
        form(
            "/aliases/delete",
            Some(&cookie),
            &[("account", ACCOUNT), ("address", "ghost@example.org")],
        ),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("alias not found"), "{}", res.body);
}

#[tokio::test]
async fn cross_site_form_posts_are_refused() {
    let h = harness(false).await;
    let cookie = admin_cookie(&h).await;
    let post = |extra: (&str, &str)| {
        Request::builder()
            .method("POST")
            .uri("/aliases/delete")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &cookie)
            .header(header::HOST, "stalias.example")
            .header(extra.0, extra.1)
            .body(Body::from(format!(
                "account={ACCOUNT}&address=x%40{DOMAIN}"
            )))
            .unwrap()
    };
    let forbidden = [
        ("sec-fetch-site", "cross-site"),
        ("origin", "https://evil.example"),
    ];
    for header in forbidden {
        assert_eq!(
            send(&h.app, post(header)).await.status,
            StatusCode::FORBIDDEN,
            "{header:?}"
        );
    }
    let allowed = [
        ("sec-fetch-site", "same-origin"),
        ("origin", "https://stalias.example"),
    ];
    for header in allowed {
        assert_ne!(
            send(&h.app, post(header)).await.status,
            StatusCode::FORBIDDEN,
            "{header:?}"
        );
    }
}

#[tokio::test]
async fn pages_are_not_cacheable_but_assets_are() {
    let h = harness(false).await;
    let login = send(&h.app, get("/login", None)).await;
    assert_eq!(login.headers[header::CACHE_CONTROL], "no-store");
    let asset = send(&h.app, get("/static/app.css", None)).await;
    assert!(
        asset.headers[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    // The UI font and the logo come from the binary too, so nothing is loaded from elsewhere.
    for (file, kind) in [
        ("geist-1.7.2.woff2", "font/woff2"),
        ("logo.svg", "image/svg+xml"),
    ] {
        let res = send(&h.app, get(&format!("/static/{file}"), None)).await;
        assert_eq!(res.status, StatusCode::OK, "{file}");
        assert_eq!(res.headers[header::CONTENT_TYPE], kind, "{file}");
    }
}

#[tokio::test]
async fn login_limit_is_per_client_behind_a_trusted_proxy() {
    let h = harness(true).await;
    h.store
        .create_admin("not-a-real-hash".into())
        .await
        .unwrap();
    let attempt = |ip: &str| {
        Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("x-forwarded-for", format!("198.51.100.99, {ip}"))
            .body(Body::from("password=wrong"))
            .unwrap()
    };
    for _ in 0..10 {
        assert_eq!(
            send(&h.app, attempt("203.0.113.1")).await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        send(&h.app, attempt("203.0.113.1")).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        send(&h.app, attempt("203.0.113.2")).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn login_limit_ignores_forwarded_for_without_a_trusted_proxy() {
    let h = harness(false).await;
    h.store
        .create_admin("not-a-real-hash".into())
        .await
        .unwrap();
    for i in 0..11 {
        let req = Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("x-forwarded-for", format!("203.0.113.{i}"))
            .body(Body::from("password=wrong"))
            .unwrap();
        let status = send(&h.app, req).await.status;
        let expected = if i < 10 {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::TOO_MANY_REQUESTS
        };
        assert_eq!(status, expected, "attempt {i}");
    }
}

#[tokio::test]
async fn first_run_wizard_walks_through_all_steps() {
    let h = harness(false).await;
    let store = Store::in_memory().unwrap();
    let state = AppState::new(store, Some("CODE".into()), false)
        .await
        .unwrap();
    let app = web::router(state);

    assert_eq!(send(&app, get("/", None)).await.location(), "/setup");
    let setup = |code: &str, pw: &str| {
        form(
            "/setup",
            None,
            &[("code", code), ("password", pw), ("confirm", pw)],
        )
    };
    assert_eq!(
        send(&app, setup("nope", "longpassword")).await.status,
        StatusCode::UNAUTHORIZED
    );
    // Eight bytes, but only four characters.
    assert_eq!(
        send(&app, setup("CODE", "üüüü")).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let done = send(&app, setup("CODE", "longpassword")).await;
    assert_eq!(done.status, StatusCode::SEE_OTHER);
    let cookie = done.headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let c = Some(cookie.as_str());

    assert_eq!(send(&app, get("/", c)).await.location(), "/settings");
    let guide = send(&app, get("/settings", c)).await;
    assert!(
        !guide.body.contains("id=\"sidebar\""),
        "no sidebar during setup"
    );
    for hint in ["API Keys", "Replace all permissions", "sysDomainQuery"] {
        assert!(guide.body.contains(hint), "settings guide lacks {hint}");
    }
    // A URL without a scheme must be reported, not crash the handler.
    for url in ["localhost:8080", "stalwart:8080"] {
        let bad = send(&app, form("/settings", c, &[("url", url), ("key", "k")])).await;
        assert_eq!(bad.status, StatusCode::OK);
        assert!(bad.body.contains("must start with http"), "{}", bad.body);
    }
    let uri = h._server.uri();
    let saved = send(&app, form("/settings", c, &[("url", &uri), ("key", "k")])).await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);

    assert_eq!(
        send(&app, get("/", c)).await.location(),
        "/settings/domains"
    );
    let picker = send(&app, get("/settings/domains", c)).await;
    assert!(picker.body.contains("checked"), "all domains start ticked");
    let empty = send(&app, form("/settings/domains", c, &[])).await;
    assert!(empty.body.contains("Select at least one"));
    let domains = send(&app, form("/settings/domains", c, &[("domain", DOMAIN)])).await;
    assert_eq!(domains.status, StatusCode::SEE_OTHER);

    assert_eq!(send(&app, get("/", c)).await.location(), "/tokens");
    let page = send(&app, get("/tokens", c)).await;
    assert!(page.body.contains("Setup progress"));
    assert!(page.body.contains("Choose a mailbox"));
    let created = send(
        &app,
        form("/tokens", c, &[("account_id", ACCOUNT), ("label", "")]),
    )
    .await;
    assert!(created.body.contains("Token created"));
    assert!(created.body.contains("hx-history=\"false\""));
    assert!(created.body.contains("\"historyCacheSize\": 0"));
    assert!(created.body.contains("Finish setup"));
    assert!(
        created.body.contains("Setup progress"),
        "badge stays on the result page"
    );
    // Going back through the steps must not end the wizard before "Finish setup".
    for back in ["/settings", "/settings/domains", "/tokens"] {
        let page = send(&app, get(back, c)).await;
        assert!(
            page.body.contains("Setup progress"),
            "{back} left the wizard"
        );
        assert!(
            !page.body.contains("id=\"sidebar\""),
            "{back} shows the sidebar"
        );
    }
    let dashboard = send(&app, get("/", c)).await;
    assert_eq!(dashboard.status, StatusCode::OK);
    assert!(
        dashboard.body.contains("id=\"sidebar\""),
        "sidebar once set up"
    );
    assert!(!dashboard.body.contains("Setup progress"));
    let settings = send(&app, get("/settings", c)).await;
    assert!(settings.body.contains("id=\"sidebar\""));
    assert!(!settings.body.contains("Setup progress"));
}

#[tokio::test]
async fn revoking_the_last_token_keeps_the_dashboard_reachable() {
    let h = harness(false).await;
    let cookie = admin_cookie(&h).await;
    let c = Some(cookie.as_str());
    assert_eq!(send(&h.app, get("/", c)).await.status, StatusCode::OK);

    for token in h.store.tokens().await.unwrap() {
        h.store.delete_token(token.id).await.unwrap();
    }
    assert_eq!(send(&h.app, get("/", c)).await.status, StatusCode::OK);
}

#[tokio::test]
async fn ui_only_creates_aliases_on_the_chosen_domains() {
    let h = harness(false).await;
    let cookie = admin_cookie(&h).await;
    let create = |domain: &str, local: &str| {
        form(
            "/aliases",
            Some(&cookie),
            &[
                ("account", ACCOUNT),
                ("domain", domain),
                ("local_part", local),
                ("description", ""),
            ],
        )
    };
    let res = send(&h.app, create("other.org", "shop")).await;
    assert!(res.body.contains("unknown domain"), "{}", res.body);
    assert!(h.fake.aliases.lock().unwrap().is_empty());

    let ok = send(&h.app, create(DOMAIN, "shop")).await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
    assert_eq!(h.fake.aliases.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn custom_local_parts_are_lower_cased() {
    let h = harness(false).await;
    let created = send(
        &h.app,
        api(
            "POST",
            "/api/v1/aliases",
            Some(&h.token),
            Some(json!({ "format": "custom", "local_part": "Shop" })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(created.json()["data"]["email"], format!("shop@{DOMAIN}"));
}

#[tokio::test]
async fn patch_only_changes_the_description_when_it_is_sent() {
    let h = harness(false).await;
    let t = Some(h.token.as_str());
    let path = format!("/api/v1/aliases/shop@{DOMAIN}");
    let body = json!({ "format": "custom", "local_part": "shop", "description": "keep me" });
    send(&h.app, api("POST", "/api/v1/aliases", t, Some(body))).await;

    for untouched in [json!({}), json!({ "from_name": "Shop" })] {
        let res = send(&h.app, api("PATCH", &path, t, Some(untouched))).await;
        assert_eq!(res.json()["data"]["description"], "keep me");
    }
    let cleared = send(
        &h.app,
        api("PATCH", &path, t, Some(json!({ "description": null }))),
    )
    .await;
    assert_eq!(cleared.json()["data"]["description"], Value::Null);
}

#[tokio::test]
async fn bearer_scheme_is_case_insensitive() {
    let h = harness(false).await;
    for scheme in ["Bearer", "bearer", "BEARER"] {
        let req = Request::builder()
            .uri("/api/v1/aliases")
            .header(header::AUTHORIZATION, format!("{scheme} {}", h.token))
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&h.app, req).await.status, StatusCode::OK, "{scheme}");
    }
    let token_only = Request::builder()
        .uri("/api/v1/aliases")
        .header(header::AUTHORIZATION, h.token.as_str())
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&h.app, token_only).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn login_limit_uses_the_last_forwarded_for_line() {
    let h = harness(true).await;
    h.store
        .create_admin("not-a-real-hash".into())
        .await
        .unwrap();
    // The proxy adds its own header line, so the first line is whatever the client claims.
    for i in 0..11 {
        let req = Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("x-forwarded-for", format!("198.51.100.{i}"))
            .header("x-forwarded-for", "203.0.113.7")
            .body(Body::from("password=wrong"))
            .unwrap();
        let expected = if i < 10 {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::TOO_MANY_REQUESTS
        };
        assert_eq!(send(&h.app, req).await.status, expected, "attempt {i}");
    }
}

#[tokio::test]
async fn the_mailboxs_own_address_cannot_become_an_alias() {
    let h = harness(false).await;
    let create = |local: &str| {
        api(
            "POST",
            "/api/v1/aliases",
            Some(&h.token),
            Some(json!({ "format": "custom", "local_part": local })),
        )
    };
    // The fake mailbox is alice@example.org.
    let own = send(&h.app, create("Alice")).await;
    assert_eq!(own.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", own.body);
    assert!(h.fake.aliases.lock().unwrap().is_empty());
    assert_eq!(
        send(&h.app, create("bob")).await.status,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn settings_form_keeps_the_typed_url_after_an_error() {
    let h = harness(false).await;
    let cookie = admin_cookie(&h).await;
    let res = send(
        &h.app,
        form(
            "/settings",
            Some(&cookie),
            &[("url", "http://127.0.0.1:1"), ("key", "k")],
        ),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(
        res.body.contains("could not reach Stalwart"),
        "{}",
        res.body
    );
    assert!(
        res.body.contains("value=\"http://127.0.0.1:1\""),
        "{}",
        res.body
    );
}

#[tokio::test]
async fn saving_a_key_without_update_permission_is_refused() {
    struct ReadOnly;
    impl Respond for ReadOnly {
        fn respond(&self, request: &MockRequest) -> ResponseTemplate {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let name = body["methodCalls"][0][0].as_str().unwrap().to_owned();
            let payload = match name.as_str() {
                "x:Domain/get" => json!({ "list": [{ "id": "d1", "name": DOMAIN }] }),
                "x:Account/get" => json!({ "list": [] }),
                _ => {
                    return ResponseTemplate::new(200).set_body_json(json!({
                        "methodResponses": [["error", {
                            "type": "forbidden",
                            "description": "You are not authorized to create, update or destroy objects of this type"
                        }, "0"]]
                    }));
                }
            };
            ResponseTemplate::new(200)
                .set_body_json(json!({ "methodResponses": [[name, payload, "0"]] }))
        }
    }
    let h = harness(false).await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jmap/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "apiUrl": "/jmap/", "accounts": { ACCOUNT: {} },
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/jmap/"))
        .respond_with(ReadOnly)
        .mount(&server)
        .await;
    let cookie = admin_cookie(&h).await;
    let res = send(
        &h.app,
        form(
            "/settings",
            Some(&cookie),
            &[("url", &server.uri()), ("key", "read-only")],
        ),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "must not redirect: {}",
        res.body
    );
    assert!(res.body.contains("lacks a permission"), "{}", res.body);
    let saved = h.store.settings().await.unwrap().stalwart_url.unwrap();
    assert_ne!(saved, server.uri());
}
