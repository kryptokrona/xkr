//! `save_csv` (`src/zedwallet++/command_implementations.cpp` `saveCSV`): the confirmed
//! transactions without fusions, `Timestamp,Block Height,Hash,Amount,In/Out`.

use std::fmt::Write as _;
use xkr_wallet::Transaction;

use crate::format;

/// `wallet_config::csvFilename`.
pub(crate) const CSV_FILENAME: &str = "transactions.csv";

/// The file contents.
pub(crate) fn csv_text(transactions: &[Transaction]) -> String {
    let mut out = String::from("Timestamp,Block Height,Hash,Amount,In/Out\n");
    for tx in transactions.iter().filter(|t| !t.is_fusion()) {
        let total = tx.total_amount();
        let _ = writeln!(
            out,
            "{},{},{},{},{}",
            format::date(tx.timestamp),
            tx.block_height,
            tx.hash,
            format::amount_basic(total.unsigned_abs()),
            if total > 0 { "IN" } else { "OUT" }
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use xkr_types::{Hash, PublicKey};

    #[test]
    fn rows_like_cpp() {
        let mut coinbase = Transaction::new(Hash([1; 32]));
        coinbase.is_coinbase = true;
        coinbase.block_height = 1;
        coinbase.timestamp = 1_700_000_000;
        coinbase.transfers = vec![(PublicKey([7; 32]), 47_683_714)];
        let mut sent = Transaction::new(Hash([2; 32]));
        sent.fee = 10;
        sent.block_height = 9;
        sent.timestamp = 1_700_000_090;
        sent.transfers = vec![(PublicKey([7; 32]), -100_010)];
        let mut fusion = Transaction::new(Hash([3; 32]));
        fusion.block_height = 10;
        fusion.transfers = vec![(PublicKey([7; 32]), 0)];
        let text = csv_text(&[coinbase, sent, fusion]);
        assert_eq!(
            text,
            format!(
                "Timestamp,Block Height,Hash,Amount,In/Out\n\
                 2023-11-14 22:13,1,{},476.83714,IN\n\
                 2023-11-14 22:14,9,{},1.00010,OUT\n",
                "01".repeat(32),
                "02".repeat(32)
            )
        );
    }
}
