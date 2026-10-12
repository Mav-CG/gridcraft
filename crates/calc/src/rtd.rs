//! `=RTD(progID, server, topic1, …)` — fork Mav-CG/gridcraft (the upstream leaves RTD out on purpose).
//!
//! The evaluator turns the arguments into a [`TopicKey`] and asks the [`RtdSource`] the app
//! installed ([`install`]); with none (Linux, macOS, web) every RTD is `#N/A`. [`TopicTable`] is the
//! source the desktop app uses: the last value of each topic, the connect/disconnect requests for
//! the COM client, and dropping topics no formula reads any more.
//! Spec: docs/superpowers/specs/2026-10-12-rtd-design.md

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use gridcraft_core::{CellError, Value};
use gridcraft_formula::Expr;

use crate::eval::{Ev, Evaluator};

/// One RTD topic: what the formula's arguments say.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TopicKey {
    /// Lower-case: COM ProgIDs ignore case.
    pub prog_id: String,
    pub server: String,
    pub topics: Vec<String>,
}

/// Where RTD values come from (the desktop app installs one on Windows).
pub trait RtdSource: Send + Sync {
    /// The topic's latest value, `None` while it has none (the cell shows `#N/A`).
    fn value(&self, key: &TopicKey) -> Option<Value>;
    /// A refresh pass starts.
    fn begin_pass(&self) {}
    /// The refresh pass ended; `sweep` = every open workbook was recalculated in it, so a topic
    /// nothing read since the previous sweep (in this pass, or in between: conditional formatting
    /// evaluated while painting, an edit) is unused.
    fn end_pass(&self, _sweep: bool) {}
}

static SOURCE: RwLock<Option<Arc<dyn RtdSource>>> = RwLock::new(None);

/// Installs the process-wide source (`None` removes it).
pub fn install(source: Option<Arc<dyn RtdSource>>) {
    if let Ok(mut s) = SOURCE.write() {
        *s = source;
    }
}

fn source() -> Option<Arc<dyn RtdSource>> {
    SOURCE.read().ok().and_then(|s| s.clone())
}

pub fn begin_pass() {
    if let Some(s) = source() {
        s.begin_pass();
    }
}

pub fn end_pass(sweep: bool) {
    if let Some(s) = source() {
        s.end_pass(sweep);
    }
}

impl Evaluator<'_> {
    /// `RTD(progID, server, topic1, …)`: at least one topic; each argument is used as text, and one
    /// that is an error gives that error.
    pub(crate) fn rtd(&mut self, args: &[Expr]) -> Ev {
        if args.len() < 3 {
            return Ev::V(Value::Error(CellError::Value));
        }
        let mut parts = Vec::with_capacity(args.len());
        for a in args {
            match self.value(a).scalar().to_text() {
                Ok(t) => parts.push(t),
                Err(e) => return Ev::V(Value::Error(e)),
            }
        }
        let mut parts = parts.into_iter();
        let prog_id = parts.next().unwrap_or_default().to_ascii_lowercase();
        let server = parts.next().unwrap_or_default();
        let key = TopicKey { prog_id, server, topics: parts.collect() };
        Ev::V(source().and_then(|s| s.value(&key)).unwrap_or(Value::Error(CellError::NA)))
    }
}

/// What the COM client has to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Connect(TopicKey),
    Disconnect(TopicKey),
}

/// The desktop app's source: values by topic, plus the requests for the COM client.
pub struct TopicTable {
    allowed: fn(&str) -> bool,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Topics asked for and not dropped.
    live: HashSet<TopicKey>,
    values: HashMap<TopicKey, Value>,
    /// Topics read since the last sweep.
    seen: HashSet<TopicKey>,
    requests: Vec<Request>,
}

impl TopicTable {
    /// `allowed(prog_id)` decides which servers may be used; any other is `#N/A` with no request.
    pub fn new(allowed: fn(&str) -> bool) -> TopicTable {
        TopicTable { allowed, inner: Mutex::new(Inner::default()) }
    }

    /// A value from the server; ignored once the topic was dropped.
    pub fn set(&self, key: TopicKey, value: Value) {
        if let Ok(mut i) = self.inner.lock()
            && i.live.contains(&key)
        {
            i.values.insert(key, value);
        }
    }

    pub fn take_requests(&self) -> Vec<Request> {
        self.inner.lock().map(|mut i| std::mem::take(&mut i.requests)).unwrap_or_default()
    }
}

impl RtdSource for TopicTable {
    fn value(&self, key: &TopicKey) -> Option<Value> {
        if !(self.allowed)(&key.prog_id) {
            return None;
        }
        let mut i = self.inner.lock().ok()?;
        i.seen.insert(key.clone());
        if i.live.insert(key.clone()) {
            i.requests.push(Request::Connect(key.clone()));
        }
        i.values.get(key).cloned()
    }

    fn end_pass(&self, sweep: bool) {
        let Ok(mut i) = self.inner.lock() else { return };
        if !sweep {
            return;
        }
        let unused: Vec<TopicKey> = i.live.iter().filter(|k| !i.seen.contains(*k)).cloned().collect();
        for k in unused {
            i.live.remove(&k);
            i.values.remove(&k);
            i.requests.push(Request::Disconnect(k));
        }
        i.seen.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, MutexGuard};

    use gridcraft_core::{CellError, CellRef, Value};
    use gridcraft_model::{Cell, Formula, Workbook};

    use super::*;
    use crate::Calc;

    /// The source is process-wide: RTD tests take turns.
    static TURN: Mutex<()> = Mutex::new(());
    fn turn() -> MutexGuard<'static, ()> {
        TURN.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct Book {
        wb: Workbook,
        calc: Calc,
    }

    impl Book {
        fn new() -> Book {
            Book { wb: Workbook::new(), calc: Calc::new() }
        }
        fn set(&mut self, at: &str, input: &str) {
            let c = CellRef::parse(at).unwrap();
            let cell = if input.starts_with('=') {
                Cell::formula(Formula::new(input))
            } else {
                Cell::value(gridcraft_core::parse::parse_input(input, self.wb.date_system).value)
            };
            self.wb.sheet_mut(0).unwrap().cells.set(c, cell);
            self.calc.cells_changed(&mut self.wb, &[(0, c)]);
        }
        fn get(&self, at: &str) -> Value {
            self.wb.sheet(0).unwrap().value(CellRef::parse(at).unwrap())
        }
        /// What a refresh does: recalculates the RTD cells and what depends on them.
        fn refresh(&mut self) {
            self.calc.cells_changed(&mut self.wb, &[]);
        }
    }

    fn key(prog_id: &str, topics: &[&str]) -> TopicKey {
        TopicKey { prog_id: prog_id.into(), server: String::new(), topics: topics.iter().map(|t| t.to_string()).collect() }
    }

    fn table() -> Arc<TopicTable> {
        let t = Arc::new(TopicTable::new(|p| p == "srv.rtd" || p == "rtdtrading.rtdserver"));
        install(Some(t.clone()));
        t
    }

    #[test]
    fn without_a_source_rtd_is_na() {
        let _turn = turn();
        install(None);
        let mut b = Book::new();
        b.set("A1", r#"=RTD("srv.rtd",,"SQT","PETR4","LAST")"#);
        assert_eq!(b.get("A1"), Value::Error(CellError::NA));
    }

    #[test]
    fn value_arrives_and_dependents_follow() {
        let _turn = turn();
        let t = table();
        let mut b = Book::new();
        b.set("A1", r#"=RTD("srv.rtd",,"SQT","PETR4","LAST")"#);
        b.set("B1", "=A1*2");
        assert_eq!(b.get("A1"), Value::Error(CellError::NA));
        let k = key("srv.rtd", &["SQT", "PETR4", "LAST"]);
        assert_eq!(t.take_requests(), vec![Request::Connect(k.clone())]);
        t.set(k.clone(), Value::Number(56.0));
        b.refresh();
        assert_eq!((b.get("A1"), b.get("B1")), (Value::Number(56.0), Value::Number(112.0)));
        t.set(k, Value::Number(57.5));
        b.refresh();
        assert_eq!(b.get("B1"), Value::Number(115.0));
        assert!(t.take_requests().is_empty(), "a known topic is not asked for again");
    }

    #[test]
    fn arguments_become_the_topic() {
        let _turn = turn();
        let t = table();
        let mut b = Book::new();
        b.set("C1", "PETR4");
        b.set("D1", "27");
        b.set("A1", r#"=RTD("SRV.RTD","","PLAYERSRANKING",C1,D1,"SALDO")"#);
        b.set("A2", r#"=RTD("srv.rtd",,"PLAYERSRANKING",C1,D1,"SALDO")"#);
        let k = key("srv.rtd", &["PLAYERSRANKING", "PETR4", "27", "SALDO"]);
        assert_eq!(t.take_requests(), vec![Request::Connect(k)], "two cells, one topic; ProgID case ignored");
    }

    #[test]
    fn errors_arity_and_servers_not_allowed() {
        let _turn = turn();
        let t = table();
        let mut b = Book::new();
        b.set("A1", r#"=RTD("srv.rtd",,1/0)"#);
        b.set("A2", r#"=RTD("srv.rtd","")"#);
        b.set("A3", r#"=RTD("outro.servidor",,"X")"#);
        assert_eq!(b.get("A1"), Value::Error(CellError::Div0));
        assert_eq!(b.get("A2"), Value::Error(CellError::Value));
        assert_eq!(b.get("A3"), Value::Error(CellError::NA));
        assert!(t.take_requests().is_empty(), "nothing reaches a server that is not allowed");
    }

    #[test]
    fn unused_topics_are_dropped_only_on_a_full_pass() {
        let _turn = turn();
        let t = table();
        let mut b = Book::new();
        b.set("A1", r#"=RTD("rtdtrading.rtdserver",,"PETR4_B_0","ULT")"#);
        b.set("A2", r#"=RTD("rtdtrading.rtdserver",,"VALE3_B_0","ULT")"#);
        t.take_requests();
        b.set("A2", "");
        begin_pass();
        b.refresh();
        end_pass(false);
        assert!(t.take_requests().is_empty(), "no sweep while some workbook was not recalculated");
        begin_pass();
        b.refresh();
        end_pass(true);
        assert!(t.take_requests().is_empty(), "VALE3 was read since the last sweep (before A2 was cleared)");
        begin_pass();
        b.refresh();
        end_pass(true);
        let vale3 = key("rtdtrading.rtdserver", &["VALE3_B_0", "ULT"]);
        assert_eq!(t.take_requests(), vec![Request::Disconnect(vale3.clone())]);
        b.set("A2", r#"=RTD("rtdtrading.rtdserver",,"VALE3_B_0","ULT")"#);
        assert_eq!(t.take_requests(), vec![Request::Connect(vale3)], "typed again: connected again");
    }

    /// Conditional formatting and the like evaluate RTD while painting, between refresh passes
    /// (which only recalculate cells): such a topic must not be dropped and connected again.
    #[test]
    fn a_topic_read_between_passes_is_kept() {
        let _turn = turn();
        let t = table();
        let k = key("srv.rtd", &["SQT", "PETR4", "LAST"]);
        for _ in 0..3 {
            let _ = t.value(&k); // painted
            begin_pass();
            end_pass(true); // a refresh that recalculated every workbook without reading it
        }
        assert_eq!(t.take_requests(), vec![Request::Connect(k)], "connected once, never dropped");
    }
}
