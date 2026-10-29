use std::time::Duration;

use reqwest::{RequestBuilder, Response};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use url::Url;

use crate::{DeviceInfo, Plan, StartError, proxy};

const API_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Serialize)]
pub(crate) struct OpenRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
    pub device: &'a DeviceInfo,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenResponse {
    pub public_url: String,
    pub ws_url: String,
    pub token: String,
    pub max_body_bytes: usize,
    #[serde(default)]
    pub plan: Option<Plan>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub account: Option<Account>,
    #[serde(default)]
    pub latest_agent_version: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub email: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_url: String,
    pub expires_in_seconds: u64,
    pub interval_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginUser {
    pub email: String,
    pub name: String,
    pub plan: Plan,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LoginPoll {
    Pending,
    Approved { token: String, user: LoginUser },
    Denied,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NameCheck {
    pub status: String,
    pub message: String,
    #[serde(default)]
    pub suggestions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VersionInfo {
    pub latest: String,
    pub download_url: String,
}

#[derive(Debug, Clone)]
pub struct Api {
    client: reqwest::Client,
    server: Url,
    proxy: Option<Url>,
}

impl Api {
    #[must_use]
    pub fn new(server: &Url, proxy: Option<Url>) -> Self {
        let mut client = reqwest::Client::builder().no_proxy();
        if let Some(proxy) = &proxy {
            let proxy = reqwest::Proxy::all(proxy.as_str()).expect("proxy URL is valid");
            client = client.proxy(proxy);
        }
        Self {
            client: client.build().expect("HTTP client always builds"),
            server: server.clone(),
            proxy,
        }
    }

    #[must_use]
    pub fn from_env(server: &Url) -> Self {
        Self::new(server, proxy::from_env(server))
    }

    #[must_use]
    pub const fn server(&self) -> &Url {
        &self.server
    }

    #[must_use]
    pub const fn proxy(&self) -> Option<&Url> {
        self.proxy.as_ref()
    }

    fn url(&self, path: &str) -> Url {
        self.server.join(path).expect("server URL has no path")
    }

    async fn send(&self, request: RequestBuilder) -> Result<Response, StartError> {
        let (server, proxy) = (&self.server, self.proxy.as_ref());
        let response = request
            .timeout(API_TIMEOUT)
            .send()
            .await
            .map_err(|e| StartError::from_http(server, proxy, &e))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.bytes().await.unwrap_or_default();
        Err(StartError::from_refusal(
            server,
            proxy,
            status.as_u16(),
            &body,
        ))
    }

    async fn call<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T, StartError> {
        self.send(request)
            .await?
            .json()
            .await
            .map_err(|e| StartError::from_http(&self.server, self.proxy.as_ref(), &e))
    }

    async fn call_empty(&self, request: RequestBuilder) -> Result<(), StartError> {
        self.send(request).await.map(drop)
    }

    pub(crate) async fn open(
        &self,
        body: &OpenRequest<'_>,
        token: Option<&str>,
    ) -> Result<OpenResponse, StartError> {
        let mut request = self.client.post(self.url("/api/tunnels/open")).json(body);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        self.call(request).await
    }

    pub async fn start_login(&self, device: &DeviceInfo) -> Result<LoginStart, StartError> {
        let request = self.client.post(self.url("/api/agent/login")).json(device);
        self.call(request).await
    }

    pub async fn poll_login(&self, device_code: &str) -> Result<LoginPoll, StartError> {
        let request = self
            .client
            .post(self.url("/api/agent/login/poll"))
            .json(&json!({ "device_code": device_code }));
        self.call(request).await
    }

    pub async fn logout(&self, token: &str) -> Result<(), StartError> {
        let request = self
            .client
            .post(self.url("/api/agent/logout"))
            .bearer_auth(token);
        self.call_empty(request).await
    }

    pub async fn check_name(&self, token: &str, name: &str) -> Result<NameCheck, StartError> {
        let mut url = self.url("/api/names/check");
        url.query_pairs_mut().append_pair("name", name);
        let request = self.client.get(url).bearer_auth(token);
        self.call(request).await
    }

    pub async fn register_name(
        &self,
        token: &str,
        name: &str,
        target: Option<&str>,
    ) -> Result<(), StartError> {
        let request = self
            .client
            .post(self.url("/api/names"))
            .json(&json!({ "name": name, "target": target }))
            .bearer_auth(token);
        self.call_empty(request).await
    }

    pub async fn version(&self) -> Result<VersionInfo, StartError> {
        self.call(self.client.get(self.url("/api/agent/version")))
            .await
    }

    pub async fn download(&self, url: &str) -> reqwest::Result<Vec<u8>> {
        let response = self
            .client
            .get(url)
            .timeout(DOWNLOAD_TIMEOUT)
            .send()
            .await?
            .error_for_status()?;
        Ok(response.bytes().await?.to_vec())
    }
}
