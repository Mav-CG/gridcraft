//! Fork Mav-CG/gridcraft: refreshing `=RTD()` cells (docs/superpowers/specs/2026-10-12-rtd-design.md §4.3).

use std::sync::Arc;

use gridcraft_model::CalcMode;

use crate::Session;

impl Session {
    /// Recalculates the RTD cells, and what depends on them, in every open workbook once new
    /// values arrived. Not a command and not history: no undo, redo or journal entry, no new
    /// revision, and a workbook with no unsaved changes stays that way. Workbooks in manual
    /// calculation wait for F9; while one is open no topic is dropped (its cells didn't read theirs).
    pub fn rtd_refresh(&mut self) {
        gridcraft_calc::rtd::begin_pass();
        let mut sweep = true;
        for d in &mut self.docs {
            if d.wb.calc.mode == CalcMode::Manual {
                sweep = false;
                continue;
            }
            let clean = !d.is_dirty();
            let mut wb = (*d.wb).clone();
            d.calc.cells_changed(&mut wb, &[]);
            d.wb = Arc::new(wb);
            if clean {
                d.saved = d.wb.clone();
            }
        }
        gridcraft_calc::rtd::end_pass(sweep);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, MutexGuard};

    use gridcraft_calc::rtd::{Request, TopicKey, TopicTable, install};
    use gridcraft_core::{CellError, CellRef, Value};
    use serde_json::json;

    use crate::Session;

    /// The RTD source is process-wide: these tests take turns.
    static TURN: Mutex<()> = Mutex::new(());
    fn turn() -> MutexGuard<'static, ()> {
        TURN.lock().unwrap_or_else(|e| e.into_inner())
    }

    const PETR4: &str = r#"=RTD("srv.rtd",,"SQT","PETR4","LAST")"#;
    const VALE3: &str = r#"=RTD("srv.rtd",,"SQT","VALE3","LAST")"#;

    fn topic(ticker: &str) -> TopicKey {
        TopicKey { prog_id: "srv.rtd".into(), server: String::new(), topics: vec!["SQT".into(), ticker.into(), "LAST".into()] }
    }

    fn table() -> Arc<TopicTable> {
        let t = Arc::new(TopicTable::new(|p| p == "srv.rtd"));
        install(Some(t.clone()));
        t
    }

    fn set(s: &mut Session, cell: &str, input: &str) {
        s.execute("cell.set", json!({"cell": cell, "input": input})).unwrap();
    }

    fn v(s: &Session, a: &str) -> Value {
        s.doc().unwrap().wb.active().unwrap().value(CellRef::parse(a).unwrap())
    }

    /// A workbook with A1 = PETR4 and B1 = A1+1, requests already taken.
    fn book(t: &TopicTable) -> Session {
        let mut s = Session::new();
        s.new_workbook();
        set(&mut s, "A1", PETR4);
        set(&mut s, "B1", "=A1+1");
        t.take_requests();
        s
    }

    #[test]
    fn refresh_leaves_no_history_and_keeps_a_saved_workbook_clean() {
        let _turn = turn();
        let t = table();
        let mut s = book(&t);
        let d = s.doc_mut().unwrap();
        d.saved = d.wb.clone(); // as if just saved
        let before = (s.doc().unwrap().undo.len(), s.journal.len(), s.doc().unwrap().revision);
        t.set(topic("PETR4"), Value::Number(56.0));
        s.rtd_refresh();
        assert_eq!(v(&s, "B1"), Value::Number(57.0));
        let d = s.doc().unwrap();
        assert_eq!((d.undo.len(), s.journal.len(), d.revision), before);
        assert!(!d.is_dirty());
    }

    #[test]
    fn refresh_keeps_an_edited_workbook_dirty() {
        let _turn = turn();
        let t = table();
        let mut s = book(&t);
        assert!(s.doc().unwrap().is_dirty());
        t.set(topic("PETR4"), Value::Number(56.0));
        s.rtd_refresh();
        assert_eq!(v(&s, "A1"), Value::Number(56.0));
        assert!(s.doc().unwrap().is_dirty());
    }

    #[test]
    fn manual_calculation_waits_for_f9_and_skips_the_sweep() {
        let _turn = turn();
        let t = table();
        let mut s = book(&t);
        set(&mut s, "A2", VALE3);
        t.take_requests();
        s.execute("formulas.calculationOptions", json!({"mode": "manual"})).unwrap();
        set(&mut s, "A2", "");
        t.set(topic("PETR4"), Value::Number(56.0));
        s.rtd_refresh();
        assert_eq!(v(&s, "A1"), Value::Error(CellError::NA));
        assert!(t.take_requests().is_empty(), "VALE3 is kept: the manual workbook didn't read its topics");
        s.execute("formulas.calculateNow", json!({})).unwrap();
        assert_eq!(v(&s, "A1"), Value::Number(56.0));
    }

    #[test]
    fn refresh_covers_every_open_workbook_and_drops_unused_topics() {
        let _turn = turn();
        let t = table();
        let mut s = book(&t); // Book1
        s.new_workbook(); // Book2, active
        set(&mut s, "A1", PETR4);
        set(&mut s, "A2", VALE3);
        set(&mut s, "A2", "");
        t.take_requests();
        t.set(topic("PETR4"), Value::Number(56.0));
        s.rtd_refresh();
        assert_eq!(v(&s, "A1"), Value::Number(56.0));
        s.set_active(0);
        assert_eq!(v(&s, "B1"), Value::Number(57.0));
        assert!(t.take_requests().is_empty(), "VALE3 was read since the last sweep");
        s.rtd_refresh();
        assert_eq!(t.take_requests(), vec![Request::Disconnect(topic("VALE3"))]);
    }

    #[test]
    fn changing_an_argument_cell_swaps_the_topic() {
        let _turn = turn();
        let t = table();
        let mut s = Session::new();
        s.new_workbook();
        set(&mut s, "C1", "PETR4");
        set(&mut s, "A1", r#"=RTD("srv.rtd",,"SQT",C1,"LAST")"#);
        assert_eq!(t.take_requests(), vec![Request::Connect(topic("PETR4"))]);
        set(&mut s, "C1", "VALE3");
        assert_eq!(t.take_requests(), vec![Request::Connect(topic("VALE3"))]);
        t.set(topic("VALE3"), Value::Number(60.0));
        s.rtd_refresh();
        assert_eq!(v(&s, "A1"), Value::Number(60.0));
        s.rtd_refresh();
        assert_eq!(t.take_requests(), vec![Request::Disconnect(topic("PETR4"))], "dropped on the sweep after it was last read");
    }
}
