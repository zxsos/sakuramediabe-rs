//! 延迟测量：一元 RPC 与流式首帧的量级。
//!
//! 只做汇总统计，不做 benchmark 框架 —— 需要的是「够不够把插件拆出去」
//! 这个判断题的答案，而不是精确数字（报告 §3）。

use std::time::Duration;

/// 一组延迟样本的 min / p50 / p95 / max，单位微秒。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// 样本数。
    pub count: usize,
    /// 最小。
    pub min_us: u128,
    /// 中位数。
    pub p50_us: u128,
    /// 95 分位。
    pub p95_us: u128,
    /// 最大。
    pub max_us: u128,
}

/// 把一组样本汇总成 [`Summary`]。
///
/// 样本必须非空：空样本给不出分位数，宁可 panic 也不要返回一堆 0 去误导读者。
pub fn summarize(samples: &[Duration]) -> Summary {
    assert!(!samples.is_empty(), "延迟样本不能为空");

    let mut micros: Vec<u128> = samples.iter().map(|sample| sample.as_micros()).collect();
    micros.sort_unstable();

    let last = micros.len() - 1;
    let pick = |quantile: f64| {
        let index = (last as f64 * quantile).round() as usize;
        micros[index.min(last)]
    };

    Summary {
        count: micros.len(),
        min_us: micros[0],
        p50_us: pick(0.5),
        p95_us: pick(0.95),
        max_us: micros[last],
    }
}
