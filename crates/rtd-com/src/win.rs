//! The COM side (spec §4.2): VARIANT and SAFEARRAY helpers, the RTD interfaces and the STA thread.
#![allow(non_snake_case)] // COM member names (`ServerStart`, `UpdateNotify`) from the RTD interfaces.
#![cfg_attr(not(test), allow(dead_code))] // until the client (Task 2) uses the helpers

use std::mem::ManuallyDrop;

use windows::Win32::System::Variant::{
    VAR_CHANGE_FLAGS, VARIANT, VT_BOOL, VT_BSTR, VT_EMPTY, VT_ERROR, VT_I4, VT_NULL, VT_R8, VariantChangeType, VariantClear,
};
use windows_core::BSTR;

use crate::RtdValue;

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
}
