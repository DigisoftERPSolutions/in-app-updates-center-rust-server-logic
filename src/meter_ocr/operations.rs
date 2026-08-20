use std::sync::Arc;

use axum::{
    extract::{Multipart, State},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::PgPool;

/// Fallback used only when `METER_OCR_DAILY_CAP_PER_COMPANY` is unset or
/// doesn't parse as an i64. Raised from the old fallback-era default of 300:
/// this endpoint (`/meter-ocr/read`) is now called on every meter-photo
/// capture the app takes — open AND close, every nozzle — not just the rare
/// case where on-device OCR already failed. At, say, 20 nozzles x 2
/// shifts/day x a couple of retakes each, a single busy station alone can
/// comfortably clear a few hundred calls/day, so 300 would false-trip on
/// perfectly normal usage. 2000 keeps the same purpose the old cap had (turn
/// "something is actually wrong" into a bounded cost instead of an
/// open-ended one) without being a normal-day nuisance.
const DEFAULT_DAILY_CAP_PER_COMPANY: i64 = 2000;

/// Fallback for `MAX_LITERS_DELTA_PER_SHIFT` when unset/unparseable. A
/// station's busiest single shift moving this many litres across one nozzle
/// would be extraordinary — this exists to catch "the model misread a digit
/// and the totalizer looks like it jumped by 500,000 litres", not to model
/// real fuel throughput precisely.
const DEFAULT_MAX_LITERS_DELTA_PER_SHIFT: f64 = 20000.0;

/// Fallback for `SALE_NUMBER_ROLLOVER_CEILING` when unset/unparseable. Most
/// pump displays we've seen wrap their sale/transaction counter back to 0/1
/// somewhere in the 4-5 digit range; 99999 is a safe assumption until we
/// have a per-company reason to say otherwise (see the `companies` table
/// override column added in migration 007).
const DEFAULT_SALE_NUMBER_ROLLOVER_CEILING: i64 = 99999;

/// Meter-OCR — two device-facing endpoints, same public/no-login trust model
/// as GET /configure and POST /telemetry (the mobile app never authenticates
/// as a user for any device-facing endpoint on this service):
///
/// - `POST /read` sends a meter photo and gets back a structured reading.
///   Every call costs real money (an OpenAI API call), so — like the
///   `/fallback` endpoint this replaces — it validates the caller's
///   (company_url, company_prefix) against the `companies` directory and
///   enforces a per-company daily cap before spending anything.
/// - `POST /confirm` sends the attendant's final confirmed numbers (no
///   image, no OpenAI call) so we can update the "last known reading"
///   used for next time's anomaly checks and keep a permanent audit trail.
///
/// The old single `/fallback` endpoint (occasional manual escape hatch when
/// on-device OCR failed) is gone — `/read` is now the primary read path
/// called on every capture, paired with `/confirm` recording what the
/// attendant actually went with.
pub fn meter_ocr_route(db: Arc<PgPool>) -> Router {
    Router::new()
        .route("/read", post(meter_ocr_read))
        .route("/confirm", post(meter_ocr_confirm))
        .with_state(db)
}

/// Filled from multipart form fields, all of which axum hands back as
/// `String` — kept as bare strings rather than typed enums for the same
/// reason the old `FallbackRequest` did.
#[derive(Debug)]
struct ReadRequest {
    company_prefix: String,
    company_url: String,
    site: String,
    device_id: String,
    pump_id: String,
    nozzle_id: String,
    shift_event: String, // "open" | "close"
    image_bytes: Vec<u8>,
    image_media_type: String,
}

/// What the model is asked for: the two rows exactly as displayed, nothing
/// combined or computed. Turning `sale_token`/`liters_token` into
/// `sale_number`/`liters`/`reading` and running the anomaly checks is real
/// business logic and lives in plain Rust functions below (`parse_sale_token`,
/// `parse_liters_token`, `compute_reading`, `liters_decreased`,
/// `delta_too_large`, `sale_number_regression`) — never delegated to the
/// model, so it can't drift between calls or hide behind opaque reasoning.
#[derive(Debug, Deserialize)]
struct MeterReadingTokens {
    /// Exact digits for the SALE ("LL") row, e.g. "0009" or "9". Empty
    /// string if genuinely illegible.
    sale_token: String,
    /// Exact digits + decimal point for the LITERS row, e.g. "7530.22".
    /// Empty string if genuinely illegible.
    liters_token: String,
    /// "high" | "medium" | "low"
    confidence: String,
    /// Brief free-text note on which digit(s)/row were ambiguous, else "".
    uncertain_digits: String,
}

async fn meter_ocr_read(
    State(db): State<Arc<PgPool>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let req = match parse_multipart(&mut multipart).await {
        Ok(req) => req,
        Err(msg) => return bad_request(&msg),
    };

    if !matches!(req.shift_event.as_str(), "open" | "close") {
        return bad_request("shift_event must be \"open\" or \"close\"");
    }

    // Single query does double duty: it's both the "is this even one of our
    // stations" allow-list check (same non-security-boundary purpose the
    // old /fallback endpoint's separate EXISTS query served — just enough to
    // keep a stray script hitting this URL from costing anything) AND the
    // lookup for this company's optional per-company anomaly-threshold
    // overrides (see migration 007). NULL columns mean "use the env-var/
    // built-in default", handled further down.
    let company: Option<(Option<f64>, Option<i64>)> = sqlx::query_as(
        r#"SELECT max_liters_delta_per_shift, sale_number_rollover_ceiling
           FROM companies WHERE company_url = $1 AND company_prefix = $2"#,
    )
    .bind(&req.company_url)
    .bind(&req.company_prefix)
    .fetch_optional(db.as_ref())
    .await
    .unwrap_or(None);

    let (company_max_delta, company_rollover_ceiling) = match company {
        Some(pair) => pair,
        None => {
            // Distinct error code from generic `bad_request` — the client
            // contract treats "we don't recognize this company" as its own
            // case (e.g. worth surfacing differently from a malformed
            // request), not lumped in with shift_event validation errors.
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": "unrecognized_company",
                    "message": "unrecognized company_url/company_prefix",
                })),
            )
                .into_response();
        }
    };

    let daily_cap = std::env::var("METER_OCR_DAILY_CAP_PER_COMPANY")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(DEFAULT_DAILY_CAP_PER_COMPANY);

    let today_count: Option<(i64,)> = sqlx::query_as(
        r#"SELECT COUNT(*) FROM meter_ocr_fallback_calls
           WHERE company_prefix = $1 AND created_at > NOW() - INTERVAL '24 hours'"#,
    )
    .bind(&req.company_prefix)
    .fetch_optional(db.as_ref())
    .await
    .unwrap_or(None);

    if today_count.map(|(c,)| c).unwrap_or(0) >= daily_cap {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "success": false,
                "error": "daily_cap_reached",
                "message": "This company has hit its daily meter-OCR limit — contact support if this is a real spike in captures, not a stuck retry loop.",
            })),
        )
            .into_response();
    }

    // Last CONFIRMED reading for this exact nozzle, written only by
    // /confirm — never by /read itself — so a bad model guess can never
    // poison the baseline the next anomaly check compares against.
    let last_state: Option<(Option<f64>, Option<i64>)> = sqlx::query_as(
        r#"SELECT last_liters, last_sale_number FROM pump_reading_state
           WHERE company_prefix = $1 AND company_url = $2 AND site = $3
             AND pump_id = $4 AND nozzle_id = $5"#,
    )
    .bind(&req.company_prefix)
    .bind(&req.company_url)
    .bind(&req.site)
    .bind(&req.pump_id)
    .bind(&req.nozzle_id)
    .fetch_optional(db.as_ref())
    .await
    .unwrap_or(None);
    let (last_liters, last_sale_number) = last_state.unwrap_or((None, None));

    let model = resolve_openai_model();
    let outcome = call_openai(&req, &model).await;

    let (status, body, log) = match &outcome {
        Ok(reading) => {
            let sale_number = parse_sale_token(&reading.sale_token);
            let liters = parse_liters_token(&reading.liters_token);
            let reading_value = compute_reading(liters, &reading.sale_token, &reading.liters_token);

            let mut anomalies: Vec<String> = Vec::new();

            if let (Some(l), Some(last_l)) = (liters, last_liters) {
                if liters_decreased(l, last_l) {
                    anomalies.push("liters_decreased".to_string());
                }
                let max_delta = company_max_delta
                    .or_else(|| {
                        std::env::var("MAX_LITERS_DELTA_PER_SHIFT")
                            .ok()
                            .and_then(|s| s.parse::<f64>().ok())
                    })
                    .unwrap_or(DEFAULT_MAX_LITERS_DELTA_PER_SHIFT);
                if delta_too_large(l, last_l, max_delta) {
                    anomalies.push("delta_too_large".to_string());
                }
            }

            if let (Some(sn), Some(last_sn)) = (sale_number, last_sale_number) {
                let ceiling = company_rollover_ceiling
                    .or_else(|| {
                        std::env::var("SALE_NUMBER_ROLLOVER_CEILING")
                            .ok()
                            .and_then(|s| s.parse::<i64>().ok())
                    })
                    .unwrap_or(DEFAULT_SALE_NUMBER_ROLLOVER_CEILING);
                if sale_number_regression(sn, last_sn, ceiling) {
                    anomalies.push("sale_number_regression".to_string());
                }
            }

            let needs_review = reading.confidence == "low"
                || !anomalies.is_empty()
                || sale_number.is_none()
                || liters.is_none();

            (
                StatusCode::OK,
                serde_json::json!({
                    "success": true,
                    "sale_token": reading.sale_token,
                    "liters_token": reading.liters_token,
                    "sale_number": sale_number,
                    "liters": liters,
                    "reading": reading_value,
                    "confidence": reading.confidence,
                    "uncertain_digits": reading.uncertain_digits,
                    "anomalies": anomalies,
                    "needs_review": needs_review,
                    "last_liters": last_liters,
                    "last_sale_number": last_sale_number,
                    "model": &model,
                }),
                ReadLog::succeeded(&req, reading, &model),
            )
        }
        // NOTE: these three failure arms deliberately return 200 rather than
        // a 5xx/502 — Cloudflare (and most CDNs/reverse proxies fronting
        // this host) silently REPLACES the response body for 502/503/504
        // with its own generic "error code: 502" plain-text page, even when
        // the origin already sent a well-formed JSON error. That stripped
        // every one of these `message` fields in production and made a
        // handled vision-call failure (bad model output, OpenAI hiccup,
        // etc.) indistinguishable from the app being unreachable — the
        // client's axios wrapper falls back to "Could not reach the meter
        // reading service" whenever `response.data.message` is missing,
        // which is exactly what a Cloudflare-mangled 502 body produces. The
        // client already keys off `success`/`error` in the body, not the
        // HTTP status, so 200 here loses nothing and stops the CDN from
        // eating the message. Verified live: an identical request with no
        // `image` field (a genuine 400) passes through Cloudflare with its
        // JSON body intact — only the 502 range gets swallowed.
        Err(VisionCallError::Declined) => (
            StatusCode::OK,
            serde_json::json!({
                "success": false,
                "error": "declined",
                "message": "The cloud reading was declined — enter the reading manually.",
            }),
            ReadLog::failed(&req, "declined", &model),
        ),
        Err(VisionCallError::NoTextDetected) => (
            StatusCode::OK,
            serde_json::json!({
                "success": false,
                "error": "no_text_detected",
                "message": "Couldn't make out the display in this photo — retake with better lighting/focus.",
            }),
            ReadLog::failed(&req, "no_text_detected", &model),
        ),
        Err(VisionCallError::RateLimited(msg)) => {
            tracing::error!("[METER_OCR] OpenAI call rate-limited: {msg}");
            (
                StatusCode::TOO_MANY_REQUESTS,
                serde_json::json!({
                    "success": false,
                    "error": "rate_limited",
                    "message": "The meter reading service is busy right now — wait a few seconds and try again, or enter the reading manually.",
                }),
                ReadLog::failed(&req, "rate_limited", &model),
            )
        }
        Err(VisionCallError::SchemaMismatch(msg)) | Err(VisionCallError::Upstream(msg)) => {
            tracing::error!("[METER_OCR] OpenAI call failed: {msg}");
            (
                StatusCode::OK,
                serde_json::json!({
                    "success": false,
                    "error": "upstream_error",
                    "message": "Cloud reading is temporarily unavailable — try again, or enter the reading manually.",
                }),
                ReadLog::failed(&req, "upstream_unavailable", &model),
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

async fn parse_multipart(multipart: &mut Multipart) -> Result<ReadRequest, String> {
    let mut company_prefix = None;
    let mut company_url = None;
    let mut site = None;
    let mut device_id = None;
    let mut pump_id = None;
    let mut nozzle_id = None;
    let mut shift_event = None;
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
            "pump_id" => pump_id = Some(text_field(field).await?),
            "nozzle_id" => nozzle_id = Some(text_field(field).await?),
            "shift_event" => shift_event = Some(text_field(field).await?),
            _ => {} // ignore unknown fields rather than rejecting the whole request
        }
    }

    Ok(ReadRequest {
        company_prefix: company_prefix.ok_or("missing company_prefix")?,
        company_url: company_url.ok_or("missing company_url")?,
        site: site.unwrap_or_default(),
        device_id: device_id.unwrap_or_default(),
        pump_id: pump_id.ok_or("missing pump_id")?,
        nozzle_id: nozzle_id.ok_or("missing nozzle_id")?,
        shift_event: shift_event.ok_or("missing shift_event")?,
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

// ---------------------------------------------------------------------------
// Deterministic combine + anomaly logic — plain Rust, not delegated to the
// model. Kept as small standalone functions so the rules are easy to read
// (and test) independently of the request/response plumbing around them.
// ---------------------------------------------------------------------------

/// Leading zeros are fine — integer parsing just drops them, which is
/// exactly the value we want (`"0009"` -> `9`).
fn parse_sale_token(token: &str) -> Option<i64> {
    let t = token.trim();
    if t.is_empty() {
        None
    } else {
        t.parse::<i64>().ok()
    }
}

fn parse_liters_token(token: &str) -> Option<f64> {
    let t = token.trim();
    if t.is_empty() {
        None
    } else {
        t.parse::<f64>().ok()
    }
}

/// Pastes only the LAST digit of the SALE ("LL") counter row in front of
/// the exact LITERS token, then parses that as one number — e.g. SALE
/// "00012", LITERS "4567.89" -> "2" + "4567.89" -> `24567.89`, NOT
/// `124567.89`. It's the last digit only, not the whole parsed counter:
/// the SALE register overflows past its own display width every ~10
/// units, so LITERS's own leading digit is exactly what that overflow
/// would have carried into — anything in SALE beyond the ones digit is
/// redundant with what LITERS already shows correctly on its own. Pasting
/// the whole counter (e.g. "12") would double-count that overflow and
/// produce a reading with extra leading digits.
///
/// Deliberately takes the raw `sale_token` string (not the parsed
/// `sale_number`) so it can pull the last character directly, with no
/// reformatting/precision loss on the `liters_token` side either — same
/// reasoning as before for pasting the exact displayed string rather than
/// reconstructing it from the parsed `liters` float.
///
/// `sale_number` (the full parsed counter) is untouched by this fix and
/// still used elsewhere in this file — `sale_number_regression` and the
/// audit trail/response both need the whole counter value, just not this
/// concatenation.
fn compute_reading(liters: Option<f64>, sale_token: &str, liters_token: &str) -> Option<f64> {
    liters?;
    let last_digit = sale_token.trim().chars().last().filter(char::is_ascii_digit)?;
    format!("{last_digit}{liters_token}").parse::<f64>().ok()
}

fn liters_decreased(liters: f64, last_liters: f64) -> bool {
    liters < last_liters
}

fn delta_too_large(liters: f64, last_liters: f64, max_delta: f64) -> bool {
    (liters - last_liters) > max_delta
}

/// A lower sale number than last time is either (a) a genuine misread/bad
/// data, or (b) the pump's counter plausibly wrapped back around near its
/// display ceiling (e.g. last=99998, ceiling=99999, new=5 after a wrap).
/// The two look very different in how far they drop: a real wrap drops by
/// an amount close to the whole ceiling (last_sale_number - sale_number is
/// large), while accidental bad data (wrong pump, fat-fingered entry, stale
/// cache) typically drops by a small amount relative to the ceiling. We use
/// "drop >= 90% of the ceiling" as the wrap heuristic — deliberately
/// conservative toward flagging a regression when unsure, since a false
/// positive here just makes the attendant double check (safe), while a
/// false negative would silently accept a bad read as a legitimate wrap.
fn sale_number_regression(sale_number: i64, last_sale_number: i64, rollover_ceiling: i64) -> bool {
    if sale_number >= last_sale_number {
        return false;
    }
    let drop = last_sale_number - sale_number;
    let looks_like_rollover = drop >= (rollover_ceiling * 9 / 10);
    !looks_like_rollover
}

// ---------------------------------------------------------------------------
// OpenAI call
// ---------------------------------------------------------------------------

enum VisionCallError {
    /// The model declined to produce a reading — a non-null/non-empty
    /// `message.refusal` (structured-outputs refusal) or
    /// `finish_reason: "content_filter"`. Vanishingly unlikely for a
    /// pump-display photo, but worth its own branch rather than an assumed-
    /// successful response.
    Declined,
    /// A well-formed response that plainly isn't a reading — both token
    /// fields came back empty.
    NoTextDetected,
    /// A 200 response whose JSON text didn't parse/validate into
    /// `MeterReadingTokens` — including a response cut off by
    /// `finish_reason: "length"` before it finished writing valid JSON.
    /// Kept distinct from `Upstream` so `call_openai` can retry ONLY this
    /// failure mode once, same image, per spec — a network/5xx/429 failure
    /// already gets its own retry inside `send_with_retry`, and this is a
    /// separate, outer retry layer on top of that for "the call succeeded
    /// but the payload was junk".
    SchemaMismatch(String),
    /// HTTP 429 from OpenAI, surviving `send_with_retry`'s own one retry
    /// (with a longer, rate-limit-appropriate backoff — see that
    /// function). Kept distinct from `Upstream` so the app can tell the
    /// attendant "the service is busy, try again shortly" instead of a
    /// generic failure message — a meaningfully different, and likely
    /// self-resolving, situation compared to an actual outage.
    RateLimited(String),
    /// Network/HTTP failure talking to the OpenAI API, or a non-2xx
    /// response that isn't one of the above. Carries a short message for
    /// the server log only — never echoed to the client.
    Upstream(String),
}

/// `OPENAI_MODEL` lets ops swap models via `.env` alone (no redeploy) if
/// OpenAI renames/deprecates the default — model availability under this
/// API has moved fast enough that hardcoding one string here would likely
/// be the first thing to go stale. Defaults to GPT-4o, a widely available
/// vision-capable model with structured-outputs support; swap in whatever
/// current-generation vision model you actually want to run this on.
fn resolve_openai_model() -> String {
    std::env::var("OPENAI_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "gpt-4o".to_string())
}

/// Wraps `call_openai_once` with exactly one extra retry, and ONLY for
/// `SchemaMismatch` — see that variant's doc comment. Network/5xx/429
/// retries are already handled one layer down inside `send_with_retry`, so
/// this is deliberately not a generic "retry everything" loop.
async fn call_openai(req: &ReadRequest, model: &str) -> Result<MeterReadingTokens, VisionCallError> {
    match call_openai_once(req, model).await {
        Err(VisionCallError::SchemaMismatch(_)) => call_openai_once(req, model).await,
        other => other,
    }
}

async fn call_openai_once(req: &ReadRequest, model: &str) -> Result<MeterReadingTokens, VisionCallError> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| VisionCallError::Upstream("OPENAI_API_KEY not set".to_string()))?;

    let image_b64 = STANDARD.encode(&req.image_bytes);

    let prompt = "This is a photo of a fuel pump's digital totalizer display. Reading top to \
bottom, the display shows up to three rows: SALE (labeled \"LL\" on-screen — a short \
whole-number transaction/sale counter), LITERS (below it — a decimal cumulative volume \
totalizer), and sometimes a PRICE row below that. Ignore any PRICE row completely if it's \
present in the photo — never read or report it, it isn't relevant here.\n\n\
Read the SALE row and the LITERS row carefully. Seven-segment displays like this one commonly \
cause specific digit confusions — 0/8/9, 1/7, 3/8, 5/6 — especially where a segment is dim, \
viewed at an angle, or has glare on it. Reason about each digit's individual shape before \
deciding what it is, rather than pattern-matching the row as a whole.\n\n\
Return sale_token and liters_token as the EXACT digit strings shown on the display, not as \
parsed or re-formatted numbers — preserve leading zeros in sale_token, and preserve the decimal \
point exactly where it's lit in liters_token. If a row is genuinely illegible after careful \
reasoning, return an empty string for that token rather than guessing at a plausible value.\n\n\
If there is genuine doubt about any digit, set confidence to \"low\" and use uncertain_digits \
to note which digit(s) and which row were ambiguous — do not silently guess and report high \
confidence.".to_string();

    let body = serde_json::json!({
        "model": model,
        // Bounded generously enough that structured-output JSON never gets
        // cut off mid-write; digit-reading is a short, scoped answer, so
        // this is pure headroom, not an expected spend.
        "max_completion_tokens": 4096,
        // Digit-reading has no upside from sampling variety — pin it to 0
        // rather than leave it at a default that invites read-to-read
        // variance on the same photo.
        "temperature": 0,
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "meter_reading",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": {
                        "sale_token": { "type": "string" },
                        "liters_token": { "type": "string" },
                        "confidence": { "type": "string", "enum": ["high", "medium", "low"] },
                        "uncertain_digits": { "type": "string" }
                    },
                    "required": ["sale_token", "liters_token", "confidence", "uncertain_digits"],
                    "additionalProperties": false
                }
            }
        },
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": prompt },
                {
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{};base64,{}", req.image_media_type, image_b64)
                    }
                }
            ]
        }]
    });

    // Bounded well under the app's own axios timeout so a slow upstream
    // fails HERE, with a real JSON error body the app can show, rather than
    // the client giving up first with no response at all.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(25))
        .build()
        .map_err(|e| VisionCallError::Upstream(format!("client build failed: {e}")))?;

    let parsed = send_with_retry(&client, &api_key, &body).await?;

    let choice = parsed.get("choices").and_then(|c| c.as_array()).and_then(|arr| arr.first());
    let message = choice.and_then(|c| c.get("message"));
    let finish_reason = choice.and_then(|c| c.get("finish_reason")).and_then(|v| v.as_str());

    let refused = message
        .and_then(|m| m.get("refusal"))
        .and_then(|r| r.as_str())
        .map(|r| !r.trim().is_empty())
        .unwrap_or(false);
    if refused || finish_reason == Some("content_filter") {
        return Err(VisionCallError::Declined);
    }
    if finish_reason == Some("length") {
        return Err(VisionCallError::SchemaMismatch(
            "response truncated at max_completion_tokens before finishing valid JSON".to_string(),
        ));
    }

    let text = message
        .and_then(|m| m.get("content"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| VisionCallError::SchemaMismatch("no message content in OpenAI response".to_string()))?;

    let reading: MeterReadingTokens = serde_json::from_str(text)
        .map_err(|e| VisionCallError::SchemaMismatch(format!("response didn't match schema: {e}")))?;

    if reading.sale_token.trim().is_empty() && reading.liters_token.trim().is_empty() {
        return Err(VisionCallError::NoTextDetected);
    }

    Ok(reading)
}

/// One retry, and ONLY for failures that are plausibly transient: a
/// network-level error (`Client::send` itself failing — connect/TLS/
/// timeout), a 5xx from OpenAI (server-side overload, gateway hiccup), or
/// a 429 (rate limit). A 4xx other than 429 (bad request, auth, invalid
/// image) means retrying the exact same body would just fail the exact
/// same way, so those return immediately on the first attempt.
///
/// 429 gets a longer backoff than network/5xx: the rate-limit window resets
/// per-minute, not per-request, so retrying after the same short delay used
/// for a transient network hiccup would almost certainly just fail again
/// against a window that hasn't cleared, burning the one retry this
/// function budgets for nothing. 5 seconds doesn't guarantee the window has
/// reset either, but it's a meaningfully better bet than a sub-second delay
/// against a per-minute limit.
async fn send_with_retry(
    client: &reqwest::Client,
    api_key: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, VisionCallError> {
    const DEFAULT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(600);
    const RATE_LIMIT_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);

    let mut backoff = DEFAULT_BACKOFF;
    let mut last_err = None;
    let mut last_was_rate_limited = false;

    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(backoff).await;
        }

        let sent = client
            .post("https://api.openai.com/v1/chat/completions")
            .header("authorization", format!("Bearer {api_key}"))
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await;

        let resp = match sent {
            Ok(resp) => resp,
            Err(e) => {
                last_err = Some(format!("request failed: {e}"));
                last_was_rate_limited = false;
                backoff = DEFAULT_BACKOFF;
                continue; // network/timeout — worth the one retry
            }
        };

        if resp.status().is_success() {
            return resp
                .json()
                .await
                .map_err(|e| VisionCallError::Upstream(format!("bad JSON response: {e}")));
        }

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if status.as_u16() == 429 {
            last_err = Some(format!("HTTP 429: {text}"));
            last_was_rate_limited = true;
            backoff = RATE_LIMIT_BACKOFF;
            continue; // rate limit — worth the one retry, with a longer wait
        }

        if status.is_server_error() {
            last_err = Some(format!("HTTP {status}: {text}"));
            last_was_rate_limited = false;
            backoff = DEFAULT_BACKOFF;
            continue; // 5xx — worth the one retry
        }

        // 4xx other than 429 — same body would fail the same way again,
        // don't waste the retry.
        return Err(VisionCallError::Upstream(format!("HTTP {status}: {text}")));
    }

    let message = last_err.unwrap_or_else(|| "exhausted retries".to_string());
    if last_was_rate_limited {
        Err(VisionCallError::RateLimited(message))
    } else {
        Err(VisionCallError::Upstream(message))
    }
}

/// Deferred insert into `meter_ocr_fallback_calls` — built once the outcome
/// is known, written after the response is already assembled so a slow/
/// failed log write never delays or breaks the actual answer to the app.
/// This table's job is still just cost/cap tracking (see migration 006) —
/// per-nozzle correctness auditing now lives in `pump_reading_audit`,
/// written by `/confirm`. The `top_token`/`bottom_token` columns are reused
/// as-is (not renamed, to avoid an unnecessary extra migration) to hold
/// `sale_token`/`liters_token` respectively; `expected_mode` is reused to
/// hold `shift_event` ("open"/"close").
struct ReadLog {
    company_prefix: String,
    company_url: String,
    site: String,
    device_id: String,
    shift_event: String,
    ok: bool,
    error: Option<String>,
    sale_token: Option<String>,
    liters_token: Option<String>,
    confidence: Option<String>,
    model: String,
}

impl ReadLog {
    fn succeeded(req: &ReadRequest, reading: &MeterReadingTokens, model: &str) -> Self {
        Self {
            company_prefix: req.company_prefix.clone(),
            company_url: req.company_url.clone(),
            site: req.site.clone(),
            device_id: req.device_id.clone(),
            shift_event: req.shift_event.clone(),
            ok: true,
            error: None,
            sale_token: Some(reading.sale_token.clone()),
            liters_token: Some(reading.liters_token.clone()),
            confidence: Some(reading.confidence.clone()),
            model: model.to_string(),
        }
    }

    fn failed(req: &ReadRequest, error: &str, model: &str) -> Self {
        Self {
            company_prefix: req.company_prefix.clone(),
            company_url: req.company_url.clone(),
            site: req.site.clone(),
            device_id: req.device_id.clone(),
            shift_event: req.shift_event.clone(),
            ok: false,
            error: Some(error.to_string()),
            sale_token: None,
            liters_token: None,
            confidence: None,
            model: model.to_string(),
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
        .bind(&self.shift_event)
        .bind(self.ok)
        .bind(&self.error)
        .bind(&self.sale_token)
        .bind(&self.liters_token)
        .bind(&self.confidence)
        .bind(&self.model)
        .execute(db)
        .await;

        if let Err(e) = result {
            tracing::error!("[METER_OCR] failed to log read call: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// /confirm
// ---------------------------------------------------------------------------

/// JSON body for `POST /confirm` — no image here, this just records what the
/// attendant actually went with (possibly after editing the model's read, or
/// entering everything manually with no cloud read at all, in which case
/// `raw_model_response` is `None`).
#[derive(Debug, Deserialize)]
struct ConfirmRequest {
    company_prefix: String,
    company_url: String,
    #[serde(default)]
    site: String,
    pump_id: String,
    nozzle_id: String,
    shift_event: String, // "open" | "close"
    raw_model_response: Option<serde_json::Value>,
    #[serde(default)]
    anomaly_flags: Vec<String>,
    confirmed_sale_number: Option<i64>,
    confirmed_liters: Option<f64>,
    confirmed_reading: Option<f64>,
    #[serde(default)]
    was_edited: bool,
    image_ref: Option<String>,
    attendant_id: Option<String>,
    /// ISO-8601 string from the client; parsed to `DateTime<Utc>` below, or
    /// `NOW()` is used at insert time if this is missing/unparseable.
    timestamp: Option<String>,
}

async fn meter_ocr_confirm(
    State(db): State<Arc<PgPool>>,
    Json(req): Json<ConfirmRequest>,
) -> impl IntoResponse {
    if !matches!(req.shift_event.as_str(), "open" | "close") {
        return bad_request("shift_event must be \"open\" or \"close\"");
    }

    let client_timestamp: Option<DateTime<Utc>> = req
        .timestamp
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    // `raw_model_response` is the full /read response body passed straight
    // through by the client — rather than asking the client to re-send
    // confidence/sale_token/liters_token/needs_review as separate fields
    // (which would just be a second, driftable copy of the same data), pull
    // them back out of that JSON blob here for the dedicated audit columns.
    // All default to NULL/false when there was no model call at all.
    let (confidence, sale_token, liters_token, needs_review) = match &req.raw_model_response {
        Some(v) => (
            v.get("confidence").and_then(|x| x.as_str()).map(str::to_string),
            v.get("sale_token").and_then(|x| x.as_str()).map(str::to_string),
            v.get("liters_token").and_then(|x| x.as_str()).map(str::to_string),
            v.get("needs_review").and_then(|x| x.as_bool()).unwrap_or(false),
        ),
        None => (None, None, None, false),
    };

    let inserted: Result<(i64,), sqlx::Error> = sqlx::query_as(
        r#"
        INSERT INTO pump_reading_audit
            (company_prefix, company_url, site, pump_id, nozzle_id, shift_event,
             raw_model_response, confidence, sale_token, liters_token, anomaly_flags,
             needs_review, confirmed_sale_number, confirmed_liters, confirmed_reading,
             was_edited, image_ref, attendant_id, client_timestamp)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19)
        RETURNING id
        "#,
    )
    .bind(&req.company_prefix)
    .bind(&req.company_url)
    .bind(&req.site)
    .bind(&req.pump_id)
    .bind(&req.nozzle_id)
    .bind(&req.shift_event)
    .bind(&req.raw_model_response)
    .bind(&confidence)
    .bind(&sale_token)
    .bind(&liters_token)
    .bind(&req.anomaly_flags)
    .bind(needs_review)
    .bind(req.confirmed_sale_number)
    .bind(req.confirmed_liters)
    .bind(req.confirmed_reading)
    .bind(req.was_edited)
    .bind(&req.image_ref)
    .bind(&req.attendant_id)
    .bind(client_timestamp)
    .fetch_one(db.as_ref())
    .await;

    let audit_id = match inserted {
        Ok((id,)) => id,
        Err(e) => {
            tracing::error!("[METER_OCR] confirm audit insert failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": "database_error",
                    "message": "Could not record the confirmed reading — try again.",
                })),
            )
                .into_response();
        }
    };

    // Only clobber the "last known" state with fields the attendant actually
    // confirmed — COALESCE against the existing row so a confirm submitted
    // with a null sale/liters value (shouldn't normally happen, but the
    // client could in theory send one) can't wipe out a good baseline.
    let upserted = sqlx::query(
        r#"
        INSERT INTO pump_reading_state
            (company_prefix, company_url, site, pump_id, nozzle_id,
             last_liters, last_sale_number, last_confirmed_at, updated_at)
        VALUES ($1,$2,$3,$4,$5,$6,$7,COALESCE($8, NOW()),NOW())
        ON CONFLICT (company_prefix, company_url, site, pump_id, nozzle_id)
        DO UPDATE SET
            last_liters = COALESCE(EXCLUDED.last_liters, pump_reading_state.last_liters),
            last_sale_number = COALESCE(EXCLUDED.last_sale_number, pump_reading_state.last_sale_number),
            last_confirmed_at = COALESCE(EXCLUDED.last_confirmed_at, pump_reading_state.last_confirmed_at),
            updated_at = NOW()
        "#,
    )
    .bind(&req.company_prefix)
    .bind(&req.company_url)
    .bind(&req.site)
    .bind(&req.pump_id)
    .bind(&req.nozzle_id)
    .bind(req.confirmed_liters)
    .bind(req.confirmed_sale_number)
    .bind(client_timestamp)
    .execute(db.as_ref())
    .await;

    if let Err(e) = upserted {
        // The audit row is already safely written — that's the durable
        // record — so a failure updating the "last known" cache is logged
        // but not surfaced as a failure to the attendant, who has already
        // done their job correctly.
        tracing::error!("[METER_OCR] pump_reading_state upsert failed: {e}");
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({ "success": true, "audit_id": audit_id })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sale_token_drops_leading_zeros() {
        assert_eq!(parse_sale_token("0009"), Some(9));
        assert_eq!(parse_sale_token("00012"), Some(12));
        assert_eq!(parse_sale_token("0"), Some(0));
    }

    #[test]
    fn parse_sale_token_empty_or_unparseable_is_none() {
        assert_eq!(parse_sale_token(""), None);
        assert_eq!(parse_sale_token("   "), None);
        assert_eq!(parse_sale_token("abc"), None);
    }

    #[test]
    fn parse_liters_token_parses_decimal() {
        assert_eq!(parse_liters_token("7530.22"), Some(7530.22));
        assert_eq!(parse_liters_token("0050.00"), Some(50.0));
    }

    #[test]
    fn parse_liters_token_empty_is_none() {
        assert_eq!(parse_liters_token(""), None);
    }

    #[test]
    fn compute_reading_pastes_only_the_last_digit_of_sale_token() {
        // Spec example: counter "00012", liters "4567.89" -> "24567.89",
        // NOT "124567.89" (the whole-counter bug this fix replaces).
        let liters = parse_liters_token("4567.89");
        assert_eq!(compute_reading(liters, "00012", "4567.89"), Some(24567.89));
    }

    #[test]
    fn compute_reading_multi_digit_counter_uses_last_digit_not_whole_number() {
        // The old (buggy) whole-integer paste would have produced
        // 1234567.89 here.
        let liters = parse_liters_token("4567.89");
        assert_eq!(compute_reading(liters, "123", "4567.89"), Some(34567.89));
    }

    #[test]
    fn compute_reading_single_digit_counter_is_unaffected_by_the_fix() {
        let liters = parse_liters_token("7530.22");
        assert_eq!(compute_reading(liters, "0009", "7530.22"), Some(97530.22));
        assert_eq!(compute_reading(liters, "9", "7530.22"), Some(97530.22));
    }

    #[test]
    fn compute_reading_zero_counter() {
        let liters = parse_liters_token("1234.56");
        assert_eq!(compute_reading(liters, "0", "1234.56"), Some(1234.56));
    }

    #[test]
    fn compute_reading_none_when_liters_missing() {
        assert_eq!(compute_reading(None, "9", ""), None);
    }

    #[test]
    fn compute_reading_none_when_sale_token_empty_or_unparseable() {
        let liters = parse_liters_token("6252.95");
        assert_eq!(compute_reading(liters, "", "6252.95"), None);
        assert_eq!(compute_reading(liters, "abc", "6252.95"), None);
    }
}
