//! Shared, defensive client for the local InvokeAI HTTP API.
//!
//! All requests use one configured origin, bounded timeouts, consistent error
//! messages, and retries only for idempotent reads. Uploads and mutations are
//! deliberately never retried automatically because a timeout can happen after
//! Invoke has already committed the request.

use reqwest::blocking::{multipart, Client, Response};
use reqwest::{Method, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::{fs::File, io::{Read, Seek}, path::Path, thread, time::Duration};

const MAX_ERROR_BODY: usize = 2_000;
const MAX_GET_ATTEMPTS: usize = 3;

#[derive(Deserialize, Serialize)]
pub(crate) struct ProjectSummary {
    pub project_id: String,
    pub board_id: String,
    pub name: String,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct BoardSummary {
    pub board_id: String,
    pub board_name: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub is_inbox: bool,
}

#[derive(Clone)]
pub(crate) struct InvokeApiClient {
    client: Client,
    base_url: String,
    bearer_token: Option<String>,
}

impl InvokeApiClient {
    pub(crate) fn new(base_url: &str, bearer_token: Option<&str>) -> Result<Self, String> {
        let base_url = base_url.trim().trim_end_matches('/');
        let parsed = reqwest::Url::parse(base_url)
            .map_err(|_| "Enter a valid Invoke server address, such as http://127.0.0.1:9090.".to_owned())?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err("The Invoke server address must start with http:// or https:// and include a host.".into());
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err("Enter only the Invoke server address, without a query or fragment.".into());
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err("Put login details in the authentication fields, not in the server address.".into());
        }
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(90))
            .pool_idle_timeout(Duration::from_secs(30))
            .user_agent(concat!("Invoke-Tool-Suite/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| format!("Could not initialize the Invoke API connection: {error}"))?;
        Ok(Self {
            client,
            base_url: base_url.to_owned(),
            bearer_token: bearer_token.map(str::trim).filter(|token| !token.is_empty()).map(str::to_owned),
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}{}", self.base_url, if path.starts_with('/') { "" } else { "/" }, path)
    }

    fn request(&self, method: Method, path: &str) -> reqwest::blocking::RequestBuilder {
        self.request_url(method, self.endpoint(path))
    }

    fn request_url(&self, method: Method, url: String) -> reqwest::blocking::RequestBuilder {
        let mut request = self.client.request(method, url);
        if let Some(token) = &self.bearer_token {
            request = request.bearer_auth(token);
        }
        request
    }

    fn explain_response(&self, response: Response) -> Result<Response, String> {
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let body = response.text().unwrap_or_default();
        let excerpt: String = body.chars().take(MAX_ERROR_BODY).collect();
        let detail = serde_json::from_str::<Value>(&excerpt)
            .ok()
            .and_then(|value| value.get("detail").or_else(|| value.get("message")).map(|v| {
                v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())
            }))
            .unwrap_or_else(|| excerpt.trim().to_owned());
        let detail = if detail.is_empty() { "No details were returned.".into() } else { detail };
        let hint = match status {
            StatusCode::UNAUTHORIZED => " Check the Invoke login or saved access token.",
            StatusCode::FORBIDDEN => " Invoke denied this operation for the current account.",
            StatusCode::NOT_FOUND => " Check the Invoke server address and endpoint version.",
            StatusCode::TOO_MANY_REQUESTS => " Invoke is rate limiting requests; wait a moment and try again.",
            _ => "",
        };
        Err(format!("Invoke API returned HTTP {status}: {detail}{hint}"))
    }

    fn decode<T: DeserializeOwned>(&self, response: Response) -> Result<T, String> {
        let response = self.explain_response(response)?;
        response.json::<T>().map_err(|error| format!("Invoke returned an unreadable API response: {error}"))
    }

    /// GET requests can be safely retried. Mutating calls must use `send_json`.
    pub(crate) fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let mut last_error = String::new();
        for attempt in 0..MAX_GET_ATTEMPTS {
            match self.request(Method::GET, path).send() {
                Ok(response) if response.status().is_server_error() || response.status() == StatusCode::TOO_MANY_REQUESTS => {
                    let status = response.status();
                    let retry_after = response.headers().get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok())
                        .filter(|seconds| *seconds <= 15);
                    let body = response.text().unwrap_or_default();
                    last_error = format!("Invoke API returned HTTP {status}: {}", body.chars().take(MAX_ERROR_BODY).collect::<String>());
                    if attempt + 1 == MAX_GET_ATTEMPTS { break; }
                    thread::sleep(Duration::from_millis(retry_after.map(|n| n * 1000).unwrap_or(250 * (1 << attempt))));
                }
                Ok(response) => return self.decode(response),
                Err(error) => {
                    last_error = if error.is_timeout() {
                        "The Invoke API request timed out.".to_owned()
                    } else {
                        format!("Could not reach the Invoke API: {error}")
                    };
                    if attempt + 1 == MAX_GET_ATTEMPTS { break; }
                    thread::sleep(Duration::from_millis(250 * (1 << attempt)));
                }
            }
        }
        Err(last_error)
    }

    pub(crate) fn send_json<T: DeserializeOwned>(&self, method: Method, path: &str, body: &Value) -> Result<T, String> {
        let response = self.request(method, path).json(body).send()
            .map_err(|error| if error.is_timeout() { "The Invoke API request timed out. The server may still have completed the operation; check the destination before retrying.".to_owned() } else { format!("Could not send the Invoke API request: {error}") })?;
        self.decode(response)
    }

    pub(crate) fn upload_image(
        &self,
        file_path: &Path,
        category: &str,
        board_id: Option<&str>,
        project_id: Option<&str>,
        metadata: Option<&str>,
    ) -> Result<Value, String> {
        let mut file = File::open(file_path).map_err(|error| format!("Could not open source image: {error}"))?;
        let length = file.metadata().map_err(|error| format!("Could not inspect source image: {error}"))?.len();
        let filename = file_path.file_name().and_then(|name| name.to_str()).ok_or("The source image has an invalid filename.")?;
        let mut signature = [0u8; 64];
        let signature_length = file.read(&mut signature).map_err(|error| format!("Could not identify source image: {error}"))?;
        file.rewind().map_err(|error| format!("Could not prepare source image: {error}"))?;
        let mime = match image::guess_format(&signature[..signature_length])
            .map_err(|error| format!("Could not identify source image format: {error}"))?
        {
            image::ImageFormat::Png => "image/png",
            image::ImageFormat::Jpeg => "image/jpeg",
            image::ImageFormat::WebP => "image/webp",
            _ => return Err("Invoke image transfer supports PNG, JPEG, and WebP files.".into()),
        };
        let part = multipart::Part::reader_with_length(file, length)
            .file_name(filename.to_owned())
            .mime_str(mime)
            .map_err(|error| format!("Could not prepare the image upload: {error}"))?;
        let mut form = multipart::Form::new().part("file", part);
        if let Some(metadata) = metadata {
            form = form.text("metadata", metadata.to_owned());
        }
        let mut request = self.request(Method::POST, "/api/v1/images/upload")
            .query(&[("image_category", category), ("is_intermediate", "false")])
            .multipart(form);
        if let Some(board_id) = board_id { request = request.query(&[("board_id", board_id)]); }
        if let Some(project_id) = project_id { request = request.query(&[("project_id", project_id)]); }
        let response = request.send().map_err(|error| if error.is_timeout() {
            "The image upload timed out. Invoke may have accepted it; check the destination before retrying.".to_owned()
        } else { format!("Could not upload image to Invoke: {error}") })?;
        self.decode(response)
    }

    pub(crate) fn image_names(&self, board_id: Option<&str>) -> Result<Vec<String>, String> {
        let path = match board_id {
            Some(board_id) => format!("/api/v1/boards/{board_id}/image_names"),
            None => "/api/v1/images/names?board_id=none&categories=general&is_intermediate=false".to_owned(),
        };
        if board_id.is_some() {
            self.get(&path)
        } else {
            let response: Value = self.get(&path)?;
            response.get("image_names").and_then(Value::as_array)
                .ok_or_else(|| "Invoke returned an unexpected uncategorized image list.".to_owned())?
                .iter().map(|name| name.as_str().map(str::to_owned)
                    .ok_or_else(|| "Invoke returned an invalid image name.".to_owned()))
                .collect()
        }
    }

    pub(crate) fn model_records(&self) -> Result<Vec<Value>, String> {
        let response: Value = self.get("/api/v2/models/?order_by=name&direction=ASC")?;
        response.get("models").and_then(Value::as_array).cloned()
            .ok_or_else(|| "Invoke returned an unexpected model list.".to_owned())
    }

    pub(crate) fn update_model_record(&self, key: &str, changes: &Value) -> Result<Value, String> {
        let mut url = reqwest::Url::parse(&self.endpoint("/api/v2/models/i/"))
            .map_err(|error| format!("Could not construct model API address: {error}"))?;
        url.path_segments_mut().map_err(|_| "The Invoke server address cannot accept model paths.".to_owned())?
            .pop_if_empty().push(key);
        let response = self.request_url(Method::PATCH, url.to_string()).json(changes).send()
            .map_err(|error| format!("Could not update Invoke model details: {error}"))?;
        self.decode(response)
    }

    pub(crate) fn update_model_image(&self, key: &str, bytes: Vec<u8>) -> Result<(), String> {
        let mut url = reqwest::Url::parse(&self.endpoint("/api/v2/models/i/"))
            .map_err(|error| format!("Could not construct model image API address: {error}"))?;
        url.path_segments_mut().map_err(|_| "The Invoke server address cannot accept model paths.".to_owned())?
            .pop_if_empty().push(key).push("image");
        let part = multipart::Part::bytes(bytes)
            .file_name("preview.jpg")
            .mime_str("image/jpeg")
            .map_err(|error| format!("Could not prepare the model thumbnail upload: {error}"))?;
        let response = self.request_url(Method::PATCH, url.to_string())
            .multipart(multipart::Form::new().part("image", part))
            .send()
            .map_err(|error| if error.is_timeout() {
                "The thumbnail upload timed out. Invoke may have saved it; check the model before retrying.".to_owned()
            } else { format!("Could not upload the model thumbnail: {error}") })?;
        self.explain_response(response)?;
        Ok(())
    }

    pub(crate) fn test_connection(&self) -> Result<Value, String> {
        self.get("/api/v1/auth/status")
    }

    pub(crate) fn login(&self, email: &str, password: &str) -> Result<String, String> {
        let body = serde_json::json!({ "email": email, "password": password, "remember_me": true });
        let response: Value = self.send_json(Method::POST, "/api/v1/auth/login", &body)?;
        response.get("token").and_then(Value::as_str).map(str::to_owned)
            .ok_or_else(|| "Invoke accepted the login but did not return an access token.".to_owned())
    }

    pub(crate) fn projects(&self) -> Result<Vec<ProjectSummary>, String> {
        self.get("/api/v1/projects/")
    }

    pub(crate) fn boards(&self) -> Result<Vec<BoardSummary>, String> {
        let mut result = Vec::new();
        let mut offset = 0usize;
        loop {
            let response: Value = self.get(&format!(
                "/api/v1/boards/?all=true&limit=1000&offset={offset}&include_archived=false"
            ))?;
            let items = response.as_array()
                .or_else(|| response.get("items").and_then(Value::as_array))
                .ok_or_else(|| "Invoke returned an unexpected board list.".to_owned())?;
            for item in items {
                result.push(serde_json::from_value(item.clone())
                    .map_err(|error| format!("Invoke returned an invalid board record: {error}"))?);
            }
            if items.len() < 1000 { break; }
            offset += items.len();
        }
        Ok(result)
    }
}
