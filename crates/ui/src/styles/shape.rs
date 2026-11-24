use gpui::{App, Pixels, SharedString, px};

/// Corner radius shared by buttons, selected rows and inputs.
pub const CORNER_RADIUS: Pixels = px(6.);

/// Minimum height of a list row. Pixels, not rems, so it does not follow the
/// UI font size.
pub const LIST_ROW_HEIGHT: Pixels = px(28.);

/// The font family used by monospace labels (key hints, counts): the buffer
/// font family.
pub fn mono_label_font(cx: &App) -> SharedString {
    theme::theme_settings(cx).buffer_font(cx).family.clone()
}

#[cfg(test)]
pub(crate) mod test_support {
    use gpui::{App, Font, Pixels, px};
    use theme::{LoadThemes, ThemeSettingsProvider, UiDensity};

    pub(crate) struct TestThemeSettings {
        pub ui_font: Font,
        pub buffer_font: Font,
    }

    impl ThemeSettingsProvider for TestThemeSettings {
        fn ui_font<'a>(&'a self, _cx: &'a App) -> &'a Font {
            &self.ui_font
        }

        fn buffer_font<'a>(&'a self, _cx: &'a App) -> &'a Font {
            &self.buffer_font
        }

        fn ui_font_size(&self, _cx: &App) -> Pixels {
            px(14.)
        }

        fn buffer_font_size(&self, _cx: &App) -> Pixels {
            px(15.)
        }

        fn ui_density(&self, _cx: &App) -> UiDensity {
            UiDensity::default()
        }
    }

    /// Installs the base theme and a settings provider with distinct UI and
    /// buffer families.
    pub(crate) fn init_test_theme(cx: &mut App) {
        theme::init(LoadThemes::JustBase, cx);
        theme::set_theme_settings_provider(
            Box::new(TestThemeSettings {
                ui_font: gpui::font("Test Sans"),
                buffer_font: gpui::font("Test Mono"),
            }),
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    #[gpui::test]
    fn mono_label_font_is_the_buffer_family(cx: &mut TestAppContext) {
        cx.update(|cx| {
            test_support::init_test_theme(cx);
            assert_eq!(mono_label_font(cx), SharedString::from("Test Mono"));
            assert_ne!(
                mono_label_font(cx),
                theme::theme_settings(cx).ui_font(cx).family
            );
        });
    }

    #[test]
    fn shared_shape_constants() {
        assert_eq!(CORNER_RADIUS, px(6.));
        assert_eq!(LIST_ROW_HEIGHT, px(28.));
    }
}
