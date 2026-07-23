//! Per-market max-bet cap module.
//!
//! This module implements two public entrypoints:
//!
//! * [`BettingService::set_max_bet`] — admin-only; sets or removes the
//!   maximum single-bet size for a specific market.
//! * [`BettingService::place_bet`] — validates a bet amount against the
//!   per-market cap (when set) before recording it.
//!
//! # Storage key
//!
//! The cap is stored under `DataKey::MaxBet(market_id)` in persistent
//! storage.  `None` (key absent) means "no cap — unlimited bets".
//!
//! # Overflow safety
//!
//! All arithmetic on `i128` amounts uses `checked_add` / `checked_sub`
//! and propagates [`BettingError::Overflow`] on overflow.  No `unwrap()`
//! appears in production paths.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Storage key type (mirrors the pattern in recovery.rs / storage.rs)
// ---------------------------------------------------------------------------

/// Unique key for the per-market max-bet cap in persistent storage.
///
/// This type mirrors `DataKey` in `storage.rs`; the full contract merges
/// these variants into a single enum for on-chain storage.
///
/// | Variant | Storage tier | Description |
/// |---------|-------------|-------------|
/// | `MaxBet(market_id)` | Persistent | Optional `i128` cap for market |
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BettingDataKey {
    /// Per-market maximum single-bet amount.
    MaxBet(u64),
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// A market's betting configuration, stored in persistent storage.
///
/// The `max_bet_amount` field is `None` (absent from storage) when no
/// cap has been configured for the market — all bet sizes are accepted.
///
/// When `Some(cap)`, any single bet whose `amount > cap` is rejected
/// with [`BettingError::BetExceedsMaximum`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BettingMarket {
    /// Market identifier.  Matches `DataKey::Market(id)` in the main contract.
    pub id: u64,

    /// Maximum allowed amount for a single bet, in stroops (1 XLM = 10^7 stroops).
    ///
    /// `None` means uncapped (the previous behaviour before this feature).
    /// `Some(0)` disables all betting on this market.
    pub max_bet_amount: Option<i128>,
}

/// Input data describing a bet to be placed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BetRequest {
    /// The market to bet on.
    pub market_id: u64,

    /// The participant placing the bet.
    pub bettor: String,

    /// Amount in stroops.  Must be `> 0` and `<= max_bet_amount` when set.
    pub amount: i128,

    /// The outcome the bettor is staking on.
    pub outcome: String,
}

/// A successfully placed bet, returned from [`BettingService::place_bet`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacedBet {
    /// The market the bet was placed on.
    pub market_id: u64,

    /// The participant who placed the bet.
    pub bettor: String,

    /// The validated and accepted amount.
    pub amount: i128,

    /// The outcome staked on.
    pub outcome: String,
}

/// Errors that can be returned by betting entrypoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BettingError {
    /// The supplied bet amount exceeds the per-market cap.
    ///
    /// The tuple contains `(amount, cap)` for diagnostic purposes.
    BetExceedsMaximum { amount: i128, cap: i128 },

    /// The bet amount must be strictly positive.
    InvalidAmount { amount: i128 },

    /// The cap value must be strictly positive when `Some(cap)` is supplied
    /// to `set_max_bet`.  Pass `None` to remove the cap instead.
    InvalidCap { cap: i128 },

    /// Integer overflow during an `i128` arithmetic operation.
    Overflow,

    /// The caller is not authorised to perform this admin action.
    ///
    /// In the full Soroban contract this error is unreachable because
    /// `require_admin` panics on auth failure; it is kept here so the
    /// service logic is self-contained and testable in isolation.
    Unauthorized,

    /// The market was not found in storage.
    MarketNotFound { market_id: u64 },
}

impl std::fmt::Display for BettingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BettingError::BetExceedsMaximum { amount, cap } => write!(
                f,
                "Bet amount {amount} exceeds the per-market maximum of {cap}"
            ),
            BettingError::InvalidAmount { amount } => {
                write!(f, "Bet amount must be positive, got {amount}")
            }
            BettingError::InvalidCap { cap } => write!(
                f,
                "Max-bet cap must be positive when setting a cap, got {cap}; \
                 pass None to remove the cap"
            ),
            BettingError::Overflow => {
                write!(f, "Arithmetic overflow in bet calculation")
            }
            BettingError::Unauthorized => {
                write!(f, "Admin authorisation required")
            }
            BettingError::MarketNotFound { market_id } => {
                write!(f, "Market {market_id} not found")
            }
        }
    }
}

impl std::error::Error for BettingError {}

// ---------------------------------------------------------------------------
// In-memory market store (replaces Soroban persistent storage in pure tests)
// ---------------------------------------------------------------------------

/// A minimal in-memory store that mirrors what `storage.rs` provides on-chain.
///
/// Used exclusively in unit tests; the real contract wires this up via
/// `soroban_sdk::storage::persistent_get/set`.
#[derive(Debug, Default)]
pub struct MarketStore {
    markets: std::collections::HashMap<u64, BettingMarket>,
}

impl MarketStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a market.
    pub fn insert(&mut self, market: BettingMarket) {
        self.markets.insert(market.id, market);
    }

    /// Retrieve a market by ID, returning `None` if absent.
    pub fn get(&self, market_id: u64) -> Option<&BettingMarket> {
        self.markets.get(&market_id)
    }

    /// Retrieve a mutable reference to a market.
    pub fn get_mut(&mut self, market_id: u64) -> Option<&mut BettingMarket> {
        self.markets.get_mut(&market_id)
    }
}

// ---------------------------------------------------------------------------
// Service logic
// ---------------------------------------------------------------------------

/// Stateless service layer for per-market betting cap logic.
///
/// The entrypoints are plain functions that accept an explicit
/// `&mut MarketStore` rather than an `Env` reference.  In the full
/// Soroban contract the store is replaced by `soroban_sdk::storage`
/// calls; the logic is identical.
pub struct BettingService;

impl BettingService {
    /// **Admin entrypoint** — Set or clear the maximum single-bet amount
    /// for a market.
    ///
    /// # Authorization
    ///
    /// In the full Soroban contract this function calls `require_admin(env)`
    /// which panics if the transaction was not signed by the contract admin.
    /// In this pure-Rust layer the caller must pass `is_admin = true` (the
    /// Soroban host guarantees this before dispatching).
    ///
    /// # Arguments
    ///
    /// * `store` — Mutable reference to the market store (persistent
    ///   storage in the Soroban contract).
    /// * `market_id` — The market whose cap to configure.
    /// * `max_bet` — `Some(cap)` to set a cap; `None` to remove it
    ///   (revert to unlimited).
    /// * `is_admin` — Whether the caller has admin rights (must be `true`).
    ///
    /// # Errors
    ///
    /// * [`BettingError::Unauthorized`] — `is_admin` is `false`.
    /// * [`BettingError::InvalidCap`] — `max_bet` is `Some(cap)` where
    ///   `cap <= 0`.
    /// * [`BettingError::MarketNotFound`] — no market exists for
    ///   `market_id`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use predictify_hybrid::betting::{BettingMarket, BettingService, MarketStore};
    ///
    /// let mut store = MarketStore::new();
    /// store.insert(BettingMarket { id: 1, max_bet_amount: None });
    ///
    /// // Set cap to 5_000_000 stroops (0.5 XLM).
    /// BettingService::set_max_bet(&mut store, 1, Some(5_000_000), true).unwrap();
    ///
    /// assert_eq!(store.get(1).unwrap().max_bet_amount, Some(5_000_000));
    /// ```
    pub fn set_max_bet(
        store: &mut MarketStore,
        market_id: u64,
        max_bet: Option<i128>,
        is_admin: bool,
    ) -> Result<(), BettingError> {
        // ── auth guard ────────────────────────────────────────────────────
        if !is_admin {
            return Err(BettingError::Unauthorized);
        }

        // ── validate cap value ────────────────────────────────────────────
        if let Some(cap) = max_bet {
            if cap <= 0 {
                return Err(BettingError::InvalidCap { cap });
            }
        }

        // ── update storage ────────────────────────────────────────────────
        let market = store
            .get_mut(market_id)
            .ok_or(BettingError::MarketNotFound { market_id })?;

        market.max_bet_amount = max_bet;

        tracing::info!(
            market_id,
            max_bet = ?max_bet,
            "set_max_bet: cap updated"
        );

        Ok(())
    }

    /// **Betting entrypoint** — Validate and record a bet against the
    /// per-market cap.
    ///
    /// # Cap enforcement
    ///
    /// If the market has `max_bet_amount = Some(cap)` and
    /// `request.amount > cap`, the bet is rejected with
    /// [`BettingError::BetExceedsMaximum`].  A market with
    /// `max_bet_amount = None` accepts any positive bet.
    ///
    /// # Arguments
    ///
    /// * `store` — Mutable reference to the market store.
    /// * `request` — The bet to validate and place.
    ///
    /// # Errors
    ///
    /// * [`BettingError::MarketNotFound`] — market does not exist.
    /// * [`BettingError::InvalidAmount`] — `request.amount <= 0`.
    /// * [`BettingError::BetExceedsMaximum`] — amount exceeds the cap.
    ///
    /// # Overflow safety
    ///
    /// Amount validation uses only comparisons — no arithmetic is
    /// performed on the amount at this stage, so overflow is impossible.
    ///
    /// # Example
    ///
    /// ```rust
    /// use predictify_hybrid::betting::{BetRequest, BettingMarket, BettingService, MarketStore};
    ///
    /// let mut store = MarketStore::new();
    /// store.insert(BettingMarket { id: 1, max_bet_amount: Some(10_000_000) });
    ///
    /// let req = BetRequest {
    ///     market_id: 1,
    ///     bettor: "GUSER".into(),
    ///     amount: 5_000_000,
    ///     outcome: "yes".into(),
    /// };
    ///
    /// let bet = BettingService::place_bet(&mut store, req).unwrap();
    /// assert_eq!(bet.amount, 5_000_000);
    /// ```
    pub fn place_bet(
        store: &mut MarketStore,
        request: BetRequest,
    ) -> Result<PlacedBet, BettingError> {
        // ── validate amount is positive ───────────────────────────────────
        if request.amount <= 0 {
            return Err(BettingError::InvalidAmount {
                amount: request.amount,
            });
        }

        // ── load market ───────────────────────────────────────────────────
        let market = store
            .get(request.market_id)
            .ok_or(BettingError::MarketNotFound {
                market_id: request.market_id,
            })?;

        // ── enforce cap (overflow-safe: pure comparison, no arithmetic) ───
        if let Some(cap) = market.max_bet_amount {
            if request.amount > cap {
                return Err(BettingError::BetExceedsMaximum {
                    amount: request.amount,
                    cap,
                });
            }
        }

        tracing::info!(
            market_id = request.market_id,
            bettor = %request.bettor,
            amount = request.amount,
            outcome = %request.outcome,
            "place_bet: accepted"
        );

        Ok(PlacedBet {
            market_id: request.market_id,
            bettor: request.bettor,
            amount: request.amount,
            outcome: request.outcome,
        })
    }

    /// Retrieve the current max-bet cap for a market.
    ///
    /// Returns `None` if the market has no cap, or `None` if the market
    /// does not exist.
    pub fn get_max_bet(store: &MarketStore, market_id: u64) -> Option<i128> {
        store.get(market_id)?.max_bet_amount
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── helpers ──────────────────────────────────────────────────────────

    /// Build a store with one market whose cap is `max_bet_amount`.
    fn store_with_market(market_id: u64, max_bet_amount: Option<i128>) -> MarketStore {
        let mut store = MarketStore::new();
        store.insert(BettingMarket {
            id: market_id,
            max_bet_amount,
        });
        store
    }

    fn bet(market_id: u64, amount: i128) -> BetRequest {
        BetRequest {
            market_id,
            bettor: "GBETTOR".to_string(),
            amount,
            outcome: "yes".to_string(),
        }
    }

    // ── set_max_bet ───────────────────────────────────────────────────────

    #[test]
    fn set_max_bet_sets_cap_for_admin() {
        let mut store = store_with_market(1, None);

        BettingService::set_max_bet(&mut store, 1, Some(10_000_000), true).unwrap();

        assert_eq!(store.get(1).unwrap().max_bet_amount, Some(10_000_000));
    }

    #[test]
    fn set_max_bet_removes_cap_when_none() {
        let mut store = store_with_market(1, Some(5_000_000));

        BettingService::set_max_bet(&mut store, 1, None, true).unwrap();

        assert_eq!(store.get(1).unwrap().max_bet_amount, None);
    }

    #[test]
    fn set_max_bet_rejects_non_admin() {
        let mut store = store_with_market(1, None);

        let err = BettingService::set_max_bet(&mut store, 1, Some(1_000_000), false)
            .unwrap_err();

        assert_eq!(err, BettingError::Unauthorized);
    }

    #[test]
    fn set_max_bet_rejects_zero_cap() {
        let mut store = store_with_market(1, None);

        let err = BettingService::set_max_bet(&mut store, 1, Some(0), true)
            .unwrap_err();

        assert_eq!(err, BettingError::InvalidCap { cap: 0 });
    }

    #[test]
    fn set_max_bet_rejects_negative_cap() {
        let mut store = store_with_market(1, None);

        let err = BettingService::set_max_bet(&mut store, 1, Some(-1), true)
            .unwrap_err();

        assert_eq!(err, BettingError::InvalidCap { cap: -1 });
    }

    #[test]
    fn set_max_bet_rejects_unknown_market() {
        let mut store = MarketStore::new();

        let err = BettingService::set_max_bet(&mut store, 99, Some(1_000_000), true)
            .unwrap_err();

        assert_eq!(err, BettingError::MarketNotFound { market_id: 99 });
    }

    #[test]
    fn set_max_bet_overwrites_existing_cap() {
        let mut store = store_with_market(1, Some(5_000_000));

        BettingService::set_max_bet(&mut store, 1, Some(20_000_000), true).unwrap();

        assert_eq!(store.get(1).unwrap().max_bet_amount, Some(20_000_000));
    }

    // ── place_bet — happy path ─────────────────────────────────────────────

    #[test]
    fn place_bet_accepts_bet_exactly_at_cap() {
        let mut store = store_with_market(1, Some(10_000_000));
        let placed = BettingService::place_bet(&mut store, bet(1, 10_000_000)).unwrap();
        assert_eq!(placed.amount, 10_000_000);
    }

    #[test]
    fn place_bet_accepts_bet_below_cap() {
        let mut store = store_with_market(1, Some(10_000_000));
        let placed = BettingService::place_bet(&mut store, bet(1, 1)).unwrap();
        assert_eq!(placed.amount, 1);
    }

    #[test]
    fn place_bet_accepts_any_amount_when_no_cap() {
        let mut store = store_with_market(1, None);
        let placed = BettingService::place_bet(&mut store, bet(1, i128::MAX)).unwrap();
        assert_eq!(placed.amount, i128::MAX);
    }

    #[test]
    fn place_bet_returns_correct_placed_bet_fields() {
        let mut store = store_with_market(42, Some(100_000_000));
        let req = BetRequest {
            market_id: 42,
            bettor: "GPARTICIPANT".to_string(),
            amount: 50_000_000,
            outcome: "no".to_string(),
        };
        let placed = BettingService::place_bet(&mut store, req).unwrap();

        assert_eq!(placed.market_id, 42);
        assert_eq!(placed.bettor, "GPARTICIPANT");
        assert_eq!(placed.amount, 50_000_000);
        assert_eq!(placed.outcome, "no");
    }

    // ── place_bet — cap enforcement ────────────────────────────────────────

    #[test]
    fn place_bet_rejects_bet_above_cap() {
        let mut store = store_with_market(1, Some(10_000_000));

        let err = BettingService::place_bet(&mut store, bet(1, 10_000_001)).unwrap_err();

        assert_eq!(
            err,
            BettingError::BetExceedsMaximum {
                amount: 10_000_001,
                cap: 10_000_000,
            }
        );
    }

    #[test]
    fn place_bet_rejects_bet_when_cap_is_one_stroop() {
        let mut store = store_with_market(1, Some(1));

        // Amount = 1 is exactly at cap: accepted.
        BettingService::place_bet(&mut store, bet(1, 1)).unwrap();

        // Amount = 2 exceeds cap: rejected.
        let err = BettingService::place_bet(&mut store, bet(1, 2)).unwrap_err();
        assert_eq!(
            err,
            BettingError::BetExceedsMaximum { amount: 2, cap: 1 }
        );
    }

    #[test]
    fn place_bet_enforces_updated_cap() {
        let mut store = store_with_market(1, Some(10_000_000));

        // Bet of 10_000_000 is accepted initially.
        BettingService::place_bet(&mut store, bet(1, 10_000_000)).unwrap();

        // Admin lowers cap.
        BettingService::set_max_bet(&mut store, 1, Some(5_000_000), true).unwrap();

        // Now the same amount is rejected.
        let err = BettingService::place_bet(&mut store, bet(1, 10_000_000)).unwrap_err();
        assert_eq!(
            err,
            BettingError::BetExceedsMaximum {
                amount: 10_000_000,
                cap: 5_000_000,
            }
        );
    }

    #[test]
    fn place_bet_allowed_after_cap_removed() {
        let mut store = store_with_market(1, Some(1_000_000));

        // Large amount rejected while cap is set.
        BettingService::place_bet(&mut store, bet(1, 2_000_000)).unwrap_err();

        // Admin removes cap.
        BettingService::set_max_bet(&mut store, 1, None, true).unwrap();

        // Same amount now accepted.
        BettingService::place_bet(&mut store, bet(1, 2_000_000)).unwrap();
    }

    // ── place_bet — invalid amount ─────────────────────────────────────────

    #[test]
    fn place_bet_rejects_zero_amount() {
        let mut store = store_with_market(1, None);

        let err = BettingService::place_bet(&mut store, bet(1, 0)).unwrap_err();

        assert_eq!(err, BettingError::InvalidAmount { amount: 0 });
    }

    #[test]
    fn place_bet_rejects_negative_amount() {
        let mut store = store_with_market(1, None);

        let err = BettingService::place_bet(&mut store, bet(1, -1)).unwrap_err();

        assert_eq!(err, BettingError::InvalidAmount { amount: -1 });
    }

    #[test]
    fn place_bet_rejects_i128_min_amount() {
        let mut store = store_with_market(1, None);

        let err = BettingService::place_bet(&mut store, bet(1, i128::MIN)).unwrap_err();

        assert_eq!(err, BettingError::InvalidAmount { amount: i128::MIN });
    }

    // ── place_bet — market not found ───────────────────────────────────────

    #[test]
    fn place_bet_rejects_unknown_market() {
        let mut store = MarketStore::new();

        let err = BettingService::place_bet(&mut store, bet(99, 1_000)).unwrap_err();

        assert_eq!(err, BettingError::MarketNotFound { market_id: 99 });
    }

    // ── get_max_bet ────────────────────────────────────────────────────────

    #[test]
    fn get_max_bet_returns_current_cap() {
        let store = store_with_market(1, Some(7_000_000));
        assert_eq!(BettingService::get_max_bet(&store, 1), Some(7_000_000));
    }

    #[test]
    fn get_max_bet_returns_none_when_no_cap() {
        let store = store_with_market(1, None);
        assert_eq!(BettingService::get_max_bet(&store, 1), None);
    }

    #[test]
    fn get_max_bet_returns_none_for_unknown_market() {
        let store = MarketStore::new();
        assert_eq!(BettingService::get_max_bet(&store, 99), None);
    }

    // ── error display ──────────────────────────────────────────────────────

    #[test]
    fn error_display_bet_exceeds_maximum() {
        let err = BettingError::BetExceedsMaximum { amount: 200, cap: 100 };
        let msg = err.to_string();
        assert!(msg.contains("200"));
        assert!(msg.contains("100"));
    }

    #[test]
    fn error_display_invalid_amount() {
        let err = BettingError::InvalidAmount { amount: -5 };
        assert!(err.to_string().contains("-5"));
    }

    #[test]
    fn error_display_invalid_cap() {
        let err = BettingError::InvalidCap { cap: 0 };
        assert!(err.to_string().contains("0"));
    }

    #[test]
    fn error_display_market_not_found() {
        let err = BettingError::MarketNotFound { market_id: 42 };
        assert!(err.to_string().contains("42"));
    }

    #[test]
    fn error_display_overflow() {
        assert!(BettingError::Overflow.to_string().contains("overflow"));
    }

    #[test]
    fn error_display_unauthorized() {
        assert!(BettingError::Unauthorized.to_string().contains("Admin"));
    }

    // ── multiple markets are independent ──────────────────────────────────

    #[test]
    fn caps_are_independent_per_market() {
        let mut store = MarketStore::new();
        store.insert(BettingMarket { id: 1, max_bet_amount: Some(1_000_000) });
        store.insert(BettingMarket { id: 2, max_bet_amount: Some(5_000_000) });

        // Market 1: amount 1_000_000 accepted, 1_000_001 rejected.
        BettingService::place_bet(&mut store, bet(1, 1_000_000)).unwrap();
        BettingService::place_bet(&mut store, bet(1, 1_000_001)).unwrap_err();

        // Market 2: same amounts — different cap applies.
        BettingService::place_bet(&mut store, bet(2, 1_000_001)).unwrap();
        BettingService::place_bet(&mut store, bet(2, 5_000_001)).unwrap_err();
    }
}
