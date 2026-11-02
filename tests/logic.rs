//! Plain assert checks for the network-free logic, with known vectors.

use dengai::badges::{parse_badges, parse_drops, BadgeGame};
use dengai::pics::{self, App, Owned, Package, Skip};
use dengai::purchase::{self, Claim};
use dengai::state::{State, TokenBucket};
use dengai::totp;

// ---- TOTP ----

#[test]
fn totp_rfc4226_vectors() {
    // RFC 4226 appendix D: secret "12345678901234567890", counters 0..9, truncated values.
    let expected = [
        1284755224, 1094287082, 137359152, 1726969429, 1640338314, 868254676, 1918287922, 82162583, 673399871,
        645520489,
    ];
    for (counter, want) in expected.iter().enumerate() {
        assert_eq!(totp::hotp_value(b"12345678901234567890", counter as u64), *want, "counter {counter}");
    }
}

#[test]
fn totp_steam_codes() {
    // base64("12345678901234567890"); 1284755224 and 1094287082 in Steam's base-26 alphabet.
    let secret = "MTIzNDU2Nzg5MDEyMzQ1Njc4OTA=";
    assert_eq!(totp::steam_code_b64(secret, 0).unwrap(), "GG5F5");
    assert_eq!(totp::steam_code_b64(secret, 29).unwrap(), "GG5F5");
    assert_eq!(totp::steam_code_b64(secret, 59).unwrap(), "PV9M4");
    assert!(totp::steam_code_b64("not base64!", 0).is_err());
}

// ---- badge pages ----

const BADGES: &str = r#"<html><script>g_steamID = "76561198000000000";</script>
<div class="badge_row is_link">
 <a class="badge_row_overlay" href="https://steamcommunity.com/profiles/76561198000000000/gamecards/440/"></a>
 <div class="badge_row_inner">
  <div class="badge_title_row"><div class="badge_title">
   Team Fortress 2&nbsp;<span class="badge_view_details">View details</span>
  </div></div>
  <div class="badge_title_stats"><div class="badge_title_stats_content">
   <div class="badge_title_stats_playtime">&nbsp;12.5 hrs on record</div>
   <div class="card_drop_info_dialog" id="card_drop_info_gamebadge_440_1_0" style="display: none"></div>
   <div class="badge_title_stats_drops"><span class="progress_info_bold">3 card drops remaining</span></div>
  </div></div>
 </div>
</div>
<div class="badge_row is_link">
 <div class="badge_row_inner">
  <div class="badge_title">Done Game&nbsp;<span>View details</span></div>
  <div class="card_drop_info_dialog" id="card_drop_info_gamebadge_730_1_0"></div>
  <span class="progress_info_bold">No card drops remaining</span>
 </div>
</div>
<div class="badge_row is_link">
 <div class="badge_row_inner">
  <div class="badge_title">Years of Service</div>
 </div>
</div>
<div class="badge_row is_link">
 <div class="badge_row_inner">
  <div class="badge_title">Tom &amp; Jerry&nbsp;<span>View details</span></div>
  <div class="card_drop_info_dialog" id="card_drop_info_gamebadge_570_1_0"></div>
  <span class="progress_info_bold">1 card drop remaining</span>
 </div>
</div>
<div class="pageLinks"><a class="pagelink" href="?p=1">1</a> <a class="pagelink" href="?p=2">2</a>
<a class="pagelink" href="?p=3">3</a> <a class="pagebtn" href="?p=2">&gt;</a></div></html>"#;

#[test]
fn badge_page_parser() {
    let (games, pages) = parse_badges(BADGES);
    assert_eq!(pages, 3);
    assert_eq!(
        games,
        vec![
            BadgeGame { appid: 440, name: "Team Fortress 2".into(), drops: 3 },
            BadgeGame { appid: 570, name: "Tom & Jerry".into(), drops: 1 },
        ]
    );
    assert_eq!(parse_badges("<html>no badges</html>"), (vec![], 1));
    assert_eq!(parse_drops(r#"<span class="progress_info_bold">12 card drops remaining</span>"#), Some(12));
    assert_eq!(parse_drops(r#"<span class="progress_info_bold">No card drops remaining</span>"#), Some(0));
    assert_eq!(parse_drops("<html></html>"), None);
    assert!(dengai::badges::is_logged_out("<script>g_steamID = false;</script>"));
    assert!(!dengai::badges::is_logged_out(BADGES));
}

// ---- PICS package filter ----

/// Binary KeyValues encoder for test fixtures.
enum V<'a> {
    I(i32),
    S(&'a str),
    M(Vec<(&'a str, V<'a>)>),
}

fn encode(out: &mut Vec<u8>, pairs: &[(&str, V)]) {
    for (k, v) in pairs {
        let t = match v {
            V::M(_) => 0,
            V::S(_) => 1,
            V::I(_) => 2,
        };
        out.push(t);
        out.extend(k.as_bytes());
        out.push(0);
        match v {
            V::I(n) => out.extend(n.to_le_bytes()),
            V::S(s) => {
                out.extend(s.as_bytes());
                out.push(0);
            }
            V::M(m) => {
                encode(out, m);
                out.push(8);
            }
        }
    }
}

fn package_buffer(id: u32, billing: i32, extended: Vec<(&str, V)>, apps: &[i32]) -> Vec<u8> {
    let mut b = id.to_le_bytes().to_vec();
    let appids = apps.iter().enumerate().map(|(i, a)| (["0", "1", "2"][i], V::I(*a))).collect();
    let id_s = id.to_string();
    encode(
        &mut b,
        &[(
            id_s.as_str(),
            V::M(vec![
                ("packageid", V::I(id as i32)),
                ("billingtype", V::I(billing)),
                ("licensetype", V::I(1)),
                ("status", V::I(0)),
                ("extended", V::M(extended)),
                ("appids", V::M(appids)),
            ]),
        )],
    );
    b.push(8);
    b
}

const APP_DLC: &[u8] = b"\"appinfo\"\n{\n\t\"appid\"\t\t\"441\"\n\t\"common\"\n\t{\n\t\t\"name\"\t\t\"Some DLC\"\n\t\t\"type\"\t\t\"DLC\"\n\t\t\"parent\"\t\t\"440\"\n\t\t\"releasestate\"\t\t\"released\"\n\t}\n\t\"extended\"\n\t{\n\t\t\"listofdlc\"\t\t\"\"\n\t}\n}\n\0";

#[test]
fn pics_package_filter() {
    let now = 1_700_000_000;
    let buf = package_buffer(
        1001,
        12,
        vec![("dontgrantifappidowned", V::I(730)), ("purchaserestrictedcountries", V::S("de fr"))],
        &[441],
    );
    let p = pics::parse_package_buffer(1001, &buf).expect("package parses");
    assert_eq!(p.billing_type, pics::BILLING_FREE_ON_DEMAND);
    assert_eq!(p.app_ids, vec![441]);
    assert_eq!(p.dont_grant_if_app_owned, 730);
    assert_eq!(p.purchase_restricted_countries, Some(vec!["DE".to_string(), "FR".to_string()]));

    let app = pics::parse_app_buffer(441, APP_DLC).expect("app parses");
    assert_eq!(app, App { id: 441, kind: "dlc".into(), parent: 440, available: true, ..App::default() });

    let mut owned = Owned { country: "ID".into(), ..Owned::default() };
    let skip = Skip { demos: true, playtests: true, unowned_dlc: true };
    assert_eq!(pics::precheck(&p, &owned, now), Ok(()));
    // DLC for a game not owned is skipped; owning the base game makes it claimable.
    assert_eq!(
        pics::check_apps(&p, std::slice::from_ref(&app), &owned, skip),
        Err("DLC for a game not owned (skipped)")
    );
    owned.apps.insert(440);
    assert_eq!(pics::check_apps(&p, std::slice::from_ref(&app), &owned, skip), Ok(()));
    assert_eq!(pics::check_apps(&p, std::slice::from_ref(&app), &owned, Skip::default()), Ok(()));
    // Missing app info blocks FreeOnDemand packages.
    assert_eq!(pics::check_apps(&p, &[], &owned, skip), Err("app info missing"));

    // Region: purchase-restricted in DE.
    assert_eq!(
        pics::precheck(&p, &Owned { country: "DE".into(), ..owned.clone() }, now),
        Err("purchase restricted in region")
    );
    // dontgrantifappidowned.
    owned.apps.insert(730);
    assert_eq!(pics::precheck(&p, &owned, now), Err("owns an excluding app"));
    owned.apps.remove(&730);
    // Already owned package.
    owned.packages.insert(1001);
    assert_eq!(pics::precheck(&p, &owned, now), Err("already owned"));
    owned.packages.clear();

    // Billing, status, time window.
    let paid = Package { billing_type: 1, ..p.clone() };
    assert_eq!(pics::precheck(&paid, &owned, now), Err("not free"));
    assert_eq!(pics::precheck(&Package { status: 2, ..p.clone() }, &owned, now), Err("not available"));
    assert_eq!(pics::precheck(&Package { start_time: now + 10, ..p.clone() }, &owned, now), Err("not started yet"));
    assert_eq!(pics::precheck(&Package { expiry_time: now - 10, ..p.clone() }, &owned, now), Err("expired"));
    assert_eq!(
        pics::precheck(&Package { expiry_time: now + 10, start_time: now - 10, ..p.clone() }, &owned, now),
        Ok(())
    );
    assert_eq!(pics::precheck(&Package { beta_tester: true, ..p.clone() }, &owned, now), Err("playtest package"));

    // Demo skip and NoCost tolerance for unreleased contents.
    let demo = App { id: 441, kind: "demo".into(), available: false, ..App::default() };
    assert_eq!(pics::check_apps(&p, std::slice::from_ref(&demo), &owned, Skip::default()), Err("app not released"));
    let nocost = Package { billing_type: pics::BILLING_NO_COST, ..p.clone() };
    assert_eq!(pics::check_apps(&nocost, std::slice::from_ref(&demo), &owned, Skip::default()), Ok(()));
    assert_eq!(pics::check_apps(&nocost, &[demo], &owned, skip), Err("demo (skipped)"));
    // All contents owned.
    owned.apps.insert(441);
    assert_eq!(pics::check_apps(&p, &[app], &owned, skip), Err("all apps owned"));
}

// ---- state, rate limit, results ----

#[test]
fn state_and_bucket() {
    let mut s = State::default();
    assert!(s.enqueue(5));
    assert!(!s.enqueue(5));
    s.rejected.insert(6);
    assert!(!s.enqueue(6));
    let dir = std::env::temp_dir().join(format!("dengai-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.json");
    s.change_number = 42;
    s.save(&path).unwrap();
    let loaded = State::load(&path).unwrap();
    assert_eq!((loaded.change_number, loaded.queue.len(), loaded.rejected.len()), (42, 1, 1));
    std::fs::remove_dir_all(&dir).unwrap();

    let mut b = TokenBucket::new(30, 1000);
    assert_eq!((0..40).filter(|_| b.try_take(1000)).count(), 30);
    assert!(b.try_take(1000 + 120)); // 30/h refills one token per 120 s
    assert!(!b.try_take(1000 + 120));
    b.back_off(2000, 3600);
    assert!(!b.try_take(2000 + 3599));
    assert!(b.try_take(2000 + 3600 + 120));
}

#[test]
fn purchase_results() {
    assert_eq!(purchase::detail_text(15), "key already used");
    assert_eq!(purchase::classify_free_license(200, "[]"), Claim::Ok);
    assert_eq!(purchase::classify_free_license(200, "{\"purchaseresultdetail\":53}"), Claim::RateLimited);
    assert_eq!(
        purchase::classify_free_license(200, "{\"error\":\"You got rate limited, try again in an hour.\"}"),
        Claim::RateLimited
    );
    assert_eq!(purchase::classify_free_license(403, ""), Claim::Rejected("access denied".into()));
    assert!(matches!(purchase::classify_free_license(502, ""), Claim::Retry(_)));
    assert!(matches!(purchase::classify_free_license(200, "{\"purchaseresultdetail\":5}"), Claim::Rejected(_)));
}

#[test]
fn config_env_overrides() {
    let env = |k: &str| match k {
        "DENGAI_CONFIG" => Some("/nonexistent/dengai.toml".to_string()),
        _ => None,
    };
    assert!(dengai::config::Config::load(env).is_err(), "explicit missing config file is an error");
    let env = |k: &str| match k {
        "DENGAI_USERNAME" => Some("bob".to_string()),
        "DENGAI_FARM" => Some("off".to_string()),
        "DENGAI_CLAIMS_PER_HOUR" => Some("10".to_string()),
        _ => None,
    };
    let cfg = dengai::config::Config::load(env).unwrap();
    assert_eq!((cfg.username.as_str(), cfg.farm, cfg.claims_per_hour, cfg.auto_claim), ("bob", false, 10, true));
    assert!(dengai::config::Config::load(|k| (k == "DENGAI_FARM").then(|| "maybe".into())).is_err());
}
