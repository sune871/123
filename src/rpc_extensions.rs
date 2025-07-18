use solana_client::rpc_client::RpcClient;
use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;
use std::time::Duration;
use anyhow::Result;

pub trait RpcClientExt {
    fn get_account_with_timeout_blocking(&self, pubkey: &Pubkey, timeout: Duration) -> Result<Account>;
}

impl RpcClientExt for RpcClient {
    fn get_account_with_timeout_blocking(&self, pubkey: &Pubkey, timeout: Duration) -> Result<Account> {
        use std::sync::mpsc;
        use std::thread;

        let (tx, rx) = mpsc::channel();
        let pubkey = *pubkey;
        let url = self.url();

        thread::spawn(move || {
            let client = RpcClient::new(url);
            let result = client.get_account(&pubkey);
            let _ = tx.send(result);
        });

        match rx.recv_timeout(timeout) {
            Ok(Ok(account)) => Ok(account),
            Ok(Err(e)) => Err(anyhow::anyhow!("RPC错误: {}", e)),
            Err(_) => Err(anyhow::anyhow!("RPC请求超时")),
        }
    }
}

// 新增异步安全版本
pub async fn get_account_with_timeout_async(rpc: &RpcClient, pubkey: &Pubkey, timeout: Duration) -> Result<Account> {
    let pubkey = *pubkey;
    let url = rpc.url();
    let result = tokio::time::timeout(timeout, tokio::task::spawn_blocking(move || {
        let client = RpcClient::new(url);
        client.get_account(&pubkey)
    })).await;
    match result {
        Ok(Ok(Ok(account))) => Ok(account),
        Ok(Ok(Err(e))) => Err(anyhow::anyhow!("RPC错误: {}", e)),
        Ok(Err(join_err)) => Err(anyhow::anyhow!("线程join失败: {}", join_err)),
        Err(_) => Err(anyhow::anyhow!("RPC请求超时")),
    }
} 