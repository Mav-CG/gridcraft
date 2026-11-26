//! Fork Mav-CG (26/11): `=RTD()` topics of Profit's server go through the dash-app book_reader.
//!
//! Profit's `rtdtrading.rtdserver` sends `UpdateNotify` to ONE client: with GridCraft holding a
//! connection of its own, it and the book_reader took the notifications from each other (the
//! book_reader's watchdog restarted it, the new reader took them back and GridCraft froze). So
//! GridCraft no longer talks to Profit: it asks the book_reader for its topics through Redis and
//! reads the values the book_reader publishes. The other side is
//! `E:\dash-app\backend\tools\book_reader\ponte_rtd.py`, which documents the protocol.
//!
//! Without the book daemon (window closed, weekend) or without Redis, the topics fall back to a
//! COM connection of our own (`Route::Direct`) — then GridCraft is Profit's only client. Once the
//! daemon is back (`RTD_PONTE:VIVO`, set before its reader connects) they return to the bridge and
//! our connection is dropped, so the reader keeps the notifications.
// `Instant::now` is disallowed because it traps on wasm; this module only builds on Windows.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::{RtdValue, Topic};

/// ProgIDs read through the book_reader instead of COM.
const BRIDGED: &[&str] = &["rtdtrading.rtdserver"];
const URL: &str = "redis://127.0.0.1:6379/";
const CLIENTS: &str = "RTD_PONTE:CLIENTES";
const VALUES: &str = "RTD_PONTE:VALORES";
const SEP_STRINGS: char = '\u{1f}';
const SEP_TOPICS: char = '\u{1e}';
/// The request is renewed this often and lives `ASK_TTL_S`: once GridCraft is gone (closed or
/// crashed), the book_reader drops its topics.
const ASK_EVERY: Duration = Duration::from_secs(2);
const ASK_TTL_S: u64 = 10;
const READ_EVERY: Duration = Duration::from_millis(100);
const RETRY: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_millis(500);

/// Set by the book daemon while it runs a book_reader (`book_reader_autostart._sinal_da_ponte`).
const ALIVE: &str = "RTD_PONTE:VIVO";

/// Where the bridged topics go from now on.
#[derive(Debug, PartialEq)]
pub(crate) enum Route {
    /// No book daemon (closed, weekend) or no Redis: connect these topics to Profit ourselves.
    Direct(Vec<Topic>),
    /// The book daemon is back: drop our own connection, the book_reader serves the topics.
    Bridge,
}

pub(crate) fn is_bridged(prog_id: &str) -> bool {
    BRIDGED.iter().any(|b| b.eq_ignore_ascii_case(prog_id))
}

fn key(t: &Topic) -> String {
    t.strings.join(&SEP_STRINGS.to_string())
}

fn request<'a>(keys: impl Iterator<Item = &'a String>) -> String {
    let mut keys: Vec<&str> = keys.map(String::as_str).collect();
    keys.sort_unstable();
    keys.join(&SEP_TOPICS.to_string())
}

/// `s<text>`, `n<number>`, `b1`/`b0`, `e` (empty), as `ponte_rtd.codificar` writes them.
fn decode(v: &str) -> RtdValue {
    match v.split_at_checked(1) {
        Some(("s", "")) | Some(("e", "")) => RtdValue::Empty,
        Some(("s", text)) => RtdValue::Text(text.to_string()),
        Some(("n", n)) => n.parse().map_or(RtdValue::Error, RtdValue::Number),
        Some(("b", b)) => RtdValue::Bool(b == "1"),
        _ => RtdValue::Error,
    }
}

pub(crate) struct Ponte {
    client: String,
    topics: HashMap<String, Topic>,
    /// The value last handed out per topic: only changes go out.
    sent: HashMap<String, RtdValue>,
    /// The topic set changed: ask again now, not at `ask_at`.
    dirty: bool,
    /// The topics are connected to Profit directly (no book daemon).
    direct: bool,
    con: Option<redis::Connection>,
    retry_at: Option<Instant>,
    ask_at: Option<Instant>,
    read_at: Option<Instant>,
}

impl Ponte {
    pub(crate) fn new() -> Ponte {
        Ponte {
            client: format!("gridcraft:{}", std::process::id()),
            topics: HashMap::new(),
            sent: HashMap::new(),
            dirty: false,
            direct: false,
            con: None,
            retry_at: None,
            ask_at: None,
            read_at: None,
        }
    }

    pub(crate) fn want(&mut self, t: Topic) {
        self.topics.insert(key(&t), t);
        self.dirty = true;
    }

    pub(crate) fn unwant(&mut self, t: &Topic) {
        let k = key(t);
        self.topics.remove(&k);
        self.sent.remove(&k);
        self.dirty = true;
    }

    pub(crate) fn direct(&self) -> bool {
        self.direct
    }

    /// Whether the topics change hands, given whether the book daemon is running.
    fn route(&mut self, alive: bool) -> Option<Route> {
        if alive == !self.direct {
            return None;
        }
        self.direct = !alive;
        if alive {
            self.sent.clear(); // hand out the bridge's values again, whatever direct gave
            log::info!("rtd: Profit through the book_reader");
            Some(Route::Bridge)
        } else {
            log::info!("rtd: no book daemon, Profit connected directly");
            Some(Route::Direct(self.topics.values().cloned().collect()))
        }
    }

    /// Renews the request, checks the book daemon and reads the values: returns the values that
    /// changed and, when the topics change hands, where they go now.
    pub(crate) fn tick(&mut self, now: Instant) -> (Vec<(Topic, RtdValue)>, Option<Route>) {
        if self.topics.is_empty() && !self.dirty {
            return (Vec::new(), None);
        }
        if self.retry_at.is_some_and(|at| now < at) {
            return (Vec::new(), None);
        }
        match self.exchange(now) {
            Ok(out) => {
                self.retry_at = None;
                out
            }
            Err(e) => {
                if self.retry_at.is_none() {
                    log::warn!("rtd: ponte (Redis {URL}): {e}");
                }
                self.con = None;
                self.retry_at = Some(now + RETRY);
                (Vec::new(), self.route(false))
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn exchange(&mut self, now: Instant) -> redis::RedisResult<(Vec<(Topic, RtdValue)>, Option<Route>)> {
        if self.con.is_none() {
            let con = redis::Client::open(URL)?.get_connection_with_timeout(IO_TIMEOUT)?;
            con.set_read_timeout(Some(IO_TIMEOUT))?;
            con.set_write_timeout(Some(IO_TIMEOUT))?;
            self.con = Some(con);
            self.dirty = true;
        }
        let Some(con) = self.con.as_mut() else { return Ok((Vec::new(), None)) };
        let mut route = None;
        if self.dirty || self.ask_at.is_none_or(|at| now >= at) {
            let (alive,): (bool,) = redis::pipe()
                .cmd("SADD")
                .arg(CLIENTS)
                .arg(&self.client)
                .ignore()
                .cmd("SET")
                .arg(format!("RTD_PONTE:PEDIDO:{}", self.client))
                .arg(request(self.topics.keys()))
                .arg("EX")
                .arg(ASK_TTL_S)
                .ignore()
                .cmd("EXISTS")
                .arg(ALIVE)
                .query(con)?;
            self.dirty = false;
            self.ask_at = Some(now + ASK_EVERY);
            route = self.route(alive);
        }
        if self.direct || self.topics.is_empty() || self.read_at.is_some_and(|at| now < at) {
            return Ok((Vec::new(), route));
        }
        let Some(con) = self.con.as_mut() else { return Ok((Vec::new(), route)) };
        self.read_at = Some(now + READ_EVERY);
        let keys: Vec<&String> = self.topics.keys().collect();
        let values: Vec<Option<String>> = redis::cmd("HMGET").arg(VALUES).arg(&keys).query(con)?;
        let mut out = Vec::new();
        for (k, v) in keys.into_iter().zip(values) {
            let Some(v) = v else { continue };
            let v = decode(&v);
            if self.sent.get(k) != Some(&v)
                && let Some(t) = self.topics.get(k)
            {
                self.sent.insert(k.clone(), v.clone());
                out.push((t.clone(), v));
            }
        }
        Ok((out, route))
    }

    /// Withdraws the request so the book_reader drops our topics now, not when it expires.
    pub(crate) fn stop(&mut self) {
        if let Some(con) = self.con.as_mut() {
            let _ = redis::pipe()
                .cmd("DEL")
                .arg(format!("RTD_PONTE:PEDIDO:{}", self.client))
                .ignore()
                .cmd("SREM")
                .arg(CLIENTS)
                .arg(&self.client)
                .ignore()
                .exec(con);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(strings: &[&str]) -> Topic {
        Topic { prog_id: "rtdtrading.rtdserver".into(), server: String::new(), strings: strings.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn only_profit_is_bridged() {
        assert!(is_bridged("RTDTrading.RtdServer"));
        assert!(!is_bridged("srv.rtd"));
    }

    #[test]
    fn values_come_back_typed_as_the_book_reader_writes_them() {
        assert_eq!(decode("s165.900"), RtdValue::Text("165.900".into()));
        assert_eq!(decode("s"), RtdValue::Empty);
        assert_eq!(decode("e"), RtdValue::Empty);
        assert_eq!(decode("n165.925"), RtdValue::Number(165.925));
        assert_eq!(decode("n3.0"), RtdValue::Number(3.0));
        assert_eq!(decode("b1"), RtdValue::Bool(true));
        assert_eq!(decode("b0"), RtdValue::Bool(false));
        assert_eq!(decode("nxx"), RtdValue::Error);
        assert_eq!(decode(""), RtdValue::Error);
    }

    #[test]
    fn the_request_joins_strings_and_topics_with_the_separators() {
        let keys = [key(&t(&["WINZ26", "ULT"])), key(&t(&["PETR4", "ULT"]))];
        assert_eq!(request(keys.iter()), "PETR4\u{1f}ULT\u{1e}WINZ26\u{1f}ULT");
        assert_eq!(request(std::iter::empty()), "");
    }

    #[test]
    fn topics_go_direct_without_the_book_daemon_and_back_when_it_returns() {
        let mut p = Ponte::new();
        p.want(t(&["WINZ26_F_0", "ULT"]));
        assert_eq!(p.route(true), None, "daemon running from the start: nothing changes hands");
        assert_eq!(p.route(false), Some(Route::Direct(vec![t(&["WINZ26_F_0", "ULT"])])));
        assert!(p.direct());
        assert_eq!(p.route(false), None, "already direct");
        p.sent.insert(key(&t(&["WINZ26_F_0", "ULT"])), RtdValue::Number(1.0));
        assert_eq!(p.route(true), Some(Route::Bridge));
        assert!(!p.direct() && p.sent.is_empty(), "the bridge's values go out again");
    }

    #[test]
    fn a_dropped_topic_marks_the_request_to_be_sent_again() {
        let mut p = Ponte::new();
        p.want(t(&["PETR4", "ULT"]));
        p.dirty = false;
        p.unwant(&t(&["PETR4", "ULT"]));
        assert!(p.topics.is_empty() && p.dirty, "an empty request must still go out once");
    }
}
