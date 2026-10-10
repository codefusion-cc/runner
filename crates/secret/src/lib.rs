//! A value that never shows itself.
//!
//! [`Secret`] holds a token, a key or a configuration that must not reach a log, an error message or a debug dump.
//! `Debug` prints `Secret(..)` and nothing of the value; there is no `Display`, no `Serialize`, and no `Clone` for
//! types that do not already allow it. The value comes out only through [`Secret::expose`], so every use of it can be
//! found with a search.

use std::fmt;

/// A value that must not be printed. See the [crate documentation](crate).
#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// Wraps a value.
    pub fn new(value: T) -> Self {
        Secret(value)
    }

    /// The value. Whoever calls this is responsible for where it goes: not into a log line, an error or a file.
    pub fn expose(&self) -> &T {
        &self.0
    }

    /// Takes the value out.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

impl<T> From<T> for Secret<T> {
    fn from(value: T) -> Self {
        Secret(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_prints_nothing_of_the_value() {
        let secret = Secret::new("ghs_supersecret".to_string());
        assert_eq!(format!("{secret:?}"), "Secret(..)");
        assert_eq!(format!("{secret:#?}"), "Secret(..)");
    }

    #[test]
    fn a_struct_holding_a_secret_does_not_show_it() {
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Holder {
            name: &'static str,
            token: Secret<String>,
        }
        let shown = format!(
            "{:?}",
            Holder {
                name: "air",
                token: Secret::new("ghs_abc123".into())
            }
        );
        assert_eq!(shown, r#"Holder { name: "air", token: Secret(..) }"#);
    }

    #[test]
    fn expose_and_into_inner_give_the_value_back() {
        let secret = Secret::new(vec![1u8, 2, 3]);
        assert_eq!(secret.expose(), &[1, 2, 3]);
        assert_eq!(secret.into_inner(), vec![1, 2, 3]);
    }
}
