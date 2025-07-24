use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::collections::HashMap;
use crate::types::TradeExecutionConfig;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub rpc_url: String,
    pub target_wallets: Vec<String>,
    pub copy_wallet_private_key: String,
    pub trading_settings: TradingSettings,
    pub execution_config: ExecutionConfig,
    pub copy_trading: CopyTradingConfig,
    pub grpc_endpoint: String,
    pub broadcast_channels: BroadcastChannels,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TradingSettings {
    pub max_position_size: f64,
    pub slippage_tolerance: f64,
    pub gas_price_multiplier: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExecutionConfig {
    pub enabled: bool,
    pub min_trade_amount: f64,
    pub max_trade_amount: f64,
    pub max_position_size: f64,
    pub slippage_tolerance: f64,
    pub gas_price_multiplier: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CopyTradingConfig {
    pub enabled: bool,
    pub min_sol_balance: f64,
    pub max_trade_amount_sol: f64,
    pub skip_large_trades: bool,
    pub fixed_trade_amount_sol: f64,
    pub use_fixed_amount: bool,
    pub slippage_bps: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BroadcastChannels {
    pub enabled: bool,
    pub rpc: RpcChannel,
    pub bloxroute: BloxrouteChannel,
    pub zero_slot: ZeroSlotChannel,
    pub jupiter: JupiterChannel,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcChannel {
    pub enabled: bool,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BloxrouteChannel {
    pub enabled: bool,
    pub name: String,
    pub api_key: String,
    pub region: String,
    pub timeout_ms: u64,
    pub use_official_api: bool,
    pub regions: HashMap<String, BloxrouteRegion>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BloxrouteRegion {
    pub name: String,
    pub endpoint: String,
    pub region_type: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ZeroSlotChannel {
    pub enabled: bool,
    pub name: String,
    pub api_key: String,
    pub endpoint: String,
    pub staked_endpoint: String,
    pub timeout_ms: u64,
    pub use_staked_conn: bool,
    pub durable_nonce: DurableNonceConfig,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DurableNonceConfig {
    pub enabled: bool,
    pub auto_advance: bool,
    pub max_retries: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JupiterChannel {
    pub enabled: bool,
    pub name: String,
    pub endpoint: String,
    pub timeout_ms: u64,
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_str = fs::read_to_string("config.json")?;
        let config: Config = serde_json::from_str(&config_str)?;
        Ok(config)
    }
    
    pub fn get_execution_config(&self) -> TradeExecutionConfig {
        TradeExecutionConfig {
            copy_wallet_private_key: self.copy_wallet_private_key.clone(),
            max_position_size: self.execution_config.max_position_size,
            slippage_tolerance: self.execution_config.slippage_tolerance,
            gas_price_multiplier: self.execution_config.gas_price_multiplier,
            min_trade_amount: self.execution_config.min_trade_amount,
            max_trade_amount: self.execution_config.max_trade_amount,
            enabled: self.execution_config.enabled,
        }
    }
}