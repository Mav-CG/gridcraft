//! Fork Mav-CG/gridcraft: live `=RTD()` on Windows. Connects calc's RTD source (`TopicTable`) to
//! the COM client (`gridcraft-rtd-com`) and refreshes the open workbooks at most every 200 ms.
//! Spec: docs/superpowers/specs/2026-10-12-rtd-design.md §4.3

use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gridcraft_engine::calc::rtd::{Request, TopicKey, TopicTable};
use gridcraft_engine::core::{CellError, Value};
use gridcraft_rtd_com::{RtdClient, RtdValue, Topic, is_allowed};
use gridcraft_ui_egui::SheetApp;

/// At most one refresh per this long.
const THROTTLE: Duration = Duration::from_millis(200);

struct Live {
    table: Arc<TopicTable>,
    client: Option<RtdClient>,
    last: Option<Instant>,
    /// Values arrived that no refresh has shown yet.
    dirty: bool,
}

thread_local! {
    static LIVE: RefCell<Option<Live>> = const { RefCell::new(None) };
}

/// Before any workbook opens, so its RTD formulas register their topics on the first calculation.
pub fn install() {
    let table = Arc::new(TopicTable::new(is_allowed));
    gridcraft_engine::calc::rtd::install(Some(table.clone()));
    LIVE.with(|l| *l.borrow_mut() = Some(Live { table, client: None, last: None, dirty: false }));
}

/// Once the window exists: starts the COM thread, which wakes the UI when values arrive.
pub fn start(ctx: &egui::Context) {
    let ctx = ctx.clone();
    LIVE.with(|l| {
        if let Some(live) = l.borrow_mut().as_mut() {
            live.client = Some(RtdClient::start(Box::new(move || ctx.request_repaint())));
        }
    });
}

/// Every frame (`App::logic`).
pub fn tick(app: &mut SheetApp, ctx: &egui::Context) {
    LIVE.with(|l| {
        if let Some(live) = l.borrow_mut().as_mut() {
            live.tick(app, ctx);
        }
    });
}

/// On exit: disconnects the topics and terminates the servers (waits at most 2 s).
pub fn stop() {
    LIVE.with(|l| {
        if let Some(mut live) = l.borrow_mut().take()
            && let Some(client) = live.client.as_mut()
        {
            client.stop();
        }
    });
}

impl Live {
    // `Instant::now` is disallowed because it traps on wasm; this module only builds on Windows.
    #[allow(clippy::disallowed_methods)]
    fn tick(&mut self, app: &mut SheetApp, ctx: &egui::Context) {
        let Some(client) = &self.client else { return };
        send(&self.table, client);
        let updates = client.take_updates();
        if !updates.is_empty() {
            for (t, v) in updates {
                self.table.set(key(t), value(v));
            }
            self.dirty = true;
        }
        if !self.dirty {
            return;
        }
        let since = self.last.map_or(THROTTLE, |t| t.elapsed());
        if since < THROTTLE {
            ctx.request_repaint_after(THROTTLE - since);
            return;
        }
        app.session.rtd_refresh();
        self.last = Some(Instant::now());
        self.dirty = false;
        // Topics the refresh dropped (or first read) go out now.
        send(&self.table, client);
        ctx.request_repaint();
    }
}

fn send(table: &TopicTable, client: &RtdClient) {
    for r in table.take_requests() {
        match r {
            Request::Connect(k) => client.connect(topic(k)),
            Request::Disconnect(k) => client.disconnect(topic(k)),
        }
    }
}

fn topic(k: TopicKey) -> Topic {
    Topic { prog_id: k.prog_id, server: k.server, strings: k.topics }
}

fn key(t: Topic) -> TopicKey {
    TopicKey { prog_id: t.prog_id, server: t.server, topics: t.strings }
}

fn value(v: RtdValue) -> Value {
    match v {
        RtdValue::Empty => Value::Empty,
        RtdValue::Number(n) => Value::number(n),
        RtdValue::Text(s) => Value::text(s),
        RtdValue::Bool(b) => Value::Bool(b),
        RtdValue::Error => Value::Error(CellError::NA),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_values_become_cell_values() {
        assert_eq!(value(RtdValue::Empty), Value::Empty);
        assert_eq!(value(RtdValue::Number(56.0)), Value::Number(56.0));
        assert_eq!(value(RtdValue::Text("ABERTO".into())), Value::text("ABERTO"));
        assert_eq!(value(RtdValue::Bool(true)), Value::Bool(true));
        assert_eq!(value(RtdValue::Error), Value::Error(CellError::NA));
        assert_eq!(value(RtdValue::Number(f64::NAN)), Value::Error(CellError::Num));
    }

    #[test]
    fn topics_round_trip() {
        let k = TopicKey { prog_id: "srv.rtd".into(), server: String::new(), topics: vec!["SQT".into(), "PETR4".into(), "LAST".into()] };
        let t: Topic = topic(k.clone());
        assert_eq!(t.strings, k.topics);
        assert_eq!(key(t), k);
    }
}
