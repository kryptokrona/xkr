//! Text helpers of the terminal wallets (`src/utilities/format_tools.cpp`,
//! `src/zedwallet++/utilities.cpp` `unixTimeToDate`).

use std::fmt::Write as _;
use xkr_wallet::Transaction;
use xkr_wallet_core::ATOMIC_UNITS_PER_COIN;

use crate::input::Painter;

/// `utilities::formatAmount`: `1,234.56789 XKR`.
#[must_use]
pub fn amount(atomic: u64) -> String {
    xkr_wallet::send::relay::format_amount(atomic)
}

/// `utilities::formatAmountBasic`: `1234.56789` (the CSV form).
#[must_use]
pub fn amount_basic(atomic: u64) -> String {
    format!(
        "{}.{:05}",
        atomic / ATOMIC_UNITS_PER_COIN,
        atomic % ATOMIC_UNITS_PER_COIN
    )
}

/// `n` with a space between groups of three digits (`2 620 300`), as the node prints numbers.
#[must_use]
pub fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// An amount in the compact style: grouped whole part, at least two decimals and no trailing
/// zeros after them (`1 234.56`, `0.0001`, `12.00`), without the ticker.
#[must_use]
pub fn amount_short(atomic: u64) -> String {
    let mut decimals = format!("{:05}", atomic % ATOMIC_UNITS_PER_COIN);
    while decimals.len() > 2 && decimals.ends_with('0') {
        decimals.pop();
    }
    format!("{}.{decimals}", group(atomic / ATOMIC_UNITS_PER_COIN))
}

/// An address shortened to its first 8 and last 5 characters (`SEKReX4M…yBZCf`).
#[must_use]
pub fn short_address(address: &str) -> String {
    let chars: Vec<char> = address.chars().collect();
    if chars.len() <= 14 {
        return address.to_owned();
    }
    let head: String = chars[..8].iter().collect();
    let tail: String = chars[chars.len() - 5..].iter().collect();
    format!("{head}…{tail}")
}

/// How long a step took, as the node shows it: `12 ms`, `1.4 s`, `1m 05s`, `2h 03m`.
#[must_use]
pub fn elapsed(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    let secs = d.as_secs();
    if ms < 1_000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", d.as_secs_f64())
    } else if secs < 3_600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3_600, secs / 60 % 60)
    }
}

/// Year, month and day of a day count since 1970-01-01 (proleptic Gregorian calendar).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `zed_utilities::unixTimeToDate` (`%F %R`): `2026-11-15 18:02`, in UTC.
#[must_use]
pub fn date(timestamp: u64) -> String {
    let days = (timestamp / 86_400) as i64;
    let seconds = timestamp % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        seconds / 3600,
        seconds % 3600 / 60
    )
}

/// `utilities::get_sync_percentage`: two decimals, never 100.00 below the target.
#[must_use]
#[allow(clippy::cast_precision_loss)] // as C++: a float percentage
pub fn sync_percentage(height: u64, target: u64) -> String {
    if height == 0 || target == 0 {
        return "0.00".to_owned();
    }
    let height = height.min(target);
    let mut percent = 100.0f32 * height as f32 / target as f32;
    if height < target && percent > 99.99 {
        percent = 99.99;
    }
    format!("{percent:.2}")
}

/// `utilities::get_mining_speed`.
#[must_use]
#[allow(clippy::cast_precision_loss)] // as C++: shown with two decimals
pub fn mining_speed(hashrate: u64) -> String {
    let rate = hashrate as f64;
    if rate > 1e9 {
        format!("{:.2} GH/s", rate / 1e9)
    } else if rate > 1e6 {
        format!("{:.2} MH/s", rate / 1e6)
    } else if rate > 1e3 {
        format!("{:.2} KH/s", rate / 1e3)
    } else {
        format!("{hashrate} H/s")
    }
}

/// `printIncomingTransfer` (`src/zedwallet++/command_implementations.cpp`), with the final
/// newline of `std::endl`.
#[must_use]
pub fn incoming_transfer(paint: Painter, tx: &Transaction) -> String {
    let mut text = format!(
        "Incoming transfer:\nHash: {}\nBlock height: {}\nTimestamp: {}\nAmount: {}\n",
        tx.hash,
        tx.block_height,
        date(tx.timestamp),
        amount(tx.total_amount().unsigned_abs())
    );
    if !tx.payment_id.is_empty() {
        let _ = writeln!(text, "Payment ID: {}", tx.payment_id);
    }
    format!("{}\n", paint.ok(&text))
}

/// `printOutgoingTransfer`: unconfirmed transactions have no height and timestamp.
#[must_use]
pub fn outgoing_transfer(paint: Painter, tx: &Transaction) -> String {
    let total = tx.total_amount().unsigned_abs();
    let mut text = format!("Outgoing transfer:\nHash: {}\n", tx.hash);
    if tx.block_height != 0 && tx.timestamp != 0 {
        let _ = writeln!(
            text,
            "Block height: {}\nTimestamp: {}",
            tx.block_height,
            date(tx.timestamp)
        );
    }
    let _ = writeln!(
        text,
        "Spent: {}\nFee: {}\nTotal Spent: {}",
        amount(total.saturating_sub(tx.fee)),
        amount(tx.fee),
        amount(total)
    );
    if !tx.payment_id.is_empty() {
        let _ = writeln!(text, "Payment ID: {}", tx.payment_id);
    }
    format!("{}\n", paint.warn(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts() {
        assert_eq!(amount(1_430_301_516), "14,303.01516 XKR");
        assert_eq!(amount(10), "0.00010 XKR");
        assert_eq!(amount_basic(47_683_714), "476.83714");
        assert_eq!(amount_basic(5), "0.00005");
    }

    #[test]
    fn compact_forms() {
        assert_eq!(group(2_620_301), "2 620 301");
        assert_eq!(group(12), "12");
        assert_eq!(amount_short(123_456_000), "1 234.56");
        assert_eq!(amount_short(10), "0.0001");
        assert_eq!(amount_short(1_200_000), "12.00");
        assert_eq!(amount_short(100_010), "1.0001");
        assert_eq!(amount_short(1), "0.00001");
        assert_eq!(amount_short(0), "0.00");
        assert_eq!(
            short_address(
                "SEKReX4MYn2HAJFoHvcqVBPyHYom2ynKeK6dpYptbp8gQNzdzE73ZDkNmNurqfhhcMSUXpS1ZGEJKiKJUcPCyw7vYaCc35yBZCf"
            ),
            "SEKReX4M…yBZCf"
        );
        assert_eq!(elapsed(std::time::Duration::from_millis(12)), "12 ms");
        assert_eq!(elapsed(std::time::Duration::from_millis(1_440)), "1.4 s");
        assert_eq!(elapsed(std::time::Duration::from_secs(65)), "1m 05s");
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01 00:00");
        assert_eq!(date(1_700_000_000), "2023-11-14 22:13");
        assert_eq!(date(951_782_400), "2000-02-29 00:00");
        assert_eq!(date(1_798_761_599), "2026-12-31 23:59");
    }

    #[test]
    fn percentages_and_speeds() {
        assert_eq!(sync_percentage(0, 10), "0.00");
        assert_eq!(sync_percentage(5, 10), "50.00");
        assert_eq!(sync_percentage(20, 10), "100.00");
        assert_eq!(sync_percentage(99_999, 100_000), "99.99");
        assert_eq!(mining_speed(950), "950 H/s");
        assert_eq!(mining_speed(12_346), "12.35 KH/s");
        assert_eq!(mining_speed(2_500_000), "2.50 MH/s");
        assert_eq!(mining_speed(3_000_000_001), "3.00 GH/s");
    }
}
