//! Against the real servers: Profit and Cedro Fast Trade open and logged in. By hand:
//! `cargo test -p gridcraft-rtd-com --test live -- --ignored --nocapture`
#![cfg(windows)]
#![allow(clippy::disallowed_methods)] // `Instant::now`: Windows only, never wasm

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gridcraft_rtd_com::{RtdClient, RtdValue, Topic};

fn topic(prog_id: &str, strings: &[&str]) -> Topic {
    Topic { prog_id: prog_id.into(), server: String::new(), strings: strings.iter().map(|s| s.to_string()).collect() }
}

fn number(v: Option<&RtdValue>) -> bool {
    matches!(v, Some(RtdValue::Number(n)) if *n > 0.0)
}

#[test]
#[ignore = "needs Profit and Fast Trade running"]
fn reads_petr4_from_both_servers() {
    let mut c = RtdClient::start(Box::new(|| {}));
    let fast = topic("srv.rtd", &["SQT", "PETR4", "LAST"]);
    let profit = topic("rtdtrading.rtdserver", &["PETR4_B_0", "ULT"]);
    c.connect(fast.clone());
    c.connect(profit.clone());
    let mut got: HashMap<Topic, RtdValue> = HashMap::new();
    let end = Instant::now() + Duration::from_secs(30);
    while Instant::now() < end && !(number(got.get(&fast)) && number(got.get(&profit))) {
        for (t, v) in c.take_updates() {
            println!("{:?} = {v:?}", t.strings);
            got.insert(t, v);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    c.stop();
    assert!(number(got.get(&fast)), "Fast Trade: {:?}", got.get(&fast));
    assert!(number(got.get(&profit)), "Profit: {:?}", got.get(&profit));
}
