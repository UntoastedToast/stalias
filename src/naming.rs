//! How the names of randomly generated aliases look. Nothing here knows about Stalwart, the
//! database or the web layer.

use std::sync::LazyLock;

use petname::Petnames;
use rand::seq::IndexedRandom;

/// The small English word lists of the `petname` crate: about 260 adverbs, 450 adjectives and
/// 450 nouns. They hold common words, so names stay short and easy to read.
static PETNAMES: LazyLock<Petnames<'static>> = LazyLock::new(Petnames::small);

const CHARACTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
const CHARACTERS_LEN: u32 = 10;
const WORDS_PER_NAME: u8 = 3;

/// The style of a generated local part.
///
/// The text form (`random_words`, `random_characters`) is shared by the database, the settings
/// form and the `format` field of the Addy.io API.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NameFormat {
    /// An adverb, an adjective and a noun, like `gently-brave-otter`.
    #[default]
    Words,
    /// Ten letters and digits, like `4gh9nzibm3`.
    Characters,
}

impl NameFormat {
    /// Reads the text form. Anything unknown is `None`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "random_words" => Some(Self::Words),
            "random_characters" => Some(Self::Characters),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Words => "random_words",
            Self::Characters => "random_characters",
        }
    }

    /// A new random local part in this style.
    pub fn generate(self) -> String {
        let mut rng = rand::rng();
        match self {
            Self::Words => PETNAMES
                .namer(WORDS_PER_NAME, "-")
                .iter(&mut rng)
                .next()
                .expect("the word lists are not empty"),
            Self::Characters => (0..CHARACTERS_LEN)
                .map(|_| *CHARACTERS.choose(&mut rng).expect("non-empty") as char)
                .collect(),
        }
    }

    /// How many different names this style can produce.
    pub fn combinations(self) -> u128 {
        match self {
            Self::Words => PETNAMES.cardinality(WORDS_PER_NAME),
            Self::Characters => (CHARACTERS.len() as u128).pow(CHARACTERS_LEN),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_word(word: &str) -> bool {
        !word.is_empty() && word.bytes().all(|c| c.is_ascii_lowercase())
    }

    #[test]
    fn words_are_an_adverb_an_adjective_and_a_noun() {
        for _ in 0..200 {
            let name = NameFormat::Words.generate();
            let parts: Vec<&str> = name.split('-').collect();
            assert_eq!(parts.len(), usize::from(WORDS_PER_NAME), "{name}");
            assert!(parts.iter().all(|p| is_word(p)), "{name}");
            assert!(PETNAMES.adverbs.contains(&parts[0]), "{name}");
            assert!(PETNAMES.adjectives.contains(&parts[1]), "{name}");
            assert!(PETNAMES.nouns.contains(&parts[2]), "{name}");
        }
    }

    #[test]
    fn characters_are_ten_letters_and_digits() {
        let (a, b) = (
            NameFormat::Characters.generate(),
            NameFormat::Characters.generate(),
        );
        assert_eq!(a.len(), CHARACTERS_LEN as usize);
        assert!(a.bytes().all(|c| CHARACTERS.contains(&c)));
        assert_ne!(a, b);
    }

    #[test]
    fn the_word_lists_are_clean_and_fit_into_a_local_part() {
        let lists = [&PETNAMES.adverbs, &PETNAMES.adjectives, &PETNAMES.nouns];
        for list in lists {
            assert!(list.iter().all(|w| is_word(w)));
        }
        let longest: usize = lists
            .iter()
            .map(|l| l.iter().map(|w| w.len()).max().unwrap())
            .sum();
        // The longest of each kind plus two hyphens; local parts may be 64 bytes long.
        assert!(longest + 2 <= 64, "{longest}");
    }

    #[test]
    fn combinations_match_the_lists() {
        let expected = PETNAMES.adverbs.len() * PETNAMES.adjectives.len() * PETNAMES.nouns.len();
        assert_eq!(NameFormat::Words.combinations(), expected as u128);
        assert!(NameFormat::Words.combinations() > 10_000_000);
        assert_eq!(NameFormat::Characters.combinations(), 36u128.pow(10));
    }

    #[test]
    fn text_form_round_trips_and_unknown_values_are_none() {
        for format in [NameFormat::Words, NameFormat::Characters] {
            assert_eq!(NameFormat::parse(format.as_str()), Some(format));
        }
        assert_eq!(NameFormat::parse("uuid"), None);
        assert_eq!(NameFormat::parse(""), None);
        assert_eq!(NameFormat::default(), NameFormat::Words);
    }
}
