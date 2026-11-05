//! The README's Reference tables list every key the parsers accept, so a new key can't arrive undocumented.

use serde::de::{self, Deserialize, Deserializer, Visitor};
use soundcheck::suite::config::{
    Hooks, Models, PlatformConfig, ProxyRoute, Remote, SuiteConfig, Tickets,
};
use soundcheck::suite::spec::Frontmatter;

/// The field names a derived `Deserialize` asks for, as written in YAML (renames applied).
fn fields<'de, T: Deserialize<'de>>() -> &'static [&'static str] {
    struct Fields(Option<&'static [&'static str]>);

    #[derive(Debug)]
    struct Stop;
    impl std::fmt::Display for Stop {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("stop")
        }
    }
    impl std::error::Error for Stop {}
    impl de::Error for Stop {
        fn custom<M: std::fmt::Display>(_: M) -> Self {
            Stop
        }
    }

    impl<'de> Deserializer<'de> for &mut Fields {
        type Error = Stop;

        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Stop> {
            Err(Stop)
        }

        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            fields: &'static [&'static str],
            _: V,
        ) -> Result<V::Value, Stop> {
            self.0 = Some(fields);
            Err(Stop)
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf option unit
            unit_struct newtype_struct seq tuple tuple_struct map enum identifier ignored_any
        }
    }

    let mut found = Fields(None);
    let _ = T::deserialize(&mut found);
    found.0.expect("a struct with named fields")
}

/// The keys in the first column of the README's tables under `heading`, up to the next `### ` or `## `.
fn listed(readme: &str, heading: &str) -> Vec<String> {
    let start = readme
        .find(heading)
        .unwrap_or_else(|| panic!("the README has no {heading:?}"));
    let section = &readme[start + heading.len()..];
    let end = section
        .find("\n## ")
        .into_iter()
        .chain(section.find("\n### "))
        .min();
    section[..end.unwrap_or(section.len())]
        .lines()
        .filter_map(|line| line.strip_prefix("| "))
        .filter_map(|cell| cell.split('`').nth(1))
        .map(|key| key.trim_start_matches("## ").to_string())
        .collect()
}

fn assert_listed(readme: &str, heading: &str, keys: &[&str]) {
    let listed = listed(readme, heading);
    let missing: Vec<_> = keys
        .iter()
        .filter(|k| !listed.iter().any(|l| l == *k))
        .collect();
    assert!(
        missing.is_empty(),
        "README {heading:?} doesn't list {missing:?}"
    );
}

#[test]
fn the_reference_lists_every_key() {
    let readme = include_str!("../../../README.md");

    assert_listed(readme, "### `suite.yaml`", fields::<SuiteConfig>());
    // Their one key each is described in the suite table's row.
    for nested in [fields::<Models>(), fields::<Tickets>(), fields::<Remote>()] {
        for key in nested {
            assert!(
                readme.contains(&format!("`{key}`")),
                "README doesn't mention `{key}`"
            );
        }
    }
    assert_listed(readme, "### A platform", fields::<PlatformConfig>());
    assert_listed(readme, "### A platform", fields::<ProxyRoute>());
    assert_listed(readme, "### Hooks", fields::<Hooks>());
    assert_listed(readme, "### A spec", fields::<Frontmatter>());
}
