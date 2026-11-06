//! PICS package/app models and the free-package filter.
//!
//! The filter rules are ported from the FreePackages ASF plugin
//! (https://github.com/Citrinate/FreePackages, Apache-2.0). See NOTICE.

use crate::vdf::{self, Kv};
use serde::Deserialize;
use std::collections::HashSet;

pub const BILLING_NO_COST: u64 = 10;
pub const BILLING_FREE_ON_DEMAND: u64 = 12;
pub const LICENSE_SINGLE_PURCHASE: u64 = 1;
pub const STATUS_AVAILABLE: u64 = 0;

/// The fields of a PICS package that matter for claiming it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Package {
    pub id: u32,
    pub billing_type: u64,
    pub license_type: u64,
    pub status: u64,
    pub start_time: u64,
    pub expiry_time: u64,
    pub dont_grant_if_app_owned: u32,
    pub must_own_app: u32,
    pub restricted_countries: Option<Vec<String>>,
    pub only_allow_restricted: bool,
    pub purchase_restricted_countries: Option<Vec<String>>,
    pub allow_purchase_from_restricted: bool,
    pub deactivated_demo: bool,
    pub beta_tester: bool,
    pub app_ids: Vec<u32>,
}

/// The fields of a PICS app that matter for claiming a package containing it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct App {
    pub id: u32,
    /// `common/type`, lowercased: game, dlc, demo, beta (playtest), tool, ...
    pub kind: String,
    /// `common/parent` (or `extended/betaforappid` for playtests).
    pub parent: u32,
    /// Released (`releasestate` released/preloadonly or `state` eStateAvailable).
    pub available: bool,
    /// `extended/isfreeapp`.
    pub free: bool,
    pub must_own_app: u32,
    pub restricted_countries: Option<Vec<String>>,
    pub purchase_restricted_countries: Option<Vec<String>>,
    pub allow_purchase_from_restricted: bool,
}

/// What the account owns and where it is.
#[derive(Debug, Clone, Default)]
pub struct Owned {
    pub packages: HashSet<u32>,
    pub apps: HashSet<u32>,
    /// ISO country code, upper case. Empty when unknown.
    pub country: String,
}

/// Optional skips on top of the redeemability rules.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Skip {
    pub demos: bool,
    pub playtests: bool,
    pub unowned_dlc: bool,
}

fn countries(s: Option<&str>) -> Option<Vec<String>> {
    s.map(|s| s.split([' ', ',']).filter(|c| !c.is_empty()).map(str::to_ascii_uppercase).collect())
}

fn listed(list: &[String], country: &str) -> bool {
    list.iter().any(|c| c.eq_ignore_ascii_case(country))
}

fn u32_at(kv: &Kv, path: &[&str]) -> u32 {
    u32::try_from(kv.u64_at(path)).unwrap_or(0)
}

impl Package {
    pub fn from_kv(id: u32, kv: &Kv) -> Self {
        let ext = |k: &'static str| -> [&'static str; 2] { ["extended", k] };
        Package {
            id,
            billing_type: kv.u64_at(&["billingtype"]),
            license_type: kv.u64_at(&["licensetype"]),
            status: kv.u64_at(&["status"]),
            start_time: kv.u64_at(&ext("starttime")),
            expiry_time: kv.u64_at(&ext("expirytime")),
            dont_grant_if_app_owned: u32_at(kv, &ext("dontgrantifappidowned")),
            must_own_app: u32_at(kv, &ext("mustownapptopurchase")),
            restricted_countries: countries(kv.str_at(&ext("restrictedcountries"))),
            only_allow_restricted: kv.bool_at(&ext("onlyallowrestrictedcountries")),
            purchase_restricted_countries: countries(kv.str_at(&ext("purchaserestrictedcountries"))),
            allow_purchase_from_restricted: kv.bool_at(&ext("allowpurchasefromrestrictedcountries")),
            deactivated_demo: kv.bool_at(&ext("deactivated_demo")),
            beta_tester: kv.bool_at(&ext("betatesterpackage")),
            app_ids: kv
                .get("appids")
                .map(|a| a.children().iter().filter_map(|(_, v)| v.as_u64()?.try_into().ok()).collect())
                .unwrap_or_default(),
        }
    }
}

/// A PICS package buffer: a u32 package id followed by binary KeyValues `{ "<id>" { ... } }`.
pub fn parse_package_buffer(id: u32, buf: &[u8]) -> Option<Package> {
    let root = vdf::parse_binary(buf.get(4..)?)?;
    let (_, kv) = root.children().first()?;
    Some(Package::from_kv(id, kv))
}

impl App {
    /// `kv` is the `appinfo` node of a PICS app buffer.
    pub fn from_kv(id: u32, kv: &Kv) -> Self {
        let kind = kv.str_at(&["common", "type"]).unwrap_or_default().to_ascii_lowercase();
        let mut parent = if kind == "beta" { u32_at(kv, &["extended", "betaforappid"]) } else { 0 };
        if parent == 0 {
            parent = u32_at(kv, &["common", "parent"]);
        }
        let release = kv.str_at(&["common", "releasestate"]).unwrap_or_default();
        App {
            id,
            kind,
            parent,
            available: release == "released"
                || release == "preloadonly"
                || kv.str_at(&["extended", "state"]) == Some("eStateAvailable"),
            free: kv.bool_at(&["extended", "isfreeapp"]),
            must_own_app: u32_at(kv, &["extended", "mustownapptopurchase"]),
            restricted_countries: countries(kv.str_at(&["common", "restricted_countries"])),
            purchase_restricted_countries: countries(kv.str_at(&["extended", "purchaserestrictedcountries"])),
            allow_purchase_from_restricted: kv.bool_at(&["extended", "allowpurchasefromrestrictedcountries"]),
        }
    }
}

/// A PICS app buffer: text KeyValues `"appinfo" { ... }`, NUL terminated.
pub fn parse_app_buffer(id: u32, buf: &[u8]) -> Option<App> {
    let root = vdf::parse_text(&String::from_utf8_lossy(buf))?;
    Some(App::from_kv(id, root.get("appinfo")?))
}

/// Package-level rules, needing only the package's own product info.
pub fn precheck(p: &Package, owned: &Owned, now: u64) -> Result<(), &'static str> {
    if p.billing_type != BILLING_NO_COST && p.billing_type != BILLING_FREE_ON_DEMAND {
        return Err("not free");
    }
    if p.status != STATUS_AVAILABLE {
        return Err("not available");
    }
    if p.license_type != LICENSE_SINGLE_PURCHASE {
        return Err("not a single-purchase license");
    }
    if p.app_ids.is_empty() {
        return Err("no apps");
    }
    if p.start_time > now {
        return Err("not started yet");
    }
    if p.expiry_time > 0 && p.expiry_time <= now {
        return Err("expired");
    }
    if p.deactivated_demo {
        return Err("deactivated demo");
    }
    if p.beta_tester {
        return Err("playtest package");
    }
    if p.id == 17906 {
        // Anonymous Dedicated Server Comp: always AccessDenied.
        return Err("never redeemable");
    }
    if owned.packages.contains(&p.id) {
        return Err("already owned");
    }
    if p.dont_grant_if_app_owned > 0 && owned.apps.contains(&p.dont_grant_if_app_owned) {
        return Err("owns an excluding app");
    }
    if p.must_own_app > 0 && !owned.apps.contains(&p.must_own_app) {
        return Err("missing a required app");
    }
    package_region(p, &owned.country)
}

/// The package's own country locks for an account in `country`.
pub fn package_region(p: &Package, country: &str) -> Result<(), &'static str> {
    if let Some(list) = &p.restricted_countries {
        if listed(list, country) != p.only_allow_restricted {
            return Err("region restricted");
        }
    }
    if let Some(list) = &p.purchase_restricted_countries {
        if listed(list, country) != p.allow_purchase_from_restricted {
            return Err("purchase restricted in region");
        }
    }
    Ok(())
}

/// An app's country locks for an account in `country`.
pub fn app_region(app: &App, country: &str) -> Result<(), &'static str> {
    if app.restricted_countries.as_deref().is_some_and(|l| listed(l, country)) {
        return Err("app region restricted");
    }
    if let Some(list) = &app.purchase_restricted_countries {
        if listed(list, country) != app.allow_purchase_from_restricted {
            return Err("app purchase restricted in region");
        }
    }
    Ok(())
}

/// Rules needing the product info of the package's apps (`apps`: the ones PICS returned).
pub fn check_apps(p: &Package, apps: &[App], owned: &Owned, skip: Skip) -> Result<(), &'static str> {
    // NoCost packages are often redeemable even when some contents are unknown or unreleased.
    if p.billing_type != BILLING_NO_COST {
        if p.app_ids.iter().any(|id| !apps.iter().any(|a| a.id == *id)) {
            return Err("app info missing");
        }
        if apps.iter().any(|a| !a.available) {
            return Err("app not released");
        }
    }
    if p.app_ids.iter().all(|id| owned.apps.contains(id)) {
        return Err("all apps owned");
    }
    apps.iter().try_for_each(|app| app_rules(app, owned, &p.app_ids, skip))
}

/// FreePackages' `IsRedeemableApp` (minus ownership) plus the `skip_*` options. `included`: the other
/// apps granted together with this one, which count as owned.
fn app_rules(app: &App, owned: &Owned, included: &[u32], skip: Skip) -> Result<(), &'static str> {
    let has = |id: u32| owned.apps.contains(&id) || included.contains(&id);
    if app.must_own_app > 0 && !has(app.must_own_app) {
        return Err("app needs another app");
    }
    app_region(app, &owned.country)?;
    match app.kind.as_str() {
        "demo" if skip.demos => Err("demo (skipped)"),
        "beta" if skip.playtests => Err("playtest (skipped)"),
        "dlc" if skip.unowned_dlc && app.parent > 0 && !has(app.parent) => Err("DLC for a game not owned (skipped)"),
        _ => Ok(()),
    }
}

/// Rules for claiming an app on its own (`a/<id>` via `CMsgClientRequestFreeLicense`): FreePackages'
/// `FilterableApp.IsFree` + `IsAvailable` + `PackageFilter.IsRedeemableApp`, plus the `skip_*` options.
pub fn check_app(app: &App, owned: &Owned, skip: Skip) -> Result<(), &'static str> {
    // ponytail: FreePackages joins playtests through the store's ajaxrequestplaytestaccess (needs the
    // parent's product info and a waitlist cache); dengai doesn't, so a beta app is never claimed alone.
    if app.kind == "beta" {
        return Err("playtest (not supported)");
    }
    if !app.free && app.kind != "demo" {
        return Err("not free");
    }
    if !app.available {
        return Err("not released");
    }
    if owned.apps.contains(&app.id) {
        return Err("already owned");
    }
    app_rules(app, owned, &[], skip)
}
