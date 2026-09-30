//! HTTP client for the plugin-initiated enrollment flow.
use crate::secret_store::Credentials;
use std::time::Duration;

/// Longest api key or secret, in bytes, accepted from the server.
const MAX_CRED_LEN: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum EnrollError {
    #[error("invalid or expired token")]
    InvalidToken,
    #[error("enrollment request failed: {0}")]
    Http(String),
    #[error("enrollment response invalid or incomplete")]
    InvalidResponse,
    #[error("insecure base_url: https:// is required (localhost/127.0.0.1 excepted)")]
    InsecureUrl,
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn validate(creds: &Credentials) -> Result<(), EnrollError> {
    if creds.user_id.is_nil()
        || !creds.api_key.starts_with("rpa_")
        || creds.api_key.len() > MAX_CRED_LEN
        || !creds.secret.starts_with("rps_")
        || creds.secret.len() > MAX_CRED_LEN
    {
        return Err(EnrollError::InvalidResponse);
    }
    Ok(())
}

/// Requires `https://`, except `http://localhost` / `http://127.0.0.1`
/// (any port) for tests and local dev.
fn validate_url(base_url: &str) -> Result<(), EnrollError> {
    let parsed = url::Url::parse(base_url).map_err(|_| EnrollError::InsecureUrl)?;
    match parsed.scheme().to_ascii_lowercase().as_str() {
        "https" => Ok(()),
        "http" => match parsed.host_str() {
            Some("localhost") | Some("127.0.0.1") => Ok(()),
            _ => Err(EnrollError::InsecureUrl),
        },
        _ => Err(EnrollError::InsecureUrl),
    }
}

fn build_client(connect_timeout: Duration, timeout: Duration) -> reqwest::Client {
    // Never follow redirects: a redirect to another host (or an https→http
    // downgrade) would re-send the email and token to a URL we never
    // validated.
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(connect_timeout)
        .timeout(timeout)
        .build()
        .expect("reqwest client config is always valid")
}

/// reqwest's `Display` for a timed-out request doesn't mention "timeout"
/// (e.g. "error sending request for url (...)"); surface it explicitly via
/// `is_timeout()` so callers can distinguish timeouts from other transport
/// failures without string-matching reqwest's undocumented wording.
fn map_request_error(e: reqwest::Error) -> EnrollError {
    if e.is_timeout() {
        EnrollError::Http(format!("request timed out: {e}"))
    } else {
        EnrollError::Http(e.to_string())
    }
}

pub async fn request(base_url: &str, email: &str) -> Result<(), EnrollError> {
    request_with_timeouts(base_url, email, CONNECT_TIMEOUT, REQUEST_TIMEOUT).await
}

async fn request_with_timeouts(
    base_url: &str,
    email: &str,
    connect_timeout: Duration,
    timeout: Duration,
) -> Result<(), EnrollError> {
    validate_url(base_url)?;
    let resp = build_client(connect_timeout, timeout)
        .post(format!("{base_url}/api/enroll/request"))
        .json(&serde_json::json!({ "email": email }))
        .send()
        .await
        .map_err(map_request_error)?;

    if resp.status().as_u16() == 202 {
        Ok(())
    } else {
        Err(EnrollError::Http(format!("status {}", resp.status())))
    }
}

pub async fn claim(base_url: &str, email: &str, token: &str) -> Result<Credentials, EnrollError> {
    claim_with_timeouts(base_url, email, token, CONNECT_TIMEOUT, REQUEST_TIMEOUT).await
}

async fn claim_with_timeouts(
    base_url: &str,
    email: &str,
    token: &str,
    connect_timeout: Duration,
    timeout: Duration,
) -> Result<Credentials, EnrollError> {
    validate_url(base_url)?;
    let resp = build_client(connect_timeout, timeout)
        .post(format!("{base_url}/api/enroll/claim"))
        .json(&serde_json::json!({ "email": email, "token": token }))
        .send()
        .await
        .map_err(map_request_error)?;

    match resp.status().as_u16() {
        200 => {
            // Any undecodable 200 body (missing/unparseable fields, non-JSON)
            // is an invalid enrollment response, not a transport error.
            let creds = resp
                .json::<Credentials>()
                .await
                .map_err(|_| EnrollError::InvalidResponse)?;
            validate(&creds)?;
            Ok(creds)
        }
        401 => Err(EnrollError::InvalidToken),
        s => Err(EnrollError::Http(format!("status {s}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn request_treats_202_as_ok() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/enroll/request"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        assert!(request(&server.uri(), "a@b.com").await.is_ok());
    }

    #[tokio::test]
    async fn claim_returns_credentials_on_200() {
        let server = MockServer::start().await;
        let user_id = uuid::Uuid::new_v4();
        Mock::given(method("POST"))
            .and(path("/api/enroll/claim"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "api_key": "rpa_x", "secret": "rps_y", "user_id": user_id
            })))
            .mount(&server)
            .await;

        let creds = claim(&server.uri(), "a@b.com", "TOK").await.unwrap();
        assert_eq!(creds.api_key, "rpa_x");
        assert_eq!(creds.user_id, user_id);
    }

    #[tokio::test]
    async fn claim_never_follows_redirects() {
        // A 307 re-sends the POST body (email + token) to the Location
        // target — possibly a different host or a plain-http downgrade. The
        // client must surface the redirect status instead of following it.
        let attacker = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/enroll/claim"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "api_key": "rpa_x", "secret": "rps_y", "user_id": uuid::Uuid::new_v4()
            })))
            .expect(0)
            .mount(&attacker)
            .await;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/enroll/claim"))
            .respond_with(ResponseTemplate::new(307).insert_header(
                "location",
                format!("{}/api/enroll/claim", attacker.uri()).as_str(),
            ))
            .mount(&server)
            .await;

        assert!(matches!(
            claim(&server.uri(), "a@b.com", "TOK").await,
            Err(EnrollError::Http(s)) if s.contains("307")
        ));
    }

    #[tokio::test]
    async fn claim_rejects_missing_or_nil_user_id() {
        let missing = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/enroll/claim"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"api_key":"rpa_x","secret":"rps_y"})),
            )
            .mount(&missing)
            .await;
        assert!(matches!(
            claim(&missing.uri(), "a@b.com", "TOK").await,
            Err(EnrollError::InvalidResponse)
        ));

        let nil = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/enroll/claim"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "api_key": "rpa_x",
                "secret": "rps_y",
                "user_id": "00000000-0000-0000-0000-000000000000"
            })))
            .mount(&nil)
            .await;
        assert!(matches!(
            claim(&nil.uri(), "a@b.com", "TOK").await,
            Err(EnrollError::InvalidResponse)
        ));
    }

    #[tokio::test]
    async fn claim_rejects_bad_prefix_or_oversized_fields() {
        let user_id = uuid::Uuid::new_v4();

        let bad_prefix = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/enroll/claim"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "api_key": "xxx_bad", "secret": "rps_y", "user_id": user_id
            })))
            .mount(&bad_prefix)
            .await;
        assert!(matches!(
            claim(&bad_prefix.uri(), "a@b.com", "TOK").await,
            Err(EnrollError::InvalidResponse)
        ));

        let oversized = MockServer::start().await;
        let oversized_key = format!("rpa_{}", "a".repeat(200));
        Mock::given(method("POST"))
            .and(path("/api/enroll/claim"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "api_key": oversized_key, "secret": "rps_y", "user_id": user_id
            })))
            .mount(&oversized)
            .await;
        assert!(matches!(
            claim(&oversized.uri(), "a@b.com", "TOK").await,
            Err(EnrollError::InvalidResponse)
        ));

        let bad_secret_prefix = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/enroll/claim"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "api_key": "rpa_x", "secret": "xxx_bad", "user_id": user_id
            })))
            .mount(&bad_secret_prefix)
            .await;
        assert!(matches!(
            claim(&bad_secret_prefix.uri(), "a@b.com", "TOK").await,
            Err(EnrollError::InvalidResponse)
        ));

        let oversized_secret = MockServer::start().await;
        let oversized_secret_value = format!("rps_{}", "a".repeat(200));
        Mock::given(method("POST"))
            .and(path("/api/enroll/claim"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "api_key": "rpa_x", "secret": oversized_secret_value, "user_id": user_id
            })))
            .mount(&oversized_secret)
            .await;
        assert!(matches!(
            claim(&oversized_secret.uri(), "a@b.com", "TOK").await,
            Err(EnrollError::InvalidResponse)
        ));
    }

    #[tokio::test]
    async fn claim_rejects_unparseable_user_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/enroll/claim"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "api_key": "rpa_x", "secret": "rps_y", "user_id": "not-a-uuid"
            })))
            .mount(&server)
            .await;
        assert!(matches!(
            claim(&server.uri(), "a@b.com", "TOK").await,
            Err(EnrollError::InvalidResponse)
        ));
    }

    #[tokio::test]
    async fn claim_rejects_plain_http_base_url() {
        // No wiremock server needed: rejection happens before any HTTP call.
        assert!(matches!(
            claim("http://example.com", "a@b.com", "TOK").await,
            Err(EnrollError::InsecureUrl)
        ));
        assert!(matches!(
            request("http://example.com", "a@b.com").await,
            Err(EnrollError::InsecureUrl)
        ));
    }

    #[tokio::test]
    async fn claim_allows_localhost_and_127_0_0_1_over_http() {
        // wiremock's server.uri() is http://127.0.0.1:<port>; the existing
        // wiremock-backed tests above only pass because this is allowed.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/enroll/request"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        assert!(request(&server.uri(), "a@b.com").await.is_ok());

        assert!(validate_url("http://localhost").is_ok());
        assert!(validate_url("http://localhost:8080").is_ok());
        assert!(validate_url("http://127.0.0.1:9999").is_ok());
        assert!(validate_url("https://example.com").is_ok());
    }

    #[tokio::test]
    async fn validate_url_accepts_uppercase_https_scheme() {
        assert!(validate_url("HTTPS://example.com").is_ok());
        assert!(validate_url("HTTP://example.com").is_err());
        assert!(validate_url("HTTP://localhost").is_ok());
    }

    #[tokio::test]
    async fn request_times_out() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/enroll/request"))
            .respond_with(
                ResponseTemplate::new(202).set_delay(std::time::Duration::from_millis(300)),
            )
            .mount(&server)
            .await;

        let result = request_with_timeouts(
            &server.uri(),
            "a@b.com",
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(50),
        )
        .await;
        assert!(
            matches!(&result, Err(EnrollError::Http(msg)) if msg.to_lowercase().contains("time")),
            "expected a timeout-flavored Http error, got {result:?}"
        );
    }

    #[tokio::test]
    async fn claim_maps_401_to_invalid_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/enroll/claim"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        assert!(matches!(
            claim(&server.uri(), "a@b.com", "TOK").await,
            Err(EnrollError::InvalidToken)
        ));
    }
}
