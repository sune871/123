use borsh::BorshDeserialize;
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

use crate::streaming::{
    event_parser::{
        common::EventMetadata, protocols::raydium_amm_v4::RaydiumAmmV4AmmInfoAccountEvent,
        UnifiedEvent,
    },
    grpc::AccountPretty,
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, BorshDeserialize)]
pub struct Fees {
    pub min_separate_numerator: u64,
    pub min_separate_denominator: u64,
    pub trade_fee_numerator: u64,
    pub trade_fee_denominator: u64,
    pub pnl_numerator: u64,
    pub pnl_denominator: u64,
    pub swap_fee_numerator: u64,
    pub swap_fee_denominator: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, BorshDeserialize)]
pub struct OutPutData {
    pub need_take_pnl_coin: u64,
    pub need_take_pnl_pc: u64,
    pub total_pnl_pc: u64,
    pub total_pnl_coin: u64,
    pub pool_open_time: u64,
    pub punish_pc_amount: u64,
    pub punish_coin_amount: u64,
    pub orderbook_to_init_time: u64,
    pub swap_coin_in_amount: u128,
    pub swap_pc_out_amount: u128,
    pub swap_take_pc_fee: u64,
    pub swap_pc_in_amount: u128,
    pub swap_coin_out_amount: u128,
    pub swap_take_coin_fee: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, BorshDeserialize)]
pub struct AmmInfo {
    pub status: u64,
    pub nonce: u64,
    pub order_num: u64,
    pub depth: u64,
    pub coin_decimals: u64,
    pub pc_decimals: u64,
    pub state: u64,
    pub reset_flag: u64,
    pub min_size: u64,
    pub vol_max_cut_ratio: u64,
    pub amount_wave: u64,
    pub coin_lot_size: u64,
    pub pc_lot_size: u64,
    pub min_price_multiplier: u64,
    pub max_price_multiplier: u64,
    pub sys_decimal_value: u64,
    pub fees: Fees,
    pub out_put: OutPutData,
    pub token_coin: Pubkey,
    pub token_pc: Pubkey,
    pub coin_mint: Pubkey,
    pub pc_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub open_orders: Pubkey,
    pub market: Pubkey,
    pub serum_dex: Pubkey,
    pub target_orders: Pubkey,
    pub withdraw_queue: Pubkey,
    pub token_temp_lp: Pubkey,
    pub amm_owner: Pubkey,
    pub lp_amount: u64,
    pub client_order_id: u64,
    pub padding: [u64; 2],
}

pub const AMM_INFO_SIZE: usize = 752;

pub fn amm_info_decode(data: &[u8]) -> Option<AmmInfo> {
    if data.len() < AMM_INFO_SIZE {
        return None;
    }
    borsh::from_slice::<AmmInfo>(&data[..AMM_INFO_SIZE]).ok()
}

pub fn amm_info_parser(
    account: &AccountPretty,
    metadata: EventMetadata,
) -> Option<Box<dyn UnifiedEvent>> {
    if account.data.len() < AMM_INFO_SIZE {
        return None;
    }
    if let Some(amm_info) = amm_info_decode(&account.data[..AMM_INFO_SIZE]) {
        Some(Box::new(RaydiumAmmV4AmmInfoAccountEvent {
            metadata,
            pubkey: account.pubkey,
            executable: account.executable,
            lamports: account.lamports,
            owner: account.owner,
            rent_epoch: account.rent_epoch,
            amm_info: amm_info,
        }))
    } else {
        None
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, BorshDeserialize)]
pub struct MarketState {
    pub padding: [u8; 5],
    pub account_flags: u64,
    pub own_address: Pubkey,
    pub vault_signer_nonce: u64,
    pub coin_mint: Pubkey,
    pub pc_mint: Pubkey,
    pub serum_coin_vault_account: Pubkey,
    pub coin_deposits_total: u64,
    pub coin_fees_accrued: u64,
    pub serum_pc_vault_account: Pubkey,
    pub pc_deposits_total: u64,
    pub pc_fees_accrued: u64,
    pub pc_dust_threshold: u64,
    pub request_queue: Pubkey,
    pub serum_event_queue: Pubkey,
    pub serum_bids: Pubkey,
    pub serum_asks: Pubkey,
    pub coin_lot_size: u64,
    pub pc_lot_size: u64,
    pub fee_rate_bps: u64,
    pub referrer_rebate_accrued: u64,
    pub padding2: [u8; 7],
}

pub const MARKET_STATE_SIZE: usize = 388;

pub fn market_state_decode(data: &[u8]) -> Option<MarketState> {
    if data.len() < MARKET_STATE_SIZE {
        return None;
    }
    borsh::from_slice::<MarketState>(&data[..MARKET_STATE_SIZE]).ok()
}
