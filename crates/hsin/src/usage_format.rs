//! Formatting shared by the stats screen and `hsin stats`.

use hsin_core::UsageCost;

/// `1234567` as `1.2m`.
pub fn compact_tokens(value: u64) -> String {
    if value >= 1_000_000_000 {
        format!(
            "{}.{}b",
            value / 1_000_000_000,
            value % 1_000_000_000 / 100_000_000
        )
    } else if value >= 1_000_000 {
        format!("{}.{}m", value / 1_000_000, value % 1_000_000 / 100_000)
    } else if value >= 1_000 {
        format!("{}.{}k", value / 1_000, value % 1_000 / 100)
    } else {
        value.to_string()
    }
}

/// Each currency on its own, `$1.20 · ¥3.50`; amounts are estimates and never converted.
pub fn format_cost(costs: &[UsageCost]) -> String {
    costs
        .iter()
        .map(|cost| {
            let amount = if cost.amount >= 100.0 {
                format!("{:.0}", cost.amount)
            } else if cost.amount >= 1.0 {
                format!("{:.2}", cost.amount)
            } else if cost.amount > 0.0 && cost.amount < 0.001 {
                "<0.001".to_owned()
            } else {
                format!("{:.3}", cost.amount)
            };
            match cost.currency.as_str() {
                "USD" => format!("${amount}"),
                "CNY" => format!("¥{amount}"),
                "EUR" => format!("€{amount}"),
                "GBP" => format!("£{amount}"),
                currency => format!("{amount} {currency}"),
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn costs_keep_currencies_apart_and_scale_precision() {
        let cost = |currency: &str, amount: f64| UsageCost {
            currency: currency.into(),
            amount,
        };
        assert_eq!(
            format_cost(&[cost("CNY", 3.5), cost("USD", 0.0421), cost("JPY", 250.4)]),
            "¥3.50 · $0.042 · 250 JPY"
        );
        assert_eq!(format_cost(&[cost("USD", 0.000_01)]), "$<0.001");
        assert_eq!(format_cost(&[]), "");
        assert_eq!(compact_tokens(1_234_567), "1.2m");
    }
}
