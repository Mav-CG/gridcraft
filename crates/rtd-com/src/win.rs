//! The COM side (spec §4.2): VARIANT and SAFEARRAY helpers, the RTD interfaces and the STA thread.
#![allow(non_snake_case)]
// COM member names (`ServerStart`, `UpdateNotify`) from the RTD interfaces.
// `Instant::now` is disallowed because it traps on wasm; this module only builds on Windows.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{DISP_E_MEMBERNOTFOUND, DISP_E_UNKNOWNNAME, E_FAIL, E_NOTIMPL, E_OUTOFMEMORY, VARIANT_BOOL, VARIANT_TRUE};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, CLSIDFromProgID, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    DISPATCH_FLAGS, DISPATCH_PROPERTYGET, DISPPARAMS, EXCEPINFO, IDispatch, IDispatch_Impl, IDispatch_Vtbl, ITypeInfo, SAFEARRAY,
};
use windows::Win32::System::Ole::{
    SafeArrayCreateVector, SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayPutElement,
};
use windows::Win32::System::Variant::{
    VAR_CHANGE_FLAGS, VARIANT, VT_BOOL, VT_BSTR, VT_EMPTY, VT_ERROR, VT_I4, VT_NULL, VT_R8, VT_VARIANT, VariantChangeType, VariantClear,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT, TranslateMessage,
};
use windows_core::{BSTR, GUID, HRESULT, Interface, PCWSTR, implement, interface};

use crate::{RtdValue, Topic};

/// A server or topic that failed is tried again after this.
const RETRY: Duration = Duration::from_secs(5);
/// `stop` waits this long for the COM thread (a frozen server must not hold up closing the app).
const STOP_WAIT: Duration = Duration::from_secs(2);
/// How long the thread sleeps waiting for COM messages (`UpdateNotify`) or requests.
const PUMP: Duration = Duration::from_millis(50);
const HEARTBEAT_MS: i32 = 15_000;
/// Cap on the topics read from one `RefreshData` array (input from another program).
const MAX_TOPICS: i32 = 1_000_000;

// Excel's RTD interfaces (IIDs from the type library the servers register). Dual: the vtable
// follows IDispatch, members in IDL order.
#[interface("EC0E6191-DB51-11D3-8F3E-00C04F3651B8")]
unsafe trait IRtdServer: IDispatch {
    fn ServerStart(&self, callback: *mut c_void, result: *mut i32) -> HRESULT;
    fn ConnectData(&self, topic_id: i32, strings: *mut *mut SAFEARRAY, get_new_values: *mut VARIANT_BOOL, value: *mut VARIANT) -> HRESULT;
    fn RefreshData(&self, topic_count: *mut i32, values: *mut *mut SAFEARRAY) -> HRESULT;
    fn DisconnectData(&self, topic_id: i32) -> HRESULT;
    fn Heartbeat(&self, result: *mut i32) -> HRESULT;
    fn ServerTerminate(&self) -> HRESULT;
}

#[interface("A43788C1-D91B-11D3-8F39-00C04F3651B8")]
unsafe trait IRTDUpdateEvent: IDispatch {
    fn UpdateNotify(&self) -> HRESULT;
    fn get_HeartbeatInterval(&self, value: *mut i32) -> HRESULT;
    fn put_HeartbeatInterval(&self, value: i32) -> HRESULT;
    fn Disconnect(&self) -> HRESULT;
}

/// What the servers call back: `UpdateNotify` only raises a flag (it may come from another thread);
/// the COM thread then asks for the values with `RefreshData`.
#[implement(IRTDUpdateEvent)]
struct Callback {
    pending: Arc<AtomicBool>,
}

impl IRTDUpdateEvent_Impl for Callback_Impl {
    unsafe fn UpdateNotify(&self) -> HRESULT {
        self.pending.store(true, Ordering::Release);
        HRESULT(0)
    }
    unsafe fn get_HeartbeatInterval(&self, value: *mut i32) -> HRESULT {
        if !value.is_null() {
            // SAFETY: the caller passes a writable i32.
            unsafe { *value = HEARTBEAT_MS };
        }
        HRESULT(0)
    }
    unsafe fn put_HeartbeatInterval(&self, _value: i32) -> HRESULT {
        HRESULT(0)
    }
    unsafe fn Disconnect(&self) -> HRESULT {
        HRESULT(0)
    }
}

/// Late-bound callers (a .NET server may go through IDispatch) reach the same members by DISPID:
/// 10 UpdateNotify, 11 HeartbeatInterval, 12 Disconnect.
impl IDispatch_Impl for Callback_Impl {
    fn GetTypeInfoCount(&self) -> windows_core::Result<u32> {
        Ok(0)
    }
    fn GetTypeInfo(&self, _itinfo: u32, _lcid: u32) -> windows_core::Result<ITypeInfo> {
        Err(E_NOTIMPL.into())
    }
    fn GetIDsOfNames(&self, _riid: *const GUID, _names: *const PCWSTR, _count: u32, _lcid: u32, _ids: *mut i32) -> windows_core::Result<()> {
        Err(DISP_E_UNKNOWNNAME.into())
    }
    fn Invoke(
        &self,
        id: i32,
        _riid: *const GUID,
        _lcid: u32,
        flags: DISPATCH_FLAGS,
        _params: *const DISPPARAMS,
        result: *mut VARIANT,
        _excep: *mut EXCEPINFO,
        _argerr: *mut u32,
    ) -> windows_core::Result<()> {
        match id {
            10 => {
                self.pending.store(true, Ordering::Release);
                Ok(())
            }
            11 => {
                if flags.0 & DISPATCH_PROPERTYGET.0 != 0 && !result.is_null() {
                    let v = ManuallyDrop::new(i4_variant(HEARTBEAT_MS));
                    // SAFETY: the caller passes a VARIANT for the result; an i4 owns nothing to free.
                    unsafe { *result = std::ptr::read(&v.0) };
                }
                Ok(())
            }
            12 => Ok(()),
            _ => Err(DISP_E_MEMBERNOTFOUND.into()),
        }
    }
}

/// A VARIANT we own, cleared (`VariantClear`) when dropped.
pub(crate) struct Var(pub(crate) VARIANT);

impl Drop for Var {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid VARIANT owned by this value.
        let _ = unsafe { VariantClear(&mut self.0) };
    }
}

pub(crate) fn bstr_variant(s: &str) -> Var {
    let mut v = VARIANT::default();
    // SAFETY: a fresh VARIANT; `vt` names the union member written next to it.
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_BSTR;
        inner.Anonymous.bstrVal = ManuallyDrop::new(BSTR::from(s));
    }
    Var(v)
}

pub(crate) fn i4_variant(n: i32) -> Var {
    let mut v = VARIANT::default();
    // SAFETY: as in `bstr_variant`.
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_I4;
        inner.Anonymous.lVal = n;
    }
    Var(v)
}

/// Text, booleans and anything numeric (integers, currency, dates) become values; an empty text
/// is empty, errors and arrays are `Error`.
pub(crate) fn to_value(v: &Var) -> RtdValue {
    // SAFETY: `Var` holds a valid VARIANT and `vt` names the member that is set.
    unsafe {
        let inner = &v.0.Anonymous.Anonymous;
        match inner.vt {
            VT_EMPTY | VT_NULL => RtdValue::Empty,
            VT_BSTR => {
                let s = inner.Anonymous.bstrVal.to_string();
                if s.is_empty() { RtdValue::Empty } else { RtdValue::Text(s) }
            }
            VT_BOOL => RtdValue::Bool(inner.Anonymous.boolVal.0 != 0),
            VT_ERROR => RtdValue::Error,
            _ => {
                let mut n = Var(VARIANT::default());
                match VariantChangeType(&mut n.0, &v.0, VAR_CHANGE_FLAGS(0), VT_R8) {
                    Ok(()) => RtdValue::Number(n.0.Anonymous.Anonymous.Anonymous.dblVal),
                    Err(_) => RtdValue::Error,
                }
            }
        }
    }
}

struct Server {
    rtd: IRtdServer,
    pending: Arc<AtomicBool>,
    _callback: IRTDUpdateEvent,
}

/// CoCreateInstance (in-process DLL or local EXE, which COM starts if it isn't running) + ServerStart.
fn start_server(prog_id: &str) -> windows_core::Result<Server> {
    let wide: Vec<u16> = prog_id.encode_utf16().chain(std::iter::once(0)).collect();
    let pending = Arc::new(AtomicBool::new(false));
    let callback: IRTDUpdateEvent = Callback { pending: pending.clone() }.into();
    // SAFETY: `wide` is NUL-terminated and outlives the calls; `callback` stays alive in `Server`.
    unsafe {
        let clsid = CLSIDFromProgID(PCWSTR(wide.as_ptr()))?;
        let rtd: IRtdServer = CoCreateInstance(&clsid, None, CLSCTX_INPROC_SERVER | CLSCTX_LOCAL_SERVER)?;
        let mut started = 0;
        rtd.ServerStart(callback.as_raw(), &mut started).ok()?;
        if started <= 0 {
            return Err(windows_core::Error::new(E_FAIL, format!("ServerStart returned {started}")));
        }
        Ok(Server { rtd, pending, _callback: callback })
    }
}

/// ConnectData: the topic strings go as a 1-D array of BSTR VARIANTs; returns the initial value.
fn connect_data(rtd: &IRtdServer, id: i32, strings: &[String]) -> windows_core::Result<RtdValue> {
    let len = u32::try_from(strings.len()).map_err(|_| windows_core::Error::from(E_FAIL))?;
    // SAFETY: creates an array we own and destroy below.
    let psa = unsafe { SafeArrayCreateVector(VT_VARIANT, 0, len) };
    if psa.is_null() {
        return Err(E_OUTOFMEMORY.into());
    }
    let r = fill_and_connect(rtd, id, psa, strings);
    // SAFETY: created above; the server only read it.
    let _ = unsafe { SafeArrayDestroy(psa) };
    r
}

fn fill_and_connect(rtd: &IRtdServer, id: i32, psa: *mut SAFEARRAY, strings: &[String]) -> windows_core::Result<RtdValue> {
    for (i, s) in strings.iter().enumerate() {
        let v = bstr_variant(s);
        let i = i32::try_from(i).map_err(|_| windows_core::Error::from(E_FAIL))?;
        // SAFETY: `i` is inside the vector; PutElement copies the VARIANT.
        unsafe { SafeArrayPutElement(psa, &i, &v.0 as *const VARIANT as *const c_void)? };
    }
    let mut strings_array = psa;
    let mut get_new_values = VARIANT_TRUE;
    let mut out = Var(VARIANT::default());
    // SAFETY: every pointer is valid for the duration of the call.
    unsafe { rtd.ConnectData(id, &mut strings_array, &mut get_new_values, &mut out.0).ok()? };
    Ok(to_value(&out))
}

/// RefreshData: (topic id, value) for each topic that changed.
fn refresh_data(rtd: &IRtdServer) -> windows_core::Result<Vec<(i32, RtdValue)>> {
    let mut count = 0;
    let mut psa: *mut SAFEARRAY = std::ptr::null_mut();
    // SAFETY: out-pointers valid for the call; we own and destroy the array it returns.
    unsafe { rtd.RefreshData(&mut count, &mut psa).ok()? };
    if psa.is_null() {
        return Ok(Vec::new());
    }
    let r = read_pairs(psa);
    // SAFETY: returned to us by the server, read above.
    let _ = unsafe { SafeArrayDestroy(psa) };
    r
}

/// RefreshData's array is 2 × n: row 0 the topic ids, row 1 their values. Servers write it with
/// `index = {row, topic}` (dimension 1 first), so that is how it is read.
fn read_pairs(psa: *mut SAFEARRAY) -> windows_core::Result<Vec<(i32, RtdValue)>> {
    // SAFETY: `psa` is the server's valid array; bounds come from the array itself.
    unsafe {
        if SafeArrayGetDim(psa) != 2 {
            return Ok(Vec::new());
        }
        let row = SafeArrayGetLBound(psa, 1)?;
        let (lo, hi) = (SafeArrayGetLBound(psa, 2)?, SafeArrayGetUBound(psa, 2)?);
        let mut out = Vec::new();
        for j in lo..=hi.min(lo.saturating_add(MAX_TOPICS)) {
            let id = element(psa, [row, j])?;
            let value = element(psa, [row.saturating_add(1), j])?;
            if let RtdValue::Number(n) = to_value(&id) {
                out.push((n as i32, to_value(&value)));
            }
        }
        Ok(out)
    }
}

fn element(psa: *mut SAFEARRAY, at: [i32; 2]) -> windows_core::Result<Var> {
    let mut v = Var(VARIANT::default());
    // SAFETY: `at` has one index per dimension; GetElement copies into `v`, which we own.
    unsafe { SafeArrayGetElement(psa, at.as_ptr(), &mut v.0 as *mut VARIANT as *mut c_void)? };
    Ok(v)
}

/// The server process is gone (closed or crashed): forget it and start it again later.
fn is_gone(e: &windows_core::Error) -> bool {
    matches!(
        e.code().0 as u32,
        0x8001_0108 // RPC_E_DISCONNECTED
            | 0x8001_0007 // RPC_E_SERVER_DIED
            | 0x8001_0012 // RPC_E_SERVER_DIED_DNE
            | 0x8007_06BA // RPC_S_SERVER_UNAVAILABLE
            | 0x8004_01FD // CO_E_OBJNOTCONNECTED
    )
}

/// Wait up to `wait` for COM messages, then dispatch them (this is how `UpdateNotify` arrives).
fn pump(wait: Duration) {
    let ms = u32::try_from(wait.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: message-loop calls on this thread's own queue.
    unsafe {
        let _ = MsgWaitForMultipleObjects(None, false, ms, QS_ALLINPUT);
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }
}

#[derive(Default)]
struct State {
    servers: HashMap<String, Server>,
    /// When a ProgID that failed (start, connect or refresh) may be tried again.
    retry_at: HashMap<String, Instant>,
    /// Topics asked for: `Some(id)` once connected.
    topics: HashMap<Topic, Option<i32>>,
    ids: HashMap<i32, Topic>,
    next_id: i32,
}

impl State {
    fn want(&mut self, t: Topic) {
        self.topics.entry(t).or_insert(None);
    }

    fn unwant(&mut self, t: &Topic) {
        if let Some(Some(id)) = self.topics.remove(t) {
            self.ids.remove(&id);
            if let Some(s) = self.servers.get(&t.prog_id) {
                // SAFETY: a live server and one of its topic ids.
                let _ = unsafe { s.rtd.DisconnectData(id) };
            }
        }
    }

    fn server(&mut self, prog_id: &str) -> windows_core::Result<&Server> {
        if !self.servers.contains_key(prog_id) {
            let s = start_server(prog_id)?;
            self.servers.insert(prog_id.to_string(), s);
        }
        self.servers.get(prog_id).ok_or_else(|| E_FAIL.into())
    }

    /// ConnectData for the topics not connected yet; a failure (the Profit still loading, say)
    /// holds that ProgID back for `RETRY`.
    fn connect_pending(&mut self) -> Vec<(Topic, RtdValue)> {
        let now = Instant::now();
        let waiting: Vec<Topic> = self.topics.iter().filter(|(_, id)| id.is_none()).map(|(t, _)| t.clone()).collect();
        let mut out = Vec::new();
        for t in waiting {
            if self.retry_at.get(&t.prog_id).is_some_and(|at| now < *at) {
                continue;
            }
            let rtd = match self.server(&t.prog_id) {
                Ok(s) => s.rtd.clone(),
                Err(e) => {
                    log::warn!("rtd: {}: {e}", t.prog_id);
                    self.retry_at.insert(t.prog_id.clone(), now + RETRY);
                    continue;
                }
            };
            self.next_id = self.next_id.checked_add(1).unwrap_or(1);
            let id = self.next_id;
            match connect_data(&rtd, id, &t.strings) {
                Ok(v) => {
                    self.topics.insert(t.clone(), Some(id));
                    self.ids.insert(id, t.clone());
                    out.push((t, v));
                }
                Err(e) => {
                    log::warn!("rtd: ConnectData {} {:?}: {e}", t.prog_id, t.strings);
                    if is_gone(&e) {
                        self.forget(&t.prog_id);
                    }
                    self.retry_at.insert(t.prog_id.clone(), now + RETRY);
                }
            }
        }
        out
    }

    /// RefreshData on the servers that called UpdateNotify. A failing server is forgotten.
    fn refresh(&mut self) -> Vec<(Topic, RtdValue)> {
        let mut out = Vec::new();
        let mut gone = Vec::new();
        for (prog_id, s) in &self.servers {
            if !s.pending.swap(false, Ordering::AcqRel) {
                continue;
            }
            match refresh_data(&s.rtd) {
                Ok(pairs) => out.extend(pairs.into_iter().filter_map(|(id, v)| Some((self.ids.get(&id)?.clone(), v)))),
                Err(e) => {
                    log::warn!("rtd: RefreshData {prog_id}: {e}");
                    gone.push(prog_id.clone());
                }
            }
        }
        let now = Instant::now();
        for p in gone {
            self.forget(&p);
            self.retry_at.insert(p, now + RETRY);
        }
        out
    }

    /// The server went away: drop it; its topics wait to be connected again (cells keep their
    /// last value meanwhile).
    fn forget(&mut self, prog_id: &str) {
        self.servers.remove(prog_id);
        for (_, id) in self.topics.iter_mut().filter(|(t, _)| t.prog_id == prog_id) {
            if let Some(id) = id.take() {
                self.ids.remove(&id);
            }
        }
    }

    fn shutdown(&mut self) {
        for (t, id) in self.topics.drain() {
            if let (Some(id), Some(s)) = (id, self.servers.get(&t.prog_id)) {
                // SAFETY: a live server and one of its topic ids.
                let _ = unsafe { s.rtd.DisconnectData(id) };
            }
        }
        for s in self.servers.values() {
            // SAFETY: a started server.
            let _ = unsafe { s.rtd.ServerTerminate() };
        }
        self.servers.clear();
        self.ids.clear();
    }
}

enum Msg {
    Connect(Topic),
    Disconnect(Topic),
    Stop,
}

/// Handle to the COM thread. Requests are queued; values come back through `take_updates`.
pub struct RtdClient {
    tx: Sender<Msg>,
    updates: Arc<Mutex<Vec<(Topic, RtdValue)>>>,
    done: Option<Receiver<()>>,
}

impl RtdClient {
    /// Starts the COM thread. `waker` is called (from that thread) whenever values arrive.
    pub fn start(waker: Box<dyn Fn() + Send>) -> RtdClient {
        let (tx, rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let updates = Arc::new(Mutex::new(Vec::new()));
        let out = updates.clone();
        let spawned = std::thread::Builder::new().name("rtd-com".into()).spawn(move || {
            run(&rx, &out, &*waker);
            let _ = done_tx.send(());
        });
        if let Err(e) = spawned {
            log::error!("rtd: the COM thread did not start: {e}");
        }
        RtdClient { tx, updates, done: Some(done_rx) }
    }

    pub fn connect(&self, topic: Topic) {
        let _ = self.tx.send(Msg::Connect(topic));
    }

    pub fn disconnect(&self, topic: Topic) {
        let _ = self.tx.send(Msg::Disconnect(topic));
    }

    /// Values received since the last call (the first one of a topic comes from ConnectData).
    pub fn take_updates(&self) -> Vec<(Topic, RtdValue)> {
        self.updates.lock().map(|mut u| std::mem::take(&mut *u)).unwrap_or_default()
    }

    /// Disconnects every topic and terminates the servers, waiting at most `STOP_WAIT`.
    pub fn stop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(done) = self.done.take() {
            let _ = done.recv_timeout(STOP_WAIT);
        }
    }
}

impl Drop for RtdClient {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(rx: &Receiver<Msg>, out: &Mutex<Vec<(Topic, RtdValue)>>, waker: &dyn Fn()) {
    // SAFETY: once on this thread, paired with CoUninitialize below.
    if let Err(e) = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok() {
        log::error!("rtd: CoInitializeEx: {e}");
        return;
    }
    let mut st = State::default();
    'run: loop {
        loop {
            match rx.try_recv() {
                Ok(Msg::Connect(t)) => st.want(t),
                Ok(Msg::Disconnect(t)) => st.unwant(&t),
                Ok(Msg::Stop) | Err(TryRecvError::Disconnected) => break 'run,
                Err(TryRecvError::Empty) => break,
            }
        }
        let mut fresh = st.connect_pending();
        fresh.extend(st.refresh());
        if !fresh.is_empty() {
            if let Ok(mut o) = out.lock() {
                o.extend(fresh);
            }
            waker();
        }
        pump(PUMP);
    }
    st.shutdown();
    drop(st);
    // SAFETY: matches the CoInitializeEx above; every COM object of this thread is released.
    unsafe { CoUninitialize() };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_become_values() {
        assert_eq!(to_value(&bstr_variant("PETR4")), RtdValue::Text("PETR4".into()));
        assert_eq!(to_value(&bstr_variant("")), RtdValue::Empty);
        assert_eq!(to_value(&i4_variant(27)), RtdValue::Number(27.0));
        assert_eq!(to_value(&Var(VARIANT::default())), RtdValue::Empty);
    }

    #[test]
    fn a_server_that_does_not_exist_gives_nothing_and_stops_quickly() {
        let mut c = RtdClient::start(Box::new(|| {}));
        c.connect(Topic { prog_id: "gridcraft.nao.existe".into(), server: String::new(), strings: vec!["X".into()] });
        std::thread::sleep(Duration::from_millis(300));
        assert!(c.take_updates().is_empty());
        let t0 = Instant::now();
        c.stop();
        assert!(t0.elapsed() < Duration::from_secs(3));
    }

    /// The array as RTD servers build it (Microsoft's C++ RTD sample): bounds `{2, n}`, and
    /// `SafeArrayPutElement` with `index = {row, topic}` (row 0 the topic id, row 1 the value).
    #[test]
    fn refresh_arrays_are_read_as_servers_write_them() {
        use windows::Win32::System::Com::SAFEARRAYBOUND;
        use windows::Win32::System::Ole::SafeArrayCreate;
        let bounds = [SAFEARRAYBOUND { cElements: 2, lLbound: 0 }, SAFEARRAYBOUND { cElements: 3, lLbound: 0 }];
        let values = [(7, i4_variant(56)), (8, bstr_variant("PETR4")), (9, i4_variant(-1))];
        unsafe {
            let psa = SafeArrayCreate(VT_VARIANT, 2, bounds.as_ptr());
            assert!(!psa.is_null());
            for (topic, (id, value)) in values.iter().enumerate() {
                let id = i4_variant(*id);
                SafeArrayPutElement(psa, [0, topic as i32].as_ptr(), &id.0 as *const VARIANT as *const c_void).unwrap();
                SafeArrayPutElement(psa, [1, topic as i32].as_ptr(), &value.0 as *const VARIANT as *const c_void).unwrap();
            }
            let pairs = read_pairs(psa).unwrap();
            SafeArrayDestroy(psa).unwrap();
            assert_eq!(pairs, vec![(7, RtdValue::Number(56.0)), (8, RtdValue::Text("PETR4".into())), (9, RtdValue::Number(-1.0))]);
        }
    }
}
