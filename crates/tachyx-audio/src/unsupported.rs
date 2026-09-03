//! The same names on a target with no sound output this client can reach.
//!
//! Naming them anyway keeps one shape above, so a caller gets an error that says
//! what is missing instead of a link that fails.

/// A sound output. Cannot be built on this target.
#[derive(Debug)]
pub(crate) struct Speaker(std::convert::Infallible);

/// Always fails here.
///
/// # Errors
/// Every call. This target has no sound output this client can reach.
pub(crate) fn open() -> Result<Speaker, String> {
    Err("this target has no sound output".to_owned())
}

impl Speaker {
    /// Never reached: no value of this type exists.
    ///
    /// # Errors
    /// Never.
    pub(crate) fn write(&mut self, _frame: &[i16]) -> Result<(), String> {
        match self.0 {}
    }

    pub(crate) fn finish(&mut self) {
        match self.0 {}
    }
}
