use anyhow::{Context, Result, bail};
use serde::Deserialize;

const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const BOOTSTRAP_TOKEN_TYPE: &str = "urn:t3:params:oauth:token-type:environment-bootstrap";
const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";

#[derive(Debug, Deserialize)]
pub struct AccessToken {
    pub access_token: String,
    pub token_type: String,
    pub scope: String,
}

#[derive(Debug, Deserialize)]
struct WebSocketTicket {
    ticket: String,
}

/// A request the server answered with a status instead of what was asked for. Kept as a
/// type rather than a sentence because what to do about it turns on which status it was:
/// a credential the server will not accept is not going to be accepted by asking again,
/// and a server halfway through starting is.
#[derive(Debug)]
pub struct HttpStatus {
    /// What was being asked for, for the message.
    pub what: &'static str,
    pub status: reqwest::StatusCode,
    pub body: String,
}

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} rejected ({}): {}", self.what, self.status, self.body)
    }
}

impl std::error::Error for HttpStatus {}

impl HttpStatus {
    /// Whether asking again is pointless. A client error is this client's fault or this
    /// token's, and neither changes by the second; a server error, a timeout and a rate
    /// limit all pass. Everything else — a refused socket, a name that will not
    /// resolve — never reaches here, and is retried as it always was.
    pub fn is_settled(&self) -> bool {
        self.status.is_client_error()
            && !matches!(
                self.status,
                reqwest::StatusCode::REQUEST_TIMEOUT | reqwest::StatusCode::TOO_MANY_REQUESTS
            )
    }

    /// Whether the server is refusing the stored token itself, which is the one of these
    /// with something the person at the keyboard can do about it.
    pub fn is_credential(&self) -> bool {
        matches!(
            self.status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        )
    }

    /// What to put in front of somebody, which for a refused token is what to do about
    /// it. The body is the server's own words and is worth keeping, but it is a line in
    /// a chat window rather than a page, so only the first of them.
    pub fn report(&self) -> String {
        if self.is_credential() {
            return format!(
                "the server would not accept the stored token ({}) ·                  run `tria pair <credential>` again, then `:reconnect`",
                self.status.as_u16()
            );
        }
        let body: String = self
            .body
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(96)
            .collect();
        format!("{} refused ({}): {body}", self.what, self.status.as_u16())
    }
}

/// Accepts a raw pairing credential or a `/pair#token=...` URL.
fn extract_credential(input: &str) -> String {
    if let Ok(url) = url::Url::parse(input)
        && let Some(fragment) = url.fragment()
    {
        for (key, value) in url::form_urlencoded::parse(fragment.as_bytes()) {
            if key == "token" {
                return value.into_owned();
            }
        }
    }
    input.trim().to_string()
}

pub async fn exchange_pairing_credential(origin: &str, input: &str) -> Result<AccessToken> {
    let credential = extract_credential(input);
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{origin}/oauth/token"))
        .form(&[
            ("grant_type", GRANT_TYPE),
            ("subject_token", credential.as_str()),
            ("subject_token_type", BOOTSTRAP_TOKEN_TYPE),
            ("requested_token_type", ACCESS_TOKEN_TYPE),
            ("client_label", "tria"),
            ("client_device_type", "desktop"),
            ("client_os", std::env::consts::OS),
        ])
        .send()
        .await
        .context("token exchange request failed")?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(HttpStatus {
            what: "token exchange",
            status,
            body,
        }
        .into());
    }
    let token: AccessToken = serde_json::from_str(&body).context("decoding token response")?;
    if token.token_type != "Bearer" {
        bail!(
            "server issued a {} token; only Bearer is supported",
            token.token_type
        );
    }
    Ok(token)
}

pub async fn websocket_ticket(origin: &str, bearer: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{origin}/api/auth/websocket-ticket"))
        .bearer_auth(bearer)
        .send()
        .await
        .context("websocket ticket request failed")?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(HttpStatus {
            what: "websocket ticket",
            status,
            body,
        }
        .into());
    }
    let ticket: WebSocketTicket = serde_json::from_str(&body).context("decoding ticket")?;
    Ok(ticket.ticket)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(status: u16) -> HttpStatus {
        HttpStatus {
            what: "websocket ticket",
            status: reqwest::StatusCode::from_u16(status).unwrap(),
            body: "{\"error\":\"nope\"}\nand more of it".into(),
        }
    }

    /// The client used to read every failed request as a refused token, because every
    /// one of them said "rejected". A server restarting answers 503 for a second or two,
    /// and that second cost the whole session: tria stopped trying and stayed stopped.
    #[test]
    fn only_the_server_refusing_us_is_worth_giving_up_on() {
        assert!(refusal(401).is_settled(), "the token is not accepted");
        assert!(refusal(403).is_settled());
        assert!(refusal(404).is_settled(), "this client is asking wrongly");

        for passing in [408, 429, 500, 502, 503, 504] {
            assert!(
                !refusal(passing).is_settled(),
                "{passing} is a server to ask again"
            );
        }
    }

    /// A red dot and the words "auth failed" leave somebody with nothing to do. The one
    /// of these with a remedy says what it is.
    #[test]
    fn a_refused_token_says_what_to_do_about_it() {
        let said = refusal(401).report();
        assert!(said.contains("tria pair"), "{said}");
        assert!(said.contains(":reconnect"), "{said}");

        // The rest carry the server's own words, and only as far as a line goes.
        let said = refusal(500).report();
        assert!(said.contains("500"), "{said}");
        assert!(said.contains("nope"), "{said}");
        assert!(!said.contains("and more of it"), "{said}");
    }
}
