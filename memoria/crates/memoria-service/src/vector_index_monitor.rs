//! Lightweight vector index performance monitor.
//!
//! Tracks query latency and triggers rebuild signals when performance degrades.
//! Uses lock-free data structures to minimize overhead.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

const RING_BUFFER_SIZE: usize = 100;
const SLOW_QUERY_THRESHOLD_MS: u64 = 100;
const CHECK_INTERVAL_SECS: i64 = 300; // 5分钟最多检查一次

/// 粗粒度时钟：后台每秒更新一次，避免每次查询都做系统调用
static COARSE_CLOCK_SECS: AtomicI64 = AtomicI64::new(0);

pub fn init_coarse_clock() {
    use std::sync::Once;
    use std::time::{SystemTime, UNIX_EPOCH};
    
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // 初始化
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        COARSE_CLOCK_SECS.store(now, Ordering::Relaxed);
        
        // 后台线程每秒更新
        std::thread::Builder::new()
            .name("coarse-clock".into())
            .spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let t = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                COARSE_CLOCK_SECS.store(t, Ordering::Relaxed);
            })
            .expect("failed to spawn coarse-clock thread");
    });
}

#[inline]
fn coarse_now() -> i64 {
    COARSE_CLOCK_SECS.load(Ordering::Relaxed)
}

#[derive(Clone)]
pub struct VectorIndexMonitor {
    table_name: String,
    /// 最近N次查询延迟（环形缓冲区，无锁）
    recent_latencies: Arc<RingBuffer>,
    /// 慢查询计数
    slow_query_count: Arc<AtomicU64>,
    /// 上次检查时间戳
    pub last_check_ts: Arc<AtomicI64>,
    /// 发送重建信号的channel
    tx: tokio::sync::mpsc::UnboundedSender<RebuildSignal>,
}

pub struct RebuildSignal {
    pub table_name: String,
    pub reason: SignalReason,
}

#[derive(Debug)]
pub enum SignalReason {
    HighLatency { p95_ms: u64, slow_count: u64 },
}

impl VectorIndexMonitor {
    pub fn new(
        table_name: String,
        tx: tokio::sync::mpsc::UnboundedSender<RebuildSignal>,
    ) -> Self {
        Self {
            table_name,
            recent_latencies: Arc::new(RingBuffer::new(RING_BUFFER_SIZE)),
            slow_query_count: Arc::new(AtomicU64::new(0)),
            last_check_ts: Arc::new(AtomicI64::new(0)),
            tx,
        }
    }

    /// 记录一次查询（轻量级，无阻塞）
    pub fn record_query(&self, latency_ms: u64, _result_count: usize) {
        // 1. 更新环形缓冲区
        self.recent_latencies.push(latency_ms);

        // 2. 慢查询计数
        if latency_ms > SLOW_QUERY_THRESHOLD_MS {
            self.slow_query_count.fetch_add(1, Ordering::Relaxed);
        }

        // 3. 检查是否需要触发（限流：5分钟最多一次）
        let now = coarse_now();
        let last_check = self.last_check_ts.load(Ordering::Relaxed);
        if now - last_check < CHECK_INTERVAL_SECS {
            return; // 跳过，避免频繁检查
        }

        // 4. 尝试触发（CAS确保只有一个线程执行）
        if self
            .last_check_ts
            .compare_exchange(last_check, now, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            self.try_trigger();
        }
    }

    /// 尝试触发重建信号（轻量级判断）
    fn try_trigger(&self) {
        // 基于延迟和慢查询比例触发
        let p95 = self.recent_latencies.percentile(95);
        let slow_count = self.slow_query_count.swap(0, Ordering::Relaxed);

        // 触发条件：p95 > 200ms 或 慢查询比例 > 20%
        if p95 > 200 || slow_count > 20 {
            let _ = self.tx.send(RebuildSignal {
                table_name: self.table_name.clone(),
                reason: SignalReason::HighLatency {
                    p95_ms: p95,
                    slow_count,
                },
            });
        }
    }
}

/// 无锁环形缓冲区（用于存储最近N次延迟）
pub struct RingBuffer {
    data: Vec<AtomicU64>,
    index: AtomicU64,
    size: usize,
}

impl RingBuffer {
    fn new(size: usize) -> Self {
        let mut data = Vec::with_capacity(size);
        for _ in 0..size {
            data.push(AtomicU64::new(0));
        }
        Self {
            data,
            index: AtomicU64::new(0),
            size,
        }
    }

    fn push(&self, value: u64) {
        let idx = self.index.fetch_add(1, Ordering::Relaxed) % self.size as u64;
        self.data[idx as usize].store(value, Ordering::Relaxed);
    }

    pub fn percentile(&self, p: u8) -> u64 {
        let mut values: Vec<u64> = self
            .data
            .iter()
            .map(|v| v.load(Ordering::Relaxed))
            .filter(|&v| v > 0)
            .collect();
        if values.is_empty() {
            return 0;
        }
        values.sort_unstable();
        let idx = (values.len() * p as usize / 100).min(values.len() - 1);
        values[idx]
    }
}
