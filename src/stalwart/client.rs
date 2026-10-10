use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::OnceCell;
use url::Url;

use super::error::Error;
use super::model::{Account, Domain, RawAccount};

const USING: [&str; 2] = ["urn:ietf:params:jmap:core", "urn:stalwart:jmap"];

/// JMAP client for Stalwart's management API, authenticated with an API key.
pub struct Client {
    http: reqwest::Client,
    base: Url,
    api_key: String,
    session: OnceCell<Session>,
}

struct Session {
    api_url: Url,
    account_id: String,
}

impl Client {
    /// Validates the URL and prepares the HTTP client. Nothing is sent yet.
    ///
    /// # Errors
    ///
    /// Returns an error when `base` is not an `http://` or `https://` URL.
    pub fn new(base: &str, api_key: impl Into<String>) -> Result<Self, Error> {
        let base = Url::parse(base).map_err(|e| Error::Protocol(format!("invalid URL: {e}")))?;
        // "localhost:8080" parses as a URL with the scheme "localhost", which cannot be joined.
        if !matches!(base.scheme(), "http" | "https") || base.cannot_be_a_base() {
            return Err(Error::Protocol(
                "the URL must start with http:// or https://".into(),
            ));
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(20))
                .build()?,
            base,
            api_key: api_key.into(),
            session: OnceCell::new(),
        })
    }

    /// Verifies the URL, the key and the permissions stalias relies on.
    pub async fn check(&self) -> Result<(), Error> {
        self.domains().await?;
        self.accounts().await?;
        // An update without any target changes nothing, but Stalwart still checks the
        // permission to update accounts. Without it, creating an alias would fail later.
        self.call("x:Account/set", json!({ "update": {} })).await?;
        Ok(())
    }

    /// All domains known to Stalwart.
    pub async fn domains(&self) -> Result<Vec<Domain>, Error> {
        let res = self
            .call(
                "x:Domain/get",
                json!({ "ids": null, "properties": ["name"] }),
            )
            .await?;
        parse_list(&res)
    }

    /// Lists user accounts. Groups are skipped because nobody logs in as them.
    pub async fn accounts(&self) -> Result<Vec<Account>, Error> {
        let res = self
            .call(
                "x:Account/get",
                json!({ "ids": null, "properties": ["name", "@type", "emailAddress", "aliases"] }),
            )
            .await?;
        let raw: Vec<RawAccount> = parse_list(&res)?;
        Ok(raw
            .into_iter()
            .filter(|a| a.kind.as_deref().is_none_or(|k| k == "User"))
            .map(Account::from)
            .collect())
    }

    /// One mailbox by id, or `None` if it does not exist.
    pub async fn account(&self, id: &str) -> Result<Option<Account>, Error> {
        let res = self
            .call(
                "x:Account/get",
                json!({ "ids": [id], "properties": ["name", "@type", "emailAddress", "aliases"] }),
            )
            .await?;
        let raw: Vec<RawAccount> = parse_list(&res)?;
        Ok(raw.into_iter().next().map(Account::from))
    }

    /// Applies a JMAP patch (e.g. `"aliases/3": {...}`) to one account.
    pub async fn patch_account(&self, id: &str, patch: Map<String, Value>) -> Result<(), Error> {
        let res = self
            .call("x:Account/set", json!({ "update": { id: patch } }))
            .await?;
        match res.get("notUpdated").and_then(|m| m.get(id)) {
            None => Ok(()),
            Some(err) => Err(set_error(err)),
        }
    }

    async fn session(&self) -> Result<&Session, Error> {
        self.session
            .get_or_try_init(|| async {
                let url = self
                    .base
                    .join("jmap/session")
                    .expect("http(s) URLs can be joined");
                let res = self.http.get(url).bearer_auth(&self.api_key).send().await?;
                check_status(res.status())?;
                let body: Value = res.json().await?;
                // Stalwart advertises its configured public hostname, which is often not
                // reachable from here (internal Docker network, split DNS). Keep only the path.
                let advertised = body["apiUrl"]
                    .as_str()
                    .and_then(|p| self.base.join(p).ok())
                    .ok_or_else(|| Error::Protocol("session has no apiUrl".into()))?;
                let mut api_url = self.base.clone();
                api_url.set_path(advertised.path());
                let account_id = USING
                    .iter()
                    .rev()
                    .find_map(|cap| body["primaryAccounts"][cap].as_str())
                    .or_else(|| {
                        body["accounts"]
                            .as_object()?
                            .keys()
                            .next()
                            .map(String::as_str)
                    })
                    .map(str::to_owned)
                    .ok_or_else(|| Error::Protocol("session has no account".into()))?;
                Ok(Session {
                    api_url,
                    account_id,
                })
            })
            .await
    }

    async fn call(&self, method: &str, mut args: Value) -> Result<Value, Error> {
        let session = self.session().await?;
        args["accountId"] = json!(session.account_id);
        let res = self
            .http
            .post(session.api_url.clone())
            .bearer_auth(&self.api_key)
            .json(&json!({ "using": USING, "methodCalls": [[method, args, "0"]] }))
            .send()
            .await?;
        check_status(res.status())?;
        let body: Value = res.json().await?;
        let (name, payload) = body["methodResponses"][0]
            .as_array()
            .and_then(|r| Some((r.first()?.as_str()?, r.get(1)?.clone())))
            .ok_or_else(|| Error::Protocol("missing method response".into()))?;
        if name == "error" {
            return Err(method_error(&payload));
        }
        Ok(payload)
    }
}

fn check_status(status: StatusCode) -> Result<(), Error> {
    match status {
        s if s.is_success() => Ok(()),
        StatusCode::UNAUTHORIZED => Err(Error::Unauthorized),
        StatusCode::FORBIDDEN => Err(Error::Forbidden(
            "authenticate (or the key is not allowed from this IP)".into(),
        )),
        s => Err(Error::Protocol(format!("HTTP {s}"))),
    }
}

fn method_error(payload: &Value) -> Error {
    let kind = payload["type"].as_str().unwrap_or("unknown").to_owned();
    let description = payload["description"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    match kind.as_str() {
        "forbidden" => Error::Forbidden(description),
        "unknownMethod" => Error::Unsupported(description),
        _ => Error::Jmap { kind, description },
    }
}

fn set_error(err: &Value) -> Error {
    match err["type"].as_str() {
        Some("primaryKeyViolation") => Error::Conflict,
        Some("forbidden") => {
            Error::Forbidden(err["description"].as_str().unwrap_or_default().into())
        }
        kind => Error::Jmap {
            kind: kind.unwrap_or("unknown").into(),
            description: err["description"].as_str().unwrap_or_default().into(),
        },
    }
}

fn parse_list<T: for<'de> Deserialize<'de>>(res: &Value) -> Result<Vec<T>, Error> {
    serde_json::from_value(res["list"].clone())
        .map_err(|e| Error::Protocol(format!("cannot parse list: {e}")))
}
