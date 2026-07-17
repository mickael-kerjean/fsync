use std::time::{Duration, SystemTime};

use futures_util::TryStreamExt;
use reqwest::header::{HeaderMap, AUTHORIZATION, CONTENT_TYPE, SET_COOKIE};
use reqwest::{Body, Method, Response, StatusCode};
use serde::Deserialize;
use url::Url;

use crate::ByteStream;

const COOKIE_NAME_SESSION: &str = "auth";
pub const DELTA_MEDIA_TYPE: &str = "application/vnd.filestash.delta.rdiff";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("not authenticated")]
    NotAuthenticated,
    #[error("permission denied")]
    PermissionDenied,
    #[error("not found")]
    NotFound,
    #[error("precondition failed")]
    PreconditionFailed,
    #[error("not a Filestash server")]
    NotFilestash,
    #[error("api error: {0}")]
    Api(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("invalid url: {0}")]
    Url(#[from] url::ParseError),
}

impl From<Error> for std::io::Error {
    fn from(err: Error) -> Self {
        match err {
            Error::NotFound => std::io::ErrorKind::NotFound.into(),
            Error::PermissionDenied => std::io::ErrorKind::PermissionDenied.into(),
            err => std::io::Error::other(err),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub name: String,
    pub kind: FileType,
    pub size: Option<u64>,
    pub mtime: Option<SystemTime>,
}

impl FileInfo {
    fn of(path: &str, headers: &reqwest::header::HeaderMap) -> Self {
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        Self {
            name: path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string(),
            kind: if header("content-type") == Some("inode/directory") {
                FileType::Directory
            } else {
                FileType::File
            },
            size: header("content-length").and_then(|v| v.parse().ok()),
            mtime: header("last-modified").and_then(|v| httpdate::parse_http_date(v).ok()),
        }
    }
}

#[derive(Clone)]
pub struct Sdk {
    url: Url,
    http: reqwest::Client,
    token: Option<String>,
    delta: std::sync::Arc<std::sync::OnceLock<bool>>,
}

pub struct SdkBuilder {
    url: String,
    insecure: bool,
}

impl SdkBuilder {
    pub fn insecure(mut self, insecure: bool) -> Self {
        self.insecure = insecure;
        self
    }

    pub fn token(self, token: String) -> Result<Sdk> {
        let mut sdk = Sdk::with_options(&self.url, self.insecure)?;
        sdk.set_token(token);
        Ok(sdk)
    }

    pub async fn probe(&self) -> Result<String> {
        Sdk::with_options(&self.url, self.insecure)?.probe().await
    }

    pub fn probe_blocking(&self) -> Result<String> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| Error::Api(err.to_string()))?
            .block_on(self.probe())
    }

    pub async fn login(self, user: &str, password: &str, storage: &str) -> Result<Sdk> {
        let mut sdk = Sdk::with_options(&self.url, self.insecure)?;
        sdk.authenticate(user, password, storage).await?;
        Ok(sdk)
    }
}

impl Sdk {
    pub fn builder(url: &str) -> SdkBuilder {
        SdkBuilder {
            url: url.to_string(),
            insecure: false,
        }
    }

    pub fn new(url: &str) -> Result<Self> {
        Self::with_options(url, false)
    }

    fn with_options(url: &str, insecure: bool) -> Result<Self> {
        let url = Url::parse(url.trim_end_matches('/'))?;
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(insecure)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            url,
            http,
            token: None,
            delta: Default::default(),
        })
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    pub fn set_token(&mut self, token: String) {
        self.token = Some(token);
    }

    pub async fn authenticate(&mut self, user: &str, password: &str, storage: &str) -> Result<()> {
        let mut url = self.api(&["api", "session", "auth", ""]);
        url.query_pairs_mut().append_pair("label", storage);
        let resp = self
            .http
            .post(url)
            .header("X-Requested-With", "SDKHttpRequest")
            .form(&[("user", user), ("password", password)])
            .send()
            .await?;
        if resp.status().as_u16() >= 400 {
            return Err(Error::InvalidCredentials);
        }
        let token = extract_token(resp.headers());
        if token.is_empty() {
            return Err(Error::InvalidCredentials);
        }
        self.token = Some(token);
        Ok(())
    }

    pub async fn ls(&self, path: &str) -> Result<Vec<FileInfo>> {
        #[derive(Deserialize)]
        struct Entry {
            name: String,
            #[serde(default)]
            size: i64,
            #[serde(default)]
            time: i64,
            #[serde(rename = "type")]
            kind: String,
        }
        let resp = self
            .request(Method::GET, &["api", "files", "ls"], &[("path", path)])
            .await?;
        let entries: Vec<Entry> = unwrap_results(resp).await?;
        Ok(entries
            .into_iter()
            .map(|e| FileInfo {
                name: e.name,
                kind: if e.kind == "directory" {
                    FileType::Directory
                } else {
                    FileType::File
                },
                size: u64::try_from(e.size).ok(),
                mtime: (e.time > 0)
                    .then(|| SystemTime::UNIX_EPOCH + Duration::from_millis(e.time as u64)),
            })
            .collect())
    }

    pub async fn stat(&self, path: &str) -> Result<FileInfo> {
        let resp = self
            .request(Method::HEAD, &["api", "files", "cat"], &[("path", path)])
            .await?;
        Ok(FileInfo::of(path, resp.headers()))
    }

    pub async fn cat(&self, path: &str) -> Result<(FileInfo, ByteStream)> {
        let resp = self
            .request(Method::GET, &["api", "files", "cat"], &[("path", path)])
            .await?;
        let info = FileInfo::of(path, resp.headers());
        Ok((
            info,
            Box::pin(resp.bytes_stream().map_err(std::io::Error::other)),
        ))
    }

    pub async fn probe(&self) -> Result<String> {
        let resp = self
            .http
            .get(self.api(&["about"]))
            .header("X-Requested-With", "SDKHttpRequest")
            .send()
            .await?;
        resp.headers()
            .get("X-Powered-By")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Filestash/"))
            .map(|version| {
                version
                    .split_whitespace()
                    .next()
                    .unwrap_or(version)
                    .to_string()
            })
            .ok_or(Error::NotFilestash)
    }

    pub async fn logout(&self) -> Result<()> {
        self.request(Method::DELETE, &["api", "session"], &[])
            .await
            .map(drop)
    }

    pub async fn thumbnail(&self, path: &str) -> Result<Vec<u8>> {
        let resp = self
            .request(
                Method::GET,
                &["api", "files", "cat"],
                &[("path", path), ("thumbnail", "true")],
            )
            .await?;
        Ok(resp.bytes().await?.to_vec())
    }

    pub async fn save(
        &self,
        path: &str,
        body: ByteStream,
        since: Option<SystemTime>,
    ) -> Result<Option<SystemTime>> {
        let mut url = self.api(&["api", "files", "cat"]);
        url.query_pairs_mut().append_pair("path", path);
        let mut req = self
            .http
            .post(url)
            .header("X-Requested-With", "SDKHttpRequest")
            .header(AUTHORIZATION, self.bearer()?);
        if let Some(since) = since {
            req = req.header("If-Unmodified-Since", httpdate::fmt_http_date(since));
        }
        let resp = req.body(Body::wrap_stream(body)).send().await?;
        let resp = check_status(resp).await?;
        Ok(resp
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| httpdate::parse_http_date(v).ok()))
    }

    pub async fn delta_supported(&self) -> bool {
        if let Some(cached) = self.delta.get() {
            return *cached;
        }
        let supported = self.probe_delta().await.unwrap_or(false);
        let _ = self.delta.set(supported);
        supported
    }

    async fn probe_delta(&self) -> Result<bool> {
        let resp = self
            .http
            .request(Method::OPTIONS, self.api(&["api", "files", "save"]))
            .header("X-Requested-With", "SDKHttpRequest")
            .header(AUTHORIZATION, self.bearer()?)
            .send()
            .await?;
        Ok(resp
            .headers()
            .get("Accept-Post")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains(DELTA_MEDIA_TYPE)))
    }

    pub async fn save_delta(
        &self,
        path: &str,
        body: Vec<u8>,
        since: SystemTime,
        base: Option<String>,
    ) -> Result<Option<SystemTime>> {
        let mut url = self.api(&["api", "files", "cat"]);
        url.query_pairs_mut().append_pair("path", path);
        let mut req = self
            .http
            .post(url)
            .header("X-Requested-With", "SDKHttpRequest")
            .header(AUTHORIZATION, self.bearer()?)
            .header(CONTENT_TYPE, DELTA_MEDIA_TYPE)
            .header("If-Unmodified-Since", httpdate::fmt_http_date(since));
        if let Some(base) = base {
            req = req.header("X-Copy-Source", base);
        }
        let resp = req.body(body).send().await?;
        let resp = check_status(resp).await?;
        Ok(resp
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| httpdate::parse_http_date(v).ok()))
    }

    pub async fn mkdir(&self, path: &str) -> Result<()> {
        self.request(Method::POST, &["api", "files", "mkdir"], &[("path", path)])
            .await
            .map(drop)
    }

    pub async fn rm(&self, path: &str) -> Result<()> {
        self.request(Method::POST, &["api", "files", "rm"], &[("path", path)])
            .await
            .map(drop)
    }

    pub async fn mv(&self, from: &str, to: &str) -> Result<()> {
        self.request(
            Method::POST,
            &["api", "files", "mv"],
            &[("from", from), ("to", to)],
        )
        .await
        .map(drop)
    }

    fn api(&self, segments: &[&str]) -> Url {
        let mut url = self.url.clone();
        url.path_segments_mut()
            .expect("base url cannot be a base")
            .extend(segments);
        url
    }

    fn bearer(&self) -> Result<String> {
        let token = self.token.as_deref().ok_or(Error::NotAuthenticated)?;
        Ok(format!("Bearer {token}"))
    }

    async fn request(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> Result<Response> {
        let mut url = self.api(segments);
        for (k, v) in query {
            url.query_pairs_mut().append_pair(k, v);
        }
        let resp = self
            .http
            .request(method, url)
            .header("X-Requested-With", "SDKHttpRequest")
            .header(AUTHORIZATION, self.bearer()?)
            .send()
            .await?;
        check_status(resp).await
    }
}

async fn check_status(resp: Response) -> Result<Response> {
    match resp.status() {
        s if s.is_success() => Ok(resp),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(Error::PermissionDenied),
        StatusCode::NOT_FOUND => Err(Error::NotFound),
        StatusCode::PRECONDITION_FAILED => Err(Error::PreconditionFailed),
        s => Err(Error::Api(format!("unexpected status {s}"))),
    }
}

async fn unwrap_results<T: serde::de::DeserializeOwned>(resp: Response) -> Result<T> {
    #[derive(Deserialize)]
    struct Envelope {
        status: String,
        results: serde_json::Value,
    }
    let body: Envelope = resp
        .json()
        .await
        .map_err(|e| Error::Api(format!("invalid json response: {e}")))?;
    if body.status != "ok" {
        return Err(Error::Api(format!("status: {}", body.status)));
    }
    serde_json::from_value(body.results).map_err(|e| Error::Api(format!("invalid results: {e}")))
}

pub fn assemble_token(cookies: &[(String, String)]) -> String {
    let mut parts: Vec<(u32, &str)> = cookies
        .iter()
        .filter_map(|(name, value)| {
            let index = name.strip_prefix(COOKIE_NAME_SESSION)?;
            let index = match index.is_empty() {
                true => 0,
                false => index.parse().ok()?,
            };
            Some((index, value.as_str()))
        })
        .collect();
    parts.sort_by_key(|(index, _)| *index);
    parts.into_iter().map(|(_, value)| value).collect()
}

pub fn normalize_server(input: &str) -> String {
    let input = input.trim();
    let url = match input.contains("://") {
        true => input.to_owned(),
        false => format!("https://{input}"),
    };
    url.trim_end_matches('/').to_owned()
}

fn extract_token(headers: &HeaderMap) -> String {
    let cookies: Vec<(&str, &str)> = headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next()?.trim().split_once('='))
        .collect();
    let mut token = String::new();
    for index in 0.. {
        let name = if index == 0 {
            COOKIE_NAME_SESSION.to_string()
        } else {
            format!("{COOKIE_NAME_SESSION}{index}")
        };
        match cookies.iter().find(|(n, _)| *n == name) {
            Some((_, value)) => token.push_str(value),
            None => break,
        }
    }
    token
}
