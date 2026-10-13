//! Converting between the ledger's integer minor units and what people type.

/// Decimal places the currency uses (ISO 4217 exponent). Codes not listed use 2.
pub fn decimals(currency: &str) -> u32 {
    match currency {
        "BIF" | "CLP" | "DJF" | "GNF" | "ISK" | "JPY" | "KMF" | "KRW" | "PYG" | "RWF" | "UGX"
        | "VND" | "VUV" | "XAF" | "XOF" | "XPF" => 0,
        "BHD" | "IQD" | "JOD" | "KWD" | "LYD" | "OMR" | "TND" => 3,
        _ => 2,
    }
}

/// Parse "10", "10.5", "1,000.50", ".5" or "-3.20" into minor units. Never rounds: too many
/// decimal places is an error, so nothing is silently lost.
pub fn parse_amount(input: &str, decimals: u32) -> Result<i64, String> {
    let s = input.trim();
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let body = body.trim();
    if body.is_empty() {
        return Err("enter an amount".to_string());
    }
    let (whole, frac) = match body.split_once('.') {
        Some((w, f)) => (w, f),
        None => (body, ""),
    };
    let whole: String = whole.chars().filter(|c| *c != ',').collect();
    if whole.is_empty() && frac.is_empty() {
        return Err("enter an amount".to_string());
    }
    if !whole.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return Err("amounts may only contain digits and one decimal point".to_string());
    }
    if frac.len() > decimals as usize {
        return Err(format!("too many decimal places (this currency uses {decimals})"));
    }
    let whole_val: i128 = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| "amount too large".to_string())?
    };
    let frac_val: i128 = if decimals == 0 {
        0
    } else {
        let padded = format!("{:0<w$}", frac, w = decimals as usize);
        padded.parse().map_err(|_| "invalid amount".to_string())?
    };
    let scale = 10i128.pow(decimals);
    let total = whole_val
        .checked_mul(scale)
        .and_then(|v| v.checked_add(frac_val))
        .ok_or_else(|| "amount too large".to_string())?;
    let total = if neg { -total } else { total };
    i64::try_from(total).map_err(|_| "amount too large".to_string())
}

/// Format minor units for display, e.g. 123456789 with 2 decimals -> "1,234,567.89".
pub fn format_minor(n: i64, decimals: u32) -> String {
    let neg = n < 0;
    let abs = n.unsigned_abs() as u128;
    let scale = 10u128.pow(decimals);
    let whole = (abs / scale).to_string();
    let frac = abs % scale;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    for (i, ch) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if decimals > 0 {
        out.push('.');
        out.push_str(&format!("{:0>w$}", frac, w = decimals as usize));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_input() {
        assert_eq!(parse_amount("10.50", 2), Ok(1050));
        assert_eq!(parse_amount("10", 2), Ok(1000));
        assert_eq!(parse_amount(".5", 2), Ok(50));
        assert_eq!(parse_amount("-1.25", 2), Ok(-125));
        assert_eq!(parse_amount("1,000.5", 2), Ok(100_050));
        assert_eq!(parse_amount(" 7 ", 2), Ok(700));
        assert_eq!(parse_amount("500", 0), Ok(500));
        assert_eq!(parse_amount("1.500", 3), Ok(1500));
    }

    #[test]
    fn rejects_bad_input_instead_of_rounding() {
        assert!(parse_amount("1.234", 2).is_err());
        assert!(parse_amount("500.5", 0).is_err());
        assert!(parse_amount("abc", 2).is_err());
        assert!(parse_amount("", 2).is_err());
        assert!(parse_amount("-", 2).is_err());
        assert!(parse_amount("1.2.3", 2).is_err());
    }

    #[test]
    fn handles_the_i64_edges() {
        assert_eq!(parse_amount("92233720368547758.07", 2), Ok(i64::MAX));
        assert!(parse_amount("92233720368547758.08", 2).is_err());
        assert_eq!(parse_amount("-92233720368547758.08", 2), Ok(i64::MIN));
        assert!(parse_amount("99999999999999999999999999999999999999999", 2).is_err());
    }

    #[test]
    fn formats_for_display() {
        assert_eq!(format_minor(1050, 2), "10.50");
        assert_eq!(format_minor(-5, 2), "-0.05");
        assert_eq!(format_minor(123_456_789, 2), "1,234,567.89");
        assert_eq!(format_minor(500, 0), "500");
        assert_eq!(format_minor(1500, 3), "1.500");
        assert_eq!(format_minor(i64::MIN, 2), "-92,233,720,368,547,758.08");
    }

    #[test]
    fn decimals_table() {
        assert_eq!(decimals("GBP"), 2);
        assert_eq!(decimals("JPY"), 0);
        assert_eq!(decimals("KWD"), 3);
    }
}
