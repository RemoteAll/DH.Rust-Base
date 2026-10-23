//! 统一 JSON 回信封（`{state, info|message}`）——Pek 生态 Rust 项目公共约定。
//!
//! # 为什么放在库里
//! 各项目（HLZX.EIP、Pek.RPanlServer、Pek.RCard、Pek.RAgent、HlkProductTool、
//! DHDeploy.Agent.Rust）**各自逐文件复制**了同几个函数：HLZX.EIP 一个仓库就有
//! 14 个文件、49 处同款 `ok / done / warn / unauthorized` 定义。信封形态一旦要
//! 调整（补 `code`、改状态词），逐仓复制必然漏改 —— 故收编到库里，各项目只 `use`。
//!
//! # 信封约定
//! | 函数 | HTTP | 响应体 |
//! |---|---|---|
//! | [`ok`] | 200 | `{"state":"success","info":<任意 JSON>}` |
//! | [`done`] | 200 | `{"state":"success"}` |
//! | [`warn`] | 200 | `{"state":"warning","message":"…"}`（业务性拒绝，前端弹提示） |
//! | [`fail`] | 200 | `{"state":"fail","message":"…"}`（同上，兼容旧前端判定） |
//! | [`unauthorized`] | 401 | `{"state":"warning","message":"未登录或登录已过期"}` |
//! | [`error`] | 500 | `{"state":"fail","message":"…"}` |
//!
//! 业务性拒绝用 HTTP 200 + `state` 字段（与 PHP 时代前端/客户端的既有判定一致，
//! 改状态码会让老前端把"口令不对"当成网络故障）。

use serde_json::{json, Value};

use super::http::{HttpOutcome, HttpResponse};

/// 默认未登录提示（各项目一致的口径）。
pub const UNAUTHORIZED_MESSAGE: &str = "未登录或登录已过期";

/// `{"state":"success","info":<info>}`（200）。
pub fn ok(info: Value) -> HttpOutcome {
    body(200, json!({ "state": "success", "info": info }))
}

/// `{"state":"success"}`（200；无数据的写操作）。
pub fn done() -> HttpOutcome {
    body(200, json!({ "state": "success" }))
}

/// `{"state":"warning","message":<msg>}`（200；业务性拒绝）。
pub fn warn(message: impl Into<String>) -> HttpOutcome {
    body(
        200,
        json!({ "state": "warning", "message": message.into() }),
    )
}

/// `{"state":"fail","message":<msg>}`（200；兼容以 `fail` 判定失败的旧前端）。
pub fn fail(message: impl Into<String>) -> HttpOutcome {
    body(200, json!({ "state": "fail", "message": message.into() }))
}

/// `{"state":"fail","message":<msg>}`（500；服务端异常）。
pub fn error(message: impl Into<String>) -> HttpOutcome {
    body(500, json!({ "state": "fail", "message": message.into() }))
}

/// 401 + 默认未登录提示。
pub fn unauthorized() -> HttpOutcome {
    unauthorized_msg(UNAUTHORIZED_MESSAGE)
}

/// 401 + 自定义提示（如"令牌无效"）。
pub fn unauthorized_msg(message: impl Into<String>) -> HttpOutcome {
    body(
        401,
        json!({ "state": "warning", "message": message.into() }),
    )
}

/// 403 + 提示（已登录但无权限）。
pub fn forbidden(message: impl Into<String>) -> HttpOutcome {
    body(
        403,
        json!({ "state": "warning", "message": message.into() }),
    )
}

/// 404 + 提示。
pub fn not_found(message: impl Into<String>) -> HttpOutcome {
    body(
        404,
        json!({ "state": "warning", "message": message.into() }),
    )
}

/// 任意状态码 + 任意信封（特殊场景兜底）。
pub fn body(status: u16, payload: Value) -> HttpOutcome {
    HttpOutcome::Response(HttpResponse::json(status, payload.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(out: HttpOutcome) -> (u16, String) {
        match out {
            HttpOutcome::Response(r) => (
                r.status,
                String::from_utf8_lossy(&r.body).into_owned(),
            ),
            _ => panic!("应返回普通响应"),
        }
    }

    #[test]
    fn ok_carries_info() {
        let (status, body) = text(ok(json!({ "id": 3 })));
        assert_eq!(status, 200);
        assert!(body.contains("\"state\":\"success\""));
        assert!(body.contains("\"info\":{\"id\":3}"));
    }

    #[test]
    fn done_has_no_info() {
        let (status, body) = text(done());
        assert_eq!(status, 200);
        assert_eq!(body, "{\"state\":\"success\"}");
    }

    #[test]
    fn warn_and_fail_are_200() {
        let (status, body) = text(warn("口令不正确"));
        assert_eq!(status, 200);
        assert!(body.contains("\"state\":\"warning\""));
        assert!(body.contains("口令不正确"));

        let (status, body) = text(fail("库存不足"));
        assert_eq!(status, 200);
        assert!(body.contains("\"state\":\"fail\""));
    }

    #[test]
    fn unauthorized_is_401() {
        let (status, body) = text(unauthorized());
        assert_eq!(status, 401);
        assert!(body.contains(UNAUTHORIZED_MESSAGE));

        let (status, _) = text(unauthorized_msg("令牌无效"));
        assert_eq!(status, 401);
    }

    #[test]
    fn error_and_forbidden_codes() {
        assert_eq!(text(error("炸了")).0, 500);
        assert_eq!(text(forbidden("无权限")).0, 403);
        assert_eq!(text(not_found("不存在")).0, 404);
    }

    #[test]
    fn json_is_escaped() {
        let (_, body) = text(warn("引号\"与反斜杠\\"));
        // 必须仍是合法 JSON（转义由 serde_json 保证）
        let parsed: Value = serde_json::from_str(&body).expect("合法 JSON");
        assert_eq!(parsed["message"], "引号\"与反斜杠\\");
    }
}
