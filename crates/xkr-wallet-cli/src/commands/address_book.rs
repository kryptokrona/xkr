//! `ab_add`, `ab_delete`, `ab_list` and `ab_send` (`src/zedwallet++/address_book.cpp`).
//!
//! The book is `.addressBook.json` in the session directory, read in either C++ form and
//! written back in the form it was read (a new book in the zedwallet++ form); see
//! [`xkr_wallet_files::address_book`].

use std::fmt::Write as _;
use std::path::PathBuf;

use xkr_wallet_files::{AddressBook, AddressBookEntry, AddressBookForm};

use crate::commands::transfer::is_standard_length;
use crate::commands::{Number, leading_int};
use crate::input::Tone;
use crate::session::{NodeFactory, OpenedOf, Session};

impl<F: NodeFactory> Session<F> {
    fn address_book_path(&self) -> PathBuf {
        self.book.path().clone()
    }

    /// `getAddressBook`: a missing file is an empty book.
    fn load_address_book(&self) -> Option<AddressBook> {
        match AddressBook::load(
            &self.address_book_path(),
            AddressBookForm::ZedwalletPlusPlus,
        ) {
            Ok(book) => Some(book),
            Err(e) => {
                self.say(&format!(
                    "{}{}\n",
                    self.paint.warn("Failed to read your address book: "),
                    self.paint.warn(&e.to_string())
                ));
                None
            }
        }
    }

    /// `saveAddressBook`.
    fn save_address_book(&self, book: &AddressBook) -> bool {
        if self.book.save(book) {
            return true;
        }
        let p = self.paint;
        self.say(&format!(
            "{}\n{}{}\n",
            p.warn("Failed to save address book to disk!"),
            p.warn("Check you are able to write files to your "),
            p.warn("current directory.")
        ));
        false
    }

    /// `isAddressBookEmpty`.
    fn address_book_is_empty(&self, book: &AddressBook) -> bool {
        if !book.entries.is_empty() {
            return false;
        }
        let p = self.paint;
        self.say(&format!(
            "{}{}\n",
            p.warn("Your address book is empty! Add some people "),
            p.warn("to it first.")
        ));
        true
    }

    fn address_book_cancelled(&self) {
        self.say(&format!(
            "{}\n",
            self.paint.warn("Cancelling addition to address book.")
        ));
    }

    /// `getAddressBookName`: a name not in the book yet.
    fn get_address_book_name(&mut self, book: &AddressBook) -> Option<String> {
        loop {
            let line = self.console.read_line(
                "What friendly name do you want to give this address book entry?: ",
                Tone::Info,
            )?;
            let name = line.trim();
            if book.get(name).is_none() {
                return Some(name.to_owned());
            }
            let p = self.paint;
            self.say(&format!(
                "{}{}\n\n",
                p.warn("An address book entry with this "),
                p.warn("name already exists!")
            ));
        }
    }

    /// `addToAddressBook`.
    pub(crate) fn ab_add(&mut self) {
        self.say(&format!(
            "{}\n\n",
            self.paint.info(
                "Note: You can type cancel at any time to cancel adding someone to your \
                 address book."
            )
        ));
        let Some(mut book) = self.load_address_book() else {
            return;
        };
        let name = match self.get_address_book_name(&book) {
            Some(name) if name != "cancel" => name,
            _ => return self.address_book_cancelled(),
        };
        let Some(address) = self.get_address("\nWhat address does this user have?: ", true, true)
        else {
            return self.address_book_cancelled();
        };
        let standard = is_standard_length(&address);
        let mut payment_id = String::new();
        if standard {
            let Some(id) = self.get_payment_id(true) else {
                return self.address_book_cancelled();
            };
            payment_id = id;
        }
        book.add(AddressBookEntry {
            address,
            friendly_name: name,
            payment_id,
            integrated_address: !standard,
        });
        if self.save_address_book(&book) {
            self.say(
                &self
                    .paint
                    .ok("\nA new entry has been added to your address book!\n"),
            );
        }
    }

    /// `deleteFromAddressBook`.
    pub(crate) fn ab_delete(&mut self) {
        let Some(mut book) = self.load_address_book() else {
            return;
        };
        if self.address_book_is_empty(&book) {
            return;
        }
        let p = self.paint;
        loop {
            self.say(&p.info("Note: You can type cancel at any time to cancel the deletion.\n\n"));
            let Some(line) = self.console.read_line(
                "What address book entry do you want to delete?: ",
                Tone::Info,
            ) else {
                self.say(&p.warn("Cancelling deletion.\n"));
                return;
            };
            let name = line.trim();
            if name == "cancel" {
                self.say(&p.warn("Cancelling deletion.\n"));
                return;
            }
            if book.remove(name) {
                if self.save_address_book(&book) {
                    self.say(&format!(
                        "\n{}{}\n",
                        p.ok("This entry has been deleted from "),
                        p.ok("your address book!")
                    ));
                }
                return;
            }
            self.say(&format!(
                "{}{}{}",
                p.warn("\nCould not find a user with the name of "),
                p.info(name),
                p.warn(" in your address book!\n\n")
            ));
            self.offer_address_book_list();
        }
    }

    /// "Would you like to list everyone in your address book?"
    fn offer_address_book_list(&mut self) {
        let list = self.confirm(
            "Would you like to list everyone in your address book?",
            true,
        );
        self.say("\n");
        if list {
            self.ab_list();
        }
    }

    /// `listAddressBook`.
    pub(crate) fn ab_list(&self) {
        let Some(book) = self.load_address_book() else {
            return;
        };
        if self.address_book_is_empty(&book) {
            return;
        }
        let p = self.paint;
        let mut text = String::new();
        for (i, entry) in book.entries.iter().enumerate() {
            let _ = writeln!(
                text,
                "{}{}{}{}\n{}{}",
                p.info("Address Book Entry: "),
                p.info(&(i + 1).to_string()),
                p.info(" | "),
                p.ok(&entry.friendly_name),
                p.info("Address: "),
                p.ok(&entry.address)
            );
            if entry.payment_id.is_empty() {
                text.push('\n');
            } else {
                let _ = writeln!(
                    text,
                    "{}{}\n",
                    p.info("Payment ID: "),
                    p.ok(&entry.payment_id)
                );
            }
        }
        self.say(&text);
    }

    /// `getAddressBookEntry`: by name or 1-based number.
    fn get_address_book_entry(&mut self, book: &AddressBook) -> Option<AddressBookEntry> {
        let p = self.paint;
        loop {
            let line = self.console.read_line("  Contact › ", Tone::Bold)?;
            let name = line.trim();
            if name.is_empty() {
                continue;
            }
            if name == "cancel" {
                return None;
            }
            let bad_number = || {
                format!(
                    "{}{}{}{}{}\n\n",
                    p.warn("Bad input, expected a friendly name, "),
                    p.warn("or number from "),
                    p.info("1"),
                    p.warn(" to "),
                    p.info(&book.entries.len().to_string())
                )
            };
            match leading_int(name) {
                Number::Value(n) => {
                    let entry = usize::try_from(n)
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .and_then(|i| book.entries.get(i));
                    if let Some(entry) = entry {
                        return Some(entry.clone());
                    }
                    self.say(&bad_number());
                    continue;
                }
                Number::OutOfRange => {
                    self.say(&bad_number());
                    continue;
                }
                Number::None => {
                    if let Some(entry) = book.get(name) {
                        return Some(entry.clone());
                    }
                    self.say(&format!(
                        "\n{}{}{}\n\n",
                        p.warn("Could not find a user with the name of "),
                        p.info(name),
                        p.warn(" in your address book!")
                    ));
                }
            }
            self.offer_address_book_list();
        }
    }

    /// `sendFromAddressBook`.
    pub(crate) fn ab_send(&mut self, w: &OpenedOf<F>) {
        let Some(book) = self.load_address_book() else {
            return;
        };
        if self.address_book_is_empty(&book) {
            return;
        }
        let p = self.paint;
        self.say(&format!(
            "  {}\n",
            p.dim("cancel stops at any question · a name or a number from contacts")
        ));
        let Some(entry) = self.get_address_book_entry(&book) else {
            self.cancel_transaction();
            return;
        };
        let Some(amount) = self.get_amount("  Amount › ", true) else {
            self.cancel_transaction();
            return;
        };
        self.send_transaction(w, &entry.address, amount, &entry.payment_id);
    }
}
