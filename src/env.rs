//! Environment variables, read as Go's `os.Getenv` reads them: an unset
//! variable is the empty string.
//!
//! Tests set variables per thread through `overrides`, so they never change the
//! real process environment that other tests read at the same time.

/// Returns the value of `name`, or "" when it is not set.
pub fn var(name: &str) -> String {
    #[cfg(test)]
    if let Some(v) = overrides::get(name) {
        return v;
    }
    std::env::var(name).unwrap_or_default()
}

/// Returns the trimmed value of `name`, or `def` when that is empty.
pub fn trimmed_or(name: &str, def: &str) -> String {
    let v = var(name);
    let v = v.trim();
    if v.is_empty() {
        def.to_string()
    } else {
        v.to_string()
    }
}

#[cfg(test)]
pub mod overrides {
    use std::cell::RefCell;
    use std::collections::HashMap;

    thread_local! {
        static VARS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    }

    /// Sets `name` for the calling thread only. An empty value reads as unset,
    /// as with Go's t.Setenv(name, "").
    pub fn set(name: &str, value: &str) {
        VARS.with(|v| v.borrow_mut().insert(name.into(), value.into()));
    }

    pub(super) fn get(name: &str) -> Option<String> {
        VARS.with(|v| v.borrow().get(name).cloned())
    }
}
