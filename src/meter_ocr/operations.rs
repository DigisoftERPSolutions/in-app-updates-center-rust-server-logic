use std::sync::Arc;

use axum::{
    extract::{Multipart, State},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Deserialize;
use sqlx::PgPool;

/// Same per-company daily ceiling regardless of how many stations/devices a
/// company runs — cheap to raise later (env var) if a real customer needs
/// more, but a fixed constant is enough to turn "a bug/loop calls this in a
/// tight retry loop" into a bounded cost instead of an open-ended one. For
/// scale: at ~2 meter photos/nozzle/shift, a station with 10 nozzles running
/// 2 shifts/day is 40 *captures*, and this fallback only fires on captures
/// the free on-device OCR already failed — 300 successful fallback CALLS in
/// a day means something is actually wrong (stuck retry loop, most captures
/// failing), which is exactly when you'd want this to start returning 429s
/// rather than quietly running up the Anthropic bill further.
const DAILY_FALLBACK_CAP_PER_COMPANY: i64 = 300;

/// Meter-OCR cloud fallback — POST /meter-ocr/fallback, same public/no-login
/// trust model as GET /configure and POST /telemetry (the mobile app never
/// authenticates as a user for any device-facing endpoint on this service).
/// Unlike those two, every call here costs real money (an Anthropic API
/// call), so it's the one device endpoint that also validates the caller's
/// (company_url, company_prefix) against the `companies` directory and
/// enforces `DAILY_FALLBACK_CAP_PER_COMPANY` — see `meter_ocr_fallback` below.
pub fn meter_ocr_route(db: Arc<PgPool>) -> Router {
    Router::new()
        .route("/fallback", post(meter_ocr_fallback))
        .with_state(db)
}

/// Mirrors `MeterMode` in the app's ocrParser.ts — kept as a bare string
/// over the wire (not a typed enum) since this struct is filled from
/// multipart form fields, all of which axum hands back as `String`.
#[derive(Debug)]
struct FallbackRequest {
    company_prefix: String,
    company_url: String,
    site: String,
    device_id: String,
    expected_mode: String,
    previous_value: Option<f64>,
    image_bytes: Vec<u8>,
    image_media_type: String,
}

/// The only thing Claude is asked to do is read the two totalizer rows off
/// the photo — it does NOT combine them into one cumulative value. That
/// combination (stray-dot stripping, implied-decimal fallback, wrap-around
/// block math via `previous_value`) is real business logic that already
/// lives once, client-side, in `combineTopBottom`/`inferTopBlock`
/// (ocrParser.ts) — duplicating it here in Rust would mean two
/// implementations of the same rule drifting apart. So this endpoint hands
/// back raw digit tokens and lets the app run them through the exact same
/// parsing path a successful on-device OCR read would have used.
#[derive(Debug, Deserialize)]
struct ClaudeMeterReading {
    /// "LL" | "PP" | "unclear" — deliberately not constrained to the same
    /// two-letter set the app's OCR fuzzy-matches (11/IL/LI/L1/1L/II handle
    /// on-device misreads of the *glyphs*; asking an LLM to read a label is
    /// a different failure mode, so it just says what it saw).
    label_seen: String,
    top_token: String,
    bottom_token: String,
    #[allow(dead_code)] // logged for the cost/quality dashboard, not branched on yet
    confidence: String,
}

async fn meter_ocr_fallback(
    State(db): State<Arc<PgPool>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let req = match parse_multipart(&mut multipart).await {
        Ok(req) => req,
        Err(msg) => return bad_request(&msg),
    };

    if !matches!(req.expected_mode.as_str(), "volume" | "price") {
        return bad_request("expected_mode must be \"volume\" or \"price\"");
    }

    // Reject an unrecognized (company_url, company_prefix) pair up front —
    // same directory `get_version_uploads` already trusts for release
    // scoping, reused here purely as a "is this even one of our stations"
    // gate before spending money on an API call. NOT a security boundary
    // (both values are visible, guessable strings already sent on every
    // /configure and /telemetry call) — just enough to keep a stray script
    // hitting this URL from a browser tab from costing anything.
    let known_company: Option<(bool,)> = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM companies WHERE company_url = $1 AND company_prefix = $2)",
    )
    .bind(&req.company_url)
    .bind(&req.company_prefix)
    .fetch_optional(db.as_ref())
    .await
    .unwrap_or(None);

    if !known_company.map(|(exists,)| exists).unwrap_or(false) {
        return bad_request("unrecognized company_url/company_prefix");
    }

    let today_count: Option<(i64,)> = sqlx::query_as(
        r#"SELECT COUNT(*) FROM meter_ocr_fallback_calls
           WHERE company_prefix = $1 AND created_at > NOW() - INTERVAL '24 hours'"#,
    )
    .bind(&req.company_prefix)
    .fetch_optional(db.as_ref())
    .await
    .unwrap_or(None);

    if today_count.map(|(c,)| c).unwrap_or(0) >= DAILY_FALLBACK_CAP_PER_COMPANY {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "success": false,
                "error": "daily_cap_reached",
                "message": "This company has hit its daily meter-OCR fallback limit — contact support if this is a real spike in captures, not a stuck retry.",
            })),
        )
            .into_response();
    }

    let outcome = call_claude(&req).await;

    let (status, body, log) = match &outcome {
        Ok(reading) => {
            let mode = match reading.label_seen.as_str() {
                "LL" => Some("volume"),
                "PP" => Some("price"),
                _ => None,
            };
            match mode {
                None => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    serde_json::json!({
                        "success": false,
                        "error": "no_mode_label",
                        "message": "Couldn't find a clear PP/LL label in the photo.",
                    }),
                    FallbackLog::failed(&req, "no_mode_label"),
                ),
                Some(mode) if mode != req.expected_mode => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    serde_json::json!({
                        "success": false,
                        "error": "wrong_mode",
                        "message": format!("Photo shows the {mode} screen, expected {}.", req.expected_mode),
                    }),
                    FallbackLog::failed(&req, "wrong_mode"),
                ),
                Some(mode) => (
                    StatusCode::OK,
                    serde_json::json!({
                        "success": true,
                        "mode": mode,
                        "top_token": reading.top_token,
                        "bottom_token": reading.bottom_token,
                        "confidence": reading.confidence,
                    }),
                    FallbackLog::succeeded(&req, reading),
                ),
            }
        }
        Err(ClaudeCallError::Declined) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({
                "success": false,
                "error": "declined",
                "message": "The cloud reading was declined — enter the reading manually below.",
            }),
            FallbackLog::failed(&req, "declined"),
        ),
        Err(ClaudeCallError::NoTextDetected) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({
                "success": false,
                "error": "no_text_detected",
                "message": "Couldn't make out the display in this photo — retake with better lighting/focus.",
            }),
            FallbackLog::failed(&req, "no_text_detected"),
        ),
        Err(ClaudeCallError::Upstream(msg)) => {
            tracing::error!("[METER_OCR] Anthropic call failed: {msg}");
            (
                StatusCode::BAD_GATEWAY,
                serde_json::json!({
                    "success": false,
                    "error": "upstream_unavailable",
                    "message": "Cloud reading is temporarily unavailable — try again, or enter the reading manually.",
                }),
                FallbackLog::failed(&req, "upstream_unavailable"),
            )
        }
    };

    log.insert(&db).await;

    (status, Json(body)).into_response()
}

fn bad_request(message: &str) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "success": false, "error": "bad_request", "message": message })),
    )
        .into_response()
}

async fn parse_multipart(multipart: &mut Multipart) -> Result<FallbackRequest, String> {
    let mut company_prefix = None;
    let mut company_url = None;
    let mut site = None;
    let mut device_id = None;
    let mut expected_mode = None;
    let mut previous_value = None;
    let mut image_bytes = None;
    let mut image_media_type = "image/jpeg".to_string();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return Err(format!("malformed multipart body: {e}")),
        };

        match field.name().unwrap_or("").to_string().as_str() {
            "image" => {
                if let Some(ct) = field.content_type() {
                    image_media_type = ct.to_string();
                }
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| format!("could not read image field: {e}"))?;
                // 8MB is generous headroom over the ~1280px/70%-quality JPEG
                // the app actually sends (see MeterCaptureScreen.tsx) — this
                // is a sanity ceiling against a malformed/oversized upload,
                // not a real expected size.
                if bytes.len() > 8 * 1024 * 1024 {
                    return Err("image too large (max 8MB)".to_string());
                }
                if bytes.is_empty() {
                    return Err("image field was empty".to_string());
                }
                image_bytes = Some(bytes.to_vec());
            }
            "company_prefix" => company_prefix = Some(text_field(field).await?),
            "company_url" => company_url = Some(text_field(field).await?),
            "site" => site = Some(text_field(field).await?),
            "device_id" => device_id = Some(text_field(field).await?),
            "expected_mode" => expected_mode = Some(text_field(field).await?),
            "previous_value" => {
                let raw = text_field(field).await?;
                if !raw.trim().is_empty() {
                    previous_value = Some(
                        raw.trim()
                            .parse::<f64>()
                            .map_err(|_| "previous_value must be a number".to_string())?,
                    );
                }
            }
            _ => {} // ignore unknown fields rather than rejecting the whole request
        }
    }

    Ok(FallbackRequest {
        company_prefix: company_prefix.ok_or("missing company_prefix")?,
        company_url: company_url.ok_or("missing company_url")?,
        site: site.unwrap_or_default(),
        device_id: device_id.unwrap_or_default(),
        expected_mode: expected_mode.ok_or("missing expected_mode")?,
        previous_value,
        image_bytes: image_bytes.ok_or("missing image")?,
        image_media_type,
    })
}

async fn text_field(field: axum::extract::multipart::Field<'_>) -> Result<String, String> {
    field
        .text()
        .await
        .map_err(|e| format!("could not read form field: {e}"))
}

enum ClaudeCallError {
    /// Safety classifiers declined the request (`stop_reason: "refusal"`).
    /// Vanishingly unlikely for a pump-display photo, but Claude Opus 5 can
    /// decline any request, so this has to be a distinct branch rather than
    /// an assumed-successful response.
    Declined,
    /// A well-formed response that plainly isn't a reading — e.g. the model
    /// says so directly, or every token field comes back empty.
    NoTextDetected,
    /// Network/HTTP/parse failure talking to the Anthropic API, or a
    /// non-2xx response. Carries a short message for the server log only —
    /// never echoed to the client (see `ClaudeCallError::Upstream` handling
    /// in `meter_ocr_fallback`, which returns a fixed, generic message).
    Upstream(String),
}

/// The actual "how do the two rows become one cumulative value" combine
/// step is intentionally NOT here — see `ClaudeMeterReading`'s doc comment.
/// This function's only job is turning a photo into `{label, top, bottom}`.
async fn call_claude(req: &FallbackRequest) -> Result<ClaudeMeterReading, ClaudeCallError> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| ClaudeCallError::Upstream("ANTHROPIC_API_KEY not set".to_string()))?;

    let image_b64 = STANDARD.encode(&req.image_bytes);

    let previous_value_hint = match req.previous_value {
        Some(v) => format!(
            "The nozzle's last confirmed combined reading was approximately {v:.2}. \
             If the top reference-number row is illegible, you may still report the bottom \
             row alone and leave top_token empty — the caller can recover the top block from \
             this previous value the same way the on-device OCR path already does."
        ),
        None => "No previous reading is available for this nozzle.".to_string(),
    };

    let prompt = format!(
        "This is a photo of a fuel pump's LCD totalizer display. The display always shows a \
         two-letter mode label (LL for cumulative volume in litres, or PP for cumulative price \
         in KSh) followed by two stacked numeric rows: a short whole-number reference row on \
         top, and a longer decimal row below it that is the actual reading. Expected mode for \
         this capture: {}. {previous_value_hint}\n\n\
         Read the label and both rows exactly as displayed, digit for digit — do not combine \
         them into one number, do not guess a decimal point that isn't visibly lit on the LCD, \
         and do not correct for what a 'plausible' reading might be. If a row is genuinely not \
         legible, report an empty string for that token rather than guessing. If you cannot \
         find the label at all, set label_seen to \"unclear\".",
        req.expected_mode
    );

    let body = serde_json::json!({
        "model": "claude-opus-5",
        "max_tokens": 2048,
        // Reading two rows off a clear photo is a short, scoped, low-
        // ambiguity task — not the kind of work that benefits from this
        // model's default high-effort deliberation.
        "output_config": {
            "effort": "low",
            "format": {
                "type": "json_schema",
                "schema": {
                    "type": "object",
                    "properties": {
                        "label_seen": { "type": "string", "enum": ["LL", "PP", "unclear"] },
                        "top_token": { "type": "string" },
                        "bottom_token": { "type": "string" },
                        "confidence": { "type": "string", "enum": ["high", "medium", "low"] }
                    },
                    "required": ["label_seen", "top_token", "bottom_token", "confidence"],
                    "additionalProperties": false
                }
            }
        },
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": req.image_media_type,
                        "data": image_b64,
                    }
                },
                { "type": "text", "text": prompt }
            ]
        }]
    });

    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| ClaudeCallError::Upstream(format!("request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(ClaudeCallError::Upstream(format!("HTTP {status}: {text}")));
    }

    let parsed: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| ClaudeCallError::Upstream(format!("bad JSON response: {e}")))?;

    if parsed.get("stop_reason").and_then(|v| v.as_str()) == Some("refusal") {
        return Err(ClaudeCallError::Declined);
    }

    let text = parsed
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| blocks.iter().find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text")))
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| ClaudeCallError::Upstream("no text block in response".to_string()))?;

    let reading: ClaudeMeterReading = serde_json::from_str(text)
        .map_err(|e| ClaudeCallError::Upstream(format!("response didn't match schema: {e}")))?;

    if reading.top_token.trim().is_empty() && reading.bottom_token.trim().is_empty() {
        return Err(ClaudeCallError::NoTextDetected);
    }

    Ok(reading)
}

/// Deferred insert into `meter_ocr_fallback_calls` — built once the outcome
/// is known, written after the response is already assembled so a slow/
/// failed log write never delays or breaks the actual answer to the app.
struct FallbackLog {
    company_prefix: String,
    company_url: String,
    site: String,
    device_id: String,
    expected_mode: String,
    ok: bool,
    error: Option<String>,
    top_token: Option<String>,
    bottom_token: Option<String>,
    confidence: Option<String>,
}

impl FallbackLog {
    fn succeeded(req: &FallbackRequest, reading: &ClaudeMeterReading) -> Self {
        Self {
            company_prefix: req.company_prefix.clone(),
            company_url: req.company_url.clone(),
            site: req.site.clone(),
            device_id: req.device_id.clone(),
            expected_mode: req.expected_mode.clone(),
            ok: true,
            error: None,
            top_token: Some(reading.top_token.clone()),
            bottom_token: Some(reading.bottom_token.clone()),
            confidence: Some(reading.confidence.clone()),
        }
    }

    fn failed(req: &FallbackRequest, error: &str) -> Self {
        Self {
            company_prefix: req.company_prefix.clone(),
            company_url: req.company_url.clone(),
            site: req.site.clone(),
            device_id: req.device_id.clone(),
            expected_mode: req.expected_mode.clone(),
            ok: false,
            error: Some(error.to_string()),
            top_token: None,
            bottom_token: None,
            confidence: None,
        }
    }

    async fn insert(&self, db: &PgPool) {
        let result = sqlx::query(
            r#"
            INSERT INTO meter_ocr_fallback_calls
                (company_prefix, company_url, site, device_id, expected_mode,
                 ok, error, top_token, bottom_token, confidence, model)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
            "#,
        )
        .bind(&self.company_prefix)
        .bind(&self.company_url)
        .bind(&self.site)
        .bind(&self.device_id)
        .bind(&self.expected_mode)
        .bind(self.ok)
        .bind(&self.error)
        .bind(&self.top_token)
        .bind(&self.bottom_token)
        .bind(&self.confidence)
        .bind("claude-opus-5")
        .execute(db)
        .await;

        if let Err(e) = result {
            tracing::error!("[METER_OCR] failed to log fallback call: {e}");
        }
    }
}
