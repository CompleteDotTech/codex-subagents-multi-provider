use i18n_embed::{
    I18nEmbedError, LanguageLoader, Localizer,
    fluent::{FluentLanguageLoader, fluent_language_loader},
    unic_langid::LanguageIdentifier,
};
use lazy_static::lazy_static;
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "i18n"]
struct Localizations;

lazy_static! {
    pub(crate) static ref LANGUAGE_LOADER: FluentLanguageLoader = {
        let language_loader = fluent_language_loader!();
        // Ensure that the fallback language is always loaded, even if the library user
        // doesn't call `localizer().select(languages)`.
        let fallback: LanguageIdentifier = "en-US".parse().unwrap();
        language_loader.load_languages(&Localizations, &[fallback]).unwrap();
        language_loader
    };
}

/// Loads a localized age string.
#[doc(hidden)]
#[macro_export]
macro_rules! fl {
    ($message_id:literal) => {{
        i18n_embed_fl::fl!($crate::i18n::LANGUAGE_LOADER, $message_id)
    }};

    ($message_id:literal, $($args:expr),* $(,)?) => {{
        i18n_embed_fl::fl!($crate::i18n::LANGUAGE_LOADER, $message_id, $($args), *)
    }};
}

/// age-localized version of the write! macro.
#[doc(hidden)]
#[macro_export]
macro_rules! wfl {
    ($f:ident, $message_id:literal) => {
        write!($f, "{}", $crate::fl!($message_id))
    };

    ($f:ident, $message_id:literal, $($args:expr),* $(,)?) => {
        write!($f, "{}", $crate::fl!($message_id, $($args), *))
    };
}

/// age-localized version of the writeln! macro.
#[doc(hidden)]
#[macro_export]
macro_rules! wlnfl {
    ($f:ident, $message_id:literal) => {
        writeln!($f, "{}", $crate::fl!($message_id))
    };

    ($f:ident, $message_id:literal, $($args:expr),* $(,)?) => {
        writeln!($f, "{}", $crate::fl!($message_id, $($args), *))
    };
}

/// Returns the [`Localizer`] to be used for localizing this library.
///
/// # Examples
///
/// ```
/// // Fetch the set of languages that the user's desktop environment requests.
/// let requested_languages = i18n_embed::DesktopLanguageRequester::requested_languages();
///
/// // Localize the age crate based on the requested languages.
/// //
/// // If none of the requested languages are available, or if this function
/// // is not called, age defaults to en-US.
/// age::localizer().select(&requested_languages).unwrap();
/// ```
pub fn localizer() -> Box<dyn Localizer> {
    Box::from(AgeLocalizer)
}

struct AgeLocalizer;

impl Localizer for AgeLocalizer {
    fn language_loader(&self) -> &'_ dyn LanguageLoader {
        &*LANGUAGE_LOADER
    }

    fn i18n_assets(&self) -> &'_ dyn i18n_embed::I18nAssets {
        &Localizations
    }

    fn select(
        &self,
        requested_languages: &[LanguageIdentifier],
    ) -> Result<Vec<LanguageIdentifier>, I18nEmbedError> {
        let supported_languages =
            i18n_embed::select(&*LANGUAGE_LOADER, &Localizations, requested_languages)?;
        // Unfortunately the common Windows terminals don't support Unicode Directionality
        // Isolation Marks, so we disable them for now.
        LANGUAGE_LOADER.set_use_isolating(false);
        Ok(supported_languages)
    }
}

#[cfg(test)]
mod tests {
    use super::{FluentLanguageLoader, LanguageIdentifier, LanguageLoader, Localizations};
    use i18n_embed::fluent::fluent_language_loader;

    #[test]
    fn localization_resources_and_fallback_are_available() {
        let mut resources: Vec<_> = Localizations::iter()
            .map(|path| path.into_owned())
            .collect();
        resources.sort();
        assert_eq!(
            resources,
            [
                "en-US/age.ftl",
                "es-AR/age.ftl",
                "fr/age.ftl",
                "it/age.ftl",
                "ru/age.ftl",
                "zh-CN/age.ftl",
                "zh-TW/age.ftl",
            ]
        );
        for path in resources {
            let resource = Localizations::get(&path).expect("localization resource is available");
            let text = std::str::from_utf8(resource.data.as_ref()).expect("valid Fluent UTF-8");
            assert!(text.contains("cli-secret-input-required ="), "{path}");
        }

        // Use a fresh loader so this test cannot change the process-wide locale.
        let loader: FluentLanguageLoader = fluent_language_loader!();
        let fallback: LanguageIdentifier = "en-US".parse().unwrap();
        loader.load_languages(&Localizations, &[fallback]).unwrap();
        assert_eq!(loader.get("cli-secret-input-required"), "Input is required");

        // An unavailable requested locale must still resolve through the real fallback.
        let unavailable: LanguageIdentifier = "de-DE".parse().unwrap();
        i18n_embed::select(&loader, &Localizations, &[unavailable]).unwrap();
        assert_eq!(loader.get("cli-secret-input-required"), "Input is required");
    }

    #[test]
    fn french_localization_loads_from_resources() {
        let loader: FluentLanguageLoader = fluent_language_loader!();
        let french: LanguageIdentifier = "fr".parse().unwrap();
        let selected =
            i18n_embed::select(&loader, &Localizations, std::slice::from_ref(&french)).unwrap();
        assert!(selected.contains(&french));
        assert_eq!(loader.get("cli-secret-input-required"), "Entrée requise");
    }
}
