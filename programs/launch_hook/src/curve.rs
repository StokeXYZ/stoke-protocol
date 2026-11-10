//! A pump.fun-style virtual constant-product curve, priced in the quote currency (SOL).
//!
//! With supply N, raise target R and ratio a (in bps): the curve sells N/(1+a) tokens,
//! N·a/(1+a) are kept for the pool, and the virtual reserves start at
//! vq = R·a/(1−a), vt = N/(1−a²). When the last curve token sells, the curve's price equals the
//! pool's (R raised against N·a/(1+a) tokens), so graduating moves no price.

pub const BPS: u128 = 10_000;

fn ceil_div(a: u128, b: u128) -> u128 {
    a.div_ceil(b)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CurveParams {
    pub virtual_quote: u64,
    pub virtual_token: u64,
    /// Tokens the curve sells.
    pub alloc: u64,
    /// Tokens kept for the pool at graduation.
    pub lp_tokens: u64,
}

pub fn params(total_supply: u64, raise_target: u64, ratio_bps: u16) -> CurveParams {
    let (n, a, r) = (total_supply as u128, ratio_bps as u128, raise_target as u128);
    let alloc = n * BPS / (BPS + a);
    CurveParams {
        virtual_quote: (r * a / (BPS - a)) as u64,
        virtual_token: (n * BPS * BPS / (BPS * BPS - a * a)) as u64,
        alloc: alloc as u64,
        lp_tokens: (n - alloc) as u64,
    }
}

/// Virtual reserves once every curve token is sold: the graduation price is vq/vt.
pub fn final_reserves(p: &CurveParams) -> (u128, u128) {
    let k = p.virtual_quote as u128 * p.virtual_token as u128;
    let vt = p.virtual_token as u128 - p.alloc as u128;
    (ceil_div(k, vt), vt)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Curve {
    pub virtual_quote: u64,
    pub virtual_token: u64,
    pub real_quote: u64,
    pub real_token: u64,
}

/// One curve trade. For buys `quote` is what the buyer pays (gross) and `tokens` what they get;
/// for sells `tokens` is what the seller pays and `quote` what they get (net).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fill {
    pub quote: u64,
    pub tokens: u64,
    /// Trading fee (protocol + creator), in quote.
    pub fee: u64,
    /// Sniper tax, in quote. It buys tokens that are burned.
    pub snipe: u64,
    /// Tokens bought with the sniper tax.
    pub burned: u64,
    /// Quote added to (buys) or removed from (sells) the curve.
    pub curve_quote: u64,
    /// Tokens removed from (buys, incl. burned) or added to (sells) the curve.
    pub curve_tokens: u64,
    /// This buy sold the curve's last token.
    pub completes: bool,
}

impl Curve {
    fn k(&self) -> u128 {
        self.virtual_quote as u128 * self.virtual_token as u128
    }

    fn tokens_for(&self, eff: u128) -> u128 {
        let (vq, vt) = (self.virtual_quote as u128, self.virtual_token as u128);
        vt - ceil_div(self.k(), vq + eff)
    }

    fn quote_for_tokens(&self, t: u128) -> u128 {
        let (vq, vt) = (self.virtual_quote as u128, self.virtual_token as u128);
        ceil_div(self.k(), vt - t) - vq
    }

    /// Spot price, quote per token, as a fraction.
    pub fn price(&self) -> (u64, u64) {
        (self.virtual_quote, self.virtual_token)
    }

    pub fn buy_exact_in(&self, gross: u64, fee_bps: u16, snipe_bps: u16) -> Fill {
        let (f, s) = (fee_bps as u128, snipe_bps as u128);
        let g = gross as u128;
        let (mut fee, mut snipe) = (g * f / BPS, g * s / BPS);
        let mut eff = g - fee;
        let mut total = self.tokens_for(eff);
        let mut used = g;
        let completes = total >= self.real_token as u128;
        if completes {
            total = self.real_token as u128;
            let needed = self.quote_for_tokens(total);
            used = ceil_div(needed * BPS, BPS - f).min(g);
            fee = used * f / BPS;
            snipe = used * s / BPS;
            eff = used - fee;
        }
        let user = if eff == 0 { 0 } else { total * (eff - snipe) / eff };
        Fill {
            quote: used as u64,
            tokens: user as u64,
            fee: fee as u64,
            snipe: snipe as u64,
            burned: (total - user) as u64,
            curve_quote: eff as u64,
            curve_tokens: total as u64,
            completes,
        }
    }

    pub fn buy_exact_out(&self, wanted: u64, fee_bps: u16, snipe_bps: u16) -> Fill {
        let (f, s) = (fee_bps as u128, snipe_bps as u128);
        let net_bps = BPS - f - s;
        let mut user = wanted as u128;
        let mut total = ceil_div(user * (BPS - f), net_bps);
        let completes = total >= self.real_token as u128;
        if completes {
            total = self.real_token as u128;
            user = total * net_bps / (BPS - f);
        }
        let needed = self.quote_for_tokens(total);
        let gross = ceil_div(needed * BPS, BPS - f);
        let fee = gross * f / BPS;
        Fill {
            quote: gross as u64,
            tokens: user as u64,
            fee: fee as u64,
            snipe: (gross * s / BPS) as u64,
            burned: (total - user) as u64,
            curve_quote: (gross - fee) as u64,
            curve_tokens: total as u64,
            completes,
        }
    }

    pub fn sell_exact_in(&self, tokens: u64, fee_bps: u16) -> Fill {
        let (vq, vt) = (self.virtual_quote as u128, self.virtual_token as u128);
        let gross = (vq - ceil_div(self.k(), vt + tokens as u128)).min(self.real_quote as u128);
        let fee = ceil_div(gross * fee_bps as u128, BPS);
        Fill {
            quote: (gross - fee) as u64,
            tokens,
            fee: fee as u64,
            curve_quote: gross as u64,
            curve_tokens: tokens,
            ..Default::default()
        }
    }

    /// `None` if the curve doesn't hold that much quote.
    pub fn sell_exact_out(&self, wanted: u64, fee_bps: u16) -> Option<Fill> {
        let gross = ceil_div(wanted as u128 * BPS, BPS - fee_bps as u128);
        if gross > self.real_quote as u128 {
            return None;
        }
        let (vq, vt) = (self.virtual_quote as u128, self.virtual_token as u128);
        let tokens = ceil_div(self.k(), vq - gross) - vt;
        Some(Fill {
            quote: wanted,
            tokens: tokens as u64,
            fee: (gross - wanted as u128) as u64,
            curve_quote: gross as u64,
            curve_tokens: tokens as u64,
            ..Default::default()
        })
    }

    pub fn apply_buy(&mut self, f: &Fill) {
        self.virtual_quote += f.curve_quote;
        self.virtual_token -= f.curve_tokens;
        self.real_quote += f.curve_quote;
        self.real_token -= f.curve_tokens;
    }

    pub fn apply_sell(&mut self, f: &Fill) {
        self.virtual_quote -= f.curve_quote;
        self.virtual_token += f.curve_tokens;
        self.real_quote -= f.curve_quote;
        self.real_token += f.curve_tokens;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: u64 = 1_000_000_000_000_000;
    const R: u64 = 85_000_000_000;

    fn fresh() -> (CurveParams, Curve) {
        let p = params(N, R, 2609);
        (p, Curve { virtual_quote: p.virtual_quote, virtual_token: p.virtual_token, real_quote: 0, real_token: p.alloc })
    }

    #[test]
    fn pump_shape() {
        let (p, _) = fresh();
        // ~30 SOL virtual, ~1.073B virtual tokens, ~793M sold on the curve, ~207M to the pool.
        assert_eq!(p.virtual_quote, 30_004_735_489);
        assert_eq!(p.alloc, 793_084_304_861_606);
        assert_eq!(p.lp_tokens, N - p.alloc);
        assert!((1_073_000_000_000_000..1_074_000_000_000_000).contains(&p.virtual_token));
        // The curve's last price equals the pool's (R over the LP tokens), to 0.01%.
        let (vq, vt) = final_reserves(&p);
        let curve_price = vq as f64 / vt as f64;
        let pool_price = R as f64 / p.lp_tokens as f64;
        assert!((curve_price / pool_price - 1.0).abs() < 1e-4, "{curve_price} vs {pool_price}");
    }

    #[test]
    fn buying_the_whole_curve_raises_the_target() {
        let (_, mut c) = fresh();
        let f = c.buy_exact_in(1_000_000_000_000, 100, 0);
        assert!(f.completes);
        assert_eq!(f.curve_tokens, c.real_token);
        c.apply_buy(&f);
        assert_eq!(c.real_token, 0);
        assert!(c.real_quote >= R && c.real_quote < R + 10, "{}", c.real_quote);
        assert!(f.quote < 1_000_000_000_000);
    }

    #[test]
    fn round_trips_never_profit() {
        let (_, mut c) = fresh();
        for gross in [1_000u64, 1_000_000, 50_000_000, 3_000_000_000, 20_000_000_000] {
            let b = c.buy_exact_in(gross, 100, 0);
            c.apply_buy(&b);
            let s = c.sell_exact_in(b.tokens, 100);
            assert!(s.quote < gross, "{gross}: {} back", s.quote);
            c.apply_sell(&s);
        }
        assert_eq!(c.real_token, fresh().1.real_token);
    }

    #[test]
    fn exact_out_matches_exact_in() {
        let (_, c) = fresh();
        let out = c.buy_exact_out(10_000_000_000_000, 100, 0);
        assert_eq!(out.tokens, 10_000_000_000_000);
        let back = c.buy_exact_in(out.quote, 100, 0);
        assert!(back.tokens >= out.tokens && back.tokens - out.tokens < 1_000_000);
        let s = c.sell_exact_out(0, 100);
        assert!(s.is_some());
        assert!(c.sell_exact_out(1, 100).is_none(), "an empty curve can't pay out");
    }

    #[test]
    fn sniper_tax_buys_tokens_that_are_burned() {
        let (_, c) = fresh();
        let clean = c.buy_exact_in(1_000_000_000, 100, 0);
        let taxed = c.buy_exact_in(1_000_000_000, 100, 5_000);
        assert_eq!(taxed.curve_tokens, clean.curve_tokens, "the whole payment still buys from the curve");
        assert!(taxed.tokens * 2 < clean.tokens + 2_000_000);
        assert_eq!(taxed.tokens + taxed.burned, taxed.curve_tokens);
        let out = c.buy_exact_out(taxed.tokens, 100, 5_000);
        assert!(out.quote <= 1_000_000_001 && out.quote >= 999_000_000, "{}", out.quote);
    }

    #[test]
    fn meridian_shape() {
        // Every Meridian coin: 400 SOL raised, a = 0.375. Opens at ~206 SOL of market cap and
        // graduates at ~1,467 SOL (7.1×), selling 72.7% on the curve and pooling 27.3%.
        let p = params(N, 400_000_000_000, 3_750);
        let open_mc = p.virtual_quote as f64 * N as f64 / p.virtual_token as f64 / 1e9;
        let (vq, vt) = final_reserves(&p);
        let grad_mc = vq as f64 * N as f64 / vt as f64 / 1e9;
        assert!((open_mc - 206.25).abs() < 0.01, "{open_mc}");
        assert!((grad_mc - 1466.67).abs() < 0.01, "{grad_mc}");
        assert!((p.alloc as f64 / N as f64 - 1.0 / 1.375).abs() < 1e-9);
    }
}
