//! Embedded startup-only Fluent presentation resources.
use fluent_bundle::{FluentArgs, FluentResource, FluentValue, concurrent::FluentBundle};
use std::collections::HashMap;

const EN: &str = concat!(
    include_str!("../data/locales/en-US/core.ftl"),
    "\n",
    include_str!("../data/locales/en-US/gui-shell.ftl"),
    "\n",
    include_str!("../data/locales/en-US/gui-settings.ftl"),
    "\n",
    include_str!("../data/locales/en-US/gui-library.ftl"),
    "\n",
    include_str!("../data/locales/en-US/terminal.ftl"),
    "\n",
);
const ZH: &str = concat!(
    include_str!("../data/locales/zh-CN/core.ftl"),
    "\n",
    include_str!("../data/locales/zh-CN/gui-shell.ftl"),
    "\n",
    include_str!("../data/locales/zh-CN/gui-settings.ftl"),
    "\n",
    include_str!("../data/locales/zh-CN/gui-library.ftl"),
    "\n",
    include_str!("../data/locales/zh-CN/terminal.ftl"),
    "\n",
);

pub use rivu_format::config::normalize_language;

fn environment_language<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> &'static str {
    let value = values
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty())
        .unwrap_or("en-US");
    let tag = value
        .split(['.', '@'])
        .next()
        .unwrap_or("")
        .replace('_', "-")
        .to_ascii_lowercase();
    if tag == "zh"
        || tag == "zh-cn"
        || tag == "zh-sg"
        || tag == "zh-hans"
        || tag.starts_with("zh-hans-")
    {
        "zh-CN"
    } else {
        "en-US"
    }
}

/// A presentation owner's immutable startup language. Fixed messages are resolved
/// once; render loops borrow them without formatting or allocating.
pub struct Localizer {
    language: &'static str,
    bundle: FluentBundle<FluentResource>,
    fixed: HashMap<String, String>,
}

impl Localizer {
    pub fn new(language: &str) -> Self {
        let language = match normalize_language(language) {
            "auto" => {
                let values = ["LC_ALL", "LC_MESSAGES", "LANG"].map(|key| std::env::var(key).ok());
                environment_language(values.iter().map(|value| value.as_deref()))
            }
            "zh-CN" => "zh-CN",
            _ => "en-US",
        };
        Self::for_language(language)
    }

    fn for_language(language: &'static str) -> Self {
        Self::with_resources(language, EN, (language == "zh-CN").then_some(ZH))
    }

    fn with_resources(language: &'static str, english: &str, translated: Option<&str>) -> Self {
        let resource = FluentResource::try_new(english.to_owned())
            .unwrap_or_else(|(_, errors)| panic!("invalid embedded Fluent resource: {errors:?}"));
        let ids = resource
            .entries()
            .filter_map(|entry| match entry {
                fluent_syntax::ast::Entry::Message(message) => Some(message.id.name.to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut bundle =
            FluentBundle::new_concurrent(vec![language.parse().expect("supported language tag")]);
        bundle
            .add_resource(resource)
            .expect("unique embedded Fluent message IDs");
        if let Some(source) = translated {
            let translated =
                FluentResource::try_new(source.to_owned()).unwrap_or_else(|(_, errors)| {
                    panic!("invalid embedded Fluent resource: {errors:?}")
                });
            bundle.add_resource_overriding(translated);
        }
        let mut fixed = HashMap::new();
        for entry in ids {
            let message = bundle.get_message(&entry).expect("embedded message");
            if let Some(pattern) = message.value() {
                let mut errors = Vec::new();
                let value = bundle.format_pattern(pattern, None, &mut errors);
                if errors.is_empty() {
                    fixed.insert(entry, value.into_owned());
                }
            }
        }
        Self {
            language,
            bundle,
            fixed,
        }
    }

    pub fn language(&self) -> &'static str {
        self.language
    }

    /// Fixed startup messages for presentation-specific immutable string caches.
    pub fn fixed_texts(&self) -> impl Iterator<Item = (&str, &str)> {
        self.fixed
            .iter()
            .map(|(id, text)| (id.as_str(), text.as_str()))
    }

    /// Borrow a fixed message. Unknown IDs and dynamic messages are programming errors.
    pub fn text(&self, id: &str) -> &str {
        self.fixed
            .get(id)
            .unwrap_or_else(|| panic!("unknown or dynamic Fluent text ID: {id}"))
    }

    /// Format named arguments. Missing/incorrect arguments are programming errors,
    /// never silently replaced with an untranslated message or raw Fluent syntax.
    pub fn format(&self, id: &str, args: &[(&str, FluentValue<'_>)]) -> String {
        let message = self
            .bundle
            .get_message(id)
            .unwrap_or_else(|| panic!("unknown Fluent ID: {id}"));
        let pattern = message.value().expect("Fluent message must have a value");
        let mut named = FluentArgs::with_capacity(args.len());
        for (name, value) in args {
            named.set(*name, value.clone());
        }
        let mut errors = Vec::new();
        let text = self
            .bundle
            .format_pattern(pattern, Some(&named), &mut errors);
        assert!(
            errors.is_empty(),
            "invalid arguments for Fluent ID {id}: {errors:?}"
        );
        text.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluent_syntax::ast::{self, Entry, Expression, InlineExpression, PatternElement};
    use std::collections::{BTreeMap, BTreeSet};

    fn variables(pattern: &ast::Pattern<&str>, names: &mut BTreeSet<String>) {
        for element in &pattern.elements {
            if let PatternElement::Placeable { expression } = element {
                match expression {
                    Expression::Inline(InlineExpression::VariableReference { id }) => {
                        names.insert(id.name.to_owned());
                    }
                    Expression::Select { selector, variants } => {
                        if let InlineExpression::VariableReference { id } = selector {
                            names.insert(id.name.to_owned());
                        }
                        for variant in variants {
                            variables(&variant.value, names);
                        }
                    }
                    Expression::Inline(InlineExpression::Placeable { expression }) => {
                        if let Expression::Inline(InlineExpression::VariableReference { id }) =
                            expression.as_ref()
                        {
                            names.insert(id.name.to_owned());
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn signatures(source: &str) -> BTreeMap<String, BTreeSet<String>> {
        let resource = fluent_syntax::parser::parse(source).expect("valid Fluent syntax");
        let mut result = BTreeMap::new();
        for entry in resource.body {
            if let Entry::Message(message) = entry {
                assert!(
                    message.attributes.is_empty(),
                    "use named messages, not hidden attributes"
                );
                let mut names = BTreeSet::new();
                variables(&message.value.expect("message value"), &mut names);
                assert!(
                    result.insert(message.id.name.to_owned(), names).is_none(),
                    "duplicate message ID"
                );
            }
        }
        result
    }

    #[test]
    fn language_normalization_and_environment_precedence_are_deterministic() {
        assert_eq!(normalize_language("ZH_cn"), "zh-CN");
        assert_eq!(normalize_language("EN_us"), "en-US");
        assert_eq!(normalize_language("AUTO"), "auto");
        assert_eq!(normalize_language("fr-FR"), "fr-FR");
        assert_eq!(
            environment_language([Some(""), Some("zh_CN.UTF-8"), Some("en_US")]),
            "zh-CN"
        );
        assert_eq!(
            environment_language([Some("C"), Some("zh_CN"), None]),
            "en-US"
        );
        assert_eq!(
            environment_language([None, None, Some("zh-Hans@variant")]),
            "zh-CN"
        );
        for tag in ["zh-TW", "zh-HK", "zh-MO", "zh-Hant", "zh-Hant-CN"] {
            assert_eq!(environment_language([Some(tag)]), "en-US");
        }
        assert_eq!(environment_language([None, None, None]), "en-US");
        assert_eq!(Localizer::new("unsupported").language(), "en-US");
    }

    #[test]
    fn embedded_catalogs_are_complete_and_format_without_errors() {
        let english = signatures(EN);
        assert_eq!(
            english,
            signatures(ZH),
            "both languages need identical IDs and named arguments"
        );
        for language in ["en-US", "zh-CN"] {
            let localizer = Localizer::new(language);
            for (id, names) in &english {
                let args = names
                    .iter()
                    .map(|name| (name.as_str(), FluentValue::from(2)))
                    .collect::<Vec<_>>();
                let rendered = localizer.format(id, &args);
                if names.is_empty() {
                    assert_eq!(localizer.text(id), rendered);
                }
            }
        }
    }

    #[test]
    #[should_panic(expected = "invalid arguments")]
    fn missing_named_arguments_are_not_silently_accepted() {
        Localizer::new("en-US").format("core-command-accepted", &[]);
    }

    #[test]
    fn named_arguments_preserve_user_text() {
        let localizer = Localizer::with_resources("zh-CN", "value = { $path }\n", None);
        let path = "My 音乐/{name}";
        assert_eq!(localizer.format("value", &[("path", path.into())]), path);
    }

    #[test]
    fn missing_translation_uses_english_per_message() {
        let localizer = Localizer::with_resources(
            "zh-CN",
            "fixed = English\ndynamic = Hello { $name }\n",
            Some("fixed = 中文\n"),
        );
        assert_eq!(localizer.text("fixed"), "中文");
        assert_eq!(
            localizer.format("dynamic", &[("name", "Ada".into())]),
            "Hello \u{2068}Ada\u{2069}"
        );
    }

    #[test]
    fn localizer_can_be_shared_by_presentation_workers() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Localizer>();
    }
}
