//! Scripted sessions of the terminal wallet without a terminal, against an in-process xkrd on
//! a fresh testnet chain: the C++ prompts and messages (`tests/fixtures/cpp/*.txt`), the
//! wallet files, the address book and the CSV they leave behind.

mod common;

use std::time::{Duration, Instant};

use common::{
    A, A_SPEND, A_VIEW, A_XKR, DUMMY_ADDR, K, K_SPEND, K_VIEW, OfflineNodes, PID1, RouterNodes,
    Run, SEED_A, node, runtime, wait_height, xkr_form,
};
use xkr_wallet_cli::SignalOutcome;
use xkr_wallet_files::{AddressBook, AddressBookForm};

fn router(node: &common::TestNode) -> RouterNodes {
    RouterNodes(node.router.clone())
}

/// `seed_restore` of seed A with an empty password into `<name>.wallet`, scanning from 0,
/// until the menu is shown.
fn restore_a(run: &mut Run, name: &str, password: &str) {
    run.send_all(&["restore", SEED_A, name, password, ""]);
    run.wait_for(&format!("  ✔ Restored {name}.wallet · {}\n", short(A)));
    run.wait_for(MENU_END);
}

/// `  ✔ Opened <file> · <A short>`.
fn opened_a(file: &str) -> String {
    format!("  ✔ Opened {file} · {}\n", short(A))
}

/// The last line of the command list (`help`).
const MENU_END: &str = "  Type a command, help <command> for details, exit to close.\n";

/// The title of the start menu.
const WELCOME: &str = "Welcome to your XKR wallet";

/// The end of the start menu and of the node-down menu.
const MENU_EXIT: &str = "  0  exit\n";

/// Blocks for the transactions in the pool to confirm and for their outputs to reach the
/// age the node wants for ring members (`CRYPTONOTE_DEFAULT_TX_SPENDABLE_AGE`), then waits
/// until the wallet has scanned them.
fn confirm(rt: &tokio::runtime::Runtime, node: &common::TestNode, run: &mut Run) {
    let top = node.top() + SPENDABLE_AGE;
    rt.block_on(node.mine_to(DUMMY_ADDR, top));
    wait_height(run, top);
}

/// More than `CRYPTONOTE_DEFAULT_TX_SPENDABLE_AGE` (10) blocks.
const SPENDABLE_AGE: u64 = 12;

/// The plain line of a finished step (`<step> … done <time> (<detail>)`).
fn step_line<'a>(out: &'a str, step: &str) -> &'a str {
    out.lines()
        .find(|l| l.starts_with(&format!("{step} … done ")))
        .unwrap_or_else(|| panic!("no {step:?} step in {out}"))
}

/// An address as the confirmation shows it: first 8, `…`, last 5.
fn short(address: &str) -> String {
    format!("{}…{}", &address[..8], &address[address.len() - 5..])
}

/// The hash printed after `Sent · hash `.
fn sent_hash(run: &mut Run) -> String {
    run.wait_for("  ✔ Sent · hash ");
    let line = run.wait_for("\n");
    let hash = line.trim().to_owned();
    assert_eq!(hash.len(), 64, "{line:?}");
    hash
}

#[test]
fn create_backup_status_and_subwallets() {
    let rt = runtime();
    let node = node(&rt);
    rt.block_on(node.mine_to(DUMMY_ADDR, 3));
    let dir = tempfile::tempdir().unwrap();
    let mut run = Run::start(router(&node), &rt, dir.path(), &[]);
    // the start menu
    let menu = run.wait_for(MENU_EXIT);
    assert!(
        menu.contains(
            "\n  Welcome to your XKR wallet\n\n  1  open      Open a wallet on this computer\n"
        ),
        "{menu}"
    );
    // the header: the logo two spaces in, no version line
    assert!(menu.starts_with("\n  ▀▄ ▄▀ █ ▄▀ █▀▀▄\n"), "{menu}");
    assert!(!menu.contains("xkrwallet v"), "{menu}");
    run.send_all(&["create", "fresh", "pw"]);
    let created = run.wait_for("\nSync … done ");
    for part in [
        "\n  New wallet\n  Name › ",
        "  ✔ Created fresh.wallet\n",
        "  Write these down and keep them private: they restore this wallet, and anyone who \
         has them can spend your XKR.\n",
        "\n  Address    SEKR",
        "\n  Seed       ",
        "\n  View key   ",
        "\n  Spend key  ",
    ] {
        assert!(created.contains(part), "{part:?} in {created}");
    }
    assert!(dir.path().join("fresh.wallet").is_file());
    let commands = run.wait_for(MENU_END);
    assert!(
        commands.contains("\n  Wallet         balance  address  send  seed  keys  back\n"),
        "{commands}"
    );
    assert!(
        commands.contains("\n  fresh.wallet · SEKR"),
        "the status line: {commands}"
    );
    // the balance under the status line (normal digits without a terminal)
    assert!(commands.contains("\n  0.00 XKR\n"), "{commands}");

    run.send_all(&["address", "backup", "wrong", "pw"]);
    run.wait_for("\n  Xkr");
    run.wait_for("Incorrect password! Try again.\n");
    let keys = run.wait_for("Mnemonic seed:\n");
    assert!(keys.contains("Private view key:\n"), "{keys}");
    // upper case, a number (menus only) and a wrong name
    run.send_all(&["balance", "STATUS", "3", "nope"]);
    run.wait_for("\n  0.00 XKR\n");
    let status = run.wait_for("Yay! You are synced!\n");
    for part in [
        "Local blockchain height: 3\n",
        "Network blockchain height: 3\n",
        "Network sync status: 100.00%\n",
        "Peers: 0\n",
        "Transaction versions: ",
        "Next fork: ",
        "Wallet: xkrwallet v",
    ] {
        assert!(status.contains(part), "{part:?} in {status}");
    }
    run.wait_for("  No entry 3 · type help for the list\n");
    run.wait_for("  Unknown command: nope · type help for the list\n");
    // nothing to send
    run.send("send-all");
    run.wait_for("  Nothing to send · the minimum is 0.00001 XKR and you have 0.00 XKR\n");
    // help: the groups; one command with its C++ name
    run.send_all(&["advanced", "help transfer"]);
    let help = run.wait_for(MENU_END);
    assert!(
        help.contains("\n  Tools          optimize  send-all"),
        "{help}"
    );
    assert!(help.contains("import-subwallet"), "{help}");
    run.wait_for("\n  send  Send XKR to someone\n  also: s, transfer\n");

    // subwallets
    run.send_all(&[
        "create_subwallet",
        "import_subwallet",
        K_SPEND,
        "",
        "subwallets",
    ]);
    run.wait_for("A new address has been added to your wallet:\nSEKR");
    run.wait_for("Private spend key of this address:\n");
    run.wait_for(" has been added to your wallet.\n");
    let list = run.wait_for("Address 3: SEKR");
    assert!(list.contains("Address 1 (primary): SEKR"), "{list}");
    assert!(list.contains("Address 2: SEKR"), "{list}");

    run.send("exit");
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    assert!(
        out.ends_with("Wallet saved · goodbye\nThanks for stopping by...\n"),
        "{out}"
    );
}

#[test]
fn seed_restore_transfers_address_book_and_csv() {
    let rt = runtime();
    let node = node(&rt);
    rt.block_on(node.mine_to(A, 20));
    // filler blocks: ring members for every coinbase denomination
    rt.block_on(node.mine_to(DUMMY_ADDR, 150));
    let dir = tempfile::tempdir().unwrap();
    let mut run = Run::start(router(&node), &rt, dir.path(), &[]);
    run.send_all(&["seed_restore", SEED_A, "a", "", ""]);
    let restored = run.wait_for(MENU_END);
    // without a terminal the typed lines are not echoed, so prompts run together
    assert!(
        restored.contains(
            "\n  Restore from a seed\n  Seed (25 words) ›   Name ›   a lower height finds older \
             transactions; 0 scans everything\n  Scan from height (enter = 0) › Wallet file … \
             done "
        ),
        "{restored}"
    );
    assert!(
        restored.contains(&format!("  ✔ Restored a.wallet · {}\n", short(A))),
        "{restored}"
    );
    assert!(
        restored.contains("(imported a.wallet, wallet file)\n"),
        "{restored}"
    );
    assert!(
        restored.contains("\nNode … done ")
            && restored.contains("(127.0.0.1:24899 · height 150)\n"),
        "{restored}"
    );
    assert!(restored.contains("\nSync … done "), "{restored}");
    run.send("balance");
    let balance = run.wait_for(" XKR\n");
    assert!(!balance.contains("  0.00 XKR"), "{balance}");

    // to a SEKR address, no payment id (the C++ name `transfer` works too)
    run.send_all(&["send", K, "", "1", "y", ""]);
    let first = sent_hash(&mut run);
    let output = run.output();
    assert!(
        output.contains(&format!(
            // without a terminal the typed lines are not echoed, so prompts run together
            "  Address ›   If you were given a payment ID, you must use it, or the funds may be \
             lost.\n  Payment ID (enter for none) ›   Amount ›   Fee 0.0001 XKR · total \
             1.0001 XKR\n  Send 1.00 XKR to {}? [y/N] › ",
            short(K)
        )),
        "{output}"
    );
    // The three sends go out before any block: each spends coinbase outputs only, never the
    // change of the one before (its denominations are rare on this short chain, too few
    // ring members), and every input is old enough to be a ring member.

    // to the Xkr form, with a payment id
    let k_xkr = xkr_form(K);
    run.send_all(&["transfer", k_xkr.as_str(), PID1, "2", "y", ""]);
    sent_hash(&mut run);
    assert!(
        run.output().contains(&format!(
            "  Payment ID {PID1}\n  Send 2.00 XKR to {}? [y/N] › ",
            short(&k_xkr)
        )),
        "the Xkr transfer was not confirmed as asked"
    );

    // the address book: add, list, send by number
    run.send_all(&["ab_add", "friend", K, ""]);
    run.wait_for("\nA new entry has been added to your address book!\n");
    run.send("ab_list");
    run.wait_for(&format!("Address Book Entry: 1 | friend\nAddress: {K}\n\n"));
    run.send_all(&["ab_send", "1", "0.5", "y", ""]);
    sent_hash(&mut run);
    assert!(
        run.output().contains(
            "  cancel stops at any question · a name or a number from contacts\n  Contact ›   \
             Amount ›   Fee 0.0001 XKR · total 0.5001 XKR\n"
        ),
        "the address book send was not asked in the compact style"
    );
    confirm(&rt, &node, &mut run);
    let book = AddressBook::from_json(
        &std::fs::read(dir.path().join(".addressBook.json")).expect("address book written"),
    )
    .expect("address book parses");
    assert_eq!(book.form, AddressBookForm::ZedwalletPlusPlus);
    assert_eq!(book.entries.len(), 1);
    assert_eq!(book.entries[0].address, K);

    // txs: one line each, newest first; filtered by direction
    run.send("txs out");
    let out = run.wait_for(" transactions\n");
    assert!(out.ends_with("  3 transactions\n"), "{out}");
    for amount in ["-1.0001 XKR", "-2.0001 XKR", "-0.5001 XKR"] {
        assert!(
            out.contains(&format!(" · {amount} · ")),
            "{amount} in {out}"
        );
    }
    assert!(out.contains(&format!(" · pid {}\n", short(PID1))), "{out}");
    run.send("txs in");
    let incoming = run.wait_for(" transactions\n");
    assert!(incoming.ends_with("  20 transactions\n"), "{incoming}");
    assert!(!incoming.contains(" · -"), "{incoming}");
    assert!(incoming.contains(" · +476."), "{incoming}");
    run.send("transactions");
    run.wait_for("  23 transactions\n");
    run.send("txs sideways");
    run.wait_for("  Unknown filter: sideways · txs, txs in or txs out\n");

    run.send("outgoing_transfers");
    run.wait_for("3 outgoing transactions, totalling 3.50030 XKR\n");
    run.send("list_transfers");
    run.wait_for("20 incoming transactions, totalling ");
    run.wait_for("3 outgoing transactions, totalling 3.50030 XKR\n");

    run.send_all(&["get_tx_private_key", first.as_str()]);
    let key = run.wait_for("Transaction private key: ");
    assert!(!key.contains("Invalid hash"), "{key}");
    run.send_all(&["make_integrated_address", A, PID1]);
    run.wait_for("Address: Payment ID: SEKR");

    run.send("save_csv");
    run.wait_for("Saving CSV file...\nCSV successfully written to transactions.csv!\n");
    let csv = std::fs::read_to_string(dir.path().join("transactions.csv")).expect("csv");
    assert!(
        csv.starts_with("Timestamp,Block Height,Hash,Amount,In/Out\n"),
        "{csv}"
    );
    assert_eq!(
        csv.lines().filter(|l| l.ends_with(",IN")).count(),
        20,
        "{csv}"
    );
    assert_eq!(
        csv.lines().filter(|l| l.ends_with(",OUT")).count(),
        3,
        "{csv}"
    );
    assert!(csv.contains(&format!(",{first},1.00010,OUT\n")), "{csv}");

    run.send("exit");
    let (code, _) = run.finish();
    assert_eq!(code, 0);
}

#[test]
fn optimize_then_send_all() {
    let rt = runtime();
    let node = node(&rt);
    rt.block_on(node.mine_to(A, 6));
    rt.block_on(node.mine_to(DUMMY_ADDR, 150));
    let dir = tempfile::tempdir().unwrap();
    let mut run = Run::start(router(&node), &rt, dir.path(), &[]);
    restore_a(&mut run, "o", "");

    run.send_all(&["optimize", "y"]);
    run.wait_for("Running optimization round 1...\n\n");
    // blocks only once a fusion was sent: it has to confirm and unlock
    let deadline = Instant::now() + Duration::from_secs(300);
    while !run.printed("Optimization complete!\n") {
        assert!(
            Instant::now() < deadline,
            "optimize did not finish; output after the last match:\n{}",
            &run.output()[run.seen()..]
        );
        if run.printed("Sent fusion transaction #") {
            let top = node.top();
            rt.block_on(node.mine_to(DUMMY_ADDR, top + 1));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    run.wait_for("Optimization complete!\n");
    // the fusion outputs reach the age the node wants for ring members
    confirm(&rt, &node, &mut run);

    run.send_all(&["send-all", K, "", "y", ""]);
    sent_hash(&mut run);
    confirm(&rt, &node, &mut run);
    run.send("balance");
    run.wait_for("\n  0.00 XKR\n");
    run.send("exit");
    assert_eq!(run.finish().0, 0);
}

#[test]
fn key_restore_and_view_wallet() {
    let rt = runtime();
    let node = node(&rt);
    rt.block_on(node.mine_to(A, 3));
    rt.block_on(node.mine_to(DUMMY_ADDR, 10));
    let dir = tempfile::tempdir().unwrap();

    let mut run = Run::start(router(&node), &rt, dir.path(), &[]);
    let not_on_curve = "ff".repeat(32);
    let not_hex = "zz".repeat(32);
    run.send_all(&[
        "key_restore",
        "abc",
        not_hex.as_str(),
        not_on_curve.as_str(),
        K_SPEND,
        K_VIEW,
        "k",
        "pw2",
        "",
    ]);
    run.wait_for("Invalid private key, should be 64 characters! Try again.");
    run.wait_for("Invalid private key, it is not a valid hex string! Try again.");
    run.wait_for("Invalid private key, is not on the ed25519 curve!");
    run.wait_for(&format!("  ✔ Restored k.wallet · {}\n", short(K)));
    run.send_all(&["address", "backup", "pw2", "seed", "exit"]);
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    assert!(
        out.contains(&format!("Private view key:\n{K_VIEW}\n")),
        "{out}"
    );
    assert!(
        out.contains(&format!("\nPrivate spend key:\n{K_SPEND}\n")),
        "{out}"
    );
    // keys K are not deterministic: no seed
    assert!(!out.contains("Mnemonic seed:"), "{out}");
    assert!(
        out.contains(
            "  No seed · the keys of this wallet were not made from a seed; keys shows them\n"
        ),
        "{out}"
    );

    let mut run = Run::start(router(&node), &rt, dir.path(), &[]);
    run.send_all(&["view_wallet", "y", A_VIEW, A, "v", "vp", ""]);
    run.wait_for(
        "  A view-only wallet sees incoming transfers only and cannot send.\n  Go on? [Y/n] › ",
    );
    run.wait_for(&format!(
        "  ✔ Imported v.wallet · {} · view only\n",
        short(A)
    ));
    run.wait_for("  Spent funds do not show in a view-only wallet");
    // a view wallet's commands: nothing that sends
    let menu = run.wait_for(MENU_END);
    assert!(menu.contains(" · view only\n"), "{menu}");
    assert!(!menu.contains("send"), "{menu}");
    assert!(!menu.contains("optimize"), "{menu}");
    run.send_all(&[
        "balance", "transfer", "send", "backup", "vp", "seed", "exit",
    ]);
    let balance = run.wait_for("and so your wallet balance may appear inflated.\n");
    assert!(!balance.contains("  0.00 XKR"), "{balance}");
    run.wait_for("Unknown command: transfer · type help for the list\n");
    run.wait_for("Unknown command: send · type help for the list\n");
    run.wait_for("  No seed · a view-only wallet has no private spend key\n");
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    assert!(
        out.contains(&format!("Private view key:\n{A_VIEW}\n")),
        "{out}"
    );
    assert!(!out.contains("Private spend key:"), "{out}");
}

#[test]
fn change_password_reset_and_reopen() {
    let rt = runtime();
    let node = node(&rt);
    rt.block_on(node.mine_to(A, 4));
    rt.block_on(node.mine_to(DUMMY_ADDR, 12));
    let dir = tempfile::tempdir().unwrap();

    let mut run = Run::start(router(&node), &rt, dir.path(), &[]);
    restore_a(&mut run, "r", "old");
    run.send_all(&["change_password", "old", "new"]);
    run.wait_for("Your password has been changed!\n");
    run.send_all(&["backup", "old", "new"]);
    run.wait_for("Incorrect password! Try again.\n");
    run.wait_for(&format!("\nPrivate spend key:\n{A_SPEND}\n"));
    run.wait_for(&format!("\nMnemonic seed:\n{SEED_A}\n"));
    run.send_all(&["reset", "", "y"]);
    run.wait_for("This process may take some time to complete.");
    run.wait_for("Are you sure? (Y/n): ");
    run.wait_for("Resetting wallet...\n");
    // scanned again: the same balance as before
    run.send("balance");
    let after = run.wait_for(" XKR\n");
    assert!(!after.contains("  0.00 XKR"), "{after}");
    run.send_all(&["save", "exit"]);
    run.wait_for("Saving.\nSaved.\n");
    assert_eq!(run.finish().0, 0);

    // the new password opens it, the old one does not; `-p` is tried once
    let mut run = Run::start(router(&node), &rt, dir.path(), &["-w", "r", "-p", "old"]);
    run.wait_for("  ✗ Incorrect password! Try again.\n");
    run.send_all(&["new", "address", "exit"]);
    run.wait_for(&opened_a("r.wallet"));
    run.wait_for(&format!("  {A}\n  {A_XKR}\n"));
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    assert!(out.contains("Thanks for stopping by..."), "{out}");
}

#[test]
fn opens_cpp_xkrwallet_container() {
    let rt = runtime();
    let node = node(&rt);
    let dir = tempfile::tempdir().unwrap();
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../xkr-wallet-files/tests/data/green/xkrwallet-seed.wallet");
    let Ok(bytes) = std::fs::read(&fixture) else {
        eprintln!("skipped: {} not present", fixture.display());
        return;
    };
    let copy = dir.path().join("xkrwallet-seed.wallet");
    std::fs::write(&copy, &bytes).unwrap();

    let mut run = Run::start(
        router(&node),
        &rt,
        dir.path(),
        &["-w", "xkrwallet-seed.wallet", "-p", ""],
    );
    run.wait_for(&opened_a("xkrwallet-seed.wallet"));
    run.send_all(&["address", "backup", "", "exit"]);
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    assert!(out.contains(&format!("  {A}\n  {A_XKR}\n")), "{out}");
    assert!(
        out.contains(&format!("\nPrivate spend key:\n{A_SPEND}\n")),
        "{out}"
    );
    assert!(
        out.contains(&format!("\nMnemonic seed:\n{SEED_A}\n")),
        "{out}"
    );
    assert!(
        out.contains(
            "(xkrwallet-seed.wallet, container; state in xkrwallet-seed.wallet.xkrstate)\n"
        ),
        "{out}"
    );
    // the container is left as it was; the engine's state goes next to it
    assert_eq!(std::fs::read(&copy).unwrap(), bytes);
    assert!(xkr_wallet::sidecar_path(&copy).is_file());
}

#[test]
fn node_down_menu_and_exits() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();

    // `exit` at the start menu
    let mut run = Run::start(OfflineNodes, &rt, dir.path(), &[]);
    run.send("exit");
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    assert!(
        out.ends_with("  0  exit\n\nThanks for stopping by...\n"),
        "{out}"
    );

    let mut run = Run::start(OfflineNodes, &rt, dir.path(), &[]);
    run.send_all(&["create", "off", "pw"]);
    run.wait_for("It looks like kryptokronad isn't open!");
    let menu = run.wait_for(MENU_EXIT);
    assert!(
        menu.contains("\n  1  retry     Ask the node again\n"),
        "{menu}"
    );
    let output = run.output();
    assert!(
        output.contains("\nNode … failed after ")
            && output.contains(": 127.0.0.1:24899 · not answering\n"),
        "{output}"
    );
    run.send("try_again");
    run.wait_for("It looks like kryptokronad isn't open!");
    run.send_all(&["swap_node", "localhost:1"]);
    run.wait_for("Swapping node, this may take some time...\n");
    run.wait_for("Node swap complete.\n\n");
    run.wait_for("It looks like kryptokronad isn't open!");
    run.send_all(&["continue", "status"]);
    run.wait_for("Uh oh, it looks like you don't have kryptokronad open!\n");
    // the input ends: the wallet saves and exits
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    assert!(
        out.ends_with("Wallet saved · goodbye\nThanks for stopping by...\n"),
        "{out}"
    );
    assert!(dir.path().join("off.wallet").is_file());

    // `exit` in the node-down menu
    let mut run = Run::start(
        OfflineNodes,
        &rt,
        dir.path(),
        &["-w", "off.wallet", "-p", "pw"],
    );
    run.wait_for("It looks like kryptokronad isn't open!");
    run.send("exit");
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    assert!(out.ends_with("Thanks for stopping by...\n"), "{out}");
}

#[test]
fn exit_end_of_input_and_signals_save_the_wallet() {
    let rt = runtime();
    let node = node(&rt);
    rt.block_on(node.mine_to(A, 3));
    rt.block_on(node.mine_to(DUMMY_ADDR, 10));
    let dir = tempfile::tempdir().unwrap();

    // `exit`, after changing the address book
    let mut run = Run::start(router(&node), &rt, dir.path(), &[]);
    restore_a(&mut run, "s", "pw");
    let opened = run.output();
    assert!(
        opened.contains(&format!("\n  s.wallet · {}…", &A[..8])),
        "the status line: {opened}"
    );
    assert!(opened.contains(" · synced 10\n"), "{opened}");
    assert!(
        opened.contains("(127.0.0.1:24899 · height 10)\n"),
        "{opened}"
    );
    run.send_all(&["ab_add", "friend", K, "", "exit"]);
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    // `exit` quits: the start menu does not come back
    assert_eq!(out.matches(WELCOME).count(), 1, "{out}");
    let closing = &out[out.find("\nClosing the wallet\n").expect("closing steps")..];
    for (step, detail) in [
        ("Stopping sync", "(at height 10)"),
        ("Saving the wallet file", "(s.wallet, wallet file)"),
        (
            "Saving the address book",
            "(.addressBook.json is up to date)",
        ),
        ("Closing the node connection", "(127.0.0.1:24899)"),
    ] {
        let line = step_line(closing, step);
        assert!(line.ends_with(detail), "{line}");
    }
    assert!(
        closing.ends_with("\nWallet saved · goodbye\nThanks for stopping by...\n"),
        "{closing}"
    );

    // the end of the input, nothing changed in the address book
    let mut run = Run::start(router(&node), &rt, dir.path(), &["-w", "s", "-p", "pw"]);
    run.wait_for(&opened_a("s.wallet"));
    run.wait_for(MENU_END);
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    assert!(
        step_line(&out, "Saving the wallet file").ends_with("(s.wallet, wallet file)"),
        "{out}"
    );
    assert!(!out.contains("Saving the address book"), "{out}");
    assert!(
        out.ends_with("Wallet saved · goodbye\nThanks for stopping by...\n"),
        "{out}"
    );

    // Ctrl+C / SIGTERM: closes like `exit`; more signals while closing only ask to wait
    let mut run = Run::start(router(&node), &rt, dir.path(), &["-w", "s", "-p", "pw"]);
    run.wait_for(MENU_END);
    assert_eq!(
        run.shutdown.interrupt(),
        SignalOutcome::Closed { saved: true }
    );
    // the second shows the note once; later ones do nothing (no forced quit)
    assert_eq!(run.shutdown.interrupt(), SignalOutcome::PleaseWait);
    assert_eq!(run.shutdown.interrupt(), SignalOutcome::Ignored);
    assert_eq!(run.shutdown.interrupt(), SignalOutcome::Ignored);
    run.wait_for("Wallet saved · goodbye\n");
    run.wait_for("closing cleanly — please wait, the wallet is being saved\n");
    // input is locked: a command after the signal is not run
    run.send("balance");
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    assert_eq!(out.matches("Closing the wallet").count(), 1, "{out}");
    assert_eq!(out.matches("closing cleanly").count(), 1, "{out}");
    assert!(!out.contains("quit without"), "{out}");
    let after = &out[out.find("closing cleanly").unwrap_or(0)..];
    assert!(
        !after.contains(" XKR\n"),
        "a command ran after the signal: {after}"
    );

    // still a wallet that opens
    let mut run = Run::start(router(&node), &rt, dir.path(), &["-w", "s", "-p", "pw"]);
    run.wait_for(&opened_a("s.wallet"));
    run.send("exit");
    assert_eq!(run.finish().0, 0);
}

#[test]
fn back_closes_the_wallet_and_shows_the_start_menu() {
    let rt = runtime();
    let node = node(&rt);
    rt.block_on(node.mine_to(A, 3));
    rt.block_on(node.mine_to(DUMMY_ADDR, 10));
    let dir = tempfile::tempdir().unwrap();
    let mut run = Run::start(router(&node), &rt, dir.path(), &[]);
    restore_a(&mut run, "b", "pw");

    // `back`: the closing steps, then the start menu again
    run.send("back");
    run.wait_for("\nClosing the wallet\n");
    run.wait_for("Wallet saved · goodbye\n");
    let menu = run.wait_for(MENU_EXIT);
    assert!(menu.contains(WELCOME), "{menu}");
    assert!(!menu.contains("Thanks for stopping by"), "{menu}");
    // no wallet open in the start menu: Ctrl+C quits at once (the watcher exits)
    assert_eq!(run.shutdown.interrupt(), SignalOutcome::NoWallet);

    // the saved wallet opens again from the menu; `back` once more, then `0` quits
    run.send_all(&["open", "b", "pw"]);
    run.wait_for(&opened_a("b.wallet"));
    run.wait_for(MENU_END);
    run.send("back");
    run.wait_for("Wallet saved · goodbye\n");
    run.wait_for(MENU_EXIT);
    run.send("0");
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    assert_eq!(out.matches("\nClosing the wallet\n").count(), 2, "{out}");
    assert_eq!(out.matches(WELCOME).count(), 3, "{out}");
    assert!(
        out.ends_with("  0  exit\n\nThanks for stopping by...\n"),
        "{out}"
    );

    // Ctrl+C after `back` and a new open closes that wallet and quits
    let mut run = Run::start(router(&node), &rt, dir.path(), &[]);
    run.send_all(&["open", "b", "pw"]);
    run.wait_for(MENU_END);
    run.send("back");
    run.wait_for(MENU_EXIT);
    run.send_all(&["open", "b", "pw"]);
    run.wait_for(MENU_END);
    assert_eq!(
        run.shutdown.interrupt(),
        SignalOutcome::Closed { saved: true }
    );
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    assert_eq!(out.matches("\nClosing the wallet\n").count(), 2, "{out}");
    // the end of the input after the signal: no third close, no start menu
    assert_eq!(out.matches(WELCOME).count(), 2, "{out}");
}

#[test]
fn seed_keys_and_short_commands() {
    let rt = runtime();
    let node = node(&rt);
    rt.block_on(node.mine_to(A, 3));
    rt.block_on(node.mine_to(DUMMY_ADDR, 10));
    let dir = tempfile::tempdir().unwrap();
    let mut run = Run::start(router(&node), &rt, dir.path(), &[]);
    restore_a(&mut run, "q", "pw");

    // seed: the password first (a wrong one is asked again), a warning, the 25 words
    run.send_all(&["seed", "wrong", "pw"]);
    run.wait_for("Incorrect password! Try again.\n");
    run.wait_for("  Keep this private: anyone who has it can spend your XKR.\n");
    let words = run.wait_for(&format!("  {SEED_A}\n"));
    assert_eq!(SEED_A.split(' ').count(), 25, "{words}");
    // keys: both private keys after the password
    run.send_all(&["keys", "pw"]);
    run.wait_for("  Keep this private: anyone who has it can spend your XKR.\n");
    run.wait_for(&format!("  View key   {A_VIEW}\n  Spend key  {A_SPEND}\n"));
    // the short names
    run.send_all(&["bal", "addr", "money", "s", "cancel", "help seed"]);
    let balance = run.wait_for(" XKR\n");
    assert!(!balance.contains("  0.00 XKR"), "{balance}");
    run.wait_for(&format!("  {A}\n  {A_XKR}\n"));
    run.wait_for(" XKR\n");
    run.wait_for("  Cancelled · nothing was sent\n");
    run.wait_for("\n  seed  Show your 25-word seed (asks your password)\n  also: mnemonic\n");
    run.send("exit");
    assert_eq!(run.finish().0, 0);

    // a wrong password and then the end of the input: nothing secret is printed
    let mut run = Run::start(router(&node), &rt, dir.path(), &["-w", "q", "-p", "pw"]);
    run.wait_for(MENU_END);
    run.send_all(&["seed", "wrong", "keys", "wrong"]);
    run.wait_for("Incorrect password! Try again.\n");
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    for secret in [
        SEED_A.split(' ').next().unwrap_or_default(),
        A_SPEND,
        A_VIEW,
    ] {
        assert!(!out.contains(secret), "{secret} printed: {out}");
    }
    assert!(!out.contains("Keep this private"), "{out}");
}
