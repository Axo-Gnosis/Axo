//! The JSON shapes callback values take on the wire, and the checks a runner's reply passes
//! before Rust accepts it. Errors name the owner and method (the caller's `label`) and the
//! failing check, never an author-supplied name or value: credentials and requests carry secrets.

use axo_ai::{Credential, ProviderRequest};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

/// Credential fields axo reads; an extra field spelled like one of them (any case, `_` or
/// `-`) would be ambiguous.
const CREDENTIAL_FIELDS: [&str; 7] = [
    "type", "key", "access", "refresh", "expires", "headers", "baseurl",
];

/// `{url, headers: [[name, value]...], body}`; a value is a string when it is UTF-8, else
/// `{"base64": ...}`. Repeated header names stay separate pairs, in order.
pub(crate) fn encode_request(request: ProviderRequest) -> Value {
    let ProviderRequest { url, headers, body } = request;
    let pairs: Vec<Value> = headers
        .iter()
        .map(|(name, value)| {
            let value = match std::str::from_utf8(value.as_bytes()) {
                Ok(text) => Value::String(text.to_owned()),
                Err(_) => json!({ "base64": STANDARD.encode(value.as_bytes()) }),
            };
            json!([name.as_str(), value])
        })
        .collect();
    let mut encoded = Map::new();
    encoded.insert("url".into(), Value::String(url));
    encoded.insert("headers".into(), Value::Array(pairs));
    encoded.insert("body".into(), body);
    Value::Object(encoded)
}

fn header_map(count: usize) -> Result<HeaderMap, &'static str> {
    // The largest HeaderMap has 32,768 buckets at a 75% load factor.
    if count > 24_576 {
        return Err("too many headers");
    }
    HeaderMap::try_with_capacity(count).map_err(|_| "too many headers")
}

/// Checks and rebuilds a runner's replacement request.
pub(crate) fn decode_request(value: Value, label: &str) -> Result<ProviderRequest, String> {
    let bad = |what: &str| format!("{label}: invalid request ({what})");
    let Value::Object(mut fields) = value else {
        return Err(bad("not an object"));
    };
    if fields
        .keys()
        .any(|key| !matches!(key.as_str(), "url" | "headers" | "body"))
    {
        return Err(bad("unknown field"));
    }
    let Some(Value::String(url)) = fields.remove("url") else {
        return Err(bad("url"));
    };
    check_url(&url).map_err(|what| bad(&what))?;
    let Some(Value::Array(pairs)) = fields.remove("headers") else {
        return Err(bad("headers"));
    };
    let mut headers = header_map(pairs.len()).map_err(bad)?;
    for (index, pair) in pairs.into_iter().enumerate() {
        let Value::Array(pair) = pair else {
            return Err(bad(&format!("header {index}")));
        };
        let [Value::String(name), value] =
            <[Value; 2]>::try_from(pair).map_err(|_| bad(&format!("header {index}")))?
        else {
            return Err(bad(&format!("header {index} name")));
        };
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| bad(&format!("header {index} name")))?;
        let bytes = match value {
            Value::String(text) => text.into_bytes(),
            Value::Object(object) => match object.get("base64") {
                Some(Value::String(encoded)) if object.len() == 1 => STANDARD
                    .decode(encoded)
                    .map_err(|_| bad(&format!("header {index} base64 value")))?,
                _ => return Err(bad(&format!("header {index} value"))),
            },
            _ => return Err(bad(&format!("header {index} value"))),
        };
        let value =
            HeaderValue::from_bytes(&bytes).map_err(|_| bad(&format!("header {index} value")))?;
        headers.append(name, value);
    }
    let body = fields.remove("body").ok_or_else(|| bad("body"))?;
    Ok(ProviderRequest { url, headers, body })
}

/// Checks a runner's credential before anything stores or uses it.
pub(crate) fn decode_credential(value: Value, label: &str) -> Result<Credential, String> {
    let bad = |what: &str| format!("{label}: invalid credential ({what})");
    let Value::Object(fields) = &value else {
        return Err(bad("not an object"));
    };
    let string = |key: &str, non_empty: bool| match fields.get(key) {
        Some(Value::String(text)) if !non_empty || !text.is_empty() => Ok(()),
        _ => Err(bad(key)),
    };
    match fields.get("type").and_then(Value::as_str) {
        Some("api_key") => {
            string("key", true)?;
            if fields
                .keys()
                .any(|key| !matches!(key.as_str(), "type" | "key"))
            {
                return Err(bad("unknown field"));
            }
        }
        Some("oauth") => {
            string("access", true)?;
            string("refresh", false)?;
            if fields.get("expires").and_then(Value::as_i64).is_none() {
                return Err(bad("expires"));
            }
            match fields.get("baseUrl") {
                None => {}
                Some(Value::String(url)) => {
                    check_url(url).map_err(|what| bad(&format!("baseUrl {what}")))?
                }
                Some(_) => return Err(bad("baseUrl")),
            }
            if let Some(headers) = fields.get("headers") {
                check_credential_headers(headers).map_err(bad)?;
            }
            if fields.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "type" | "access" | "refresh" | "expires" | "headers" | "baseUrl"
                ) && CREDENTIAL_FIELDS.contains(&normalize(key).as_str())
            }) {
                return Err(bad("field collides with a reserved field"));
            }
        }
        _ => return Err(bad("type")),
    }
    serde_json::from_value(value).map_err(|_| bad("shape"))
}

/// The credential `headers` object: valid, case-insensitively distinct header names with
/// valid values, since auth turns them into request headers.
fn check_credential_headers(headers: &Value) -> Result<(), &'static str> {
    let Value::Object(headers) = headers else {
        return Err("headers");
    };
    let mut seen = header_map(headers.len())?;
    for (name, value) in headers {
        let parsed = HeaderName::from_bytes(name.as_bytes()).map_err(|_| "header name")?;
        let Value::String(value) = value else {
            return Err("header value");
        };
        let value = HeaderValue::from_str(value).map_err(|_| "header value")?;
        if seen.insert(parsed, value).is_some() {
            return Err("duplicate header");
        }
    }
    Ok(())
}

fn check_url(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|_| "url".to_owned())?;
    match parsed.scheme() {
        "http" | "https" => Ok(()),
        _ => Err("url scheme".to_owned()),
    }
}

fn normalize(key: &str) -> String {
    key.chars()
        .filter(|c| !matches!(c, '_' | '-'))
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LABEL: &str = "extension lab-provider provider lab rewrite";

    #[test]
    fn requests_round_trip_duplicate_and_binary_headers() {
        let mut headers = HeaderMap::new();
        headers.append("x-dup", HeaderValue::from_static("one"));
        headers.append("x-dup", HeaderValue::from_static("two"));
        headers.append(
            "x-bin",
            HeaderValue::from_bytes(&[0xff, 0x80, b'a']).unwrap(),
        );
        headers.append("x-utf8", HeaderValue::from_bytes("é".as_bytes()).unwrap());
        let request = ProviderRequest {
            url: "https://example.test/v1".into(),
            headers,
            body: json!({ "model": "m", "stream": true }),
        };
        let wire = encode_request(request.clone());
        assert_eq!(
            wire["headers"],
            json!([
                ["x-dup", "one"],
                ["x-dup", "two"],
                ["x-bin", { "base64": "/4Bh" }],
                ["x-utf8", "é"],
            ])
        );
        // Through JSON text, as on the socket.
        let wire: Value = serde_json::from_str(&wire.to_string()).unwrap();
        let back = decode_request(wire, LABEL).unwrap();
        assert_eq!(back.url, request.url);
        assert_eq!(back.body, request.body);
        assert_eq!(back.headers, request.headers);
        let dup: Vec<_> = back.headers.get_all("x-dup").iter().collect();
        assert_eq!(dup, ["one", "two"]);
    }

    #[test]
    fn header_counts_are_bounded_before_map_construction() {
        for count in [24_576, 24_577] {
            let mut request = json!({ "url": "https://example.test", "body": null });
            request["headers"] = Value::Array(vec![json!(["x-dup", "v"]); count]);
            let mut credential =
                json!({ "type": "oauth", "access": "a", "refresh": "r", "expires": 1 });
            credential["headers"] =
                Value::Object((0..count).map(|i| (format!("x-{i}"), "v".into())).collect());
            let request = decode_request(request, LABEL);
            let credential = decode_credential(credential, LABEL);
            if count == 24_576 {
                assert_eq!(request.unwrap().headers.len(), count);
                let Credential::Oauth { headers, .. } = credential.unwrap() else {
                    panic!("not oauth");
                };
                assert_eq!(headers.len(), count);
            } else {
                assert_eq!(
                    request.unwrap_err(),
                    format!("{LABEL}: invalid request (too many headers)")
                );
                assert_eq!(
                    credential.unwrap_err(),
                    format!("{LABEL}: invalid credential (too many headers)")
                );
            }
        }
    }

    #[test]
    fn requests_reject_bad_shapes_without_leaking() {
        let secret = "sk-secret-value";
        let ok = || {
            json!({
                "url": "https://example.test/v1",
                "headers": [["authorization", format!("Bearer {secret}")]],
                "body": { "token": secret },
            })
        };
        let reject = |value: Value, what: &str| {
            let error = decode_request(value, LABEL).unwrap_err();
            assert_eq!(error, format!("{LABEL}: invalid request ({what})"));
        };
        decode_request(ok(), LABEL).unwrap();
        let with = |key: &str, value: Value| {
            let mut request = ok();
            request[key] = value;
            request
        };
        reject(json!([1]), "not an object");
        reject(with("url", json!("file:///etc/passwd")), "url scheme");
        reject(with("url", json!("ftp://example.test/")), "url scheme");
        reject(with("url", json!(format!("not a url {secret}"))), "url");
        reject(with("secret-marker", json!(secret)), "unknown field");
        reject(with("headers", json!({ "a": "b" })), "headers");
        // Any valid header passes; restrictions belong to the native transport.
        decode_request(
            with("headers", json!([["host", "x"], ["Content-Length", "1"]])),
            LABEL,
        )
        .unwrap();
        reject(
            with("headers", json!([["bad name", secret]])),
            "header 0 name",
        );
        reject(with("headers", json!([["x", secret, "z"]])), "header 0");
        reject(
            with("headers", json!([["x", format!("{secret}\n")]])),
            "header 0 value",
        );
        reject(
            with("headers", json!([["x", { "base64": "!!" }]])),
            "header 0 base64 value",
        );
        reject(
            with("headers", json!([["x", { "base64": "YQ==", "x": 1 }]])),
            "header 0 value",
        );
        reject(with("headers", json!([["x", 1]])), "header 0 value");
        let mut missing = ok();
        missing.as_object_mut().unwrap().remove("body");
        reject(missing, "body");
    }

    #[test]
    fn credentials_validate_without_leaking() {
        let secret = "rt-secret-value";
        let label = "extension lab-provider provider lab refresh";
        let oauth = || {
            json!({
                "type": "oauth",
                "access": secret,
                "refresh": secret,
                "expires": 1_900_000_000_000_i64,
                "headers": { "x-account": "a1" },
                "baseUrl": "https://api.example.test",
                "accountId": "acct",
            })
        };
        let credential = decode_credential(oauth(), label).unwrap();
        let Credential::Oauth {
            extra,
            headers,
            base_url,
            ..
        } = &credential
        else {
            panic!("not oauth");
        };
        assert_eq!(extra.get("accountId"), Some(&json!("acct")));
        assert_eq!(headers.get("x-account").map(String::as_str), Some("a1"));
        assert_eq!(base_url.as_deref(), Some("https://api.example.test"));
        decode_credential(json!({ "type": "api_key", "key": secret }), label).unwrap();

        let reject = |value: Value, what: &str| {
            let error = decode_credential(value, label).unwrap_err();
            assert_eq!(error, format!("{label}: invalid credential ({what})"));
        };
        let with = |key: &str, value: Value| {
            let mut credential = oauth();
            credential[key] = value;
            credential
        };
        reject(json!(secret), "not an object");
        reject(with("type", json!("cookie")), "type");
        reject(with("access", json!("")), "access");
        reject(with("access", json!(1)), "access");
        reject(with("refresh", json!(null)), "refresh");
        reject(with("expires", json!(1.5)), "expires");
        reject(with("expires", json!(secret)), "expires");
        reject(with("expires", json!(u64::MAX)), "expires");
        reject(
            with("baseUrl", json!("file:///tmp/x")),
            "baseUrl url scheme",
        );
        reject(with("headers", json!([secret])), "headers");
        reject(
            with("headers", json!({ "secret-marker": 1 })),
            "header value",
        );
        reject(
            with("headers", json!({ "secret-marker": format!("{secret}\n") })),
            "header value",
        );
        reject(
            with("headers", json!({ "bad name": secret })),
            "header name",
        );
        reject(
            with(
                "headers",
                json!({ "Secret-Marker": "1", "secret-marker": "2" }),
            ),
            "duplicate header",
        );
        for alias in ["base_url", "Access"] {
            reject(
                with(alias, json!(secret)),
                "field collides with a reserved field",
            );
        }
        reject(json!({ "type": "api_key", "key": "" }), "key");
        reject(
            json!({ "type": "api_key", "key": secret, "secret-marker": secret }),
            "unknown field",
        );
    }
}
