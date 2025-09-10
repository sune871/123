use borsh::BorshDeserialize;
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

use crate::impl_unified_event;
use crate::streaming::event_parser::common::EventMetadata;
use crate::streaming::event_parser::protocols::pumpswap::types::{GlobalConfig, Pool};

/// 买入事件
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, BorshDeserialize)]
pub struct PumpSwapBuyEvent {
    #[borsh(skip)]
    pub metadata: EventMetadata,
    pub timestamp: i64,
    pub base_amount_out: u64,
    pub max_quote_amount_in: u64,
    pub user_base_token_reserves: u64,
    pub user_quote_token_reserves: u64,
    pub pool_base_token_reserves: u64,
    pub pool_quote_token_reserves: u64,
    pub quote_amount_in: u64,
    pub lp_fee_basis_points: u64,
    pub lp_fee: u64,
    pub protocol_fee_basis_points: u64,
    pub protocol_fee: u64,
    pub quote_amount_in_with_lp_fee: u64,
    pub user_quote_amount_in: u64,
    pub pool: Pubkey,
    pub user: Pubkey,
    pub user_base_token_account: Pubkey,
    pub user_quote_token_account: Pubkey,
    pub protocol_fee_recipient: Pubkey,
    pub protocol_fee_recipient_token_account: Pubkey,
    pub coin_creator: Pubkey,
    pub coin_creator_fee_basis_points: u64,
    pub coin_creator_fee: u64,
    pub track_volume: bool,
    pub total_unclaimed_tokens: u64,
    pub total_claimed_tokens: u64,
    pub current_sol_volume: u64,
    pub last_update_timestamp: i64,
    #[borsh(skip)]
    pub base_mint: Pubkey,
    #[borsh(skip)]
    pub quote_mint: Pubkey,
    #[borsh(skip)]
    pub pool_base_token_account: Pubkey,
    #[borsh(skip)]
    pub pool_quote_token_account: Pubkey,
    #[borsh(skip)]
    pub coin_creator_vault_ata: Pubkey,
    #[borsh(skip)]
    pub coin_creator_vault_authority: Pubkey,
    #[borsh(skip)]
    pub base_token_program: Pubkey,
    #[borsh(skip)]
    pub quote_token_program: Pubkey,
}

pub const PUMP_SWAP_BUY_EVENT_LOG_SIZE: usize = 385;

pub fn pump_swap_buy_event_log_decode(data: &[u8]) -> Option<PumpSwapBuyEvent> {
    if data.len() < PUMP_SWAP_BUY_EVENT_LOG_SIZE {
        return None;
    }
    borsh::from_slice::<PumpSwapBuyEvent>(&data[..PUMP_SWAP_BUY_EVENT_LOG_SIZE]).ok()
}

// 使用宏生成UnifiedEvent实现，指定需要合并的字段
impl_unified_event!(
    PumpSwapBuyEvent,
    timestamp,
    base_amount_out,
    max_quote_amount_in,
    user_base_token_reserves,
    user_quote_token_reserves,
    pool_base_token_reserves,
    pool_quote_token_reserves,
    quote_amount_in,
    lp_fee_basis_points,
    lp_fee,
    protocol_fee_basis_points,
    protocol_fee,
    quote_amount_in_with_lp_fee,
    user_quote_amount_in,
    pool,
    user,
    user_base_token_account,
    user_quote_token_account,
    protocol_fee_recipient,
    protocol_fee_recipient_token_account,
    coin_creator,
    coin_creator_fee_basis_points,
    coin_creator_fee
);

/// 卖出事件
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, BorshDeserialize)]
pub struct PumpSwapSellEvent {
    #[borsh(skip)]
    pub metadata: EventMetadata,
    pub timestamp: i64,
    pub base_amount_in: u64,
    pub min_quote_amount_out: u64,
    pub user_base_token_reserves: u64,
    pub user_quote_token_reserves: u64,
    pub pool_base_token_reserves: u64,
    pub pool_quote_token_reserves: u64,
    pub quote_amount_out: u64,
    pub lp_fee_basis_points: u64,
    pub lp_fee: u64,
    pub protocol_fee_basis_points: u64,
    pub protocol_fee: u64,
    pub quote_amount_out_without_lp_fee: u64,
    pub user_quote_amount_out: u64,
    pub pool: Pubkey,
    pub user: Pubkey,
    pub user_base_token_account: Pubkey,
    pub user_quote_token_account: Pubkey,
    pub protocol_fee_recipient: Pubkey,
    pub protocol_fee_recipient_token_account: Pubkey,
    pub coin_creator: Pubkey,
    pub coin_creator_fee_basis_points: u64,
    pub coin_creator_fee: u64,
    #[borsh(skip)]
    pub base_mint: Pubkey,
    #[borsh(skip)]
    pub quote_mint: Pubkey,
    #[borsh(skip)]
    pub pool_base_token_account: Pubkey,
    #[borsh(skip)]
    pub pool_quote_token_account: Pubkey,
    #[borsh(skip)]
    pub coin_creator_vault_ata: Pubkey,
    #[borsh(skip)]
    pub coin_creator_vault_authority: Pubkey,
    #[borsh(skip)]
    pub base_token_program: Pubkey,
    #[borsh(skip)]
    pub quote_token_program: Pubkey,
}

pub const PUMP_SWAP_SELL_EVENT_LOG_SIZE: usize = 352;

pub fn pump_swap_sell_event_log_decode(data: &[u8]) -> Option<PumpSwapSellEvent> {
    if data.len() < PUMP_SWAP_SELL_EVENT_LOG_SIZE {
        return None;
    }
    borsh::from_slice::<PumpSwapSellEvent>(&data[..PUMP_SWAP_SELL_EVENT_LOG_SIZE]).ok()
}

// 使用宏生成UnifiedEvent实现，指定需要合并的字段
impl_unified_event!(
    PumpSwapSellEvent,
    timestamp,
    base_amount_in,
    min_quote_amount_out,
    user_base_token_reserves,
    user_quote_token_reserves,
    pool_base_token_reserves,
    pool_quote_token_reserves,
    quote_amount_out,
    lp_fee_basis_points,
    lp_fee,
    protocol_fee_basis_points,
    protocol_fee,
    quote_amount_out_without_lp_fee,
    user_quote_amount_out,
    pool,
    user,
    user_base_token_account,
    user_quote_token_account,
    protocol_fee_recipient,
    protocol_fee_recipient_token_account,
    coin_creator,
    coin_creator_fee_basis_points,
    coin_creator_fee
);

/// 创建池子事件
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, BorshDeserialize)]
pub struct PumpSwapCreatePoolEvent {
    #[borsh(skip)]
    pub metadata: EventMetadata,
    pub timestamp: i64,
    pub index: u16,
    pub creator: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_mint_decimals: u8,
    pub quote_mint_decimals: u8,
    pub base_amount_in: u64,
    pub quote_amount_in: u64,
    pub pool_base_amount: u64,
    pub pool_quote_amount: u64,
    pub minimum_liquidity: u64,
    pub initial_liquidity: u64,
    pub lp_token_amount_out: u64,
    pub pool_bump: u8,
    pub pool: Pubkey,
    pub lp_mint: Pubkey,
    pub user_base_token_account: Pubkey,
    pub user_quote_token_account: Pubkey,
    pub coin_creator: Pubkey,
    #[borsh(skip)]
    pub user_pool_token_account: Pubkey,
    #[borsh(skip)]
    pub pool_base_token_account: Pubkey,
    #[borsh(skip)]
    pub pool_quote_token_account: Pubkey,
}

pub const PUMP_SWAP_CREATE_POOL_EVENT_LOG_SIZE: usize = 325;

pub fn pump_swap_create_pool_event_log_decode(data: &[u8]) -> Option<PumpSwapCreatePoolEvent> {
    if data.len() < PUMP_SWAP_CREATE_POOL_EVENT_LOG_SIZE {
        return None;
    }
    borsh::from_slice::<PumpSwapCreatePoolEvent>(&data[..PUMP_SWAP_CREATE_POOL_EVENT_LOG_SIZE]).ok()
}

impl_unified_event!(
    PumpSwapCreatePoolEvent,
    timestamp,
    index,
    creator,
    base_mint,
    quote_mint,
    base_mint_decimals,
    quote_mint_decimals,
    base_amount_in,
    quote_amount_in,
    pool_base_amount,
    pool_quote_amount,
    minimum_liquidity,
    initial_liquidity,
    lp_token_amount_out,
    pool_bump,
    pool,
    lp_mint,
    user_base_token_account,
    user_quote_token_account,
    coin_creator
);

/// 存款事件
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, BorshDeserialize)]
pub struct PumpSwapDepositEvent {
    #[borsh(skip)]
    pub metadata: EventMetadata,
    pub timestamp: i64,
    pub lp_token_amount_out: u64,
    pub max_base_amount_in: u64,
    pub max_quote_amount_in: u64,
    pub user_base_token_reserves: u64,
    pub user_quote_token_reserves: u64,
    pub pool_base_token_reserves: u64,
    pub pool_quote_token_reserves: u64,
    pub base_amount_in: u64,
    pub quote_amount_in: u64,
    pub lp_mint_supply: u64,
    pub pool: Pubkey,
    pub user: Pubkey,
    pub user_base_token_account: Pubkey,
    pub user_quote_token_account: Pubkey,
    pub user_pool_token_account: Pubkey,
    #[borsh(skip)]
    pub base_mint: Pubkey,
    #[borsh(skip)]
    pub quote_mint: Pubkey,
    #[borsh(skip)]
    pub pool_base_token_account: Pubkey,
    #[borsh(skip)]
    pub pool_quote_token_account: Pubkey,
}

pub const PUMP_SWAP_DEPOSIT_EVENT_LOG_SIZE: usize = 248;

pub fn pump_swap_deposit_event_log_decode(data: &[u8]) -> Option<PumpSwapDepositEvent> {
    if data.len() < PUMP_SWAP_DEPOSIT_EVENT_LOG_SIZE {
        return None;
    }
    borsh::from_slice::<PumpSwapDepositEvent>(&data[..PUMP_SWAP_DEPOSIT_EVENT_LOG_SIZE]).ok()
}

impl_unified_event!(
    PumpSwapDepositEvent,
    timestamp,
    lp_token_amount_out,
    max_base_amount_in,
    max_quote_amount_in,
    user_base_token_reserves,
    user_quote_token_reserves,
    pool_base_token_reserves,
    pool_quote_token_reserves,
    base_amount_in,
    quote_amount_in,
    lp_mint_supply,
    pool,
    user,
    user_base_token_account,
    user_quote_token_account,
    user_pool_token_account
);

/// 提款事件
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, BorshDeserialize)]
pub struct PumpSwapWithdrawEvent {
    #[borsh(skip)]
    pub metadata: EventMetadata,
    pub timestamp: i64,
    pub lp_token_amount_in: u64,
    pub min_base_amount_out: u64,
    pub min_quote_amount_out: u64,
    pub user_base_token_reserves: u64,
    pub user_quote_token_reserves: u64,
    pub pool_base_token_reserves: u64,
    pub pool_quote_token_reserves: u64,
    pub base_amount_out: u64,
    pub quote_amount_out: u64,
    pub lp_mint_supply: u64,
    pub pool: Pubkey,
    pub user: Pubkey,
    pub user_base_token_account: Pubkey,
    pub user_quote_token_account: Pubkey,
    pub user_pool_token_account: Pubkey,
    #[borsh(skip)]
    pub base_mint: Pubkey,
    #[borsh(skip)]
    pub quote_mint: Pubkey,
    #[borsh(skip)]
    pub pool_base_token_account: Pubkey,
    #[borsh(skip)]
    pub pool_quote_token_account: Pubkey,
}

pub const PUMP_SWAP_WITHDRAW_EVENT_LOG_SIZE: usize = 248;

pub fn pump_swap_withdraw_event_log_decode(data: &[u8]) -> Option<PumpSwapWithdrawEvent> {
    if data.len() < PUMP_SWAP_WITHDRAW_EVENT_LOG_SIZE {
        return None;
    }
    borsh::from_slice::<PumpSwapWithdrawEvent>(&data[..PUMP_SWAP_WITHDRAW_EVENT_LOG_SIZE]).ok()
}

impl_unified_event!(
    PumpSwapWithdrawEvent,
    timestamp,
    lp_token_amount_in,
    min_base_amount_out,
    min_quote_amount_out,
    user_base_token_reserves,
    user_quote_token_reserves,
    pool_base_token_reserves,
    pool_quote_token_reserves,
    base_amount_out,
    quote_amount_out,
    lp_mint_supply,
    pool,
    user,
    user_base_token_account,
    user_quote_token_account,
    user_pool_token_account
);

/// 全局配置
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, BorshDeserialize)]
pub struct PumpSwapGlobalConfigAccountEvent {
    #[borsh(skip)]
    pub metadata: EventMetadata,
    pub pubkey: Pubkey,
    pub executable: bool,
    pub lamports: u64,
    pub owner: Pubkey,
    pub rent_epoch: u64,
    pub global_config: GlobalConfig,
}
impl_unified_event!(PumpSwapGlobalConfigAccountEvent,);

/// 池
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, BorshDeserialize)]
pub struct PumpSwapPoolAccountEvent {
    #[borsh(skip)]
    pub metadata: EventMetadata,
    pub pubkey: Pubkey,
    pub executable: bool,
    pub lamports: u64,
    pub owner: Pubkey,
    pub rent_epoch: u64,
    pub pool: Pool,
}
impl_unified_event!(PumpSwapPoolAccountEvent,);

/// 事件鉴别器常量
pub mod discriminators {
    // 事件鉴别器
    // pub const BUY_EVENT: &str = "0xe445a52e51cb9a1d67f4521f2cf57777";
    pub const BUY_EVENT: &[u8] =
        &[228, 69, 165, 46, 81, 203, 154, 29, 103, 244, 82, 31, 44, 245, 119, 119];
    // pub const SELL_EVENT: &str = "0xe445a52e51cb9a1d3e2f370aa503dc2a";
    pub const SELL_EVENT: &[u8] =
        &[228, 69, 165, 46, 81, 203, 154, 29, 62, 47, 55, 10, 165, 3, 220, 42];
    // pub const CREATE_POOL_EVENT: &str = "0xe445a52e51cb9a1db1310cd2a076a774";
    pub const CREATE_POOL_EVENT: &[u8] =
        &[228, 69, 165, 46, 81, 203, 154, 29, 177, 49, 12, 210, 160, 118, 167, 116];
    // pub const DEPOSIT_EVENT: &str = "0xe445a52e51cb9a1d78f83d531f8e6b90";
    pub const DEPOSIT_EVENT: &[u8] =
        &[228, 69, 165, 46, 81, 203, 154, 29, 120, 248, 61, 83, 31, 142, 107, 144];
    // pub const WITHDRAW_EVENT: &str = "0xe445a52e51cb9a1d1609851aa02c47c0";
    pub const WITHDRAW_EVENT: &[u8] =
        &[228, 69, 165, 46, 81, 203, 154, 29, 22, 9, 133, 26, 160, 44, 71, 192];

    // 指令鉴别器
    pub const BUY_IX: &[u8] = &[102, 6, 61, 18, 1, 218, 235, 234];
    pub const SELL_IX: &[u8] = &[51, 230, 133, 164, 1, 127, 131, 173];
    pub const CREATE_POOL_IX: &[u8] = &[233, 146, 209, 142, 207, 104, 64, 188];
    pub const DEPOSIT_IX: &[u8] = &[242, 35, 198, 137, 82, 225, 242, 182];
    pub const WITHDRAW_IX: &[u8] = &[183, 18, 70, 156, 148, 109, 161, 34];

    // 账户鉴别器
    pub const GLOBAL_CONFIG_ACCOUNT: &[u8] = &[149, 8, 156, 202, 160, 252, 176, 217];
    pub const POOL_ACCOUNT: &[u8] = &[241, 154, 109, 4, 17, 177, 109, 188];
}
