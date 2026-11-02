//! Parsers for the Steam Community badge pages (`/badges?p=N`, `/gamecards/<appid>`).
//!
//! Same selectors as ArchiSteamFarm's CardsFarmer (Apache-2.0, see NOTICE), done with plain
//! string search instead of a DOM.

use serde::Serialize;

/// A game with card drops remaining.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BadgeGame {
    pub appid: u32,
    pub name: String,
    pub drops: u32,
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let rest = &s[s.find(start)? + start.len()..];
    Some(&rest[..rest.find(end)?])
}

fn first_number(s: &str) -> Option<u32> {
    let start = s.find(|c: char| c.is_ascii_digit())?;
    let digits: String =
        s[start..].chars().take_while(|c| c.is_ascii_digit() || *c == ',').filter(|c| *c != ',').collect();
    digits.parse().ok()
}

fn decode(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_string()
}

/// Card drops remaining from the first `progress_info_bold` span ("3 card drops remaining",
/// "No card drops remaining"). `None` when the span is missing.
pub fn parse_drops(html: &str) -> Option<u32> {
    let text = between(html, "class=\"progress_info_bold\">", "<")?;
    Some(first_number(text).unwrap_or(0))
}

/// True when the page was served to a logged-out visitor.
pub fn is_logged_out(html: &str) -> bool {
    html.contains("g_steamID = false")
}

/// Parses one badges page: the games with drops remaining, and the number of pages.
pub fn parse_badges(html: &str) -> (Vec<BadgeGame>, u32) {
    let mut games = Vec::new();
    for row in html.split("class=\"badge_row_inner\"").skip(1) {
        // Game badges carry <div class="card_drop_info_dialog" id="card_drop_info_gamebadge_<appid>_...">.
        let Some(appid) = between(row, "id=\"card_drop_info_gamebadge_", "_").and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let drops = parse_drops(row).unwrap_or(0);
        if appid == 0 || drops == 0 || games.iter().any(|g: &BadgeGame| g.appid == appid) {
            continue;
        }
        let name = between(row, "class=\"badge_title\">", "<").map(decode).unwrap_or_default();
        games.push(BadgeGame { appid, name, drops });
    }
    let pages = html
        .split("class=\"pagelink\"")
        .skip(1)
        .filter_map(|s| between(s, ">", "<")?.trim().parse::<u32>().ok())
        .max()
        .unwrap_or(1);
    (games, pages)
}
