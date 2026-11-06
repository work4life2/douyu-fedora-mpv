//! 斗鱼签名：内嵌 QuickJS（rquickjs）执行服务端下发的混淆脚本。
//! 签名脚本按房间下发、每日重生成，无法手写算法，必须用 JS 引擎执行。
//! CryptoJS 以字符串内嵌进二进制（与斗鱼网页端同源）。

use rquickjs::{Context, Runtime};

const CRYPTOJS: &str = include_str!("cryptojs.min.js");

/// 执行 ub98484234(rid, did, tt)，返回 "v=..&did=..&tt=..&sign=.."
pub fn sign(rid: &str, did: &str, tt: i64, script: &str) -> Result<String, String> {
    let rt = Runtime::new().map_err(|e| format!("JS 运行时创建失败: {e}"))?;
    let ctx = Context::full(&rt).map_err(|e| format!("JS 上下文创建失败: {e}"))?;

    ctx.with(|ctx| -> Result<String, String> {
        // 1. 加载 CryptoJS（UMD 在裸引擎里会挂到全局）
        ctx.eval::<(), _>(CRYPTOJS)
            .map_err(|e| format!("CryptoJS 加载失败: {e}"))?;
        // 2. 加载房间签名脚本
        ctx.eval::<(), _>(script)
            .map_err(|e| format!("签名脚本执行失败: {e}"))?;
        // 3. 调用签名函数
        let expr = format!("ub98484234('{rid}','{did}','{tt}')");
        ctx.eval::<String, _>(expr.as_str())
            .map_err(|e| format!("签名计算失败: {e}"))
    })
}
