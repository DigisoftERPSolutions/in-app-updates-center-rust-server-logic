//! Persistent APK storage on Werevu. Binary bodies never pass through Vercel.
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, Query},
    http::{HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::PathBuf};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};
use uuid::Uuid;

const MAX_APK: usize = 250 * 1024 * 1024;
type Error = (StatusCode, Json<Value>);
fn error(status: StatusCode, message: &str) -> Error {
    (status, Json(json!({"error":message})))
}
fn internal(e: impl std::fmt::Display) -> Error {
    tracing::error!("APK library: {e}");
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "APK storage is unavailable.",
    )
}
#[cfg(not(test))]
fn root() -> PathBuf {
    std::env::var("APK_STORAGE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("data/apk-library"))
}
#[cfg(test)]
fn root() -> PathBuf {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| std::env::temp_dir().join(format!("werevu-apk-test-{}", Uuid::new_v4())))
        .clone()
}
#[cfg(not(test))]
fn secret() -> Result<String, Error> {
    std::env::var("JWT_SECRET").map_err(internal)
}
#[cfg(test)]
fn secret() -> Result<String, Error> {
    Ok(std::env::var("JWT_SECRET").unwrap_or_else(|_| "change_me".into()))
}
fn private(value: Value) -> Response {
    ([("cache-control", "private, no-store")], Json(value)).into_response()
}

pub fn routes() -> Router {
    Router::new()
        .route("/", get(list))
        .route("/tickets", post(ticket))
        .route_layer(middleware::from_fn(
            crate::authentication::guard::require_auth,
        ))
        .route(
            "/upload",
            post(upload).layer(DefaultBodyLimit::max(MAX_APK + 1024 * 1024)),
        )
        .route("/{id}/download", get(download))
}
#[derive(Debug, Serialize, Deserialize)]
struct Ticket {
    purpose: String,
    id: Uuid,
    exp: usize,
}
#[derive(Deserialize)]
struct TicketRequest {
    purpose: String,
    id: Option<Uuid>,
}
async fn ticket(Json(req): Json<TicketRequest>) -> Result<Response, Error> {
    let (id, ttl) = match req.purpose.as_str() {
        "upload" => (Uuid::new_v4(), 900),
        "download" => (
            req.id
                .ok_or_else(|| error(StatusCode::BAD_REQUEST, "Release id is required."))?,
            120,
        ),
        _ => return Err(error(StatusCode::BAD_REQUEST, "Invalid ticket purpose.")),
    };
    if req.purpose == "download" {
        metadata(id).await?;
    }
    let claims = Ticket {
        purpose: req.purpose,
        id,
        exp: (chrono::Utc::now().timestamp() + ttl) as usize,
    };
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret()?.as_bytes()),
    )
    .map_err(internal)?;
    Ok(private(json!({"token":token,"id":id})))
}
fn verify(token: &str, purpose: &str) -> Result<Ticket, Error> {
    let data = decode::<Ticket>(
        token,
        &DecodingKey::from_secret(secret()?.as_bytes()),
        &Validation::default(),
    )
    .map_err(|_| {
        error(
            StatusCode::UNAUTHORIZED,
            "Upload/download permission expired. Please retry.",
        )
    })?;
    if data.claims.purpose != purpose {
        return Err(error(StatusCode::FORBIDDEN, "Invalid ticket purpose."));
    }
    Ok(data.claims)
}
async fn metadata(id: Uuid) -> Result<Value, Error> {
    let bytes = fs::read(root().join(id.to_string()).join("release.json"))
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                error(StatusCode::NOT_FOUND, "Release not found.")
            } else {
                internal(e)
            }
        })?;
    serde_json::from_slice(&bytes).map_err(internal)
}
async fn list() -> Result<Response, Error> {
    fs::create_dir_all(root()).await.map_err(internal)?;
    let mut dirs = fs::read_dir(root()).await.map_err(internal)?;
    let mut releases = Vec::new();
    while let Some(entry) = dirs.next_entry().await.map_err(internal)? {
        if let Ok(id) = Uuid::parse_str(&entry.file_name().to_string_lossy()) {
            releases.push(metadata(id).await?);
        }
    }
    releases.sort_by(|a, b| b["createdAt"].as_str().cmp(&a["createdAt"].as_str()));
    Ok(private(json!({"data":releases})))
}
fn multipart_error(e: axum::extract::multipart::MultipartError) -> Error {
    error(
        e.status(),
        "Invalid upload or upload exceeds the 250 MB limit.",
    )
}
async fn upload(headers: HeaderMap, mut form: Multipart) -> Result<Response, Error> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let ticket = verify(token, "upload")?;
    fs::create_dir_all(root()).await.map_err(internal)?;
    let staging = root().join(format!(".upload-{}", ticket.id));
    fs::create_dir(&staging).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            error(StatusCode::CONFLICT, "This upload is already running.")
        } else {
            internal(e)
        }
    })?;
    let result = save(&mut form, &staging, ticket.id).await;
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging).await;
    }
    result.map(|release| {
        (
            StatusCode::CREATED,
            [("cache-control", "private, no-store")],
            Json(json!({"data":release})),
        )
            .into_response()
    })
}
async fn save(form: &mut Multipart, staging: &std::path::Path, id: Uuid) -> Result<Value, Error> {
    let mut fields = HashMap::new();
    let mut file_info = None;
    while let Some(mut field) = form.next_field().await.map_err(multipart_error)? {
        let name = field.name().unwrap_or("").to_string();
        if name == "apk" {
            if file_info.is_some() {
                return Err(error(StatusCode::BAD_REQUEST, "Only one APK is allowed."));
            }
            let filename = field.file_name().unwrap_or("").to_string();
            if !filename.to_lowercase().ends_with(".apk") {
                return Err(error(StatusCode::BAD_REQUEST, "Choose an .apk file."));
            }
            let filename: String = filename
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || "._-".contains(c) {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let mut file = fs::File::create(staging.join("app.apk"))
                .await
                .map_err(internal)?;
            let mut size = 0usize;
            let mut hash = Sha256::new();
            let mut signature: Vec<u8> = Vec::new();
            while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
                size += chunk.len();
                if size > MAX_APK {
                    return Err(error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "APK exceeds the 250 MB limit.",
                    ));
                }
                signature.extend(chunk.iter().take(4 - signature.len()));
                hash.update(&chunk);
                file.write_all(&chunk).await.map_err(internal)?;
            }
            if signature != b"PK\x03\x04" {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "The file is not an APK/ZIP archive.",
                ));
            }
            file.sync_all().await.map_err(internal)?;
            file_info = Some((filename, size, format!("{:x}", hash.finalize())));
        } else {
            let max = match name.as_str() {
                "name" => 100,
                "version" => 60,
                "summary" => 240,
                "notes" => 20000,
                "architecture" | "channel" => 30,
                _ => return Err(error(StatusCode::BAD_REQUEST, "Unknown upload field.")),
            };
            if fields.contains_key(&name) {
                return Err(error(StatusCode::BAD_REQUEST, "Duplicate upload field."));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
                if bytes.len() + chunk.len() > max * 4 {
                    return Err(error(StatusCode::BAD_REQUEST, "Release field is too long."));
                }
                bytes.extend_from_slice(&chunk);
            }
            let value = String::from_utf8(bytes)
                .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid release text."))?;
            let value = value.trim();
            if value.is_empty() || value.chars().count() > max {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "Release field is empty or too long.",
                ));
            }
            fields.insert(name, value.to_string());
        }
    }
    for key in [
        "name",
        "version",
        "summary",
        "notes",
        "architecture",
        "channel",
    ] {
        if !fields.contains_key(key) {
            return Err(error(StatusCode::BAD_REQUEST, "Missing release details."));
        }
    }
    if !["universal", "arm64-v8a", "armeabi-v7a", "x86_64"]
        .contains(&fields["architecture"].as_str())
        || !["stable", "beta"].contains(&fields["channel"].as_str())
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Invalid architecture or release type.",
        ));
    }
    let (filename, size, sha256) =
        file_info.ok_or_else(|| error(StatusCode::BAD_REQUEST, "Choose an APK to upload."))?;
    let mut release = serde_json::to_value(fields).map_err(internal)?;
    for (key, value) in [
        ("id", json!(id)),
        ("filename", json!(filename)),
        ("size", json!(size)),
        ("sha256", json!(sha256)),
        ("createdAt", json!(chrono::Utc::now().to_rfc3339())),
    ] {
        release[key] = value;
    }
    fs::write(
        staging.join("release.json"),
        serde_json::to_vec_pretty(&release).map_err(internal)?,
    )
    .await
    .map_err(internal)?;
    fs::rename(staging, root().join(id.to_string()))
        .await
        .map_err(|e| {
            if root().join(id.to_string()).exists() {
                error(
                    StatusCode::CONFLICT,
                    "Release already published. Refresh the library.",
                )
            } else {
                internal(e)
            }
        })?;
    Ok(release)
}
#[derive(Deserialize)]
struct DownloadQuery {
    token: String,
}
async fn download(
    Path(id): Path<Uuid>,
    Query(query): Query<DownloadQuery>,
) -> Result<Response, Error> {
    if verify(&query.token, "download")?.id != id {
        return Err(error(
            StatusCode::FORBIDDEN,
            "Ticket belongs to another release.",
        ));
    }
    let release = metadata(id).await?;
    let file = fs::File::open(root().join(id.to_string()).join("app.apk"))
        .await
        .map_err(internal)?;
    let stream = futures_util::stream::try_unfold(file, |mut file| async move {
        let mut buffer = vec![0; 64 * 1024];
        let count = file.read(&mut buffer).await?;
        if count == 0 {
            Ok::<_, std::io::Error>(None)
        } else {
            buffer.truncate(count);
            Ok(Some((buffer, file)))
        }
    });
    let filename: String = release["filename"]
        .as_str()
        .unwrap_or("app.apk")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    Response::builder()
        .header("content-type", "application/vnd.android.package-archive")
        .header(
            "content-disposition",
            format!("attachment; filename=\"{filename}\""),
        )
        .header("cache-control", "private, no-store")
        .header("referrer-policy", "no-referrer")
        .header("x-content-type-options", "nosniff")
        .body(Body::from_stream(stream))
        .map_err(internal)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn multipart(apk: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (name, value) in [
            ("name", "Test app"),
            ("version", "1"),
            ("summary", "Test upload"),
            ("notes", "Release notes"),
            ("architecture", "arm64-v8a"),
            ("channel", "beta"),
        ] {
            bytes.extend_from_slice(format!("--boundary\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
        }
        bytes.extend_from_slice(b"--boundary\r\nContent-Disposition: form-data; name=\"apk\"; filename=\"test.apk\"\r\nContent-Type: application/octet-stream\r\n\r\n");
        bytes.extend_from_slice(apk);
        bytes.extend_from_slice(b"\r\n--boundary--\r\n");
        bytes
    }
    #[tokio::test]
    async fn direct_upload_download_and_ticket_boundaries() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async { axum::serve(listener, routes()).await.unwrap() });
        let client = reqwest::Client::new();
        let auth = encode(&Header::default(), &json!({"sub":Uuid::new_v4(),"email":"test@example.com","exp":chrono::Utc::now().timestamp()+600}), &EncodingKey::from_secret(secret().unwrap().as_bytes())).unwrap();
        assert_eq!(client.get(&base).send().await.unwrap().status(), 401);
        assert_eq!(
            client
                .post(format!("{base}/tickets"))
                .json(&json!({"purpose":"upload"}))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let ticket: Value = client
            .post(format!("{base}/tickets"))
            .bearer_auth(&auth)
            .json(&json!({"purpose":"upload"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let token = ticket["token"].as_str().unwrap();
        // Larger than Vercel's 4.5 MB payload ceiling, streamed to Werevu's disk.
        let mut apk = vec![42u8; 5 * 1024 * 1024];
        apk[..4].copy_from_slice(b"PK\x03\x04");
        let response = client
            .post(format!("{base}/upload"))
            .bearer_auth(token)
            .header("content-type", "multipart/form-data; boundary=boundary")
            .body(multipart(&apk))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        let published: Value = response.json().await.unwrap();
        assert_eq!(published["data"]["size"], apk.len());
        assert_eq!(
            published["data"]["sha256"],
            format!("{:x}", Sha256::digest(&apk))
        );
        let id = published["data"]["id"].as_str().unwrap();
        let list: Value = client
            .get(&base)
            .bearer_auth(&auth)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list["data"].as_array().unwrap().len(), 1);
        // Tickets are purpose-scoped and cannot authenticate normal admin endpoints.
        assert_eq!(
            client
                .get(&base)
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            client
                .get(format!("{base}/{id}/download?token={token}"))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let download: Value = client
            .post(format!("{base}/tickets"))
            .bearer_auth(&auth)
            .json(&json!({"purpose":"download","id":id}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let download_token = download["token"].as_str().unwrap();
        assert_eq!(
            client
                .get(format!(
                    "{base}/{}/download?token={download_token}",
                    Uuid::new_v4()
                ))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let response = client
            .get(format!("{base}/{id}/download?token={download_token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        assert_eq!(response.bytes().await.unwrap().as_ref(), apk.as_slice());
        let replay = client
            .post(format!("{base}/upload"))
            .bearer_auth(token)
            .header("content-type", "multipart/form-data; boundary=boundary")
            .body(multipart(b"PK\x03\x04changed"))
            .send()
            .await
            .unwrap();
        assert_eq!(replay.status(), 409);
        // A malformed upload is rejected and its staging files are removed.
        let bad: Value = client
            .post(format!("{base}/tickets"))
            .bearer_auth(&auth)
            .json(&json!({"purpose":"upload"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let response = client
            .post(format!("{base}/upload"))
            .bearer_auth(bad["token"].as_str().unwrap())
            .header("content-type", "multipart/form-data; boundary=boundary")
            .body(multipart(b"not an apk"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert!(
            !root()
                .join(format!(".upload-{}", bad["id"].as_str().unwrap()))
                .exists()
        );
        let expired = encode(
            &Header::default(),
            &Ticket {
                purpose: "upload".into(),
                id: Uuid::new_v4(),
                exp: 1,
            },
            &EncodingKey::from_secret(secret().unwrap().as_bytes()),
        )
        .unwrap();
        assert_eq!(
            verify(&expired, "upload").unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
        server.abort();
        fs::remove_dir_all(root()).await.unwrap();
    }
}
