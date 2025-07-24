use std::sync::Arc;
use anyhow::Result;
use futures::{StreamExt, SinkExt};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::bs58;
use solana_sdk::signature::Signer;
use std::collections::HashMap;
use tracing::{info, error, warn, debug};
use yellowstone_grpc_proto::geyser::{
    CommitmentLevel, SubscribeRequest, SubscribeRequestFilterAccounts,
    SubscribeRequestFilterTransactions, SubscribeUpdate, SubscribeUpdateTransaction,
};
use yellowstone_grpc_proto::prelude::{Transaction, Message, TransactionStatusMeta};

// 添加新的导入
use crate::parser::TransactionParser;
use crate::types::TradeDetails;
use crate::trade_executor::{TradeExecutor, RaydiumCpmmSwapAccounts};
use serde_json;
use std::str::FromStr;
use spl_associated_token_account::get_associated_token_address;
use solana_client::rpc_client::RpcClient;
use solana_sdk::instruction::CompiledInstruction;
use tonic::transport::ClientTlsConfig;
use bytes::Bytes;
use std::collections::VecDeque;
use tokio::sync::Mutex;
use lazy_static::lazy_static;
use crate::trade_log;

// 1. 新增QuickTradeParser结构体和quick_parse方法

pub struct QuickTradeParser {
    pub wsol_mint: Pubkey,
}

impl QuickTradeParser {
    pub fn quick_parse(&self, instruction: &CompiledInstruction, accounts: &[String], user_wallet: &Pubkey) -> Option<RaydiumCpmmSwapAccounts> {
        // 只处理Raydium CPMM swap指令
        if instruction.data.len() < 16 { return None; }
        let discriminator = &instruction.data[0..8];
        // 你需要根据实际Raydium CPMM discriminator替换下面的常量
        const RAYDIUM_CPMM_SWAP_DISCRIMINATOR: [u8; 8] = [0x8f, 0xbe, 0x5a, 0xda, 0xc4, 0x1e, 0x33, 0xde];
        if discriminator != RAYDIUM_CPMM_SWAP_DISCRIMINATOR { return None; }
        // 直接用accounts数组组装cpmm_accounts
        if accounts.len() < 13 { return None; }
        let input_mint = Pubkey::from_str(&accounts[10]).ok()?;
        let output_mint = Pubkey::from_str(&accounts[11]).ok()?;
        let user_input_ata = get_associated_token_address(user_wallet, &input_mint);
        let user_output_ata = get_associated_token_address(user_wallet, &output_mint);
        debug!("quick_parse: user_wallet={}, input_mint={}, output_mint={}, user_input_ata={}, user_output_ata={}", user_wallet, input_mint, output_mint, user_input_ata, user_output_ata);
        Some(RaydiumCpmmSwapAccounts {
            payer: *user_wallet,
            authority: Pubkey::from_str(&accounts[1]).ok()?,
            amm_config: Pubkey::from_str(&accounts[2]).ok()?,
            pool_state: Pubkey::from_str(&accounts[3]).ok()?,
            user_input_ata,
            user_output_ata,
            input_vault: Pubkey::from_str(&accounts[6]).ok()?,
            output_vault: Pubkey::from_str(&accounts[7]).ok()?,
            input_token_program: Pubkey::from_str(&accounts[8]).ok()?,
            output_token_program: Pubkey::from_str(&accounts[9]).ok()?,
            input_mint,
            output_mint,
            observation_state: Pubkey::from_str(&accounts[12]).ok()?,
            token_0_mint: Pubkey::default(),
            token_1_mint: Pubkey::default(),
        })
    }
}

// RPC连接池结构体
type SharedRpcClient = Arc<RpcClient>;

pub struct RpcConnectionPool {
    connections: Vec<Arc<RpcClient>>,
    current_index: Arc<tokio::sync::RwLock<usize>>,
}

impl RpcConnectionPool {
    pub fn new(urls: Vec<String>, pool_size: usize) -> Self {
        let mut connections = Vec::new();
        for url in urls.iter().cycle().take(pool_size) {
            connections.push(Arc::new(RpcClient::new(url)));
        }
        Self {
            connections,
            current_index: Arc::new(tokio::sync::RwLock::new(0)),
        }
    }
    pub async fn get_client(&self) -> Arc<RpcClient> {
        let mut index = self.current_index.write().await;
        let client = self.connections[*index].clone();
        *index = (*index + 1) % self.connections.len();
        client
    }
}

// Common DEX program IDs
const RAYDIUM_V4: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
const JUPITER_V6: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
const ORCA_WHIRLPOOL: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

// 重构GrpcMonitor结构体
pub struct GrpcMonitor {
    endpoint: String,
    auth_token: Option<String>,
    target_wallet: Pubkey,
    executor: Option<Arc<TradeExecutor>>,
    rpc_pool: Arc<RpcConnectionPool>,
    rpc_url: String, // 新增字段
    execution_semaphore: Arc<tokio::sync::Semaphore>, // 新增
    transaction_buffer: Arc<tokio::sync::Mutex<Vec<TradeDetails>>>,
    batch_processor: Arc<tokio::sync::Notify>,
    circular_cache: CircularCache<(String, usize)>,
}

impl GrpcMonitor {
    pub fn new(endpoint: String, auth_token: Option<String>, target_wallet: Pubkey, rpc_url: String) -> Self {
        let endpoint_clone = endpoint.clone();
        GrpcMonitor {
            endpoint: endpoint_clone,
            auth_token,
            target_wallet,
            executor: None,
            rpc_pool: Arc::new(RpcConnectionPool::new(vec![rpc_url.clone()], 5)),
            rpc_url,
            execution_semaphore: Arc::new(tokio::sync::Semaphore::new(10)), // 默认并发5
            transaction_buffer: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            batch_processor: Arc::new(tokio::sync::Notify::new()),
            circular_cache: CircularCache::new(10000), // 只保留最近10000条
        }
    }
    
    pub fn new_with_executor(
        endpoint: String, 
        auth_token: Option<String>, 
        target_wallet: Pubkey,
        executor: Arc<TradeExecutor>,
        rpc_url: String,
    ) -> Self {
        let endpoint_clone = endpoint.clone();
        GrpcMonitor {
            endpoint: endpoint_clone,
            auth_token,
            target_wallet,
            executor: Some(executor),
            rpc_pool: Arc::new(RpcConnectionPool::new(vec![rpc_url.clone()], 5)),
            rpc_url,
            execution_semaphore: Arc::new(tokio::sync::Semaphore::new(5)), // 默认并发5
            transaction_buffer: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            batch_processor: Arc::new(tokio::sync::Notify::new()),
            circular_cache: CircularCache::new(10000), // 只保留最近10000条
        }
    }

    pub async fn start_monitoring(self: Arc<Self>) -> Result<()> {
        trade_log!("启动gRPC监控服务，目标钱包: {}", self.target_wallet);
        trade_log!("连接到gRPC端点: {}", self.endpoint);
        // 启动批处理任务
        let batch_monitor = Arc::clone(&self);
        tokio::spawn(async move {
            batch_monitor.batch_process_trades().await;
        });
        // 启动健康检查任务
        let health_monitor = Arc::clone(&self);
        tokio::spawn(async move {
            health_monitor.health_check_loop().await;
        });
        loop {
            match self.monitor_loop().await {
                Ok(_) => warn!("监控循环结束，准备重启..."),
                Err(e) => error!("监控错误: {:?}", e),
            }
            info!("5秒后重试...");
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }
    }

    async fn monitor_loop(&self) -> Result<()> {
        let mut client = if let Some(token) = &self.auth_token {
            yellowstone_grpc_client::GeyserGrpcClient::build_from_shared(self.endpoint.clone())?
                .x_token(Some(token.clone()))?
                .tls_config(ClientTlsConfig::new().with_native_roots())? 
                .connect()
                .await?
        } else {
            yellowstone_grpc_client::GeyserGrpcClient::build_from_shared(self.endpoint.clone())?
                .tls_config(ClientTlsConfig::new().with_native_roots())?
                .connect()
                .await?
        };
        // 构建订阅请求
        let mut accounts = HashMap::new();
        accounts.insert(
            "wallet".to_string(),
            SubscribeRequestFilterAccounts {
                account: vec![self.target_wallet.to_string()],
                owner: vec![],
                filters: vec![],
                nonempty_txn_signature: None,
            },
        );
        let mut transactions = HashMap::new();
        transactions.insert(
            "wallet_tx".to_string(),
            SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                signature: None,
                account_include: vec![self.target_wallet.to_string()],
                account_exclude: vec![],
                account_required: vec![self.target_wallet.to_string()], // 只监控目标钱包参与的交易
            },
        );
        let request = SubscribeRequest {
            accounts,
            slots: HashMap::new(),
            transactions,
            transactions_status: HashMap::new(),
            blocks: HashMap::new(),
            blocks_meta: HashMap::new(),
            entry: HashMap::new(),
            commitment: Some(CommitmentLevel::Processed as i32),
            accounts_data_slice: vec![], // 可按需指定字段
            ping: None,
            from_slot: None,
        };
        info!("发送订阅请求...");
        let (mut subscribe_tx, mut stream) = client.subscribe_with_request(Some(request)).await?;
        info!("订阅成功，开始接收数据...");
        while let Some(message) = stream.next().await {
            match message {
                Ok(msg) => {
                    use yellowstone_grpc_proto::geyser::subscribe_update::UpdateOneof;
                    match msg.update_oneof {
                        Some(UpdateOneof::Ping(_)) => {
                            // 自动回复ping，保持连接
                            subscribe_tx.send(SubscribeRequest {
                                ping: Some(yellowstone_grpc_proto::geyser::SubscribeRequestPing { id: 1 }),
                                ..Default::default()
                            }).await?;
                        }
                        _ => {
                            // 保留原有业务处理逻辑
                            self.process_message(msg).await;
                        }
                    }
                }
                Err(e) => {
                    error!("消息接收错误: {:?}", e);
                    break;
                }
            }
        }
        Ok(())
    }

    async fn process_message(&self, msg: SubscribeUpdate) {
        if let Some(update_oneof) = &msg.update_oneof {
            use yellowstone_grpc_proto::geyser::subscribe_update::UpdateOneof;
            
            match update_oneof {
                UpdateOneof::Transaction(tx_update) => {
                    self.process_transaction(tx_update).await;
                }
                UpdateOneof::Account(account) => {
                    if let Some(acc) = &account.account {
                        let sol = acc.lamports as f64 / 1_000_000_000.0;
                        info!("=== 账户更新 ===");
                        info!("余额: {} SOL", sol);
                    }
                }
                UpdateOneof::Ping(_) => {
                    // 忽略ping消息
                }
                _ => {
                    // 忽略其他更新
                }
            }
        }
    }

    async fn process_transaction(&self, tx_update: &SubscribeUpdateTransaction) {
        // === CPMM 过滤逻辑：只处理 CPMM 相关交易 ===
        if let Some(tx_info) = &tx_update.transaction {
            if let Some(transaction) = &tx_info.transaction {
                if let Some(message) = &transaction.message {
                    let account_keys: Vec<String> = message.account_keys.iter()
                        .map(|key| bs58::encode(key).into_string())
                        .collect();
                    let mut is_cpmm = false;
                    for instruction in &message.instructions {
                        let program_id_index = instruction.program_id_index as usize;
                        if let Some(program_id) = account_keys.get(program_id_index) {
                            if program_id == crate::types::RAYDIUM_CPMM {
                                is_cpmm = true;
                                break;
                            }
                        }
                    }
                    if !is_cpmm {
                        // 不是 CPMM 相关交易，直接忽略
                        return;
                    }
                }
            }
        }
        // === 原有逻辑 ===
        if let Some(tx_info) = &tx_update.transaction {
            // 获取签名
            let signature = bs58::encode(&tx_info.signature).into_string();
            if let (Some(transaction), Some(meta)) = (&tx_info.transaction, &tx_info.meta) {
                if let Some(message) = &transaction.message {
                    let account_keys: Vec<String> = message.account_keys.iter()
                        .map(|key| bs58::encode(key).into_string())
                        .collect();
                    let mut found_dex_trade = false;
                    let mut is_pump_trade = false;
                    for (instruction_index, instruction) in message.instructions.iter().enumerate() {
                        let program_id = if (instruction.program_id_index as usize) < account_keys.len() {
                            &account_keys[instruction.program_id_index as usize]
                        } else {
                            continue;
                        };
                        if program_id != crate::types::RAYDIUM_AMM_V4 && 
                           program_id != crate::types::RAYDIUM_CPMM &&
                           program_id != crate::types::RAYDIUM_CLMM &&
                           program_id != crate::types::PUMP_FUN_PROGRAM {
                            continue;
                        }
                        if program_id == crate::types::PUMP_FUN_PROGRAM {
                            is_pump_trade = true;
                        }
                        found_dex_trade = true;
                        // 去重：同一signature+指令索引只处理一次
                        if !self.circular_cache.insert((signature.clone(), instruction_index)).await {
                            continue;
                        }
                        let pre_token_balances: Vec<serde_json::Value> = meta.pre_token_balances.iter()
                            .map(|balance| {
                                serde_json::json!({
                                    "accountIndex": balance.account_index,
                                    "mint": balance.mint,
                                    "owner": balance.owner,
                                    "programId": balance.program_id,
                                    "uiTokenAmount": {
                                        "amount": balance.ui_token_amount.as_ref().map(|ui| &ui.amount).unwrap_or(&"0".to_string()),
                                        "decimals": balance.ui_token_amount.as_ref().map(|ui| ui.decimals).unwrap_or(0),
                                        "uiAmountString": balance.ui_token_amount.as_ref().map(|ui| &ui.ui_amount_string).unwrap_or(&"0".to_string())
                                    }
                                })
                            })
                            .collect();
                        let post_token_balances: Vec<serde_json::Value> = meta.post_token_balances.iter()
                            .map(|balance| {
                                serde_json::json!({
                                    "accountIndex": balance.account_index,
                                    "mint": balance.mint,
                                    "owner": balance.owner,
                                    "programId": balance.program_id,
                                    "uiTokenAmount": {
                                        "amount": balance.ui_token_amount.as_ref().map(|ui| &ui.amount).unwrap_or(&"0".to_string()),
                                        "decimals": balance.ui_token_amount.as_ref().map(|ui| ui.decimals).unwrap_or(0),
                                        "uiAmountString": balance.ui_token_amount.as_ref().map(|ui| &ui.ui_amount_string).unwrap_or(&"0".to_string())
                                    }
                                })
                            })
                            .collect();
                        let parser = TransactionParser::new();
                        // 新增：只针对Raydium CPMM指令，传递指令账户并调用新方法
                        if program_id == crate::types::RAYDIUM_CPMM {
                            let instruction_accounts: Vec<String> = instruction.accounts.iter()
                                .filter_map(|&idx| account_keys.get(idx as usize).cloned())
                                .collect();
                            debug!("即将await parse_transaction_data_with_instruction_accounts");
                            let trade_result = parser.parse_transaction_data_with_instruction_accounts(
                                &signature,
                                &instruction_accounts,
                                &instruction.data,
                                &meta.pre_balances,
                                &meta.post_balances,
                                &pre_token_balances,
                                &post_token_balances,
                                &meta.log_messages,
                            ).await;
                            debug!("await parse_transaction_data_with_instruction_accounts完成");
                            match trade_result {
                                Ok(Some(trade_details)) => {
                                    // 替换原有is_signer和is_writable推断逻辑
                                    let mut chain_is_signer = Vec::new();
                                    let mut chain_is_writable = Vec::new();
                                    if let Some(msg) = &transaction.message {
                                        if let Some(header) = &msg.header {
                                            let total_keys = msg.account_keys.len();
                                            chain_is_signer = vec![false; total_keys];
                                            chain_is_writable = vec![false; total_keys];
                                            // 前 num_required_signatures 个是 signer
                                            for i in 0..header.num_required_signatures as usize {
                                                chain_is_signer[i] = true;
                                            }
                                            // 前 (num_required_signatures - num_readonly_signed_accounts) 个 signer 是 writable
                                            for i in 0..(header.num_required_signatures - header.num_readonly_signed_accounts) as usize {
                                                chain_is_writable[i] = true;
                                            }
                                            // 非 signer 部分
                                            let i = header.num_required_signatures as usize;
                                            // 非 signer 里，前 (total_keys - i - num_readonly_unsigned_accounts) 个是 writable
                                            for j in 0..(total_keys - i - header.num_readonly_unsigned_accounts as usize) {
                                                chain_is_writable[i + j] = true;
                                            }
                                            // 其余都是 readonly
                                        }
                                    }
                                    self.handle_parsed_trade_with_chain_meta(
                                        trade_details,
                                        instruction_accounts.clone(),
                                        chain_is_signer.clone(),
                                        chain_is_writable.clone(),
                                        instruction.data.clone(),
                                        Pubkey::from_str(program_id).unwrap()
                                    );
                                    found_dex_trade = true;
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    warn!("解析交易失败: {}", e);
                                }
                            }
                            continue;
                        }
                        // 其它DEX类型保持原有逻辑
                        debug!("即将await parse_transaction_data (其它DEX)");
                        let trade_result = parser.parse_transaction_data(
                            &signature,
                            &account_keys,
                            &instruction.data,
                            &meta.pre_balances,
                            &meta.post_balances,
                            &pre_token_balances,
                            &post_token_balances,
                            &meta.log_messages,
                        ).await;
                        debug!("await parse_transaction_data (其它DEX)完成");
                        match trade_result {
                            Ok(Some(trade_details)) => {
                                // 替换原有is_signer和is_writable推断逻辑
                                let mut chain_is_signer = Vec::new();
                                let mut chain_is_writable = Vec::new();
                                if let Some(msg) = &transaction.message {
                                    if let Some(header) = &msg.header {
                                        let total_keys = msg.account_keys.len();
                                        chain_is_signer = vec![false; total_keys];
                                        chain_is_writable = vec![false; total_keys];
                                        // 前 num_required_signatures 个是 signer
                                        for i in 0..header.num_required_signatures as usize {
                                            chain_is_signer[i] = true;
                                        }
                                        // 前 (num_required_signatures - num_readonly_signed_accounts) 个 signer 是 writable
                                        for i in 0..(header.num_required_signatures - header.num_readonly_signed_accounts) as usize {
                                            chain_is_writable[i] = true;
                                        }
                                        // 非 signer 部分
                                        let i = header.num_required_signatures as usize;
                                        // 非 signer 里，前 (total_keys - i - num_readonly_unsigned_accounts) 个是 writable
                                        for j in 0..(total_keys - i - header.num_readonly_unsigned_accounts as usize) {
                                            chain_is_writable[i + j] = true;
                                        }
                                        // 其余都是 readonly
                                    }
                                }
                                self.handle_parsed_trade_with_chain_meta(
                                    trade_details,
                                    account_keys.clone(),
                                    chain_is_signer.clone(),
                                    chain_is_writable.clone(),
                                    instruction.data.clone(),
                                    Pubkey::from_str(program_id).unwrap()
                                );
                                found_dex_trade = true;
                            }
                            Ok(None) => {}
                            Err(e) => {
                                warn!("解析交易失败: {}", e);
                            }
                        }
                    }
                    if !found_dex_trade {
                        if let Some(dex_name) = self.identify_dex(transaction) {
                            info!("DEX平台: {}", dex_name);
                        }
                        let fee_sol = meta.fee as f64 / 1_000_000_000.0;
                        info!("Gas费: {} SOL", fee_sol);
                        if !is_pump_trade {
                            self.analyze_balance_changes(meta, &transaction.message);
                        } else {
                            info!("[Pump提示] 该交易为Pump.fun，已省略详细余额变化分析，仅看上方业务摘要即可");
                        }
                    }
                }
            }
        }
    }

    /// 处理解析后的交易和账户
    fn handle_parsed_trade(&self, trade: TradeDetails, account_keys: Vec<String>) {
        trade_log!("trade.wallet = {}, self.target_wallet = {}", trade.wallet, self.target_wallet);
        trade_log!("相等判断: {}", trade.wallet == self.target_wallet);
        if trade.dex_type == crate::types::DexType::PumpFun {
            trade_log!("╔═══════════════ 📊 Pump.fun 交易解析 ═══════════════╗");
            trade_log!("║ DEX平台: Pump.fun");
            trade_log!("║ 交易方向: {:?}", trade.trade_direction);
            trade_log!("║ 交易钱包: {}", trade.wallet);
            trade_log!("║ 代币对: {} -> {}", 
                trade.token_in.symbol.as_ref().unwrap_or(&format!("代币({}...{})", 
                    &trade.token_in.mint.to_string()[..4],
                    &trade.token_in.mint.to_string().chars().rev().take(4).collect::<String>().chars().rev().collect::<String>()
                )),
                trade.token_out.symbol.as_ref().unwrap_or(&format!("代币({}...{})",
                    &trade.token_out.mint.to_string()[..4],
                    &trade.token_out.mint.to_string().chars().rev().take(4).collect::<String>().chars().rev().collect::<String>()
                ))
            );
            trade_log!("║ 输入金额: {} {}",
                self.format_token_amount(trade.amount_in, trade.token_in.decimals),
                trade.token_in.symbol.as_ref().unwrap_or(&"代币".to_string())
            );
            trade_log!("║ 输出金额: {} {}",
                self.format_token_amount(trade.amount_out, trade.token_out.decimals),
                trade.token_out.symbol.as_ref().unwrap_or(&"代币".to_string())
            );
            trade_log!("║ 价格: {:.8} SOL/代币", trade.price);
            trade_log!("║ 池子地址: {}", trade.pool_address);
            trade_log!("║ Gas费用: {:.6} SOL", trade.gas_fee as f64 / 1e9);
            trade_log!("║ [Pump提示] 该交易链上会有mint/销毁/分账等多种Token流转，以下只展示用户实际swap的输入输出");
            trade_log!("╚════════════════════════════════════════════╝");
        } else {
            trade_log!("╔═══════════════ 📊 交易解析成功 ═══════════════╗");
            trade_log!("║ DEX平台: {:?}", trade.dex_type);
            trade_log!("║ 交易方向: {:?}", trade.trade_direction);
            trade_log!("║ 交易钱包: {}", trade.wallet);
            trade_log!("║ 代币对: {} -> {}", 
                trade.token_in.symbol.as_ref().unwrap_or(&format!("代币({}...{})", 
                    &trade.token_in.mint.to_string()[..4],
                    &trade.token_in.mint.to_string().chars().rev().take(4).collect::<String>().chars().rev().collect::<String>()
                )),
                trade.token_out.symbol.as_ref().unwrap_or(&format!("代币({}...{})",
                    &trade.token_out.mint.to_string()[..4],
                    &trade.token_out.mint.to_string().chars().rev().take(4).collect::<String>().chars().rev().collect::<String>()
                ))
            );
            trade_log!("║ 输入金额: {}",
                self.format_token_amount(trade.amount_in, trade.token_in.decimals)
            );
            trade_log!("║ 输出金额: {} {}",
                self.format_token_amount(trade.amount_out, trade.token_out.decimals),
                trade.token_out.symbol.as_ref().unwrap_or(&"代币".to_string())
            );
            trade_log!("║ 价格: {:.8} SOL/代币", trade.price);
            trade_log!("║ 池子地址: {}", trade.pool_address);
            trade_log!("║ Gas费用: {:.6} SOL", trade.gas_fee as f64 / 1e9);
            trade_log!("╚════════════════════════════════════════════╝");
        }
        if trade.wallet == self.target_wallet {
            trade_log!("进入目标钱包跟单分支");
            if let Some(executor) = &self.executor {
                trade_log!("executor已配置，准备执行跟单");
                let executor = Arc::clone(executor);
                match trade.dex_type {
                    crate::types::DexType::RaydiumCPMM => {
                            warn!("PumpFun分支，account_keys数量不足，跳过跟单");
                    }
                    _ => {
                        warn!("未知DEX类型，跳过跟单");
                    }
                }
            } else {
                warn!("executor未配置，无法跟单");
            }
        } else {
            trade_log!("交易不是目标钱包，跳过跟单");
        }
    }

    /// 处理目标钱包的交易
    fn handle_target_wallet_trade(&self, trade: TradeDetails) {
        match trade.trade_direction {
            crate::types::TradeDirection::Buy => {
                trade_log!("目标钱包买入: {} {} (mint: {})", self.format_token_amount(trade.amount_out, trade.token_out.decimals), trade.token_out.symbol.as_ref().unwrap_or(&"未知代币".to_string()), trade.token_out.mint);
            }
            crate::types::TradeDirection::Sell => {
                trade_log!("目标钱包卖出: {} {} (mint: {})", self.format_token_amount(trade.amount_in, trade.token_in.decimals), trade.token_in.symbol.as_ref().unwrap_or(&"未知代币".to_string()), trade.token_in.mint);
            }
            _ => {}
        }
        
        // 执行跟单交易
        if let Some(_executor) = &self.executor {
            trade_log!("🚀 开始执行跟单交易...");
            
            // 由于TradeExecutor不支持Clone，我们需要在这里直接执行
            // 注意：这可能会阻塞监控线程，在生产环境中应该使用更好的异步处理方式
            let _trade_clone = trade.clone();
            
            // 使用tokio::spawn在后台执行交易
            tokio::spawn(async move {
                // 这里我们需要重新创建TradeExecutor实例
                // 在实际应用中，应该使用更好的架构来处理这个问题
                warn!("⚠️  跟单功能需要重新实现以支持异步执行");
            });
        } else {
            trade_log!("⚠️  交易执行器未配置，跳过跟单");
        }
    }

    fn identify_dex(&self, transaction: &Transaction) -> Option<String> {
        if let Some(message) = &transaction.message {
            for account_key in &message.account_keys {
                let key_str = bs58::encode(account_key).into_string();
                
                if key_str == RAYDIUM_V4 {
                    return Some("Raydium V4".to_string());
                } else if key_str == JUPITER_V6 {
                    return Some("Jupiter V6".to_string());
                } else if key_str == ORCA_WHIRLPOOL {
                    return Some("Orca Whirlpool".to_string());
                }
            }
        }
        None
    }

    fn analyze_balance_changes(&self, meta: &TransactionStatusMeta, message: &Option<Message>) {
        // 检查是否为PumpFun类型交易，如果是则跳过详细余额变化分析
        if let Some(msg) = message {
            // 取出所有account_keys
            let account_keys: Vec<String> = msg.account_keys.iter()
                .map(|k| bs58::encode(k).into_string())
                .collect();
            // 判断是否包含PumpFun program id
            if account_keys.iter().any(|k| k == crate::types::PUMP_FUN_PROGRAM) {
                info!("[Pump提示] 该交易为Pump.fun，已省略详细余额变化分析，仅看上方业务摘要即可");
                return;
            }
        }
        if meta.pre_balances.len() > 0 && meta.post_balances.len() > 0 {
            info!("║ ---- 余额变化分析 ----");
            
            let account_keys = message.as_ref()
                .map(|m| &m.account_keys)
                .map(|keys| keys.iter()
                    .map(|k| bs58::encode(k).into_string())
                    .collect::<Vec<String>>())
                .unwrap_or_default();
            
            for (i, (pre, post)) in meta.pre_balances.iter()
                .zip(meta.post_balances.iter()).enumerate() {
                if pre != post {
                    let change = *post as i64 - *pre as i64;
                    let change_sol = change as f64 / 1_000_000_000.0;
                    
                    if change_sol.abs() > 0.0001 {
                        let account_str = if i < account_keys.len() {
                            let addr = &account_keys[i];
                            if *addr == self.target_wallet.to_string() {
                                format!("目标钱包")
                            } else if addr == "So11111111111111111111111111111111111111112" {
                                format!("SOL")
                            } else {
                                format!("{}...{}", &addr[..4], &addr[addr.len()-4..])
                            }
                        } else {
                            format!("账户 {}", i)
                        };
                        
                        if change > 0 {
                            info!("║ {} 收到: +{:.6} SOL", account_str, change_sol);
                        } else {
                            info!("║ {} 发送: {:.6} SOL", account_str, change_sol);
                        }
                    }
                }
            }
            
            if meta.pre_token_balances.len() > 0 || meta.post_token_balances.len() > 0 {
                info!("║ ---- 代币余额变化 ----");
                self.analyze_token_balance_changes(meta);
            }
        }
    }

    fn analyze_token_balance_changes(&self, meta: &TransactionStatusMeta) {
        let mut token_changes: HashMap<usize, (Option<u64>, Option<u64>, Option<String>)> = HashMap::new();
        
        for pre_balance in &meta.pre_token_balances {
            let key = pre_balance.account_index as usize;
            let amount = pre_balance.ui_token_amount.as_ref()
                .and_then(|ui| ui.ui_amount_string.parse::<f64>().ok())
                .map(|v| (v * 10f64.powi(pre_balance.ui_token_amount.as_ref().map(|ui| ui.decimals).unwrap_or(0) as i32)) as u64);
            token_changes.entry(key).or_insert((None, None, None)).0 = amount;
            token_changes.entry(key).or_insert((None, None, None)).2 = Some(pre_balance.mint.clone());
        }
        
        for post_balance in &meta.post_token_balances {
            let key = post_balance.account_index as usize;
            let amount = post_balance.ui_token_amount.as_ref()
                .and_then(|ui| ui.ui_amount_string.parse::<f64>().ok())
                .map(|v| (v * 10f64.powi(post_balance.ui_token_amount.as_ref().map(|ui| ui.decimals).unwrap_or(0) as i32)) as u64);
            token_changes.entry(key).or_insert((None, None, None)).1 = amount;
            if token_changes.get(&key).unwrap().2.is_none() {
                token_changes.entry(key).or_insert((None, None, None)).2 = Some(post_balance.mint.clone());
            }
        }
        
        for (_account_index, (pre, post, mint)) in token_changes {
            if let (Some(pre_amount), Some(post_amount), Some(mint_addr)) = (pre, post, mint) {
                if pre_amount != post_amount {
                    let change = post_amount as i64 - pre_amount as i64;
                    let token_symbol = self.get_token_symbol(&mint_addr);
                    
                    if change > 0 {
                        info!("║ 代币收到: +{} {} ({}...{})", 
                            change, token_symbol, &mint_addr[..4], &mint_addr[mint_addr.len()-4..]);
                    } else {
                        info!("║ 代币发送: {} {} ({}...{})", 
                            change.abs(), token_symbol, &mint_addr[..4], &mint_addr[mint_addr.len()-4..]);
                    }
                }
            }
        }
    }

    fn get_token_symbol(&self, mint: &str) -> String {
        match mint {
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" => "USDC".to_string(),
            "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB" => "USDT".to_string(),
            _ => "未知".to_string(),
        }
    }

    // 新增：带链上账户顺序和权限的handle_parsed_trade
    fn handle_parsed_trade_with_chain_meta(&self, trade: TradeDetails, account_keys: Vec<String>, _is_signer: Vec<bool>, _is_writable: Vec<bool>, instruction_data: Vec<u8>, _program_id: Pubkey) {
        // 只在RaydiumCPMM分支用链上顺序
        if trade.wallet == self.target_wallet {
            if let Some(executor) = &self.executor {
                let executor = Arc::clone(executor);
                match trade.dex_type {
                    crate::types::DexType::RaydiumCPMM => {
                        if account_keys.len() > 3 {
                            debug!("Raydium CPMM分支，account_keys数量: {}", account_keys.len());
                            // 1. 优先尝试极速quick_parse
                            let quick_parser = QuickTradeParser { wsol_mint: Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap() };
                            let instruction = CompiledInstruction {
                                program_id_index: 0, // 实际用不到
                                accounts: vec![],    // 实际用不到
                                data: instruction_data.clone(),
                            };
                            if let Some(cpmm_accounts) = quick_parser.quick_parse(&instruction, &account_keys, &executor.copy_wallet.pubkey()) {
                                debug!("极速分支已触发");
                                let trade_clone = trade.clone();
                                let executor = Arc::clone(&executor);
                                let wallet = executor.copy_wallet.clone();
                                let rpc_pool = self.rpc_pool.clone();
                                let execution_semaphore = self.execution_semaphore.clone();
                                tokio::spawn(async move {
                                    let _permit = execution_semaphore.acquire_owned().await;
                                    let client = rpc_pool.get_client().await;
                                    // 加载配置
                                    let config = match crate::config::Config::load() {
                                        Ok(cfg) => cfg,
                                        Err(e) => {
                                            error!("加载配置失败: {}", e);
                                            return;
                                        }
                                    };
                                    // 检查跟单是否启用
                                    if !config.copy_trading.enabled {
                                        info!("[配置] 跟单功能已禁用，跳过交易");
                                        return;
                                    }
                                    // 创建修改后的交易对象，使用固定金额
                                    let mut modified_trade = trade_clone.clone();
                                    if config.copy_trading.use_fixed_amount {
                                        let fixed_amount_sol = config.copy_trading.fixed_trade_amount_sol;
                                        let fixed_amount_lamports = (fixed_amount_sol * 1_000_000_000.0) as u64;
                                        
                                        if trade_clone.trade_direction == crate::types::TradeDirection::Buy {
                                            // 买入：固定输入金额（WSOL）
                                            modified_trade.amount_in = fixed_amount_lamports;
                                            modified_trade.amount_out = 0; // 让AMM计算输出
                                            info!("[固定金额] 买入交易使用固定金额: {} SOL ({} lamports)", 
                                                fixed_amount_sol, fixed_amount_lamports);
                                        } else {
                                            // 卖出：每次全部卖出钱包余额
                                            let target_token_balance = client.get_token_account_balance(
                                                &get_associated_token_address(&wallet.pubkey(), &trade_clone.token_in.mint)
                                            ).map(|b| b.amount.parse::<u64>().unwrap_or(0))
                                            .unwrap_or(0);
                                            modified_trade.amount_in = target_token_balance;
                                            modified_trade.amount_out = 0; // 让AMM计算输出
                                            info!("[全部卖出] 卖出交易：全部卖出 {} tokens", target_token_balance);
                                        }
                                    }
                                    // 卖出前检查余额
                                    if modified_trade.trade_direction == crate::types::TradeDirection::Sell {
                                        let token_mint = modified_trade.token_in.mint;
                                        let ata = get_associated_token_address(&wallet.pubkey(), &token_mint);
                                        info!("计算ATA: owner={}, mint={}, ata={}", wallet.pubkey(), token_mint, ata);
                                        let ata_account = client.get_account(&ata);
                                        if ata_account.is_err() {
                                            warn!("[风控] 跟单钱包ATA账户不存在，跳过卖出。ATA: {}", ata);
                                            return;
                                        }
                                        let balance = client.get_token_account_balance(&ata)
                                            .map(|b| b.amount.parse::<u64>().unwrap_or(0))
                                            .unwrap_or(0);
                                        if balance < modified_trade.amount_in {
                                            warn!("[风控] 跟单钱包无足够{}余额，跳过卖出。余额: {}，需卖出: {}", modified_trade.token_in.symbol.as_ref().unwrap_or(&"目标币种".to_string()), balance, modified_trade.amount_in);
                                            return;
                                        }
                                        info!("[风控] 卖出余额检查通过: 余额={}, 需卖出={}", balance, modified_trade.amount_in);
                                    } else if modified_trade.trade_direction == crate::types::TradeDirection::Buy {
                                        // 买入交易：检查输入代币余额（通常是WSOL）
                                        let token_mint = modified_trade.token_in.mint;
                                        let ata = get_associated_token_address(&wallet.pubkey(), &token_mint);
                                        info!("计算ATA: owner={}, mint={}, ata={}", wallet.pubkey(), token_mint, ata);
                                        let balance = client.get_token_account_balance(&ata)
                                            .map(|b| b.amount.parse::<u64>().unwrap_or(0))
                                            .unwrap_or(0);
                                        
                                        // 检查SOL余额（用于支付手续费）
                                        let sol_balance = client.get_balance(&wallet.pubkey()).unwrap_or(0);
                                        let min_sol_for_fees = (config.copy_trading.min_sol_balance * 1_000_000_000.0) as u64;
                                        
                                        if balance < modified_trade.amount_in {
                                            warn!("[风控] 跟单钱包无足够{}余额，跳过买入。余额: {}，需买入: {}", 
                                                modified_trade.token_in.symbol.as_ref().unwrap_or(&"输入币种".to_string()), 
                                                balance, 
                                                modified_trade.amount_in);
                                            return;
                                        }
                                        
                                        if sol_balance < min_sol_for_fees {
                                            warn!("[风控] 跟单钱包SOL余额不足支付手续费，跳过买入。SOL余额: {}，最小需要: {}", 
                                                sol_balance, min_sol_for_fees);
                                            return;
                                        }
                                        
                                        info!("[风控] 买入余额检查通过: {}余额={}, 需买入={}, SOL余额={}", 
                                            modified_trade.token_in.symbol.as_ref().unwrap_or(&"输入币种".to_string()),
                                            balance, 
                                            modified_trade.amount_in,
                                            sol_balance);
                                    }
                                    
                                    info!("ATA已全部创建，开始执行swap跟单");
                                    // 计算最小输出
                                    let min_amount_out = if modified_trade.trade_direction == crate::types::TradeDirection::Buy {
                                        // 买入：最小输出是代币数量，设置为1以避免0
                                        1
                                    } else {
                                        // 卖出：最小输出是SOL数量
                                        // 使用配置的滑点或默认10%
                                        let slippage = executor.config.slippage_tolerance;
                                        let input_vault_balance = client.get_token_account_balance(&cpmm_accounts.input_vault)
                                            .map(|b| b.amount.parse::<u64>().unwrap_or(0))
                                            .unwrap_or(0);
                                        let output_vault_balance = client.get_token_account_balance(&cpmm_accounts.output_vault)
                                            .map(|b| b.amount.parse::<u64>().unwrap_or(0))
                                            .unwrap_or(0);
                                        
                                        let expected_sol = (modified_trade.amount_in as f64 * output_vault_balance as f64 / input_vault_balance as f64) as u64;
                                        ((expected_sol as f64) * (1.0 - slippage)) as u64
                                    };
                                    
                                    info!("计算的最小输出: {}", min_amount_out);
                                    
                                    // 验证交易参数
                                    if modified_trade.amount_in == 0 {
                                        error!("错误：输入金额为0，跳过交易");
                                        return;
                                    }
                                    
                                    // 验证程序ID
                                    let expected_program_id = Pubkey::from_str(crate::types::RAYDIUM_CPMM).unwrap();
                                    if trade_clone.program_id != expected_program_id {
                                        warn!("警告：交易程序ID不匹配，期望: {}, 实际: {}", 
                                            expected_program_id, trade_clone.program_id);
                                        // 强制使用正确的程序ID
                                        modified_trade.program_id = expected_program_id;
                                    }
                                    
                                    // 验证账户地址
                                    info!("交易前验证：");
                                    info!("  输入代币: {}", modified_trade.token_in.mint);
                                    info!("  输出代币: {}", modified_trade.token_out.mint);
                                    info!("  输入金额: {}", modified_trade.amount_in);
                                    info!("  程序ID: {}", modified_trade.program_id);
                                    
                                    let wsol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
                                    let is_temp_wsol = modified_trade.trade_direction == crate::types::TradeDirection::Buy
                                        && cpmm_accounts.input_mint == wsol_mint;

                                    let res = if is_temp_wsol {
                                        // 极速临时WSOL方案
                                        executor.execute_trade_with_temp_wsol(&modified_trade, &cpmm_accounts, min_amount_out).await
                                    } else {
                                        // 其它情况走原有静态方法
                                        TradeExecutor::execute_raydium_cpmm_trade_static(
                                            &client, &wallet, &modified_trade, &cpmm_accounts, &[], min_amount_out,
                                            None, None, None, None, None
                                        ).await
                                    };
                                });
                                return;
                            }
                        } else {
                            warn!("Raydium CPMM分支，account_keys数量不足，跳过跟单，当前keys: {:?}", account_keys);
                        }
                    }
                    _ => {
                        // 其它DEX类型，保持原有逻辑
                        self.handle_parsed_trade(trade, account_keys);
                    }
                }
            }
        }
    }

    /// 快速路径跟单，跳过大部分验证，直接构建并发送交易
    pub async fn fast_execute_trade(&self, trade: &TradeDetails, cpmm_accounts: &RaydiumCpmmSwapAccounts) {
        // 跳过所有非必要的验证，直接构建交易并发送
        if let Some(executor) = &self.executor {
            // 预先计算好的最小输出（使用固定滑点10%）
            let min_amount_out = (trade.amount_out as f64 * 0.9) as u64;
            let trade = trade.clone();
            let cpmm_accounts = cpmm_accounts.clone();
            let executor = Arc::clone(executor);
            tokio::spawn(async move {
                // 这里调用即将实现的 execute_trade_fast_path
                if let Err(e) = executor.execute_trade_fast_path(&trade, &cpmm_accounts, min_amount_out).await {
                    tracing::error!("fast_execute_trade 执行失败: {:?}", e);
                }
            });
        } else {
            tracing::warn!("fast_execute_trade: executor 未初始化");
        }
    }

    /// 通用余额检查
    pub async fn check_balance_for_trade(
        client: &RpcClient,
        wallet: &Pubkey,
        trade: &crate::types::TradeDetails,
        amount: u64,
    ) -> anyhow::Result<bool> {
        let ata = get_associated_token_address(wallet, &trade.token_in.mint);
        let balance = client.get_token_account_balance(&ata)
            .map(|b| b.amount.parse::<u64>().unwrap_or(0))
            .unwrap_or(0);
        if balance < amount {
            warn!("[风控] 余额不足: {} < {}", balance, amount);
            return Ok(false);
        }
        Ok(true)
    }

    async fn batch_process_trades(&self) {
        let batch_size = 10;
        let batch_timeout = std::time::Duration::from_millis(100);
        loop {
            tokio::select! {
                _ = self.batch_processor.notified() => {},
                _ = tokio::time::sleep(batch_timeout) => {},
            }
            let mut buffer = self.transaction_buffer.lock().await;
            if buffer.is_empty() { continue; }
            let len = buffer.len();
            let trades: Vec<_> = buffer.drain(..std::cmp::min(len, batch_size)).collect();
            drop(buffer);
            // 并行处理批次中的交易
            let futures: Vec<_> = trades.into_iter().map(|trade| {
                let executor = self.executor.clone();
                tokio::spawn(async move {
                    // 处理交易逻辑
                    if let Some(exec) = executor {
                        // exec.execute_trade(&trade).await.ok();
                    }
                })
            }).collect();
            futures::future::join_all(futures).await;
        }
    }

    async fn health_check_loop(&self) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            if let Err(e) = self.send_ping().await {
                error!("连接健康检查失败: {:?}", e);
                // 触发重连逻辑，可根据实际需求实现
            }
        }
    }

    async fn send_ping(&self) -> Result<()> {
        let ping_request = SubscribeRequest {
            ping: Some(yellowstone_grpc_proto::geyser::SubscribeRequestPing { id: 1 }),
            ..Default::default()
        };
        // 假设 self.client 是 gRPC 客户端
        // self.client.send_ping(ping_request).await?;
        Ok(())
    }

    fn format_token_amount(&self, amount: u64, decimals: u8) -> String {
        let divisor = 10u64.pow(decimals as u32);
        let integer = amount / divisor;
        let fractional = amount % divisor;
        if decimals == 0 {
            format!("{}", integer)
        } else {
            format!("{}.{}", integer, format!("{:0>width$}", fractional, width = decimals as usize))
        }
    }
}

pub struct CircularCache<T> {
    items: Arc<Mutex<VecDeque<T>>>,
    max_size: usize,
}

impl<T: Eq + Clone> CircularCache<T> {
    pub fn new(max_size: usize) -> Self {
        Self {
            items: Arc::new(Mutex::new(VecDeque::with_capacity(max_size))),
            max_size,
        }
    }
    pub async fn insert(&self, item: T) -> bool {
        let mut items = self.items.lock().await;
        if items.contains(&item) {
            return false;
        }
        if items.len() >= self.max_size {
            items.pop_front();
        }
        items.push_back(item);
        true
    }
}

lazy_static! {
    static ref RAYDIUM_CPMM_DISCRIMINATOR: [u8; 8] = [0x8f, 0xbe, 0x5a, 0xda, 0xc4, 0x1e, 0x33, 0xde];
    static ref WSOL_MINT: Pubkey = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
}

#[cfg(debug_assertions)]
macro_rules! debug_log {
    ($($arg:tt)*) => { info!($($arg)*); };
}
#[cfg(not(debug_assertions))]
macro_rules! debug_log {
    ($($arg:tt)*) => {};
}

pub struct SwapInstruction {
    pub amount_in: u64,
    pub min_amount_out: u64,
}

pub fn parse_instruction_data(data: &Bytes) -> Result<SwapInstruction> {
    if data.len() < 24 { return Err(anyhow::anyhow!("数据长度不足")); }
    let discriminator = &data[..8];
    if discriminator != &*RAYDIUM_CPMM_DISCRIMINATOR { return Err(anyhow::anyhow!("discriminator不匹配")); }
    let amount_in = u64::from_le_bytes(data[8..16].try_into().unwrap());
    let min_amount_out = u64::from_le_bytes(data[16..24].try_into().unwrap());
    Ok(SwapInstruction { amount_in, min_amount_out })
}