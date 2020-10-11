use serde::{Deserialize, Serialize};
use std::error;
use std::io::Write;
use uuid::Uuid;

use tiny_http::{Response, Server};

use crate::ha_api;
use crate::ha_api::GetAccessTokenResponse;
use std::collections::HashMap;
use url::Url;

use tungstenite::{connect, Message};
use serde_json::Value;

// HA creates tokens for 10 years so we do the same
const LONG_LIVED_TOKEN_VALID_FOR: u32 = 3652425;

const LONG_LIVED_TOKEN_WS_COMMAND_ID: u32 = 11;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HaConfig {
    pub host: String,
    #[serde(rename = "long-lived-token")]
    pub long_lived_token: Option<String>,
    #[serde(rename = "device-id")]
    pub device_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct YamlConfig {
    pub ha: HaConfig,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct WsAuthRequest {
    #[serde(rename(serialize = "type"))]
    auth_type: String,
    access_token: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct WsLongLivedAccessTokenRequest {
    id: u32,
    #[serde(rename(serialize = "type"))]
    command_type: String,
    client_name: String,
    lifespan: u32,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct WsLongLivedAccessTokenResponse {
    id: u32,
    #[serde(rename(deserialize = "type"))]
    command_type: String,
    success: bool,
    result: Option<String>,
}

type Result<T> = std::result::Result<T, Box<dyn error::Error>>;

pub async fn wait_for_token(config: &YamlConfig) -> Result<GetAccessTokenResponse> {
    let server = Server::http("0.0.0.0:8000").unwrap();
    let mut get_token_response: Option<GetAccessTokenResponse> = None;
    println!("Open http://{}/auth/authorize?client_id=http%3A%2F%2Flocalhost%3A8000&redirect_uri=http%3A%2F%2Flocalhost%3A8000%2Fcallback in your browser", config.ha.host.as_str());
    // blocks until the next request is received
    match server.recv() {
        Ok(request) => {
            let url = format!("http://localhost:8000{}", request.url());
            let query_params: HashMap<_, _> = Url::parse(url.as_str())
                .unwrap()
                .query_pairs()
                .into_owned()
                .collect();
            match query_params.get("code") {
                Some(code) => {
                    request.respond(Response::from_string(
                        "Halcyon now authenticated to Home Assistant. You can close this page now.",
                    ))?;
                    let either = ha_api::get_access_token(config, code.to_string()).await?;
                    match either {
                        either::Left(_) => {
                            get_token_response = None;
                        }
                        either::Right(succ) => {
                            get_token_response = Some(succ);
                        }
                    }
                }
                None => {
                    request.respond(
                        Response::from_string(
                            "Something went wrong with authenticating Halcyon with Home Assistant",
                        )
                        .with_status_code(500),
                    )?;
                    get_token_response = None;
                }
            }
        }
        Err(e) => {
            println!("error: {}", e);
        }
    };
    get_token_response
        .map(Ok)
        .unwrap_or_else(|| Err("Could not retrieve HA access token".into()))
}

fn get_long_lived_token_from_ws(config: &YamlConfig, access_token: String) -> Result<WsLongLivedAccessTokenResponse> {
    let ws_url = format!("ws://{}/api/websocket", config.ha.host);
    let url = Url::parse(ws_url.as_str())?;
    let (mut socket, _) = connect(url)?;

    let mut maybe_long_lived_token_response: Option<WsLongLivedAccessTokenResponse> = None;
    loop {
        let msg = socket.read_message()?.into_text()?;
        let msg_as_json: Value = serde_json::from_str(msg.as_str())?;
        let maybe_response_type = msg_as_json.get("type").and_then(|response_type| response_type.as_str());

        match maybe_response_type {
            Some(response_type) => {
                match response_type {
                    "auth_required" => {
                        let req = WsAuthRequest {
                            auth_type: "auth".to_string(),
                            access_token: access_token.clone(),
                        };

                        let req_as_str = Message::Text(serde_json::to_string(&req)?);
                        socket.write_message(req_as_str)?;
                    }
                    "auth_ok" => {
                        let req = WsLongLivedAccessTokenRequest {
                            id: LONG_LIVED_TOKEN_WS_COMMAND_ID,
                            command_type: "auth/long_lived_access_token".to_string(),
                            client_name: "Halcyon".to_string(),
                            lifespan: LONG_LIVED_TOKEN_VALID_FOR,
                        };

                        let req_as_str = Message::Text(serde_json::to_string(&req)?);
                        socket.write_message(req_as_str).unwrap();
                    }
                    "result" => {
                        let response: WsLongLivedAccessTokenResponse =
                            serde_json::from_str(msg.as_str())?;

                        maybe_long_lived_token_response = Some(response);
                        break;
                    }
                    "auth_invalid" => {
                        let error_msg = msg_as_json.get("message").and_then(|error| error.as_str()).unwrap_or("");
                        println!("Error authorizing websocket {}", error_msg);
                        break;
                    }
                    _ => {
                        println!("Unexpected response from websocket during authorization {}", msg)
                    }
                }
            }
            None => {
                println!("Unexpected response from websocket {}", msg);
                break;
            }
        }
    }
    maybe_long_lived_token_response
        .map(Ok)
        .unwrap_or_else(|| Err("Could not retrieve long lived token from websocket (perhaps it already has been created for Halcyon?)".into()))
}

impl YamlConfig {
    pub fn update_device_id_if_needed(self, file_name: &str) -> Result<Self> {
        let new_config = match self.ha.device_id {
            None => {
                let ha_config = HaConfig {
                    device_id: Some(Uuid::new_v4().to_string()),
                    ..self.ha
                };
                let config = YamlConfig { ha: ha_config };
                let new_config_str = serde_yaml::to_string(&config)?;
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(file_name)?;
                f.write_all(new_config_str.as_bytes())?;
                println!("no device-id found in config, so we are making one for you");
                config
            }
            Some(_) => self,
        };
        Ok(new_config)
    }

    pub async fn update_long_lived_access_token_if_needed(self, file_name: &str) -> Result<Self> {
        let new_config = match self.ha.long_lived_token {
            None => {
                let access_token_resp = wait_for_token(&self).await?;
                let long_lived_access_token_resp = get_long_lived_token_from_ws(&self, access_token_resp.access_token)?;
                let ha_config = HaConfig {
                    long_lived_token: long_lived_access_token_resp.result,
                    ..self.ha
                };
                let config = YamlConfig { ha: ha_config };
                let new_config_str = serde_yaml::to_string(&config)?;
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(file_name)?;
                f.write_all(new_config_str.as_bytes())?;
                println!(
                    "no long lived access token found in config, so we are making one for you"
                );
                config
            }
            Some(_) => self,
        };
        Ok(new_config)
    }
}

pub fn read_config_yml(file_name: &str) -> Result<YamlConfig> {
    let f = std::fs::File::open(file_name)?;
    let config: YamlConfig = serde_yaml::from_reader(f)?;
    Ok(config)
}