//! The address book of the terminal wallets, `.addressBook.json` in the working directory
//! (`src/config/wallet_config.h` `addressBookFilename`). Two C++ tools write it, in two forms:
//!
//! * [`AddressBookForm::ZedwalletPlusPlus`]: zedwallet++ (xkrwallet-beta),
//!   `src/zedwallet++/address_book.cpp` `saveAddressBook` / `to_json` / `from_json`. nlohmann
//!   JSON with `std::setw(4)`: four-space indent, keys sorted (`std::map`), fields
//!   `address`, `friendlyName`, `paymentID`, and a trailing newline from `std::endl`.
//! * [`AddressBookForm::Zedwallet`]: zedwallet (xkrwallet), `src/zedwallet/address_book.cpp`
//!   `saveAddressBook` with `AddressBookEntry::serialize` (`src/zedwallet/types.h`) through
//!   `cryptonote::storeToJson`. `common::JsonValue` output: compact, keys sorted (`std::map`),
//!   fields `address`, `friendlyName`, `integratedAddress`, `paymentID`, no trailing newline.
//!
//! [`AddressBook::from_json`] recognises the form and [`AddressBook::to_json`] writes the book
//! back in the same form, byte for byte for the files these tools write.
//!
//! Strings are written with JSON escapes. C++ `JsonValue` writes strings without escaping, so
//! the two differ only for a name, address or payment ID containing `"`, `\` or a control
//! character.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::ser::{PrettyFormatter, Serializer};

use crate::FileError;
use crate::atomic::atomic_write;

/// C++ `wallet_config::addressBookFilename`.
pub const ADDRESS_BOOK_FILENAME: &str = ".addressBook.json";

/// Which tool's layout an address book file uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AddressBookForm {
    /// zedwallet++ / xkrwallet-beta: indented, no `integratedAddress`, trailing newline.
    #[default]
    ZedwalletPlusPlus,
    /// zedwallet / xkrwallet: compact, with `integratedAddress`, no trailing newline.
    Zedwallet,
}

/// One entry (C++ `AddressBookEntry` of either tool).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddressBookEntry {
    /// The address (may be an integrated address).
    pub address: String,
    /// The name the user gave the entry; entries are compared by this name only.
    pub friendly_name: String,
    /// Payment ID to send with, empty if none.
    pub payment_id: String,
    /// zedwallet's `integratedAddress`: the user entered an integrated address, so it is shown
    /// as one. Written only in the [`AddressBookForm::Zedwallet`] form; false when read from a
    /// zedwallet++ file.
    pub integrated_address: bool,
}

/// The address book: entries in file order and the form to write them in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddressBook {
    /// The layout the book was read in and will be written in.
    pub form: AddressBookForm,
    /// The entries.
    pub entries: Vec<AddressBookEntry>,
}

/// An entry as read from either form. Field order is the sorted order both tools write.
#[derive(Deserialize)]
struct RawEntry {
    address: String,
    #[serde(rename = "friendlyName")]
    friendly_name: String,
    #[serde(rename = "integratedAddress")]
    integrated_address: Option<bool>,
    #[serde(rename = "paymentID")]
    payment_id: String,
}

/// zedwallet++ entry (`to_json`).
#[derive(Serialize)]
struct PlusPlusEntry<'a> {
    address: &'a str,
    #[serde(rename = "friendlyName")]
    friendly_name: &'a str,
    #[serde(rename = "paymentID")]
    payment_id: &'a str,
}

/// zedwallet entry (`AddressBookEntry::serialize`, keys in `std::map` order).
#[derive(Serialize)]
struct ZedwalletEntry<'a> {
    address: &'a str,
    #[serde(rename = "friendlyName")]
    friendly_name: &'a str,
    #[serde(rename = "integratedAddress")]
    integrated_address: bool,
    #[serde(rename = "paymentID")]
    payment_id: &'a str,
}

impl AddressBook {
    /// An empty book that will be written in `form`.
    #[must_use]
    pub fn new(form: AddressBookForm) -> Self {
        Self {
            form,
            entries: Vec::new(),
        }
    }

    /// Parses the file contents of either form. The form is
    /// [`AddressBookForm::Zedwallet`] if any entry has `integratedAddress`, or if the book is
    /// empty and the text has no trailing newline (zedwallet writes `[]`, zedwallet++ `[]\n`);
    /// otherwise [`AddressBookForm::ZedwalletPlusPlus`].
    ///
    /// A missing `address`, `friendlyName` or `paymentID`, or a value of the wrong type, is
    /// [`FileError::WalletFileCorrupted`] (zedwallet++ `j.at(...).get<std::string>()` throws).
    pub fn from_json(bytes: &[u8]) -> Result<Self, FileError> {
        let raw: Vec<RawEntry> =
            serde_json::from_slice(bytes).map_err(|e| FileError::corrupted(e.to_string()))?;
        let has_flag = raw.iter().any(|e| e.integrated_address.is_some());
        let form = if has_flag || (raw.is_empty() && !bytes.ends_with(b"\n")) {
            AddressBookForm::Zedwallet
        } else {
            AddressBookForm::ZedwalletPlusPlus
        };
        let entries = raw
            .into_iter()
            .map(|e| AddressBookEntry {
                address: e.address,
                friendly_name: e.friendly_name,
                payment_id: e.payment_id,
                integrated_address: e.integrated_address.unwrap_or(false),
            })
            .collect();
        Ok(Self { form, entries })
    }

    /// The file contents as the C++ tool of [`AddressBook::form`] writes them.
    #[must_use]
    pub fn to_json(&self) -> Vec<u8> {
        match self.form {
            AddressBookForm::ZedwalletPlusPlus => {
                let entries: Vec<PlusPlusEntry<'_>> = self
                    .entries
                    .iter()
                    .map(|e| PlusPlusEntry {
                        address: &e.address,
                        friendly_name: &e.friendly_name,
                        payment_id: &e.payment_id,
                    })
                    .collect();
                let mut out = Vec::new();
                let mut ser =
                    Serializer::with_formatter(&mut out, PrettyFormatter::with_indent(b"    "));
                entries.serialize(&mut ser).expect("strings only");
                out.push(b'\n');
                out
            }
            AddressBookForm::Zedwallet => {
                let entries: Vec<ZedwalletEntry<'_>> = self
                    .entries
                    .iter()
                    .map(|e| ZedwalletEntry {
                        address: &e.address,
                        friendly_name: &e.friendly_name,
                        integrated_address: e.integrated_address,
                        payment_id: &e.payment_id,
                    })
                    .collect();
                serde_json::to_vec(&entries).expect("strings only")
            }
        }
    }

    /// C++ `getAddressBook`: a missing file is an empty book in `form_if_missing`.
    pub fn load(path: &Path, form_if_missing: AddressBookForm) -> Result<Self, FileError> {
        match std::fs::read(path) {
            Ok(bytes) => Self::from_json(&bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::new(form_if_missing)),
            Err(e) => Err(FileError::InvalidWalletFilename(e)),
        }
    }

    /// C++ `saveAddressBook`, replacing the file in one step ([`crate::atomic_write`]).
    pub fn save(&self, path: &Path) -> Result<(), FileError> {
        atomic_write(path, &self.to_json()).map_err(FileError::InvalidWalletFilename)
    }

    /// The entry with this friendly name (C++ compares entries by name only).
    #[must_use]
    pub fn get(&self, friendly_name: &str) -> Option<&AddressBookEntry> {
        self.entries
            .iter()
            .find(|e| e.friendly_name == friendly_name)
    }

    /// Adds an entry; `false` (and nothing added) if the name is taken, as both tools refuse a
    /// duplicate name.
    pub fn add(&mut self, entry: AddressBookEntry) -> bool {
        if self.get(&entry.friendly_name).is_some() {
            return false;
        }
        self.entries.push(entry);
        true
    }

    /// Removes the entry with this name (C++ `deleteFromAddressBook`); `false` if none.
    pub fn remove(&mut self, friendly_name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.friendly_name != friendly_name);
        self.entries.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLUS_PLUS: &str = "[\n    {\n        \"address\": \"SEKReAAA\",\n        \"friendlyName\": \"alice\",\n        \"paymentID\": \"\"\n    },\n    {\n        \"address\": \"SEKReBBB\",\n        \"friendlyName\": \"bob \\\"b\\\"\",\n        \"paymentID\": \"aa\"\n    }\n]\n";

    const ZEDWALLET: &str = r#"[{"address":"SEKReAAA","friendlyName":"alice","integratedAddress":false,"paymentID":"aa"},{"address":"SEKReIII","friendlyName":"int","integratedAddress":true,"paymentID":""}]"#;

    #[test]
    fn zedwallet_plus_plus_roundtrip() {
        let book = AddressBook::from_json(PLUS_PLUS.as_bytes()).unwrap();
        assert_eq!(book.form, AddressBookForm::ZedwalletPlusPlus);
        assert_eq!(book.entries.len(), 2);
        assert_eq!(book.entries[1].friendly_name, "bob \"b\"");
        assert!(!book.entries[0].integrated_address);
        assert_eq!(String::from_utf8(book.to_json()).unwrap(), PLUS_PLUS);
    }

    #[test]
    fn zedwallet_roundtrip() {
        let book = AddressBook::from_json(ZEDWALLET.as_bytes()).unwrap();
        assert_eq!(book.form, AddressBookForm::Zedwallet);
        assert!(book.get("int").unwrap().integrated_address);
        assert_eq!(String::from_utf8(book.to_json()).unwrap(), ZEDWALLET);
    }

    #[test]
    fn empty_books_keep_their_form() {
        let pp = AddressBook::from_json(b"[]\n").unwrap();
        assert_eq!(pp.form, AddressBookForm::ZedwalletPlusPlus);
        assert_eq!(pp.to_json(), b"[]\n");
        let z = AddressBook::from_json(b"[]").unwrap();
        assert_eq!(z.form, AddressBookForm::Zedwallet);
        assert_eq!(z.to_json(), b"[]");
    }

    #[test]
    fn field_order_does_not_matter_on_read() {
        let book =
            AddressBook::from_json(br#"[{"paymentID":"","friendlyName":"x","address":"SEKRe"}]"#)
                .unwrap();
        assert_eq!(book.get("x").unwrap().address, "SEKRe");
    }

    #[test]
    fn errors() {
        assert!(matches!(
            AddressBook::from_json(b"{"),
            Err(FileError::WalletFileCorrupted(_))
        ));
        assert!(matches!(
            AddressBook::from_json(br#"[{"friendlyName":"x","address":"a"}]"#),
            Err(FileError::WalletFileCorrupted(_))
        ));
    }

    #[test]
    fn add_remove_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ADDRESS_BOOK_FILENAME);
        let mut book = AddressBook::load(&path, AddressBookForm::Zedwallet).unwrap();
        assert!(book.entries.is_empty());
        assert_eq!(book.form, AddressBookForm::Zedwallet);
        let e = AddressBookEntry {
            address: "SEKRe1".into(),
            friendly_name: "n".into(),
            payment_id: String::new(),
            integrated_address: false,
        };
        assert!(book.add(e.clone()));
        assert!(!book.add(e));
        book.save(&path).unwrap();
        let loaded = AddressBook::load(&path, AddressBookForm::ZedwalletPlusPlus).unwrap();
        assert_eq!(loaded, book);
        assert!(book.remove("n"));
        assert!(!book.remove("n"));
    }
}
