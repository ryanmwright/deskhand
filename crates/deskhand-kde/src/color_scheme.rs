//! Switches Plasma between light and dark with `plasma-apply-colorscheme`,
//! for the portal-based `appearance` integration. Scheme names come from
//! `[integrations.appearance] light_scheme` / `dark_scheme` (default
//! `BreezeLight` / `BreezeDark`; `plasma-apply-colorscheme --list-schemes`
//! lists them).

use deskhand_linux::appearance::{AppearanceOptions, ColorSchemeWriter};

use crate::run_tool;

pub const DEFAULT_LIGHT: &str = "BreezeLight";
pub const DEFAULT_DARK: &str = "BreezeDark";

/// `plasma-apply-colorscheme`.
#[derive(Debug, Clone)]
pub struct KdeColorScheme {
    program: String,
}

impl Default for KdeColorScheme {
    fn default() -> Self {
        Self {
            program: "plasma-apply-colorscheme".into(),
        }
    }
}

impl KdeColorScheme {
    pub fn scheme(dark: bool, options: &AppearanceOptions) -> &str {
        match dark {
            true => options.dark_scheme.as_deref().unwrap_or(DEFAULT_DARK),
            false => options.light_scheme.as_deref().unwrap_or(DEFAULT_LIGHT),
        }
    }
}

#[async_trait::async_trait]
impl ColorSchemeWriter for KdeColorScheme {
    async fn set_dark(&self, dark: bool, options: &AppearanceOptions) -> Result<(), String> {
        run_tool(&self.program, &[Self::scheme(dark, options)]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_names() {
        let mut o = AppearanceOptions::default();
        assert_eq!(KdeColorScheme::scheme(true, &o), "BreezeDark");
        assert_eq!(KdeColorScheme::scheme(false, &o), "BreezeLight");
        o.dark_scheme = Some("Nordic".into());
        assert_eq!(KdeColorScheme::scheme(true, &o), "Nordic");
    }
}
