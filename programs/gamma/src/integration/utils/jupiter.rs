use anchor_lang::AccountDeserialize;
use anyhow::{anyhow, Context, Result};

use gamma_deseralize_pool_state::PoolState;
use jupiter_amm_interface::{
    try_get_account_data, AccountMap, Amm, AmmContext, KeyedAccount, Quote, QuoteParams,
    SwapAndAccountMetas, SwapParams,
};
use rust_decimal::prelude::FromPrimitive;
use solana_pubkey::Pubkey;
use spl_token_2022_interface::extension::BaseStateWithExtensions;
use spl_token_2022_interface::extension::{
    transfer_fee::TransferFeeConfig, StateWithExtensions, StateWithExtensionsOwned,
};
use spl_token_2022_interface as spl_token_2022;
use spl_token_2022_interface::state::Mint;
use std::sync::atomic::{AtomicI64, AtomicU64};
use std::sync::Arc;

use anchor_lang::ToAccountMetas;
// Add to Cargo.toml
// gamma-os = { package = "gamma", git = "https://github.com/GooseFX1/gamma-swap", branch = "master" }
use crate::{
    curve::{ConstantProductCurve, CurveCalculator, SwapResult, TradeDirection},
    fees::{ceil_div, DynamicFee, FeeType, StaticFee, FEE_RATE_DENOMINATOR_VALUE},
    states::{AmmConfig, ObservationState, PoolStatusBitIndex},
    AUTH_SEED,
};

#[derive(Clone)]
pub struct TokenMints {
    token0: Pubkey,
    token1: Pubkey,
    token0_mint: StateWithExtensionsOwned<Mint>,
    token1_mint: StateWithExtensionsOwned<Mint>,
    token0_program: Pubkey,
    token1_program: Pubkey,
}

#[derive(Clone)]
pub struct Gamma {
    key: Pubkey,
    pool_state: PoolState,
    amm_config: Option<AmmConfig>,
    vault_0_amount: Option<u64>,
    vault_1_amount: Option<u64>,
    token_mints_and_token_programs: Option<TokenMints>,
    epoch: Arc<AtomicU64>,
    timestamp: Arc<AtomicI64>,
    observation_state: Option<ObservationState>,
}

impl Gamma {
    fn get_authority(&self) -> Pubkey {
        Pubkey::create_program_address(
            &[AUTH_SEED.as_bytes(), &[self.pool_state.auth_bump]],
            &crate::ID,
        )
        .unwrap()
    }
}

impl Amm for Gamma {
    fn from_keyed_account(keyed_account: &KeyedAccount, amm_context: &AmmContext) -> Result<Self> {
        let pool_state = PoolState::deserialize_account(&mut keyed_account.account.data.as_ref())?;

        Ok(Self {
            key: keyed_account.key,
            pool_state,
            amm_config: None,
            vault_0_amount: None,
            vault_1_amount: None,
            token_mints_and_token_programs: None,
            epoch: amm_context.clock_ref.epoch.clone(),
            timestamp: amm_context.clock_ref.unix_timestamp.clone(),
            observation_state: None,
        })
    }

    fn label(&self) -> String {
        "GAMMA".into()
    }

    fn program_id(&self) -> Pubkey {
        crate::ID
    }

    fn key(&self) -> Pubkey {
        self.key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        vec![self.pool_state.token_0_mint, self.pool_state.token_1_mint]
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        let mut keys = vec![
            self.key,
            self.pool_state.token_0_vault,
            self.pool_state.token_1_vault,
            self.pool_state.amm_config,
        ];
        keys.extend([self.pool_state.token_0_mint, self.pool_state.token_1_mint]);
        keys
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        let pool_state_data = try_get_account_data(account_map, &self.key)?;
        self.pool_state = PoolState::deserialize_account(&mut pool_state_data.as_ref())?;

        let token0_mint = try_get_account_data(account_map, &self.pool_state.token_0_mint)
            .ok()
            .and_then(|account_data| {
                StateWithExtensionsOwned::<spl_token_2022::state::Mint>::unpack(
                    account_data.to_vec(),
                )
                .ok()
            })
            .context("Token 0 mint not found")?;

        let token1_mint = try_get_account_data(account_map, &self.pool_state.token_1_mint)
            .ok()
            .and_then(|account_data| {
                StateWithExtensionsOwned::<spl_token_2022::state::Mint>::unpack(
                    account_data.to_vec(),
                )
                .ok()
            })
            .context("Token 1 mint not found")?;

        self.token_mints_and_token_programs = Some(TokenMints {
            token0: self.pool_state.token_0_mint,
            token1: self.pool_state.token_1_mint,
            token0_mint,
            token1_mint,
            token0_program: self.pool_state.token_0_program,
            token1_program: self.pool_state.token_1_program,
        });

        let amm_config_data = try_get_account_data(account_map, &self.pool_state.amm_config)?;
        self.amm_config = Some(AmmConfig::try_deserialize(&mut amm_config_data.as_ref())?);

        let get_unfrozen_token_amount = |token_vault| {
            try_get_account_data(account_map, token_vault)
                .ok()
                .and_then(|account_data| {
                    StateWithExtensions::<spl_token_2022::state::Account>::unpack(account_data).ok()
                })
                .and_then(|token_account| {
                    if token_account.base.is_frozen() {
                        None
                    } else {
                        Some(token_account.base.amount)
                    }
                })
        };

        let observation_state =
            try_get_account_data(account_map, &self.pool_state.observation_key)?;
        self.observation_state = Some(ObservationState::try_deserialize(
            &mut observation_state.as_ref(),
        )?);

        self.vault_0_amount = get_unfrozen_token_amount(&self.pool_state.token_0_vault);
        self.vault_1_amount = get_unfrozen_token_amount(&self.pool_state.token_1_vault);

        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        if !self.pool_state.get_status_by_bit(PoolStatusBitIndex::Swap)
            || (self.timestamp.load(std::sync::atomic::Ordering::Relaxed) as u64)
                < self.pool_state.open_time
        {
            return Err(anyhow!("Pool is not trading"));
        }

        let amm_config = self.amm_config.as_ref().context("Missing AmmConfig")?;

        let zero_for_one: bool = quote_params.input_mint == self.pool_state.token_0_mint;

        if self.token_mints_and_token_programs.is_none() {
            return Err(anyhow!("Missing token mints and token programs"));
        }

        let TokenMints {
            token0_mint: token_mint_0,
            token1_mint: token_mint_1,
            ..
        } = self
            .token_mints_and_token_programs
            .as_ref()
            .ok_or(anyhow!("Missing token mints and token programs"))?;

        let token_mint_0_transfer_fee_config: Option<_> =
            token_mint_0.get_extension::<TransferFeeConfig>().ok();
        let token_mint_1_transfer_fee_config =
            token_mint_1.get_extension::<TransferFeeConfig>().ok();

        let (source_mint_transfer_fee_config, destination_mint_transfer_fee_config) =
            if zero_for_one {
                (
                    token_mint_0_transfer_fee_config,
                    token_mint_1_transfer_fee_config,
                )
            } else {
                (
                    token_mint_1_transfer_fee_config,
                    token_mint_0_transfer_fee_config,
                )
            };

        let amount = quote_params.amount;
        let epoch = self.epoch.load(std::sync::atomic::Ordering::Relaxed);

        let actual_amount_in = if let Some(transfer_fee_config) = source_mint_transfer_fee_config {
            amount.saturating_sub(
                transfer_fee_config
                    .calculate_epoch_fee(epoch, amount)
                    .context("Fee calculation failure")?,
            )
        } else {
            amount
        };
        if actual_amount_in == 0 {
            return Err(anyhow!("Amount too low"));
        }

        // Calculate the trade amounts
        let (total_token_0_amount, total_token_1_amount) =
            self.pool_state.vault_amount_without_fee()?;

        let result = OracleBasedSwapCalculator::swap_base_input(
            actual_amount_in.into(),
            if zero_for_one {
                total_token_0_amount.into()
            } else {
                total_token_1_amount.into()
            },
            if zero_for_one {
                total_token_1_amount.into()
            } else {
                total_token_0_amount.into()
            },
            &amm_config,
            &self.pool_state,
            self.timestamp.load(std::sync::atomic::Ordering::Relaxed) as u64,
            self.observation_state
                .as_ref()
                .context("Missing observation state")?,
            false,
        )
        .context("swap failed")?;

        let amount_out: u64 = result.destination_amount_swapped.try_into()?;
        let actual_amount_out =
            if let Some(transfer_fee_config) = destination_mint_transfer_fee_config {
                amount_out.saturating_sub(
                    transfer_fee_config
                        .calculate_epoch_fee(epoch, amount_out)
                        .context("Fee calculation failure")?,
                )
            } else {
                amount_out
            };

        Ok(Quote {
            in_amount: actual_amount_in,
            out_amount: actual_amount_out,
            fee_mint: quote_params.input_mint,
            fee_amount: result.dynamic_fee as u64,
            // our understanding is this is the fee percentage of the input amount
            fee_pct: rust_decimal::Decimal::from_u128(result.dynamic_fee)
                .ok_or(anyhow!("Math overflow"))?
                .checked_div(
                    rust_decimal::Decimal::from_u64(actual_amount_in)
                        .ok_or(anyhow!("Math overflow"))?,
                )
                .context("Failed to divide")?,
            ..Default::default()
        })
    }

    fn get_accounts_len(&self) -> usize {
        14
    }

    fn get_swap_and_account_metas(&self, swap_params: &SwapParams) -> Result<SwapAndAccountMetas> {
        if self.token_mints_and_token_programs.is_none() {
            return Err(anyhow!("Missing token mints and token programs"));
        }

        let TokenMints {
            token0_program: token_0_token_program,
            token1_program: token_1_token_program,
            ..
        } = self
            .token_mints_and_token_programs
            .as_ref()
            .ok_or(anyhow!("Missing token mints and token programs"))?;

        let (
            input_token_program,
            input_vault,
            input_token_mint,
            output_token_program,
            output_vault,
            output_token_mint,
        ) = if swap_params.source_mint == self.pool_state.token_0_mint {
            (
                *token_0_token_program,
                self.pool_state.token_0_vault,
                self.pool_state.token_0_mint,
                *token_1_token_program,
                self.pool_state.token_1_vault,
                self.pool_state.token_1_mint,
            )
        } else {
            (
                *token_1_token_program,
                self.pool_state.token_1_vault,
                self.pool_state.token_1_mint,
                *token_0_token_program,
                self.pool_state.token_0_vault,
                self.pool_state.token_0_mint,
            )
        };

        let account_metas = crate::accounts::Swap {
            payer: swap_params.token_transfer_authority,
            authority: self.get_authority(),
            amm_config: self.pool_state.amm_config,
            pool_state: self.key,
            input_token_account: swap_params.source_token_account,
            output_token_account: swap_params.destination_token_account,
            input_vault,
            output_vault,
            input_token_program,
            output_token_program,
            input_token_mint,
            output_token_mint,
            observation_state: self.pool_state.observation_key,
        }
        .to_account_metas(None);
        // The discriminator for the new instruction is
        // "discriminator": [239, 82, 192, 187, 160, 26, 223, 223],
        // Everything else is the same as the old instruction.

        unimplemented!()
        // Ok(SwapAndAccountMetas {
        //     swap: Swap::Gamma, // TODO: Add Gamma as option.
        //     account_metas,
        // })
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }
}
// Price scaled to 9 decimal places
pub const D9: u128 = 1_000_000_000;
const D9_TIMES_D9: u128 = D9 * D9;

pub struct OracleBasedSwapCalculator {}

impl OracleBasedSwapCalculator {
    /// Get the amount to be swapped at oracle price without reaching the acceptable price difference.
    pub fn get_amount_to_be_swapped_at_oracle_price(
        source_amount_to_be_swapped: u128,
        swap_source_amount: u128,
        swap_destination_amount: u128,
        // If swap is happening from x->y price is y/x
        // If swap is happening from y->x Price is x/y
        oracle_price: u128,
        pool_state: &PoolState,
    ) -> Result<u128> {
        let max_amount_swappable_at_oracle_price = swap_source_amount
            .checked_mul(pool_state.max_amount_swappable_at_oracle_price.into())
            .ok_or(anyhow!("Math overflow"))?
            .checked_div(FEE_RATE_DENOMINATOR_VALUE.into())
            .ok_or(anyhow!("Math overflow"))?;

        // Max amount that can be swapped without reaching the acceptable price difference limit
        let price_difference_limit = FEE_RATE_DENOMINATOR_VALUE
            .checked_sub(pool_state.acceptable_price_difference.into())
            .ok_or(anyhow!("Math overflow"))?;
        // We can swap with oracle price, P until we reach spot_price_at_acceptable_price_difference_limit Z
        // We want to calculate the spot_price_at_acceptable_price_difference_limit that is away from current oracle_price and not current spot_price.
        let spot_price_at_acceptable_price_difference_limit = oracle_price
            .checked_mul(price_difference_limit.into())
            .ok_or(anyhow!("Math overflow"))?
            .checked_div(FEE_RATE_DENOMINATOR_VALUE.into())
            .ok_or(anyhow!("Math overflow"))?;

        // Max tradeable amount with price Oracle Price P before we reach spot_price_at_acceptable_price_difference_limit Z
        // Can we derived by the formula:
        // x_delta_max = (|(Z*X) - Y)| / (Z + P)
        let z_times_x = spot_price_at_acceptable_price_difference_limit
            .checked_mul(swap_source_amount)
            .ok_or(anyhow!("Math overflow"))?;
        let y_scaled_by_d9 = swap_destination_amount
            .checked_mul(D9)
            .ok_or(anyhow!("Math overflow"))?;

        // numerator = |(Z*X) - Y|
        let numerator = z_times_x.abs_diff(y_scaled_by_d9);
        // denominator = Z + P
        let denominator = oracle_price
            .checked_add(spot_price_at_acceptable_price_difference_limit)
            .ok_or(anyhow!("Math overflow"))?;

        let max_amount_swappable_at_oracle_price_without_reaching_acceptable_price_difference =
            numerator
                .checked_div(denominator)
                .ok_or(anyhow!("Math overflow"))?;

        let max_swap_at_oracle_price = std::cmp::min(
            max_amount_swappable_at_oracle_price,
            max_amount_swappable_at_oracle_price_without_reaching_acceptable_price_difference,
        );

        Ok(std::cmp::min(
            max_swap_at_oracle_price,
            source_amount_to_be_swapped,
        ))
    }

    pub fn get_spot_price_and_oracle_price_rate_difference(
        oracle_price: u128,
        spot_price: u128,
    ) -> Result<u128> {
        let difference_in_oracle_price = spot_price.abs_diff(oracle_price);
        let rate_difference = difference_in_oracle_price
            .checked_mul(FEE_RATE_DENOMINATOR_VALUE.into())
            .ok_or(anyhow!("Math overflow"))?
            .checked_div(oracle_price)
            .ok_or(anyhow!("Math overflow"))?;

        Ok(rate_difference)
    }

    /// Dynamic fee changed for oracle based swaps
    /// If an oracle swap is changing price, and making it move away from oracle price we can charge extra fees.
    fn get_drift_fee(
        amount_in_swappable_at_oracle_price: u128,
        swap_source_amount: u128,
        swap_destination_amount: u128,
        oracle_price: u128,
        pool_state: &PoolState,
    ) -> Result<u64> {
        // HERE TO assume that all the amount is being swapped, and fee is not deducted before the swap.
        // This means we have a very small deviation, for the drift factor, but one that makes it bigger, meaning the fee higher so it is fine.
        let amount_out = oracle_price
            .checked_mul(amount_in_swappable_at_oracle_price)
            .ok_or(anyhow!("Math overflow"))?
            .checked_div(D9)
            .ok_or(anyhow!("Math overflow"))?;
        let new_swap_source_amount = swap_source_amount
            .checked_add(amount_in_swappable_at_oracle_price)
            .ok_or(anyhow!("Math overflow"))?;
        let new_swap_destination_amount = swap_destination_amount
            .checked_sub(amount_out)
            .ok_or(anyhow!("Math overflow"))?;

        let new_spot_price = new_swap_destination_amount
            .checked_mul(D9)
            .ok_or(anyhow!("Math overflow"))?
            .checked_div(new_swap_source_amount)
            .ok_or(anyhow!("Math overflow"))?;

        let difference_in_oracle_price =
            OracleBasedSwapCalculator::get_spot_price_and_oracle_price_rate_difference(
                oracle_price,
                new_spot_price,
            )?;

        let drift_fee = difference_in_oracle_price
            .checked_mul(pool_state.drift_factor.into())
            .ok_or(anyhow!("Math overflow"))?;

        let final_drift_fee = std::cmp::min(pool_state.max_drift_factor.into(), drift_fee);

        Ok(final_drift_fee as u64)
    }

    fn get_price_delay_fee(current_time: u64, pool_state: &PoolState) -> Result<u64> {
        let difference = current_time.saturating_sub(pool_state.oracle_price_updated_at);
        let price_delay_fee = difference
            .checked_mul(pool_state.oracle_price_delay_fee_rate_per_second.into())
            .ok_or(anyhow!("Math overflow"))?;

        let final_price_delay_fee = std::cmp::min(
            pool_state.max_oracle_price_delay_fee.into(),
            price_delay_fee,
        );

        Ok(final_price_delay_fee as u64)
    }

    /// Subtract fees and calculate how much destination token will be received
    /// for a given amount of source token
    pub fn swap_base_input(
        source_amount_to_be_swapped: u128,
        swap_source_amount: u128,
        swap_destination_amount: u128,
        amm_config: &AmmConfig,
        pool_state: &PoolState,
        block_timestamp: u64,
        observation_state: &ObservationState,
        is_invoked_by_signed_segmenter: bool,
    ) -> Result<SwapResult> {
        let oracle_price_updated_at = pool_state.oracle_price_updated_at;
        let difference = block_timestamp.saturating_sub(oracle_price_updated_at);
        let pool_state_gamma_os: crate::states::PoolState = pool_state.into();

        if difference > pool_state.max_oracle_price_update_time_diff as u64
            || block_timestamp < oracle_price_updated_at
            || oracle_price_updated_at == 0
            || pool_state.oracle_price_token_0_by_token_1 == 0
        {
            return Ok(CurveCalculator::swap_base_input(
                source_amount_to_be_swapped,
                swap_source_amount,
                swap_destination_amount,
                amm_config,
                &pool_state_gamma_os,
                block_timestamp,
                observation_state,
                is_invoked_by_signed_segmenter,
            )?);
        }

        let vault_amounts = pool_state.vault_amount_without_fee()?;
        let trade_direction = if swap_source_amount == vault_amounts.0 as u128 {
            TradeDirection::ZeroForOne
        } else {
            TradeDirection::OneForZero
        };

        // We always take the price to be opposite of the trade direction
        // If swap is happening from x->y price is y/x
        // If swap is happening from y->x Price is x/y
        let spot_price = swap_destination_amount
            .checked_mul(D9)
            .ok_or(anyhow!("Math overflow"))?
            .checked_div(swap_source_amount)
            .ok_or(anyhow!("Math overflow"))?;

        let oracle_price = match trade_direction {
            TradeDirection::OneForZero => pool_state.oracle_price_token_0_by_token_1,
            TradeDirection::ZeroForOne => D9_TIMES_D9
                .checked_div(pool_state.oracle_price_token_0_by_token_1)
                .ok_or(anyhow!("Math overflow"))?,
        };

        let rate_difference =
            Self::get_spot_price_and_oracle_price_rate_difference(oracle_price, spot_price)?;

        // If spot price is better than oracle price, in that case we want to prevent anyone from swapping with the curve calculator.
        // To prevent any losses from arbitrage, and possible LP losses because of that.
        let swap_in_curve = if spot_price > oracle_price {
            false
        } else {
            rate_difference > pool_state.acceptable_price_difference as u128
        };
        if swap_in_curve {
            // If the price difference between pool and oracle is too high, we will use the old calculator.
            return Ok(CurveCalculator::swap_base_input(
                source_amount_to_be_swapped,
                swap_source_amount,
                swap_destination_amount,
                amm_config,
                &pool_state_gamma_os,
                block_timestamp,
                observation_state,
                is_invoked_by_signed_segmenter,
            )?);
        }

        let amount_to_be_swapped_at_oracle_price = Self::get_amount_to_be_swapped_at_oracle_price(
            source_amount_to_be_swapped,
            swap_source_amount,
            swap_destination_amount,
            oracle_price,
            pool_state,
        )?;
        let amount_to_be_swapped_with_invariant_curve = source_amount_to_be_swapped
            .checked_sub(amount_to_be_swapped_at_oracle_price)
            .ok_or(anyhow!("Math overflow"))?;

        if amount_to_be_swapped_at_oracle_price == 0 {
            return Ok(CurveCalculator::swap_base_input(
                source_amount_to_be_swapped,
                swap_source_amount,
                swap_destination_amount,
                amm_config,
                &pool_state_gamma_os,
                block_timestamp,
                observation_state,
                is_invoked_by_signed_segmenter,
            )?);
        }

        let dynamic_fee_rate = DynamicFee::dynamic_fee_rate(
            block_timestamp,
            observation_state,
            FeeType::Volatility,
            amm_config.trade_fee_rate,
            &pool_state_gamma_os,
            is_invoked_by_signed_segmenter,
        )?;

        let trade_rate_on_amount_to_be_swapped_at_oracle_price = std::cmp::max(
            dynamic_fee_rate,
            pool_state.min_trade_rate_at_oracle_price.into(),
        );
        let drift_fee = Self::get_drift_fee(
            amount_to_be_swapped_at_oracle_price,
            swap_source_amount,
            swap_destination_amount,
            oracle_price,
            pool_state,
        )?;
        let price_delay_fee = Self::get_price_delay_fee(block_timestamp, pool_state)?;
        let trade_rate_on_amount_to_be_swapped_at_oracle_price =
            trade_rate_on_amount_to_be_swapped_at_oracle_price
                .checked_add(drift_fee)
                .ok_or(anyhow!("Math overflow"))?
                .checked_add(price_delay_fee)
                .ok_or(anyhow!("Math overflow"))?;

        let trade_fees_for_oracle_swap = ceil_div(
            amount_to_be_swapped_at_oracle_price.into(),
            trade_rate_on_amount_to_be_swapped_at_oracle_price.into(),
            FEE_RATE_DENOMINATOR_VALUE.into(),
        )
        .ok_or(anyhow!("Math overflow"))?;

        let source_amount_to_be_swapped_after_fees = amount_to_be_swapped_at_oracle_price
            .checked_sub(trade_fees_for_oracle_swap)
            .ok_or(anyhow!("Math overflow"))?;

        let execution_oracle_price = oracle_price;

        // The price is Y/X, we have delta_x, so to find y, we need to do y = delta_x * price
        // Since price was scaled by D9, we need to scale down by D9
        let output_tokens = execution_oracle_price
            .checked_mul(source_amount_to_be_swapped_after_fees)
            .ok_or(anyhow!("Math overflow"))?
            .checked_div(D9)
            .ok_or(anyhow!("Math overflow"))?;

        let new_swap_source_amount = swap_source_amount
            .checked_add(amount_to_be_swapped_at_oracle_price)
            .ok_or(anyhow!("Math overflow"))?;

        let new_swap_destination_amount = swap_destination_amount
            .checked_sub(output_tokens)
            .ok_or(anyhow!("Math overflow"))?;

        let trade_fees_for_invariant_curve = ceil_div(
            amount_to_be_swapped_with_invariant_curve.into(),
            dynamic_fee_rate.into(),
            FEE_RATE_DENOMINATOR_VALUE.into(),
        )
        .ok_or(anyhow!("Math overflow"))?;

        let source_amount_after_fees = amount_to_be_swapped_with_invariant_curve
            .checked_sub(trade_fees_for_invariant_curve)
            .ok_or(anyhow!("Math overflow"))?;
        let trade_fee_charged = trade_fees_for_invariant_curve
            .checked_add(trade_fees_for_oracle_swap)
            .ok_or(anyhow!("Math overflow"))?;

        let trade_fee_rate = trade_fee_charged
            .checked_mul(FEE_RATE_DENOMINATOR_VALUE.into())
            .ok_or(anyhow!("Math overflow"))?
            .checked_div(source_amount_to_be_swapped)
            .ok_or(anyhow!("Math overflow"))?;

        let destination_amount_swapped_with_curve_calculator =
            ConstantProductCurve::swap_base_input_without_fees(
                source_amount_after_fees,
                new_swap_source_amount,
                new_swap_destination_amount,
            )?;

        #[cfg(feature = "enable-log")]
        msg!(
            "trade_fee_charged: {}, trade_fee_rate: {}",
            trade_fee_charged,
            trade_fee_rate
        );
        let destination_amount_swapped = destination_amount_swapped_with_curve_calculator
            .checked_add(output_tokens)
            .ok_or(anyhow!("Math overflow"))?;

        let protocol_fee = StaticFee::protocol_fee(trade_fee_charged, amm_config.protocol_fee_rate)
            .ok_or(anyhow!("Invalid fee"))?;
        let fund_fee = StaticFee::fund_fee(trade_fee_charged, amm_config.fund_fee_rate)
            .ok_or(anyhow!("Invalid fee"))?;

        let new_swap_source_amount = swap_source_amount
            .checked_add(source_amount_to_be_swapped)
            .ok_or(anyhow!("Math overflow"))?;

        let new_swap_destination_amount = swap_destination_amount
            .checked_sub(destination_amount_swapped)
            .ok_or(anyhow!("Math overflow"))?;

        let spot_price_after_swap = new_swap_destination_amount
            .checked_mul(D9)
            .ok_or(anyhow!("Math overflow"))?
            .checked_div(new_swap_source_amount)
            .ok_or(anyhow!("Math overflow"))?;

        // make sure both are configured, i.e we have a price range min and max
        if pool_state.min_swap_at_spot_price > 0 && pool_state.max_swap_at_spot_price > 0 {
            let min_swap_at_spot_price = match trade_direction {
                TradeDirection::OneForZero => pool_state.min_swap_at_spot_price as u128,
                TradeDirection::ZeroForOne => D9_TIMES_D9
                    .checked_div(pool_state.min_swap_at_spot_price as u128)
                    .ok_or(anyhow!("Math overflow"))?,
            };

            let max_swap_at_spot_price = match trade_direction {
                TradeDirection::OneForZero => pool_state.max_swap_at_spot_price as u128,
                TradeDirection::ZeroForOne => D9_TIMES_D9
                    .checked_div(pool_state.max_swap_at_spot_price as u128)
                    .ok_or(anyhow!("Math overflow"))?,
            };

            if spot_price_after_swap < min_swap_at_spot_price
                || spot_price_after_swap > max_swap_at_spot_price
            {
                return Err(anyhow!("Swap in not allowed"));
            }
        };

        Ok(SwapResult {
            new_swap_source_amount,
            new_swap_destination_amount,
            source_amount_swapped: source_amount_to_be_swapped,
            destination_amount_swapped,
            dynamic_fee: trade_fee_charged,
            protocol_fee,
            fund_fee,
            dynamic_fee_rate: trade_fee_rate as u64,
        })
    }
}

pub mod gamma_deseralize_pool_state {
    use super::*;
    use crate::states::PartnerInfo;
    use anchor_lang::{prelude::AnchorDeserialize, Discriminator};
    use std::ops::BitAnd;

    #[derive(Default, Debug, AnchorDeserialize, Clone)]
    pub struct PoolState {
        pub amm_config: Pubkey,
        pub pool_creator: Pubkey,
        pub token_0_vault: Pubkey,
        pub token_1_vault: Pubkey,
        pub oracle_price_token_0_by_token_1: u128,     // 16
        pub oracle_price_updated_at: u64,              // 8
        pub acceptable_price_difference: u32,          // 4
        pub max_amount_swappable_at_oracle_price: u32, // 4
        pub token_0_mint: Pubkey,
        pub token_1_mint: Pubkey,
        pub token_0_program: Pubkey,
        pub token_1_program: Pubkey,
        pub observation_key: Pubkey,
        pub auth_bump: u8,
        pub status: u8,
        pub _padding2: u8,
        pub mint_0_decimals: u8,
        pub mint_1_decimals: u8,
        pub lp_supply: u64,
        pub protocol_fees_token_0: u64,
        pub protocol_fees_token_1: u64,
        pub fund_fees_token_0: u64,
        pub fund_fees_token_1: u64,
        pub open_time: u64,
        pub recent_epoch: u64,
        pub cumulative_trade_fees_token_0: u128,
        pub cumulative_trade_fees_token_1: u128,
        pub cumulative_volume_token_0: u128,
        pub cumulative_volume_token_1: u128,
        pub latest_dynamic_fee_rate: u64,
        pub max_trade_fee_rate: u64,
        pub volatility_factor: u64,
        pub token_0_vault_amount: u64,
        pub token_1_vault_amount: u64,
        pub max_shared_token0: u64,
        pub max_shared_token1: u64,
        pub min_trade_rate_at_oracle_price: u32, // 4
        pub drift_factor: u32,                   // 4
        pub max_oracle_price_update_time_diff: u32,
        pub max_drift_factor: u32,
        pub oracle_price_delay_fee_rate_per_second: u32, // 4
        pub max_oracle_price_delay_fee: u32,             // 4
        pub _padding3: [u8; 8],                          // 8
        pub token_0_amount_in_kamino: u64,
        pub token_1_amount_in_kamino: u64,
        pub withdrawn_kamino_profit_token_0: u64,
        pub withdrawn_kamino_profit_token_1: u64,
        pub partner_share_rate: u64,
        pub partner_protocol_fees_token_0: u64,
        pub partner_protocol_fees_token_1: u64,
        pub min_swap_at_spot_price: u64,
        pub max_swap_at_spot_price: u64,
        pub padding: [u64; 3],
    }

    impl PoolState {
        const ACCOUNT_DISCRIMINATOR: &[u8] = crate::states::PoolState::DISCRIMINATOR;

        pub fn deserialize_account(data: &[u8]) -> Result<Self> {
            if data[0..8] != *Self::ACCOUNT_DISCRIMINATOR {
                println!("data: {:?}", data);
                println!(
                    "Self::ACCOUNT_DISCRIMINATOR: {:?}",
                    Self::ACCOUNT_DISCRIMINATOR
                );
                return Err(anyhow!("Invalid discriminator"));
            }

            let mut reader = &data[Self::ACCOUNT_DISCRIMINATOR.len()..];
            Ok(gamma_deseralize_pool_state::PoolState::deserialize(
                &mut reader,
            )?)
        }

        // Get status by bit, if it is 'normal'/enabled return true
        pub fn get_status_by_bit(&self, bit: PoolStatusBitIndex) -> bool {
            let status = u8::from(1) << (bit as u8);
            self.status.bitand(status) == 0
        }

        pub fn vault_amount_without_fee(&self) -> Result<(u64, u64)> {
            Ok((self.token_0_vault_amount, self.token_1_vault_amount))
        }
    }

    impl Into<crate::states::PoolState> for &PoolState {
        fn into(self) -> crate::states::PoolState {
            crate::states::PoolState {
                amm_config: self.amm_config,
                pool_creator: self.pool_creator,
                token_0_vault: self.token_0_vault,
                token_1_vault: self.token_1_vault,
                _padding1: [0; 32],
                token_0_mint: self.token_0_mint,
                token_1_mint: self.token_1_mint,
                token_0_program: self.token_0_program,
                token_1_program: self.token_1_program,
                observation_key: self.observation_key,
                auth_bump: self.auth_bump,
                status: self.status,
                _padding2: 0,
                mint_0_decimals: self.mint_0_decimals,
                mint_1_decimals: self.mint_1_decimals,
                lp_supply: self.lp_supply,
                protocol_fees_token_0: self.protocol_fees_token_0,
                protocol_fees_token_1: self.protocol_fees_token_1,
                fund_fees_token_0: self.fund_fees_token_0,
                fund_fees_token_1: self.fund_fees_token_1,
                open_time: self.open_time,
                recent_epoch: self.recent_epoch,
                cumulative_trade_fees_token_0: self.cumulative_trade_fees_token_0,
                cumulative_trade_fees_token_1: self.cumulative_trade_fees_token_1,
                cumulative_volume_token_0: self.cumulative_volume_token_0,
                cumulative_volume_token_1: self.cumulative_volume_token_1,
                latest_dynamic_fee_rate: self.latest_dynamic_fee_rate,
                max_trade_fee_rate: self.max_trade_fee_rate,
                volatility_factor: self.volatility_factor,
                token_0_vault_amount: self.token_0_vault_amount,
                token_1_vault_amount: self.token_1_vault_amount,
                max_shared_token0: self.max_shared_token0,
                max_shared_token1: self.max_shared_token1,
                partners: [PartnerInfo::default(); 1],
                token_0_amount_in_kamino: self.token_0_amount_in_kamino,
                token_1_amount_in_kamino: self.token_1_amount_in_kamino,
                withdrawn_kamino_profit_token_0: self.withdrawn_kamino_profit_token_0,
                withdrawn_kamino_profit_token_1: self.withdrawn_kamino_profit_token_1,
                padding: [0; 8],
            }
        }
    }
}
