//! Cursor provider implementation
//!
//! Fetches usage data from Cursor's API, preferring the desktop app's locally
//! stored access token and falling back to browser cookies.

mod api;
mod token;
mod token_cost;

use async_trait::async_trait;

use crate::core::{
    FetchContext, Provider, ProviderError, ProviderFetchResult, ProviderId, ProviderMetadata,
    SourceMode, UsageSnapshot,
};

use api::{CursorApi, CursorUsage};
use token_cost::CursorTokenCostReport;

const COOKIE_DOMAINS: [&str; 2] = ["cursor.com", "cursor.sh"];

/// Cursor provider for fetching AI usage limits
pub struct CursorProvider {
    metadata: ProviderMetadata,
    api: CursorApi,
}

impl CursorProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                id: ProviderId::Cursor,
                display_name: "Cursor",
                session_label: "Plan",
                weekly_label: "Auto",
                supports_opus: false,
                supports_credits: true,
                default_enabled: true,
                is_primary: false,
                dashboard_url: Some("https://cursor.com/dashboard/usage"),
                status_page_url: None,
            },
            api: CursorApi::new(),
        }
    }

    /// Fetch usage using the desktop app's local access token (Bearer auth).
    /// Mirrors the Codex/Claude pattern of reading a locally stored credential
    /// instead of scraping browser cookies.
    async fn fetch_token_usage(&self) -> Result<ProviderFetchResult, ProviderError> {
        let creds = token::read_credentials()?;
        let usage = self
            .api
            .fetch_usage_with_bearer_token(&creds.access_token, creds.email)
            .await?;

        // The token-cost dashboard page is cookie-gated, so the Bearer path
        // omits it; usage/plan/cost all come from the usage-summary response.
        Ok(Self::build_fetch_result(usage, "oauth", None))
    }

    /// Fetch usage via the browser-cookie web path.
    async fn fetch_web_usage(
        &self,
        ctx: &FetchContext,
    ) -> Result<ProviderFetchResult, ProviderError> {
        let cookie_header = match ctx.manual_cookie_header.as_deref() {
            Some(header) => header.to_string(),
            None => crate::providers::browser_cookie_header(&COOKIE_DOMAINS)?,
        };

        let usage = self
            .api
            .fetch_usage_with_cookie_header(&cookie_header)
            .await
            .inspect_err(|e| tracing::warn!("Cursor API fetch failed: {e}"))?;

        // Best-effort token-cost page; never fail the main usage fetch.
        let token_report = token_cost::fetch_token_cost_report(
            self.api.client(),
            &cookie_header,
            Some(token_cost::default_since()),
            Some(chrono::Utc::now()),
        )
        .await
        .inspect_err(|err| tracing::debug!("Cursor token-cost events unavailable: {err}"))
        .ok();

        Ok(Self::build_fetch_result(
            usage,
            "web",
            token_report.as_ref(),
        ))
    }

    fn build_fetch_result(
        usage: CursorUsage,
        source: &str,
        token_report: Option<&CursorTokenCostReport>,
    ) -> ProviderFetchResult {
        let cost = token_report
            .and_then(|r| r.merge_into_cost(usage.cost.clone()))
            .or(usage.cost);

        let mut snapshot = UsageSnapshot::new(usage.primary);
        if let Some(secondary) = usage.secondary {
            snapshot = snapshot.with_secondary(secondary);
        }
        if let Some(model_specific) = usage.model_specific {
            snapshot = snapshot.with_model_specific(model_specific);
        }
        if let Some(email) = usage.email {
            snapshot = snapshot.with_email(email);
        }
        if let Some(plan) = usage.plan_type {
            snapshot = snapshot.with_login_method(plan);
        }
        if let Some(report) = token_report {
            snapshot
                .extra_rate_windows
                .extend(report.to_extra_windows());
        }

        let mut result = ProviderFetchResult::new(snapshot, source);
        if let Some(cost) = cost {
            result = result.with_cost(cost);
        }
        result
    }
}

impl Default for CursorProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for CursorProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Cursor
    }

    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    async fn fetch_usage(&self, ctx: &FetchContext) -> Result<ProviderFetchResult, ProviderError> {
        tracing::debug!("Fetching Cursor usage");

        match ctx.source_mode {
            // Read the desktop app's local access token and use Bearer auth.
            SourceMode::OAuth => self.fetch_token_usage().await,
            // Prefer the local token; fall back to browser cookies when Cursor
            // isn't installed/signed in so browser-only users still work.
            SourceMode::Auto => match self.fetch_token_usage().await {
                Ok(result) => Ok(result),
                Err(token_err) => {
                    tracing::debug!(
                        "Cursor token path unavailable ({token_err}); falling back to browser cookies"
                    );
                    self.fetch_web_usage(ctx).await
                }
            },
            // Cli is only ever set by the shell for "no cookie yet"; treat it as
            // web so empty-manual users get a browser cookie attempt (or
            // AuthRequired) instead of "Source mode 'Cli' not supported" (#212).
            SourceMode::Web | SourceMode::Cli => self.fetch_web_usage(ctx).await,
        }
    }

    fn available_sources(&self) -> Vec<SourceMode> {
        vec![SourceMode::Auto, SourceMode::OAuth, SourceMode::Web]
    }

    fn supports_web(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::FetchContext;

    #[tokio::test]
    async fn cli_mode_does_not_return_unsupported_source() {
        let provider = CursorProvider::new();
        let ctx = FetchContext {
            source_mode: SourceMode::Cli,
            manual_cookie_header: None,
            ..FetchContext::default()
        };
        let err = provider
            .fetch_usage(&ctx)
            .await
            .expect_err("no cookies on this machine");
        // Must not be UnsupportedSource — that was the user-visible #212 bug.
        assert!(
            !matches!(err, ProviderError::UnsupportedSource(_)),
            "unexpected UnsupportedSource: {err}"
        );
        assert!(
            matches!(
                err,
                ProviderError::NoCookies | ProviderError::AuthRequired | ProviderError::Other(_)
            ),
            "expected cookie/auth style error, got: {err}"
        );
    }

    #[tokio::test]
    async fn oauth_mode_reads_local_token() {
        // Point at a missing state DB so the token read fails fast (no network).
        // SAFETY: single-threaded test setup; no other thread reads this var.
        unsafe {
            std::env::set_var("CURSOR_STATE_DB", "/nonexistent/cursor-state.vscdb");
        }
        let provider = CursorProvider::new();
        let ctx = FetchContext {
            source_mode: SourceMode::OAuth,
            ..FetchContext::default()
        };
        let err = provider.fetch_usage(&ctx).await.expect_err("no cursor db");
        unsafe {
            std::env::remove_var("CURSOR_STATE_DB");
        }
        // OAuth is a supported source backed by the local token, so the error
        // must reflect a missing/unauthenticated token, not UnsupportedSource.
        assert!(
            matches!(
                err,
                ProviderError::NotInstalled(_) | ProviderError::AuthRequired
            ),
            "expected missing-token style error, got: {err}"
        );
    }
}
