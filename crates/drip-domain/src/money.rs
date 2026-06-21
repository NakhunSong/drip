//! Financial value objects: [`Money`], [`Price`], [`Percent`], [`Shares`].
//!
//! Invariant of the whole system: anything that touches an order, a balance, or an
//! average price is an exact [`rust_decimal::Decimal`] — never an `f64`. Penny rounding
//! errors compound across an "infinite buying" cycle, so floats are banned here by
//! construction (there is simply no `f64` constructor on these types).

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::{Add, AddAssign, Sub, SubAssign};

/// An exact monetary amount in the position's working currency (USD for US ETFs).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Money(Decimal);

impl Money {
    pub const ZERO: Money = Money(Decimal::ZERO);

    pub fn new(amount: Decimal) -> Money {
        Money(amount)
    }

    pub fn amount(self) -> Decimal {
        self.0
    }

    pub fn is_positive(self) -> bool {
        self.0 > Decimal::ZERO
    }

    pub fn is_zero(self) -> bool {
        self.0.is_zero()
    }

    /// Split into `n` equal tranches (e.g. `seed.split(40)` for the daily budget).
    /// `n` is clamped to at least 1 to avoid division by zero.
    pub fn split(self, n: u32) -> Money {
        Money(self.0 / Decimal::from(n.max(1)))
    }

    /// Scale by a ratio, e.g. `budget.scaled(dec!(0.5))` for half the daily budget.
    pub fn scaled(self, factor: Decimal) -> Money {
        Money(self.0 * factor)
    }

    /// Largest whole-share quantity affordable at `price` (floor division). Returns zero
    /// shares for a non-positive price.
    pub fn shares_affordable(self, price: Price) -> Shares {
        if price.0 <= Decimal::ZERO {
            return Shares::ZERO;
        }
        let n = (self.0 / price.0).floor();
        Shares(n.to_u32().unwrap_or(0))
    }

    /// Round to whole cents for display or settlement.
    pub fn round_cents(self) -> Money {
        Money(self.0.round_dp(2))
    }
}

impl Add for Money {
    type Output = Money;
    fn add(self, rhs: Money) -> Money {
        Money(self.0 + rhs.0)
    }
}
impl Sub for Money {
    type Output = Money;
    fn sub(self, rhs: Money) -> Money {
        Money(self.0 - rhs.0)
    }
}
impl AddAssign for Money {
    fn add_assign(&mut self, rhs: Money) {
        self.0 += rhs.0;
    }
}
impl SubAssign for Money {
    fn sub_assign(&mut self, rhs: Money) {
        self.0 -= rhs.0;
    }
}
impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.round_dp(2))
    }
}

/// A strictly positive per-share price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Price(Decimal);

impl Price {
    /// Construct a price, rejecting non-positive values.
    pub fn new(value: Decimal) -> Option<Price> {
        (value > Decimal::ZERO).then_some(Price(value))
    }

    pub fn value(self) -> Decimal {
        self.0
    }

    /// Apply a relative move: `price.adjusted(Percent::from_percent(dec!(15)))` is
    /// `price × 1.15`. The caller keeps `1 + pct > 0` (always true for the infinite-buying
    /// ladder, whose most negative move is bounded by `T ≤ splits`).
    pub fn adjusted(self, pct: Percent) -> Price {
        Price(self.0 * (Decimal::ONE + pct.0))
    }

    /// Total value of `shares` at this price.
    pub fn total(self, shares: Shares) -> Money {
        Money(self.0 * Decimal::from(shares.0))
    }
}
impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A relative ratio. `from_percent(dec!(15))` and `from_ratio(dec!(0.15))` are equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Percent(Decimal);

impl Percent {
    pub fn from_ratio(ratio: Decimal) -> Percent {
        Percent(ratio)
    }
    pub fn from_percent(percent: Decimal) -> Percent {
        Percent(percent / Decimal::from(100))
    }
    pub fn ratio(self) -> Decimal {
        self.0
    }
}

/// A whole-share quantity. M1 trades whole shares only (leveraged ETFs); fractional
/// shares (a Toss feature) are a later extension.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Shares(u32);

impl Shares {
    pub const ZERO: Shares = Shares(0);

    pub fn new(count: u32) -> Shares {
        Shares(count)
    }
    pub fn get(self) -> u32 {
        self.0
    }
    pub fn is_zero(self) -> bool {
        self.0 == 0
    }
    /// Floor a fraction of these shares, e.g. `holding.fraction_floor(dec!(0.25))` for the
    /// "quarter" sell leg.
    pub fn fraction_floor(self, fraction: Decimal) -> Shares {
        let n = (Decimal::from(self.0) * fraction).floor();
        Shares(n.to_u32().unwrap_or(0))
    }
}
impl Add for Shares {
    type Output = Shares;
    fn add(self, rhs: Shares) -> Shares {
        Shares(self.0 + rhs.0)
    }
}
impl Sub for Shares {
    type Output = Shares;
    fn sub(self, rhs: Shares) -> Shares {
        Shares(self.0.saturating_sub(rhs.0))
    }
}
impl fmt::Display for Shares {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn split_divides_seed_into_tranches() {
        assert_eq!(Money::new(dec!(32000)).split(40), Money::new(dec!(800)));
    }

    #[test]
    fn split_guards_against_zero() {
        assert_eq!(Money::new(dec!(100)).split(0), Money::new(dec!(100)));
    }

    #[test]
    fn shares_affordable_floors_to_whole_shares() {
        // 800 / 105 = 7.61... -> 7 whole shares
        let budget = Money::new(dec!(800));
        let price = Price::new(dec!(105)).unwrap();
        assert_eq!(budget.shares_affordable(price), Shares::new(7));
    }

    #[test]
    fn zero_budget_affords_no_shares() {
        let price = Price::new(dec!(105)).unwrap();
        assert_eq!(Money::ZERO.shares_affordable(price), Shares::ZERO);
    }

    #[test]
    fn fraction_floor_takes_a_quarter() {
        assert_eq!(Shares::new(15).fraction_floor(dec!(0.25)), Shares::new(3));
    }

    #[test]
    fn adjusted_price_is_take_profit_target() {
        let avg = Price::new(dec!(100)).unwrap();
        assert_eq!(
            avg.adjusted(Percent::from_percent(dec!(15))),
            Price::new(dec!(115)).unwrap()
        );
    }

    #[test]
    fn price_total_is_cost_of_shares() {
        assert_eq!(
            Price::new(dec!(100)).unwrap().total(Shares::new(10)),
            Money::new(dec!(1000))
        );
    }

    #[test]
    fn percent_ratio_and_percent_agree() {
        assert_eq!(Percent::from_percent(dec!(15)).ratio(), dec!(0.15));
        assert_eq!(Percent::from_ratio(dec!(0.15)).ratio(), dec!(0.15));
    }

    #[test]
    fn price_rejects_non_positive() {
        assert!(Price::new(dec!(0)).is_none());
        assert!(Price::new(dec!(-1)).is_none());
    }
}
