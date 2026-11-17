# Wallet tools: moving from the C++ wallets

This guide is for exchanges, pools, services and users who run the C++ Kryptokrona wallet tools
(`wallet-api`, `kryptokrona-service`, `xkrwallet`, `xkrwallet-beta`, `miner`). The Rust tools do
the same jobs with the same flags, the same wallet files and the same API answers. In most cases
you only replace the binary.

## 1. What is in the release

| Rust program | Replaces | Copy in the archive under the old name |
|---|---|---|
| `xkr-wallet-api` | `wallet-api` (REST API) | `wallet-api` |
| `xkr-walletd` | `kryptokrona-service` (walletd, JSON-RPC) | `kryptokrona-service` |
| `xkrwallet` | `xkrwallet` and `xkrwallet-beta` (terminal wallets) | `xkrwallet` |
| `xkr-miner` | `miner` | `miner` |
| `xkrd` | `kryptokronad` (the node) | |

The copies are the same programs. If your scripts, service files or containers start
`wallet-api` or `kryptokrona-service`, they keep working when you put the new files in place of
the old ones.

## 2. Switching an exchange or service

1. Stop the C++ program the way you always do (`exit` on the wallet-api console, `SIGTERM` for
   `kryptokrona-service`). Both save the wallet when they stop.
2. Make a copy of the wallet file. For `kryptokrona-service` this is the container file
   (`-w`/`--container-file`).
3. Put the new binary in place of the old one (or use the copy with the old name from the
   archive).
4. Start it with the same command line, the same config file and the same wallet file. Nothing
   in your code that calls the API needs to change.
5. Wait until the wallet reports it is synced (`/status` for wallet-api, `getStatus` for
   walletd), then check the balance.

Details that stay the same:

- Flags: every C++ flag is accepted with the same meaning and default. `wallet-api` still
  needs `--rpc-password`; both programs still listen on port 8070 by default.
- Requests and answers: the same routes and methods, the same JSON fields, the same error codes
  and messages, the same HTTP status codes.
- Config files: `kryptokrona-service --config` reads the same file, also the old ini form.
- One addition: `--network testnet` selects the test network (the C++ tools needed a separate
  build for that). Mainnet is the default, so production command lines do not change.

The tools work with `xkrd`. Point them at an `xkrd` node before fork 14 (section 5): C++ nodes
stop following the chain at the fork height.

## 3. Wallet files

The Rust tools open both kinds of wallet file the C++ tools write:

| Written by | Kind | What the Rust tools do with it |
|---|---|---|
| `wallet-api`, `xkrwallet-beta`, wallet-backend-js | wallet file (WalletBackend) | open it and save to it in the same format; the C++ tools can open it again |
| `kryptokrona-service`, `xkrwallet` | container (WalletGreen) | open it, keep the container as it is, and keep their own state next to it in `<container>.xkrstate` |

### Containers: the first start scans once

The first time a Rust tool opens a container it reads the keys and the transaction history from
the container, then scans the chain for the wallet's outputs once, starting from the
container's creation time. How long that takes depends on how long ago the wallet was created;
a wallet created recently is done in seconds, an old one with a busy history can take a while.
During the scan the balance is not final, as with any sync. After the scan the state is saved
in `<container>.xkrstate` (encrypted with the same password), and later starts continue from
there.

The container itself is left as it was. It is rewritten only when its addresses change
(`createAddress`, `createAddressList`, `deleteAddress`). Then it holds the keys of every
address, and the C++ tools can still open it; they scan the chain again to find the history.

If the `.xkrstate` file is lost or deleted, the next start scans again from the container.
Nothing is lost: the keys are in the container.

Back up both files: the container (keys) and `<container>.xkrstate` (state, so a restore does
not need a scan).

### Exports

`kryptokrona-service`'s `export` method writes a container with the keys and the transaction
history. Exports made by the Rust walletd open in the C++ walletd and the other way round.

## 4. Xkr addresses

Every address has two forms with the same keys: the `SEKR…` form and the `Xkr…` form. Both
forms are accepted wherever an address is expected (destinations, `validateAddress`, address
routes, the terminal wallets, integrated addresses). Funds sent to either form arrive in the
same wallet. The tools show both forms where they print an address. You do not need to change
the addresses you have stored for your users; you can accept the `Xkr…` form from them right
away.

## 5. Fork 14: the deadline for the C++ wallets

At fork 14 the network moves to transaction version 2 (view tags, smaller ring signatures). The
heights for mainnet:

| Block | What changes |
|---|---|
| 2 700 000 | fork 13; blocks are version 6 from 2 700 001, transactions unchanged |
| F = 2 750 000 | fork 14; blocks are version 7 from F + 1 |
| F + 1 to F + 28 800 (30 days) | grace window: version 1 and version 2 transactions are both valid |
| from 2 778 801 | only version 2 transactions are valid |

What this means for each setup:

- **C++ wallet on a C++ node:** the C++ node stops following the chain at F. The wallet stops
  seeing new blocks at F.
- **C++ wallet-api, kryptokrona-service, xkrwallet or xkrwallet-beta on xkrd:** the wallet keeps
  syncing and keeps receiving after F. It can send only up to F + 28 800. After that the node
  refuses its transactions with `transaction version not allowed at this height`.
- **Rust tools on xkrd:** nothing to do. They ask the node which transaction versions it accepts
  (`tx_versions` in `/getinfo`) and build version 1 before the fork and version 2 as soon as
  the node accepts it. They receive version 2 transfers and version 2 coinbase outputs.

Exchanges should run the Rust tools on `xkrd` before F. Block F + 28 800 is the last moment at
which a C++ wallet attached to `xkrd` can still move funds.

## 6. Saving and crashes

The C++ tools save when you ask (`PUT /save`, `save`) and when they stop. The Rust tools do the
same, and `xkr-wallet-api` and `xkr-walletd` also save on their own while a wallet is open:

- right after a transaction is recorded (received, sent or cancelled);
- every 5 minutes when something changed;
- after every 1 000 blocks of sync;
- never more often than every 10 seconds.

The terminal wallet `xkrwallet` saves on `save`, `back`, `exit`, at the end of its input and on
Ctrl+C or `SIGTERM`.

Every save replaces the file in one step: the new content goes to a temporary file in the same
directory, is flushed to disk and then renamed over the old file. If the process is killed or
the power fails during a save, the old file or the new file is there, never a half-written one.
Temporary files left by an interrupted save are removed the next time the wallet is opened. For
containers the order of the two files is chosen so that every stopping point opens: at worst
the state file is rebuilt by a scan.

## 7. The terminal wallet

`xkrwallet` takes the flags of both C++ terminal wallets (`-w/--wallet-file`, `-p/--password`,
`-r/--remote-daemon host:port`, `--debug`, plus `--network`) and knows their commands under the
old names (`transfer`, `balance`, `save_csv`, `ab_add`, …) as well as shorter new ones. It opens
files of both `xkrwallet` and `xkrwallet-beta`; new wallets are wallet files (the
`xkrwallet-beta` kind).

Without a terminal (input from a pipe or a file) it reads one answer per line and prints no
colours, so it can be scripted:

```sh
printf '%s\n' balance "txs out" export exit | xkrwallet -w my.wallet -p "$PASSWORD" -r 127.0.0.1:11898
```

## 8. Going back to the C++ tools

You can go back as long as the C++ tools can still send (before F + 28 800, section 5):

1. Stop the Rust program (it saves).
2. Put the C++ binary back and start it with the same command line and file.

- Wallet files (`wallet-api`, `xkrwallet-beta`): the Rust tools save them in the C++ format; the
  C++ tools open them with the same balance and history.
- Containers (`kryptokrona-service`, `xkrwallet`): the container is the one the C++ tool wrote,
  unless addresses were added or removed; then it holds the keys of every address. The C++
  tools ignore `<container>.xkrstate` (you can delete it) and scan the chain from the container.
- Wallets the Rust tools created: `xkr-wallet-api` and `xkrwallet` create wallet files and
  `xkr-walletd -g` creates containers, all in the formats the C++ tools read.

## 9. Checklist for exchanges

- [ ] Run `xkrd` and point the wallet at it.
- [ ] Back up the wallet file (and, later, `<container>.xkrstate`).
- [ ] Replace `wallet-api` or `kryptokrona-service` with the Rust program (or its copy with the
      old name), same flags, same file.
- [ ] Let the first start finish its scan; compare the balance with the one before the switch.
- [ ] Make a small deposit to an `Xkr…` address and a small withdrawal.
- [ ] Be on the Rust tools before fork 14 (mainnet F = 2 750 000).

## 10. How this is tested

The testnet suite (`docs/testnet-suite.md`) runs the Rust tools next to the C++ ones built from
the current C++ source:

| Scenario | What it shows |
|---|---|
| `60-wallet-api` | `xkr-wallet-api` and C++ `wallet-api` with the same keys give the same balances, transactions and status through sends, payment IDs, Xkr destinations and fusion; each opens the other's file |
| `61-walletd` | the same for `xkr-walletd` and `kryptokrona-service`, including delayed transactions and exports |
| `62-wallet-migration` | files of all four C++ wallet tools open in all three Rust tools with the same balance, and back |
| `63-wallet-fork14` | the Rust tools through fork 14; a C++ wallet on `xkrd` receiving after the fork and refused after the grace window |
| `64-wallet-cli` | `xkrwallet` scripted without a terminal, and opening `xkrwallet` and `xkrwallet-beta` files |
