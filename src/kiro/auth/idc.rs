//! AWS SSO OIDC 设备授权登录流程
//!
//! 实现三步流程：
//! 1. 注册 OIDC 客户端（register_client）
//! 2. 发起设备授权，获取用户验证码（start_device_authorization）
//! 3. 轮询令牌端点，等待用户完成授权（poll_token）

use anyhow::Context;

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::model::token_refresh::{
    CreateTokenRequest, CreateTokenResponse, OidcErrorResponse, RegisterClientRequest,
    RegisterClientResponse, StartDeviceAuthorizationRequest, StartDeviceAuthorizationResponse,
};
use crate::model::config::Config;

/// 设备授权轮询结果
#[derive(Debug)]
pub enum PollResult {
    /// 用户尚未完成授权，继续等待
    Pending,
    /// AWS 要求降低轮询频率（RFC 8628：后续间隔至少增加 5 秒）
    SlowDown,
    /// 授权成功，返回 token
    Success(CreateTokenResponse),
    /// 设备码已过期，需重新发起
    Expired,
    /// 其他错误
    Error(anyhow::Error),
}

/// AWS Builder ID / IAM Identity Center 的默认 Start URL
pub const BUILDER_ID_START_URL: &str = "https://view.awsapps.com/start";

/// AWS 授权页面展示的设备码 OIDC 客户端名称。
/// 与 Kiro-Go-Plus 的 Builder ID 设备授权流程保持一致。
const KIRO_OIDC_CLIENT_NAME: &str = "Kiro";

/// Kiro 登录流程使用的浏览器兼容 User-Agent。
const KIRO_LOGIN_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36";

/// Kiro IDE 使用的 OIDC 作用域
const KIRO_SCOPES: &[&str] = &[
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "codewhisperer:transformations",
    "codewhisperer:taskassist",
];

/// 规范化 AWS Region，并拒绝可改变 OIDC 主机名的字符。
pub fn normalize_region(raw: &str) -> anyhow::Result<String> {
    let region = raw.trim().to_ascii_lowercase();
    let parts: Vec<&str> = region.split('-').collect();
    let valid = (3..=5).contains(&parts.len())
        && parts.first().is_some_and(|part| {
            part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_lowercase())
        })
        && parts.last().is_some_and(|part| {
            !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
        })
        && parts[1..parts.len() - 1].iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        });
    if !valid {
        anyhow::bail!("AWS Region 格式无效: {}", raw.trim());
    }
    Ok(region)
}

/// 规范化 IAM Identity Center Start URL，只允许 AWS 托管的 HTTPS `/start` 地址。
pub fn normalize_start_url(raw: &str) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(raw.trim()).context("SSO Start URL 无法解析")?;
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let trusted_host = host.ends_with(".awsapps.com") || host.ends_with(".awsapps.cn");
    if url.scheme() != "https"
        || !trusted_host
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.path().trim_end_matches('/') != "/start"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        anyhow::bail!(
            "SSO Start URL 必须是 AWS 托管的 HTTPS 地址，例如 https://d-xxxxxxxxxx.awsapps.com/start"
        );
    }
    url.set_path("/start");
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn oidc_endpoint(region: &str) -> anyhow::Result<String> {
    Ok(format!(
        "https://oidc.{}.amazonaws.com",
        normalize_region(region)?
    ))
}

/// 注册 OIDC 客户端
///
/// 每次发起设备授权前调用，获得 clientId 和 clientSecret。
/// 注册结果有过期时间（通常数天），但此处每次重新注册以保持简单。
/// `start_url` 作为 issuerUrl 一并提交：Builder ID 为默认 Start URL，
/// 企业 IAM Identity Center 为组织自己的 Start URL。
pub async fn register_client(
    region: &str,
    start_url: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<RegisterClientResponse> {
    let region = normalize_region(region)?;
    let start_url = normalize_start_url(start_url)?;
    let url = format!("{}/client/register", oidc_endpoint(&region)?);
    let client = build_client(proxy, 30, config.tls_backend)?;

    let body = RegisterClientRequest {
        client_name: KIRO_OIDC_CLIENT_NAME.to_string(),
        client_type: "public".to_string(),
        scopes: KIRO_SCOPES.iter().map(|s| s.to_string()).collect(),
        grant_types: vec![
            "urn:ietf:params:oauth:grant-type:device_code".to_string(),
            "refresh_token".to_string(),
        ],
        issuer_url: start_url,
    };

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("user-agent", KIRO_LOGIN_USER_AGENT)
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .json(&body)
        .send()
        .await
        .context("注册 OIDC 客户端请求失败")?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("注册 OIDC 客户端失败 {}: {}", status, body_text);
    }

    resp.json::<RegisterClientResponse>()
        .await
        .context("解析注册响应失败")
}

/// 发起设备授权，返回供用户访问的验证码和 URL
pub async fn start_device_authorization(
    region: &str,
    start_url: &str,
    client_id: &str,
    client_secret: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<StartDeviceAuthorizationResponse> {
    let region = normalize_region(region)?;
    let start_url = normalize_start_url(start_url)?;
    let url = format!("{}/device_authorization", oidc_endpoint(&region)?);
    let client = build_client(proxy, 30, config.tls_backend)?;

    let body = StartDeviceAuthorizationRequest {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        start_url,
    };

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("user-agent", KIRO_LOGIN_USER_AGENT)
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .json(&body)
        .send()
        .await
        .context("发起设备授权请求失败")?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("发起设备授权失败 {}: {}", status, body_text);
    }

    resp.json::<StartDeviceAuthorizationResponse>()
        .await
        .context("解析设备授权响应失败")
}

/// 轮询一次令牌端点
///
/// 返回 `PollResult`，由调用方决定是否继续轮询。
pub async fn poll_token(
    region: &str,
    client_id: &str,
    client_secret: &str,
    device_code: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> PollResult {
    let region = match normalize_region(region) {
        Ok(region) => region,
        Err(error) => return PollResult::Error(error),
    };
    let url = match oidc_endpoint(&region) {
        Ok(endpoint) => format!("{}/token", endpoint),
        Err(error) => return PollResult::Error(error),
    };
    let client = match build_client(proxy, 30, config.tls_backend) {
        Ok(c) => c,
        Err(e) => return PollResult::Error(e),
    };

    let body = CreateTokenRequest {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        grant_type: "urn:ietf:params:oauth:grant-type:device_code".to_string(),
        device_code: device_code.to_string(),
    };

    let resp = match client
        .post(&url)
        .header("content-type", "application/json")
        .header("user-agent", KIRO_LOGIN_USER_AGENT)
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return PollResult::Error(e.into()),
    };

    let status = resp.status();

    if status.is_success() {
        return match resp.json::<CreateTokenResponse>().await {
            Ok(token) => PollResult::Success(token),
            Err(e) => PollResult::Error(e.into()),
        };
    }

    let body_text = match resp.text().await {
        Ok(t) => t,
        Err(e) => return PollResult::Error(e.into()),
    };

    // 解析标准 OIDC 错误码
    if let Ok(err_resp) = serde_json::from_str::<OidcErrorResponse>(&body_text) {
        match err_resp.error.as_str() {
            "authorization_pending" => return PollResult::Pending,
            "slow_down" => return PollResult::SlowDown,
            "expired_token" => return PollResult::Expired,
            "access_denied" => return PollResult::Error(anyhow::anyhow!("用户拒绝了授权请求")),
            _ => {}
        }
    }

    PollResult::Error(anyhow::anyhow!("轮询令牌失败 {}: {}", status, body_text))
}

#[cfg(test)]
mod tests {
    use super::{
        BUILDER_ID_START_URL, KIRO_LOGIN_USER_AGENT, KIRO_OIDC_CLIENT_NAME, normalize_region,
        normalize_start_url,
    };

    #[test]
    fn login_identity_matches_kiro_on_macos_chrome() {
        assert_eq!(KIRO_OIDC_CLIENT_NAME, "Kiro");
        assert!(KIRO_LOGIN_USER_AGENT.contains("Mac OS X 10_15_7"));
        assert!(KIRO_LOGIN_USER_AGENT.contains("Chrome/151.0.0.0"));
    }

    #[test]
    fn normalize_region_accepts_aws_regions_and_trims_input() {
        assert_eq!(normalize_region(" US-EAST-1 ").unwrap(), "us-east-1");
        assert_eq!(normalize_region("us-gov-west-1").unwrap(), "us-gov-west-1");
    }

    #[test]
    fn normalize_region_rejects_host_injection() {
        for value in ["", "us-east-1.amazonaws.com", "us-east-1/evil", "../us-east-1"] {
            assert!(normalize_region(value).is_err(), "应拒绝 {value:?}");
        }
    }

    #[test]
    fn normalize_start_url_accepts_aws_portal_and_removes_trailing_slash() {
        assert_eq!(
            normalize_start_url(" https://d-1234567890.awsapps.com/start/ ").unwrap(),
            "https://d-1234567890.awsapps.com/start"
        );
        assert_eq!(
            normalize_start_url(BUILDER_ID_START_URL).unwrap(),
            BUILDER_ID_START_URL
        );
    }

    #[test]
    fn normalize_start_url_rejects_untrusted_or_ambiguous_urls() {
        for value in [
            "http://view.awsapps.com/start",
            "https://view.awsapps.com.evil.example/start",
            "https://view.awsapps.com/start?next=evil",
            "https://user@view.awsapps.com/start",
        ] {
            assert!(normalize_start_url(value).is_err(), "应拒绝 {value:?}");
        }
    }
}
