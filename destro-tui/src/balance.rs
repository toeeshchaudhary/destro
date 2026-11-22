//! Provider balance via DeepSeek's public `GET /user/balance` (API-key auth).
//! This is NOT the harness's own account view, which uses a browser-session
//! credential this client cannot see; it needs `DEEPSEEK_API_KEY` in the
//! environment or a `.env` file. The key is only ever sent as the bearer header.

use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::backend::Msg;

#[derive(Clone, Debug)]
pub struct Wallet {
    pub currency: String,
    pub total: String,
    pub granted: String,
    pub topped_up: String,
}

#[derive(Clone, Debug)]
pub struct Info {
    pub available: bool,
    pub wallets: Vec<Wallet>,
}

pub enum Balance {
    /// Provider has no balance endpoint we know of (non-DeepSeek route).
    Unsupported,
    NoKey,
    Loading,
    Ok {
        info: Info,
        at: Instant,
    },
    Err {
        why: String,
        last: Option<(Info, Instant)>,
    },
}

impl Balance {
    pub fn sym(currency: &str) -> &'static str {
        match currency {
            "CNY" => "¥",
            "USD" => "$",
            _ => "",
        }
    }

    /// Short header text, e.g. `¥ 42.17`.
    pub fn short(&self) -> Option<String> {
        let info = match self {
            Balance::Ok { info, .. } => info,
            Balance::Err {
                last: Some((info, _)),
                ..
            } => info,
            _ => return None,
        };
        let w = info.wallets.first()?;
        Some(format!("{} {}", Balance::sym(&w.currency), w.total))
    }

    /// Low when the first wallet is under 5 units or the account is unavailable.
    pub fn is_low(&self) -> bool {
        let info = match self {
            Balance::Ok { info, .. } => info,
            Balance::Err {
                last: Some((info, _)),
                ..
            } => info,
            _ => return false,
        };
        !info.available
            || info
                .wallets
                .first()
                .and_then(|w| w.total.parse::<f64>().ok())
                .is_some_and(|v| v < 5.0)
    }
}

pub fn parse(body: &str) -> Result<Info, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad json: {e}"))?;
    let arr = v["balance_infos"]
        .as_array()
        .ok_or("no balance_infos in response")?;
    let s = |w: &Value, k: &str| w[k].as_str().unwrap_or("0").to_string();
    Ok(Info {
        available: v["is_available"].as_bool().unwrap_or(true),
        wallets: arr
            .iter()
            .map(|w| Wallet {
                currency: s(w, "currency"),
                total: s(w, "total_balance"),
                granted: s(w, "granted_balance"),
                topped_up: s(w, "topped_up_balance"),
            })
            .collect(),
    })
}

/// `DEEPSEEK_API_KEY` from the environment, else from `.env` in any of `dirs`.
pub fn find_key(dirs: &[String]) -> Option<String> {
    if let Ok(k) = std::env::var("DEEPSEEK_API_KEY")
        && !k.trim().is_empty()
    {
        return Some(k.trim().to_string());
    }
    for d in dirs.iter().map(String::as_str).chain(std::iter::once(".")) {
        if let Ok(text) = std::fs::read_to_string(std::path::Path::new(d).join(".env")) {
            for line in text.lines() {
                if let Some(v) = line.trim().strip_prefix("DEEPSEEK_API_KEY=") {
                    let v = v.trim().trim_matches('"').trim_matches('\'');
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

fn fetch(key: &str, base: &str) -> Result<Info, String> {
    let url = format!(
        "{}/user/balance",
        base.trim_end_matches('/').trim_end_matches("/v1")
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    match agent
        .get(&url)
        .header("Authorization", format!("Bearer {key}"))
        .call()
    {
        Ok(mut r) => parse(&r.body_mut().read_to_string().map_err(|e| e.to_string())?),
        Err(ureq::Error::StatusCode(401)) => Err("invalid API key (401)".into()),
        Err(ureq::Error::StatusCode(c)) => Err(format!("HTTP {c}")),
        Err(e) => Err(e.to_string().replace(key, "***")),
    }
}

/// Poll every 60s on a background thread; results arrive as `Msg::Balance`.
pub fn spawn_poller(key: String, tx: Sender<Msg>) {
    let base =
        std::env::var("DEEPSEEK_BASE_URL").unwrap_or_else(|_| "https://api.deepseek.com".into());
    thread::spawn(move || {
        loop {
            let r = fetch(&key, &base);
            if tx.send(Msg::Balance(r)).is_err() {
                return;
            }
            thread::sleep(Duration::from_secs(60));
        }
    });
}

/// Demo stand-in so the panel is visible without a key.
pub fn spawn_demo(tx: Sender<Msg>) {
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(900));
        let body = r#"{"is_available":true,"balance_infos":[{"currency":"CNY","total_balance":"42.17","granted_balance":"10.00","topped_up_balance":"32.17"}]}"#;
        let _ = tx.send(Msg::Balance(parse(body)));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_response() {
        let i = parse(r#"{"is_available":true,"balance_infos":[{"currency":"CNY","total_balance":"110.00","granted_balance":"10.00","topped_up_balance":"100.00"}]}"#).unwrap();
        assert!(i.available);
        assert_eq!(i.wallets[0].total, "110.00");
        assert_eq!(
            Balance::Ok {
                info: i,
                at: Instant::now()
            }
            .short()
            .unwrap(),
            "¥ 110.00"
        );
    }

    #[test]
    fn low_balance_flags() {
        let i = parse(r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"1.20","granted_balance":"0","topped_up_balance":"1.20"}]}"#).unwrap();
        assert!(
            Balance::Ok {
                info: i,
                at: Instant::now()
            }
            .is_low()
        );
    }
}
