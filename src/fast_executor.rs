use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    message::Message,
    signature::Keypair,
    signer::Signer,
    transaction::Transaction,
    compute_budget::ComputeBudgetInstruction,
    pubkey::Pubkey,
};
use std::sync::Arc;
use crate::types::{TradeDetails, ExecutedTrade};
use crate::pool_cache::PoolCache;
use tracing::{info, error};
use chrono::Utc;

pub struct FastExecutor {
    client: RpcClient,
    wallet: Arc<Keypair>,
    pool_cache: Arc<PoolCache>,
}

impl FastExecutor {
    pub fn new(rpc_url: &str, wallet: Arc<Keypair>, pool_cache: Arc<PoolCache>) -> Result<Self> {
        let client = RpcClient::new_with_commitment(
            rpc_url.to_string(),
            CommitmentConfig::processed(), // 使用processed以获得更快响应
        );
        Ok(FastExecutor {
            client,
            wallet,
            pool_cache,
        })
    }
    /// 极速执行跟单交易
    pub async fn execute_fast_trade(&self, trade: &TradeDetails) -> Result<ExecutedTrade> {
        let start_time = std::time::Instant::now();
        // 从缓存获取池子参数（零延迟）
        let swap_accounts = self.pool_cache.build_swap_accounts(
            &self.client,
            &trade.pool_address,
            &self.wallet.pubkey(),
            &trade.token_in.mint,
            &trade.token_out.mint,
        ).map_err(|_| anyhow::anyhow!("池子参数未缓存"))?;
        info!("缓存命中，耗时: {:?}", start_time.elapsed());
        // 构建交易指令
        let mut instructions = vec![];
        // 设置更高的计算单元和优先费用（加速交易确认）
        instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(400_000));
        instructions.push(ComputeBudgetInstruction::set_compute_unit_price(50_000)); // 更高的优先费
        // 构建swap指令
        let swap_ix = self.build_swap_instruction(trade, &swap_accounts)?;
        instructions.push(swap_ix);
        // 获取最新区块哈希（这是唯一的网络请求）
        let recent_blockhash = self.client.get_latest_blockhash()?;
        // 构建并签名交易
        let message = Message::new(&instructions, Some(&self.wallet.pubkey()));
        let mut transaction = Transaction::new_unsigned(message);
        transaction.sign(&[self.wallet.as_ref()], recent_blockhash);
        // 发送交易（使用skip_preflight加速）
        let config = solana_client::rpc_config::RpcSendTransactionConfig {
            skip_preflight: true,
            preflight_commitment: Some(solana_sdk::commitment_config::CommitmentLevel::Processed),
            ..Default::default()
        };
        match self.client.send_transaction_with_config(&transaction, config) {
            Ok(signature) => {
                let total_time = start_time.elapsed();
                info!("极速跟单成功！签名: {}, 总耗时: {:?}", signature, total_time);
                Ok(ExecutedTrade {
                    original_signature: trade.signature.clone(),
                    copy_signature: signature.to_string(),
                    trade_direction: trade.trade_direction.clone(),
                    amount_in: trade.amount_in,
                    amount_out: trade.amount_out,
                    price: trade.price,
                    gas_fee: trade.gas_fee,
                    timestamp: Utc::now().timestamp(),
                    success: true,
                    error_message: None,
                })
            }
            Err(e) => {
                error!("极速跟单失败: {}", e);
                Ok(ExecutedTrade {
                    original_signature: trade.signature.clone(),
                    copy_signature: "".to_string(),
                    trade_direction: trade.trade_direction.clone(),
                    amount_in: trade.amount_in,
                    amount_out: trade.amount_out,
                    price: trade.price,
                    gas_fee: trade.gas_fee,
                    timestamp: Utc::now().timestamp(),
                    success: false,
                    error_message: Some(e.to_string()),
                })
            }
        }
    }
    fn build_swap_instruction(&self, trade: &TradeDetails, accounts: &crate::trade_executor::RaydiumCpmmSwapAccounts) -> Result<Instruction> {
        // 使用预编译的指令数据模板
        let mut data = vec![0x8f, 0xbe, 0x5a, 0xda, 0xc4, 0x1e, 0x33, 0xde]; // discriminator
        data.extend_from_slice(&trade.amount_in.to_le_bytes());
        data.extend_from_slice(&1u64.to_le_bytes()); // min_amount_out = 1
        Ok(Instruction {
            program_id: trade.program_id,
            accounts: vec![
                solana_sdk::instruction::AccountMeta::new(accounts.payer, true),
                solana_sdk::instruction::AccountMeta::new_readonly(accounts.authority, false),
                solana_sdk::instruction::AccountMeta::new_readonly(accounts.amm_config, false),
                solana_sdk::instruction::AccountMeta::new(accounts.pool_state, false),
                solana_sdk::instruction::AccountMeta::new(accounts.user_input_ata, false),
                solana_sdk::instruction::AccountMeta::new(accounts.user_output_ata, false),
                solana_sdk::instruction::AccountMeta::new(accounts.input_vault, false),
                solana_sdk::instruction::AccountMeta::new(accounts.output_vault, false),
                solana_sdk::instruction::AccountMeta::new_readonly(accounts.input_token_program, false),
                solana_sdk::instruction::AccountMeta::new_readonly(accounts.output_token_program, false),
                solana_sdk::instruction::AccountMeta::new_readonly(accounts.input_mint, false),
                solana_sdk::instruction::AccountMeta::new_readonly(accounts.output_mint, false),
                solana_sdk::instruction::AccountMeta::new(accounts.observation_state, false),
            ],
            data,
        })
    }
} 