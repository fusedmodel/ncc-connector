//! Web 层小工具：请求头解析、查询参数、CORS、二进制响应。

use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};

/// 取 `Authorization: Bearer <token>` 里的 token。
pub fn bearer(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let rest = v.strip_prefix("Bearer ")?;
    let t = rest.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// 客户端 IP：优先 `X-Forwarded-For` 第一段，其次 `X-Real-IP`，最后回落 `127.0.0.1`。
///
/// 反代场景下这正是要的；同时**不**拿它做安全判断（那需要可信代理列表）。
pub fn client_ip(headers: &HeaderMap) -> String {
    if let Some(v) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = v.split(',').next() {
            let first = first.trim();
            if !first.is_empty() {
                return first.to_string();
            }
        }
    }
    if let Some(v) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    "127.0.0.1".to_string()
}

/// 从查询串里取参数（已做百分号解码）。
pub fn query(uri: &Uri, key: &str) -> Option<String> {
    let q = uri.query()?;
    for pair in q.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        if k == key {
            return Some(percent_decode(v));
        }
    }
    None
}

/// 查询串里的整数，解析失败回落默认值。
pub fn query_i64(uri: &Uri, key: &str, def: i64) -> i64 {
    query(uri, key)
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(def)
}

/// 查询串里的布尔：`1/true/yes/on` 为真（与 Go 的 `mine=1` / `mine=true` 一致）。
pub fn query_bool(uri: &Uri, key: &str) -> bool {
    matches!(
        query(uri, key)
            .unwrap_or_default()
            .trim()
            .to_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// 百分号解码（`+` 还原成空格，与查询串惯例一致）。
pub fn percent_decode(s: &str) -> String {
    let replaced = s.replace('+', " ");
    percent_encoding::percent_decode_str(&replaced)
        .decode_utf8_lossy()
        .to_string()
}

/// 逗号分隔 → 去空白去空项的列表。
pub fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
        .collect()
}

/// CORS 层。
///
/// `origins` 为空或 `*` 时放开所有来源：这是内网节点的默认形态
/// （控制台与 CLI 从任意主机访问）。配了具体列表就只放行列表里的来源。
pub fn cors_layer(origins: &str) -> tower_http::cors::CorsLayer {
    use tower_http::cors::{Any, CorsLayer};
    let origins = origins.trim();
    if origins.is_empty() || origins == "*" {
        return CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);
    }
    let mut list = Vec::new();
    for o in split_csv(origins) {
        if let Ok(v) = HeaderValue::from_str(&o) {
            list.push(v);
        }
    }
    CorsLayer::new()
        .allow_origin(list)
        .allow_methods(Any)
        .allow_headers(Any)
}

/// 二进制响应（下载字节用）。
///
/// `Content-Disposition` 里的文件名只保留 ASCII 安全字符：原名可能带引号/分号/换行，
/// 直接拼进头里就是一个响应头注入点。非 ASCII 一律用 `filename*=UTF-8''` 形式给出。
pub fn bytes_response(data: Vec<u8>, content_type: &str, filename: Option<&str>) -> Response {
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    if let Some(name) = filename {
        let ascii: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "._-".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let encoded = utf8_percent_encode(name, NON_ALPHANUMERIC).to_string();
        let value = format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}");
        if let Ok(v) = HeaderValue::from_str(&value) {
            headers.insert(header::CONTENT_DISPOSITION, v);
        }
    }
    (StatusCode::OK, headers, data).into_response()
}

/// 猜测内容类型（按扩展名），未知回落 `application/octet-stream`。
pub fn guess_content_type(name: &str) -> String {
    mime_guess::from_path(name)
        .first_raw()
        .unwrap_or("application/octet-stream")
        .to_string()
}

/// 把可能不是合法 JSON 的字符串解析成 `Value`（Go 的 `parseJSONAny` 等价物）。
///
/// 库里存的是 JSON 文本，但历史行可能为空或不合法 —— 那种情况回 `null`，
/// 而不是让整个列表接口 500。
pub fn parse_json_any(s: &str) -> serde_json::Value {
    if s.trim().is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_str(s).unwrap_or(serde_json::Value::Null)
}

/// 从 `Value` 里安全取字符串字段。
pub fn str_field(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

/// 从 `Value` 里安全取整数（兼容字符串形态）。
pub fn int_field(v: &serde_json::Value, key: &str) -> Option<i64> {
    v.get(key).and_then(|x| {
        x.as_i64()
            .or_else(|| x.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
    })
}

/// 从 `Value` 里安全取布尔（兼容 `"true"` / 1）。
pub fn bool_field(v: &serde_json::Value, key: &str) -> bool {
    match v.get(key) {
        Some(x) => match x {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
            serde_json::Value::String(s) => {
                matches!(
                    s.trim().to_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            }
            _ => false,
        },
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 查询串解析() {
        let uri: Uri = "/api/registry?q=%E4%B8%AD%E6%96%87&page=2&mine=true"
            .parse()
            .unwrap();
        assert_eq!(query(&uri, "q").unwrap(), "中文");
        assert_eq!(query_i64(&uri, "page", 1), 2);
        assert!(query_bool(&uri, "mine"));
        assert!(!query_bool(&uri, "missing"));
    }

    #[test]
    fn json_字段安全取() {
        let v = serde_json::json!({"a": "x", "n": "12", "b": "true"});
        assert_eq!(str_field(&v, "a"), "x");
        assert_eq!(int_field(&v, "n"), Some(12));
        assert!(bool_field(&v, "b"));
        assert_eq!(parse_json_any("不合法"), serde_json::Value::Null);
    }
}
