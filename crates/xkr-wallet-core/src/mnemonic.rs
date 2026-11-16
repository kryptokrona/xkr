//! 25-word mnemonic seeds (`src/mnemonics/mnemonics.cpp`, `namespace Mnemonics`).
//!
//! A seed encodes a private spend key only; the view key of a seed wallet is derived from it
//! ([`crate::keys::view_key_from_spend`]). The 32 key bytes are read as eight little-endian
//! `u32` values and each becomes three words of the 1626-word English list ([`english`]); the
//! 25th word is a checksum: `CRC32` over the first three characters of each of the 24 words,
//! modulo 24, picks one of the 24 words (`GetChecksumWord`).
//!
//! Parsing follows `Mnemonics::MnemonicToPrivateKey`: the word count is checked first, then
//! that every word is in the list, then the checksum, then that every word triple is a valid
//! encoding. The errors carry the C++ error codes and messages (`src/errors/errors.cpp`).
//! Words are split on ASCII whitespace (`std::istringstream >> word`) and compared in lower
//! case, as wallet-backend-js does (`lib/Utilities.ts` `isValidMnemonic`).

pub mod english;

use xkr_types::SecretKey;

use self::english::{ENGLISH, ENGLISH_LEN};

/// Words in a seed (`24` data words and one checksum word).
pub const WORD_COUNT: usize = 25;

/// Data words in a seed (three per 32-bit chunk of the key).
const DATA_WORDS: usize = WORD_COUNT - 1;

/// [`ENGLISH_LEN`] as a `u64` operand.
const N: u64 = ENGLISH_LEN as u64;

/// Why a seed does not parse. The messages are the C++ ones; [`MnemonicError::code`] is the
/// C++ `WalletErrorCode`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MnemonicError {
    /// `MNEMONIC_WRONG_LENGTH` (34): not 25 words.
    WrongLength(usize),
    /// `MNEMONIC_INVALID_WORD` (33): a word that is not in the English list (lower case).
    InvalidWord(String),
    /// `MNEMONIC_INVALID_CHECKSUM` (35): the 25th word does not match.
    InvalidChecksum,
    /// `INVALID_MNEMONIC` (7): a word triple that does not encode a 32-bit value.
    Invalid,
}

impl std::fmt::Display for MnemonicError {
    /// The messages of `Mnemonics::MnemonicToPrivateKey` (custom messages) and
    /// `Error::getErrorMessage`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongLength(len) => write!(
                f,
                "The mnemonic seed given is the wrong length. It should be 25 words long, but \
                 it is {len} {} long.",
                if *len == 1 { "word" } else { "words" }
            ),
            Self::InvalidWord(word) => write!(
                f,
                "The mnemonic seed given has a word that is not present in the english word \
                 list ({word})."
            ),
            Self::InvalidChecksum => {
                f.write_str("The mnemonic seed given has an invalid checksum word.")
            }
            Self::Invalid => f.write_str("The mnemonic seed given is invalid."),
        }
    }
}

impl std::error::Error for MnemonicError {}

impl MnemonicError {
    /// The C++ error code (`src/errors/errors.h` `WalletErrorCode`), as wallet-api returns it
    /// in `errorCode`.
    #[must_use]
    pub const fn code(&self) -> u16 {
        match self {
            Self::WrongLength(_) => 34,
            Self::InvalidWord(_) => 33,
            Self::InvalidChecksum => 35,
            Self::Invalid => 7,
        }
    }
}

/// `CRC32::crc32` (`src/mnemonics/crc32.h`): the reflected IEEE CRC-32 (polynomial
/// `0xEDB88320`, initial value and final XOR `0xFFFFFFFF`).
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    crc ^ 0xFFFF_FFFF
}

/// Index of `word` in the English list (`GetWordIndexes`); the list is sorted.
#[must_use]
pub fn word_index(word: &str) -> Option<usize> {
    ENGLISH.binary_search(&word).ok()
}

/// Whether `word` (exactly, lower case) is in the English list.
#[must_use]
pub fn is_valid_word(word: &str) -> bool {
    word_index(word).is_some()
}

/// `Mnemonics::GetChecksumWord`: CRC32 over the first three characters of every word, modulo
/// the number of words, picks one of `words`. `None` for an empty slice.
#[must_use]
pub fn checksum_word<'a>(words: &[&'a str]) -> Option<&'a str> {
    if words.is_empty() {
        return None;
    }
    let mut trimmed = Vec::with_capacity(words.len() * 3);
    for word in words {
        let bytes = word.as_bytes();
        trimmed.extend_from_slice(&bytes[..bytes.len().min(3)]);
    }
    let index = u64::from(crc32(&trimmed)) % words.len() as u64;
    Some(words[index as usize])
}

/// `Mnemonics::PrivateKeyToMnemonic`: the 25-word seed of a private spend key.
#[must_use]
pub fn mnemonic_from_secret(secret: &SecretKey) -> String {
    let mut words: Vec<&'static str> = Vec::with_capacity(WORD_COUNT);
    for chunk in secret.0.as_chunks::<4>().0 {
        let val = u64::from(u32::from_le_bytes(*chunk));
        let w1 = val % N;
        let w2 = (val / N + w1) % N;
        let w3 = (val / N / N + w2) % N;
        words.push(ENGLISH[w1 as usize]);
        words.push(ENGLISH[w2 as usize]);
        words.push(ENGLISH[w3 as usize]);
    }
    let checksum = checksum_word(&words).unwrap_or_default();
    words.push(checksum);
    words.join(" ")
}

/// `Mnemonics::MnemonicToPrivateKey(std::string)`: the private spend key of a seed. Words are
/// separated by ASCII whitespace (space, tab, line feed, vertical tab, form feed, carriage
/// return) and compared in lower case.
pub fn secret_from_mnemonic(mnemonic: &str) -> Result<SecretKey, MnemonicError> {
    let words: Vec<&str> = mnemonic
        .split([' ', '\t', '\n', '\x0B', '\x0C', '\r'])
        .filter(|w| !w.is_empty())
        .collect();
    secret_from_words(&words)
}

/// `Mnemonics::MnemonicToPrivateKey(std::vector<std::string>)`: the private spend key of a
/// seed given as words.
pub fn secret_from_words<S: AsRef<str>>(words: &[S]) -> Result<SecretKey, MnemonicError> {
    if words.len() != WORD_COUNT {
        return Err(MnemonicError::WrongLength(words.len()));
    }
    let lower: Vec<String> = words
        .iter()
        .map(|w| w.as_ref().to_ascii_lowercase())
        .collect();
    let mut indexes = [0u64; WORD_COUNT];
    for (slot, word) in indexes.iter_mut().zip(&lower) {
        *slot = word_index(word).ok_or_else(|| MnemonicError::InvalidWord(word.clone()))? as u64;
    }
    let data: Vec<&str> = lower[..DATA_WORDS].iter().map(String::as_str).collect();
    if checksum_word(&data) != Some(lower[DATA_WORDS].as_str()) {
        return Err(MnemonicError::InvalidChecksum);
    }
    let mut key = [0u8; 32];
    for (triple, out) in indexes[..DATA_WORDS]
        .as_chunks::<3>()
        .0
        .iter()
        .zip(key.as_chunks_mut::<4>().0)
    {
        let (w1, w2, w3) = (triple[0], triple[1], triple[2]);
        // Computed in `size_t` and stored as `uint32_t`, as C++ does; the check is on the stored
        // value.
        let wide = w1 + N * ((N - w1 + w2) % N) + N * N * ((N - w2 + w3) % N);
        let val = wide as u32;
        if u64::from(val) % N != w1 {
            return Err(MnemonicError::Invalid);
        }
        out.copy_from_slice(&val.to_le_bytes());
    }
    Ok(SecretKey(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::WalletKeys;

    /// The seed of the wallet-backend-js tests (`tests/tests.ts`, "Verifying seed restore works
    /// correctly"), with its private spend and view keys and its address.
    const JS_SEED: &str = "skulls woozy ouch summon gifts huts waffle ourselves obtains hexagon \
                           tadpoles hacksaw dormant hence abort listen history atom cadets \
                           stylishly snout vegan girth guest history";
    const JS_SPEND: &str = "d61a57a59318d70ff77cc7f8ad7f62887c828da1d5d3f3b0d2f7d3fa596c2904";
    const JS_VIEW: &str = "688e5229df6463ec4c27f6ee11c3f1d3d4b4d2480c0aabe64fb807182cfdc801";
    const JS_ADDRESS: &str = "SEKReX4MYn2HAJFoHvcqVBPyHYom2ynKeK6dpYptbp8gQNzdzE73ZDkNmNurqfhhcMSUXpS1ZGEJKiKJUcPCyw7vYaCc35yBZCf";

    /// A second valid seed of the wallet-backend-js tests (`isValidMnemonic`).
    const JS_SEED_2: &str = "nugget lazy gang sonic vulture exit veteran poverty affair ringing \
                             opus soapy sonic afield dating lectures worry tuxedo ruffled rated \
                             locker bested aunt bifocals opus";

    fn words(seed: &str) -> Vec<&str> {
        seed.split_whitespace().collect()
    }

    #[test]
    fn word_list_is_sorted_unique_and_has_unique_prefixes() {
        assert_eq!(ENGLISH.len(), 1626);
        assert!(ENGLISH.windows(2).all(|w| w[0] < w[1]));
        let mut prefixes: Vec<&str> = ENGLISH.iter().map(|w| &w[..3]).collect();
        prefixes.dedup();
        assert_eq!(prefixes.len(), ENGLISH.len());
        assert_eq!(word_index("abbey"), Some(0));
        assert_eq!(word_index("zoom"), Some(1625));
        assert!(!is_valid_word("aaaaa"));
    }

    #[test]
    fn crc32_matches_the_reference_check_value() {
        // The standard CRC-32 check value and the empty input.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn wallet_backend_js_seed() {
        let secret: SecretKey = JS_SPEND.parse().unwrap();
        assert_eq!(secret_from_mnemonic(JS_SEED), Ok(secret));
        assert_eq!(mnemonic_from_secret(&secret), words(JS_SEED).join(" "));
        let keys = WalletKeys::from_spend_secret(&secret).unwrap();
        assert_eq!(keys.view_secret, JS_VIEW.parse().unwrap());
        assert_eq!(keys.address().to_string(), JS_ADDRESS);
    }

    #[test]
    fn round_trips() {
        let secret = secret_from_mnemonic(JS_SEED_2).unwrap();
        assert_eq!(mnemonic_from_secret(&secret), words(JS_SEED_2).join(" "));
        let mut rng = xkr_crypto::os_rng();
        for _ in 0..64 {
            let keys = WalletKeys::generate(&mut rng);
            let seed = mnemonic_from_secret(&keys.spend_secret);
            assert_eq!(seed.split(' ').count(), WORD_COUNT);
            assert_eq!(secret_from_mnemonic(&seed), Ok(keys.spend_secret));
        }
        // Every 32-bit chunk value at the edges.
        for bytes in [[0u8; 32], [0xff; 32]] {
            let secret = SecretKey(bytes);
            assert_eq!(
                secret_from_mnemonic(&mnemonic_from_secret(&secret)),
                Ok(secret)
            );
        }
    }

    #[test]
    fn whitespace_and_case() {
        let secret: SecretKey = JS_SPEND.parse().unwrap();
        let spaced = format!("  {}\n", words(JS_SEED).join("\t \r\n"));
        assert_eq!(secret_from_mnemonic(&spaced), Ok(secret));
        assert_eq!(secret_from_mnemonic(&JS_SEED.to_uppercase()), Ok(secret));
        assert_eq!(secret_from_words(&words(JS_SEED)), Ok(secret));
    }

    #[test]
    fn errors_match_the_cpp_messages() {
        // checksum: the second JS seed with "soapy" as the last word
        let bad_checksum = format!("{} soapy", words(JS_SEED_2)[..24].join(" "));
        let e = secret_from_mnemonic(&bad_checksum).unwrap_err();
        assert_eq!(e, MnemonicError::InvalidChecksum);
        assert_eq!(e.code(), 35);
        assert_eq!(
            e.to_string(),
            "The mnemonic seed given has an invalid checksum word."
        );

        // unknown word: "a" in place of "nugget"
        let unknown = format!("a {}", words(JS_SEED_2)[1..].join(" "));
        let e = secret_from_mnemonic(&unknown).unwrap_err();
        assert_eq!(e, MnemonicError::InvalidWord("a".to_owned()));
        assert_eq!(e.code(), 33);
        assert_eq!(
            e.to_string(),
            "The mnemonic seed given has a word that is not present in the english word list (a)."
        );
        // the message shows the word in lower case
        let shouted = format!("AAAAA {}", words(JS_SEED_2)[1..].join(" "));
        assert_eq!(
            secret_from_mnemonic(&shouted),
            Err(MnemonicError::InvalidWord("aaaaa".to_owned()))
        );

        // word count
        let short = words(JS_SEED)[..24].join(" ");
        let e = secret_from_mnemonic(&short).unwrap_err();
        assert_eq!(e, MnemonicError::WrongLength(24));
        assert_eq!(e.code(), 34);
        assert_eq!(
            e.to_string(),
            "The mnemonic seed given is the wrong length. It should be 25 words long, but it is 24 words long."
        );
        assert_eq!(
            secret_from_mnemonic("abbey").unwrap_err().to_string(),
            "The mnemonic seed given is the wrong length. It should be 25 words long, but it is 1 word long."
        );
        assert_eq!(secret_from_mnemonic(""), Err(MnemonicError::WrongLength(0)));
        // the length is checked before the words
        assert_eq!(
            secret_from_mnemonic("a b c"),
            Err(MnemonicError::WrongLength(3))
        );

        // A triple that encodes no 32-bit value: "abbey abbey zoom" (indexes 0, 0, 1625) is
        // 0 + 1626 * 0 + 1626^2 * 1625 = 4 296 298 500 > 2^32; stored as `uint32_t` it is
        // 1 331 204, and 1 331 204 % 1626 = 1136 != 0.
        let mut triple_words: Vec<&str> = vec!["abbey", "abbey", "zoom"];
        triple_words.extend(std::iter::repeat_n("abbey", 21));
        let checksum = checksum_word(&triple_words).unwrap();
        triple_words.push(checksum);
        let e = secret_from_words(&triple_words).unwrap_err();
        assert_eq!(e, MnemonicError::Invalid);
        assert_eq!(e.code(), 7);
        assert_eq!(e.to_string(), "The mnemonic seed given is invalid.");
    }
}
