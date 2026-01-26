use std::fmt;

///
/// [`poll()`](crate::Metric::poll) value for a [`Metric`](crate::Metric)
///
/// A collection of `Measurement`s are passed to each configured
/// [`Exporter`](crate::exporters::Exporter) to disseminate the measurements.
///
#[derive(Debug)]
pub struct Measurement {
    pub title: String,
    pub units: String,
    pub help: String,
    pub value: Option<f64>,
}

impl fmt::Display for Measurement {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(value) = self.value {
            write!(f, "{}: {:.1?}{}", self.title, value, self.units)
        } else {
            write!(f, "{}: ---{}", self.title, self.units)
        }
    }
}

impl Measurement {
    pub fn new(title: &str, value: Option<f64>, units: &str, help: &str) -> Self {
        if title.is_empty() {
            panic!("Title is required");
        }

        // Empty units are ok

        // Empty help is generally ok, but some exporters require help (eg. Pushgateway)
        let this_help = if help.is_empty() {
            title.to_owned()
        } else {
            help.to_owned()
        };

        Self {
            title: title.to_owned(),
            units: units.to_owned(),
            help: this_help,
            value,
        }
    }

    //
    // Some exporters can choke on strings with spaces (eg. Pushgateway URLs), this method
    // normalizes to lowercase and replaces whitespace characters with underscores.
    //
    fn to_safe(s: &str) -> String {
        s.to_lowercase().replace(char::is_whitespace, "_")
    }

    ///
    /// Returns `title_as_lower()` with a namespace prepended.
    ///
    pub fn title_as_namespaced(&self) -> String {
        format!("ac_{}", Self::to_safe(&self.title))
    }

    ///
    /// Returns the `Measurement`'s title to lowercase and replaces all whitespace with `_`.
    ///
    pub fn title_as_lower(&self) -> String {
        Self::to_safe(&self.title)
    }

    ///
    /// Returns the `Measurement`'s units to lowercase and replaces all whitespace with `_`.
    ///
    pub fn units_as_lower(&self) -> String {
        Self::to_safe(&self.units)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use test_log::test;

    #[test]
    fn poll_result() {
        let er = Measurement::new(
            "Bean Counter",
            Some(56.723456789),
            "shipped cans",
            "Counts the total cans of beans",
        );
        assert_eq!("bean_counter", er.title_as_lower());
        assert_eq!("shipped_cans", er.units_as_lower());
        assert_eq!("Bean Counter: 56.7shipped cans", er.to_string());
    }

    #[test]
    #[should_panic(expected = "Title is required")]
    fn title_is_required() {
        let _ = Measurement::new(
            "",
            Some(56.7),
            "shipped cans",
            "Counts the total cans of beans",
        );
    }

    #[test]
    fn missing_help_defaults_to_title() {
        let er = Measurement::new("Bean Counter", Some(56.7), "shipped cans", "");
        assert_eq!(er.title, er.help);
    }
}
