//! The tokens of the Runners App's key ([`KeyTokens`]) against a fake GitHub at the HTTP edge: what a host that still
//! holds the key signs with until the console's scale set calls answered it.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cf_fake_github::httpd::Reply;
use cf_fake_github::{FakeGithub, Options, test_key};
use cf_ghapp::{AppCredentials, AppKey, Clock, Error, GitHubApp, Http, Installation, KeyTokens};
use cf_harness::{TestClock, fast, key_app, key_tokens};
use cf_secret::Secret;

struct Harness {
    fake: FakeGithub,
    tokens: Arc<KeyTokens>,
    app: GitHubApp,
    clock: Arc<TestClock>,
}

async fn harness() -> Harness {
    let fake = FakeGithub::start(Options::default()).await;
    fake.add_repo("org/app", true);
    fake.add_repo("org/other", false);
    let clock = Arc::new(TestClock::default());
    let http = Http::new(Duration::from_secs(10), fast());
    let tokens = key_tokens(&fake, http.clone(), clock.clone());
    let app = key_app(&fake, &tokens, http);
    Harness {
        fake,
        tokens,
        app,
        clock,
    }
}

fn claims_of(authorization: &str) -> serde_json::Value {
    let jwt = authorization.strip_prefix("Bearer ").expect("bearer");
    let payload = jwt.split('.').nth(1).expect("payload");
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).expect("base64")).expect("json")
}

#[test]
fn a_key_that_is_not_pem_is_refused_without_echoing_it() {
    let err = AppKey::from_pem(&Secret::new("not a key SECRETMARKER".into()))
        .expect_err("refused")
        .to_string();
    assert!(!err.contains("SECRETMARKER"), "{err}");
    let truncated = test_key().private_pem[..200].to_string();
    assert!(AppKey::from_pem(&Secret::new(truncated)).is_err());
    let wrong_label = test_key().private_pem.replace("RSA PRIVATE KEY", "CERTIFICATE");
    assert!(AppKey::from_pem(&Secret::new(wrong_label)).is_err());
}

#[tokio::test]
async fn the_key_and_tokens_never_show_in_debug_output_or_errors() {
    let h = harness().await;
    let key = AppKey::from_pem(&Secret::new(test_key().private_pem.clone())).expect("a key");
    assert_eq!(format!("{key:?}"), "AppKey(..)");
    let body_line = test_key().private_pem.lines().nth(2).expect("a line").to_string();
    let shown = format!(
        "{:?} {:?} {:?}",
        h.app,
        h.tokens,
        AppCredentials {
            client_id: "x".into(),
            key
        }
    );
    assert!(!shown.contains(&body_line), "{shown}");
    let token = h.app.installation_token(1).await.expect("a token");
    let error = h
        .app
        .installation_token(99)
        .await
        .expect_err("no such installation");
    for text in [error.to_string(), format!("{error:?}")] {
        assert!(
            !text.contains(token.expose().as_str()) && !text.contains(&body_line),
            "{text}"
        );
    }
    assert_eq!(format!("{token:?}"), "Secret(..)");
    let jwt = h.tokens.jwt().expect("a JWT");
    assert!(!format!("{jwt:?} {:?} {:?}", h.app, h.tokens).contains(jwt.expose().as_str()));
}

#[tokio::test]
async fn the_jwt_is_what_github_accepts() {
    let h = harness().await;
    assert_eq!(
        h.app
            .installations()
            .await
            .expect("the fake checks the JWT as GitHub does"),
        vec![Installation { id: 1 }]
    );
    let seen = h
        .fake
        .requests()
        .into_iter()
        .find(|r| r.line == "GET /app/installations")
        .expect("asked");
    let claims = claims_of(&seen.authorization.expect("signed"));
    assert_eq!(claims["iss"], "Iv23test");
    let (iat, exp) = (
        claims["iat"].as_i64().expect("iat"),
        claims["exp"].as_i64().expect("exp"),
    );
    assert!(
        exp - iat <= 600,
        "GitHub refuses a JWT that lives longer than ten minutes"
    );
    let now = h
        .clock
        .now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs() as i64;
    assert!(
        iat < now,
        "issued in the past: a clock a little ahead of GitHub's makes one issued now invalid"
    );
}

#[tokio::test]
async fn a_token_is_reused_until_five_minutes_before_it_expires() {
    let h = harness().await;
    let first = h.app.installation_token(1).await.expect("a token");
    h.clock.advance(Duration::from_secs(54 * 60));
    h.fake.set_clock_offset(54 * 60);
    let again = h.app.installation_token(1).await.expect("a token");
    assert_eq!(
        again.expose(),
        first.expose(),
        "replaced a token with 6 minutes left"
    );
    assert_eq!(h.fake.count("POST /app/installations/1/access_tokens"), 1);
    h.clock.advance(Duration::from_secs(2 * 60));
    h.fake.set_clock_offset(56 * 60);
    let fresh = h.app.installation_token(1).await.expect("a token");
    assert_ne!(fresh.expose(), first.expose(), "kept a token with 4 minutes left");
}

#[tokio::test]
async fn token_errors_are_results_and_are_not_cached() {
    let h = harness().await;
    h.fake.fail(
        "access_tokens",
        Reply::status_json(401, &serde_json::json!({"message": "Bad credentials"})),
        1,
    );
    let err = h.app.installation_token(1).await.expect_err("refused");
    assert_eq!(err.status(), Some(401));
    h.fake.fail(
        "access_tokens",
        Reply::status_json(
            201,
            &serde_json::json!({"token": "", "expires_at": "2030-01-01T00:00:00Z"}),
        ),
        1,
    );
    assert!(
        matches!(h.app.installation_token(1).await, Err(Error::Decode { .. })),
        "accepted an empty token"
    );
    h.fake.fail(
        "access_tokens",
        Reply::status_json(201, &serde_json::json!({"token": "abc"})),
        1,
    );
    assert!(
        matches!(h.app.installation_token(1).await, Err(Error::Decode { .. })),
        "a token without expires_at"
    );
    let before = h.fake.count("access_tokens");
    h.app.installation_token(1).await.expect("recovers");
    assert_eq!(h.fake.count("access_tokens"), before + 1, "no failure is kept");
}

#[tokio::test]
async fn a_machine_clock_that_is_wrong_still_gets_a_token_github_accepts() {
    for offset in [-1200i64, -120, 700, 3600] {
        let h = harness().await;
        h.fake.set_clock_offset(offset);
        h.app
            .installations()
            .await
            .unwrap_or_else(|e| panic!("clock {offset}s off: {e}"));
        let before = h.fake.count("GET /app/installations");
        h.app.installations().await.expect("signed right");
        assert_eq!(h.fake.count("GET /app/installations"), before + 1, "learned once");
    }
}

#[tokio::test]
async fn a_registration_token_is_asked_with_the_installation_token() {
    let h = harness().await;
    let token = h.tokens.registration_token(1, "org/app").await.expect("a token");
    assert!(token.expose().starts_with("AREG_"));
    assert_eq!(
        h.tokens
            .registration_token(1, "org/gone")
            .await
            .expect_err("no such repository")
            .status(),
        Some(404)
    );
}

#[tokio::test]
async fn concurrent_callers_share_one_token_request() {
    let h = harness().await;
    let calls: Vec<_> = (0..16)
        .map(|_| {
            let app = h.app.clone();
            tokio::spawn(async move { app.installation_token(1).await })
        })
        .collect();
    let mut values = std::collections::HashSet::new();
    for call in calls {
        values.insert(call.await.expect("ran").expect("a token").expose().clone());
    }
    assert_eq!(values.len(), 1);
    assert_eq!(h.fake.count("access_tokens"), 1);
}

#[test]
fn credentials_load_from_a_folder_and_a_key_others_can_read_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("folder");
    std::fs::write(
        dir.path().join("app.json"),
        r#"{"client_id":"Iv23abc","other":1}"#,
    )
    .expect("written");
    let pem = dir.path().join("app.pem");
    std::fs::write(&pem, &test_key().private_pem).expect("written");
    std::fs::set_permissions(&pem, std::fs::Permissions::from_mode(0o644)).expect("a mode");
    let err = AppCredentials::load(dir.path())
        .expect_err("others can read it")
        .to_string();
    assert!(err.contains("0600") && err.contains("app.pem"), "{err}");
    assert!(!err.contains("BEGIN"));
    std::fs::set_permissions(&pem, std::fs::Permissions::from_mode(0o600)).expect("a mode");
    assert_eq!(
        AppCredentials::load(dir.path()).expect("loads").client_id,
        "Iv23abc"
    );
    std::fs::write(dir.path().join("app.json"), r#"{"client_id":""}"#).expect("written");
    assert!(
        AppCredentials::load(dir.path())
            .expect_err("no client")
            .to_string()
            .contains("client_id")
    );
    std::fs::remove_file(&pem).expect("removed");
    std::fs::write(dir.path().join("app.json"), r#"{"client_id":"Iv23abc"}"#).expect("written");
    assert!(AppCredentials::load(dir.path()).is_err());
}
