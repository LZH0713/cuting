//! Pluggable translation providers. Each is a small async function plus a pure
//! parser, so the wire formats are unit-tested without a network.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "camelCase")]
pub enum TranslatorConfig {
    #[default]
    Google,
    #[serde(rename_all = "camelCase")]
    DeepL { auth_key: String },
    #[serde(rename_all = "camelCase")]
    Baidu { app_id: String, secret: String },
    #[serde(rename_all = "camelCase")]
    OpenAi { base_url: String, api_key: String, model: String, prompt: String },
    /// Youdao's official text translation API (ai.youdao.com). Works from
    /// mainland China without a proxy.
    #[serde(rename_all = "camelCase")]
    Youdao { app_key: String, app_secret: String },
}

impl TranslatorConfig {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Google => "Google",
            Self::DeepL { .. } => "DeepL",
            Self::Baidu { .. } => "百度翻译",
            Self::OpenAi { .. } => "大模型",
            Self::Youdao { .. } => "有道翻译",
        }
    }
}

pub struct Lang {
    /// Our own stable code, used in settings.
    pub code: &'static str,
    pub zh: &'static str,
    pub en: &'static str,
    pub google: &'static str,
    pub deepl: &'static str,
    pub baidu: &'static str,
    pub youdao: &'static str,
}

pub const DEFAULT_TARGET: &str = "zh-CN";

pub const LANGS: &[Lang] = &[
    Lang { code: "zh-CN", zh: "简体中文", en: "Simplified Chinese", google: "zh-CN", deepl: "ZH", baidu: "zh", youdao: "zh-CHS" },
    Lang { code: "zh-TW", zh: "繁體中文", en: "Traditional Chinese", google: "zh-TW", deepl: "ZH-HANT", baidu: "cht", youdao: "zh-CHT" },
    Lang { code: "en", zh: "英语", en: "English", google: "en", deepl: "EN", baidu: "en", youdao: "en" },
    Lang { code: "ja", zh: "日语", en: "Japanese", google: "ja", deepl: "JA", baidu: "jp", youdao: "ja" },
    Lang { code: "ko", zh: "韩语", en: "Korean", google: "ko", deepl: "KO", baidu: "kor", youdao: "ko" },
    Lang { code: "fr", zh: "法语", en: "French", google: "fr", deepl: "FR", baidu: "fra", youdao: "fr" },
    Lang { code: "de", zh: "德语", en: "German", google: "de", deepl: "DE", baidu: "de", youdao: "de" },
    Lang { code: "es", zh: "西班牙语", en: "Spanish", google: "es", deepl: "ES", baidu: "spa", youdao: "es" },
    Lang { code: "ru", zh: "俄语", en: "Russian", google: "ru", deepl: "RU", baidu: "ru", youdao: "ru" },
    Lang { code: "pt", zh: "葡萄牙语", en: "Portuguese", google: "pt", deepl: "PT", baidu: "pt", youdao: "pt" },
    Lang { code: "it", zh: "意大利语", en: "Italian", google: "it", deepl: "IT", baidu: "it", youdao: "it" },
];

pub fn lang(code: &str) -> Option<&'static Lang> {
    LANGS.iter().find(|l| l.code == code).or_else(|| LANGS.first().filter(|_| code.is_empty()))
}

pub const DEFAULT_OPENAI_BASE_URL: &str = "https://open.bigmodel.cn/api/paas/v4";

/// `{lang}` is replaced with the target language's English name.
pub const DEFAULT_PROMPT: &str = "You are a professional translator. Translate the user's text into {lang}. \
The text was extracted from a screenshot by OCR: keep the original line breaks and list structure, \
and silently fix obvious OCR mistakes. Reply with the translation only — no notes, no quotes.";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Translation {
    pub text: String,
    pub source: Option<String>,
    pub provider: &'static str,
    pub elapsed_ms: u128,
}

fn net(e: reqwest::Error) -> String {
    format!("网络请求失败：{e}")
}

fn md5_hex(input: &str) -> String {
    use md5::{Digest, Md5};
    Md5::digest(input.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Baidu's signature is MD5(appid + query + salt + secret), lower-case hex.
pub fn baidu_sign(app_id: &str, query: &str, salt: &str, secret: &str) -> String {
    md5_hex(&format!("{app_id}{query}{salt}{secret}"))
}

pub fn parse_google(v: &serde_json::Value) -> Result<(String, Option<String>), String> {
    let segments = v.get(0).and_then(|s| s.as_array()).ok_or("Google 翻译返回了无法识别的内容")?;
    let mut out = String::new();
    for segment in segments {
        if let Some(text) = segment.get(0).and_then(|t| t.as_str()) {
            out.push_str(text);
        }
    }
    let source = v.get(2).and_then(|s| s.as_str()).filter(|s| !s.is_empty()).map(str::to_string);
    Ok((out, source))
}

pub fn parse_deepl(v: &serde_json::Value) -> Result<(String, Option<String>), String> {
    let first = v
        .get("translations")
        .and_then(|t| t.as_array())
        .and_then(|t| t.first())
        .ok_or("DeepL 翻译返回了无法识别的内容")?;
    let text = first.get("text").and_then(|t| t.as_str()).ok_or("DeepL 结果缺少文本")?;
    let source = first.get("detected_source_language").and_then(|s| s.as_str()).map(str::to_string);
    Ok((text.to_string(), source))
}

pub fn parse_baidu(v: &serde_json::Value) -> Result<(String, Option<String>), String> {
    if let Some(code) = v.get("error_code").and_then(|c| c.as_str()) {
        let msg = v.get("error_msg").and_then(|m| m.as_str()).unwrap_or("未知错误");
        return Err(format!("百度翻译错误 {code}：{msg}"));
    }
    let items = v.get("trans_result").and_then(|t| t.as_array()).ok_or("百度翻译返回了无法识别的内容")?;
    let text = items
        .iter()
        .map(|item| item.get("dst").and_then(|d| d.as_str()).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    let source = v.get("from").and_then(|s| s.as_str()).map(str::to_string);
    Ok((text, source))
}

/// Chat-completions content is either a string or a list of typed parts.
pub fn parse_openai(v: &serde_json::Value) -> Result<(String, Option<String>), String> {
    let message = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .ok_or("模型返回了无法识别的内容")?;
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        return Err(format!("模型接口错误：{err}"));
    }
    let content = message.get("content").ok_or("模型没有返回内容")?;
    let text = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .concat(),
        _ => String::new(),
    };
    let text = text.trim().trim_matches('"').to_string();
    let source = message.get("detected_source_language").and_then(|s| s.as_str()).map(str::to_string);
    Ok((text, source))
}

/// What Youdao's `errorCode` values mean, for the ones a user can act on.
const YOUDAO_ERRORS: &[(&str, &str)] = &[
    ("101", "缺少必填的参数"),
    ("102", "不支持的语言类型"),
    ("103", "翻译文本过长"),
    ("104", "不支持的 API 类型"),
    ("105", "不支持的签名类型"),
    ("108", "应用 ID 无效"),
    ("110", "没有可用的文本翻译服务实例，请到有道智云控制台创建"),
    ("111", "开发者账号无效"),
    ("112", "请求服务无效"),
    ("113", "查询为空"),
    ("201", "解密失败"),
    ("202", "签名检验失败，请检查应用 ID 和应用密钥"),
    ("203", "当前 IP 不在可访问 IP 列表内"),
    ("301", "词典查询失败"),
    ("302", "翻译查询失败"),
    ("303", "服务异常"),
];

/// Youdao answers `errorCode` as a string, but a number is tolerated.
pub fn parse_youdao(v: &serde_json::Value) -> Result<(String, Option<String>), String> {
    let code = v.get("errorCode").and_then(|c| match c {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    if code.as_deref() != Some("0") {
        let code = code.as_deref().unwrap_or("");
        let reason = YOUDAO_ERRORS.iter().find(|(c, _)| *c == code).map(|(_, r)| *r).unwrap_or("未知错误");
        return Err(format!("有道翻译错误 {code}：{reason}"));
    }
    let lines = v.get("translation").and_then(|t| t.as_array()).ok_or("有道翻译返回了无法识别的内容")?;
    let text = lines.iter().filter_map(|line| line.as_str()).collect::<Vec<_>>().join("\n");
    // `l` is "<from>2<to>", e.g. "en2zh-CHS".
    let source = v
        .get("l")
        .and_then(|l| l.as_str())
        .and_then(|l| l.split_once('2'))
        .map(|(from, _)| from.to_string())
        .filter(|from| !from.is_empty() && from != "auto");
    Ok((text, source))
}

/// Youdao's "input": the stand-in for the text inside the signature. Short
/// texts sign as they are; long ones as first ten characters + length + last
/// ten. Length is in characters, as in Youdao's own SDKs.
pub fn youdao_input(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= 20 {
        return text.to_string();
    }
    let head: String = chars[..10].iter().collect();
    let tail: String = chars[chars.len() - 10..].iter().collect();
    format!("{head}{}{tail}", chars.len())
}

/// Youdao API signature, type v3: SHA-256 of app key, input, salt, time and secret.
pub fn youdao_sign(app_key: &str, text: &str, salt: &str, curtime: u64, app_secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let signed = format!("{app_key}{}{salt}{curtime}{app_secret}", youdao_input(text));
    Sha256::digest(signed.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

async fn check(status: reqwest::StatusCode, body: String, provider: &str) -> Result<String, String> {
    if status.is_success() {
        return Ok(body);
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS && provider == "Google 翻译" {
        // Google rate-limits this unofficial endpoint on its own terms: it
        // comes and goes for the same client, and a different proxy does not
        // reliably change it. The HTML block page would say nothing useful.
        return Err(
            "Google 免费翻译接口拒绝了请求（HTTP 429）。\n\n\
             这是 Google 对这个免费接口的限流，时好时坏，换代理也未必有用。最省事的办法是在设置里\
             改用百度翻译、DeepL 或大模型；也可以过一会儿再试。"
                .to_string(),
        );
    }
    let hint: String = body.chars().take(200).collect();
    Err(format!("{provider} 返回 HTTP {status}：{hint}"))
}

async fn google(client: &reqwest::Client, text: &str, lang: &Lang) -> Result<(String, Option<String>), String> {
    let url = format!("https://translate.googleapis.com/translate_a/single?client=gtx&dt=t&sl=auto&tl={}", lang.google);
    let response = client.post(&url).form(&[("q", text)]).send().await.map_err(net)?;
    let status = response.status();
    let body = response.text().await.map_err(net)?;
    let body = check(status, body, "Google 翻译").await?;
    // Some variants prefix the JSON with `)]}'`; tolerate it.
    let body = body.trim_start_matches('\u{feff}').trim_start_matches(")]}'").trim_start();
    let value: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("Google 翻译响应无法解析：{e}"))?;
    parse_google(&value)
}

async fn deepl(client: &reqwest::Client, text: &str, target: &Lang, source: Option<&str>, key: &str) -> Result<(String, Option<String>), String> {
    let host = if key.ends_with(":fx") { "https://api-free.deepl.com" } else { "https://api.deepl.com" };
    let mut form = vec![("text", text.to_string()), ("target_lang", target.deepl.to_string())];
    if let Some(code) = source.and_then(lang).map(|l| l.deepl) {
        form.push(("source_lang", code.to_string()));
    }
    let response = client
        .post(format!("{host}/v2/translate"))
        .header("Authorization", format!("DeepL-Key-{key}"))
        .form(&form)
        .send()
        .await
        .map_err(net)?;
    let status = response.status();
    let body = response.text().await.map_err(net)?;
    let body = check(status, body, "DeepL").await?;
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("DeepL 响应无法解析：{e}"))?;
    parse_deepl(&value)
}

async fn baidu(client: &reqwest::Client, text: &str, target: &Lang, source: Option<&str>, app_id: &str, secret: &str) -> Result<(String, Option<String>), String> {
    let salt = crate::util::now_ms().to_string();
    let form = [
        ("q", text.to_string()),
        ("from", source.and_then(lang).map_or("auto", |l| l.baidu).to_string()),
        ("to", target.baidu.to_string()),
        ("appid", app_id.to_string()),
        ("salt", salt.clone()),
        ("sign", baidu_sign(app_id, text, &salt, secret)),
    ];
    let response = client
        .post("https://fanyi-api.baidu.com/api/trans/vip/translate")
        .form(&form)
        .send()
        .await
        .map_err(net)?;
    let status = response.status();
    let body = response.text().await.map_err(net)?;
    let body = check(status, body, "百度翻译").await?;
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("百度翻译响应无法解析：{e}"))?;
    parse_baidu(&value)
}

async fn openai(client: &reqwest::Client, text: &str, target: &Lang, base_url: &str, api_key: &str, model: &str, prompt: &str) -> Result<(String, Option<String>), String> {
    if model.trim().is_empty() {
        return Err("请先在设置中填写模型名称".to_string());
    }
    let system = if prompt.trim().is_empty() { DEFAULT_PROMPT } else { prompt }.replace("{lang}", target.en);
    let body = serde_json::json!({
        "model": model.trim(),
        "temperature": 0.2,
        "stream": false,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": text },
        ],
    });
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let response = client.post(&url).bearer_auth(api_key).json(&body).send().await.map_err(net)?;
    let status = response.status();
    let body = response.text().await.map_err(net)?;
    let body = check(status, body, "模型接口").await?;
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("模型响应无法解析：{e}"))?;
    parse_openai(&value)
}

async fn youdao(client: &reqwest::Client, text: &str, target: &Lang, app_key: &str, app_secret: &str) -> Result<(String, Option<String>), String> {
    let salt = crate::util::now_ms().to_string();
    let curtime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let form = [
        ("q", text.to_string()),
        ("from", "auto".to_string()),
        ("to", target.youdao.to_string()),
        ("appKey", app_key.to_string()),
        ("salt", salt.clone()),
        ("signType", "v3".to_string()),
        ("curtime", curtime.to_string()),
        ("sign", youdao_sign(app_key, text, &salt, curtime, app_secret)),
    ];
    let response = client.post("https://openapi.youdao.com/api").form(&form).send().await.map_err(net)?;
    let status = response.status();
    let body = response.text().await.map_err(net)?;
    let body = check(status, body, "有道翻译").await?;
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("有道翻译响应无法解析：{e}"))?;
    parse_youdao(&value)
}

/// One real request to the default provider, for the connection test in the
/// settings window. Goes through exactly the code a translation uses.
pub(crate) async fn google_probe(client: &reqwest::Client) -> Result<(String, Option<String>), String> {
    let target = lang("zh-CN").ok_or_else(|| "不支持的目标语言：zh-CN".to_string())?;
    google(client, "hello", target).await
}

/// Translate `text` into `target` (one of our language codes).
pub async fn translate(
    client: &reqwest::Client,
    config: &TranslatorConfig,
    text: &str,
    target: &str,
    source: Option<&str>,
) -> Result<Translation, String> {
    let t0 = crate::util::now_ms();
    if text.trim().is_empty() {
        return Err("没有可翻译的文字".to_string());
    }
    let lang = lang(target).ok_or_else(|| format!("不支持的目标语言：{target}"))?;
    let (translated, detected) = match config {
        TranslatorConfig::Google => google(client, text, lang).await?,
        TranslatorConfig::DeepL { auth_key } => {
            if auth_key.trim().is_empty() {
                return Err("请先在设置中填写 DeepL 密钥".to_string());
            }
            deepl(client, text, lang, source, auth_key.trim()).await?
        }
        TranslatorConfig::Baidu { app_id, secret } => {
            if app_id.trim().is_empty() || secret.trim().is_empty() {
                return Err("请先在设置中填写百度翻译的 APPID 和密钥".to_string());
            }
            baidu(client, text, lang, source, app_id.trim(), secret.trim()).await?
        }
        TranslatorConfig::OpenAi { base_url, api_key, model, prompt } => {
            if api_key.trim().is_empty() {
                return Err("请先在设置中填写 API Key".to_string());
            }
            let base = if base_url.trim().is_empty() { DEFAULT_OPENAI_BASE_URL } else { base_url.trim() };
            openai(client, text, lang, base, api_key.trim(), model, prompt).await?
        }
        TranslatorConfig::Youdao { app_key, app_secret } => {
            if app_key.trim().is_empty() || app_secret.trim().is_empty() {
                return Err("请先在设置中填写有道翻译的应用 ID 和应用密钥".to_string());
            }
            youdao(client, text, lang, app_key.trim(), app_secret.trim()).await?
        }
    };
    if translated.trim().is_empty() {
        return Err("翻译结果为空".to_string());
    }
    Ok(Translation {
        text: translated,
        source: detected,
        provider: config.name(),
        elapsed_ms: crate::util::now_ms() - t0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn google_segments_are_joined() {
        let v = json!([[["你好","hello",null,null,10],["\n","\n",null,null,10],["世界","world",null,null,10]],null,"en"]);
        assert_eq!(parse_google(&v).unwrap(), ("你好\n世界".to_string(), Some("en".to_string())));
        assert!(parse_google(&json!({})).is_err());
    }

    #[test]
    fn deepl_and_baidu_shapes() {
        let deepl = json!({"translations":[{"detected_source_language":"EN","text":"你好"}]});
        assert_eq!(parse_deepl(&deepl).unwrap(), ("你好".to_string(), Some("EN".to_string())));
        let ok = json!({"from":"en","to":"zh","trans_result":[{"src":"a","dst":"甲"},{"src":"b","dst":"乙"}]});
        assert_eq!(parse_baidu(&ok).unwrap(), ("甲\n乙".to_string(), Some("en".to_string())));
        let err = json!({"error_code":"54001","error_msg":"Invalid Sign"});
        assert!(parse_baidu(&err).unwrap_err().contains("54001"));
    }

    #[test]
    fn openai_content_shapes() {
        let s = json!({"choices":[{"message":{"content":"  \"你好\" "}}]});
        assert_eq!(parse_openai(&s).unwrap().0, "你好");
        let parts = json!({"choices":[{"message":{"content":[{"type":"text","text":"你"},{"type":"text","text":"好"}]}}]});
        assert_eq!(parse_openai(&parts).unwrap().0, "你好");
        let err = json!({"error":{"message":"bad key"}});
        assert!(parse_openai(&err).is_err());
    }

    #[test]
    fn baidu_sign_uses_documented_formula() {
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
        // Signatures differ as soon as any field changes, and are stable.
        let a = baidu_sign("1", "hi", "9", "k");
        assert_eq!(a, baidu_sign("1", "hi", "9", "k"));
        assert_ne!(a, baidu_sign("1", "hi", "9", "k2"));
        assert_eq!(a, md5_hex("1hi9k"));
    }

    /// The real HTTP path of the default provider: request, network, parsing.
    ///
    /// Needs direct access to Google. Behind a proxy or filter that Google
    /// does not trust, the endpoint answers 429 and this test fails for
    /// reasons outside the app.
    #[test]
    #[ignore = "network"]
    fn google_translates_live() {
        let translation = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(translate(
                &crate::proxy::client(&crate::proxy::ProxyConfig::System).unwrap(),
                &TranslatorConfig::Google,
                "hello world",
                "zh-CN",
                None,
            ))
            .unwrap();
        assert_eq!(translation.text, "你好世界");
        assert_eq!(translation.provider, "Google");
        assert!(translation.source.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("en")));
    }

    #[test]
    fn google_429_explains_the_way_out() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let blocked = rt
            .block_on(check(reqwest::StatusCode::TOO_MANY_REQUESTS, "<html>Sorry...</html>".into(), "Google 翻译"))
            .unwrap_err();
        assert!(blocked.contains("429") && blocked.contains("设置"), "{blocked}");
        assert!(blocked.contains("换代理也未必有用"), "must not promise that a proxy fixes it");
        assert!(!blocked.contains("<html>"), "the block page must not leak into the message");
        // Other providers keep the raw hint: their 429s are ordinary rate limits.
        let limited = rt
            .block_on(check(reqwest::StatusCode::TOO_MANY_REQUESTS, "quota exceeded".into(), "DeepL"))
            .unwrap_err();
        assert!(limited.contains("quota exceeded"), "{limited}");
    }

    #[test]
    fn youdao_signatures_follow_the_documented_shape() {
        // Up to 20 characters the text signs as it is.
        assert_eq!(youdao_input("hello"), "hello");
        assert_eq!(youdao_input(&"a".repeat(20)), "a".repeat(20));
        // Beyond that: first ten, length, last ten — counted in characters.
        assert_eq!(youdao_input(&"a".repeat(21)), format!("{}21{}", "a".repeat(10), "a".repeat(10)));
        assert_eq!(youdao_input(&"中".repeat(25)), format!("{}25{}", "中".repeat(10), "中".repeat(10)));
        // sha256("key" + "hello" + "1" + "1700000000" + "secret"), lowercase hex.
        assert_eq!(
            youdao_sign("key", "hello", "1", 1_700_000_000, "secret"),
            "8f830990abb21f5a2a4969e4978accc843241abfd89c2f8e4bbe8d5ec4eb18ca"
        );
    }

    #[test]
    fn youdao_responses_are_decoded() {
        let ok = json!({"errorCode": "0", "translation": ["你好", "世界"], "l": "en2zh-CHS"});
        assert_eq!(parse_youdao(&ok).unwrap(), ("你好\n世界".to_string(), Some("en".to_string())));
        let numeric = json!({"errorCode": 0, "translation": ["hi"], "l": "zh-CHS2en"});
        assert_eq!(parse_youdao(&numeric).unwrap(), ("hi".to_string(), Some("zh-CHS".to_string())));
        assert!(parse_youdao(&json!({"errorCode": "0", "l": "auto2zh-CHS"})).is_err(), "missing translation");
        let bad_key = parse_youdao(&json!({"errorCode": "202"})).unwrap_err();
        assert!(bad_key.contains("202") && bad_key.contains("应用密钥"), "{bad_key}");
        assert!(parse_youdao(&json!({"errorCode": "999"})).unwrap_err().contains("未知错误"));
        assert!(parse_youdao(&json!({})).is_err(), "no errorCode at all must not pass");
    }

    /// The real Youdao API with real credentials, short and long text.
    /// Set `YOUDAO_APP_KEY` and `YOUDAO_APP_SECRET`, then run with `--ignored`.
    #[test]
    #[ignore = "network: needs YOUDAO_APP_KEY / YOUDAO_APP_SECRET"]
    fn youdao_translates_live() {
        let (Ok(key), Ok(secret)) = (std::env::var("YOUDAO_APP_KEY"), std::env::var("YOUDAO_APP_SECRET")) else {
            eprintln!("skipping: no credentials in the environment");
            return;
        };
        let config = TranslatorConfig::Youdao { app_key: key, app_secret: secret };
        let client = crate::proxy::client(&crate::proxy::ProxyConfig::System).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let short = rt.block_on(translate(&client, &config, "good morning", "zh-CN", None)).unwrap();
        assert_eq!(short.text, "早上好");
        assert_eq!(short.provider, "有道翻译");
        // Longer than the 20-character signature boundary, and more than one line.
        let long = "The quick brown fox jumps over the lazy dog.\nScreenshot tools should stay out of your way.";
        let translated = rt.block_on(translate(&client, &config, long, "zh-CN", None)).unwrap();
        assert!(translated.text.contains("狐狸"), "{}", translated.text);
        assert!(translated.text.lines().count() >= 2, "{}", translated.text);
        eprintln!("long text → {}", translated.text);
        let english = rt.block_on(translate(&client, &config, "截图完成后可以直接提取文字", "en", None)).unwrap();
        assert!(english.text.to_lowercase().contains("text"), "{}", english.text);
        eprintln!("zh→en → {}", english.text);
    }

    #[test]
    fn config_round_trips_as_json() {
        let s = r#"{"provider":"openAi","baseUrl":"https://x/v1","apiKey":"k","model":"m","prompt":""}"#;
        let cfg: TranslatorConfig = serde_json::from_str(s).unwrap();
        assert_eq!(cfg.name(), "大模型");
        let round: TranslatorConfig = serde_json::from_value(serde_json::to_value(&cfg).unwrap()).unwrap();
        assert_eq!(round.name(), "大模型");
        assert_eq!(serde_json::to_string(&TranslatorConfig::Google).unwrap(), r#"{"provider":"google"}"#);
        let youdao = TranslatorConfig::Youdao { app_key: "k".into(), app_secret: "s".into() };
        assert_eq!(serde_json::to_string(&youdao).unwrap(), r#"{"provider":"youdao","appKey":"k","appSecret":"s"}"#);
        assert_eq!(youdao.name(), "有道翻译");
        assert_eq!(LANGS[0].youdao, "zh-CHS");
        assert_eq!(LANGS[1].youdao, "zh-CHT");
        assert_eq!(LANGS[2].youdao, "en");
    }
}
