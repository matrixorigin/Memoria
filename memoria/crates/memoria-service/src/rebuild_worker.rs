//! Background worker for vector index rebuilding.
//!
//! Consumes rebuild signals, acquires distributed locks, and executes
//! index rebuilds with adaptive cooldown periods.

use crate::vector_index_monitor::RebuildSignal;
use memoria_core::MemoriaError;
use memoria_storage::SqlMemoryStore;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

pub struct RebuildWorker {
    store: Arc<SqlMemoryStore>,
    rx: mpsc::UnboundedReceiver<RebuildSignal>,
}

impl RebuildWorker {
    pub fn new(
        store: Arc<SqlMemoryStore>,
        rx: mpsc::UnboundedReceiver<RebuildSignal>,
    ) -> Self {
        Self { store, rx }
    }

    pub async fn run(mut self) {
        info!("RebuildWorker started");
        while let Some(signal) = self.rx.recv().await {
            if let Err(e) = self.handle_signal(signal).await {
                warn!("Failed to handle rebuild signal: {}", e);
            }
        }
    }

    async fn handle_signal(&self, signal: RebuildSignal) -> Result<(), MemoriaError> {
        let table = &signal.table_name;
        debug!("Received rebuild signal for {}: {:?}", table, signal.reason);

        // 1. 检查分布式冷却状态
        let (should_rebuild, current_rows, cooldown_remaining) =
            self.store.should_rebuild_vector_index(table).await?;

        if let Some(remaining) = cooldown_remaining {
            debug!(
                "Vector index rebuild for {} in cooldown: {}s remaining",
                table, remaining
            );
            return Ok(());
        }

        if !should_rebuild {
            debug!("Vector index rebuild for {} not needed", table);
            return Ok(());
        }

        // 2. 获取分布式锁（防止多节点同时重建）
        let lock_key = format!("vector_index_rebuild:{}", table);
        let lock_acquired = self
            .store
            .try_acquire_lock(&lock_key, 600) // 10分钟锁
            .await?;

        if !lock_acquired {
            debug!("Another node is rebuilding vector index for {}", table);
            return Ok(());
        }

        // 3. 执行重建
        info!(
            "Starting vector index rebuild for {}: {} rows, reason: {:?}",
            table, current_rows, signal.reason
        );

        let start = std::time::Instant::now();
        match self.store.rebuild_vector_index(table).await {
            Ok(rebuilt_rows) => {
                let elapsed = start.elapsed();
                info!(
                    "Vector index rebuilt for {}: {} rows in {:?}",
                    table, rebuilt_rows, elapsed
                );

                // 4. 记录冷却时间（自适应）
                let cooldown_secs = calculate_cooldown(rebuilt_rows);
                self.store
                    .record_vector_index_rebuild(table, rebuilt_rows, cooldown_secs)
                    .await?;

                info!(
                    "Next rebuild for {} allowed after {}h",
                    table,
                    cooldown_secs / 3600
                );
            }
            Err(e) => {
                warn!("Vector index rebuild failed for {}: {}", table, e);
                // 记录失败，使用指数退避
                match self.store.record_vector_index_rebuild_failure(table).await {
                    Ok(cooldown) => {
                        info!("Rebuild failure recorded, next retry in {}s", cooldown);
                    }
                    Err(record_err) => {
                        warn!("Failed to record rebuild failure: {}", record_err);
                    }
                }
            }
        }

        // 5. 释放锁
        self.store.release_lock(&lock_key).await?;

        Ok(())
    }
}

/// 自适应冷却时间计算
fn calculate_cooldown(row_count: i64) -> i64 {
    match row_count {
        0..=500 => 0,            // 不需要索引
        501..=5_000 => 3600,     // 1小时
        5_001..=20_000 => 10800,  // 3小时
        20_001..=50_000 => 21600, // 6小时
        50_001..=100_000 => 43200, // 12小时
        _ => 86400,              // 24小时
    }
}
