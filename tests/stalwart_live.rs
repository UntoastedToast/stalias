//! Runs against a real Stalwart 0.16 instance, e.g. the one from `scripts/setup-test-instance.sh`.
//!
//! STALIAS_TEST_URL      base URL, e.g. http://127.0.0.1:8080
//! STALIAS_TEST_KEY      API key with the permissions listed in docs/stalwart-0.16-notes.md
//! STALIAS_TEST_ACCOUNT  id of a mailbox that may be modified
//! STALIAS_TEST_DOMAIN   a domain name known to the server
//!
//! Run with `cargo test --test stalwart_live -- --ignored`.

use std::sync::Arc;

use stalias::aliases::{AliasService, Error, NewAlias};
use stalias::stalwart::Client;

struct Env {
    service: Arc<AliasService>,
    client: Arc<Client>,
    account: String,
    domain: String,
}

fn env() -> Env {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
    let client = Arc::new(Client::new(&var("STALIAS_TEST_URL"), var("STALIAS_TEST_KEY")).unwrap());
    Env {
        service: Arc::new(AliasService::new(client.clone())),
        client,
        account: var("STALIAS_TEST_ACCOUNT"),
        domain: var("STALIAS_TEST_DOMAIN"),
    }
}

fn custom(local: &str) -> NewAlias {
    NewAlias {
        local_part: Some(local.into()),
        description: Some("stalias live test".into()),
    }
}

#[tokio::test]
#[ignore]
async fn check_succeeds_with_valid_key() {
    env().client.check().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn bad_key_is_reported() {
    let url = std::env::var("STALIAS_TEST_URL").unwrap();
    let client = Client::new(&url, "API_invalid").unwrap();
    assert!(matches!(
        client.check().await,
        Err(stalias::stalwart::Error::Unauthorized)
    ));
}

#[tokio::test]
#[ignore]
async fn lifecycle_survives_index_shifts() {
    let e = env();
    let (a, b, c) = ("lt-first", "lt-second", "lt-third");
    for name in [a, b, c] {
        e.service
            .create(&e.account, &e.domain, custom(name))
            .await
            .unwrap();
    }

    let dup = e.service.create(&e.account, &e.domain, custom(a)).await;
    assert!(matches!(dup, Err(Error::Conflict)), "{dup:?}");

    // Removing the first entry shifts the others; the remaining ones must stay addressable.
    e.service
        .delete(&e.account, &format!("{a}@{}", e.domain))
        .await
        .unwrap();
    e.service
        .set_enabled(&e.account, &format!("{c}@{}", e.domain), false)
        .await
        .unwrap();

    let list = e.service.list(&e.account).await.unwrap();
    let find = |n: &str| list.iter().find(|x| x.local_part == n);
    assert!(find(a).is_none());
    assert!(find(b).unwrap().enabled);
    assert!(!find(c).unwrap().enabled);

    for name in [b, c] {
        e.service
            .delete(&e.account, &format!("{name}@{}", e.domain))
            .await
            .unwrap();
    }
    let missing = e
        .service
        .delete(&e.account, &format!("{b}@{}", e.domain))
        .await;
    assert!(matches!(missing, Err(Error::AliasNotFound)));
}

#[tokio::test]
#[ignore]
async fn parallel_creates_all_survive() {
    let e = env();
    let tasks: Vec<_> = (0..10)
        .map(|_| {
            let (service, account, domain) =
                (e.service.clone(), e.account.clone(), e.domain.clone());
            tokio::spawn(
                async move { service.create(&account, &domain, NewAlias::default()).await },
            )
        })
        .collect();

    let mut created = Vec::new();
    for t in tasks {
        created.push(t.await.unwrap().unwrap());
    }

    let list = e.service.list(&e.account).await.unwrap();
    for alias in &created {
        assert!(
            list.iter().any(|x| x.local_part == alias.local_part),
            "lost {}",
            alias.address()
        );
    }
    for alias in &created {
        e.service
            .delete(&e.account, &alias.address())
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn unknown_domain_and_account() {
    let e = env();
    let r = e
        .service
        .create(&e.account, "nope.invalid", NewAlias::default())
        .await;
    assert!(matches!(r, Err(Error::UnknownDomain(_))));
    let r = e
        .service
        .create("zzzz", &e.domain, NewAlias::default())
        .await;
    assert!(matches!(r, Err(Error::AccountNotFound)), "{r:?}");
}
