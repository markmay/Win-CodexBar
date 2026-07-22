//! Cursor API client for fetching usage information.
//!
//! Supports two auth paths: the desktop app's access token (Bearer auth
//! against `api2.cursor.sh`) and browser cookies (against `cursor.com`).

use crate::core::{CostSnapshot, ProviderError, RateWindow};
use chrono::{DateTime, Utc};
use serde::Deserialize;

const BASE_URL: &str = "https://cursor.com";
const API2_BASE_URL: &str = "https://api2.cursor.sh";
const CLIENT_VERSION: &str = "3.12.30";

/// Usage data assembled from a Cursor usage-summary response.
#[derive(Debug)]
pub(super) struct CursorUsage {
    pub primary: RateWindow,
    pub secondary: Option<RateWindow>,
    pub model_specific: Option<RateWindow>,
    pub cost: Option<CostSnapshot>,
    pub email: Option<String>,
    pub plan_type: Option<String>,
}

/// Cursor API client
pub struct CursorApi {
    client: reqwest::Client,
}

impl CursorApi {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    pub(super) fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Fetch usage with an already resolved Cookie header. Also fetches
    /// `/api/auth/me` (best effort) for the account email.
    pub(super) async fn fetch_usage_with_cookie_header(
        &self,
        cookie_header: &str,
    ) -> Result<CursorUsage, ProviderError> {
        let request = self
            .client
            .get(format!("{BASE_URL}/api/usage-summary"))
            .header("Cookie", cookie_header);
        let (summary, email) = tokio::join!(
            fetch_usage_summary(request),
            self.fetch_email(cookie_header)
        );
        Ok(build_usage(summary?, email))
    }

    /// Fetch usage using a Cursor access token (Bearer auth against
    /// `api2.cursor.sh`). This is the token the desktop app itself sends, read
    /// from its local state database — no browser cookies required. The email
    /// comes from the same state database, alongside the token.
    pub(super) async fn fetch_usage_with_bearer_token(
        &self,
        access_token: &str,
        email: Option<String>,
    ) -> Result<CursorUsage, ProviderError> {
        let request = self
            .client
            .get(format!("{API2_BASE_URL}/auth/usage-summary"))
            .header("Authorization", format!("Bearer {access_token}"))
            .header("x-cursor-client-version", CLIENT_VERSION);
        Ok(build_usage(fetch_usage_summary(request).await?, email))
    }

    /// Best-effort account email from `/api/auth/me` (cookie auth only).
    async fn fetch_email(&self, cookie_header: &str) -> Option<String> {
        #[derive(Deserialize)]
        struct UserInfo {
            email: Option<String>,
        }

        let response = self
            .client
            .get(format!("{BASE_URL}/api/auth/me"))
            .header("Cookie", cookie_header)
            .header("Accept", "application/json")
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        response.json::<UserInfo>().await.ok()?.email
    }
}

impl Default for CursorApi {
    fn default() -> Self {
        Self::new()
    }
}

/// Send a prepared usage-summary request (auth headers already set) and parse
/// the response. Shared by the cookie and Bearer-token paths.
async fn fetch_usage_summary(
    request: reqwest::RequestBuilder,
) -> Result<UsageSummary, ProviderError> {
    let response = request
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;

    if response.status() == 401 || response.status() == 403 {
        return Err(ProviderError::AuthRequired);
    }

    if !response.status().is_success() {
        return Err(ProviderError::Other(format!(
            "Cursor API returned {}",
            response.status()
        )));
    }

    let text = response
        .text()
        .await
        .map_err(|e| ProviderError::Parse(e.to_string()))?;
    serde_json::from_str::<UsageSummary>(&text).map_err(|e| {
        tracing::warn!(
            "Cursor usage-summary parse error: {e}; response length: {} bytes",
            text.len()
        );
        ProviderError::Parse(e.to_string())
    })
}

fn build_usage(summary: UsageSummary, email: Option<String>) -> CursorUsage {
    let billing_end = summary
        .billing_cycle_end
        .as_ref()
        .and_then(|s| parse_iso_date(s));

    let (percent_used, secondary, model_specific, cost) =
        if let Some(individual) = &summary.individual_usage {
            if let Some(plan) = &individual.plan {
                let used_cents = plan.used.unwrap_or(0) as f64;
                let limit_cents = plan
                    .limit
                    .or_else(|| plan.breakdown.as_ref().and_then(|b| b.total))
                    .unwrap_or(0) as f64;

                // Upstream #2255: clamp plan usage at 100% when included usage
                // exceeds the plan limit (overage must not paint >100% bars).
                let percent = if let Some(percent) = plan.total_percent_used {
                    clamp_percent(percent)
                } else if limit_cents > 0.0 {
                    clamp_percent((used_cents / limit_cents) * 100.0)
                } else {
                    0.0
                };

                let secondary = plan
                    .auto_percent_used
                    .map(|v| RateWindow::with_details(clamp_percent(v), None, billing_end, None));

                let model_specific = plan
                    .api_percent_used
                    .map(|v| RateWindow::with_details(clamp_percent(v), None, billing_end, None));

                let cost = on_demand_cost(individual.on_demand.as_ref(), billing_end)
                    .or_else(|| {
                        summary
                            .team_usage
                            .as_ref()
                            .and_then(|team| on_demand_cost(team.on_demand.as_ref(), billing_end))
                    })
                    .unwrap_or_else(|| {
                        // Plan-included spend (cents → USD) when on-demand is off.
                        let mut cost = CostSnapshot::new(
                            used_cents / 100.0,
                            "USD",
                            plan_period_label(summary.billing_cycle_start.as_deref()),
                        );
                        if limit_cents > 0.0 {
                            cost = cost.with_limit(limit_cents / 100.0);
                        }
                        if let Some(reset) = billing_end {
                            cost = cost.with_resets_at(reset);
                        }
                        cost
                    });

                (percent, secondary, model_specific, Some(cost))
            } else if let Some(overall) = &individual.overall {
                let percent = usage_percent(overall).unwrap_or(0.0);
                let cost = on_demand_cost(Some(overall), billing_end);
                (percent, None, None, cost)
            } else {
                (0.0, None, None, None)
            }
        } else if let Some(pooled) = summary.team_usage.as_ref().and_then(|t| t.pooled.as_ref()) {
            let percent = usage_percent(pooled).unwrap_or(0.0);
            let cost = on_demand_cost(Some(pooled), billing_end);
            (percent, None, None, cost)
        } else {
            (0.0, None, None, None)
        };

    let plan_type = summary
        .membership_type
        .as_ref()
        .map(|t| match t.to_lowercase().as_str() {
            "enterprise" => "Cursor Enterprise".to_string(),
            "pro" => "Cursor Pro".to_string(),
            "hobby" => "Cursor Hobby".to_string(),
            "team" => "Cursor Team".to_string(),
            other => format!("Cursor {}", capitalize(other)),
        });

    CursorUsage {
        primary: RateWindow::with_details(percent_used, None, billing_end, None),
        secondary,
        model_specific,
        cost,
        email,
        plan_type,
    }
}

fn on_demand_cost(
    on_demand: Option<&OnDemandUsage>,
    billing_end: Option<DateTime<Utc>>,
) -> Option<CostSnapshot> {
    let usage = on_demand?;
    if usage.enabled == Some(false) {
        return None;
    }

    let used_cents = usage.used.unwrap_or(0) as f64;
    let limit_cents = effective_limit(usage) as f64;

    if used_cents <= 0.0 && limit_cents <= 0.0 {
        return None;
    }

    // usage-summary exposes on-demand spend in cents for the billing cycle.
    // Label it explicitly so the tray/detail cost line is not a vague "Monthly".
    let mut cost = CostSnapshot::new(used_cents / 100.0, "USD", "On-demand (billing cycle)");
    if limit_cents > 0.0 {
        cost = cost.with_limit(limit_cents / 100.0);
    }
    if let Some(reset) = billing_end {
        cost = cost.with_resets_at(reset);
    }
    Some(cost)
}

fn usage_percent(usage: &OnDemandUsage) -> Option<f64> {
    let used = usage.used.unwrap_or(0) as f64;
    let limit = effective_limit(usage) as f64;
    (limit > 0.0).then_some(clamp_percent(used / limit * 100.0))
}

/// The stated limit, or `remaining + used` when only remaining is reported.
fn effective_limit(usage: &OnDemandUsage) -> i64 {
    usage
        .limit
        .or_else(|| {
            usage
                .remaining
                .map(|remaining| remaining + usage.used.unwrap_or(0))
        })
        .unwrap_or(0)
}

fn clamp_percent(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    value.clamp(0.0, 100.0)
}

/// Period label for plan-included spend from usage-summary (no new network calls).
fn plan_period_label(billing_cycle_start: Option<&str>) -> String {
    match billing_cycle_start {
        Some(start) if !start.is_empty() => format!("Plan (since {start})"),
        _ => "Plan (billing cycle)".to_string(),
    }
}

fn parse_iso_date(s: &str) -> Option<DateTime<Utc>> {
    // Try with fractional seconds
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }

    // Try without fractional seconds
    if let Ok(dt) = chrono::DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ") {
        return Some(dt.with_timezone(&Utc));
    }

    None
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().chain(chars).collect(),
    }
}

// --- API Response Types ---

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageSummary {
    billing_cycle_start: Option<String>,
    billing_cycle_end: Option<String>,
    membership_type: Option<String>,
    individual_usage: Option<IndividualUsage>,
    team_usage: Option<TeamUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndividualUsage {
    plan: Option<PlanUsage>,
    on_demand: Option<OnDemandUsage>,
    overall: Option<OnDemandUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanUsage {
    used: Option<i64>,
    limit: Option<i64>,
    breakdown: Option<PlanBreakdown>,
    auto_percent_used: Option<f64>,
    api_percent_used: Option<f64>,
    total_percent_used: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanBreakdown {
    total: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OnDemandUsage {
    enabled: Option<bool>,
    used: Option<i64>,
    limit: Option<i64>,
    remaining: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TeamUsage {
    on_demand: Option<OnDemandUsage>,
    pooled: Option<OnDemandUsage>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_from(json: &str) -> CursorUsage {
        let summary = serde_json::from_str(json).expect("fixture should parse");
        build_usage(summary, None)
    }

    #[test]
    fn test_cursor_build_result_with_lanes() {
        let usage = usage_from(
            r#"{
            "billingCycleStart": "2026-03-01T00:00:00Z",
            "billingCycleEnd": "2026-04-01T00:00:00Z",
            "membershipType": "pro",
            "individualUsage": {
                "plan": {
                    "used": 1500,
                    "limit": 5000,
                    "totalPercentUsed": 30.0,
                    "autoPercentUsed": 20.0,
                    "apiPercentUsed": 10.0
                }
            }
        }"#,
        );

        assert!((usage.primary.used_percent - 30.0).abs() < 0.01);

        let sec = usage.secondary.expect("secondary should be present");
        assert!((sec.used_percent - 20.0).abs() < 0.01);
        assert!(sec.resets_at.is_some());

        let ms = usage
            .model_specific
            .expect("model_specific should be present");
        assert!((ms.used_percent - 10.0).abs() < 0.01);
        assert!(ms.resets_at.is_some());

        assert!(usage.cost.is_some());
        assert_eq!(usage.plan_type.as_deref(), Some("Cursor Pro"));
    }

    #[test]
    fn clamps_plan_usage_percent_at_100_when_over_limit() {
        // Upstream #2255: included usage past limit must not paint >100%.
        let usage = usage_from(
            r#"{
            "membershipType": "pro",
            "individualUsage": {
                "plan": {
                    "used": 6000,
                    "limit": 5000,
                    "totalPercentUsed": 120.0,
                    "autoPercentUsed": 110.0,
                    "apiPercentUsed": 105.0
                }
            }
        }"#,
        );
        assert!((usage.primary.used_percent - 100.0).abs() < 0.01);
        assert!((usage.secondary.unwrap().used_percent - 100.0).abs() < 0.01);
        assert!((usage.model_specific.unwrap().used_percent - 100.0).abs() < 0.01);
    }

    #[test]
    fn test_cursor_build_result_prefers_api_percent_fields() {
        let usage = usage_from(
            r#"{
            "membershipType": "pro",
            "autoModelSelectedDisplayMessage": "You've used 13% of your included total usage",
            "individualUsage": {
                "plan": {
                    "used": 2000,
                    "limit": 2000,
                    "breakdown": {
                        "included": 2000,
                        "bonus": 580,
                        "total": 2580
                    },
                    "autoPercentUsed": 17.2,
                    "apiPercentUsed": 0,
                    "totalPercentUsed": 13.230769230769232
                }
            }
        }"#,
        );

        assert!((usage.primary.used_percent - 13.230769230769232).abs() < 0.01);
        assert!((usage.secondary.unwrap().used_percent - 17.2).abs() < 0.01);
        assert!((usage.model_specific.unwrap().used_percent - 0.0).abs() < 0.01);

        let cost = usage
            .cost
            .expect("plan usage should still produce cost snapshot");
        assert!((cost.used - 20.0).abs() < 0.01);
        assert_eq!(cost.limit, Some(20.0));
        assert_eq!(usage.plan_type.as_deref(), Some("Cursor Pro"));
    }

    #[test]
    fn test_cursor_build_result_cents_only() {
        let usage = usage_from(
            r#"{
            "billingCycleEnd": "2026-04-01T00:00:00Z",
            "membershipType": "pro",
            "individualUsage": {
                "plan": {
                    "used": 2500,
                    "limit": 5000
                }
            }
        }"#,
        );

        assert!((usage.primary.used_percent - 50.0).abs() < 0.01);
        assert!(usage.secondary.is_none(), "no autoPercentUsed in payload");
        assert!(
            usage.model_specific.is_none(),
            "no apiPercentUsed in payload"
        );
        assert!(usage.cost.is_some());
    }

    #[test]
    fn test_cursor_build_result_missing_plan() {
        let usage = usage_from(
            r#"{
            "membershipType": "hobby",
            "individualUsage": {}
        }"#,
        );

        assert!((usage.primary.used_percent).abs() < 0.01);
        assert!(usage.secondary.is_none());
        assert!(usage.model_specific.is_none());
        assert!(usage.cost.is_none());
    }

    #[test]
    fn test_cursor_on_demand_as_cost() {
        let usage = usage_from(
            r#"{
            "billingCycleEnd": "2026-04-01T00:00:00Z",
            "membershipType": "pro",
            "individualUsage": {
                "plan": {
                    "used": 800,
                    "limit": 5000,
                    "totalPercentUsed": 16.0
                },
                "onDemand": {
                    "enabled": true,
                    "used": 350,
                    "limit": 1000
                }
            }
        }"#,
        );

        assert!((usage.primary.used_percent - 16.0).abs() < 0.01);
        let cost = usage.cost.expect("cost should exist from on-demand usage");
        assert!((cost.used - 3.5).abs() < 0.01);
        assert_eq!(cost.limit, Some(10.0));
        assert_eq!(cost.period, "On-demand (billing cycle)");
    }

    #[test]
    fn plan_cost_period_uses_billing_cycle_start() {
        let usage = usage_from(
            r#"{
            "billingCycleStart": "2026-03-01T00:00:00Z",
            "billingCycleEnd": "2026-04-01T00:00:00Z",
            "membershipType": "pro",
            "individualUsage": {
                "plan": {
                    "used": 2500,
                    "limit": 5000
                }
            }
        }"#,
        );
        let cost = usage.cost.expect("plan cost");
        assert!((cost.used - 25.0).abs() < 0.01);
        assert_eq!(cost.limit, Some(50.0));
        assert_eq!(cost.period, "Plan (since 2026-03-01T00:00:00Z)");
    }

    #[test]
    fn test_cursor_individual_overall_fallback() {
        let usage = usage_from(r#"{"individualUsage":{"overall":{"used":2500,"limit":10000}}}"#);
        assert!((usage.primary.used_percent - 25.0).abs() < 0.01);
        assert_eq!(usage.cost.unwrap().limit, Some(100.0));
    }

    #[test]
    fn bearer_path_passes_email_through() {
        // The Bearer/token path supplies the email directly (from the local
        // state DB) rather than via /api/auth/me.
        let summary = serde_json::from_str(
            r#"{"membershipType":"pro","individualUsage":{"plan":{"used":1000,"limit":5000,"totalPercentUsed":20.0}}}"#,
        )
        .expect("fixture should parse");
        let usage = build_usage(summary, Some("person@example.com".to_string()));
        assert!((usage.primary.used_percent - 20.0).abs() < 0.01);
        assert_eq!(usage.email.as_deref(), Some("person@example.com"));
        assert_eq!(usage.plan_type.as_deref(), Some("Cursor Pro"));
    }

    #[test]
    fn test_cursor_team_pooled_fallback() {
        let usage = usage_from(r#"{"teamUsage":{"pooled":{"used":5000,"limit":10000}}}"#);
        assert!((usage.primary.used_percent - 50.0).abs() < 0.01);
        assert_eq!(usage.cost.unwrap().used, 50.0);
    }
}
