use anyhow::{Context, Result, bail};
use serde::Deserialize;

const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const BOOTSTRAP_TOKEN_TYPE: &str = "urn:t3:params:oauth:token-type:environment-bootstrap";
const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";

#[derive(Debug, Deserialize)]
pub struct AccessToken {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: f64,
    pub scope: String,
}

#[derive(Debug, Deserialize)]
struct WebSocketTicket {
    ticket: String,
}

/// Accepts a raw pairing credential or a `/pair#token=...` URL.
fn extract_credential(input: &str) -> String {
    if let Ok(url) = url::Url::parse(input) {
        if let Some(fragment) = url.fragment() {
            for (key, value) in url::form_urlencoded::parse(fragment.as_bytes()) {
                if key == "token" {
                    return value.into_owned();
                }
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
        bail!("token exchange rejected ({status}): {body}");
    }
    let token: AccessToken = serde_json::from_str(&body).context("decoding token response")?;
    if token.token_type != "Bearer" {
        bail!("server issued a {} token; only Bearer is supported", token.token_type);
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
        bail!("websocket ticket rejected ({status}): {body}");
    }
    let ticket: WebSocketTicket = serde_json::from_str(&body).context("decoding ticket")?;
    Ok(ticket.ticket)
}
