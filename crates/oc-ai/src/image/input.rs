//! `protocols/utils/media-input.ts`: shared lowering and decoding helpers of the media protocols.

use crate::error::AIError;
use crate::json::{encode, merge_records, Object};
use crate::media::{parse_data_url, MediaAsset, MediaSource};
use crate::route::media::{inline_required, invalid_request, FormData};
use base64::Engine;
use serde_json::Value;

/// Owned bytes for multipart uploads; decodes `base64` sources and rejects remote sources.
pub fn inline_bytes(route: &str, asset: &MediaAsset) -> Result<Vec<u8>, AIError> {
    match &asset.source {
        MediaSource::Bytes { data, .. } => Ok(data.clone()),
        MediaSource::Base64 { data, .. } => base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| invalid_request(format!("{route} media contains invalid base64 data"))),
        _ => Err(inline_required(route, asset)),
    }
}

/// Provider file handle when the ref belongs to this provider; refs from other providers are never forwarded.
pub fn ref_id<'a>(asset: &'a MediaAsset, provider: &str) -> Option<&'a str> {
    match &asset.source {
        MediaSource::Ref { provider: owner, id, .. } if owner == provider => Some(id),
        _ => None,
    }
}

/// Decode a provider's base64 output once into an owned `bytes` asset, sniffing the type when it is not
/// declared. `invalid` builds the protocol's malformed-output error.
pub fn decoded_asset(
    invalid: &dyn Fn(String) -> AIError,
    label: &str,
    data: &str,
    media_type: Option<&str>,
) -> Result<MediaAsset, AIError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| invalid(format!("{label} contains invalid base64 data")))?;
    Ok(MediaAsset::bytes(bytes, media_type))
}

/// One image of an OpenAI-shaped `data` array, which carries `b64_json`, a `url`, or both. `b64_json` wins; a
/// `data:` URL decodes inline with its own media type, and any other URL stays a URL asset for the caller to
/// download.
pub fn image_output(
    invalid: &dyn Fn(String) -> AIError,
    label: &str,
    b64_json: Option<&str>,
    url: Option<&str>,
    media_type: Option<&str>,
) -> Result<MediaAsset, AIError> {
    if let Some(data) = b64_json.filter(|d| !d.is_empty()) {
        return decoded_asset(invalid, label, data, media_type);
    }
    if let Some(url) = url.filter(|u| !u.is_empty()) {
        if url.starts_with("data:") {
            return parse_data_url(url).ok_or_else(|| invalid(format!("{label} has an invalid data URL")));
        }
        return Ok(MediaAsset::url(url, media_type.map(String::from)));
    }
    Err(invalid(format!("{label} has neither image data nor a URL")))
}

fn is_scalar(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_))
}

fn field_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => encode(other),
    }
}

/// How scalar arrays become multipart fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatArrays {
    KeyBrackets,
    Key,
}

/// Append multipart text fields: strings as-is, other values as JSON, or scalar arrays as one part per item
/// with `repeat`. `overlay` keys in `reserved` are dropped so `http.body` cannot replace route-owned fields.
pub fn append_fields(form: &mut FormData, fields: &Object, overlay: Option<&Object>, reserved: &[&str], repeat: Option<RepeatArrays>) {
    let overlay: Object = overlay
        .map(|o| o.iter().filter(|(k, _)| !reserved.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let merged = merge_records(&[Some(fields), Some(&overlay)]).unwrap_or_default();
    for (key, value) in merged {
        if let (Value::Array(items), Some(repeat)) = (&value, repeat)
            && items.iter().all(is_scalar)
        {
            let name = if repeat == RepeatArrays::KeyBrackets { format!("{key}[]") } else { key.clone() };
            for item in items {
                form.append(name.clone(), field_text(item));
            }
            continue;
        }
        form.append(key, field_text(&value));
    }
}
