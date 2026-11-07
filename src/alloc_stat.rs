//! 全局分配器计数（诊断内存增长用，开销只有几条原子加）。
//!
//! 目的：把「Rust 侧活字节」和「RSS」分开看。
//! - 活字节一路涨 → 我们自己的代码漏了（Vec/String/Arc 没释放），
//!   而且大小直方图能直接指出漏的块有多大；
//! - 活字节平稳而 RSS 涨 → 不是 Rust 堆，是 C 侧（mpv/libplacebo/GL 驱动）
//!   或者 glibc 的 arena/碎片行为。
//!
//! 所以它同时是诊断工具和长期看门狗：`[fps]` 行里的「堆/RSS」两列。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

/// 桶：0..=23 表示 2 的幂（<4KB 逐级细分），24 = >=16MB
const BUCKETS: usize = 26;

static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_BLOCKS: AtomicU64 = AtomicU64::new(0);
static BUCKET_BYTES: [AtomicU64; BUCKETS] = [const { AtomicU64::new(0) }; BUCKETS];

fn bucket_of(size: usize) -> usize {
    // 32B 以下归 0，然后每翻一倍一档
    let mut b = 0usize;
    let mut s = 32usize;
    while s < size && b + 1 < BUCKETS {
        s <<= 1;
        b += 1;
    }
    b
}

fn bucket_label(b: usize) -> String {
    if b == 0 {
        "<32B".to_string()
    } else if b + 1 >= BUCKETS {
        ">=512MB".to_string()
    } else {
        format!("{}B", 32usize << b)
    }
}

pub struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            LIVE_BYTES.fetch_add(l.size() as u64, Ordering::Relaxed);
            LIVE_BLOCKS.fetch_add(1, Ordering::Relaxed);
            BUCKET_BYTES[bucket_of(l.size())].fetch_add(l.size() as u64, Ordering::Relaxed);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = System.alloc_zeroed(l);
        if !p.is_null() {
            LIVE_BYTES.fetch_add(l.size() as u64, Ordering::Relaxed);
            LIVE_BLOCKS.fetch_add(1, Ordering::Relaxed);
            BUCKET_BYTES[bucket_of(l.size())].fetch_add(l.size() as u64, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE_BYTES.fetch_sub(l.size() as u64, Ordering::Relaxed);
        LIVE_BLOCKS.fetch_sub(1, Ordering::Relaxed);
        BUCKET_BYTES[bucket_of(l.size())].fetch_sub(l.size() as u64, Ordering::Relaxed);
        System.dealloc(p, l);
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        let np = System.realloc(p, l, new_size);
        if !np.is_null() {
            LIVE_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
            LIVE_BYTES.fetch_sub(l.size() as u64, Ordering::Relaxed);
            BUCKET_BYTES[bucket_of(new_size)].fetch_add(new_size as u64, Ordering::Relaxed);
            BUCKET_BYTES[bucket_of(l.size())].fetch_sub(l.size() as u64, Ordering::Relaxed);
        }
        np
    }
}

/// (活字节, 活块数)
pub fn live() -> (u64, u64) {
    (
        LIVE_BYTES.load(Ordering::Relaxed),
        LIVE_BLOCKS.load(Ordering::Relaxed),
    )
}

/// 当前活字节最大的几个尺寸档，用于定位「漏的是多大的对象」
pub fn top_buckets(n: usize) -> Vec<(String, u64)> {
    let mut v: Vec<(String, u64)> = (0..BUCKETS)
        .map(|b| (bucket_label(b), BUCKET_BYTES[b].load(Ordering::Relaxed)))
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v.into_iter().filter(|(_, x)| *x > 0).take(n).collect()
}

/// 进程 RSS（KB），从 /proc/self/statm 读（第 2 个字段是常驻页数）
pub fn rss_kb() -> u64 {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let resident: u64 = s
        .split_whitespace()
        .nth(1)
        .and_then(|x| x.parse().ok())
        .unwrap_or(0);
    resident * 4 // 页大小 4KB
}
