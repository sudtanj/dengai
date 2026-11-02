//! Readable results for key redemption and free-license requests.

use crate::vdf;

pub const DETAIL_RATE_LIMITED: i32 = 53;

/// Human text for an `EPurchaseResultDetail`.
pub fn detail_text(detail: i32) -> &'static str {
    match detail {
        0 => "ok",
        4 => "timed out, try again later",
        5 => "invalid package",
        9 => "already owned",
        13 => "not available in your country",
        14 => "invalid key",
        15 => "key already used",
        24 => "requires a game you don't own",
        36 => "must be activated on PlayStation 3",
        50 => "cannot be redeemed in the client, use the store",
        53 => "rate limited, try again in an hour",
        54 => "you own a product that excludes this one",
        _ => "failed",
    }
}

/// Item names from a `CMsgClientPurchaseResponse.purchase_receipt_info` (binary KeyValues).
pub fn receipt_items(receipt: &[u8]) -> Vec<String> {
    let Some(root) = vdf::parse_binary(receipt) else { return Vec::new() };
    let items = root.get("lineitems").or_else(|| root.children().first().and_then(|(_, kv)| kv.get("lineitems")));
    items
        .map(|l| l.children().iter().filter_map(|(_, i)| i.str_at(&["ItemDescription"]).map(str::to_string)).collect())
        .unwrap_or_default()
}

/// Outcome of a free-license request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    Ok,
    RateLimited,
    /// Transient failure: keep it queued.
    Retry(String),
    /// Steam refused it: don't try again.
    Rejected(String),
}

/// Classifies a `POST store.steampowered.com/freelicense/addfreelicense/<subid>` reply.
/// Mirrors ArchiSteamFarm's ArchiWebHandler.AddFreeLicense (Apache-2.0, see NOTICE).
pub fn classify_free_license(status: u16, body: &str) -> Claim {
    match status {
        200 | 500 => {}
        403 => return Claim::Rejected("access denied".into()),
        401 => return Claim::Retry("web session not logged in".into()),
        502..=504 | 404 | 429 => return Claim::Retry(format!("steam store unavailable (HTTP {status})")),
        _ => return Claim::Retry(format!("unexpected HTTP {status}")),
    }
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    if let Some(d) = json.get("purchaseresultdetail").and_then(|d| d.as_i64()) {
        let d = d as i32;
        return match d {
            DETAIL_RATE_LIMITED => Claim::RateLimited,
            0 if status == 200 => Claim::Ok,
            4 => Claim::Retry(detail_text(d).into()),
            _ => Claim::Rejected(format!("{} ({d})", detail_text(d))),
        };
    }
    match json.get("error").and_then(|e| e.as_str()) {
        Some(e) if e.contains("rate limited") => Claim::RateLimited,
        Some(e) if !e.is_empty() => Claim::Rejected(e.to_string()),
        _ if status == 200 => Claim::Ok,
        _ => Claim::Rejected("failed (HTTP 500)".into()),
    }
}

/// Classifies a `CMsgClientRequestFreeLicenseResponse` (`a/<id>`) like FreePackages' ClaimFreeApp:
/// OK with nothing granted means the app isn't free here (or is owned), so it is not retried.
pub fn classify_free_app(eresult: i32, granted: bool) -> Claim {
    match (eresult, granted) {
        (1, true) => Claim::Ok,
        (1, false) => Claim::Rejected("nothing granted (not free here, or already owned)".into()),
        (84, _) => Claim::RateLimited,
        // Busy, Timeout, ServiceUnavailable, TryAnotherCM.
        (10 | 16 | 20 | 48, _) => Claim::Retry(format!("EResult {eresult}")),
        _ => Claim::Rejected(format!("EResult {eresult}")),
    }
}
